"""Build all four runtime/metrics conditions with the same concurrent probe."""
from pathlib import Path

# Reuse the bounded, source-checked worker-fix builder archived alongside this.
source = Path('/tmp/spawned-pr-security-review/engine-one-worker-build.py').read_text()
source = source.replace("root = base / 'engine-one-worker-probe'", "root = base / 'engine-concurrent-probe'")
source = source.replace("base / 'engine_actor_probe.rs'", "base / 'engine_concurrent_probe.rs'")
source = source.replace("if p['name'] != 'spawned-rt']", "if not (p['name'] == 'spawned-rt' and p['version'] == '0.6.0')]")
old = "for label, features in [('fixed06', []), ('fixedmetrics06', ['--features', 'probe-metrics'])]:"
new = "for label, features in [('fixed06', []), ('fixedmetrics06', ['--features', 'probe-metrics']), ('control06', []), ('metrics06', ['--features', 'probe-metrics'])]:"
assert old in source
source = source.replace(old, new)
marker = "    command = [str(prefix / 'bin/cargo'), 'build'"
assert marker in source
switch = '''    # Change only the runtime source; the actor/Engine-client probe is identical.
    patch = f'\\n[patch.crates-io]\\nspawned-rt = {{ path = "{runtime}" }}\\n'
    contents = manifest.read_text().replace(patch, '')
    if label.startswith('fixed'):
        contents += patch
    manifest.write_text(contents)
    previous = tomllib.loads((source / 'Cargo.lock').read_text())
    with (root / (label + '-metadata.json')).open('w') as f:
        subprocess.run([str(prefix / 'bin/cargo'), 'metadata', '--offline', '--filter-platform', 'x86_64-unknown-linux-gnu', '--format-version', '1'], cwd=source, env=env, stdout=f, check=True, timeout=120)
    current = tomllib.loads((source / 'Cargo.lock').read_text())
    assert normalize(previous) == normalize(current), 'unexpected dependency graph change'
    rt_node = next(p for p in current['package'] if p['name'] == 'spawned-rt' and p['version'] == '0.6.0')
    assert (rt_node.get('source') is None) == label.startswith('fixed'), 'wrong runtime source'
'''
source = source.replace(marker, switch + marker)
source = source.replace("records.append({'variant': label,", "records.append({'compiled_runtime_kind': 'fixed' if label.startswith('fixed') else 'published', 'variant': label,")
assert '# Re-score' in source
source = source.split('# Re-score', 1)[0]
exec(compile(source, 'concurrent-probe-build', 'exec'))
