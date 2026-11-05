#!/usr/bin/env python3
"""Run Nextest with the current private Teams fixture compiler artifact."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', action='store_true')
    parser.add_argument('--locked', action='store_true')
    parser.add_argument('--offline', action='store_true')
    parser.add_argument('--features', action='append', default=[])
    parser.add_argument('-p', '--package', action='append', default=[])
    parser.add_argument('--lib', action='store_true')
    parser.add_argument('--test', action='append', default=[])
    parser.add_argument('-E', '--filter-expr', action='append', default=[])
    parser.add_argument('--no-fail-fast', action='store_true')
    parser.add_argument('--no-capture', action='store_true')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    common = ['--locked']
    if args.offline:
        common.append('--offline')
    for feature in args.features:
        common += ['--features', feature]
    listed = subprocess.run(['cargo', 'nextest', 'list', '--workspace', *common,
                             '--message-format', 'json'], cwd=root,
                            stdout=subprocess.PIPE, text=True, check=True)
    metadata = json.loads(listed.stdout)
    suites = [suite for suite in metadata['rust-suites'].values()
              if suite['package-name'] == 'dione-teams' and suite['kind'] == 'lib']
    if len(suites) != 1:
        raise RuntimeError('expected exactly one current Teams unit-test compiler artifact')
    fixture = suites[0]
    service = fixture['testcases'].get('fixture_process::native_fixture_service')
    if not service or not service['ignored']:
        raise RuntimeError('private native fixture service is absent or not explicitly ignored')
    executable = Path(fixture['binary-path'])
    if not executable.is_file():
        raise RuntimeError('current compiler fixture artifact is missing')
    environment = dict(os.environ, DIONE_TEAMS_FIXTURE_EXECUTABLE=str(executable))
    run = ['cargo', 'nextest', 'run', *common]
    if args.workspace or not args.package:
        run.append('--workspace')
    for package in args.package:
        run += ['-p', package]
    if args.lib:
        run.append('--lib')
    for test in args.test:
        run += ['--test', test]
    for expression in args.filter_expr:
        run += ['-E', expression]
    if args.no_fail_fast:
        run.append('--no-fail-fast')
    if args.no_capture:
        run.append('--no-capture')
    return subprocess.run(run, cwd=root, env=environment).returncode


if __name__ == '__main__':
    sys.exit(main())
