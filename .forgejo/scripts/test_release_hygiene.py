import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[2]


@unittest.skipUnless(shutil.which("sh"), "release hygiene requires a POSIX shell")
class ReleaseHygieneTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.git("init", "--quiet")
        self.git("config", "user.name", "Dione Tests")
        self.git("config", "user.email", "dione-tests@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        (self.root / "scripts").mkdir()
        for name in ("release-hygiene.sh", "semver-is-greater.sh"):
            source = SOURCE / (".forgejo/scripts" if name == "release-hygiene.sh" else "scripts") / name
            shutil.copyfile(source, self.root / "scripts" / name)
        (self.root / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "1.0.0"\n')
        (self.root / "CHANGELOG.md").write_text("## [1.0.0]\n")
        self.commit()
        self.base = self.git("rev-parse", "HEAD").stdout.strip()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True, text=True)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "fixture")

    def check(self, path, bump=False):
        source = self.root / path
        source.parent.mkdir(parents=True, exist_ok=True)
        source.write_text("fixture\n")
        if bump:
            (self.root / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "1.1.0"\n')
            (self.root / "CHANGELOG.md").write_text("## [1.1.0]\n")
        self.commit()
        environment = dict(os.environ, EVENT_NAME="pull_request", PR_BASE=self.base,
                           PR_HEAD=self.git("rev-parse", "HEAD").stdout.strip())
        return subprocess.run(["sh", "scripts/release-hygiene.sh"], cwd=self.root,
                              env=environment, capture_output=True, text=True)

    def test_member_source_requires_release_version(self):
        result = self.check("crates/renderer/src/lib.rs")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not greater than current base version", result.stdout)

    def test_member_source_accepts_matching_release(self):
        result = self.check("crates/renderer/src/lib.rs", bump=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_application_source_still_requires_release_version(self):
        result = self.check("src/lib.rs")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not greater than current base version", result.stdout)

    def test_documentation_does_not_require_release(self):
        result = self.check("crates/renderer/README.md")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
