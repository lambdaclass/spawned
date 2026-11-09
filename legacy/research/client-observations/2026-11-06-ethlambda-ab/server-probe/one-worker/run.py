"""Reuse the original bounded controller, re-scoring old and fixed runtimes."""
from pathlib import Path

# Install the unchanged parent run.py at this path on fede first.
source = Path('/tmp/spawned-pr-security-review/engine-probe-run.py').read_text()
source = source.replace("root = base / 'engine-actor-probe'", "root = base / 'engine-one-worker-probe'")
old = "profiles = [('baseline05', 'baseline05', 0), ('control06', 'control06', 0),\n            ('metrics06', 'metrics06', 1), ('counters06', 'metrics06', 0)]"
new = "profiles = [('control06', 'control06', 0), ('fixed06', 'fixed06', 0),\n            ('metrics06', 'metrics06', 1), ('fixedmetrics06', 'fixedmetrics06', 1)]"
assert old in source
source = source.replace(old, new).replace("{'metrics06', 'counters06'}", "{'metrics06', 'fixedmetrics06'}")
assert "baseline05" not in source and "counters06" not in source
exec(compile(source, 'one-worker-controller', 'exec'))
