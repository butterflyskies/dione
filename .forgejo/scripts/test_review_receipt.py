from __future__ import annotations

from collections.abc import Collection, Mapping
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import unittest
from email.message import Message
from http.client import IncompleteRead
from contextlib import redirect_stdout
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from unittest.mock import Mock, patch
from urllib.error import HTTPError
from urllib.request import OpenerDirector, Request, build_opener

import review_receipt
from review_receipt import ApprovalReceipt, PullRequest, ReviewAuthor, ReviewSelection, StatusPayload, evaluate


HEAD = "a" * 40
BASE = "b" * 40
REVIEWERS = {"lain-construct"}


class CommentFixture(ApprovalReceipt):
    user: ReviewAuthor


class ReviewFixture(CommentFixture):
    state: str
    official: bool
    stale: bool
    dismissed: bool
    commit_id: str


def pr() -> PullRequest:
    return {
        "state": "open",
        "head": {"sha": HEAD},
        "base": {"sha": BASE, "ref": "main"},
        "user": {"login": "author"},
        "html_url": "https://forgejo.example/pr/1",
    }


def receipt(head: str = HEAD, base: str = BASE) -> str:
    lines = ["DIONE-SIX-LENS-RECEIPT v1", f"Head: {head}", f"Base: {base}", "Decision: approve"]
    lines += [
        f"{lens}: pass - checked the changed behavior"
        for lens in ("Safety", "Design", "Security", "Privacy", "Idiomacy", "Tests")
    ]
    lines.append("Unresolved P1-P3: none")
    return "\n".join(lines)


def comment(
    body: str | None = None, author: str = "lain-construct", number: int = 1,
) -> CommentFixture:
    return {
        "id": number,
        "body": receipt() if body is None else body,
        "user": {"login": author},
        "html_url": "https://forgejo.example/pr/1#comment-1",
    }


def review(
    author: str = "lain-construct",
    number: int = 1,
    state: str = "APPROVED",
    official: bool = True,
    head: str = HEAD,
    body: str | None = None,
) -> ReviewFixture:
    return {
        "id": number,
        "user": {"login": author},
        "state": state,
        "official": official,
        "stale": False,
        "dismissed": False,
        "commit_id": head,
        "body": receipt() if body is None else body,
        "html_url": f"https://forgejo.example/pr/1#review-{number}",
    }


class JsonResponse(io.BytesIO):
    def __init__(self, data: bytes) -> None:
        super().__init__(data)
        self.headers: dict[str, str] = {}


def selection(approvals: Mapping[str, ReviewFixture], blockers: Collection[str] = ()) -> ReviewSelection:
    receipts: dict[str, ApprovalReceipt] = {}
    snapshots: dict[str, Mapping[str, object]] = {}
    for login, item in approvals.items():
        receipts[login] = {"id": item["id"], "html_url": item["html_url"]}
        if "body" in item:
            receipts[login]["body"] = item["body"]
        snapshots[login] = review_receipt.object_fields(item, "fixture")
    return ReviewSelection(receipts, frozenset(blockers), snapshots)


def selected_snapshots(value: ReviewSelection) -> tuple[Mapping[str, Mapping[str, object]], frozenset[str]]:
    return value.approval_snapshots, value.blockers


def check_with(pr_reads: list[object], review_reads: list[list[object]],
               posted: list[StatusPayload]) -> int:
    reads = iter(pr_reads)

    def fake_api(_opener: OpenerDirector, _base_url: str, _token: str, path: str,
                 payload: StatusPayload | None = None, with_total: bool = False) -> object:
        if payload is not None:
            posted.append(payload)
            return {"ignored": [None, True]}
        if path == "repos/lacuna/dione/pulls/1":
            return next(reads)
        raise AssertionError("unexpected API read")

    with (patch.object(review_receipt, "api_request", side_effect=fake_api),
          patch.object(review_receipt, "complete_list", side_effect=review_reads)):
        return review_receipt.check_pr(build_opener(), "base", "token", "lacuna/dione", 1)


def run_event(event: object, name: str) -> int:
    with tempfile.TemporaryDirectory() as directory:
        event_path = Path(directory) / "event.json"
        event_path.write_text(json.dumps(event), encoding="utf-8")
        env = {"FORGEJO_API_URL": review_receipt.PUBLIC_API,
               "FORGEJO_REPOSITORY": "lacuna/dione", "FORGEJO_EVENT_PATH": str(event_path),
               "FORGEJO_EVENT_NAME": name}
        with patch.dict(os.environ, env, clear=True), patch.object(review_receipt, "review_token", return_value="fake"):
            return review_receipt.main()


