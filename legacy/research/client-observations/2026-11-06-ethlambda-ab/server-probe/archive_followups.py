"""Preserve both follow-up campaigns with lossless compressed raw samples."""
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import sys

base, out = map(Path, sys.argv[1:])
assert not out.exists(), 'preserve previous archives'
for campaign, folder in [('one-worker', 'engine-one-worker-probe'), ('concurrent', 'engine-concurrent-probe')]:
    root = base / folder
    assert (root / 'results/COMPLETE').exists()
    dest = out / campaign
    dest.mkdir(parents=True)
    for name in ['builds.json', 'run.log', 'runtime.patch']:
        shutil.copy2(root / name, dest / name)
    for p in root.glob('*-build.log'):
        shutil.copy2(p, dest / p.name)
    hashes = {}
    for src in sorted((root / 'results').rglob('*')):
        if not src.is_file():
            continue
        target = dest / src.relative_to(root)
        target.parent.mkdir(parents=True, exist_ok=True)
        data = src.read_bytes()
        if src.name == 'stdout.json':
            target = target.with_suffix('.json.gz')
            target.write_bytes(gzip.compress(data, mtime=0))
            assert gzip.decompress(target.read_bytes()) == data
            hashes[str(target.relative_to(dest))] = hashlib.sha256(data).hexdigest()
        else:
            target.write_bytes(data)
    (dest / 'uncompressed-stdout-sha256.json').write_text(json.dumps(hashes, indent=2) + '\n')
    print(campaign, len(hashes), 'raw outputs verified and compressed')
shutil.copy2(base / 'engine-concurrent-build-preflight.log', out / 'concurrent/build-preflight.log')
