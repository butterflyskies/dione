import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[2]


class TeamsFixtureSetupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / 'scripts').mkdir()
        shutil.copyfile(SOURCE / 'scripts/run-tests.py', self.root / 'scripts/run-tests.py')
        self.fixture = self.root / 'current-fixture'
        self.fixture.write_text('current compiled test artifact')
        self.codex_fixture = self.root / 'current-codex-fixture'
        self.codex_fixture.write_text('current Codex compiler artifact')
        self.log = self.root / 'calls.jsonl'
        tools = self.root / 'tools'
        tools.mkdir()
        cargo = tools / 'cargo'
        cargo.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
with open(os.environ['CALL_LOG'], 'a') as out:
    out.write(json.dumps({'args': args, 'fixture': os.environ.get('DIONE_TEAMS_FIXTURE_EXECUTABLE'), 'codex_fixture': os.environ.get('DIONE_CODEX_FIXTURE_EXECUTABLE')}) + '\\n')
if args[:2] == ['nextest', 'list']:
    case = os.environ.get('CASE', 'valid')
    suite = {'package-name': 'dione-teams', 'kind': 'lib', 'binary-path': os.environ['CURRENT_FIXTURE'],
             'testcases': {'fixture_process::native_fixture_service': {'ignored': case != 'not-ignored'}}}
    if case == 'no-service': suite['testcases'] = {}
    suites = {} if case == 'no-owner' else {'owner': suite}
    if case == 'duplicate-owner': suites['duplicate'] = suite
    codex = {'package-name': 'dione-codex', 'kind': 'lib', 'binary-path': os.environ['CURRENT_CODEX_FIXTURE'],
             'testcases': {'boundary_fixture::private_owner_fixture_service': {'ignored': case != 'codex-not-ignored'}}}
    if case == 'codex-no-service': codex['testcases'] = {}
    if case != 'codex-no-owner': suites['codex-owner'] = codex
    if case == 'codex-duplicate-owner': suites['codex-duplicate'] = codex
    print(json.dumps({'rust-suites': suites}))
''')
        cargo.chmod(0o755)
        self.environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'],
                                CALL_LOG=str(self.log), CURRENT_FIXTURE=str(self.fixture),
                                DIONE_TEAMS_FIXTURE_EXECUTABLE='/stale/previous-run',
                                CURRENT_CODEX_FIXTURE=str(self.codex_fixture),
                                DIONE_CODEX_FIXTURE_EXECUTABLE='/stale/codex-previous-run')

    def run_wrapper(self, *args, case='valid'):
        return subprocess.run(['python3', 'scripts/run-tests.py', *args], cwd=self.root,
                              env=dict(self.environment, CASE=case), capture_output=True, text=True)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_refreshes_exact_artifact_and_preserves_build_features_and_test_selection(self):
        result = self.run_wrapper('-p', 'dione', '--lib', '--features', 'oneshot-test-seam',
                                  '--offline', '--no-capture', '-E', 'test(native)')
        self.assertEqual(result.returncode, 0, result.stderr)
        listed, run = self.calls()
        self.assertEqual(listed['args'], ['nextest', 'list', '--workspace', '--locked', '--offline',
                                         '--features', 'oneshot-test-seam', '--message-format', 'json'])
        self.assertEqual(run['args'], ['nextest', 'run', '--locked', '--offline', '--features',
                                      'oneshot-test-seam', '-p', 'dione', '--lib', '-E',
                                      'test(native)', '--no-capture'])
        self.assertEqual(run['fixture'], str(self.fixture))
        self.assertNotEqual(run['fixture'], '/stale/previous-run')
        self.assertEqual(run['codex_fixture'], str(self.codex_fixture))
        self.assertNotEqual(run['codex_fixture'], '/stale/codex-previous-run')

    def test_discovers_again_for_each_invocation(self):
        self.assertEqual(self.run_wrapper('--workspace').returncode, 0)
        newer = self.root / 'newer-fixture'
        newer.write_text('new compiler artifact')
        self.environment['CURRENT_FIXTURE'] = str(newer)
        newer_codex = self.root / 'newer-codex-fixture'
        newer_codex.write_text('new Codex compiler artifact')
        self.environment['CURRENT_CODEX_FIXTURE'] = str(newer_codex)
        self.assertEqual(self.run_wrapper('--workspace').returncode, 0)
        self.assertEqual([call['fixture'] for call in self.calls() if call['args'][1] == 'run'],
                         [str(self.fixture), str(newer)])
        self.assertEqual([call['codex_fixture'] for call in self.calls() if call['args'][1] == 'run'],
                         [str(self.codex_fixture), str(newer_codex)])

    def test_rejects_missing_ambiguous_or_nonprivate_service_before_running(self):
        for case in ('no-owner', 'duplicate-owner', 'no-service', 'not-ignored',
                     'codex-no-owner', 'codex-duplicate-owner', 'codex-no-service', 'codex-not-ignored'):
            with self.subTest(case=case):
                self.log.unlink(missing_ok=True)
                result = self.run_wrapper(case=case)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(self.calls()), 1)
                self.assertEqual(self.calls()[0]['args'][1], 'list')

    def test_rejects_missing_current_binary_instead_of_using_stale_environment(self):
        self.fixture.unlink()
        result = self.run_wrapper()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('current dione-teams compiler fixture artifact is missing', result.stderr)
        self.assertEqual(len(self.calls()), 1)

    def test_rejects_missing_current_codex_binary_instead_of_using_stale_environment(self):
        self.codex_fixture.unlink()
        result = self.run_wrapper()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('current dione-codex compiler fixture artifact is missing', result.stderr)
        self.assertEqual(len(self.calls()), 1)


if __name__ == '__main__':
    unittest.main()
