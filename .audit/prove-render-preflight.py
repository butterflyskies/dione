from pathlib import Path
import fcntl
import hashlib
import os
import subprocess

root = Path.cwd().resolve()
assert not str(root).startswith('/mnt/'), 'Use the native Linux verification checkout.'
path = root / 'src/discord/rendering.rs'
site = ('    messaging::check_outbound(ctx, channel_id)\n'
        '        .await\n'
        '        .map_err(messaging_error)?;\n')
command = ['cargo', 'nextest', 'run', '--lib', '--locked', '-E',
           'test(render_contract_preflight_precedence_and_no_http)']
environment = dict(os.environ)
assert environment.get('CARGO_TARGET_DIR'), 'Set the shared native target directory.'

with (root.parent / 'dione-pr-queries-source.lock').open('a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    assert not subprocess.check_output(['git', 'status', '--porcelain']).strip()
    original = path.read_bytes()
    committed = subprocess.check_output(['git', 'show', 'HEAD:src/discord/rendering.rs'])
    assert original == committed
    assert original.count(site.encode()) == 1
    before = hashlib.sha256(original).hexdigest()
    mutant_log = root.parent / 'dione-render-preflight-mutant.log'
    try:
        path.write_bytes(original.replace(site.encode(), b'', 1))
        with mutant_log.open('wb') as output:
            mutant = subprocess.run(command, env=environment, stdout=output,
                                    stderr=subprocess.STDOUT, timeout=300)
        report = mutant_log.read_text()
        assert mutant.returncode != 0, 'The authorization-order mutant survived.'
        assert '1 failed' in report and 'assertion `left == right` failed' in report
        assert 'channel 42 is not a permitted outbound target' in report
        assert 'iftypst is not supported' in report
    finally:
        path.write_bytes(original)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == before
    assert not subprocess.check_output(['git', 'status', '--porcelain']).strip()
    restored_log = root.parent / 'dione-render-preflight-restored.log'
    with restored_log.open('wb') as output:
        restored = subprocess.run(command, env=environment, stdout=output,
                                  stderr=subprocess.STDOUT, timeout=300)
    assert restored.returncode == 0, restored_log
    print(f'Mutant rejected; original restored and passed. SHA256 {before}.')
    print(f'Logs: {mutant_log}; {restored_log}')
