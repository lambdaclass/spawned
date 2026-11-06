"""End-to-end research-driver checks; run on Linux with the built bench binary.

Set SPAWNED_RESEARCH_BINARY to the built bench, then run
python3 -m unittest discover -s benchmarks -p test_research_driver.py.
"""
import csv
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from fede_research import process


@unittest.skipUnless(os.environ.get('SPAWNED_RESEARCH_BINARY'), 'requires built Linux research bench')
class ResearchDriver(unittest.TestCase):
    def invoke(self, root, selector, flags):
        env = dict(os.environ, LC_ALL='C')
        for key in ['SPAWNED_EXPERIMENT_SCHEDULER', 'SPAWNED_PROFILE_PREFIX', 'SPAWNED_EXPERIMENT_RUNTIME']:
            env.pop(key, None)
        if selector:
            env.update(SPAWNED_EXPERIMENT_SCHEDULER='sharded', SPAWNED_EXPERIMENT_WORKERS='2', SPAWNED_EXPERIMENT_BATCH='16')
        command = ['taskset', '-c', '2,3,4,5', os.environ['SPAWNED_RESEARCH_BINARY'],
                   '--workers=2', '--actors=32', '--producers=2', '--driver-cpus=6,7,8,9',
                   f'--series={root}/series.csv', f'--threads={root}/threads.csv', *flags]
        status, stdout, stderr = process(command, env, Path(root)/'process', 60)
        self.assertEqual(status, 'ok', stderr)
        rows = list(csv.DictReader(stdout.splitlines()))
        self.assertEqual(len(rows), 1)
        with (Path(root)/'series.csv').open() as file:
            series = list(csv.DictReader(file))
        self.assertEqual(int(series[-1]['queued']), 0)
        self.assertEqual(int(series[-1]['outstanding']), 0)
        return rows[0], series

    def test_timeout_keeps_work_and_cold_replies_are_observed_independently(self):
        for selector in ['', 'sharded']:
            with self.subTest(selector=selector), tempfile.TemporaryDirectory() as root:
                row, series = self.invoke(root, selector, ['--mode=offered', '--layout=concentrated',
                    '--messages=2000', '--rate=1000000', '--iterations=100', '--hot-iterations=1000000', '--deadline-ms=100'])
                self.assertEqual(int(row['messages']), 2000)
                self.assertEqual(int(row['succeeded'])+int(row['timeouts']), 2000)
                self.assertEqual(int(series[-1]['completed']), 2000)
                self.assertGreater(int(row['hot_timeouts']), 0)
                self.assertGreater(max(int(r['hot_queued']) for r in series), 0)
                # A sequential hot-first reply collector would delay the fast
                # other-shard cohort until the hot request's 100 ms deadline.
                if selector:
                    self.assertLess(int(row['cold_other_observed_p99_ns']), 100_000_000)
                self.assertEqual(int(row['cold_samples']), 400)
                self.assertEqual(int(row['hot_samples']), 1600)
                print(selector or 'async', {k:row[k] for k in ['timeouts','hot_timeouts','cold_timeouts','cold_other_observed_p99_ns']})

    def test_strict_controls_fail_before_series_or_workers(self):
        for flags in [['--unknown=1'],['--messages=0'],['--mode=offered','--layout=spread','--rate=0'],['--width=1']]:
            with self.subTest(flags=flags), tempfile.TemporaryDirectory() as root:
                result = subprocess.run([os.environ['SPAWNED_RESEARCH_BINARY'], f'--series={root}/series.csv', *flags],
                                        text=True, capture_output=True, timeout=5)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((Path(root)/'series.csv').exists())


if __name__ == '__main__':
    unittest.main()
