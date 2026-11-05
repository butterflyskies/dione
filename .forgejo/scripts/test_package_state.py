import hashlib
import io
import json
import os
import re
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
    def test_scheduler_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        self.assertIn("needs: [verify-dione, reconcile-contracts]", jobs["publish-delivery-scheduler"])
        self.assertIn("needs.reconcile-contracts.result == 'success'", jobs["reconcile-delivery-scheduler"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-delivery-scheduler", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-delivery-scheduler.result == 'success'", jobs["reconcile-dione"])

    def test_access_requests_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        self.assertIn("needs: verify-dione", jobs["publish-access-requests"])
        self.assertIn("needs: [verify-dione, publish-access-requests]", jobs["reconcile-access-requests"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-access-requests", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-access-requests.result == 'success'", jobs["reconcile-dione"])

    def test_mutes_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        self.assertIn("needs: verify-dione", jobs["publish-mutes"])
        self.assertIn("needs: [verify-dione, publish-mutes]", jobs["reconcile-mutes"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-mutes", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-mutes.result == 'success'", jobs["reconcile-dione"])

    def test_coordination_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        self.assertIn("needs: verify-dione", jobs["publish-coordination"])
        self.assertIn("needs: [verify-dione, publish-coordination]", jobs["reconcile-coordination"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-coordination", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-coordination.result == 'success'", jobs["reconcile-dione"])

    def test_teams_identities_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        self.assertIn("needs: verify-dione", jobs["publish-teams-identities"])
        self.assertIn("needs: [verify-dione, publish-teams-identities]", jobs["reconcile-teams-identities"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-teams-identities", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-teams-identities.result == 'success'", jobs["reconcile-dione"])

    def test_discord_identities_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        self.assertIn("needs: verify-dione", jobs["publish-discord-identities"])
        self.assertIn("needs: [verify-dione, publish-discord-identities]", jobs["reconcile-discord-identities"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-discord-identities", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-discord-identities.result == 'success'", jobs["reconcile-dione"])

    def test_pluralkit_identities_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        self.assertIn("needs: verify-dione", jobs["publish-pluralkit-identities"])
        self.assertIn("needs: [verify-dione, publish-pluralkit-identities]", jobs["reconcile-pluralkit-identities"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-pluralkit-identities", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-pluralkit-identities.result == 'success'", jobs["reconcile-dione"])

    def test_config_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                self.assertNotIn(current, jobs)
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        capabilities = ('attention', 'bell-rings', 'contradictionary', 'coordination', 'discord-identities', 'gaie', 'identity-display', 'messages', 'pluralkit-identities', 'pre-send', 'rate-limiter', 'teams-identities', 'vaelii')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-config", set()), ("reconcile-config", {"publish-config"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-config"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-config.result == 'success' || needs.publish-config.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-config", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-config.result == 'success'", jobs["reconcile-dione"])

    def test_mcp_tools_leaf_and_root_reconciliation(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = dict(re.findall(r"^  ([\w-]+):\n(.*?)(?=^  [\w-]+:|\Z)", workflow, re.M | re.S))
        self.assertEqual(jobs["publish-mcp-tools"].splitlines()[0].strip(), "needs: verify-dione")
        self.assertEqual(jobs["reconcile-mcp-tools"].splitlines()[0].strip(), "needs: [verify-dione, publish-mcp-tools]")
        self.assertIn("always()", jobs["reconcile-mcp-tools"])
        self.assertIn("needs.verify-dione.result == 'success'", jobs["reconcile-mcp-tools"])
        self.assertIn("(needs.publish-mcp-tools.result == 'success' || needs.publish-mcp-tools.result == 'skipped')", jobs["reconcile-mcp-tools"])
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-mcp-tools", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-mcp-tools.result == 'success'", jobs["reconcile-dione"])

    def test_provider_delivery_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                self.assertNotIn(current, jobs)
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        capabilities = ('messages', 'teams-identities')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-provider-delivery", set()), ("reconcile-provider-delivery", {"publish-provider-delivery"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-provider-delivery"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-provider-delivery.result == 'success' || needs.publish-provider-delivery.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-provider-delivery", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-provider-delivery.result == 'success'", jobs["reconcile-dione"])

    def test_outbound_consent_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                self.assertNotIn(current, jobs)
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        capabilities = ('config', 'evidence', 'contradictionary', 'messages')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-outbound-consent", set()), ("reconcile-outbound-consent", {"publish-outbound-consent"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-outbound-consent"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-outbound-consent.result == 'success' || needs.publish-outbound-consent.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-outbound-consent", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-outbound-consent.result == 'success'", jobs["reconcile-dione"])

    def test_pre_send_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = {}
        current = None
        for line in workflow.splitlines():
            if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                current = line.strip().removesuffix(":")
                jobs[current] = ""
            elif current:
                jobs[current] += line + "\n"
        self.assertIn("needs: [verify-dione, reconcile-evidence]", jobs["publish-pre-send"])
        self.assertIn("needs: [verify-dione, reconcile-evidence, publish-pre-send]", jobs["reconcile-pre-send"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-pre-send", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-pre-send.result == 'success'", jobs["reconcile-dione"])
        self.assertIn("needs.reconcile-evidence.result == 'success'", jobs["reconcile-pre-send"])

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
        self.assertEqual(len(blocks), 29)
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
                packages = (("dione", "dione-rendering", "dione-durable", "dione-contracts", "dione-rook", "dione-attention", "dione-evidence", "dione-messages", "dione-rate-limiter", "dione-vaelii", "dione-gaie", "dione-identity-display", "dione-harness", "dione-presentation", "dione-contradictionary", "dione-bell-rings", "dione-delivery-scheduler", "dione-access-requests", "dione-mutes", "dione-coordination", "dione-pre-send", "dione-teams-identities", "dione-discord-identities", "dione-pluralkit-identities", "dione-config", "dione-outbound-consent", "dione-mcp-tools", "dione-provider-delivery") if index == 0
                            else ({1: "dione-rendering", 2: "dione-durable", 3: "dione-contracts", 4: "dione-rook", 5: "dione-attention", 6: "dione-evidence", 7: "dione-messages", 8: "dione-rate-limiter", 9: "dione-vaelii", 10: "dione-gaie", 11: "dione-identity-display", 12: "dione-harness", 13: "dione-presentation", 14: "dione-contradictionary", 15: "dione-bell-rings", 16: "dione-delivery-scheduler", 17: "dione-access-requests", 18: "dione-mutes", 19: "dione-coordination", 20: "dione-pre-send", 21: "dione-teams-identities", 22: "dione-discord-identities", 23: "dione-pluralkit-identities", 24: "dione-config", 25: "dione-outbound-consent", 26: "dione-mcp-tools", 27: "dione-provider-delivery", 28: "dione"}[index],))
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
                        script = script.replace("${{ needs.verify-dione.outputs.gaie-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.identity-display-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.harness-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.presentation-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.contradictionary-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.bell-rings-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.delivery-scheduler-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.access-requests-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.mutes-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.pre-send-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.outbound-consent-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.config-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.coordination-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.teams-identities-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.discord-identities-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.pluralkit-identities-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.mcp-tools-version }}", "1.0.0")
                        script = script.replace("${{ needs.verify-dione.outputs.provider-delivery-version }}", "1.0.0")
                        environment = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                                           FAIL_PACKAGE=package, GITHUB_OUTPUT=str(root / "output"))
                        result = subprocess.run(["bash", "-c", script], cwd=root,
                                                env=environment, capture_output=True, text=True)
                        self.assertEqual(result.returncode, 23, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
