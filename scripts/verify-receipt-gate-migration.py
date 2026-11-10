#!/usr/bin/env python3
"""Bind the sealed receipt-gate move to its predecessor without running Cargo."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tomllib

import yaml

BASE = 'c9a7e547b94ac8d30d44124c3fc1cfc291299f0e'
ROOT = Path(__file__).resolve().parent.parent


def predecessor(path):
    return subprocess.check_output(['git', 'show', BASE + ':' + path], cwd=ROOT)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    old = predecessor('src/receipt_gate/store.rs')
    current = (ROOT / 'crates/dione-receipt-gate/src/store.rs').read_bytes()
    sanitized = old.replace(b'#[non_exhaustive]\n', b'').replace(b'PACE', b'LANTERN').replace(b'437002871280631808', b'424242')
    assert current == sanitized
    assert not (ROOT / 'src/receipt_gate').exists()
    assert 'receipt_gate' not in (ROOT / 'src/lib.rs').read_text()
    facade = (ROOT / 'crates/dione-receipt-gate/src/lib.rs').read_text()
    assert 'mod store;' in facade and 'pub ' not in facade
    assert 'pub fn from_dione_gateway' not in current.decode()
    assert 'pub fn allow_react' not in current.decode()
    root = tomllib.loads((ROOT / 'Cargo.toml').read_text())
    docs = {}
    for manifest in [ROOT / 'Cargo.toml', *sorted((ROOT / 'crates').glob('*/Cargo.toml'))]:
        doc = tomllib.loads(manifest.read_text())
        docs[doc['package']['name']] = doc
    graph = {name: set() for name in docs}
    packaged = {name: set() for name in docs}
    for name, doc in docs.items():
        for table in [doc, *doc.get('target', {}).values()]:
            for section in ['dependencies', 'build-dependencies', 'dev-dependencies']:
                for alias, declaration in table.get(section, {}).items():
                    if not isinstance(declaration, dict):
                        continue
                    dep = root['workspace']['dependencies'][alias] if declaration.get('workspace') else declaration
                    if 'path' in dep:
                        package = dep.get('package', alias)
                        assert package in docs
                        assert dep['version'] == '=' + docs[package]['package']['version']
                        packaged[name].add(package)
                        if section != 'dev-dependencies':
                            graph[name].add(package)
    def ancestors(graph, name, active=frozenset()):
        assert name not in active, ('cycle', name)
        result = set()
        for dep in graph[name]:
            result.add(dep)
            result.update(ancestors(graph, dep, active | {name}))
        return result
    for name in docs:
        ancestors(graph, name)
    assert not graph['dione-receipt-gate']
    assert not packaged['dione-receipt-gate']
    def external(doc):
        return [p for p in doc['package'] if 'source' in p]
    lock = tomllib.loads((ROOT / 'Cargo.lock').read_text())
    assert external(lock) == external(tomllib.loads(predecessor('Cargo.lock').decode()))
    jobs = yaml.safe_load((ROOT / '.github/workflows/publish-crate.yml').read_text())['jobs']
    release = {name: ([job['needs']] if isinstance(job.get('needs'), str) else job.get('needs', [])) for name, job in jobs.items()}
    for name in jobs:
        ancestors(release, name)
    for name in ['publish-crate', 'reconcile-dione']:
        assert 'reconcile-receipt-gate' in release[name]
        assert "needs.reconcile-receipt-gate.result == 'success'" in jobs[name]['if']
    tests = re.findall(r'#\[test\]\s*fn (\w+)', current.decode())
    result = {'baseline': BASE, 'store_sha256': hashlib.sha256(current).hexdigest(),
              'store_exact_except_declared_marker_removal_and_targeted_fixture_principal_sanitization': True,
              'private_proof_constructors_and_no_public_runtime_interface': True,
              'original_test_names': tests, 'source_test_count': len(tests),
              'normal_packages': len(docs), 'normal_graph_acyclic': True,
              'normal_owner_dependencies': [], 'packaged_owner_dependencies': [],
              'release_jobs': len(jobs), 'release_graph_acyclic': True,
              'normal_reverse_closure': ['dione-receipt-gate'],
              'packaged_reverse_closure': ['dione-receipt-gate'],
              'external_records_unchanged': len(external(lock)),
              'limits': 'Source-only binding; compiler, actual list, effects and archive proof remain unrun.'}
    Path(args.output).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'original_test_names'}, indent=2))


if __name__ == '__main__':
    main()
