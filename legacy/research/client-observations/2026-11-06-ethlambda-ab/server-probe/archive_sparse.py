"""Archive the longer sparse comparison without changing existing evidence."""
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import sys

root, out = map(Path, sys.argv[1:])
assert (root / 'results/COMPLETE').exists()
assert not out.exists()
out.mkdir(parents=True)
for name in ['builds.json', 'run.log']:
    shutil.copy2(root / name, out / name)
hashes = {}
for p in sorted((root / 'results').rglob('*')):
    if not p.is_file():
        continue
    target = out / p.relative_to(root)
    target.parent.mkdir(parents=True, exist_ok=True)
    data = p.read_bytes()
    if p.name == 'stdout.json':
        target = target.with_suffix('.json.gz')
        target.write_bytes(gzip.compress(data, mtime=0))
        assert gzip.decompress(target.read_bytes()) == data
        hashes[str(target.relative_to(out))] = hashlib.sha256(data).hexdigest()
    else:
        target.write_bytes(data)
(out / 'uncompressed-stdout-sha256.json').write_text(json.dumps(hashes, indent=2) + '\n')
shutil.copy2(root.parent / 'engine-sparse-controller-preflight.log', out / 'controller-preflight.log')
print('Archived', len(hashes), 'longer sparse process outputs')
