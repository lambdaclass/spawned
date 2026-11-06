#!/usr/bin/env python3
"""Bounded lifecycle/resource runs, with exact source and executable provenance."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import random
import subprocess
import time
from fede_research import ROOT, SELECTORS, capture, process, write_rows


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--seconds',type=int,default=600)
    p.add_argument('--smoke',action='store_true')
    p.add_argument('--stop-at',type=float,required=True)
    a=p.parse_args();assert 1<=a.seconds<=1800
    out=a.output.resolve();out.mkdir(parents=True,exist_ok=False)
    for name in ['logs','series']:(out/name).mkdir()
    assert os.sched_getaffinity(0)=={2,3,4,5}
    meta=dict(started_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),revision=capture('git','rev-parse','HEAD'),dirty=bool(capture('git','status','--porcelain')),rustc=capture('rustc','--version'),cargo=capture('cargo','--version'),cpu=capture('lscpu'),load_before=Path('/proc/loadavg').read_text(),actor_cpus='2,3,4,5',driver_cpus='6,7,14,15',sampler_cpus='8,9,16,17',nice=os.nice(0),seconds=a.seconds,smoke=a.smoke,controller_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),schedule=[],environment={k:os.environ.get(k) for k in ['RUSTFLAGS','CARGO_PROFILE_BENCH_DEBUG','CARGO_BUILD_JOBS']})
    assert not meta['dirty']
    result=subprocess.run(['cargo','bench','--locked','-p','spawned-concurrency','--features','backend-experiments,profiling-experiments','--bench','lifecycle_soak','--no-run','--message-format=json'],cwd=ROOT,capture_output=True,text=True)
    (out/'build.log').write_text(result.stderr);result.check_returncode()
    executable=next(json.loads(line)['executable'] for line in result.stdout.splitlines() if json.loads(line).get('reason')=='compiler-artifact' and json.loads(line).get('executable'))
    meta['binary_sha256']=hashlib.sha256(Path(executable).read_bytes()).hexdigest()
    specs=[('async',4,'',0),('tokio-16',4,'tokio-batch',16),('tokio-pool',4,'tokio-pool',0),('sharded-16',4,'sharded',16),('native-global',3,'native-global',16),('native-local',3,'native-local',16),('native-steal',3,'native-steal',16),('native-steal',4,'native-steal',16)]
    random.Random(189).shuffle(specs);rows=[]
    def save():
        (out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
        if rows:write_rows(out/'raw.csv',rows)
    save()
    for variant,workers,scheduler,batch in specs:
        # Leave enough time for full requested duration, final drain and watchdog.
        if time.time()+a.seconds+60>a.stop_at:meta['stopped_at_deadline']=True;break
        stem=f'{len(rows):02d}-{variant}-n{workers}';env=dict(os.environ,LC_ALL='C')
        for key in SELECTORS:env.pop(key,None)
        if scheduler:env.update(SPAWNED_EXPERIMENT_SCHEDULER=scheduler,SPAWNED_EXPERIMENT_WORKERS=str(workers),SPAWNED_EXPERIMENT_BATCH=str(batch))
        env.update(SPAWNED_EXPERIMENT_REACTORS='1',SPAWNED_EXPERIMENT_LOCAL_BUDGET='3',SPAWNED_EXPERIMENT_SLICE_US='50')
        config=dict(seconds=a.seconds,workers=workers,idle=32 if a.smoke else 1024,churn=16 if a.smoke else 128,steps=4 if a.smoke else 16,series=str(out/'series'/f'{stem}.csv'),**{'driver-cpus':meta['driver_cpus'],'sampler-cpus':meta['sampler_cpus']})
        meta['schedule'].append(dict(variant=variant,actor_workers=workers,total_runtime_workers=workers+(1 if variant.startswith('native') else 0),config=config));save()
        start=time.monotonic()
        status,stdout,stderr=process(['taskset','-c',meta['actor_cpus'],'/usr/bin/time','-v',executable]+[f'--{k}={v}' for k,v in config.items()],env,out/'logs'/stem,a.seconds+60)
        row=dict(variant=variant,workers=workers,total_runtime_workers=workers+(1 if variant.startswith('native') else 0),status=status,stem=stem,wall_s=time.monotonic()-start,config_json=json.dumps(config,sort_keys=True))
        if status=='ok':
            parsed=list(csv.DictReader(stdout.splitlines()));assert len(parsed)==1 and None not in parsed[0]
            row.update(parsed[0]);assert int(row['started'])==int(row['stopped'])==int(row['dropped'])
            assert int(row['cycles'])>0
        rows.append(row);save();print(f'{len(rows)} {variant} actorN{workers}: {status}',flush=True)
        if status!='ok':raise RuntimeError('soak failed; preserve diagnostics and investigate')
    meta['finished_utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime());save()

if __name__=='__main__':main()