class ReceiptTests(unittest.TestCase):
    def test_api_request_accepts_internal_and_rejects_unknown_api_url(self) -> None:
        rejected = (
            "https://forgejo.example/api/v1",
            "http://forgejo-http.forgejo.svc.cluster.local:3443/api/v1",
            "https://forgejo-http.forgejo.svc.cluster.local:3443/other",
            "https://forgejo-http.forgejo.svc.cluster.local:3443.evil/api/v1",
        )
        for api_url in rejected:
            with self.subTest(api_url=api_url), patch.object(OpenerDirector, "open") as opened:
                opened.return_value.__enter__.return_value = io.BytesIO(b"{}")
                with self.assertRaisesRegex(ValueError, "unexpected Forgejo API URL"):
                    review_receipt.api_request(build_opener(), api_url, "token", "repos/lacuna/dione")
                opened.assert_not_called()
        for api_url in (review_receipt.PUBLIC_API, f"{review_receipt.INTERNAL_SERVICE}/api/v1"):
            with self.subTest(api_url=api_url), patch.object(OpenerDirector, "open") as opened:
                opened.return_value.__enter__.return_value = io.BytesIO(b"{}")
                self.assertEqual({}, review_receipt.api_request(build_opener(), api_url, "token", "repos/lacuna/dione"))
                request = opened.call_args.args[0]
                self.assertEqual(f"{review_receipt.INTERNAL_SERVICE}/api/v1/repos/lacuna/dione", request.full_url)
                self.assertEqual("forgejo.svc.echoes", request.get_header("Host"))
                self.assertEqual("Bearer token", request.get_header("Authorization"))

    def test_ambiguous_base_refs_cannot_skip_authentication(self) -> None:
        ambiguous: list[object] = [None, False, 0, [], {}, ""]
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            for ref in ambiguous:
                event_path.write_text(json.dumps({"pull_request": {"base": {"ref": ref}}}), encoding="utf-8")
                for event_name in ("pull_request", "pull_request_target"):
                    env = {
                        "FORGEJO_API_URL": review_receipt.PUBLIC_API,
                        "FORGEJO_REPOSITORY": "lacuna/dione",
                        "FORGEJO_EVENT_PATH": str(event_path),
                        "FORGEJO_EVENT_NAME": event_name,
                    }
                    with self.subTest(ref=ref, event_name=event_name), patch.dict(os.environ, env, clear=True):
                        with patch.object(review_receipt, "review_token", side_effect=RuntimeError("authentication required")):
                            with self.assertRaisesRegex(RuntimeError, "authentication required"):
                                review_receipt.main()

    def test_malformed_early_skip_objects_fail_visibly(self) -> None:
        malformed: list[object] = [
            None, [], "event", {"pull_request": None}, {"pull_request": []},
            {"pull_request": "pull"}, {"pull_request": {"base": None}},
            {"pull_request": {"base": []}}, {"pull_request": {"base": "base"}},
        ]
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            for event in malformed:
                event_path.write_text(json.dumps(event), encoding="utf-8")
                for event_name in ("pull_request", "pull_request_target"):
                    env = {
                        "FORGEJO_API_URL": review_receipt.PUBLIC_API,
                        "FORGEJO_REPOSITORY": "lacuna/dione",
                        "FORGEJO_EVENT_PATH": str(event_path),
                        "FORGEJO_EVENT_NAME": event_name,
                    }
                    with (
                        self.subTest(event=event, event_name=event_name),
                        patch.dict(os.environ, env, clear=True),
                        patch.object(review_receipt, "review_token") as authentication,
                        patch.object(review_receipt, "api_request") as request,
                        redirect_stdout(io.StringIO()) as output,
                    ):
                        with self.assertRaisesRegex(ValueError, "must be an object"):
                            review_receipt.main()
                        authentication.assert_not_called()
                        request.assert_not_called()
                        self.assertNotIn("no status needed", output.getvalue())

    def test_non_main_pr_event_needs_no_authentication(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text(json.dumps({"number": 513, "pull_request": {"base": {"ref": "refactor/composition-cleanup"}}}), encoding="utf-8")
            env = {
                "FORGEJO_API_URL": review_receipt.PUBLIC_API,
                "FORGEJO_REPOSITORY": "lacuna/dione",
                "FORGEJO_EVENT_PATH": str(event_path),
            }
            for event_name in ("pull_request", "pull_request_target"):
                with (
                    self.subTest(event_name=event_name),
                    patch.dict(os.environ, {**env, "FORGEJO_EVENT_NAME": event_name}, clear=True),
                    patch.object(review_receipt, "review_token", side_effect=AssertionError("unexpected authentication")),
                    patch.object(review_receipt, "api_request", side_effect=AssertionError("unexpected API request")),
                    redirect_stdout(io.StringIO()) as output,
                ):
                    self.assertEqual(0, review_receipt.main())
                    self.assertIn("targets another branch", output.getvalue())

    def test_main_and_ambiguous_events_still_require_authentication(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            for event_name, event in (
                ("pull_request", {"pull_request": {"base": {"ref": "main"}}}),
                ("pull_request_target", {"pull_request": {"base": {"ref": "main"}}}),
                ("pull_request", {"number": 513}),
                ("pull_request", {"pull_request": {"base": {"ref": ""}}}),
                ("issue_comment", {"pull_request": {"base": {"ref": "refactor/example"}}}),
                ("push", {"ref": "refs/heads/main"}),
            ):
                event_path.write_text(json.dumps(event), encoding="utf-8")
                env = {
                    "FORGEJO_API_URL": review_receipt.PUBLIC_API,
                    "FORGEJO_REPOSITORY": "lacuna/dione",
                    "FORGEJO_EVENT_PATH": str(event_path),
                    "FORGEJO_EVENT_NAME": event_name,
                }
                with (
                    self.subTest(event_name=event_name, event=event),
                    patch.dict(os.environ, env, clear=True),
                    patch.object(review_receipt, "review_token", side_effect=RuntimeError("authentication required")),
                ):
                    with self.assertRaisesRegex(RuntimeError, "authentication required"):
                        review_receipt.main()

    def test_main_rejects_unexpected_api_url(self) -> None:
        for api_url in ("http://forgejo.example/api/v1", "https://forgejo.example/api/v1", "http://forgejo-http.forgejo.svc.cluster.local:3443/api/v1"):
            with self.subTest(api_url=api_url), patch.dict(os.environ, {"FORGEJO_API_URL": api_url, "FORGEJO_REPOSITORY": "lacuna/dione"}):
                with self.assertRaisesRegex(ValueError, f"unexpected Forgejo API URL: '{re.escape(api_url)}'"):
                    review_receipt.main()

    def test_main_rejects_unexpected_repository(self) -> None:
        with patch.dict(os.environ, {"FORGEJO_API_URL": review_receipt.PUBLIC_API, "FORGEJO_REPOSITORY": "other/dione"}):
            with self.assertRaisesRegex(ValueError, "unexpected Forgejo repository: 'other/dione'"):
                review_receipt.main()

    def test_main_accepts_runner_internal_api_url(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text("{}", encoding="utf-8")
            for api_url in (f"{review_receipt.INTERNAL_SERVICE}/api/v1", f"{review_receipt.INTERNAL_SERVICE}/api/v1/", review_receipt.PUBLIC_API):
                env = {"FORGEJO_API_URL": api_url, "FORGEJO_REPOSITORY": "lacuna/dione", "FORGEJO_EVENT_PATH": str(event_path)}
                with self.subTest(api_url=api_url), patch.dict(os.environ, env), patch.object(review_receipt, "review_token", side_effect=RuntimeError("past the guard")):
                    with self.assertRaisesRegex(RuntimeError, "past the guard"):
                        review_receipt.main()

    def test_redirect_does_not_forward_integration_token(self) -> None:
        requests: list[tuple[str, str | None]] = []

        class RedirectHandler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                requests.append((self.path, self.headers.get("Authorization")))
                if self.path == "/start":
                    self.send_response(302)
                    self.send_header("Location", "/leak")
                else:
                    self.send_response(200)
                self.end_headers()

            def log_message(self, *_args: object) -> None:
                pass

        server = HTTPServer(("127.0.0.1", 0), RedirectHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}/start"
            request = Request(url, headers={"Authorization": "Bearer private-token"})
            with self.assertRaisesRegex(ValueError, "redirect 302"):
                build_opener(review_receipt.NoRedirect()).open(request, timeout=2)
            self.assertEqual([("/start", "Bearer private-token")], requests)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_main_installs_redirect_block(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text("{}", encoding="utf-8")
            env = {
                "FORGEJO_API_URL": review_receipt.PUBLIC_API,
                "FORGEJO_REPOSITORY": "lacuna/dione",
                "FORGEJO_EVENT_PATH": str(event_path),
            }
            with patch.dict(os.environ, env), patch.object(review_receipt, "review_token", side_effect=RuntimeError("stop after opener")) as token:
                with self.assertRaisesRegex(RuntimeError, "stop after opener"):
                    review_receipt.main()
            opener = token.call_args.args[0]
            self.assertTrue(any(isinstance(handler, review_receipt.NoRedirect) for handler in opener.handlers))

    def test_review_jwt_uses_internal_service_and_public_host(self) -> None:
        opener = Mock()
        opener.open.return_value = io.BytesIO(b'{"value":"short-lived-jwt"}')
        env = {
            "ACTIONS_ID_TOKEN_REQUEST_URL": "https://forgejo.svc.echoes/api/actions/oidc?run=123",
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN": "request-token",
        }
        output = io.StringIO()
        with patch.dict(os.environ, env, clear=True), redirect_stdout(output):
            self.assertEqual("short-lived-jwt", review_receipt.review_token(opener, "u:review"))
        self.assertEqual("::add-mask::short-lived-jwt\n", output.getvalue())
        request = opener.open.call_args.args[0]
        self.assertEqual(
            "https://forgejo-http.forgejo.svc.cluster.local:3443/api/actions/oidc?run=123&audience=u%3Areview",
            request.full_url,
        )
        self.assertEqual("forgejo.svc.echoes", request.get_header("Host"))
        self.assertEqual("bearer request-token", request.get_header("Authorization"))

    def test_review_jwt_fails_closed_on_missing_audience_or_other_host(self) -> None:
        opener = Mock()
        env = {
            "ACTIONS_ID_TOKEN_REQUEST_URL": "https://other.example/api/actions/oidc?run=123",
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN": "request-token",
        }
        with patch.dict(os.environ, env, clear=True):
            with self.assertRaisesRegex(ValueError, "audience is missing"):
                review_receipt.review_token(opener, "")
            with self.assertRaisesRegex(ValueError, "unexpected Forgejo OIDC request URL"):
                review_receipt.review_token(opener, "u:review")
        opener.open.assert_not_called()

    def test_review_api_uses_integration_jwt(self) -> None:
        opener = Mock()
        opener.open.side_effect = [io.BytesIO(b"{}"), io.BytesIO(b"{}")]
        review_receipt.api_request(opener, "https://forgejo.svc.echoes/api/v1", "short-lived-jwt", "repos/lacuna/dione/pulls/1")
        request = opener.open.call_args.args[0]
        self.assertEqual("Bearer short-lived-jwt", request.get_header("Authorization"))
        self.assertEqual(
            "https://forgejo-http.forgejo.svc.cluster.local:3443/api/v1/repos/lacuna/dione/pulls/1",
            request.full_url,
        )
        self.assertEqual("forgejo.svc.echoes", request.get_header("Host"))
        review_receipt.api_request(
            opener,
            "https://forgejo.svc.echoes/api/v1",
            "short-lived-jwt",
            f"repos/lacuna/dione/statuses/{HEAD}",
            {"state": "failure", "context": "test", "description": "test", "target_url": "https://example.test"},
        )
        posted = opener.open.call_args.args[0]
        self.assertEqual("POST", posted.get_method())
        self.assertEqual(f"{review_receipt.INTERNAL_SERVICE}/api/v1/repos/lacuna/dione/statuses/{HEAD}", posted.full_url)
        self.assertEqual("forgejo.svc.echoes", posted.get_header("Host"))

    def test_review_api_requires_a_list_total(self) -> None:
        opener = Mock()
        response = JsonResponse(b"[]")
        opener.open.return_value = response
        with self.assertRaisesRegex(ValueError, "list total"):
            review_receipt.api_request(
                opener, "https://forgejo.svc.echoes/api/v1", "jwt",
                "repos/lacuna/dione/pulls/1/reviews?limit=50&page=1", with_total=True,
            )

    def test_trusted_complete_receipt_passes(self) -> None:
        approved = review()
        self.assertEqual((None, approved["html_url"]), evaluate(pr(), {"lain-construct": approved}))

    def test_untrusted_and_self_review_do_not_pass(self) -> None:
        error = evaluate(pr(), {})[0]
        assert error is not None
        self.assertIn("no current official approval", error)
        own = pr()
        own["user"]["login"] = "lain-construct"
        error = evaluate(own, {"lain-construct": review()})[0]
        assert error is not None
        self.assertIn("author cannot", error)

    def test_official_reviewer_is_eligible_unless_pr_author(self) -> None:
        for reviewer in ("human-blinker", "friend-of-lacuna", "lain-construct"):
            with self.subTest(reviewer=reviewer), patch.object(review_receipt, "complete_list", return_value=[review(author=reviewer)]):
                selected = review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)
                approvals = selected.approvals
                blockers = selected.blockers
                self.assertIsNone(evaluate(pr(), approvals, blockers)[0])
                own = pr()
                own["user"]["login"] = reviewer
                error = evaluate(own, approvals, blockers)[0]
                assert error is not None
                self.assertIn("author cannot", error)

    def test_stale_or_incomplete_receipt_does_not_pass(self) -> None:
        def result(body: str) -> str:
            error = evaluate(pr(), {"lain-construct": review(body=body)})[0]
            assert error is not None
            return error

        self.assertIn("PR head", result(receipt(head="c" * 40)))
        self.assertIn("PR head", result(receipt(head="a" * 39)))
        self.assertIn("PR base", result(receipt(base="c" * 40)))
        bad_pr = pr()
        bad_pr["base"]["sha"] = ""
        error = evaluate(bad_pr, {"lain-construct": review(body=receipt(base=""))})[0]
        assert error is not None
        self.assertIn("commit SHA", error)
        self.assertIn("Security", result(receipt().replace("Security:", "Secure:")))
        self.assertIn("unresolved", result(receipt().replace("Unresolved P1-P3: none", "Unresolved P1-P3: P2")))
        self.assertIn("not approved", result(receipt().replace("Decision: approve", "Decision: comment")))
        self.assertIn("specific evidence", result(receipt().replace("checked the changed behavior", "<evidence or finding disposition>")))
        self.assertIn("specific evidence", result(receipt().replace("Safety: pass - checked the changed behavior", "Safety: n/a - not applicable")))
        for placeholder in ("n/a - <specific reason>", "pass - <evidence or finding disposition>."):
            with self.subTest(placeholder=placeholder):
                body = receipt().replace("Safety: pass - checked the changed behavior", f"Safety: {placeholder}")
                self.assertIn("specific evidence", result(body))
        for replacement in ("Safety: pass", "Safety: fail - changed safety behavior"):
            with self.subTest(replacement=replacement):
                body = receipt().replace("Safety: pass - checked the changed behavior", replacement)
                self.assertIn("Safety", result(body))
        self.assertIn("unexpected", result(receipt() + "\nExtra: unreviewed"))
        self.assertIn("marker", result("I retract approval.\n" + receipt()))

    def test_latest_trusted_receipt_controls_status(self) -> None:
        earlier = review(number=1)
        later = review(number=2, body=receipt(head="c" * 40))
        with patch.object(review_receipt, "complete_list", return_value=[earlier, later]):
            selected = review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)
            approvals = selected.approvals
            blockers = selected.blockers
        error = evaluate(pr(), approvals, blockers)[0]
        assert error is not None
        self.assertIn("PR head", error)
        newest = review(number=3)
        with patch.object(review_receipt, "complete_list", return_value=[earlier, later, newest]):
            selected = review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)
            approvals = selected.approvals
            blockers = selected.blockers
        self.assertEqual((None, newest["html_url"]), evaluate(pr(), approvals, blockers))

    def test_one_construct_cannot_override_another_constructs_block(self) -> None:
        blocked = review(body=receipt().replace("Decision: approve", "Decision: comment"))
        approved = review(author="other-construct", number=2)
        with patch.object(review_receipt, "complete_list", return_value=[blocked, approved]):
            selected = review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)
            approvals = selected.approvals
            blockers = selected.blockers
        error = evaluate(pr(), approvals, blockers)[0]
        assert error is not None
        self.assertIn("not approved", error)
        resolved = review(number=3)
        with patch.object(review_receipt, "complete_list", return_value=[blocked, approved, resolved]):
            selected = review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)
            approvals = selected.approvals
            blockers = selected.blockers
        self.assertIsNone(evaluate(pr(), approvals, blockers)[0])

    def test_receipt_without_same_reviewers_official_approval_fails(self) -> None:
        with patch.object(review_receipt, "complete_list", return_value=[review(official=False)]):
            selected = review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)
            approvals = selected.approvals
            blockers = selected.blockers
        error = evaluate(pr(), approvals, blockers)[0]
        assert error is not None
        self.assertIn("no current official approval", error)

    def test_duplicate_field_does_not_pass(self) -> None:
        error = evaluate(pr(), {"lain-construct": review(body=receipt() + "\nSafety: pass - copied from above")})[0]
        assert error is not None
        self.assertIn("duplicate", error)

    def test_complete_list_collects_every_server_capped_page(self) -> None:
        first = [comment(number=number) for number in range(1, 51)]
        second = [comment(number=number) for number in range(51, 53)]
        with patch.object(review_receipt, "api_request", side_effect=[(first, 52), (second, 52), ([], 52)]) as request:
            items = review_receipt.complete_list(build_opener(), "https://forgejo.example/api/v1", "token", "repos/lacuna/dione/pulls?limit=50")
        self.assertEqual(52, len(items))
        self.assertEqual(
            ["repos/lacuna/dione/pulls?limit=50&page=1", "repos/lacuna/dione/pulls?limit=50&page=2", "repos/lacuna/dione/pulls?limit=50&page=3"],
            [call.args[3] for call in request.call_args_list],
        )

    def test_hidden_draft_reviews_do_not_block_visible_review_pagination(self) -> None:
        visible = [review(number=1)]
        with patch.object(review_receipt, "api_request", side_effect=[(visible, 2), ([], 2)]):
            self.assertEqual(
                visible,
                review_receipt.complete_list(build_opener(), "base", "token", "repos/lacuna/dione/pulls/1/reviews?limit=50", allow_hidden=True),
            )
        with patch.object(review_receipt, "api_request", side_effect=[(visible, 2), ([], 2)]):
            self.assertEqual(
                ({"lain-construct": visible[0]}, set()),
                selected_snapshots(review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)),
            )
        with patch.object(review_receipt, "api_request", side_effect=[([], 51), (visible, 51), ([], 51)]):
            self.assertEqual(
                visible,
                review_receipt.complete_list(build_opener(), "base", "token", "repos/lacuna/dione/pulls/1/reviews?limit=50", allow_hidden=True),
            )

    def test_paged_lists_reject_excessive_and_changing_totals(self) -> None:
        with patch.object(review_receipt, "api_request", side_effect=[([], 1_000_000), AssertionError("second request")]) as request:
            with self.assertRaisesRegex(ValueError, "page limit"):
                review_receipt.complete_list(build_opener(), "base", "token", "repos/lacuna/dione/pulls?limit=50")
        request.assert_called_once()
        first = [comment(number=number) for number in range(1, 51)]
        with patch.object(review_receipt, "api_request", side_effect=[(first, 51), ([comment(number=51)], 52)]):
            with self.assertRaisesRegex(ValueError, "changed during pagination"):
                review_receipt.complete_list(build_opener(), "base", "token", "repos/lacuna/dione/pulls?limit=50")

    def test_non_review_lists_cannot_hide_reported_items(self) -> None:
        with patch.object(review_receipt, "api_request", side_effect=[([comment()], 2), ([], 2)]):
            with self.assertRaisesRegex(ValueError, "incomplete list"):
                review_receipt.complete_list(build_opener(), "base", "token", "repos/lacuna/dione/pulls?limit=50")

    def test_complete_list_rejects_an_underreported_full_page(self) -> None:
        first = [comment(number=number) for number in range(1, 51)]
        second = [comment(number=number) for number in range(51, 101)]
        third = [comment(number=number) for number in range(101, 121)]
        with patch.object(review_receipt, "api_request", side_effect=[(first, 100), (second, 100), ([], 100)]):
            self.assertEqual(100, len(review_receipt.complete_list(build_opener(), "https://forgejo.example/api/v1", "token", "repos/lacuna/dione/pulls?limit=50")))
        with patch.object(review_receipt, "api_request", side_effect=[(first, 100), (second, 100), (third, 100)]):
            with self.assertRaisesRegex(ValueError, "underreported"):
                review_receipt.complete_list(build_opener(), "https://forgejo.example/api/v1", "token", "repos/lacuna/dione/pulls?limit=50")

    def test_issue_comment_cannot_supply_an_approval_receipt(self) -> None:
        with patch.object(review_receipt, "complete_list", return_value=[review(body="")]):
            selected = review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)
            approvals = selected.approvals
            blockers = selected.blockers
        error = evaluate(pr(), approvals, blockers)[0]
        assert error is not None
        self.assertIn("marker", error)

    def test_only_current_official_reviews_count(self) -> None:
        reviews = [review(author="outsider", number=1, official=False), review(number=2)]
        with patch.object(review_receipt, "complete_list", return_value=reviews):
            self.assertEqual(({"lain-construct": reviews[1]}, set()), selected_snapshots(review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)))
        for change in ({"stale": True}, {"dismissed": True}, {"commit_id": "c" * 40}, {"state": "REQUEST_CHANGES"}, {"state": "PENDING"}, {"state": "REQUEST_REVIEW"}):
            with self.subTest(change=change):
                candidate = review_receipt.object_fields(review(), "fixture")
                candidate.update(change)
                with patch.object(review_receipt, "complete_list", return_value=[candidate]):
                    selected = review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)
                    approved = selected.approvals
                    blocked = selected.blockers
                self.assertEqual({}, approved)
                self.assertEqual(REVIEWERS if change.get("state") == "REQUEST_CHANGES" else set(), blocked)

    def test_comment_review_does_not_erase_approval_but_request_changes_does(self) -> None:
        with patch.object(review_receipt, "complete_list", return_value=[review(number=1), review(number=2, state="COMMENT")]):
            self.assertEqual(({"lain-construct": review(number=1)}, set()), selected_snapshots(review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)))
        with patch.object(review_receipt, "complete_list", return_value=[review(number=1), review(number=2, state="REQUEST_CHANGES")]):
            self.assertEqual(({}, REVIEWERS), selected_snapshots(review_receipt.official_reviewers(build_opener(), "base", "token", "lacuna/dione", 1, HEAD)))

    def test_status_is_not_posted_when_pr_or_receipt_changes_mid_check(self) -> None:
        moved = pr()
        moved["head"]["sha"] = "c" * 40
        for second_pr, decisions in ((moved, []), (pr(), [selection({"lain-construct": review(body=receipt().replace("Decision: approve", "Decision: comment"))})])):
            with self.subTest(second_pr=second_pr["head"]["sha"]):
                calls = iter([selection({"lain-construct": review()}), *decisions])
                with (
                    patch.object(review_receipt, "api_request", side_effect=[pr(), {}, second_pr]) as request,
                    patch.object(review_receipt, "official_reviewers", side_effect=lambda *_: next(calls)) as official,
                ):
                    with self.assertRaisesRegex(ValueError, "moved|changed"):
                        review_receipt.check_pr(build_opener(), "base", "token", "lacuna/dione", 1)
                self.assertEqual(3, request.call_count, "a moved PR must retain only the pending status")
                self.assertEqual("pending", request.call_args_list[1].args[4]["state"])
                if second_pr["head"]["sha"] != HEAD:
                    self.assertEqual(1, official.call_count, "a moved PR must stop before re-reading reviews")

    def test_main_rejects_unexpected_api_or_repository(self) -> None:
        for api_url, repo in (("http://forgejo.example/api/v1", "lacuna/dione"), ("https://forgejo.example/api/v1", "lacuna/dione"), ("https://forgejo.svc.echoes/api/v1", "other/dione")):
            with self.subTest(api_url=api_url, repo=repo), patch.dict(os.environ, {"FORGEJO_API_URL": api_url, "FORGEJO_REPOSITORY": repo}):
                with self.assertRaisesRegex(ValueError, "unexpected Forgejo (API URL|repository)"):
                    review_receipt.main()

    def test_event_posts_status_for_exact_head(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text(json.dumps({"issue": {"number": 1}}), encoding="utf-8")
            env = {
                "REVIEW_AUDIENCE": "review-audience",
                "FORGEJO_API_URL": "https://forgejo.svc.echoes/api/v1",
                "FORGEJO_REPOSITORY": "lacuna/dione",
                "FORGEJO_EVENT_PATH": str(event_path),
                "FORGEJO_EVENT_NAME": "issue_comment",
            }
            cases = (
                ([review()], "success"),
                ([review(body="")], "failure"),
                ([review(official=False)], "failure"),
                ([review(state="REQUEST_CHANGES"), review(author="other-construct", number=2)], "failure"),
            )
            for reviews, expected_state in cases:
                posted: list[StatusPayload] = []

                def fake_api(_opener: OpenerDirector, _base_url: str, _token: str, path: str,
                             payload: StatusPayload | None = None, with_total: bool = False) -> object:
                    if path == "repos/lacuna/dione/pulls/1":
                        return pr()
                    if path == "repos/lacuna/dione/pulls/1/reviews?limit=50&page=1":
                        return reviews, len(reviews)
                    if path == "repos/lacuna/dione/pulls/1/reviews?limit=50&page=2":
                        return [], len(reviews)
                    if path == f"repos/lacuna/dione/statuses/{HEAD}":
                        if payload is None:
                            self.fail("status payload missing")
                        posted.append(payload)
                        return payload
                    self.fail(f"unexpected API path: {path}")

                with patch.dict(os.environ, env, clear=True), patch.object(review_receipt, "review_token", return_value="test-token"), patch.object(review_receipt, "api_request", side_effect=fake_api):
                    self.assertEqual(0, review_receipt.main())
                self.assertEqual(["pending", expected_state], [status["state"] for status in posted])
                self.assertTrue(all(status["context"] == "Dione review / Six lenses" for status in posted))

    def test_failed_refresh_invalidates_earlier_success(self) -> None:
        for failure in (OSError("review API unavailable"), IncompleteRead(b"truncated", 4)):
            with self.subTest(failure=failure):
                posted: list[StatusPayload] = []

                def fake_api(_opener: OpenerDirector, _base_url: str, _token: str, path: str,
                             payload: StatusPayload | None = None, with_total: bool = False) -> object:
                    if path == "repos/lacuna/dione/pulls/1":
                        return pr()
                    if path == f"repos/lacuna/dione/statuses/{HEAD}":
                        if payload is None:
                            self.fail("status payload missing")
                        posted.append(payload)
                        return payload
                    self.fail(f"unexpected API path: {path}")

                with (
                    patch.object(review_receipt, "api_request", side_effect=fake_api),
                    patch.object(review_receipt, "official_reviewers", side_effect=failure),
                ):
                    with self.assertRaises(type(failure)):
                        review_receipt.check_pr(build_opener(), "base", "token", "lacuna/dione", 1)
                self.assertEqual(["pending"], [status["state"] for status in posted])

    def test_success_is_revoked_when_receipt_changes_during_status_write(self) -> None:
        posted: list[StatusPayload] = []
        changed = False

        def fake_api(_opener: OpenerDirector, _base_url: str, _token: str, path: str,
                     payload: StatusPayload | None = None, with_total: bool = False) -> object:
            nonlocal changed
            if path == "repos/lacuna/dione/pulls/1":
                return pr()
            if path == f"repos/lacuna/dione/statuses/{HEAD}":
                if payload is None:
                    self.fail("status payload missing")
                posted.append(payload)
                if payload["state"] == "success":
                    changed = True
                return payload
            self.fail(f"unexpected API path: {path}")

        def fake_reviews(*_args: object) -> ReviewSelection:
            if changed:
                return selection({"lain-construct": review(body=receipt().replace("Decision: approve", "Decision: comment"))})
            return selection({"lain-construct": review()})

        with (
            patch.object(review_receipt, "api_request", side_effect=fake_api),
            patch.object(review_receipt, "official_reviewers", side_effect=fake_reviews),
        ):
            with self.assertRaisesRegex(ValueError, "changed after status"):
                review_receipt.check_pr(build_opener(), "base", "token", "lacuna/dione", 1)
        self.assertEqual(["pending", "success", "pending"], [status["state"] for status in posted])

    def test_ambiguous_success_write_is_revoked(self) -> None:
        posted: list[StatusPayload] = []

        def fake_api(_opener: OpenerDirector, _base_url: str, _token: str, path: str,
                     payload: StatusPayload | None = None, with_total: bool = False) -> object:
            if path == "repos/lacuna/dione/pulls/1":
                return pr()
            if path == f"repos/lacuna/dione/statuses/{HEAD}":
                if payload is None:
                    self.fail("status payload missing")
                posted.append(payload)
                if payload["state"] == "success":
                    raise TimeoutError("success may have been accepted")
                return payload
            self.fail(f"unexpected API path: {path}")

        with (
            patch.object(review_receipt, "api_request", side_effect=fake_api),
            patch.object(review_receipt, "official_reviewers", return_value=selection({"lain-construct": review()})),
        ):
            with self.assertRaisesRegex(TimeoutError, "may have been accepted"):
                review_receipt.check_pr(build_opener(), "base", "token", "lacuna/dione", 1)
        self.assertEqual(["pending", "success", "pending"], [status["state"] for status in posted])

    def test_success_is_revoked_when_post_write_read_fails(self) -> None:
        posted: list[StatusPayload] = []

        def fake_api(_opener: OpenerDirector, _base_url: str, _token: str, path: str,
                     payload: StatusPayload | None = None, with_total: bool = False) -> object:
            if path == "repos/lacuna/dione/pulls/1":
                return pr()
            if path == f"repos/lacuna/dione/statuses/{HEAD}":
                if payload is None:
                    self.fail("status payload missing")
                posted.append(payload)
                return payload
            self.fail(f"unexpected API path: {path}")

        def fake_reviews(*_args: object) -> ReviewSelection:
            if posted[-1]["state"] == "success":
                raise IncompleteRead(b"truncated response", 100)
            return selection({"lain-construct": review()})

        with (
            patch.object(review_receipt, "api_request", side_effect=fake_api),
            patch.object(review_receipt, "official_reviewers", side_effect=fake_reviews),
        ):
            with self.assertRaises(IncompleteRead):
                review_receipt.check_pr(build_opener(), "base", "token", "lacuna/dione", 1)
        self.assertEqual(["pending", "success", "pending"], [status["state"] for status in posted])

    def test_revocation_retries_a_transient_status_write_error(self) -> None:
        posted: list[StatusPayload] = []
        attempts = 0

        def fake_api(_opener: OpenerDirector, _base_url: str, _token: str, path: str,
                     payload: StatusPayload | None = None, with_total: bool = False) -> object:
            nonlocal attempts
            if path == "repos/lacuna/dione/pulls/1":
                return pr()
            if path == f"repos/lacuna/dione/statuses/{HEAD}":
                if payload is None:
                    self.fail("status payload missing")
                if payload["state"] == "pending" and posted:
                    attempts += 1
                    if attempts == 1:
                        raise OSError("transient status write failure")
                if payload is None:
                    self.fail("status payload missing")
                posted.append(payload)
                return payload
            self.fail(f"unexpected API path: {path}")

        def fake_reviews(*_args: object) -> ReviewSelection:
            if any(status["state"] == "success" for status in posted):
                return selection({"lain-construct": review(body=receipt().replace("Decision: approve", "Decision: comment"))})
            return selection({"lain-construct": review()})

        with (
            patch.object(review_receipt, "api_request", side_effect=fake_api),
            patch.object(review_receipt, "official_reviewers", side_effect=fake_reviews),
        ):
            with self.assertRaisesRegex(ValueError, "changed after status"):
                review_receipt.check_pr(build_opener(), "base", "token", "lacuna/dione", 1)
        self.assertEqual(2, attempts)
        self.assertEqual(["pending", "success", "pending"], [status["state"] for status in posted])

    def test_main_push_revokes_receipt_for_changed_base(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text(json.dumps({"ref": "refs/heads/main"}), encoding="utf-8")
            env = {
                "REVIEW_AUDIENCE": "review-audience",
                "FORGEJO_API_URL": "https://forgejo.svc.echoes/api/v1",
                "FORGEJO_REPOSITORY": "lacuna/dione",
                "FORGEJO_EVENT_PATH": str(event_path),
                "FORGEJO_EVENT_NAME": "push",
            }
            posted: list[StatusPayload] = []

            def fake_api(_opener: OpenerDirector, _base_url: str, _token: str, path: str,
                         payload: StatusPayload | None = None, with_total: bool = False) -> object:
                if path == "repos/lacuna/dione/pulls/1/reviews?limit=50&page=1":
                    return [review()], 1
                if path == "repos/lacuna/dione/pulls/1/reviews?limit=50&page=2":
                    return [], 1
                if path == "repos/lacuna/dione/pulls?state=open&limit=50&page=1":
                    return [{"id": 1, "number": 1, "base": {"ref": "main"}}], 1
                if path == "repos/lacuna/dione/pulls?state=open&limit=50&page=2":
                    return [], 1
                if path == "repos/lacuna/dione/pulls/1":
                    changed = pr()
                    changed["base"]["sha"] = "c" * 40
                    return changed
                if path == f"repos/lacuna/dione/statuses/{HEAD}":
                    if payload is None:
                        self.fail("status payload missing")
                    posted.append(payload)
                    return payload
                self.fail(f"unexpected API path: {path}")

            with patch.dict(os.environ, env), patch.object(review_receipt, "review_token", return_value="test-token"), patch.object(review_receipt, "api_request", side_effect=fake_api):
                self.assertEqual(0, review_receipt.main())
            self.assertEqual(["pending", "failure"], [status["state"] for status in posted])
            self.assertIn("PR base", posted[-1]["description"])

    def test_main_push_continues_after_one_pr_moves(self) -> None:
        workflow = (Path(__file__).parent.parent / "workflows" / "review-receipt.yml").read_text(encoding="utf-8")
        self.assertIn("FORGEJO_EVENT_NAME: ${{ forgejo.event_name }}", workflow)
        self.assertNotIn("  workflow_dispatch:", workflow)
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text(json.dumps({"ref": "refs/heads/main"}), encoding="utf-8")
            env = {
                "REVIEW_AUDIENCE": "review-audience",
                "FORGEJO_API_URL": "https://forgejo.svc.echoes/api/v1",
                "FORGEJO_REPOSITORY": "lacuna/dione",
                "FORGEJO_EVENT_PATH": str(event_path),
                "FORGEJO_EVENT_NAME": "push",
            }
            pulls = [
                {"id": number, "number": number, "base": {"ref": "main"}}
                for number in (1, 2)
            ]
            with (
                patch.dict(os.environ, env),
                patch.object(review_receipt, "review_token", return_value="test-token"),
                patch.object(review_receipt, "complete_list", return_value=pulls),
                patch.object(review_receipt, "check_pr", side_effect=[IncompleteRead(b"partial", 2), 0]) as check,
            ):
                with self.assertRaisesRegex(ValueError, "could not refresh 1"):
                    review_receipt.main()
                self.assertEqual([1, 2], [call.args[4] for call in check.call_args_list])

    def test_main_push_rejects_missing_open_pulls(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text(json.dumps({"ref": "refs/heads/main"}), encoding="utf-8")
            env = {
                "REVIEW_AUDIENCE": "review-audience",
                "FORGEJO_API_URL": "https://forgejo.svc.echoes/api/v1",
                "FORGEJO_REPOSITORY": "lacuna/dione",
                "FORGEJO_EVENT_PATH": str(event_path),
                "FORGEJO_EVENT_NAME": "push",
            }
            visible = [{"id": 1, "number": 1, "base": {"ref": "main"}}]
            with (
                patch.dict(os.environ, env),
                patch.object(review_receipt, "review_token", return_value="test-token"),
                patch.object(review_receipt, "api_request", side_effect=[(visible, 2), ([], 2)]),
                patch.object(review_receipt, "check_pr") as check,
            ):
                with self.assertRaisesRegex(ValueError, "incomplete list"):
                    review_receipt.main()
                check.assert_not_called()


    def test_author_validation_happens_after_pending(self) -> None:
        posted: list[StatusPayload] = []
        self.assertEqual(0, check_with([pr(), pr(), pr()], [[review()]] * 3, posted))
        self.assertEqual(["pending", "success"], [item["state"] for item in posted])
        users: tuple[object, ...] = (None, [], "private author", {}, {"login": 1}, {"login": ""})
        for user in users:
            malformed = review_receipt.object_fields(pr(), "fixture")
            malformed["user"] = user
            posted = []
            with self.subTest(user=user), self.assertRaisesRegex(ValueError, "PR.user"):
                check_with([malformed], [], posted)
            self.assertEqual(["pending"], [item["state"] for item in posted])
        missing = review_receipt.object_fields(pr(), "fixture")
        del missing["user"]
        posted = []
        with self.assertRaisesRegex(ValueError, "PR.user"):
            check_with([missing], [], posted)
        self.assertEqual(["pending"], [item["state"] for item in posted])

    def test_initial_routing_validation_and_skip_order(self) -> None:
        skipped_prs: tuple[dict[str, object], ...] = ({"state": "closed"}, {"state": "open", "base": {"ref": "other"}})
        for value in skipped_prs:
            posted: list[StatusPayload] = []
            self.assertEqual(0, check_with([value], [], posted))
            self.assertEqual([], posted)
        changes: tuple[dict[str, object], ...] = ({"head": {"sha": 1}}, {"head": {"sha": "invalid"}},
                       {"base": {"ref": "main", "sha": None}}, {"html_url": []}, {"html_url": ""})
        for change in changes:
            value = review_receipt.object_fields(pr(), "fixture")
            value.update(change)
            posted = []
            with self.subTest(change=change), self.assertRaises(ValueError):
                check_with([value], [], posted)
            self.assertEqual([], posted)

    def test_selected_review_validation_preserves_blockers_without_receipts(self) -> None:
        blocker = review_receipt.object_fields(review(state="REQUEST_CHANGES"), "fixture")
        del blocker["html_url"]
        del blocker["body"]
        posted: list[StatusPayload] = []
        self.assertEqual(0, check_with([pr(), pr()], [[blocker], [blocker]], posted))
        self.assertEqual(["pending", "failure"], [item["state"] for item in posted])
        self.assertEqual("official reviewer requested changes", posted[-1]["description"])
        for state in ("APPROVED", "REQUEST_CHANGES"):
            users: tuple[object, ...] = (None, {}, [], 1, {"login": True}, {"login": ""})
            for user in users:
                malformed = review_receipt.object_fields(review(state=state), "fixture")
                malformed["user"] = user
                posted = []
                with self.subTest(state=state, user=user), self.assertRaisesRegex(ValueError, "review.user"):
                    check_with([pr()], [[malformed]], posted)
                self.assertEqual(["pending"], [item["state"] for item in posted])
        changes: tuple[dict[str, object], ...] = ({"body": []}, {"body": 1}, {"html_url": None}, {"html_url": ""})
        for change in changes:
            malformed = review_receipt.object_fields(review(), "fixture")
            malformed.update(change)
            posted = []
            with self.subTest(change=change), self.assertRaisesRegex(ValueError, "approval"):
                check_with([pr()], [[malformed]], posted)
            self.assertEqual(["pending"], [item["state"] for item in posted])
        for body in (None, ""):
            valid = review_receipt.object_fields(review(), "fixture")
            valid["body"] = body
            posted = []
            self.assertEqual(0, check_with([pr(), pr()], [[valid], [valid]], posted))
            self.assertEqual("failure", posted[-1]["state"])

    def test_ineligible_reviews_need_no_user_and_flags_do_not_coerce(self) -> None:
        for field, value in (("official", 1), ("official", "true"), ("official", None),
                             ("stale", 0), ("stale", "false"), ("stale", None),
                             ("dismissed", 0), ("dismissed", "false"), ("dismissed", None)):
            ignored = review_receipt.object_fields(review(), "fixture")
            ignored[field] = value
            del ignored["user"]
            posted: list[StatusPayload] = []
            self.assertEqual(0, check_with([pr(), pr(), pr()], [[ignored, review(number=2)]] * 3, posted))
            self.assertEqual(["pending", "success"], [item["state"] for item in posted])

    def test_list_boundary_rejects_malformed_identity_and_final_page(self) -> None:
        for value in (True, False, 0, -1, 1.0, "1", None):
            with self.subTest(value=value), patch.object(review_receipt, "api_request", return_value=([{"id": value}], 1)):
                with self.assertRaisesRegex(ValueError, "positive integer"):
                    review_receipt.complete_list(build_opener(), "base", "token", "path?limit=50")
        entry_batches: tuple[list[object], ...] = ([{}], [None], [1], [{"id": 1}, {"id": 1}])
        for entries in entry_batches:
            with self.subTest(entries=entries), patch.object(review_receipt, "api_request", return_value=(entries, len(entries))):
                with self.assertRaises(ValueError):
                    review_receipt.complete_list(build_opener(), "base", "token", "path?limit=50")
        final_pages: tuple[object, ...] = (None, {}, "", False)
        for final in final_pages:
            with self.subTest(final=final), patch.object(review_receipt, "api_request", side_effect=[([{"id": 1}], 1), (final, 1)]):
                with self.assertRaisesRegex(ValueError, "final list page"):
                    review_receipt.complete_list(build_opener(), "base", "token", "path?limit=50")
        with patch.object(review_receipt, "api_request", side_effect=[([{"id": 1}], 1), ([], 1)]):
            self.assertEqual([{"id": 1}], review_receipt.complete_list(build_opener(), "base", "token", "path?limit=50"))

    def test_event_boundary_routes_only_valid_positive_numbers(self) -> None:
        events: tuple[object, ...] = ({"number": 2}, {"number": None, "issue": {"number": 2}})
        for event in events:
            with patch.object(review_receipt, "check_pr", return_value=0) as check:
                self.assertEqual(0, run_event(event, "pull_request_target"))
            self.assertEqual(2, check.call_args.args[4])
        invalid_events: tuple[object, ...] = (None, [], "secret", {"number": True}, {"number": 0, "issue": {"number": 2}},
                      {"number": "2"}, {"issue": []}, {"issue": {"number": -1}})
        for event in invalid_events:
            with self.subTest(event=event), patch.object(review_receipt, "check_pr") as check:
                with self.assertRaises(ValueError):
                    run_event(event, "pull_request_target")
                check.assert_not_called()
        with patch.object(review_receipt, "check_pr") as check:
            with self.assertRaisesRegex(ValueError, "event name"):
                run_event({"number": 1}, "unknown")
            check.assert_not_called()

    def test_issue_404_exemption_requires_an_ordinary_issue_marker(self) -> None:
        markers: tuple[object, ...] = (None, {})
        for marker in markers:
            event = {"issue": {"number": 1, "pull_request": marker}}
            with patch.object(review_receipt, "api_request", side_effect=HTTPError("url", 404, "missing", Message(), None)) as request:
                if marker is None:
                    self.assertEqual(0, run_event(event, "issue_comment"))
                else:
                    with self.assertRaises(HTTPError):
                        run_event(event, "issue_comment")
                request.assert_called_once()
        invalid_markers: tuple[object, ...] = (False, "", 1, [])
        for marker in invalid_markers:
            with patch.object(review_receipt, "api_request") as request:
                with self.assertRaisesRegex(ValueError, "pull_request"):
                    run_event({"issue": {"number": 1, "pull_request": marker}}, "issue_comment")
                request.assert_not_called()

    def test_push_continues_after_malformed_item_but_rejects_global_list_damage(self) -> None:
        for malformed in ({"id": 1, "number": "secret", "base": {"ref": "main"}},
                          {"id": 1, "number": 1, "base": None}):
            with (patch.object(review_receipt, "complete_list", return_value=[malformed, {"id": 2, "number": 2, "base": {"ref": "main"}}]),
                  patch.object(review_receipt, "check_pr", return_value=0) as check):
                with self.assertRaisesRegex(ValueError, "could not refresh 1"):
                    run_event({"ref": "refs/heads/main"}, "push")
                self.assertEqual([2], [call.args[4] for call in check.call_args_list])
        with (patch.object(review_receipt, "api_request", return_value=([{"id": 1}, {"id": 1}], 2)),
              patch.object(review_receipt, "check_pr") as check):
            with self.assertRaisesRegex(ValueError, "unique item"):
                run_event({"ref": "refs/heads/main"}, "push")
            check.assert_not_called()

    def test_freshness_compares_raw_approvals_but_only_blocker_logins(self) -> None:
        original = review_receipt.object_fields(review(), "fixture")
        changed = dict(original, extra={"nested": [1]})
        posted: list[StatusPayload] = []
        with self.assertRaisesRegex(ValueError, "changed during check"):
            check_with([pr(), pr(), pr()], [[original], [changed], [changed]], posted)
        self.assertEqual(["pending"], [item["state"] for item in posted])
        blocked = review_receipt.object_fields(review(state="REQUEST_CHANGES"), "fixture")
        changed = dict(blocked, body=["unused"], html_url=None, extra={"nested": [1]})
        posted = []
        self.assertEqual(0, check_with([pr(), pr()], [[blocked], [changed]], posted))
        self.assertEqual(["pending", "failure"], [item["state"] for item in posted])

    def test_malformed_refresh_leaves_pending_and_post_success_revokes(self) -> None:
        for after_success in (False, True):
            malformed_prs: tuple[object, ...] = (None, {"user": []})
            for malformed_pr in malformed_prs:
                posted: list[StatusPayload] = []
                prs: list[object] = [pr(), pr(), malformed_pr] if after_success else [pr(), malformed_pr]
                with self.subTest(after_success=after_success), self.assertRaises(ValueError):
                    check_with(prs, [[review()]] * 3, posted)
                self.assertEqual(["pending", "success", "pending"] if after_success else ["pending"], [item["state"] for item in posted])
            malformed_review = review_receipt.object_fields(review(), "fixture")
            malformed_review["user"] = None
            posted = []
            batches: list[list[object]] = [[review()], [review()], [malformed_review]] if after_success else [[review()], [malformed_review]]
            with self.assertRaisesRegex(ValueError, "review.user"):
                check_with([pr()] * 3, batches, posted)
            self.assertEqual(["pending", "success", "pending"] if after_success else ["pending"], [item["state"] for item in posted])

    def test_all_three_revocation_attempts_are_made_on_failure(self) -> None:
        posted: list[StatusPayload] = []

        def fake_api(_opener: OpenerDirector, _base: str, _token: str, _path: str,
                     payload: StatusPayload | None = None, with_total: bool = False) -> object:
            if payload is None:
                return pr()
            posted.append(payload)
            if len(posted) > 1:
                raise OSError("write failed")
            return {}

        with (patch.object(review_receipt, "api_request", side_effect=fake_api),
              patch.object(review_receipt, "official_reviewers", return_value=selection({"lain-construct": review()}))):
            with self.assertRaisesRegex(OSError, "write failed"):
                review_receipt.check_pr(build_opener(), "base", "token", "lacuna/dione", 1)
        self.assertEqual(["pending", "success", "pending", "pending", "pending"], [item["state"] for item in posted])

    def test_oidc_response_shapes_do_not_mask_invalid_tokens(self) -> None:
        env = {"ACTIONS_ID_TOKEN_REQUEST_URL": "https://forgejo.svc.echoes/api/actions/oidc",
               "ACTIONS_ID_TOKEN_REQUEST_TOKEN": "fake"}
        values: tuple[object, ...] = (None, [], "private", {}, {"value": 1}, {"value": ""})
        for value in values:
            opener = Mock(spec=OpenerDirector)
            opener.open.return_value = io.BytesIO(json.dumps(value).encode())
            output = io.StringIO()
            with patch.dict(os.environ, env, clear=True), redirect_stdout(output):
                with self.assertRaises(ValueError):
                    review_receipt.review_token(opener, "test")
            self.assertEqual("", output.getvalue())

    def test_status_response_json_is_consumed_and_failed_success_is_revoked(self) -> None:
        for response in (b'{"ignored":[true,null]}', b'not json'):
            opener = Mock(spec=OpenerDirector)
            bodies = [json.dumps(pr()).encode(), b'{}', json.dumps(pr()).encode(), response, json.dumps(pr()).encode()]
            opener.open.side_effect = [io.BytesIO(body) for body in bodies]
            with patch.object(review_receipt, "official_reviewers", return_value=selection({"lain-construct": review()})):
                if response == b'not json':
                    with self.assertRaises(json.JSONDecodeError):
                        review_receipt.check_pr(opener, review_receipt.PUBLIC_API, "fake", "lacuna/dione", 1)
                else:
                    self.assertEqual(0, review_receipt.check_pr(opener, review_receipt.PUBLIC_API, "fake", "lacuna/dione", 1))
            statuses = [json.loads(call.args[0].data)["state"] for call in opener.open.call_args_list if call.args[0].data]
            self.assertEqual(["pending", "success", "pending"] if response == b'not json' else ["pending", "success"], statuses)

    def test_interrupted_success_response_is_revoked(self) -> None:
        class InterruptedResponse(io.BytesIO):
            def read(self, size: int | None = -1) -> bytes:
                raise IncompleteRead(b"partial", 4)

        opener = Mock(spec=OpenerDirector)
        opener.open.side_effect = [io.BytesIO(json.dumps(pr()).encode()), io.BytesIO(b'{}'),
                                   io.BytesIO(json.dumps(pr()).encode()), InterruptedResponse(), io.BytesIO(b'{}')]
        with patch.object(review_receipt, "official_reviewers", return_value=selection({"lain-construct": review()})):
            with self.assertRaises(IncompleteRead):
                review_receipt.check_pr(opener, review_receipt.PUBLIC_API, "fake", "lacuna/dione", 1)
        statuses = [json.loads(call.args[0].data)["state"] for call in opener.open.call_args_list if call.args[0].data]
        self.assertEqual(["pending", "success", "pending"], statuses)

    def test_python_preflight_enforces_minimum_before_checker(self) -> None:
        root = Path(__file__).parents[2]
        workflow = (root / ".forgejo/workflows/review-receipt.yml").read_text(encoding="utf-8")
        local = (root / "scripts/pr-ready.sh").read_text(encoding="utf-8")
        line = next(line.strip() for line in workflow.splitlines() if "python3 -c " in line)
        self.assertIn("if ! " + line + "; then", local)
        code = line.removeprefix("python3 -c '")[:-1]
        actual = subprocess.run([sys.executable, "-B", "-c", code], capture_output=True, text=True)
        self.assertEqual(0, actual.returncode)
        self.assertIn(sys.version.split()[0], actual.stdout)
        old = subprocess.run([sys.executable, "-B", "-c", "import sys; sys.version_info = (3, 8); " + code], capture_output=True, text=True)
        self.assertEqual(1, old.returncode)
        self.assertIn("Python 3.9 or newer is required", old.stderr)
        self.assertLess(workflow.index(line), workflow.index("python3 .forgejo/scripts/review_receipt.py"))

    def test_review_workflow_keeps_its_trusted_source_and_refresh_events(self) -> None:
        workflow = (Path(__file__).parent.parent / "workflows" / "review-receipt.yml").read_text(encoding="utf-8")
        events = workflow.split("\non:\n", 1)[1].split("\nconcurrency:", 1)[0].strip()
        self.assertEqual(
            "\n".join((
                "push:",
                "    branches: [main]",
                "  pull_request_target:",
                "    types: [opened, synchronize, reopened, edited]",
                "  issue_comment:",
                "    types: [created, edited, deleted]",
            )),
            events,
        )
        self.assertEqual(
            ["- uses: https://data.forgejo.org/actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4"],
            [line.strip() for line in workflow.splitlines() if line.lstrip().removeprefix("- ").startswith("uses:")],
        )
        self.assertEqual(
            ["run: |"],
            [line.strip() for line in workflow.splitlines() if line.lstrip().removeprefix("- ").startswith("run:")],
        )
        self.assertIn("          set -eu\n", workflow)
        self.assertIn("sys.version_info >= (3, 9)", workflow)
        self.assertIn("print(sys.version)", workflow)
        self.assertIn("          python3 .forgejo/scripts/review_receipt.py\n", workflow)
        self.assertEqual(1, workflow.count("ref: main"))
        for required in (
            "  push:\n    branches: [main]",
            "  pull_request_target:\n    types: [opened, synchronize, reopened, edited]",
            "  issue_comment:\n    types: [created, edited, deleted]",
            "    enable-openid-connect: true",
            "          ref: main",
            "          persist-credentials: false",
            "          REVIEW_AUDIENCE: ${{ vars.DIONE_REVIEW_AUDIENCE }}",
        ):
            with self.subTest(required=required):
                self.assertIn(required, workflow)


if __name__ == "__main__":
    unittest.main()
