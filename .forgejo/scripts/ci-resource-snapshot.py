#!/usr/bin/env python3
"""Best-effort local counter snapshots; never inspect env, argv or payloads."""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import sys
import time

CGROUP_FILES = (
    'cpu.max', 'cpu.stat', 'cpuset.cpus.effective', 'memory.max',
    'memory.events', 'cpu.pressure', 'memory.pressure', 'io.pressure',
)


def read_metric(path):
    try:
        with path.open() as stream:
            value = stream.read(16385)
        if len(value) > 16384:
            return {'status': 'unknown', 'reason': 'size-limit'}
        return {'status': 'readable', 'value': value.strip()}
    except (OSError, UnicodeError) as error:
        return {'status': 'unknown', 'reason': type(error).__name__}


def cgroup_snapshot(proc, mount):
    membership = read_metric(proc / 'self/cgroup')
    if membership['status'] != 'readable':
        return {'status': 'unknown', 'reason': 'membership-unavailable'}
    paths = [line[3:] for line in membership['value'].splitlines()
             if line.startswith('0::')]
    if len(paths) != 1 or not paths[0].startswith('/'):
        return {'status': 'unknown', 'reason': 'v2-membership-unavailable'}
    relative = Path(paths[0].lstrip('/'))
    if '..' in relative.parts:
        return {'status': 'unknown', 'reason': 'membership-outside-mount'}
    try:
        root = mount.resolve()
        directory = (root / relative).resolve()
        if not directory.is_relative_to(root) or not directory.is_dir():
            return {'status': 'unknown', 'reason': 'membership-not-mapped'}
    except OSError:
        return {'status': 'unknown', 'reason': 'membership-not-mapped'}
    return {
        'status': 'mapped', 'membership': paths[0], 'mount': str(mount),
        'scope': 'job-visible v2 membership; host binding and ancestor limits unmeasured',
        'files': {name: read_metric(directory / name) for name in CGROUP_FILES},
    }


def snapshot(phase, proc=Path('/proc'), mount=Path('/sys/fs/cgroup')):
    try:
        affinity_count = len(os.sched_getaffinity(0))
    except (AttributeError, OSError):
        affinity_count = 'unknown'
    return {
        'schema': 'dione-ci-resources-v1', 'phase': phase,
        'utc': datetime.now(timezone.utc).isoformat(),
        'monotonic_seconds': time.monotonic(),
        'test_workers': {
            'workflow_basis': 'no explicit Nextest worker override in test command',
            'effective': 'unknown',
            'note': 'Nextest default/config/environment resolution is not sampled; CPU counts are not worker counts',
        },
        'visible_cpu_count': os.cpu_count() or 'unknown', 'affinity_cpu_count': affinity_count,
        'cgroup': cgroup_snapshot(proc, mount),
        'kernel_pressure_scope': 'job-visible /proc/pressure; may include wider host activity',
        'kernel_pressure': {name: read_metric(proc / 'pressure' / name)
                            for name in ('cpu', 'memory', 'io')},
        'limits': 'whole cargo-nextest command including build/preparation; boundary counters only; compare matching scopes, detect resets; no per-stall or syscall attribution',
    }


if __name__ == '__main__':
    if len(sys.argv) != 2 or sys.argv[1] not in ('before-nextest', 'after-nextest'):
        raise SystemExit('expected fixed before-nextest or after-nextest phase')
    print(json.dumps(snapshot(sys.argv[1]), sort_keys=True))
