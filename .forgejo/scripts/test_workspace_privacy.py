import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

from workflow_test_support import read_jobs, run_blocks

SOURCE = Path(__file__).resolve().parents[2]


class WorkspacePrivacyTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.target = self.root / "custom target"
        (self.target / "package").mkdir(parents=True)
        (self.root / "scripts").mkdir()
        for name in ("verify-workspace-package-privacy.py", "verify-public-package-privacy.sh",
                     "public-package-structural-patterns.txt", "public-package-member-patterns.txt"):
            shutil.copyfile(SOURCE / "scripts" / name, self.root / "scripts" / name)
        self.metadata = {
            "target_directory": str(self.target),
            "workspace_members": ["root-id", "renamed-id", "private-id"],
            "packages": [
                {"id": "root-id", "name": "root-package", "version": "2.0.0", "publish": None},
                {"id": "renamed-id", "name": "public-renamed", "version": "3.1.0", "publish": ["crates-io"]},
                {"id": "private-id", "name": "private-member", "version": "1.0.0", "publish": []},
                {"id": "dependency-id", "name": "external-dependency", "version": "1.0.0", "publish": None},
            ],
        }
        cargo = self.root / "cargo"
        cargo.write_text(
            "#!/usr/bin/env python3\nimport json, os, sys\n"
            "if sys.argv[1:] == ['package', '--workspace', '--locked']: sys.exit(0)\n"
            "assert sys.argv[1:] == ['metadata', '--no-deps', '--format-version', '1', '--locked']\n"
            "print(os.environ['TEST_METADATA'])\n"
        )
        cargo.chmod(0o755)
        self.archive("root-package-2.0.0", "pub fn public() {}")
        self.archive("public-renamed-3.1.0", "pub fn public() {}")

    def archive(self, name, contents):
        with tarfile.open(self.target / "package" / (name + ".crate"), "w:gz") as archive:
            data = contents.encode()
            info = tarfile.TarInfo(name + "/src/lib.rs")
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))

    def check(self):
        return subprocess.run(
            [sys.executable, "scripts/verify-workspace-package-privacy.py"], cwd=self.root,
            env=dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ["PATH"],
                     TEST_METADATA=json.dumps(self.metadata)), capture_output=True, text=True,
        )

    def test_publishable_members_use_metadata_names_versions_and_custom_target(self):
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_missing_publishable_archive_fails_at_real_scanner(self):
        (self.target / "package/public-renamed-3.1.0.crate").unlink()
        result = self.check()
        self.assertEqual(result.returncode, 2)
        self.assertIn("crate archive does not exist", result.stderr)

    def test_member_archive_contents_cross_real_privacy_scanner(self):
        canary = "use " + "cingu" + "late::private;"
        self.archive("public-renamed-3.1.0", canary)
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(canary, result.stdout + result.stderr)

    def test_linux_workspace_package_step_scans_every_member(self):
        workflow = (SOURCE / ".forgejo/workflows/linux.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertEqual(jobs["package"].count("    timeout-minutes: 120\n"), 1)
        public_test = "        run: cargo test --test public_package --locked\n"
        self.assertEqual(jobs["package"].count(public_test), 1)
        self.assertEqual(run_blocks(jobs["package"]), [
            "cargo package --workspace --locked\n"
            "python3 scripts/verify-workspace-package-privacy.py\n",
        ])
        self.assertLess(jobs["package"].index(public_test),
                        jobs["package"].index("          cargo package --workspace --locked\n"))
        self.assertIn("    needs: [hygiene, format, lint, test, package, msrv, build, audit]\n",
                      jobs["required"])
        self.assertIn('"package=${{ needs.package.result }}"', jobs["required"])
        blocks = [block for block in run_blocks(jobs["package"])
                  if "cargo package --workspace --locked" in block]
        self.assertEqual(len(blocks), 1)
        (self.target / "package/public-renamed-3.1.0.crate").unlink()
        result = subprocess.run(
            ["sh", "-c", blocks[0]], cwd=self.root,
            env=dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ["PATH"],
                     TEST_METADATA=json.dumps(self.metadata)), capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("crate archive does not exist", result.stderr)

    def test_scanner_failure_exit_is_preserved(self):
        (self.root / "scripts/verify-public-package-privacy.sh").write_text("exit 23\n")
        self.assertEqual(self.check().returncode, 23)
