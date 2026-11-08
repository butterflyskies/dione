import hashlib
import io
import json
import os
import re
from pathlib import Path
import shutil
import shlex
import subprocess
import tarfile
import tempfile
import unittest

from workflow_test_support import read_jobs, run_blocks


SOURCE = Path(__file__).resolve().parents[2]
PACKAGES = ('dione', 'dione-rendering', 'dione-durable', 'dione-contracts', 'dione-rook', 'dione-attention', 'dione-evidence', 'dione-messages', 'dione-rate-limiter', 'dione-vaelii', 'dione-gaie', 'dione-identity-display', 'dione-harness', 'dione-presentation', 'dione-contradictionary', 'dione-bell-rings', 'dione-delivery-scheduler', 'dione-access-requests', 'dione-mutes', 'dione-coordination', 'dione-pre-send', 'dione-teams-identities', 'dione-discord-identities', 'dione-pluralkit-identities', 'dione-config', 'dione-outbound-consent', 'dione-mcp-tools', 'dione-provider-delivery', 'dione-teams', 'dione-omp', 'dione-codex', 'dione-claude', 'dione-discord-presence', 'dione-discord-state', 'dione-discord-events', 'dione-tracing-channel', 'dione-config-watcher', 'dione-drop-ledger', 'dione-permissions', 'dione-discord-bells', 'dione-discord-gate', 'dione-oneshot', 'dione-harness-runtime', 'dione-discord-scheduling', 'dione-discord-typing')


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
    def test_workflow_reader_rejects_duplicate_job_keys(self):
        with self.assertRaisesRegex(ValueError, "duplicate workflow job: publish-one"):
            read_jobs("jobs:\n  publish-one:\n    needs: verify\n  publish-one:\n    needs: other\n")

    def test_workflow_reader_preserves_jobs_when_reordered(self):
        first = "  publish-one:\n    needs: verify\n"
        second = "  publish-two:\n    needs: reconcile-one\n"
        self.assertEqual(read_jobs("jobs:\n" + first + second), read_jobs("jobs:\n" + second + first))

    def test_scheduler_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("needs: [verify-dione, reconcile-contracts]", jobs["publish-delivery-scheduler"])
        self.assertIn("needs.reconcile-contracts.result == 'success'", jobs["reconcile-delivery-scheduler"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-delivery-scheduler", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-delivery-scheduler.result == 'success'", jobs["reconcile-dione"])

    def test_access_requests_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("needs: [verify-dione, reconcile-config, reconcile-messages]", jobs["publish-access-requests"])
        self.assertIn("needs: [verify-dione, reconcile-config, reconcile-messages, publish-access-requests]", jobs["reconcile-access-requests"])
        for dep in ("config", "messages"):
            self.assertIn("needs.reconcile-" + dep + ".result == 'success'", jobs["reconcile-access-requests"])
        self.assertIn("    permissions:\n      contents: read\n      id-token: write\n", jobs["publish-access-requests"])
        self.assertIn("    permissions:\n      contents: read\n", jobs["reconcile-access-requests"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-access-requests", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-access-requests.result == 'success'", jobs["reconcile-dione"])

    def test_mutes_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        expected = {"verify-dione", "reconcile-durable"}
        for name, extra in (("publish-mutes", set()), ("reconcile-mutes", {"publish-mutes"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        self.assertIn("needs.reconcile-durable.result == 'success'", jobs["reconcile-mutes"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-mutes", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-mutes.result == 'success'", jobs["reconcile-dione"])

    def test_coordination_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("needs: verify-dione", jobs["publish-coordination"])
        self.assertIn("needs: [verify-dione, publish-coordination]", jobs["reconcile-coordination"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-coordination", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-coordination.result == 'success'", jobs["reconcile-dione"])

    def test_teams_identities_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("needs: verify-dione", jobs["publish-teams-identities"])
        self.assertIn("needs: [verify-dione, publish-teams-identities]", jobs["reconcile-teams-identities"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-teams-identities", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-teams-identities.result == 'success'", jobs["reconcile-dione"])

    def test_discord_identities_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("needs: verify-dione", jobs["publish-discord-identities"])
        self.assertIn("needs: [verify-dione, publish-discord-identities]", jobs["reconcile-discord-identities"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-discord-identities", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-discord-identities.result == 'success'", jobs["reconcile-dione"])

    def test_pluralkit_identities_reconciliation_gates_root_publication(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("needs: verify-dione", jobs["publish-pluralkit-identities"])
        self.assertIn("needs: [verify-dione, publish-pluralkit-identities]", jobs["reconcile-pluralkit-identities"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-pluralkit-identities", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-pluralkit-identities.result == 'success'", jobs["reconcile-dione"])

    def test_config_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
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

    def test_mcp_tools_requires_evidence_and_root_reconciliation(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        for name, extra in (("publish-mcp-tools", set()), ("reconcile-mcp-tools", {"publish-mcp-tools"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: ").removeprefix("[").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), {"verify-dione", "reconcile-evidence"} | extra)
            self.assertIn("needs.reconcile-evidence.result == 'success'", jobs[name])
        self.assertIn("always()", jobs["reconcile-mcp-tools"])
        self.assertIn("needs.verify-dione.result == 'success'", jobs["reconcile-mcp-tools"])
        self.assertIn("(needs.publish-mcp-tools.result == 'success' || needs.publish-mcp-tools.result == 'skipped')", jobs["reconcile-mcp-tools"])
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-mcp-tools", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-mcp-tools.result == 'success'", jobs["reconcile-dione"])

    def test_discord_presence_leaf_and_root_reconciliation(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertEqual(jobs["publish-discord-presence"].splitlines()[0].strip(), "needs: verify-dione")
        self.assertEqual(jobs["reconcile-discord-presence"].splitlines()[0].strip(), "needs: [verify-dione, publish-discord-presence]")
        self.assertIn("always()", jobs["reconcile-discord-presence"])
        self.assertIn("needs.verify-dione.result == 'success'", jobs["reconcile-discord-presence"])
        self.assertIn("(needs.publish-discord-presence.result == 'success' || needs.publish-discord-presence.result == 'skipped')", jobs["reconcile-discord-presence"])
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-discord-presence", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-discord-presence.result == 'success'", jobs["reconcile-dione"])

    def test_discord_state_leaf_and_root_reconciliation(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertEqual(jobs["publish-discord-state"].splitlines()[0].strip(), "needs: verify-dione")
        self.assertEqual(jobs["reconcile-discord-state"].splitlines()[0].strip(), "needs: [verify-dione, publish-discord-state]")
        self.assertIn("always()", jobs["reconcile-discord-state"])
        self.assertIn("needs.verify-dione.result == 'success'", jobs["reconcile-discord-state"])
        self.assertIn("(needs.publish-discord-state.result == 'success' || needs.publish-discord-state.result == 'skipped')", jobs["reconcile-discord-state"])
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-discord-state", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-discord-state.result == 'success'", jobs["reconcile-dione"])

    def test_discord_events_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        capabilities = ('attention', 'bell-rings', 'messages', 'coordination', 'evidence', 'contracts')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-discord-events", set()), ("reconcile-discord-events", {"publish-discord-events"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-discord-events"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-discord-events.result == 'success' || needs.publish-discord-events.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-discord-events", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-discord-events.result == 'success'", jobs["reconcile-dione"])

    def test_tracing_channel_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        capabilities = ('discord-events',)
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-tracing-channel", set()), ("reconcile-tracing-channel", {"publish-tracing-channel"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-tracing-channel"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-tracing-channel.result == 'success' || needs.publish-tracing-channel.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-tracing-channel", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-tracing-channel.result == 'success'", jobs["reconcile-dione"])

    def test_config_watcher_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        capabilities = ('config', 'discord-events')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-config-watcher", set()), ("reconcile-config-watcher", {"publish-config-watcher"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-config-watcher"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-config-watcher.result == 'success' || needs.publish-config-watcher.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-config-watcher", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-config-watcher.result == 'success'", jobs["reconcile-dione"])

    def test_permissions_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        capabilities = ('config', 'discord-state', 'discord-events')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-permissions", set()), ("reconcile-permissions", {"publish-permissions"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-permissions"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-permissions.result == 'success' || needs.publish-permissions.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-permissions", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-permissions.result == 'success'", jobs["reconcile-dione"])

    def test_discord_bells_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("    permissions:\n      contents: read\n      id-token: write\n", jobs["publish-discord-bells"])
        self.assertIn("    permissions:\n      contents: read\n", jobs["reconcile-discord-bells"])
        for name in ("publish-discord-bells", "reconcile-discord-bells"):
            self.assertNotIn("    discord-bells:", jobs[name])
        capabilities = ('bell-rings', 'config', 'discord-events')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-discord-bells", set()), ("reconcile-discord-bells", {"publish-discord-bells"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-discord-bells"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-discord-bells.result == 'success' || needs.publish-discord-bells.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-discord-bells", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-discord-bells.result == 'success'", jobs["reconcile-dione"])

    def test_discord_gate_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("    permissions:\n      contents: read\n      id-token: write\n", jobs["publish-discord-gate"])
        self.assertIn("    permissions:\n      contents: read\n", jobs["reconcile-discord-gate"])
        for name in ("publish-discord-gate", "reconcile-discord-gate"):
            self.assertNotIn("    discord-gate:", jobs[name])
        capabilities = ('mutes', 'config', 'discord-events')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-discord-gate", set()), ("reconcile-discord-gate", {"publish-discord-gate"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-discord-gate"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-discord-gate.result == 'success' || needs.publish-discord-gate.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-discord-gate", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-discord-gate.result == 'success'", jobs["reconcile-dione"])

    def test_oneshot_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("    permissions:\n      contents: read\n      id-token: write\n", jobs["publish-oneshot"])
        self.assertIn("    permissions:\n      contents: read\n", jobs["reconcile-oneshot"])
        for name in ("publish-oneshot", "reconcile-oneshot"):
            self.assertNotIn("    oneshot:", jobs[name])
        capabilities = ('config', 'discord-gate', 'outbound-consent', 'pre-send')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-oneshot", set()), ("reconcile-oneshot", {"publish-oneshot"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-oneshot"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-oneshot.result == 'success' || needs.publish-oneshot.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-oneshot", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-oneshot.result == 'success'", jobs["reconcile-dione"])

    def test_harness_runtime_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("    permissions:\n      contents: read\n      id-token: write\n", jobs["publish-harness-runtime"])
        self.assertIn("    permissions:\n      contents: read\n", jobs["reconcile-harness-runtime"])
        for name in ("publish-harness-runtime", "reconcile-harness-runtime"):
            self.assertNotIn("    harness-runtime:", jobs[name])
        capabilities = ('harness', 'config', 'attention', 'codex', 'omp', 'claude')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-harness-runtime", set()), ("reconcile-harness-runtime", {"publish-harness-runtime"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-harness-runtime"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-harness-runtime.result == 'success' || needs.publish-harness-runtime.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-harness-runtime", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-harness-runtime.result == 'success'", jobs["reconcile-dione"])

    def test_discord_scheduling_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("    permissions:\n      contents: read\n      id-token: write\n", jobs["publish-discord-scheduling"])
        self.assertIn("    permissions:\n      contents: read\n", jobs["reconcile-discord-scheduling"])
        for name in ("publish-discord-scheduling", "reconcile-discord-scheduling"):
            self.assertNotIn("    discord-scheduling:", jobs[name])
        capabilities = ('contracts', 'delivery-scheduler', 'discord-events')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-discord-scheduling", set()), ("reconcile-discord-scheduling", {"publish-discord-scheduling"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-discord-scheduling"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-discord-scheduling.result == 'success' || needs.publish-discord-scheduling.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-discord-scheduling", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-discord-scheduling.result == 'success'", jobs["reconcile-dione"])

    def test_discord_typing_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        self.assertIn("    permissions:\n      contents: read\n      id-token: write\n", jobs["publish-discord-typing"])
        self.assertIn("    permissions:\n      contents: read\n", jobs["reconcile-discord-typing"])
        for name in ("publish-discord-typing", "reconcile-discord-typing"):
            self.assertNotIn("    discord-typing:", jobs[name])
        capabilities = ('config', 'discord-gate', 'discord-state')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-discord-typing", set()), ("reconcile-discord-typing", {"publish-discord-typing"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-discord-typing"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-discord-typing.result == 'success' || needs.publish-discord-typing.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-discord-typing", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-discord-typing.result == 'success'", jobs["reconcile-dione"])

    def test_drop_ledger_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        capabilities = ()
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-drop-ledger", set()), ("reconcile-drop-ledger", {"publish-drop-ledger"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: ").removeprefix("[").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-drop-ledger"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-drop-ledger.result == 'success' || needs.publish-drop-ledger.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-drop-ledger", jobs[name].splitlines()[0])
        self.assertIn("needs.reconcile-drop-ledger.result == 'success'", jobs["reconcile-dione"])

    def test_provider_delivery_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
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

    def test_teams_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        capabilities = ('messages', 'teams-identities', 'provider-delivery', 'mcp-tools')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-teams", set()), ("reconcile-teams", {"publish-teams"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-teams"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-teams.result == 'success' || needs.publish-teams.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertIn("reconcile-teams", set(needs.split(", ")))
        self.assertIn("needs.reconcile-teams.result == 'success'", jobs["reconcile-dione"])

    def test_omp_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        capabilities = ('durable', 'harness', 'messages', 'provider-delivery', 'teams-identities', 'contracts', 'presentation')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-omp", set()), ("reconcile-omp", {"publish-omp"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-omp"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-omp.result == 'success' || needs.publish-omp.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertIn("reconcile-omp", set(needs.split(", ")))
        self.assertIn("needs.reconcile-omp.result == 'success'", jobs["reconcile-dione"])

    def test_claude_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        capabilities = ('contracts', 'harness', 'messages', 'presentation')
        expected = {"verify-dione"} | {"reconcile-" + name for name in capabilities}
        for name, extra in (("publish-claude", set()), ("reconcile-claude", {"publish-claude"})):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertEqual(set(needs.split(", ")), expected | extra)
        reconcile = jobs["reconcile-claude"]
        self.assertIn("always()", reconcile)
        self.assertIn("needs.verify-dione.result == 'success'", reconcile)
        for name in capabilities:
            self.assertIn("needs.reconcile-" + name + ".result == 'success'", reconcile)
        self.assertIn("(needs.publish-claude.result == 'success' || needs.publish-claude.result == 'skipped')", reconcile)
        for name in ("publish-crate", "reconcile-dione"):
            needs = jobs[name].splitlines()[0].strip().removeprefix("needs: [").removesuffix("]")
            self.assertIn("reconcile-claude", set(needs.split(", ")))
        self.assertIn("needs.reconcile-claude.result == 'success'", jobs["reconcile-dione"])

    def test_outbound_consent_reconciliation_requires_every_direct_capability(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        capabilities = ('durable', 'config', 'evidence', 'contradictionary', 'messages')
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
        jobs = read_jobs(workflow)
        self.assertIn("needs: [verify-dione, reconcile-evidence]", jobs["publish-pre-send"])
        self.assertIn("needs: [verify-dione, reconcile-evidence, publish-pre-send]", jobs["reconcile-pre-send"])
        for job in ("publish-crate", "reconcile-dione"):
            self.assertIn("reconcile-pre-send", jobs[job].splitlines()[0])
        self.assertIn("needs.reconcile-pre-send.result == 'success'", jobs["reconcile-dione"])
        self.assertIn("needs.reconcile-evidence.result == 'success'", jobs["reconcile-pre-send"])

    def assert_preflight_failures(self, workflow, success_state=None):
        jobs = read_jobs(workflow)
        packages = PACKAGES
        expected = {"verify-dione": packages, "publish-crate": ("dione",)}
        expected.update({"publish-" + package.removeprefix("dione-"): (package,)
                         for package in packages if package != "dione"})
        selected = {}
        for name, job in jobs.items():
            scripts = [block for block in run_blocks(job)
                       if ("scripts/package-preflight.sh" in block and " reconcile - - " not in block)
                       or ("case " in block and "scripts/crates-io-package-state.sh" in block)]
            if scripts:
                self.assertEqual(len(scripts), 1, name)
                selected[name] = scripts[0]
        self.assertEqual(set(selected), set(expected))
        versions = {package.removeprefix("dione-") + "-version" for package in packages}
        versions.add("dione-version")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            stubs = {
                "cargo": "exit 0",
                "scripts/workspace-package-version.sh": "echo 1.0.0",
                "scripts/verify-public-package-privacy.sh": "exit 0",
                "scripts/verify-crates-io-owners.sh": "exit 0",
                "scripts/crates-io-package-state.sh":
                    'if [ "$1" = "$FAIL_PACKAGE" ]; then exit 23; fi; echo "${TEST_PACKAGE_STATE:-missing}"',
            }
            for name, contents in stubs.items():
                path = root / name
                path.write_text("#!/bin/sh\n" + contents + "\n")
                path.chmod(0o755)
            helper = SOURCE / "scripts/package-preflight.sh"
            if helper.exists():
                shutil.copyfile(helper, root / "scripts/package-preflight.sh")
            for job, packages in expected.items():
                def version(match):
                    self.assertIn(match.group(1), versions)
                    return "1.0.0"
                script = re.sub(r"\$\{\{ needs\.verify-dione\.outputs\.([\w-]+) \}\}", version, selected[job])
                self.assertNotIn("${{", script)
                targets = packages if success_state is None else ("",)
                for package in targets:
                    with self.subTest(job=job, package=package, state=success_state):
                        output = root / "output"
                        output.unlink(missing_ok=True)
                        environment = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                                           FAIL_PACKAGE=package, TEST_PACKAGE_STATE=success_state or "missing",
                                           GITHUB_OUTPUT=str(output))
                        result = subprocess.run(["bash", "-c", script], cwd=root,
                                                env=environment, capture_output=True, text=True)
                        if success_state is None:
                            self.assertEqual(result.returncode, 23, result.stdout + result.stderr)
                        else:
                            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                            flag = "true" if success_state == "missing" else "false"
                            if job == "verify-dione":
                                expected_output = []
                                for item in PACKAGES:
                                    key = item.removeprefix("dione-")
                                    expected_output += ["publish-" + key + "=" + flag, key + "-version=1.0.0"]
                            else:
                                expected_output = ["upload=" + flag]
                            self.assertEqual(sorted(output.read_text().splitlines()), sorted(expected_output))

    def test_every_preflight_caller_preserves_comparison_and_owner_policy(self):
        jobs = read_jobs((SOURCE / ".github/workflows/publish-crate.yml").read_text())
        expected = {"verify-dione": ("verify", PACKAGES)}
        for package in PACKAGES:
            key = package.removeprefix("dione-")
            expected["publish-crate" if package == "dione" else "publish-" + key] = ("upload", (package,))
            expected["reconcile-" + key] = ("reconcile", (package,))
        for job, (mode, packages) in expected.items():
            calls = [shlex.split(line.strip()) for block in run_blocks(jobs[job])
                     for line in block.splitlines() if line.strip().startswith("sh scripts/package-preflight.sh ")]
            self.assertEqual(len(calls), len(packages), job)
            by_package = {call[2]: call for call in calls}
            self.assertEqual(set(by_package), set(packages), job)
            for package in packages:
                key = package.removeprefix("dione-")
                variable = key.replace("-", "_")
                comparison = "exact" if package == "dione" else "--ignore-vcs-info"
                output = "-" if mode == "reconcile" else "${GITHUB_OUTPUT}"
                output_key = "-" if mode == "reconcile" else ("upload" if mode == "upload" else "publish-" + key)
                self.assertEqual(by_package[package], [
                    "sh", "scripts/package-preflight.sh", package,
                    "${" + variable + "_version}", "${" + variable + "_crate}", comparison,
                    mode, output, output_key, "butterflysky",
                    "github:butterflyskies:lacuna-blinkers", "github:butterflyskies:superadmins",
                ], job)

    def test_each_named_preflight_emits_exact_missing_and_published_outputs(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        for state in ("missing", "published"):
            self.assert_preflight_failures(workflow, success_state=state)

    def test_package_state_failure_stops_each_preflight(self):
        self.assert_preflight_failures((SOURCE / ".github/workflows/publish-crate.yml").read_text())

    def test_preflight_fault_targets_survive_independent_job_reordering(self):
        workflow = (SOURCE / ".github/workflows/publish-crate.yml").read_text()
        jobs = read_jobs(workflow)
        header = workflow[:workflow.index("jobs:\n") + len("jobs:\n")]
        reordered = header + "".join("  " + name + ":\n" + body for name, body in reversed(list(jobs.items())))
        self.assert_preflight_failures(reordered)


if __name__ == "__main__":
    unittest.main()
