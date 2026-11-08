"""Compare the published runtime with only the reviewed one-worker fix applied."""
import difflib
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
root = base / 'engine-one-worker-probe'
root.mkdir(exist_ok=True)
(root / 'bin').mkdir(exist_ok=True)
old = base / 'engine-actor-probe'
os.sched_setaffinity(0, {2, 3, 4, 5})
os.nice(max(0, 10 - os.getpriority(os.PRIO_PROCESS, 0)))
published = next((Path.home() / '.cargo/registry/src').glob('*/spawned-rt-0.6.0'))
runtime = root / 'spawned-rt'
if not runtime.exists():
    shutil.copytree(published, runtime)
original = (published / 'src/threads/mod.rs').read_text()
fixed = (base / 'engine-probe-fixed-runtime.rs').read_text()
(runtime / 'src/threads/mod.rs').write_text(fixed)
assert fixed.count('.worker_threads(1)') >= 2
assert original != fixed
(root / 'runtime.patch').write_text(''.join(difflib.unified_diff(original.splitlines(True), fixed.splitlines(True), fromfile='published/src/threads/mod.rs', tofile='fixed/src/threads/mod.rs')))
for p in published.rglob('*'):
    if p.is_file() and p.relative_to(published) != Path('src/threads/mod.rs'):
        assert p.read_bytes() == (runtime / p.relative_to(published)).read_bytes()
source = root / 'source'
if not source.exists():
    subprocess.run(['git', 'worktree', 'add', '--detach', str(source), 'c585f3d2b8e816e24e7029e418a484ee3396c30a'], cwd=base / 'ethlambda-spawned-control', check=True)
crate = source / 'crates/net/engine'
(crate / 'examples').mkdir(exist_ok=True)
shutil.copy2(base / 'engine_actor_probe.rs', crate / 'examples/engine_actor_probe.rs')
shutil.copy2(old / 'control06/crates/net/engine/Cargo.toml', crate / 'Cargo.toml')
manifest = source / 'Cargo.toml'
text = manifest.read_text()
if '[patch.crates-io]' not in text:
    text += f'\n[patch.crates-io]\nspawned-rt = {{ path = "{runtime}" }}\n'
manifest.write_text(text)
shutil.copy2(old / 'control06/Cargo.lock', source / 'Cargo.lock')
before = tomllib.loads((source / 'Cargo.lock').read_text())
prefix = base / 'ethlambda-control-toolchain/installed'
env = os.environ.copy()
for key in list(env):
    if key.startswith(('SPAWNED_', 'CARGO_PROFILE_RELEASE_')) or key in {'RUSTFLAGS', 'RUSTDOCFLAGS', 'RUSTC_WRAPPER', 'TOKIO_WORKER_THREADS'}:
        env.pop(key, None)
env.update(PATH=str(prefix / 'bin') + ':' + env['PATH'], RUSTC=str(prefix / 'bin/rustc'),
           RUSTDOC=str(prefix / 'bin/rustdoc'), CARGO_TARGET_DIR=str(base / 'ethlambda-control-build/target'),
           CARGO_BUILD_JOBS='2', LIBCLANG_PATH='/usr/lib/llvm-18/lib')
with (root / 'cargo-metadata.json').open('w') as f:
    subprocess.run([str(prefix / 'bin/cargo'), 'metadata', '--offline', '--filter-platform', 'x86_64-unknown-linux-gnu', '--format-version', '1'], cwd=source, env=env, stdout=f, check=True, timeout=120)
after = tomllib.loads((source / 'Cargo.lock').read_text())
normalize = lambda lock: [p for p in lock['package'] if p['name'] != 'spawned-rt']
assert normalize(before) == normalize(after), 'only the runtime source may change'
records = []
for label, features in [('fixed06', []), ('fixedmetrics06', ['--features', 'probe-metrics'])]:
    command = [str(prefix / 'bin/cargo'), 'build', '--offline', '--locked', '--release', '-p', 'ethlambda-engine', '--example', 'engine_actor_probe', *features]
    began = time.monotonic()
    peak_rss = 0
    with (root / (label + '-build.log')).open('w') as log:
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
                assert rss < 16 * 1024**3 and threads < 128 and time.monotonic() - began < 1800, 'build budget exceeded'
                time.sleep(.5)
            assert child.wait() == 0, label
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
    binary = root / 'bin' / label
    shutil.copy2(Path(env['CARGO_TARGET_DIR']) / 'release/examples/engine_actor_probe', binary)
    records.append({'variant': label, 'artifact_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'runtime_original_sha256': hashlib.sha256(original.encode()).hexdigest(), 'runtime_fixed_sha256': hashlib.sha256(fixed.encode()).hexdigest(),
        'engine_client_sha256': hashlib.sha256((crate / 'src/client.rs').read_bytes()).hexdigest(),
        'probe_sha256': hashlib.sha256((crate / 'examples/engine_actor_probe.rs').read_bytes()).hexdigest(),
        'command': command, 'rustc': subprocess.check_output([str(prefix / 'bin/rustc'), '-Vv'], text=True),
        'elapsed_s': time.monotonic() - began, 'sampled_peak_rss_bytes': peak_rss,
        'lock_diff': subprocess.check_output(['git', 'diff', '--', 'Cargo.lock', 'Cargo.toml', 'crates/net/engine/Cargo.toml'], cwd=source, text=True)})
    (root / 'builds.json').write_text(json.dumps(records, indent=2) + '\n')
    print('Built', label, records[-1]['artifact_sha256'], flush=True)
# Re-score the exact earlier binaries; do not compare different run windows only.
for label in ['control06', 'metrics06']:
    shutil.copy2(old / 'bin' / label, root / 'bin' / label)
    records.append(next(r for r in json.loads((old / 'builds.json').read_text()) if r['variant'] == label))
(root / 'builds.json').write_text(json.dumps(records, indent=2) + '\n')
