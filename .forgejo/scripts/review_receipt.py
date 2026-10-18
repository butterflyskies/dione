#!/usr/bin/env python3
"""Check official approval review bodies and publish a status on the PR head."""

from __future__ import annotations

from collections.abc import Collection, Mapping
from dataclasses import dataclass
import json
import os
import re
import sys
from typing import Literal, NoReturn, TypedDict, overload
from urllib.error import HTTPError
from urllib.parse import quote, urlsplit, urlunsplit
from urllib.request import HTTPRedirectHandler, OpenerDirector, Request, build_opener


MARKER = "DIONE-SIX-LENS-RECEIPT v1"
CONTEXT = "Dione review / Six lenses"
LENSES = ("Safety", "Design", "Security", "Privacy", "Idiomacy", "Tests")
PUBLIC_API = "https://forgejo.svc.echoes/api/v1"
INTERNAL_HOST = "forgejo-http.forgejo.svc.cluster.local:3443"
INTERNAL_SERVICE = f"https://{INTERNAL_HOST}"
ALLOWED_API_URLS = (PUBLIC_API, f"{INTERNAL_SERVICE}/api/v1")
MAX_LIST_PAGES = 20


class CommitReference(TypedDict):
    """Commit fields read by receipt evaluation."""

    sha: str


class BaseReference(CommitReference):
    """Base commit fields included in the pull request fixture."""

    ref: str


class ReviewAuthor(TypedDict):
    """Login used to exclude approval by the pull request author."""

    login: str


class PullRequest(TypedDict):
    """Pull request fields consumed after boundary validation."""

    state: str
    head: CommitReference
    base: BaseReference
    user: ReviewAuthor
    html_url: str


class OptionalReceiptBody(TypedDict, total=False):
    """The receipt body may be absent or null in an approval."""

    body: str | None


class ApprovalReceipt(OptionalReceiptBody):
    """Approval fields used to order and evaluate six-lens receipts."""

    id: int
    html_url: str


@dataclass(frozen=True)
class PrStatusTarget:
    """Validated routing fields, before author parsing or review evaluation."""

    state: str
    base_ref: str
    head_sha: str
    base_sha: str
    html_url: str
    raw: Mapping[str, object]


@dataclass(frozen=True)
class ReviewSelection:
    """Approval receipts and full snapshots, with only login identity for blockers."""

    approvals: Mapping[str, ApprovalReceipt]
    blockers: frozenset[str]
    approval_snapshots: Mapping[str, Mapping[str, object]]


@dataclass(frozen=True)
class PushEvent:
    """A main-branch push that refreshes open pull requests."""

    ref: str


@dataclass(frozen=True)
class PullEvent:
    """An event routing a pull request or a possible ordinary issue."""

    number: int
    ordinary_issue: bool = False


class StatusPayload(TypedDict):
    """The exact status fields sent to Forgejo."""

    state: Literal["pending", "failure", "success"]
    context: str
    description: str
    target_url: str


def object_fields(value: object, context: str) -> dict[str, object]:
    """Read an object without exposing unchecked JSON values to callers."""
    if not isinstance(value, dict):
        raise ValueError(f"{context} must be an object")
    fields: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise ValueError(f"{context} must have string keys")
        fields[key] = item
    return fields


def list_items(value: object, context: str) -> list[object]:
    """Read a list while keeping its entries opaque until projection."""
    if not isinstance(value, list):
        raise ValueError(f"{context} must be a list")
    return [item for item in value]


def string_field(fields: Mapping[str, object], name: str, context: str) -> str:
    """Read a nonempty string used as routing or identity data."""
    value = fields.get(name)
    if not isinstance(value, str) or not value:
        raise ValueError(f"{context}.{name} must be a nonempty string")
    return value


def positive_id(value: object, context: str) -> int:
    """Read a positive integer identity without coercion."""
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ValueError(f"{context} must be a positive integer")
    return value


