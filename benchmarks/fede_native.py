#!/usr/bin/env python3
"""Matched native executor research. Always opt in; run stages separately."""
import argparse
import hashlib
import json
from pathlib import Path
import random
import time
from fede_research import Campaign, VARIANTS

VARIANTS.update({'native-global': ('native-global',16),
                 'native-local': ('native-local',16), 'tokio-pool': ('tokio-pool',0)})

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--stage', choices=['smoke','graphs','offered','profiles'], required=True)
    p.add_argument('--actor-cpus',default='2,3,4,5')
    p.add_argument('--driver-cpus',default='6,7,14,15')
    p.add_argument('--collector-cpus',default='8,9,16,17')
    p.add_argument('--stop-at',type=float,required=True,help='absolute Unix deadline, no new processes afterward')
    a=p.parse_args()
    a.workers=[3,4,5];a.variants=['async','tokio-16','tokio-pool','sharded-16','native-global','native-local']
    a.seed=187;a.duration=2.;a.timeout=120.;a.smoke=a.stage=='smoke'
    c=Campaign(a)
    assert not c.meta['dirty'], 'commit experimental source before measuring'
    c.meta['controller_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    c.meta['budget_note']='Native: B-1 actor workers + one shared I/O worker. Others: B actor/runtime workers. All share the same four physical actor CPUs. Idle owner/main/helper/blocking/server threads are additional and recorded separately. B=5 is five runtime threads on four cores, not five cores. Compare native4+R1 to Tokio4 as equal actor-worker control and Tokio5 as equal total-runtime-thread control; graph actor population stays1920.'
    jobs=[]
    for budget in [3,4,5]:
        for variant in a.variants:
            workers=budget-1 if variant.startswith('native') else budget
            exp={'SPAWNED_EXPERIMENT_REACTORS':1,'SPAWNED_EXPERIMENT_LOCAL_BUDGET':3}
            if a.stage in ['smoke','graphs']:
                for topology in ['chain','fanout']:
                    for layout in ['same','cross']:
                        for repeat in range(1 if a.smoke else 5):
                            jobs.append((variant,workers,dict(mode='graph',topology=topology,layout=layout,actors=1920,width=8,producers=256,messages=4096 if a.smoke else 1_048_576,iterations=100),repeat,exp,budget))
            elif a.stage=='offered':
                for layout in ['spread','concentrated']:
                    for rate in [500_000,900_000,1_200_000]:
                        for repeat in range(3):
                            jobs.append((variant,workers,dict(mode='offered',layout=layout,actors=240,producers=4,messages=rate*2,rate=rate,iterations=100,**{'hot-iterations':5000}),repeat,exp,budget))
            elif a.stage=='profiles' and budget==4:
                for layout in ['same','cross']:
                    jobs.append((variant,workers,dict(mode='graph',topology='chain',layout=layout,actors=1920,width=8,producers=256,messages=1_048_576,iterations=100),0,exp,budget))
    random.Random(a.seed).shuffle(jobs)
    c.meta['planned_samples']=len(jobs);c.save()
    for variant,workers,cfg,repeat,exp,budget in jobs:
        if time.time()>=a.stop_at:
            c.meta['stopped_at_deadline']=True;break
        row=c.sample('profile' if a.stage=='profiles' else 'native-'+a.stage,variant,workers,cfg,repeat,experiment=exp)
        row['total_runtime_workers']=budget
        row['reactor_workers']=1 if variant.startswith('native') else 0
        row['actor_workers']=workers
        c.save()
    c.meta['finished_utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime());c.save()

if __name__=='__main__':main()
