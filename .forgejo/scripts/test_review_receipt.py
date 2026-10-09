import io
import json
import os
import tempfile
import threading
import unittest
from http.client import IncompleteRead
from contextlib import redirect_stdout
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from unittest.mock import Mock, patch
from urllib.request import Request, build_opener

import review_receipt
from review_receipt import evaluate


HEAD = "a" * 40
BASE = "b" * 40
REVIEWERS = {"lain-construct"}


def pr():
    return {
        "state": "open",
        "head": {"sha": HEAD},
        "base": {"sha": BASE, "ref": "main"},
        "user": {"login": "author"},
        "html_url": "https://forgejo.example/pr/1",
    }


def receipt(head=HEAD, base=BASE):
    lines = ["DIONE-SIX-LENS-RECEIPT v1", f"Head: {head}", f"Base: {base}", "Decision: approve"]
    lines += [
        f"{lens}: pass - checked the changed behavior"
        for lens in ("Safety", "Design", "Security", "Privacy", "Idiomacy", "Tests")
    ]
    lines.append("Unresolved P1-P3: none")
    return "\n".join(lines)


def comment(body=None, author="lain-construct", number=1):
    return {
        "id": number,
        "body": receipt() if body is None else body,
        "user": {"login": author},
        "html_url": "https://forgejo.example/pr/1#comment-1",
    }


def review(author="lain-construct", number=1, state="APPROVED", official=True, head=HEAD, body=None):
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