def targets_other_branch(value: object, event_name: str) -> bool:
    """Recognize explicit non-main PR targets before requesting credentials."""
    if event_name not in {"pull_request", "pull_request_target"}:
        return False
    fields = object_fields(value, "event")
    pull = object_fields(fields.get("pull_request", {}), "event.pull_request")
    base = object_fields(pull.get("base", {}), "event.pull_request.base")
    ref = base.get("ref")
    return isinstance(ref, str) and bool(ref) and ref != "main"


def parse_event(value: object, event_name: str) -> PushEvent | PullEvent:
    """Project only the routing evidence supplied by supported events."""
    fields = object_fields(value, "event")
    if event_name == "push":
        ref = string_field(fields, "ref", "event")
        if ref != "refs/heads/main":
            raise ValueError("unexpected push branch")
        return PushEvent(ref)
    if event_name not in {"pull_request_target", "issue_comment"}:
        raise ValueError("unexpected Forgejo event name")
    number = fields.get("number")
    issue: dict[str, object] = {}
    if event_name == "issue_comment" or number is None:
        issue = object_fields(fields.get("issue"), "event.issue")
    if number is None:
        number = issue.get("number")
    ordinary_issue = False
    if event_name == "issue_comment":
        marker = issue.get("pull_request")
        if marker is not None:
            object_fields(marker, "event.issue.pull_request")
        ordinary_issue = marker is None
    return PullEvent(positive_id(number, "event PR number"), ordinary_issue)


def target_fields(fields: Mapping[str, object]) -> PrStatusTarget:
    """Read the status routing fields required on every PR refresh."""
    state = string_field(fields, "state", "PR")
    head = object_fields(fields.get("head"), "PR.head")
    base = object_fields(fields.get("base"), "PR.base")
    head_sha = string_field(head, "sha", "PR.head")
    base_sha = string_field(base, "sha", "PR.base")
    if not re.fullmatch(r"[0-9a-f]{40}", head_sha) or not re.fullmatch(r"[0-9a-f]{40}", base_sha):
        raise ValueError("PR head or base is not a commit SHA")
    return PrStatusTarget(
        state, string_field(base, "ref", "PR.base"), head_sha, base_sha,
        string_field(fields, "html_url", "PR"), fields,
    )


def parse_status_target(value: object) -> PrStatusTarget | None:
    """Skip irrelevant PRs before demanding status routing fields."""
    fields = object_fields(value, "PR")
    if string_field(fields, "state", "PR") != "open":
        return None
    base = object_fields(fields.get("base"), "PR.base")
    if string_field(base, "ref", "PR.base") != "main":
        return None
    return target_fields(fields)


def parse_evaluation_pr(target: PrStatusTarget) -> PullRequest:
    """Read author identity after the prior success has been revoked."""
    user = object_fields(target.raw.get("user"), "PR.user")
    return {
        "state": target.state, "head": {"sha": target.head_sha},
        "base": {"sha": target.base_sha, "ref": target.base_ref},
        "html_url": target.html_url,
        "user": {"login": string_field(user, "login", "PR.user")},
    }


def parse_receipt(body: str, head: str, base: str) -> str | None:
    if not re.fullmatch(r"[0-9a-f]{40}", head) or not re.fullmatch(r"[0-9a-f]{40}", base):
        return "PR head or base is not a commit SHA"
    lines = [line.strip() for line in body.splitlines() if line.strip()]
    if not lines or lines[0] != MARKER:
        return "missing receipt marker"
    fields: dict[str, str] = {}
    for line in lines[1:]:
        if ":" not in line:
            return "malformed receipt line"
        key, value = line.split(":", 1)
        if key in fields:
            return f"duplicate {key} field"
        fields[key] = value.strip()
    if fields.get("Head") != head:
        return "receipt does not match PR head"
    if fields.get("Base") != base:
        return "receipt does not match PR base"
    if fields.get("Decision") != "approve":
        return "reviewer has not approved the reviewed commit"
    if fields.get("Unresolved P1-P3") != "none":
        return "review has unresolved P1-P3 findings"
    for lens in LENSES:
        value = fields.get(lens, "")
        if not re.fullmatch(r"(?:pass|n/a) - \S.{7,}", value):
            return f"{lens} needs a pass or n/a with evidence"
        if re.search(r"<[^<>]*>", value) or value == "n/a - not applicable":
            return f"{lens} needs specific evidence or a reason"
    if set(fields) != {"Head", "Base", "Decision", "Unresolved P1-P3", *LENSES}:
        return "unexpected receipt field"
    return None


