#!/usr/bin/env python3
"""Reproducible Linux-only graph, offered-load and SIGPROF research campaigns."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import signal
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
VARIANTS = {
    'async': ('', 0), 'tokio-1': ('tokio-batch', 1),
    'tokio-16': ('tokio-batch', 16), 'tokio-adaptive': ('tokio-batch', 0),
    'sharded-1': ('sharded', 1), 'sharded-16': ('sharded', 16),
    'sharded-adaptive': ('sharded', 0),
}
SELECTORS = ['SPAWNED_EXPERIMENT_SCHEDULER', 'SPAWNED_EXPERIMENT_WORKERS',
             'SPAWNED_EXPERIMENT_BATCH', 'SPAWNED_EXPERIMENT_RUNTIME',
             'SPAWNED_PROFILE_PREFIX', 'TOKIO_WORKER_THREADS']


def capture(*command):
    return subprocess.check_output(command, cwd=ROOT, text=True).strip()


def write_rows(path, rows):
    fields = list(dict.fromkeys(k for row in rows for k in row))
    with path.open('w') as file:
        writer = csv.DictWriter(file, fields, lineterminator='\n')
        writer.writeheader()
        writer.writerows(rows)


def process(invocation, env, prefix, timeout):
    child = subprocess.Popen(invocation, cwd=ROOT, env=env, text=True,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             start_new_session=True)
    status = 'ok'
    try:
        stdout, stderr = child.communicate(timeout=timeout)
        if child.returncode:
            status = f'exit_{child.returncode}'
    except BaseException as error:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        stdout, stderr = child.communicate()
        prefix.with_suffix('.stdout').write_text(stdout)
        prefix.with_suffix('.stderr').write_text(stderr)
        if not isinstance(error, subprocess.TimeoutExpired):
            raise
        status = 'timeout'
    prefix.with_suffix('.stdout').write_text(stdout)
    prefix.with_suffix('.stderr').write_text(stderr)
    return status, stdout, stderr


class Campaign:
    def __init__(self, args):
        self.args = args
        self.out = args.output.resolve()
        self.out.mkdir(parents=True, exist_ok=False)
        for directory in ['logs', 'series', 'threads', 'profiles']:
            (self.out / directory).mkdir()
        self.rows = []
        self.schedule = []
        self.meta = {
            'started_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
            'revision': capture('git', 'rev-parse', 'HEAD'),
            'dirty': bool(capture('git', 'status', '--porcelain')),
            'platform': platform.platform(), 'rustc': capture('rustc', '--version'),
            'cargo': capture('cargo', '--version'), 'cpu': capture('lscpu'),
            'memory': Path('/proc/meminfo').read_text(),
            'load_before': Path('/proc/loadavg').read_text().strip(),
            'actor_cpus': args.actor_cpus, 'driver_cpus': args.driver_cpus, 'collector_cpus': args.collector_cpus,
            'affinity': 'workload_research: actor CPUs, separate producer CPUs, separate collector/sampler CPUs; legacy designs profiles keep all threads on actor CPUs',
            'nice': os.nice(0), 'perf_event_paranoid': Path('/proc/sys/kernel/perf_event_paranoid').read_text().strip(),
            'environment': {k: os.environ.get(k) for k in ['RUSTFLAGS', 'CARGO_PROFILE_BENCH_DEBUG', 'CARGO_BUILD_JOBS']},
            'arguments': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
            'topology': {}, 'schedule': self.schedule,
        }
        actor_set, driver_set = self.cpu_set(args.actor_cpus), self.cpu_set(args.driver_cpus)
        collector_set = self.cpu_set(args.collector_cpus)
        assert not (actor_set & driver_set or actor_set & collector_set or driver_set & collector_set), 'actor, producer and collector CPU sets must be disjoint'
        assert os.sched_getaffinity(0) == actor_set, 'launch campaign under taskset on actor CPUs'
        cores = set()
        for cpu in sorted(actor_set | driver_set | collector_set):
            root = Path(f'/sys/devices/system/cpu/cpu{cpu}/topology')
            core = (root / 'physical_package_id').read_text().strip(), (root / 'core_id').read_text().strip()
            assert core not in cores, 'do not use SMT siblings as separate research cores'
            cores.add(core)
            self.meta['topology'][cpu] = dict(package=core[0], core=core[1])
        subprocess.run(['taskset', '-c', args.driver_cpus, 'true'], check=True)
        subprocess.run(['taskset', '-c', args.collector_cpus, 'true'], check=True)
        command = ['cargo', 'bench', '--locked', '-p', 'spawned-concurrency',
                   '--features', 'backend-experiments,profiling-experiments',
                   '--bench', 'workload_research', '--bench', 'designs',
                   '--no-run', '--message-format=json']
        build = subprocess.run(command, cwd=ROOT, text=True, capture_output=True)
        (self.out / 'build.log').write_text(build.stderr)
        build.check_returncode()
        self.binaries = {}
        for line in build.stdout.splitlines():
            record = json.loads(line)
            if record.get('reason') == 'compiler-artifact' and record.get('executable'):
                self.binaries[record['target']['name']] = record['executable']
        self.meta['binaries'] = {name: hashlib.sha256(Path(binary).read_bytes()).hexdigest()
                                 for name, binary in self.binaries.items()}
        self.save()

    @staticmethod
    def cpu_set(value):
        result = set()
        for part in value.split(','):
            if '-' in part:
                low, high = map(int, part.split('-'))
                result.update(range(low, high + 1))
            else:
                result.add(int(part))
        return result

    def save(self):
        (self.out / 'metadata.json').write_text(json.dumps(self.meta, indent=2) + '\n')
        if self.rows:
            write_rows(self.out / 'raw.csv', self.rows)

    def sample(self, phase, variant, workers, config, repeat, bench='workload_research'):
        index = len(self.rows)
        stem = f'{index:04d}-{phase}-{variant}-w{workers}-{repeat}'
        env = dict(os.environ, LC_ALL='C')
        for key in SELECTORS:
            env.pop(key, None)
        scheduler, batch = VARIANTS[variant]
        if scheduler:
            env.update(SPAWNED_EXPERIMENT_SCHEDULER=scheduler,
                       SPAWNED_EXPERIMENT_WORKERS=str(workers),
                       SPAWNED_EXPERIMENT_BATCH=str(batch))
        config = dict(config, workers=workers)
        if bench == 'workload_research':
            config.update(**{'driver-cpus': self.args.driver_cpus,
                             'collector-cpus': self.args.collector_cpus,
                             'series': str(self.out / 'series' / f'{stem}.csv'),
                             'threads': str(self.out / 'threads' / f'{stem}.csv')})
        else:
            config.update(backend='sharded' if scheduler == 'sharded' else 'async',
                          observer='direct', batch=batch)
        if phase == 'profile':
            env['SPAWNED_PROFILE_PREFIX'] = str(self.out / 'profiles' / stem)
        invocation = ['taskset', '-c', self.args.actor_cpus, '/usr/bin/time', '-v',
                      self.binaries[bench]] + [f'--{k}={v}' for k, v in config.items()]
        self.schedule.append(dict(phase=phase, variant=variant, workers=workers,
                                  repeat=repeat, config=config))
        self.save()
        start = time.monotonic()
        status, stdout, stderr = process(invocation, env, self.out / 'logs' / stem, self.args.timeout)
        row = dict(index=index, phase=phase, variant=variant, workers=workers, repeat=repeat,
                   status=status, wall_s=time.monotonic()-start, config_json=json.dumps(config, sort_keys=True), stem=stem)
        if status == 'ok':
            parsed = list(csv.DictReader(stdout.splitlines()))
            if len(parsed) != 1 or None in parsed[0] or any(v is None for v in parsed[0].values()):
                row['status'] = 'invalid_output'
            else:
                row.update(parsed[0])
                if bench == 'workload_research':
                    assert int(row['messages']) == int(row['succeeded']) + int(row['timeouts'])
                    row['driver_late_fraction'] = int(row['lag_over_1ms']) / int(row['messages'])
                    row['driver_valid'] = row['driver_late_fraction'] <= 0.01
                    self.add_series(row)
        rss = re.search(r'Maximum resident set size \(kbytes\):\s*(\d+)', stderr)
        user = re.search(r'User time \(seconds\):\s*([\d.]+)', stderr)
        system = re.search(r'System time \(seconds\):\s*([\d.]+)', stderr)
        row['rss_bytes'] = int(rss[1])*1024 if rss else ''
        row['cpu_s'] = float(user[1])+float(system[1]) if user and system else ''
        if row['status'] == 'ok' and (not rss or not user or not system):
            row['status'] = 'missing_resource_metrics'
        self.rows.append(row)
        self.save()
        print(f'{len(self.rows)} {phase} {variant} w{workers} r{repeat}: {row["status"]}', flush=True)
        if row['status'] != 'ok':
            raise RuntimeError(f'campaign stopped at {stem}; diagnostic files retained')
        return row

    def add_series(self, row):
        with (self.out / 'series' / f'{row["stem"]}.csv').open() as file:
            series = list(csv.DictReader(file))
        assert series, 'missing queue/RSS samples'
        for field in ['queued', 'outstanding', 'hot_queued', 'cold_queued', 'rss_bytes']:
            row[f'peak_{field}'] = max(int(r[field]) for r in series)
        row['final_queued'] = int(series[-1]['queued'])
        row['final_outstanding'] = int(series[-1]['outstanding'])
        config = json.loads(row['config_json'])
        if config['mode'] != 'graph':
            assert int(series[-1]['completed']) == int(row['messages'])
            assert row['final_queued'] == row['final_outstanding'] == 0
        if config['mode'] == 'offered':
            duration = int(config['messages']) / int(config['rate'])
            phase = [r for r in series if duration*0.25 <= float(r['elapsed_ms'])/1000 <= duration*0.9]
            if len(phase) >= 2:
                dt = (float(phase[-1]['elapsed_ms'])-float(phase[0]['elapsed_ms']))/1000
                row['window_service_per_s'] = (int(phase[-1]['completed'])-int(phase[0]['completed']))/dt
                row['window_queue_growth_per_s'] = (int(phase[-1]['queued'])-int(phase[0]['queued']))/dt

    def calibration(self):
        estimates = {}
        for workers in self.args.workers:
            references = [self.sample('calibration', 'async', workers,
                dict(mode='closed', layout='spread', actors=128, producers=128,
                     messages=65536, iterations=100, **{'hot-iterations':5000}), r)
                for r in range(3)]
            reference = statistics.median(float(r['replies_per_s']) for r in references)
            pilots = []
            for ratio in [1, 2, 4]:
                rate = round(reference*ratio)
                count = min(round(rate*2), 4_000_000)
                pilots.append(self.sample('pilot', 'async', workers,
                    dict(mode='offered', layout='spread', actors=128, producers=4,
                         messages=count, rate=rate, iterations=100,
                         **{'hot-iterations':5000}), ratio))
            valid = [float(r['window_service_per_s']) for r in pilots
                     if r.get('driver_valid') and r.get('window_service_per_s')]
            if not valid:
                raise RuntimeError('baseline pilot could not produce a valid offered-rate schedule')
            capacity = max(valid)
            estimates[workers] = capacity
            self.meta.setdefault('calibration', {})[workers] = dict(
                closed_reference_per_s=reference, service_reference_per_s=capacity,
                pilot_indices=[r['index'] for r in pilots],
                interpretation='reference estimated from valid pilot in-window service; inspect backlog and producer lag before calling any grid point saturation')
            self.save()
        return estimates

    def run(self):
        if self.args.smoke:
            for v in self.args.variants:
                for topology in ['chain', 'fanout']:
                    for layout in ['same', 'cross']:
                        self.sample('smoke', v, 2, dict(mode='graph', topology=topology, layout=layout,
                            actors=32, width=4, producers=4, messages=128), 0)
                for layout in ['spread', 'concentrated']:
                    self.sample('smoke', v, 2, dict(mode='offered', layout=layout, actors=16,
                        producers=2, messages=1000, rate=10000, **{'deadline-ms':100}), 0)
            self.sample('profile', 'async', 2, dict(mode='graph', topology='chain', layout='cross',
                actors=32, width=4, producers=4, messages=65536, iterations=1000), 0)
            return
        estimates = self.calibration()
        graph_jobs = [(v,w,t,l,r) for v in self.args.variants for w in self.args.workers
                      for t in ['chain','fanout'] for l in ['same','cross'] for r in range(5)]
        random.Random(self.args.seed).shuffle(graph_jobs)
        for v,w,t,l,r in graph_jobs:
            self.sample('graph',v,w,dict(mode='graph',topology=t,layout=l,actors=1024,
                width=8,producers=256,messages=65536,iterations=100),r)
        load_jobs = [(v,w,l,ratio,burst,r) for v in self.args.variants for w in self.args.workers
                     for l in ['spread','concentrated'] for ratio,burst in [(0.5,1),(0.9,1),(1.05,1),(1.25,1),(1.05,1000)]
                     for r in range(3)]
        random.Random(self.args.seed+1).shuffle(load_jobs)
        for v,w,l,ratio,burst,r in load_jobs:
            rate=max(100,round(estimates[w]*ratio))
            count=min(round(rate*self.args.duration),4_000_000)
            self.sample('offered',v,w,dict(mode='offered',layout=l,actors=128,producers=4,
                messages=count,rate=rate,burst=burst,iterations=100,**{'hot-iterations':5000}),r)
        for w in self.args.workers:
            for v in ['async','sharded-1','sharded-16']:
                for case in ['cpu','hotspot','tcp']:
                    config=dict(scenario='rpc',actors=1024 if case=='cpu' else 128,
                        producers=256 if case=='cpu' else 128,messages=1_000_000,
                        routing='hotspot' if case=='hotspot' else 'round-robin',
                        work='tcp' if case=='tcp' else 'cpu',iterations=1000 if case=='hotspot' else 0,
                        warmup=128)
                    for r in range(3):
                        self.sample('profile',v,w,config,r,bench='designs')
                for placement in ['same','cross']:
                    self.sample('profile',v,w,dict(mode='graph',topology='chain',layout=placement,
                        actors=1024,width=8,producers=256,messages=524288,iterations=100),0)
        self.meta['finished_utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime())
        self.meta['load_after']=Path('/proc/loadavg').read_text().strip()
        self.save()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--workers',type=int,nargs='+',default=[2,4])
    parser.add_argument('--variants',choices=VARIANTS,nargs='+',default=list(VARIANTS))
    parser.add_argument('--actor-cpus',default='2,3,4,5')
    parser.add_argument('--driver-cpus',default='6,7,8,9')
    parser.add_argument('--collector-cpus',default='10,11,12,13')
    parser.add_argument('--seed',type=int,default=181)
    parser.add_argument('--duration',type=float,default=2.0)
    parser.add_argument('--timeout',type=float,default=120)
    parser.add_argument('--smoke',action='store_true')
    args=parser.parse_args()
    assert platform.system()=='Linux' and args.duration>0 and args.timeout>0
    Campaign(args).run()


if __name__=='__main__':
    main()
