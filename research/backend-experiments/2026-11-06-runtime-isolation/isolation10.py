#!/usr/bin/env python3
"""Frozen, closed-loop shared-vs-split runtime comparison; no open-loop SLO claim."""
from pathlib import Path
import csv,hashlib,itertools,json,os,random,subprocess,sys,time
ROOT=Path('/tmp/spawned-fede-research-25dGGmPA');SOURCE=ROOT/'isolation-source'
sys.path.insert(0,str(SOURCE/'benchmarks'))
from fede_research import process,write_rows
revision='a3cb5eb15d68f2e21a5d86eb19c84686e747547c'
assert os.nice(0)==10 and os.sched_getaffinity(0)=={2,3,4,5}
assert subprocess.check_output(['git','rev-parse','HEAD'],cwd=SOURCE,text=True).strip()==revision
assert not subprocess.check_output(['git','status','--porcelain'],cwd=SOURCE,text=True).strip()
artifacts=[json.loads(s) for s in (ROOT/'isolation-v10/build.jsonl').read_text().splitlines()]
exe=next(x['executable'] for x in artifacts if x.get('reason')=='compiler-artifact' and x['target']['name']=='runtime_isolation' and x.get('executable'))
out=ROOT/'isolation-v10/results';out.mkdir(exist_ok=False);(out/'logs').mkdir()
meta=dict(started_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),revision=revision,dirty=False,
          binary_sha256=hashlib.sha256(Path(exe).read_bytes()).hexdigest(),controller_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),actor_cpus='2,3,4,5',driver_cpus='10,11,12,13',nice=os.nice(0),planned_samples=32,schedule=[],
          purpose='Two-runtime isolation, same4actorworkers/cores. Shared4 vs critical1/general3. Direct versus general-router -> isolated-critical -> router typed reply. Idle or16backgroundactors/128closedloopproducers/50000LCG. Critical800LCG/oneclosedloopproducer.2s active plus bounded drain,3freshprocesses per full cell;8 independent100ms smokes. Actual actor affinity/IDs/LCG/message counts/18lifecycles asserted. Different achieved traffic; not equivalent offered load or an open-loop SLO. ProcessCPU includes4driverworkers. Normal Tokio scheduling in both; no new backend/API.')
rows=[]
def save():
 (out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
 if rows:write_rows(out/'raw.csv',rows)
save()
for smoke in [True,False]:
 jobs=list(itertools.product(['shared','split'],['direct','relay'],['idle','busy'],range(1 if smoke else 3)))
 random.Random(9185 if smoke else 9186).shuffle(jobs)
 for layout,path,load,repeat in jobs:
  if time.time()+20>=1793975400: # 2026-11-06 14:30:00 UTC (checked separately before launch)
   meta['stopped_at_deadline']=True;save();raise RuntimeError('hard new-case deadline')
  cfg=dict(layout=layout,path=path,load=load,seconds=.1 if smoke else 2)
  env={k:v for k,v in os.environ.items() if not k.startswith(('SPAWNED_EXPERIMENT_','SPAWNED_PROFILE_')) and k!='TOKIO_WORKER_THREADS'}
  stem=f'{len(rows):02d}-{layout}-{path}-{load}-r{repeat}';meta['schedule'].append(dict(config=cfg,repeat=repeat,smoke=smoke));save()
  status,stdout,stderr=process(['/usr/bin/time','-v',exe]+[f'--{k}={v}' for k,v in cfg.items()],env,out/'logs'/stem,20)
  row=dict(phase='smoke' if smoke else 'isolation',repeat=repeat,status=status,stem=stem,config_json=json.dumps(cfg,sort_keys=True))
  if status=='ok':
   parsed=list(csv.DictReader(stdout.splitlines()));assert len(parsed)==1 and None not in parsed[0]
   row.update(parsed[0]);assert int(row['started'])==int(row['stopped'])==int(row['dropped'])==18 and int(row['critical_completed'])>0
   assert (int(row['bg_completed'])>0)==(load=='busy')
  rows.append(row);save();print(f'{len(rows)}/32 {stem} {status}',flush=True)
  if status!='ok':raise RuntimeError('failed case retained; no further scoring')
assert len(rows)==32
meta['finished_utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime());save()
print('complete',flush=True)
