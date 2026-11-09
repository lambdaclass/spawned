"""Archive probe evidence, compressing raw stdout losslessly and deterministically."""
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import sys

root, out = map(Path, sys.argv[1:])
assert (root / 'results/COMPLETE').exists()
assert not out.exists(), 'preserve earlier evidence'
out.mkdir(parents=True)
for name in ['builds.json', 'machine.json', 'run.log', 'baseline05-build.log', 'control06-build.log', 'metrics06-build.log']:
    shutil.copy2(root / name, out / name)
source_hashes = {}
for src in sorted((root / 'results').rglob('*')):
    if not src.is_file():
        continue
    dest = out / src.relative_to(root)
    dest.parent.mkdir(parents=True, exist_ok=True)
    data = src.read_bytes()
    if src.name == 'stdout.json':
        dest = dest.with_suffix('.json.gz')
        dest.write_bytes(gzip.compress(data, mtime=0))
        assert gzip.decompress(dest.read_bytes()) == data
        source_hashes[str(dest.relative_to(out))] = hashlib.sha256(data).hexdigest()
    else:
        dest.write_bytes(data)
(out / 'uncompressed-stdout-sha256.json').write_text(json.dumps(source_hashes, indent=2) + '\n')
print('Archived', len(source_hashes), 'raw process outputs, losslessly compressed.')
