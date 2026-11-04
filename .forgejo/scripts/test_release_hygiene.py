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
        member = self.root / "crates/renderer"
        (member / "src").mkdir(parents=True)
        (member / "Cargo.toml").write_text('[package]\nname = "renderer"\nversion = "0.1.0"\n')
        (member / "src/lib.rs").write_text("// baseline\n")
        self.commit()
        self.base = self.git("rev-parse", "HEAD").stdout.strip()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True, text=True)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "fixture")

    def check(self, path, bump=False, member_bump=False):
        if path is not None:
            source = self.root / path
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text("fixture\n")
        if bump:
            (self.root / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "1.1.0"\n')
            (self.root / "CHANGELOG.md").write_text("## [1.1.0]\n")
        if member_bump:
            manifest = self.root / "crates/renderer/Cargo.toml"
            manifest.write_text(manifest.read_text().replace('"0.1.0"', '"0.2.0"'))
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
        result = self.check("crates/renderer/src/lib.rs", bump=True, member_bump=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_application_source_still_requires_release_version(self):
        result = self.check("src/lib.rs")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not greater than current base version", result.stdout)

    def test_root_bump_does_not_cover_member_source(self):
        result = self.check("crates/renderer/src/lib.rs", bump=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("crates/renderer changed", result.stdout)

    def test_member_readme_requires_member_bump(self):
        result = self.check("crates/renderer/README.md")
        self.assertNotEqual(result.returncode, 0)

    def test_member_manifest_requires_member_bump(self):
        manifest = self.root / "crates/renderer/Cargo.toml"
        manifest.write_text(manifest.read_text() + 'description = "updated"\n')
        result = self.check(None)
        self.assertNotEqual(result.returncode, 0)

    def test_member_documentation_bump_does_not_require_root_bump(self):
        result = self.check("crates/renderer/README.md", member_bump=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_root_documentation_does_not_require_release(self):
        result = self.check("README.md")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_new_member_accepts_initial_version(self):
        member = self.root / "crates/new"
        member.mkdir()
        (member / "Cargo.toml").write_text('[package]\nname = "new"\nversion = "0.1.0"\n')
        result = self.check("crates/new/src/lib.rs", bump=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_new_member_rejects_invalid_version(self):
        member = self.root / "crates/new"
        member.mkdir()
        (member / "Cargo.toml").write_text('[package]\nname = "new"\nversion = "invalid"\n')
        result = self.check("crates/new/src/lib.rs", bump=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("valid initial SemVer", result.stdout)

    def test_removed_member_requires_only_application_release(self):
        shutil.rmtree(self.root / "crates/renderer")
        result = self.check("README.md", bump=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
