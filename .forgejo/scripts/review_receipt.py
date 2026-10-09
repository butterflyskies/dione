#!/usr/bin/env python3
"""Check official approval review bodies and publish a status on the PR head."""

import json
import os
import re
import sys
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlsplit, urlunsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener


MARKER = "DIONE-SIX-LENS-RECEIPT v1"
CONTEXT = "Dione review / Six lenses"
LENSES = ("Safety", "Design", "Security", "Privacy", "Idiomacy", "Tests")
PUBLIC_API = "https://forgejo.svc.echoes/api/v1"
INTERNAL_HOST = "forgejo-http.forgejo.svc.cluster.local:3443"
INTERNAL_SERVICE = f"https://{INTERNAL_HOST}"
MAX_LIST_PAGES = 20


def parse_receipt(body, head, base):
    if not re.fullmatch(r"[0-9a-f]{40}", head) or not re.fullmatch(r"[0-9a-f]{40}", base):
        return "PR head or base is not a commit SHA"
    lines = [line.strip() for line in body.splitlines() if line.strip()]
    if not lines or lines[0] != MARKER:
        return "missing receipt marker"
    fields = {}
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


def evaluate(pr, approvals, blockers=()):
    head = pr["head"]["sha"]
    base = pr["base"]["sha"]
    author = pr["user"]["login"]
    if blockers:
        return "official reviewer requested changes", pr["html_url"]
    if not approvals:
        return "no current official approval with a six-lens receipt", pr["html_url"]
    approved = []
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
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        raise ValueError(f"Forgejo API returned redirect {code}")


def review_token(opener, audience):
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
        payload = json.load(response)
    token = payload.get("value") if isinstance(payload, dict) else None
    if not isinstance(token, str) or not token:
        raise ValueError("Forgejo OIDC request returned no JWT")
    print(f"::add-mask::{token}", flush=True)
    return token


def api_request(opener, base_url, token, path, payload=None, with_total=False):
    if base_url.rstrip("/") != PUBLIC_API:
        raise ValueError("unexpected Forgejo API URL")
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
        result = json.load(response)
        if with_total:
            total = response.headers.get("X-Total-Count")
            if total is None or not total.isdecimal():
                raise ValueError("Forgejo did not report the list total")
            return result, int(total)
        return result


def complete_list(opener, base_url, token, path, allow_hidden=False):
    # Review totals include drafts that the API hides from this integration.
    # Scan the reported number of server pages, not the visible item count.
    items = []
    total = None
    page = 1
    while total is None or (page - 1) * 50 < total:
        batch, reported_total = api_request(
            opener, base_url, token, f"{path}&page={page}", with_total=True
        )
        if not isinstance(batch, list) or len(batch) > 50:
            raise ValueError("Forgejo returned an invalid list page")
        if total is not None and reported_total != total:
            raise ValueError("Forgejo list changed during pagination")
        if reported_total > MAX_LIST_PAGES * 50:
            raise ValueError("Forgejo list exceeds the page limit")
        total = reported_total
        items.extend(batch)
        page += 1
    if len({item["id"] for item in items}) != len(items) or len(items) > total:
        raise ValueError("Forgejo did not return every unique item")
    if not allow_hidden and len(items) != total:
        raise ValueError("Forgejo returned an incomplete list")
    extra, reported_total = api_request(
        opener, base_url, token, f"{path}&page={page}", with_total=True
    )
    if extra or reported_total != total:
        raise ValueError("Forgejo list total is underreported or changed")
    return items


def official_reviewers(opener, base_url, token, repo, number, head):
    reviews = complete_list(opener, base_url, token, f"repos/{repo}/pulls/{number}/reviews?limit=50", allow_hidden=True)
    latest_decision = {}
    for review in reviews:
        login = (review.get("user") or {}).get("login")
        if not login or review.get("official") is not True or review.get("stale") is not False:
            continue
        if review.get("dismissed") is not False or review.get("commit_id") != head:
            continue
        state = review.get("state")
        if state not in {"APPROVED", "COMMENT", "REQUEST_CHANGES"}:
            continue
        if state in {"APPROVED", "REQUEST_CHANGES"}:
            if login not in latest_decision or review["id"] > latest_decision[login]["id"]:
                latest_decision[login] = review
    approvers = {login: review for login, review in latest_decision.items() if review["state"] == "APPROVED"}
    blockers = {login for login, review in latest_decision.items() if review["state"] == "REQUEST_CHANGES"}
    return approvers, blockers


