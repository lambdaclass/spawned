"""Six longer sparse processes per runtime/metrics condition."""
from pathlib import Path

source = Path('/tmp/spawned-pr-security-review/engine-probe-run.py').read_text()
source = source.replace("root = base / 'engine-actor-probe'", "root = base / 'engine-sparse-probe'")
old = "profiles = [('baseline05', 'baseline05', 0), ('control06', 'control06', 0),\n            ('metrics06', 'metrics06', 1), ('counters06', 'metrics06', 0)]"
new = "profiles = [('control06', 'control06', 0), ('fixed06', 'fixed06', 0),\n            ('metrics06', 'metrics06', 1), ('fixedmetrics06', 'fixedmetrics06', 1)]"
assert old in source
source = source.replace(old, new).replace("{'metrics06', 'counters06'}", "{'metrics06', 'fixedmetrics06'}")
start = source.index('for repeat in range(3):')
end = source.index('random.Random', start)
source = source[:start] + '''for repeat in range(6):
    for scenario, load, concurrency, gap_ms in [
        ('immediate', 'idle', 1, 50),
    ]:
        for label, binary, sample in profiles:
            cells.append(dict(variant=label, binary=binary, sample=sample, repeat=repeat,
                scenario=scenario, placement='thread', load=load, seconds=10, warmup=8,
                concurrency=concurrency, gap_ms=gap_ms))
''' + source[end:]
old = "str(cell['seconds']), str(cell['warmup']), str(cell['sample'])]"
new = "str(cell['seconds']), str(cell['warmup']), str(cell['sample']), str(cell['concurrency']), str(cell['gap_ms'])]"
assert old in source
source = source.replace(old, new)
source = source.replace("    assert len(data['samples_ns']) == data['requests']", "    assert data['concurrency'] == cell['concurrency'] and data['gap_ms'] == cell['gap_ms']\n    assert len(data['samples_ns']) == data['requests']")
exec(compile(source, 'concurrent-probe-controller', 'exec'))
