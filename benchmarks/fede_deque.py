#!/usr/bin/env python3
"""Owner-deque/Runnable comparison; new source, bounded overnight schedule."""
import argparse
import hashlib
from pathlib import Path
import random
import time
from fede_research import Campaign, VARIANTS
VARIANTS.update({v:(v,16) for v in ['native-steal','native-root','native-runnable','native-deque']})
VARIANTS['tokio-pool']=('tokio-pool',0)


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--stage',choices=['smoke','scale-smoke','graphs','scale','profiles','offered'],required=True)
    p.add_argument('--stop-at',type=float,required=True)
    a=p.parse_args();a.seed=197;a.duration=2.;a.timeout=120.;a.smoke='smoke' in a.stage
    scale=a.stage in ['scale','scale-smoke'];a.actor_cpus='2,3,4,5,6,7,8,9' if scale else '2,3,4,5'
    a.driver_cpus='10,11,12,13' if scale else '6,7,14,15';a.collector_cpus='14,15,16,17' if scale else '8,9,16,17'
    a.workers=[7,8] if scale else [3,4,5];a.variants=['async','tokio-pool','sharded-16','native-steal','native-root','native-runnable','native-deque']
    c=Campaign(a);assert not c.meta['dirty'];c.meta['controller_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    c.meta['purpose']='OwnerLIFO Crossbeamdeques/externalInjector/batchsteal vsMutexqueues+Runnable. Changestransportandinjectionpolicy,notisolatedmutexcausality. CommonnewsourceCPU/resources;versionsnotpooled. NativeN+R1plusidleowner. Deadlinepartialblocksmustnotbeclaimedcomplete.'
    if scale:
        specs=[('async',8,8),('sharded-16',8,8),('native-runnable',8,8),('native-runnable',8,32),('native-deque',7,8),('native-deque',8,8),('native-deque',8,32)]
    elif a.stage=='offered':
        specs=[('async',4,8)]+[(v,4,q) for v in ['native-runnable','native-deque'] for q in [8,32]]
    elif a.stage=='profiles':
        specs=[('async',4,8),('native-steal',4,8),('native-root',4,8),('native-runnable',4,8)]+[('native-deque',4,q) for q in [1,8,32,128]]
    else:
        specs=[('async',4,8),('async',5,8),('tokio-pool',4,8),('sharded-16',4,8),('native-steal',4,8)]
        specs += [(v,4,q) for v in ['native-root','native-runnable'] for q in [8,32]]
        specs += [('native-deque',3,8)]+[('native-deque',4,q) for q in [1,8,32,128]]
    jobs=[]
    for v,w,q in specs:
        exp=dict(SPAWNED_EXPERIMENT_SLICE_US=50,SPAWNED_EXPERIMENT_REACTORS=1,SPAWNED_EXPERIMENT_LOCAL_BUDGET=3,SPAWNED_EXPERIMENT_ROOT_QUANTUM=q)
        if a.stage=='offered':
            for layout in ['spread','concentrated']:
                for rate in [500_000,1_200_000]:
                    for burst in [1,1000]:
                        for repeat in range(2):
                            cfg=dict(mode='offered',layout=layout,actors=240,producers=4,messages=rate*2,rate=rate,burst=burst,iterations=100,**{'hot-iterations':5000})
                            jobs.append((v,w,cfg,repeat,exp))
            continue
        actors,producers=(16128,1024) if scale else (1920,256)
        for topology in ['chain','fanout']:
            for layout in ['same','cross']:
                if a.stage=='profiles' and topology!='chain':continue
                for caller in (['async'] if scale or a.stage=='profiles' else ['blocking','async']):
                    for repeat in range(1 if a.smoke or a.stage=='profiles' else (3 if scale else 5)):
                        cfg=dict(mode='graph',topology=topology,layout=layout,actors=actors,width=8,producers=producers,messages=4096 if a.smoke else (2_097_152 if scale else 1_048_576),iterations=100,caller=caller,**{'caller-threads':4,'observation-counter':'on'})
                        jobs.append((v,w,cfg,repeat,exp))
    random.Random(a.seed).shuffle(jobs);c.meta['planned_samples']=len(jobs);c.save()
    for v,w,cfg,repeat,exp in jobs:
        if time.time()>=a.stop_at:c.meta['stopped_at_deadline']=True;break
        row=c.sample('profile' if a.stage=='profiles' else 'deque-'+a.stage,v,w,cfg,repeat,experiment=exp)
        row['actor_workers']=w;row['total_runtime_workers']=w+(1 if v.startswith('native') else 0);c.save()
    c.meta['finished_utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime());c.save()


if __name__=='__main__':main()
