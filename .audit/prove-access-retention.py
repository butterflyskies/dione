from pathlib import Path
import fcntl
import hashlib
import os
import subprocess

root = Path.cwd().resolve()
assert not str(root).startswith('/mnt/'), 'Use a native Linux checkout.'
path = root / 'src/discord/access.rs'
site = b'            ConfigDurability::Unknown { warning } => {\n'
mutation = site + b'                self.queue.lock().await.approve(uid);\n'
command = ['cargo', 'nextest', 'run', '--lib', '--locked', '--features',
           'oneshot-test-seam', '-E',
           'test(access_contract_unknown_durability_retains_request_and_retry_completes)']
environment = dict(os.environ)
assert environment.get('CARGO_TARGET_DIR'), 'Set the native target directory.'

with (root.parent / 'dione-pr-queries-source.lock').open('a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    assert not subprocess.check_output(['git', 'status', '--porcelain']).strip()
    original = path.read_bytes()
    assert original == subprocess.check_output(['git', 'show', 'HEAD:src/discord/access.rs'])
    assert original.count(site) == 1
    digest = hashlib.sha256(original).hexdigest()
    mutant_log = root.parent / 'dione-access-retention-mutant.log'
    try:
        path.write_bytes(original.replace(site, mutation, 1))
        with mutant_log.open('wb') as output:
            result = subprocess.run(command, env=environment, stdout=output,
                                    stderr=subprocess.STDOUT, timeout=300)
        report = mutant_log.read_text()
        assert result.returncode != 0, 'The lost-retry-request mutant survived.'
        assert '1 failed' in report and 'assertion failed:' in report, report
        assert 'peek(42).is_some()' in report, report
    finally:
        path.write_bytes(original)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == digest
    assert not subprocess.check_output(['git', 'status', '--porcelain']).strip()
    restored_log = root.parent / 'dione-access-retention-restored.log'
    with restored_log.open('wb') as output:
        restored = subprocess.run(command, env=environment, stdout=output,
                                  stderr=subprocess.STDOUT, timeout=300)
    assert restored.returncode == 0, restored_log
    print(f'Mutant rejected; original restored and passed. SHA256 {digest}.')
    print(f'Logs: {mutant_log}; {restored_log}')
