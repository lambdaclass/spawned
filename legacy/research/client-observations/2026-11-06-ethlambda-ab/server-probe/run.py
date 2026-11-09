"""Sequential, resource-bounded, randomized fresh-process probe runs on fede."""
import hashlib
import json
import os
from pathlib import Path
import random
import signal
import subprocess
import time
import urllib.request

base = Path('/tmp/spawned-pr-security-review')
root = base / 'engine-actor-probe'
out = root / 'results'
assert not out.exists(), 'use a fresh results directory; preserve previous evidence'
out.mkdir()
os.sched_setaffinity(0, {2, 3, 4, 5})
os.nice(max(0, 10 - os.getpriority(os.PRIO_PROCESS, 0)))
env = os.environ.copy()
for key in list(env):
    if key.startswith('SPAWNED_') or key in {'LD_LIBRARY_PATH', 'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy'}:
        env.pop(key, None)
env.update(TOKIO_WORKER_THREADS='3', NO_PROXY='127.0.0.1', no_proxy='127.0.0.1')
profiles = [('baseline05', 'baseline05', 0), ('control06', 'control06', 0),
            ('metrics06', 'metrics06', 1), ('counters06', 'metrics06', 0)]
cells = []
for repeat in range(3):
    for scenario, placements, loads, seconds, warmup in [
        ('immediate', ['async', 'thread'], ['idle', 'busy'], 3, 8),
        ('delay20ms', ['thread'], ['idle', 'busy'], 3, 3),
        ('retry503', ['thread'], ['idle'], 1, 1),
    ]:
        for placement in placements:
            for load in loads:
                for label, binary, sample in profiles:
                    cells.append(dict(variant=label, binary=binary, sample=sample, repeat=repeat,
                                      scenario=scenario, placement=placement, load=load,
                                      seconds=seconds, warmup=warmup))
random.Random(20261108).shuffle(cells)
(out / 'plan.json').write_text(json.dumps({'seed': 20261108, 'client_cpus': [2, 3, 4],
    'mock_cpu': [5], 'nice': 10, 'cells': cells, 'builds': json.loads((root / 'builds.json').read_text())}, indent=2) + '\n')


def group_usage(pgid):
    rss = threads = fds = 0
    for proc in Path('/proc').glob('[0-9]*/stat'):
        try:
            fields = proc.read_text().rsplit(')', 1)[1].split()
            if int(fields[2]) != pgid:
                continue
            status = {x.split(':', 1)[0]: x.split(':', 1)[1].strip() for x in (proc.parent / 'status').read_text().splitlines() if ':' in x}
            rss += int(status.get('VmRSS', '0').split()[0]) * 1024
            threads += int(status.get('Threads', '0'))
            fds += len(list((proc.parent / 'fd').iterdir()))
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            pass
    return rss, threads, fds


for index, cell in enumerate(cells):
    folder = out / f'{index:03}-{cell["variant"]}-{cell["scenario"]}-{cell["placement"]}-{cell["load"]}-r{cell["repeat"]}'
    folder.mkdir()
    ready, server_result = folder / 'ready.json', folder / 'mock.json'
    with (folder / 'mock.log').open('w') as server_log, (folder / 'stdout.json').open('w') as stdout, (folder / 'stderr.log').open('w') as stderr:
        mock = subprocess.Popen(['taskset', '-c', '5', 'python3', str(base / 'mock_engine.py'),
            '--scenario', cell['scenario'], '--ready', str(ready), '--result', str(server_result)],
            env=env, stdout=server_log, stderr=subprocess.STDOUT, start_new_session=True)
        client = None
        began = time.monotonic()
        peak_rss = peak_threads = peak_fds = 0
        failure = None
        try:
            while not ready.exists():
                assert mock.poll() is None and time.monotonic() - began < 10, 'mock startup failed'
                time.sleep(.05)
            endpoint = json.loads(ready.read_text())['endpoint']
            command = ['taskset', '-c', '2,3,4', str(root / 'bin' / cell['binary']), endpoint,
                cell['placement'], cell['load'], str(cell['seconds']), str(cell['warmup']), str(cell['sample'])]
            client = subprocess.Popen(command, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
            while client.poll() is None:
                usage = [group_usage(p.pid) for p in (mock, client)]
                rss, threads, fds = [sum(u[k] for u in usage) for k in range(3)]
                peak_rss, peak_threads, peak_fds = max(rss, peak_rss), max(threads, peak_threads), max(fds, peak_fds)
                if rss > 1024**3 or threads > 96 or fds > 1024 or time.monotonic() - began > 35:
                    failure = 'run resource/time budget exceeded'
                    break
                time.sleep(.1)
            if failure:
                raise RuntimeError(failure)
            assert client.wait() == 0, 'probe failed; inspect stderr'
            with urllib.request.urlopen(endpoint + '/finish', timeout=3) as response:
                assert response.read() == b'ok'
            assert mock.wait(timeout=5) == 0
        finally:
            for proc in (client, mock):
                if proc is not None and proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()
    data = json.loads((folder / 'stdout.json').read_text())
    server = json.loads(server_result.read_text())
    calls = data['requests'] + data['warmup']
    assert server['unique_calls'] == calls
    assert server['injected_failures'] == (calls if cell['scenario'] == 'retry503' else 0)
    assert server['requests'] == calls + server['injected_failures']
    assert data['metrics'] == (cell['variant'] in {'metrics06', 'counters06'})
    assert data['backend'] == cell['placement'] and data['load'] == cell['load']
    assert len(data['samples_ns']) == data['requests']
    for sample in data['samples_ns']:
        assert sum(sample[:4]) == sample[4]
    record = {**cell, 'command': command, 'returncode': 0, 'sampled_peak_rss_bytes': peak_rss,
              'sampled_peak_threads': peak_threads, 'sampled_peak_fds': peak_fds,
              'artifact_sha256': hashlib.sha256((root / 'bin' / cell['binary']).read_bytes()).hexdigest(),
              'elapsed_s_including_setup': time.monotonic() - began}
    (folder / 'audit.json').write_text(json.dumps(record, indent=2) + '\n')
    (folder / 'COMPLETE').write_text('RPC counts, injected retries, stage conservation and resources checked\n')
    print(index + 1, '/', len(cells), cell['variant'], cell['scenario'], cell['placement'], cell['load'],
          'requests', data['requests'], 'total_p99_ms', round(data['stages']['total']['p99_ns'] / 1e6, 4), flush=True)
(out / 'COMPLETE').write_text(f'{len(cells)} fresh-process runs passed\n')
