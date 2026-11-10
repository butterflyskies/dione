#!/usr/bin/env python3
"""Verify packaged local prerequisites against release dependency and success paths."""
import argparse
import json
from pathlib import Path
import tomllib

import yaml

ROOT = Path(__file__).resolve().parent.parent


def local_dependencies(document, workspace):
    """Resolve normal, dev and build dependencies, including target tables and aliases."""
    result = {}
    for table in [document, *document.get('target', {}).values()]:
        for section in ['dependencies', 'dev-dependencies', 'build-dependencies']:
            for alias, declaration in table.get(section, {}).items():
                if not isinstance(declaration, dict):
                    continue
                dep = workspace[alias] if declaration.get('workspace') else declaration
                if 'path' in dep:
                    result.setdefault(dep.get('package', alias), set()).add(section)
    return result


def needs(job):
    dependencies = job.get('needs', [])
    return [dependencies] if isinstance(dependencies, str) else dependencies


def guarded_path(jobs, start, target, active=frozenset()):
    """Return a success-gated path, respecting implicit success unless always() is used."""
    assert start not in active, ('release cycle', start)
    for dependency in needs(jobs[start]):
        condition = jobs[start].get('if', '')
        if 'always()' in condition and f"needs.{dependency}.result == 'success'" not in condition:
            continue
        if dependency == target:
            return [start, target]
        path = guarded_path(jobs, dependency, target, active | {start})
        if path:
            return [start, *path]
    return None


def verify(jobs, package, prerequisites):
    """Require the repaired direct event edge and all packaged success waits."""
    short = package.removeprefix('dione-')
    paths = {}
    for job in ['publish-' + short, 'reconcile-' + short]:
        assert 'reconcile-discord-events' in needs(jobs[job]), (job, 'missing direct discord-events edge')
        assert "needs.reconcile-discord-events.result == 'success'" in jobs[job].get('if', ''), (job, 'missing discord-events success guard')
        for dependency in prerequisites:
            target = 'reconcile-' + dependency.removeprefix('dione-')
            path = guarded_path(jobs, job, target)
            assert path, (job, 'missing success-gated packaged prerequisite', dependency)
            paths[job + '/' + dependency] = path
    for job in ['publish-crate', 'reconcile-dione']:
        assert guarded_path(jobs, job, 'reconcile-' + short), (job, 'missing root wait')
        assert guarded_path(jobs, job, 'reconcile-discord-events'), (job, 'missing transitive event wait')
    return paths


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output')
    args = parser.parse_args()
    workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['dependencies']
    document = tomllib.loads((ROOT / 'crates/dione-harness-runtime/Cargo.toml').read_text())
    dependencies = local_dependencies(document, workspace)
    jobs = yaml.safe_load((ROOT / '.github/workflows/publish-crate.yml').read_text())['jobs']
    paths = verify(jobs, 'dione-harness-runtime', dependencies)
    result = {'package': 'dione-harness-runtime', 'packaged_dependencies': {name: sorted(sections) for name, sections in sorted(dependencies.items())}, 'success_paths': paths, 'limits': 'Source graph only, not registry availability or Cargo package verification.'}
    if args.output:
        Path(args.output).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
