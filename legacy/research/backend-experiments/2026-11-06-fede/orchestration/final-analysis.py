#!/usr/bin/env python3
"""Collect only the fully validated overnight plan after all scored work ends."""
import csv
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile

ROOT=Path('/tmp/spawned-fede-research-25dGGmPA')
OUT=ROOT/'final-2'
TOOLS=ROOT/'controller'
assert (ROOT/'amortized-v3/run-state').read_text().strip()=='complete'
assert (ROOT/'runnable-v4/run-state').read_text().strip()=='complete-with-scale-deferred'
assert (ROOT/'fairness-v8/run-state').read_text().strip()=='complete'
assert (ROOT/'analysis-slot-10/fairness-state').read_text().strip()=='complete'
assert (ROOT/'analysis-slot-11/pinned-state').read_text().strip()=='complete'
assert (ROOT/'pinned-v9/run-state').read_text().strip()=='complete'
assert (ROOT/'analysis-slot-12/pinned-state').read_text().strip()=='complete'
for proc in Path('/proc').glob('[0-9]*/exe'):
    try: exe=proc.resolve()
    except OSError: continue
    assert not (str(exe).startswith(str(ROOT)) and exe.name.startswith(
        ('workload_research-','lifecycle_soak-','designs-','pinned_pipeline-'))),exe
OUT.mkdir(exist_ok=False)
plan=json.loads((TOOLS/'final-input-plan.json').read_text())
assert len(plan)==45 and sum(x['expected_samples'] for x in plan)==5744
for entry in plan:
    path=ROOT/entry['directory']
    meta=json.loads((path/'metadata.json').read_text())
    with (path/'raw.csv').open() as f: raw=list(csv.DictReader(f))
    assert meta.get('finished_utc') and not meta.get('stopped_at_deadline') and not meta['dirty'],entry
    assert len(raw)==entry['expected_samples'] and all(r['status']=='ok' for r in raw),entry
# Evidence is intentionally separate from completed, scored process rows.
evidence=OUT/'evidence'
evidence.mkdir()
with tarfile.open(evidence/'validation-and-orchestration.tar.gz','w:gz') as archive:
    for name in ['analysis-slot-3','analysis-slot-4','analysis-slot-5','analysis-slot-6',
                 'analysis-slot-7','analysis-slot-9','analysis-slot-10','analysis-slot-11','analysis-slot-12']:
        directory=ROOT/name
        if directory.exists():
            for path in sorted(directory.rglob('*')):
                if path.is_file() and not {'preview-data','preview-report'}.intersection(path.parts):
                    archive.add(path,arcname=str(path.relative_to(ROOT)),recursive=False)
    for path in sorted(TOOLS.iterdir()):
        if path.is_file() and path.suffix in ['.sh','.py','.json','.txt','.log']:
            archive.add(path,arcname='controller/'+path.name,recursive=False)
    for directory in [ROOT/'amortized-v3/soaks-1',ROOT/'amortized-v3/soaks-2',
                      ROOT/'runnable-v4-preflight-9a']:
        if directory.exists():
            archive.add(directory,arcname=str(directory.relative_to(ROOT)))
    for path in [ROOT/'runnable-v4/deferred-scale.json',ROOT/'deque-v5/run-state',
                 ROOT/'frontier-v7/run-state',ROOT/'cpu-cache-topology.json',ROOT/'env.sh']:
        if path.exists():archive.add(path,arcname=str(path.relative_to(ROOT)))
    for name in ['coalesced-publication-before.log','checks-post-publication-fixed.log',
                 'checked-source.txt','final-behavior.log','final-clippy.log']:
        path=ROOT/'controls'/name
        if path.exists():archive.add(path,arcname='controls/'+name)
command=['python3',str(TOOLS/'collect_fede_overnight.py')]
for entry in plan:command+=['--run',entry['name']+'='+str(ROOT/entry['directory'])]
for bundle in sorted((TOOLS/'source-bundles').glob('*.bundle')):
    command+=['--bundle',str(bundle)]
assert list((TOOLS/'source-bundles').glob('*fairness-measured-source.bundle'))
assert list((TOOLS/'source-bundles').glob('*pinned-measured-source.bundle'))
for path in sorted((TOOLS/'review-evidence').iterdir()):
    if path.is_file():command+=['--evidence',str(path)]
for path in sorted(evidence.iterdir()):command+=['--evidence',str(path)]
command+=['--output',str(OUT/'data')]
subprocess.run(command,check=True)
subprocess.run([str(ROOT/'plots/bin/python'),str(TOOLS/'render_fede_overnight.py'),
                '--data',str(OUT/'data'),'--output',str(OUT/'report'),'--findings',str(TOOLS/'FINDINGS.md')],check=True)
(OUT/'analysis-source-sha256.json').write_text(json.dumps({
    p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in
    [Path(__file__),TOOLS/'collect_fede_overnight.py',TOOLS/'render_fede_overnight.py',
     TOOLS/'FINDINGS.md',TOOLS/'final-input-plan.json']},indent=2)+'\n')
print('FULL PLAN COLLECTED AND RENDERED',flush=True)
