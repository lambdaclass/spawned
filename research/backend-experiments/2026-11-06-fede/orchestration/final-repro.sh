#!/usr/bin/env bash
set -euo pipefail
ROOT=/tmp/spawned-fede-research-25dGGmPA
FINAL="$ROOT/final-2"
python3 "$ROOT/controller/verify_fede_overnight.py" --data "$FINAL/data" --report "$FINAL/report" > "$FINAL/verification.log"
"$ROOT/plots/bin/python" "$ROOT/controller/render_fede_overnight.py" --data "$FINAL/data" --output "$ROOT/final-reproduction" --findings "$ROOT/controller/FINDINGS.md" > "$FINAL/reproduction.log"
python3 - "$ROOT" <<'PY'
from pathlib import Path
import hashlib,json,sys
root=Path(sys.argv[1]);final=root/'final-2'
def hashes(folder):
 return {str(p.relative_to(folder)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(folder.rglob('*')) if p.is_file()}
a=hashes(final/'report');b=hashes(root/'final-reproduction')
assert a==b,[(k,a.get(k),b.get(k)) for k in sorted(set(a)|set(b)) if a.get(k)!=b.get(k)]
(final/'report-sha256.json').write_text(json.dumps(a,indent=2)+'\n')
(final/'analysis-source-sha256.json').write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in [root/'controller'/n for n in ['final-analysis.py','final-input-plan.json','collect_fede_overnight.py','render_fede_overnight.py','verify_fede_overnight.py','FINDINGS.md','final-repro.sh']]},indent=2)+'\n')
with (final/'reproduction.log').open('a') as f:f.write(f'PASS: byte-identical regeneration of {len(a)} report artifacts from the archived data.\n')
print('PASS: identical archived-data regeneration',len(a),flush=True)
PY
tar -czf "$ROOT/final-delivery.tgz" -C "$FINAL" data report verification.log reproduction.log report-sha256.json analysis-source-sha256.json
printf 'complete\n' > "$ROOT/final-delivery-state"
