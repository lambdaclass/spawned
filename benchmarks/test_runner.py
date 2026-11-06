"""Exercise watchdog cleanup with a real child, not a mocked subprocess."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import run as runner


@unittest.skipUnless(os.name == 'posix', 'benchmark process groups require POSIX')
class WatchdogTests(unittest.TestCase):
    def test_timeout_kills_grandchild_and_preserves_diagnostics(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            pidfile = root / 'child.pid'
            code = (
                'import subprocess,sys,time,pathlib; '
                'child=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]); '
                f'pathlib.Path({str(pidfile)!r}).write_text(str(child.pid)); '
                'print("ready",flush=True); print("diagnostic",file=sys.stderr,flush=True); '
                'child.wait()'
            )
            with self.assertRaises(subprocess.TimeoutExpired):
                runner.run_sample([sys.executable, '-u', '-c', code], root / 'sample', timeout=1)
            self.assertIn('ready', (root / 'sample.stdout').read_text())
            self.assertIn('diagnostic', (root / 'sample.stderr').read_text())
            pid = int(pidfile.read_text())
            state = subprocess.run(['ps', '-p', str(pid), '-o', 'stat='], capture_output=True, text=True)
            self.assertTrue(state.returncode != 0 or state.stdout.strip().startswith('Z'),
                            f'benchmark descendant still alive: {pid} {state.stdout}')


if __name__ == '__main__':
    unittest.main()