def check_pr(opener, base_url, token, repo, number, event=None):
    path = f"repos/{repo}/pulls/{number}"
    try:
        pr = api_request(opener, base_url, token, path)
    except HTTPError as error:
        if error.code == 404 and event and "issue" in event and not event["issue"].get("pull_request"):
            print("Issue comment is not on a PR; no status needed")
            return 0
        raise
    if pr["state"] != "open" or pr["base"]["ref"] != "main":
        print("PR is closed or targets another branch; no status needed")
        return 0
    if not re.fullmatch(r"[0-9a-f]{40}", pr["head"]["sha"]) or not re.fullmatch(r"[0-9a-f]{40}", pr["base"]["sha"]):
        raise ValueError("PR head or base is not a commit SHA")
    status_path = f"repos/{repo}/statuses/{quote(pr['head']['sha'])}"
    # Revoke any earlier success before reading mutable PR or review state.
    # If a refresh fails, the required context remains pending on this head.
    api_request(
        opener, base_url, token, status_path,
        {"state": "pending", "context": CONTEXT, "description": "Checking current six-lens review", "target_url": pr["html_url"]},
    )
    approvals, blockers = official_reviewers(opener, base_url, token, repo, number, pr["head"]["sha"])
    error, target_url = evaluate(pr, approvals, blockers)
    current = api_request(opener, base_url, token, path)
    if current["head"]["sha"] != pr["head"]["sha"] or current["base"]["sha"] != pr["base"]["sha"]:
        raise ValueError("PR moved during receipt check")
    fresh_approvals, fresh_blockers = official_reviewers(opener, base_url, token, repo, number, pr["head"]["sha"])
    fresh_error, fresh_target = evaluate(current, fresh_approvals, fresh_blockers)
    if (fresh_approvals, fresh_blockers, fresh_error, fresh_target) != (approvals, blockers, error, target_url):
        raise ValueError("review receipt changed during check")
    state = "failure" if error else "success"
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
            after = api_request(opener, base_url, token, path)
            if after["head"]["sha"] != pr["head"]["sha"] or after["base"]["sha"] != pr["base"]["sha"]:
                raise ValueError("PR changed after status write")
            after_approvals, after_blockers = official_reviewers(
                opener, base_url, token, repo, number, pr["head"]["sha"]
            )
            after_error, after_target = evaluate(after, after_approvals, after_blockers)
            if (after_approvals, after_blockers, after_error, after_target) != (
                approvals, blockers, error, target_url
            ):
                raise ValueError("review changed after status write")
    except Exception:
        if state == "success":
            for attempt in range(3):
                try:
                    api_request(
                        opener, base_url, token, status_path,
                        {"state": "pending", "context": CONTEXT, "description": "Review changed after status write; rerun", "target_url": pr["html_url"]},
                    )
                    break
                except Exception:
                    if attempt == 2:
                        raise
        raise
    print(f"{CONTEXT}: {state}: {description}")
    return 0


def main():
    base_url = os.environ["FORGEJO_API_URL"]
    repo = os.environ["FORGEJO_REPOSITORY"]
    if base_url.rstrip("/") != PUBLIC_API or repo != "lacuna/dione":
        raise ValueError("unexpected Forgejo API or repository")
    with open(os.environ["FORGEJO_EVENT_PATH"], encoding="utf-8") as handle:
        event = json.load(handle)
    opener = build_opener(NoRedirect())
    token = review_token(opener, os.environ.get("REVIEW_AUDIENCE", ""))
    if os.environ.get("FORGEJO_EVENT_NAME") == "push":
        if event.get("ref") != "refs/heads/main":
            raise ValueError("unexpected push branch")
        pulls = complete_list(opener, base_url, token, f"repos/{repo}/pulls?state=open&limit=50")
        failures = 0
        for pull in pulls:
            try:
                if pull["base"]["ref"] == "main":
                    check_pr(opener, base_url, token, repo, pull["number"])
            except Exception as error:
                failures += 1
                print(f"Could not refresh PR #{pull.get('number', 'unknown')}: {error}", file=sys.stderr)
        if failures:
            raise ValueError(f"could not refresh {failures} PR receipt statuses")
        return 0
    number = event.get("number") or (event.get("issue") or {}).get("number")
    if not isinstance(number, int) or number <= 0:
        raise ValueError("event has no PR number")
    return check_pr(opener, base_url, token, repo, number, event)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"Review receipt check failed closed: {error}", file=sys.stderr)
        sys.exit(1)
