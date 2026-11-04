import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[2]


@unittest.skipUnless(shutil.which("sh"), "package state requires a POSIX shell")
class PackageStateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.local = self.root / "local.crate"
        self.remote = self.root / "remote.crate"
        self.archive(self.local, "first-commit", "pub fn render() {}")
        self.archive(self.remote, "later-commit", "pub fn render() {}")
        curl = self.root / "curl"
        curl.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, shutil, sys\n"
            "args = sys.argv[1:]\n"
            "output = pathlib.Path(args[args.index('--output') + 1])\n"
            "if args[-1].endswith('/download'):\n"
            "    shutil.copyfile(os.environ['TEST_REMOTE_CRATE'], output)\n"
            "else:\n"
            "    output.write_text(json.dumps({'version': {'checksum': os.environ['TEST_CHECKSUM']}}))\n"
            "    print('200', end='')\n"
        )
        curl.chmod(0o755)

    def archive(self, destination, revision, source):
        with tarfile.open(destination, "w:gz") as archive:
            for name, contents in {
                ".cargo_vcs_info.json": json.dumps({"git": {"sha1": revision}}),
                "Cargo.toml": '[package]\nname="example"\nversion="1.0.0"\n',
                "src/lib.rs": source,
            }.items():
                data = contents.encode()
                entry = tarfile.TarInfo("example-1.0.0/" + name)
                entry.size = len(data)
                archive.addfile(entry, io.BytesIO(data))

    def check(self, *comparison):
        environment = dict(
            os.environ,
            PATH=str(self.root) + os.pathsep + os.environ["PATH"],
            CARGO_TARGET_DIR=str(self.root / "target"),
            TEST_REMOTE_CRATE=str(self.remote),
            TEST_CHECKSUM=hashlib.sha256(self.remote.read_bytes()).hexdigest(),
        )
        return subprocess.run(
            ["sh", "scripts/crates-io-package-state.sh", "example", "1.0.0",
             str(self.local), *comparison],
            cwd=SOURCE, env=environment, capture_output=True, text=True,
        )

    def test_exact_mode_rejects_changed_commit_receipt(self):
        result = self.check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("does not match local package bytes", result.stderr)

    def test_allowed_mode_accepts_only_changed_commit_receipt(self):
        result = self.check("--ignore-vcs-info")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.strip(), "published")

    def test_allowed_mode_still_rejects_changed_source(self):
        self.archive(self.remote, "later-commit", "pub fn changed_render() {}")
        result = self.check("--ignore-vcs-info")
        self.assertEqual(result.returncode, 1)
        self.assertIn("does not match local package contents", result.stderr)


if __name__ == "__main__":
    unittest.main()
