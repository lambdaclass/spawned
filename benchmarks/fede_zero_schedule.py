#!/usr/bin/env python3
"""Paired zero-sized/captured schedule callback, one common source and root quantum."""
import argparse
import hashlib
from pathlib import Path
import random
import time
from fede_research import Campaign, VARIANTS
VARIANTS.update({v:(v,16) for v in ['native-root','native-runnable','native-deque']})


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--stage',choices=['smoke','graphs','profiles'],required=True)
    p.add_argument('--stop-at',type=float,required=True)
    a=p.parse_args();a.seed=199;a.duration=2.;a.timeout=120.;a.smoke=a.stage=='smoke'
    a.actor_cpus='2,3,4,5';a.driver_cpus='6,7,14,15';a.collector_cpus='8,9,16,17';a.workers=[4,5];a.variants=['async','native-root','native-runnable','native-deque']
    c=Campaign(a);assert not c.meta['dirty'];c.meta['controller_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    c.meta['purpose']='Same source/queues/quantum8, capturedWeakSchedule0 vszeroSizedFunctionItem1 crossedwithbusy-peer wakeprecheck0/1. Metadatareadbeforetransfer. Precheckskipsrotating-targetRMWwhennopeerobservedsleeping;notahardwakeguarantee. BothTask/Deque. Tokio4/5andRoot4controls share4physicalactorcores.NativeN4+R1+idleowner. Graphasync4RT/256logicalproducers,1920actors1Mroots8handlers/root. No source pooling, no hard-preemption claim.'
    specs=[('async',4,0,0),('async',5,0,0)]+[('native-root',4,0,w) for w in [0,1]]+[(v,4,z,w) for v in ['native-runnable','native-deque'] for z in [0,1] for w in [0,1]]
    if a.stage=='profiles':specs=[s for s in specs if s[0] in ['native-runnable','native-deque']]
    jobs=[]
    for v,w,z,precheck in specs:
        for topology in ['chain','fanout']:
            if a.stage=='profiles' and topology!='chain':continue
            for layout in ['same','cross']:
                for repeat in range(5 if a.stage=='graphs' else 1):
                    cfg=dict(mode='graph',topology=topology,layout=layout,actors=1920,width=8,producers=256,messages=4096 if a.smoke else 1_048_576,iterations=100,caller='async',**{'caller-threads':4,'observation-counter':'on'})
                    exp=dict(SPAWNED_EXPERIMENT_SLICE_US=50,SPAWNED_EXPERIMENT_REACTORS=1,SPAWNED_EXPERIMENT_LOCAL_BUDGET=3,SPAWNED_EXPERIMENT_ROOT_QUANTUM=8,SPAWNED_EXPERIMENT_ZERO_SCHEDULE=z,SPAWNED_EXPERIMENT_WAKE_PRECHECK=precheck)
                    jobs.append((v,w,cfg,repeat,exp))
    random.Random(a.seed).shuffle(jobs);c.meta['planned_samples']=len(jobs);c.save()
    for v,w,cfg,repeat,exp in jobs:
        if time.time()>=a.stop_at:c.meta['stopped_at_deadline']=True;break
        row=c.sample('profile' if a.stage=='profiles' else 'callback-'+a.stage,v,w,cfg,repeat,experiment=exp)
        row['actor_workers']=w;row['total_runtime_workers']=w+(1 if v.startswith('native') else 0);c.save()
    c.meta['finished_utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime());c.save()


if __name__=='__main__':main()
