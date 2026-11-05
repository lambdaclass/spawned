#!/usr/bin/env python3
"""Run process-isolated release benchmarks; retain raw results and metadata."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
BACKENDS = ['async', 'blocking', 'dedicated', 'threads', 'tokio']
CASES = {
    'rpc_single': dict(scenario='rpc', actors=1, producers=1, messages=20000),
    'rpc_parallel': dict(scenario='rpc', actors=4, producers=4, messages=40000),
    'mostly_idle': dict(scenario='rpc', actors=128, producers=4, messages=40000),
    'burst': dict(scenario='burst', actors=1, producers=4, messages=50000),
    'large_shared_payload': dict(scenario='rpc', actors=4, producers=4, messages=20000, payload=4096),
    'cpu_paced': dict(scenario='paced', actors=1, producers=4, messages=10000, rate=20000, iterations=20000),
    'cpu_overload': dict(scenario='paced', actors=1, producers=4, messages=10000, rate=200000, iterations=20000),
    'rpc_timing_off': dict(scenario='rpc', actors=1, producers=1, messages=20000, timing='off'),
    'rpc_allocations': dict(scenario='rpc', actors=1, producers=1, messages=20000, allocations='on'),
}


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repetitions', type=int, default=5)
    parser.add_argument('--seed', type=int, default=176)
    parser.add_argument('--cases', nargs='+', choices=CASES, default=list(CASES))
    parser.add_argument('--backends', nargs='+', choices=BACKENDS, default=BACKENDS)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error('repetitions must be positive')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    (output / 'logs').mkdir()
    build = subprocess.run(['cargo', 'bench', '--locked', '-p', 'spawned-concurrency', '--bench', 'baseline', '--no-run', '--message-format=json'], cwd=ROOT, text=True, capture_output=True)
    (output / 'build.log').write_text(build.stderr)
    build.check_returncode()
    binary = None
    for line in build.stdout.splitlines():
        item = json.loads(line)
        if item.get('reason') == 'compiler-artifact' and item['target']['name'] == 'baseline':
            binary = item.get('executable')
    if not binary:
        raise RuntimeError('benchmark executable not found')
    schedule = [(case, backend, repeat) for repeat in range(args.repetitions) for case in args.cases for backend in args.backends]
    random.Random(args.seed).shuffle(schedule)
    metadata = dict(
        started_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
        revision=command('git', 'rev-parse', 'HEAD'), dirty=bool(command('git', 'status', '--porcelain')),
        rustc=command('rustc', '--version'), cargo=command('cargo', '--version'),
        platform=platform.system(), os_version=platform.release(), architecture=platform.machine(),
        cpu_count=os.cpu_count(), workers=2, affinity='uncontrolled', seed=args.seed,
        repetitions=args.repetitions, cases={case: CASES[case] for case in args.cases},
        binary_sha256=hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
        source_sha256=hashlib.sha256((ROOT / 'concurrency/benches/baseline.rs').read_bytes()).hexdigest(),
        environment={key: os.environ.get(key) for key in ['RUSTFLAGS', 'RUSTC', 'CARGO_PROFILE_BENCH_OPT_LEVEL', 'CARGO_PROFILE_BENCH_LTO']},
        schedule=schedule,
    )
    if platform.system() == 'Darwin':
        metadata['hardware'] = {key: command('sysctl', '-n', key) for key in ['machdep.cpu.brand_string', 'hw.physicalcpu', 'hw.logicalcpu', 'hw.memsize']}
    (output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    rows = []
    for index, (case, backend, repeat) in enumerate(schedule):
        options = dict(CASES[case], backend=backend, workers=2)
        invocation = [binary] + [f'--{key}={value}' for key, value in options.items()]
        prefix = ['/usr/bin/time', '-l'] if platform.system() == 'Darwin' else ['/usr/bin/time', '-v']
        result = subprocess.run(prefix + invocation, cwd=ROOT, text=True, capture_output=True, timeout=90)
        stem = f'{index:03d}-{case}-{backend}-{repeat}'
        (output / 'logs' / f'{stem}.stdout').write_text(result.stdout)
        (output / 'logs' / f'{stem}.stderr').write_text(result.stderr)
        result.check_returncode()
        parsed = list(csv.DictReader(result.stdout.splitlines()))
        if len(parsed) != 1:
            raise RuntimeError(f'unexpected output: {stem}')
        row = dict(case=case, repeat=repeat, **parsed[0])
        if platform.system() == 'Darwin':
            rss = re.search(r'(\d+)\s+maximum resident set size', result.stderr)
            cpu = re.search(r'([\d.]+)\s+user\s+([\d.]+)\s+sys', result.stderr)
            row['process_peak_rss_bytes'] = int(rss[1]) if rss else ''
            row['process_cpu_s'] = sum(map(float, cpu.groups())) if cpu else ''
        else:
            rss = re.search(r'Maximum resident set size \(kbytes\):\s*(\d+)', result.stderr)
            user = re.search(r'User time \(seconds\):\s*([\d.]+)', result.stderr)
            system = re.search(r'System time \(seconds\):\s*([\d.]+)', result.stderr)
            row['process_peak_rss_bytes'] = int(rss[1]) * 1024 if rss else ''
            row['process_cpu_s'] = float(user[1]) + float(system[1]) if user and system else ''
        rows.append(row)
        with (output / 'raw.csv').open('w') as handle:
            writer = csv.DictWriter(handle, fieldnames=list(row))
            writer.writeheader()
            writer.writerows(rows)
        print(f'{index + 1}/{len(schedule)} {case} {backend} repeat={repeat}', flush=True)
    summary = []
    metrics = ['completed_per_s', 'completion_p50_ns', 'completion_p95_ns', 'completion_p99_ns', 'observed_p99_ns', 'queue_p99_ns', 'handler_p99_ns', 'enqueue_lag_p99_ns', 'process_peak_rss_bytes', 'process_cpu_s', 'allocations', 'allocated_bytes']
    for case in args.cases:
        for backend in args.backends:
            group = [row for row in rows if row['case'] == case and row['backend'] == backend]
            item = dict(case=case, backend=backend, samples=len(group))
            for metric in metrics:
                values = [float(row[metric]) for row in group if row[metric] != '']
                if values:
                    item[metric + '_median'] = statistics.median(values)
                    item[metric + '_min'] = min(values)
                    item[metric + '_max'] = max(values)
            summary.append(item)
    with (output / 'summary.csv').open('w') as handle:
        writer = csv.DictWriter(handle, fieldnames=list(summary[0]))
        writer.writeheader()
        writer.writerows(summary)


if __name__ == '__main__':
    main()
