#!/usr/bin/env python3
"""Exercise the actual packaged-prerequisite oracle with discriminating omissions."""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import tomllib

import yaml

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('release_prerequisites', ROOT / 'scripts/verify-release-prerequisites.py')
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['dependencies']
document = tomllib.loads((ROOT / 'crates/dione-harness-runtime/Cargo.toml').read_text())
prerequisites = checker.local_dependencies(document, workspace)
jobs = yaml.safe_load((ROOT / '.github/workflows/publish-crate.yml').read_text())['jobs']
checker.verify(jobs, 'dione-harness-runtime', prerequisites)
results = [{'case': 'actual repaired graph', 'outcome': 'pass'}]
for name in ['publish-harness-runtime', 'reconcile-harness-runtime']:
    for omission in ['edge', 'guard']:
        mutant = copy.deepcopy(jobs)
        if omission == 'edge':
            mutant[name]['needs'].remove('reconcile-discord-events')
        else:
            mutant[name]['if'] = mutant[name]['if'].replace("needs.reconcile-discord-events.result == 'success' &&", '')
        try:
            checker.verify(mutant, 'dione-harness-runtime', prerequisites)
        except AssertionError as error:
            assert error.args[0] == (name, 'missing direct discord-events edge' if omission == 'edge' else 'missing discord-events success guard')
            results.append({'case': name + ' omit ' + omission, 'outcome': 'intended assertion', 'diagnostic': str(error)})
        else:
            raise AssertionError((name, omission, 'oracle accepted omission'))
baseline = subprocess.check_output(['git', 'show', 'c9a7e547b94ac8d30d44124c3fc1cfc291299f0e:.github/workflows/publish-crate.yml'], cwd=ROOT, text=True)
try:
    checker.verify(yaml.safe_load(baseline)['jobs'], 'dione-harness-runtime', prerequisites)
except AssertionError as error:
    assert error.args[0] == ('publish-harness-runtime', 'missing direct discord-events edge')
    results.append({'case': 'actual predecessor graph', 'outcome': 'intended assertion', 'diagnostic': str(error)})
else:
    raise AssertionError('predecessor graph unexpectedly passed')
for section in ['dependencies', 'dev-dependencies', 'build-dependencies']:
    for target_specific in [False, True]:
        table = {section: {'event_alias': {'workspace': True}}}
        candidate = {'target': {'cfg(unix)': table}} if target_specific else table
        resolved = checker.local_dependencies(candidate, {'event_alias': {'package': 'dione-discord-events', 'path': 'crates/dione-discord-events', 'version': '=0.1.0'}})
        assert resolved == {'dione-discord-events': {section}}
        results.append({'case': section + (' target alias' if target_specific else ' workspace alias'), 'outcome': 'pass'})
print(json.dumps({'checks': results, 'count': len(results), 'limits': 'In-memory source graph mutations, no product runtime or Cargo.'}, indent=2))
