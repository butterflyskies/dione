import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[2]


class PackagePreflightTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        (self.root / "scripts").mkdir()
        shutil.copyfile(SOURCE / "scripts/package-preflight.sh", self.root / "scripts/package-preflight.sh")
        for name, kind in (("verify-public-package-privacy.sh", "privacy"),
                           ("verify-crates-io-owners.sh", "owners"),
                           ("crates-io-package-state.sh", "state")):
            tool = self.root / "scripts" / name
            tool.write_text(
                "#!/bin/sh\nexec python3 - \"$@\" <<'PY'\nimport json, os, sys\n"
                "kind = " + repr(kind) + "\n"
                "with open('calls.jsonl', 'a') as f: f.write(json.dumps([kind, *sys.argv[1:]]) + '\\n')\n"
                "if os.environ.get('FAIL_KIND') == kind: sys.exit(23)\n"
                "if kind == 'state': print(os.environ['PACKAGE_STATE'])\nPY\n"
            )
            tool.chmod(0o755)

    def check(self, package="dione", comparison="exact", mode="verify", state="missing", failure=""):
        output = self.root / "output"
        result = subprocess.run(
            ["sh", "scripts/package-preflight.sh", package, "1.2.3", "custom/package.crate",
             comparison, mode, str(output), "publish-test", "owner-one", "team-two"],
            cwd=self.root, env=dict(os.environ, PACKAGE_STATE=state, FAIL_KIND=failure),
            capture_output=True, text=True,
        )
        calls = [json.loads(line) for line in (self.root / "calls.jsonl").read_text().splitlines()]
        return result, calls, output.read_text() if output.exists() else ""

    def test_root_exact_comparison_and_explicit_owners_output(self):
        result, calls, output = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [["privacy", "custom/package.crate"],
                                 ["owners", "dione", "owner-one", "team-two"],
                                 ["state", "dione", "1.2.3", "custom/package.crate"]])
        self.assertEqual(output, "publish-test=true\n")

    def test_member_ignore_vcs_published_upload_skips_upload(self):
        result, calls, output = self.check("dione-member", "--ignore-vcs-info", "upload", "published")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[-1], ["state", "dione-member", "1.2.3", "custom/package.crate", "--ignore-vcs-info"])
        self.assertEqual(output, "publish-test=false\n")

    def test_reconciliation_requires_published_before_owner_check(self):
        result, calls, output = self.check(mode="reconcile", state="published")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[0] for call in calls], ["privacy", "state", "owners"])
        self.assertEqual(output, "")

    def test_missing_reconciliation_rejects_without_output_or_owner_check(self):
        result, calls, output = self.check(mode="reconcile")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([call[0] for call in calls], ["privacy", "state"])
        self.assertEqual(output, "")

    def test_unknown_state_rejects_without_upload_output(self):
        result, _, output = self.check(state="unknown")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output, "")

    def test_privacy_owner_and_state_failures_preserve_exact_exit(self):
        for kind in ("privacy", "owners", "state"):
            with self.subTest(kind=kind):
                for file in ("calls.jsonl", "output"):
                    (self.root / file).unlink(missing_ok=True)
                result, calls, output = self.check(failure=kind)
                self.assertEqual(result.returncode, 23, result.stdout + result.stderr)
                self.assertEqual(calls[-1][0], kind)
                self.assertEqual(output, "")
