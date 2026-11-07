import ast
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[2]


def workspace_packages():
    # The checked-in workspace uses literal member paths and package names;
    # read those declarations without requiring Python 3.11's TOML module.
    workspace = (SOURCE / "Cargo.toml").read_text().split("[workspace]", 1)[1].split("[package]", 1)[0]
    members = ast.literal_eval(re.search(r"(?ms)^members\s*=\s*(\[.*?\])", workspace).group(1))
    packages = set()
    for member in members:
        manifest = (SOURCE / member / "Cargo.toml").read_text()
        package = manifest.split("[package]", 1)[1].split("\n[", 1)[0]
        packages.add(ast.literal_eval(re.search(r'(?m)^name\s*=\s*("[^"]+")', package).group(1)))
    return packages


@unittest.skipUnless(shutil.which("sh"), "PR verification requires a POSIX shell")
class PrReadyProfileTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name) / "repo"
        self.root.mkdir()
        self.log = Path(self.directory.name) / "cargo.jsonl"
        self.binary_log = Path(self.directory.name) / "binary.log"
        self.privacy_log = Path(self.directory.name) / "privacy.jsonl"
        (self.root / "scripts").mkdir()
        shutil.copyfile(SOURCE / "scripts/pr-ready.sh", self.root / "scripts/pr-ready.sh")
        shutil.copyfile(SOURCE / "scripts/run-tests.py", self.root / "scripts/run-tests.py")
        shutil.copyfile(SOURCE / "scripts/verify-workspace-package-privacy.py", self.root / "scripts/verify-workspace-package-privacy.py")
        (self.root / ".forgejo/scripts").mkdir(parents=True)
        (self.root / ".forgejo/scripts/test_fixture.py").write_text(
            "from pathlib import Path\nimport unittest\n"
            "class FixtureContext(unittest.TestCase):\n"
            "    def test_uses_isolated_workspace(self):\n"
            "        self.assertTrue(Path('tools/cargo').is_file())\n"
        )
        stubs = {
            ".forgejo/scripts/release-hygiene.sh": "exit 0",
            "scripts/workspace-package-version.sh": "echo 1.0.0",
            "scripts/verify-public-package-privacy.sh": "exit 0",
        }
        for name, body in stubs.items():
            path = self.root / name
            path.write_text("#!/bin/sh\n" + body + "\n")
            path.chmod(0o755)
        privacy = self.root / "scripts/verify-public-package-privacy.sh"
        privacy.write_text(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$TEST_PRIVACY_LOG\"\n"
        )
        privacy.chmod(0o755)
        tools = self.root / "tools"
        tools.mkdir()
        cargo = tools / "cargo"
        cargo.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, shlex, sys\n"
            "args = sys.argv[1:]\n"
            "with open(os.environ['TEST_CARGO_LOG'], 'a') as log:\n"
            "    log.write(json.dumps(args) + '\\n')\n"
            "if args[0] == 'metadata':\n"
            "    packages = json.loads(os.environ['TEST_WORKSPACE_PACKAGES'])\n"
            "    print(json.dumps({'workspace_members': packages, 'target_directory': os.environ['CARGO_TARGET_DIR'], 'packages': [{'id': name, 'name': name, 'version': '1.0.0', 'publish': None} for name in packages]}))\n"
            "if args[:2] == ['nextest', 'list']:\n"
            "    fixture = str(pathlib.Path(__file__).resolve())\n"
            "    print(json.dumps({'rust-suites': {'dione-teams': {'package-name': 'dione-teams', 'kind': 'lib', 'binary-path': fixture, 'testcases': {'fixture_process::native_fixture_service': {'ignored': True}}}, 'dione-codex': {'package-name': 'dione-codex', 'kind': 'lib', 'binary-path': fixture, 'testcases': {'boundary_fixture::private_owner_fixture_service': {'ignored': True}}}}}))\n"
            "if args[:2] == ['nextest', 'run']:\n"
            "    assert os.environ.get('DIONE_TEAMS_FIXTURE_EXECUTABLE') == str(pathlib.Path(__file__).resolve())\n"
            "    assert os.environ.get('DIONE_CODEX_FIXTURE_EXECUTABLE') == str(pathlib.Path(__file__).resolve())\n"
            "if args[0] == 'build':\n"
            "    profile = args[args.index('--profile') + 1]\n"
            "    binary = pathlib.Path(os.environ['CARGO_TARGET_DIR']) / profile / 'dione'\n"
            "    binary.parent.mkdir(parents=True, exist_ok=True)\n"
            "    binary.write_text('#!/bin/sh\\necho ' + shlex.quote(profile) + ' >> ' + shlex.quote(os.environ['TEST_BINARY_LOG']) + '\\n')\n"
            "    binary.chmod(0o755)\n"
        )
        cargo.chmod(0o755)
        (self.root / ".gitignore").write_text("target/\n")
        self.git("init", "--quiet")
        self.git("config", "user.name", "Dione Tests")
        self.git("config", "user.email", "dione-tests@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "fixture")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.environment = dict(
            os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
            TEST_CARGO_LOG=str(self.log), TEST_BINARY_LOG=str(self.binary_log),
            TEST_PRIVACY_LOG=str(self.privacy_log),
            TEST_WORKSPACE_PACKAGES=json.dumps(sorted(workspace_packages())),
            CARGO_TARGET_DIR="target",
        )

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

    def run_gate(self, *args):
        return subprocess.run(["sh", "scripts/pr-ready.sh", *args], cwd=self.root,
                              env=self.environment, capture_output=True, text=True)

    def assert_build(self, result, profile):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        builds = [call for call in calls if call[0] == "build"]
        self.assertEqual(builds, [["build", "--profile", profile, "--workspace", "--bins", "--locked"]])
        self.assertEqual(self.binary_log.read_text().strip(), profile)
        self.assertIn(f"({profile} build)", result.stdout)
        packages = workspace_packages()
        self.assertTrue(self.privacy_log.is_file(), "workspace archives must cross the privacy scanner")
        privacy_calls = [(line,) for line in self.privacy_log.read_text().splitlines()]
        expected = {(f"target/package/{name}-1.0.0.crate",) for name in packages}
        self.assertEqual({tuple(call) for call in privacy_calls}, expected)
        self.assertEqual(len(privacy_calls), len(expected))

    def test_profile_module_imports_without_python311_toml(self):
        code = (
            "import builtins, runpy, sys\n"
            "original_import = builtins.__import__\n"
            "def compatible_import(name, *args, **kwargs):\n"
            "    if name in ('tomllib', 'tomli'):\n"
            "        raise ModuleNotFoundError(name)\n"
            "    return original_import(name, *args, **kwargs)\n"
            "builtins.__import__ = compatible_import\n"
            "runpy.run_path(sys.argv[1], run_name='compatibility_import')\n"
        )
        result = subprocess.run(
            [os.sys.executable, "-c", code, str(Path(__file__).resolve())],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_default_builds_and_launches_review_binary(self):
        self.assert_build(self.run_gate(), "review")

    def test_explicit_release_builds_and_launches_release_binary(self):
        self.assert_build(self.run_gate("HEAD", "release"), "release")

    def test_unknown_profile_rejects_before_cargo_runs(self):
        result = self.run_gate("HEAD", "dev")
        self.assertEqual(result.returncode, 2)
        self.assertIn("choose review or release", result.stderr)
        self.assertFalse(self.log.exists())

    def test_extra_arguments_reject_before_cargo_runs(self):
        result = self.run_gate("HEAD", "review", "extra")
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage:", result.stderr)
        self.assertFalse(self.log.exists())


if __name__ == "__main__":
    unittest.main()
