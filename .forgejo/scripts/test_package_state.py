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


@unittest.skipUnless(shutil.which("bash"), "release preflight requires Bash")
class ReleasePreflightTests(unittest.TestCase):
    def test_package_state_failure_stops_each_preflight(self):
        lines = (SOURCE / ".github/workflows/publish-crate.yml").read_text().splitlines()
        blocks = []
        for index, line in enumerate(lines):
            if line.strip() != "run: |":
                continue
            indent = len(line) - len(line.lstrip()) + 2
            block = []
            for following in lines[index + 1:]:
                if following.strip() and len(following) - len(following.lstrip()) < indent:
                    break
                block.append(following[indent:])
            script = "\n".join(block)
            if "case " in script and "scripts/crates-io-package-state.sh" in script:
                blocks.append(script)
        self.assertEqual(len(blocks), 11)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            stubs = {
                "cargo": "exit 0",
                "scripts/workspace-package-version.sh": "echo 1.0.0",
                "scripts/verify-public-package-privacy.sh": "exit 0",
                "scripts/verify-crates-io-owners.sh": "exit 0",
                "scripts/crates-io-package-state.sh":
                    'if [ "$1" = "$FAIL_PACKAGE" ]; then exit 23; fi; echo missing',
            }
            for name, body in stubs.items():
                path = root / name
                path.write_text("#!/bin/sh\n" + body + "\n")
                path.chmod(0o755)
            for index, block in enumerate(blocks):
                packages = (("dione", "dione-rendering", "dione-durable", "dione-contracts", "dione-rook", "dione-attention", "dione-evidence", "dione-messages", "dione-rate-limiter", "dione-vaelii") if index == 0
                            else ({1: "dione-rendering", 2: "dione-durable", 3: "dione-contracts", 4: "dione-rook", 5: "dione-attention", 6: "dione-evidence", 7: "dione-messages", 8: "dione-rate-limiter", 9: "dione-vaelii", 10: "dione"}[index],))
                for package in packages:
                    with self.subTest(block=index, package=package):
                        script = block.replace("${{ needs.verify-dione.outputs.rendering-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.dione-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.durable-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.contracts-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.rook-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.attention-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.evidence-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.messages-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.rate-limiter-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.vaelii-version }}", "1.0.0")
                        environment = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                                           FAIL_PACKAGE=package, GITHUB_OUTPUT=str(root / "output"))
                        result = subprocess.run(["bash", "-c", script], cwd=root,
                                                env=environment, capture_output=True, text=True)
                        self.assertEqual(result.returncode, 23, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
