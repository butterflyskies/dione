import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('ci-resource-snapshot.py')


class ResourceSnapshotTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location('resource_snapshot', SCRIPT)
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)
        self.root = tempfile.TemporaryDirectory()
        self.addCleanup(self.root.cleanup)
        self.proc = Path(self.root.name) / 'proc'
        self.group = Path(self.root.name) / 'cgroup'
        (self.proc / 'self').mkdir(parents=True)
        self.group.mkdir()

    def write(self, path, text):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def test_missing_metrics_are_unknown_not_zero(self):
        row = self.module.snapshot('before-nextest', self.proc, self.group)
        self.assertEqual(row['cgroup']['status'], 'unknown')
        self.assertEqual(row['kernel_pressure']['io']['status'], 'unknown')
        self.assertNotIn('environment', row)

    def test_membership_scopes_counters_and_excludes_unrelated_files(self):
        self.write(self.proc / 'self/cgroup', '0::/job\n')
        self.write(self.group / 'job/cpu.max', '200000 100000\n')
        self.write(self.group / 'job/cpu.stat', 'usage_usec 100\nnr_throttled 2\n')
        self.write(self.group / 'job/memory.events', 'oom_kill 0\n')
        self.write(self.group / 'job/token', 'PRIVATE-SENTINEL')
        row = self.module.snapshot('after-nextest', self.proc, self.group)
        self.assertEqual(row['cgroup']['membership'], '/job')
        self.assertEqual(row['cgroup']['files']['cpu.max']['value'], '200000 100000')
        self.assertEqual(row['cgroup']['files']['memory.events']['value'], 'oom_kill 0')
        self.assertNotIn('PRIVATE-SENTINEL', str(row))

    def test_escape_membership_does_not_fall_back_to_root_counters(self):
        self.write(self.proc / 'self/cgroup', '0::/../../outside\n')
        self.write(self.group / 'cpu.stat', 'usage_usec 999\n')
        row = self.module.snapshot('before-nextest', self.proc, self.group)
        self.assertEqual(row['cgroup']['status'], 'unknown')
        self.assertNotIn('usage_usec', str(row))

    def test_oversized_metric_is_unknown(self):
        self.write(self.proc / 'pressure/io', 'x' * 20000)
        row = self.module.snapshot('before-nextest', self.proc, self.group)
        self.assertEqual(row['kernel_pressure']['io']['status'], 'unknown')

    def test_available_cpu_count_is_not_claimed_as_effective_test_workers(self):
        row = self.module.snapshot('before-nextest', self.proc, self.group)
        self.assertEqual(row['test_workers']['effective'], 'unknown')
        self.assertIn('utc', row)
        self.assertIn('monotonic_seconds', row)


if __name__ == '__main__':
    unittest.main()
