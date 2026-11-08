"""Build isolated Engine-client probe examples from the existing fede sources."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time
import tomllib

base = Path('/tmp/spawned-pr-security-review')
out = base / 'engine-actor-probe'
out.mkdir(exist_ok=True)
bin_dir = out / 'bin'
bin_dir.mkdir(exist_ok=True)
os.sched_setaffinity(0, {2, 3, 4, 5})
os.nice(max(0, 10 - os.getpriority(os.PRIO_PROCESS, 0)))
prefix = base / 'ethlambda-control-toolchain/installed'
env = os.environ.copy()
for key in list(env):
    if key.startswith(('SPAWNED_', 'CARGO_PROFILE_RELEASE_')) or key in {
        'RUSTFLAGS', 'RUSTDOCFLAGS', 'RUSTC_WRAPPER', 'TOKIO_WORKER_THREADS'}:
        env.pop(key, None)
env.update(PATH=str(prefix / 'bin') + ':' + env['PATH'],
           RUSTC=str(prefix / 'bin/rustc'), RUSTDOC=str(prefix / 'bin/rustdoc'),
           CARGO_TARGET_DIR=str(base / 'ethlambda-control-build/target'),
           CARGO_BUILD_JOBS='2', LIBCLANG_PATH='/usr/lib/llvm-18/lib')
records = []
for variant, original, commit in [
    ('baseline05', 'ethlambda-spawned-baseline', '4117c5aef5ca3b916c3d871cea042805b57a4e88'),
    ('control06', 'ethlambda-spawned-control', 'c585f3d2b8e816e24e7029e418a484ee3396c30a'),
]:
    source = out / variant
    if not source.exists():
        subprocess.run(['git', 'worktree', 'add', '--detach', str(source), commit], cwd=base / original, check=True)
    crate = source / 'crates/net/engine'
    (crate / 'examples').mkdir(exist_ok=True)
    shutil.copy2(base / 'engine_actor_probe.rs', crate / 'examples/engine_actor_probe.rs')
    manifest = crate / 'Cargo.toml'
    contents = manifest.read_text()
    if 'spawned-concurrency.workspace' not in contents:
        contents = contents.replace('[dev-dependencies]', '[dev-dependencies]\nspawned-concurrency.workspace = true')
    if variant == 'control06' and 'probe-metrics' not in contents:
        contents += '\n[features]\nprobe-metrics = ["spawned-concurrency/metrics"]\n'
    manifest.write_text(contents)
    before = tomllib.loads((source / 'Cargo.lock').read_text())
    if variant == 'control06':
        graph_cmd = [str(prefix / 'bin/cargo'), 'metadata', '--offline', '--filter-platform', 'x86_64-unknown-linux-gnu', '--format-version', '1']
    else:
        graph_cmd = [str(prefix / 'bin/cargo'), 'metadata', '--offline', '--filter-platform', 'x86_64-unknown-linux-gnu', '--format-version', '1']
    with (out / (variant + '-metadata.json')).open('w') as f:
        subprocess.run(graph_cmd, cwd=source, env=env, stdout=f, check=True, timeout=120)
    after = tomllib.loads((source / 'Cargo.lock').read_text())
    normalize = lambda lock: [p for p in lock['package'] if p['name'] != 'ethlambda-engine']
    assert normalize(before) == normalize(after), 'probe must not change dependency versions or unrelated edges'
    variants = [(variant, [])]
    if variant == 'control06':
        variants.append(('metrics06', ['--features', 'probe-metrics']))
    for label, features in variants:
        command = [str(prefix / 'bin/cargo'), 'build', '--offline', '--locked', '--release',
                   '-p', 'ethlambda-engine', '--example', 'engine_actor_probe', *features]
        started = time.monotonic()
        peak_rss = 0
        failure = None
        with (out / (label + '-build.log')).open('w') as log:
            child = subprocess.Popen(command, cwd=source, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                while child.poll() is None:
                    rss = threads = 0
                    for proc in Path('/proc').glob('[0-9]*/stat'):
                        try:
                            fields = proc.read_text().rsplit(')', 1)[1].split()
                            if int(fields[2]) != child.pid:
                                continue
                            status = {x.split(':', 1)[0]: x.split(':', 1)[1].strip() for x in (proc.parent / 'status').read_text().splitlines() if ':' in x}
                            rss += int(status.get('VmRSS', '0').split()[0]) * 1024
                            threads += int(status.get('Threads', '0'))
                        except (FileNotFoundError, PermissionError, ProcessLookupError):
                            pass
                    peak_rss = max(peak_rss, rss)
                    if rss > 16 * 1024**3 or threads > 128 or time.monotonic() - started > 1800:
                        failure = 'build resource/time budget exceeded'
                        os.killpg(child.pid, signal.SIGKILL)
                        break
                    time.sleep(.5)
                code = child.wait()
            finally:
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
        assert code == 0 and failure is None, (label, code, failure)
        artifact = bin_dir / label
        shutil.copy2(Path(env['CARGO_TARGET_DIR']) / 'release/examples/engine_actor_probe', artifact)
        record = {'variant': label, 'source_commit': commit, 'command': command,
                  'sha256': hashlib.sha256(artifact.read_bytes()).hexdigest(),
                  'lock_sha256': hashlib.sha256((source / 'Cargo.lock').read_bytes()).hexdigest(),
                  'engine_client_sha256': hashlib.sha256((crate / 'src/client.rs').read_bytes()).hexdigest(),
                  'rustc': subprocess.check_output([str(prefix / 'bin/rustc'), '-Vv'], text=True),
                  'elapsed_s': time.monotonic() - started, 'sampled_peak_rss_bytes': peak_rss,
                  'returncode': code, 'source_diff': subprocess.check_output(['git', 'diff', '--', 'Cargo.lock', 'crates/net/engine/Cargo.toml'], cwd=source, text=True)}
        records.append(record)
        (out / 'builds.json').write_text(json.dumps(records, indent=2) + '\n')
        print('Built', label, record['sha256'], flush=True)
