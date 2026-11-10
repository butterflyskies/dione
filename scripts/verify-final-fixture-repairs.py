#!/usr/bin/env python3
"""Bind the two exact fixture corrections to the accepted receipt source."""
import argparse
import ast
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tomllib

BASE = 'a76727a6bf246a0e4f2c695e9b183baebe22cd77'
ROOT = Path(__file__).resolve().parent.parent


def old(path):
    return subprocess.check_output(['git', 'show', BASE + ':' + path], cwd=ROOT)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    bell_path = 'src/discord_bells_tests.rs'
    before = old(bell_path)
    principal_line = before.splitlines()[357]
    identified = re.search(rb'BellScope::parse\("([^"]+)"\)', principal_line)[1]
    assert before.count(identified) == 10
    current = (ROOT / bell_path).read_bytes()
    assert current == before.replace(identified, b'fixture-orbit-27')
    assert identified not in current
    proxy_checks = []
    for path in ['src/codex_test_process.rs', 'crates/dione-mcp-session/src/codex_test_process.rs']:
        predecessor = old(path).decode()
        candidate = (ROOT / path).read_text()
        prefix, suffix = 'impl Proxy {\n', '\nimpl HarnessDelivery for Proxy {'
        original_proxy = predecessor.split(prefix, 1)[1].split(suffix, 1)[0]
        replacement_proxy = candidate.split(prefix, 1)[1].split(suffix, 1)[0]
        assert hashlib.sha256(replacement_proxy.encode()).hexdigest() == '90c691a5e9313a307ba8c954a991cbbd11366c098f9f9bf20263c981bc63dc44'
        reversed_candidate = candidate.replace(replacement_proxy, original_proxy, 1)
        assert reversed_candidate.count('.timeout(Duration::from_secs(120))') == 1
        reversed_candidate = reversed_candidate.replace('.timeout(Duration::from_secs(120))', '.timeout(Duration::from_secs(10))', 1)
        assert reversed_candidate == predecessor
        proxy_checks.append({'path': path, 'exact_only_proxy_diagnostic_and_request_allowance_delta': True,
                             'private_cfgtest': True, 'production_changed': False})
    for path in (ROOT / 'crates/dione-receipt-gate').rglob('*'):
        if path.is_file():
            assert path.read_bytes() == old(str(path.relative_to(ROOT)))
    assert (ROOT / '.github/workflows/publish-crate.yml').read_bytes() == old('.github/workflows/publish-crate.yml')
    for path in subprocess.check_output(['git', 'ls-tree', '-r', '--name-only', BASE, 'src', 'crates'], cwd=ROOT, text=True).splitlines():
        if path.endswith('.rs') and path not in [bell_path, *[row['path'] for row in proxy_checks]]:
            assert (ROOT / path).read_bytes() == old(path), path
    for path in ['src/lib.rs', 'crates/dione-mcp-session/src/lib.rs']:
        assert re.search(r'#\[cfg\(test\)\]\nmod codex_test_process;', (ROOT / path).read_text())
    workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text())
    documents = {}
    for path in [ROOT / 'Cargo.toml', *sorted((ROOT / 'crates').glob('*/Cargo.toml'))]:
        document = tomllib.loads(path.read_text())
        documents[document['package']['name']] = document
    normal = {name: set() for name in documents}
    packaged = {name: set() for name in documents}
    for name, document in documents.items():
        for table in [document, *document.get('target', {}).values()]:
            for section in ['dependencies', 'build-dependencies', 'dev-dependencies']:
                for alias, declaration in table.get(section, {}).items():
                    if not isinstance(declaration, dict):
                        continue
                    dep = workspace['workspace']['dependencies'][alias] if declaration.get('workspace') else declaration
                    if 'path' in dep:
                        package = dep.get('package', alias)
                        assert dep['version'] == '=' + documents[package]['package']['version']
                        packaged[name].add(package)
                        if section != 'dev-dependencies':
                            normal[name].add(package)
    def reverse_closure(graph):
        closure = {'dione-mcp-session'}
        while True:
            enlarged = closure | {name for name, deps in graph.items() if deps & closure}
            if enlarged == closure:
                return sorted(closure)
            closure = enlarged
    assert reverse_closure(normal) == ['dione', 'dione-mcp-session']
    assert reverse_closure(packaged) == ['dione', 'dione-mcp-session']
    baseline_lock = tomllib.loads(old('Cargo.lock').decode())
    lock = tomllib.loads((ROOT / 'Cargo.lock').read_text())
    def external(document):
        return [package for package in document['package'] if 'source' in package]
    assert external(lock) == external(baseline_lock)
    root = tomllib.loads((ROOT / 'Cargo.toml').read_text())
    assert root['package']['version'] == '0.150.9'
    assert root['workspace']['dependencies']['dione-mcp-session']['version'] == '=0.1.1'
    assert tomllib.loads((ROOT / 'crates/dione-mcp-session/Cargo.toml').read_text())['package']['version'] == '0.1.1'
    ns = {'re': re}
    for node in ast.parse((ROOT / 'scripts/verify-recipient-attention-migration.py').read_text()).body:
        if isinstance(node, ast.FunctionDef) and node.name in ['tokens', 'funcs']:
            exec(compile(ast.Module(body=[node], type_ignores=[]), 'maintained lexical reader', 'exec'), ns)
    text = current.decode()
    test_names = re.findall(r'#\[(?:tokio::)?test\]\s*(?:async )?fn (\w+)', text)
    cases = [function['name'] for function in ns['funcs'](text) if function['name'] in test_names and
             (re.search(r'\b(?:config|hit)\(', function['body']) or 'fixture-orbit-27' in function['body'])]
    result = {'base': BASE, 'Bell_exact_ten_substitutions': True,
              'identified_source_token_sha256': hashlib.sha256(identified).hexdigest(),
              'substitution_lines': [i for i, line in enumerate(before.splitlines(), 1) if identified in line],
              'invented_namespace': 'fixture-orbit-27', 'proxy_copies': proxy_checks,
              'all_other_Rust_bytes_unchanged': True, 'accepted_receipt_bytes_and_release_workflow_unchanged': True,
              'external_records_unchanged': len(external(lock)), 'normal_reverse_version_closure': reverse_closure(normal),
              'packaged_reverse_version_closure': reverse_closure(packaged),
              'normal_packages': len(documents),
              'affected_Bell_test_names': cases, 'affected_Bell_test_count': len(cases),
              'Codex_exact_named_case': 'owned_admission_completion_retains_native_authority_after_queue_restart',
              'limits': 'Source-only binding. Historical timeout command/cause unknown. No native result or official approval.'}
    Path(args.output).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