class ReceiptTests(unittest.TestCase):
    def test_redirect_does_not_forward_integration_token(self):
        requests = []

        class RedirectHandler(BaseHTTPRequestHandler):
            def do_GET(self):
                requests.append((self.path, self.headers.get("Authorization")))
                if self.path == "/start":
                    self.send_response(302)
                    self.send_header("Location", "/leak")
                else:
                    self.send_response(200)
                self.end_headers()

            def log_message(self, *_args):
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

    def test_main_installs_redirect_block(self):
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

    def test_review_jwt_uses_internal_service_and_public_host(self):
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

    def test_review_jwt_fails_closed_on_missing_audience_or_other_host(self):
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

    def test_review_api_uses_integration_jwt(self):
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
            {"state": "failure"},
        )
        posted = opener.open.call_args.args[0]
        self.assertEqual("POST", posted.get_method())
        self.assertEqual(f"{review_receipt.INTERNAL_SERVICE}/api/v1/repos/lacuna/dione/statuses/{HEAD}", posted.full_url)
        self.assertEqual("forgejo.svc.echoes", posted.get_header("Host"))

    def test_review_api_requires_a_list_total(self):
        opener = Mock()
        response = io.BytesIO(b"[]")
        response.headers = {}
        opener.open.return_value = response
        with self.assertRaisesRegex(ValueError, "list total"):
            review_receipt.api_request(
                opener, "https://forgejo.svc.echoes/api/v1", "jwt",
                "repos/lacuna/dione/pulls/1/reviews?limit=50&page=1", with_total=True,
            )

    def test_trusted_complete_receipt_passes(self):
        approved = review()
        self.assertEqual((None, approved["html_url"]), evaluate(pr(), {"lain-construct": approved}))

    def test_untrusted_and_self_review_do_not_pass(self):
        self.assertIn("no current official approval", evaluate(pr(), {})[0])
        own = pr()
        own["user"]["login"] = "lain-construct"
        self.assertIn("author cannot", evaluate(own, {"lain-construct": review()})[0])

    def test_official_reviewer_is_eligible_unless_pr_author(self):
        for reviewer in ("human-blinker", "friend-of-lacuna", "lain-construct"):
            with self.subTest(reviewer=reviewer), patch.object(review_receipt, "complete_list", return_value=[review(author=reviewer)]):
                approvals, blockers = review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD)
                self.assertIsNone(evaluate(pr(), approvals, blockers)[0])
                own = pr()
                own["user"]["login"] = reviewer
                self.assertIn("author cannot", evaluate(own, approvals, blockers)[0])

    def test_stale_or_incomplete_receipt_does_not_pass(self):
        def result(body):
            return evaluate(pr(), {"lain-construct": review(body=body)})[0]

        self.assertIn("PR head", result(receipt(head="c" * 40)))
        self.assertIn("PR head", result(receipt(head="a" * 39)))
        self.assertIn("PR base", result(receipt(base="c" * 40)))
        bad_pr = pr()
        bad_pr["base"]["sha"] = ""
        self.assertIn("commit SHA", evaluate(bad_pr, {"lain-construct": review(body=receipt(base=""))})[0])
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

    def test_latest_trusted_receipt_controls_status(self):
        earlier = review(number=1)
        later = review(number=2, body=receipt(head="c" * 40))
        with patch.object(review_receipt, "complete_list", return_value=[earlier, later]):
            approvals, blockers = review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD)
        self.assertIn("PR head", evaluate(pr(), approvals, blockers)[0])
        newest = review(number=3)
        with patch.object(review_receipt, "complete_list", return_value=[earlier, later, newest]):
            approvals, blockers = review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD)
        self.assertEqual((None, newest["html_url"]), evaluate(pr(), approvals, blockers))

    def test_one_construct_cannot_override_another_constructs_block(self):
        blocked = review(body=receipt().replace("Decision: approve", "Decision: comment"))
        approved = review(author="other-construct", number=2)
        with patch.object(review_receipt, "complete_list", return_value=[blocked, approved]):
            approvals, blockers = review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD)
        self.assertIn("not approved", evaluate(pr(), approvals, blockers)[0])
        resolved = review(number=3)
        with patch.object(review_receipt, "complete_list", return_value=[blocked, approved, resolved]):
            approvals, blockers = review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD)
        self.assertIsNone(evaluate(pr(), approvals, blockers)[0])

    def test_receipt_without_same_reviewers_official_approval_fails(self):
        with patch.object(review_receipt, "complete_list", return_value=[review(official=False)]):
            approvals, blockers = review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD)
        self.assertIn("no current official approval", evaluate(pr(), approvals, blockers)[0])

    def test_duplicate_field_does_not_pass(self):
        self.assertIn("duplicate", evaluate(pr(), {"lain-construct": review(body=receipt() + "\nSafety: pass - copied from above")})[0])

    def test_complete_list_collects_every_server_capped_page(self):
        first = [comment(number=number) for number in range(1, 51)]
        second = [comment(number=number) for number in range(51, 53)]
        with patch.object(review_receipt, "api_request", side_effect=[(first, 52), (second, 52), ([], 52)]) as request:
            items = review_receipt.complete_list(None, "https://forgejo.example/api/v1", "token", "repos/lacuna/dione/pulls?limit=50")
        self.assertEqual(52, len(items))
        self.assertEqual(
            ["repos/lacuna/dione/pulls?limit=50&page=1", "repos/lacuna/dione/pulls?limit=50&page=2", "repos/lacuna/dione/pulls?limit=50&page=3"],
            [call.args[3] for call in request.call_args_list],
        )

    def test_hidden_draft_reviews_do_not_block_visible_review_pagination(self):
        visible = [review(number=1)]
        with patch.object(review_receipt, "api_request", side_effect=[(visible, 2), ([], 2)]):
            self.assertEqual(
                visible,
                review_receipt.complete_list(None, "base", "token", "repos/lacuna/dione/pulls/1/reviews?limit=50", allow_hidden=True),
            )
        with patch.object(review_receipt, "api_request", side_effect=[(visible, 2), ([], 2)]):
            self.assertEqual(
                ({"lain-construct": visible[0]}, set()),
                review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD),
            )
        with patch.object(review_receipt, "api_request", side_effect=[([], 51), (visible, 51), ([], 51)]):
            self.assertEqual(
                visible,
                review_receipt.complete_list(None, "base", "token", "repos/lacuna/dione/pulls/1/reviews?limit=50", allow_hidden=True),
            )

    def test_paged_lists_reject_excessive_and_changing_totals(self):
        with patch.object(review_receipt, "api_request", side_effect=[([], 1_000_000), AssertionError("second request")]) as request:
            with self.assertRaisesRegex(ValueError, "page limit"):
                review_receipt.complete_list(None, "base", "token", "repos/lacuna/dione/pulls?limit=50")
        request.assert_called_once()
        first = [comment(number=number) for number in range(1, 51)]
        with patch.object(review_receipt, "api_request", side_effect=[(first, 51), ([comment(number=51)], 52)]):
            with self.assertRaisesRegex(ValueError, "changed during pagination"):
                review_receipt.complete_list(None, "base", "token", "repos/lacuna/dione/pulls?limit=50")

    def test_non_review_lists_cannot_hide_reported_items(self):
        with patch.object(review_receipt, "api_request", side_effect=[([comment()], 2), ([], 2)]):
            with self.assertRaisesRegex(ValueError, "incomplete list"):
                review_receipt.complete_list(None, "base", "token", "repos/lacuna/dione/pulls?limit=50")

    def test_complete_list_rejects_an_underreported_full_page(self):
        first = [comment(number=number) for number in range(1, 51)]
        second = [comment(number=number) for number in range(51, 101)]
        third = [comment(number=number) for number in range(101, 121)]
        with patch.object(review_receipt, "api_request", side_effect=[(first, 100), (second, 100), ([], 100)]):
            self.assertEqual(100, len(review_receipt.complete_list(None, "https://forgejo.example/api/v1", "token", "repos/lacuna/dione/pulls?limit=50")))
        with patch.object(review_receipt, "api_request", side_effect=[(first, 100), (second, 100), (third, 100)]):
            with self.assertRaisesRegex(ValueError, "underreported"):
                review_receipt.complete_list(None, "https://forgejo.example/api/v1", "token", "repos/lacuna/dione/pulls?limit=50")

    def test_issue_comment_cannot_supply_an_approval_receipt(self):
        with patch.object(review_receipt, "complete_list", return_value=[review(body="")]):
            approvals, blockers = review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD)
        self.assertIn("marker", evaluate(pr(), approvals, blockers)[0])

    def test_only_current_official_reviews_count(self):
        reviews = [review(author="outsider", number=1, official=False), review(number=2)]
        with patch.object(review_receipt, "complete_list", return_value=reviews):
            self.assertEqual(({"lain-construct": reviews[1]}, set()), review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD))
        for change in ({"stale": True}, {"dismissed": True}, {"commit_id": "c" * 40}, {"state": "REQUEST_CHANGES"}, {"state": "PENDING"}, {"state": "REQUEST_REVIEW"}):
            with self.subTest(change=change):
                candidate = review(**{key: value for key, value in change.items() if key in {"state"}})
                candidate.update(change)
                with patch.object(review_receipt, "complete_list", return_value=[candidate]):
                    approved, blocked = review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD)
                self.assertEqual({}, approved)
                self.assertEqual(REVIEWERS if change.get("state") == "REQUEST_CHANGES" else set(), blocked)

    def test_comment_review_does_not_erase_approval_but_request_changes_does(self):
        with patch.object(review_receipt, "complete_list", return_value=[review(number=1), review(number=2, state="COMMENT")]):
            self.assertEqual(({"lain-construct": review(number=1)}, set()), review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD))
        with patch.object(review_receipt, "complete_list", return_value=[review(number=1), review(number=2, state="REQUEST_CHANGES")]):
            self.assertEqual(({}, REVIEWERS), review_receipt.official_reviewers(None, "base", "token", "lacuna/dione", 1, HEAD))

    def test_status_is_not_posted_when_pr_or_receipt_changes_mid_check(self):
        moved = pr()
        moved["head"]["sha"] = "c" * 40
        for second_pr, decisions in ((moved, []), (pr(), [({"lain-construct": review(body=receipt().replace("Decision: approve", "Decision: comment"))}, set())])):
            with self.subTest(second_pr=second_pr["head"]["sha"]):
                calls = iter([({"lain-construct": review()}, set()), *decisions])
                with (
                    patch.object(review_receipt, "api_request", side_effect=[pr(), {}, second_pr]) as request,
                    patch.object(review_receipt, "official_reviewers", side_effect=lambda *_: next(calls)) as official,
                ):
                    with self.assertRaisesRegex(ValueError, "moved|changed"):
                        review_receipt.check_pr(None, "base", "token", "lacuna/dione", 1)
                self.assertEqual(3, request.call_count, "a moved PR must retain only the pending status")
                self.assertEqual("pending", request.call_args_list[1].args[4]["state"])
                if second_pr["head"]["sha"] != HEAD:
                    self.assertEqual(1, official.call_count, "a moved PR must stop before re-reading reviews")

    def test_main_rejects_unexpected_api_or_repository(self):
        for api_url, repo in (("http://forgejo.example/api/v1", "lacuna/dione"), ("https://forgejo.example/api/v1", "lacuna/dione"), ("https://forgejo.svc.echoes/api/v1", "other/dione")):
            with self.subTest(api_url=api_url, repo=repo), patch.dict(os.environ, {"FORGEJO_API_URL": api_url, "FORGEJO_REPOSITORY": repo}):
                with self.assertRaisesRegex(ValueError, "unexpected Forgejo API or repository"):
                    review_receipt.main()

    def test_event_posts_status_for_exact_head(self):
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
                posted = []

                def fake_api(_opener, _base_url, _token, path, payload=None, with_total=False):
                    if path == "repos/lacuna/dione/pulls/1":
                        return pr()
                    if path == "repos/lacuna/dione/pulls/1/reviews?limit=50&page=1":
                        return reviews, len(reviews)
                    if path == "repos/lacuna/dione/pulls/1/reviews?limit=50&page=2":
                        return [], len(reviews)
                    if path == f"repos/lacuna/dione/statuses/{HEAD}":
                        posted.append(payload)
                        return payload
                    self.fail(f"unexpected API path: {path}")

                with patch.dict(os.environ, env, clear=True), patch.object(review_receipt, "review_token", return_value="test-token"), patch.object(review_receipt, "api_request", side_effect=fake_api):
                    self.assertEqual(0, review_receipt.main())
                self.assertEqual(["pending", expected_state], [status["state"] for status in posted])
                self.assertTrue(all(status["context"] == "Dione review / Six lenses" for status in posted))

    def test_failed_refresh_invalidates_earlier_success(self):
        for failure in (OSError("review API unavailable"), IncompleteRead(b"truncated", 4)):
            with self.subTest(failure=failure):
                posted = []

                def fake_api(_opener, _base_url, _token, path, payload=None, with_total=False):
                    if path == "repos/lacuna/dione/pulls/1":
                        return pr()
                    if path == f"repos/lacuna/dione/statuses/{HEAD}":
                        posted.append(payload)
                        return payload
                    self.fail(f"unexpected API path: {path}")

                with (
                    patch.object(review_receipt, "api_request", side_effect=fake_api),
                    patch.object(review_receipt, "official_reviewers", side_effect=failure),
                ):
                    with self.assertRaises(type(failure)):
                        review_receipt.check_pr(None, "base", "token", "lacuna/dione", 1)
                self.assertEqual(["pending"], [status["state"] for status in posted])

    def test_success_is_revoked_when_receipt_changes_during_status_write(self):
        posted = []
        changed = False

        def fake_api(_opener, _base_url, _token, path, payload=None, with_total=False):
            nonlocal changed
            if path == "repos/lacuna/dione/pulls/1":
                return pr()
            if path == f"repos/lacuna/dione/statuses/{HEAD}":
                posted.append(payload)
                if payload["state"] == "success":
                    changed = True
                return payload
            self.fail(f"unexpected API path: {path}")

        def fake_reviews(*_args):
            if changed:
                return {"lain-construct": review(body=receipt().replace("Decision: approve", "Decision: comment"))}, set()
            return {"lain-construct": review()}, set()

        with (
            patch.object(review_receipt, "api_request", side_effect=fake_api),
            patch.object(review_receipt, "official_reviewers", side_effect=fake_reviews),
        ):
            with self.assertRaisesRegex(ValueError, "changed after status"):
                review_receipt.check_pr(None, "base", "token", "lacuna/dione", 1)
        self.assertEqual(["pending", "success", "pending"], [status["state"] for status in posted])

    def test_ambiguous_success_write_is_revoked(self):
        posted = []

        def fake_api(_opener, _base_url, _token, path, payload=None, with_total=False):
            if path == "repos/lacuna/dione/pulls/1":
                return pr()
            if path == f"repos/lacuna/dione/statuses/{HEAD}":
                posted.append(payload)
                if payload["state"] == "success":
                    raise TimeoutError("success may have been accepted")
                return payload
            self.fail(f"unexpected API path: {path}")

        with (
            patch.object(review_receipt, "api_request", side_effect=fake_api),
            patch.object(review_receipt, "official_reviewers", return_value=({"lain-construct": review()}, set())),
        ):
            with self.assertRaisesRegex(TimeoutError, "may have been accepted"):
                review_receipt.check_pr(None, "base", "token", "lacuna/dione", 1)
        self.assertEqual(["pending", "success", "pending"], [status["state"] for status in posted])

    def test_success_is_revoked_when_post_write_read_fails(self):
        posted = []

        def fake_api(_opener, _base_url, _token, path, payload=None, with_total=False):
            if path == "repos/lacuna/dione/pulls/1":
                return pr()
            if path == f"repos/lacuna/dione/statuses/{HEAD}":
                posted.append(payload)
                return payload
            self.fail(f"unexpected API path: {path}")

        def fake_reviews(*_args):
            if posted[-1]["state"] == "success":
                raise IncompleteRead(b"truncated response", 100)
            return {"lain-construct": review()}, set()

        with (
            patch.object(review_receipt, "api_request", side_effect=fake_api),
            patch.object(review_receipt, "official_reviewers", side_effect=fake_reviews),
        ):
            with self.assertRaises(IncompleteRead):
                review_receipt.check_pr(None, "base", "token", "lacuna/dione", 1)
        self.assertEqual(["pending", "success", "pending"], [status["state"] for status in posted])

    def test_revocation_retries_a_transient_status_write_error(self):
        posted = []
        attempts = 0

        def fake_api(_opener, _base_url, _token, path, payload=None, with_total=False):
            nonlocal attempts
            if path == "repos/lacuna/dione/pulls/1":
                return pr()
            if path == f"repos/lacuna/dione/statuses/{HEAD}":
                if payload["state"] == "pending" and posted:
                    attempts += 1
                    if attempts == 1:
                        raise OSError("transient status write failure")
                posted.append(payload)
                return payload
            self.fail(f"unexpected API path: {path}")

        def fake_reviews(*_args):
            if any(status["state"] == "success" for status in posted):
                return {"lain-construct": review(body=receipt().replace("Decision: approve", "Decision: comment"))}, set()
            return {"lain-construct": review()}, set()

        with (
            patch.object(review_receipt, "api_request", side_effect=fake_api),
            patch.object(review_receipt, "official_reviewers", side_effect=fake_reviews),
        ):
            with self.assertRaisesRegex(ValueError, "changed after status"):
                review_receipt.check_pr(None, "base", "token", "lacuna/dione", 1)
        self.assertEqual(2, attempts)
        self.assertEqual(["pending", "success", "pending"], [status["state"] for status in posted])

    def test_main_push_revokes_receipt_for_changed_base(self):
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
            posted = []

            def fake_api(_opener, _base_url, _token, path, payload=None, with_total=False):
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
                    posted.append(payload)
                    return payload
                self.fail(f"unexpected API path: {path}")

            with patch.dict(os.environ, env), patch.object(review_receipt, "review_token", return_value="test-token"), patch.object(review_receipt, "api_request", side_effect=fake_api):
                self.assertEqual(0, review_receipt.main())
            self.assertEqual(["pending", "failure"], [status["state"] for status in posted])
            self.assertIn("PR base", posted[-1]["description"])

    def test_main_push_continues_after_one_pr_moves(self):
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

    def test_main_push_rejects_missing_open_pulls(self):
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

    def test_review_workflow_keeps_its_trusted_source_and_refresh_events(self):
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
            ["run: python3 .forgejo/scripts/review_receipt.py"],
            [line.strip() for line in workflow.splitlines() if line.lstrip().removeprefix("- ").startswith("run:")],
        )
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