def evaluate(
    pr: PullRequest,
    approvals: Mapping[str, ApprovalReceipt],
    blockers: Collection[str] = (),
) -> tuple[str | None, str]:
    head = pr["head"]["sha"]
    base = pr["base"]["sha"]
    author = pr["user"]["login"]
    if blockers:
        return "official reviewer requested changes", pr["html_url"]
    if not approvals:
        return "no current official approval with a six-lens receipt", pr["html_url"]
    approved: list[ApprovalReceipt] = []
    for login, review in sorted(approvals.items(), key=lambda item: item[1]["id"], reverse=True):
        if login == author:
            continue
        error = parse_receipt(review.get("body") or "", head, base)
        if error:
            return error, review["html_url"]
        approved.append(review)
    if not approved:
        return "PR author cannot attest their own review", pr["html_url"]
    return None, approved[0]["html_url"]


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(
        self,
        request: Request,
        fp: object,
        code: int,
        msg: str,
        headers: object,
        newurl: str,
    ) -> NoReturn:
        raise ValueError(f"Forgejo API returned redirect {code}")


def review_token(opener: OpenerDirector, audience: str) -> str:
    if not audience:
        raise ValueError("review Authorized Integration audience is missing")
    request_url = os.environ.get("ACTIONS_ID_TOKEN_REQUEST_URL", "")
    request_token = os.environ.get("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "")
    parts = urlsplit(request_url)
    if parts.scheme != "https" or parts.netloc != "forgejo.svc.echoes" or not parts.path.startswith("/api/actions/"):
        raise ValueError("unexpected Forgejo OIDC request URL")
    if not request_token:
        raise ValueError("Forgejo OIDC request token is missing")
    # The public Gateway cannot be reached from cluster pods. Use its internal
    # service and retain the public Host header, as the release tagger does.
    query = (parts.query + "&" if parts.query else "") + "audience=" + quote(audience, safe="")
    url = urlunsplit(("https", INTERNAL_HOST, parts.path, query, ""))
    request = Request(url, headers={"Authorization": f"bearer {request_token}", "Host": parts.netloc})
    with opener.open(request, timeout=30) as response:
        payload: object = json.load(response)
    token = object_fields(payload, "Forgejo OIDC response").get("value")
    if not isinstance(token, str) or not token:
        raise ValueError("Forgejo OIDC request returned no JWT")
    print(f"::add-mask::{token}", flush=True)
    return token


def require_known_api_url(base_url: str) -> None:
    """Accept only the public and fixed internal Forgejo API identities."""
    if base_url.rstrip("/") not in ALLOWED_API_URLS:
        raise ValueError(f"unexpected Forgejo API URL: {base_url!r}")


@overload
def api_request(opener: OpenerDirector, base_url: str, token: str, path: str,
                payload: StatusPayload | None = None, *, with_total: Literal[False] = False) -> object: ...


@overload
def api_request(opener: OpenerDirector, base_url: str, token: str, path: str,
                payload: StatusPayload | None = None, *, with_total: Literal[True]) -> tuple[object, int]: ...


def api_request(opener: OpenerDirector, base_url: str, token: str, path: str,
                payload: StatusPayload | None = None, *, with_total: bool = False) -> object:
    require_known_api_url(base_url)
    url = INTERNAL_SERVICE + "/api/v1/" + path.lstrip("/")
    data = None if payload is None else json.dumps(payload).encode("utf-8")
    request = Request(
        url,
        data=data,
        headers={
            "Authorization": f"Bearer {token}",
            "Host": "forgejo.svc.echoes",
            "Accept": "application/json",
            "Content-Type": "application/json",
        },
        method="GET" if payload is None else "POST",
    )
    with opener.open(request, timeout=20) as response:
        result: object = json.load(response)
        if with_total:
            total = response.headers.get("X-Total-Count")
            if total is None or not total.isdecimal():
                raise ValueError("Forgejo did not report the list total")
            return result, int(total)
        return result


def complete_list(opener: OpenerDirector, base_url: str, token: str, path: str,
                  allow_hidden: bool = False) -> list[dict[str, object]]:
    # Review totals include drafts that the API hides from this integration.
    # Scan the reported number of server pages, not the visible item count.
    items: list[dict[str, object]] = []
    identities: set[int] = set()
    total: int | None = None
    page = 1
    while total is None or (page - 1) * 50 < total:
        batch, reported_total = api_request(
            opener, base_url, token, f"{path}&page={page}", with_total=True
        )
        entries = list_items(batch, "Forgejo list page")
        if len(entries) > 50:
            raise ValueError("Forgejo returned an invalid list page")
        if total is not None and reported_total != total:
            raise ValueError("Forgejo list changed during pagination")
        if reported_total > MAX_LIST_PAGES * 50:
            raise ValueError("Forgejo list exceeds the page limit")
        total = reported_total
        for entry in entries:
            item = object_fields(entry, "Forgejo list item")
            identities.add(positive_id(item.get("id"), "Forgejo list item.id"))
            items.append(item)
        page += 1
    if len(identities) != len(items) or len(items) > total:
        raise ValueError("Forgejo did not return every unique item")
    if not allow_hidden and len(items) != total:
        raise ValueError("Forgejo returned an incomplete list")
    extra, reported_total = api_request(
        opener, base_url, token, f"{path}&page={page}", with_total=True
    )
    if list_items(extra, "Forgejo final list page") or reported_total != total:
        raise ValueError("Forgejo list total is underreported or changed")
    return items


def official_reviewers(opener: OpenerDirector, base_url: str, token: str, repo: str,
                       number: int, head: str) -> ReviewSelection:
    reviews = complete_list(opener, base_url, token, f"repos/{repo}/pulls/{number}/reviews?limit=50", allow_hidden=True)
    latest: dict[str, tuple[int, Literal["APPROVED", "REQUEST_CHANGES"], Mapping[str, object]]] = {}
    for review in reviews:
        review_id = positive_id(review.get("id"), "review.id")
        if review.get("official") is not True or review.get("stale") is not False:
            continue
        if review.get("dismissed") is not False or review.get("commit_id") != head:
            continue
        raw_state = review.get("state")
        state: Literal["APPROVED", "REQUEST_CHANGES"]
        if raw_state == "APPROVED":
            state = "APPROVED"
        elif raw_state == "REQUEST_CHANGES":
            state = "REQUEST_CHANGES"
        else:
            continue
        user = object_fields(review.get("user"), "review.user")
        login = string_field(user, "login", "review.user")
        if login not in latest or review_id > latest[login][0]:
            latest[login] = review_id, state, review
    approvals: dict[str, ApprovalReceipt] = {}
    snapshots: dict[str, Mapping[str, object]] = {}
    blockers: set[str] = set()
    for login, (review_id, state, raw) in latest.items():
        if state == "REQUEST_CHANGES":
            blockers.add(login)
            continue
        approval: ApprovalReceipt = {"id": review_id, "html_url": string_field(raw, "html_url", "approval")}
        if "body" in raw:
            body = raw["body"]
            if body is not None and not isinstance(body, str):
                raise ValueError("approval.body must be a string or null")
            approval["body"] = body
        approvals[login] = approval
        snapshots[login] = raw
    return ReviewSelection(approvals, frozenset(blockers), snapshots)


def check_pr(opener: OpenerDirector, base_url: str, token: str, repo: str,
             number: int, event: PullEvent | None = None) -> int:
    path = f"repos/{repo}/pulls/{number}"
    try:
        pr = api_request(opener, base_url, token, path)
    except HTTPError as request_error:
        if request_error.code == 404 and event is not None and event.ordinary_issue:
            print("Issue comment is not on a PR; no status needed")
            return 0
        raise
    target = parse_status_target(pr)
    if target is None:
        print("PR is closed or targets another branch; no status needed")
        return 0
    status_path = f"repos/{repo}/statuses/{quote(target.head_sha)}"
    # Revoke any earlier success before reading mutable PR or review state.
    # If a refresh fails, the required context remains pending on this head.
    api_request(
        opener, base_url, token, status_path,
        {"state": "pending", "context": CONTEXT, "description": "Checking current six-lens review", "target_url": target.html_url},
    )
    evaluated_pr = parse_evaluation_pr(target)
    selection = official_reviewers(opener, base_url, token, repo, number, target.head_sha)
    error, target_url = evaluate(evaluated_pr, selection.approvals, selection.blockers)
    current = target_fields(object_fields(api_request(opener, base_url, token, path), "PR"))
    if current.head_sha != target.head_sha or current.base_sha != target.base_sha:
        raise ValueError("PR moved during receipt check")
    fresh = official_reviewers(opener, base_url, token, repo, number, target.head_sha)
    fresh_error, fresh_target = evaluate(parse_evaluation_pr(current), fresh.approvals, fresh.blockers)
    if (fresh, fresh_error, fresh_target) != (selection, error, target_url):
        raise ValueError("review receipt changed during check")
    state: Literal["failure", "success"] = "failure" if error else "success"
    description = error or "Official approval includes six lenses at this head and base"
    try:
        api_request(
            opener,
            base_url,
            token,
            status_path,
            {"state": state, "context": CONTEXT, "description": description, "target_url": target_url},
        )
        if state == "success":
            after = target_fields(object_fields(api_request(opener, base_url, token, path), "PR"))
            if after.head_sha != target.head_sha or after.base_sha != target.base_sha:
                raise ValueError("PR changed after status write")
            after_selection = official_reviewers(opener, base_url, token, repo, number, target.head_sha)
            after_error, after_target = evaluate(parse_evaluation_pr(after), after_selection.approvals, after_selection.blockers)
            if (after_selection, after_error, after_target) != (selection, error, target_url):
                raise ValueError("review changed after status write")
    except Exception:
        if state == "success":
            for attempt in range(3):
                try:
                    api_request(
                        opener, base_url, token, status_path,
                        {"state": "pending", "context": CONTEXT, "description": "Review changed after status write; rerun", "target_url": target.html_url},
                    )
                    break
                except Exception:
                    if attempt == 2:
                        raise
        raise
    print(f"{CONTEXT}: {state}: {description}")
    return 0


def main() -> int:
    base_url = os.environ["FORGEJO_API_URL"]
    repo = os.environ["FORGEJO_REPOSITORY"]
    require_known_api_url(base_url)
    if repo != "lacuna/dione":
        raise ValueError(f"unexpected Forgejo repository: {repo!r}")
    with open(os.environ["FORGEJO_EVENT_PATH"], encoding="utf-8") as handle:
        raw_event: object = json.load(handle)
    if targets_other_branch(raw_event, os.environ.get("FORGEJO_EVENT_NAME", "")):
        print("PR targets another branch; no status needed")
        return 0
    opener = build_opener(NoRedirect())
    token = review_token(opener, os.environ.get("REVIEW_AUDIENCE", ""))
    event = parse_event(raw_event, os.environ.get("FORGEJO_EVENT_NAME", ""))
    if isinstance(event, PushEvent):
        pulls = complete_list(opener, base_url, token, f"repos/{repo}/pulls?state=open&limit=50")
        failures = 0
        for pull in pulls:
            number: int | None = None
            try:
                base = object_fields(pull.get("base"), "pull list item.base")
                if string_field(base, "ref", "pull list item.base") == "main":
                    number = positive_id(pull.get("number"), "pull list item.number")
                    check_pr(opener, base_url, token, repo, number)
            except Exception as error:
                failures += 1
                print(f"Could not refresh PR #{number if number is not None else 'unknown'}: {error}", file=sys.stderr)
        if failures:
            raise ValueError(f"could not refresh {failures} PR receipt statuses")
        return 0
    return check_pr(opener, base_url, token, repo, event.number, event)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"Review receipt check failed closed: {error}", file=sys.stderr)
        sys.exit(1)
