#!/usr/bin/env python3
"""Controlled native queue/caller/clock experiments; keep blocks separate."""
import argparse
import hashlib
from pathlib import Path
import random
import time
from fede_research import Campaign, VARIANTS

VARIANTS.update({'native-global':('native-global',16),'native-local':('native-local',16),
                 'native-steal':('native-steal',16),'tokio-pool':('tokio-pool',0)})

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--stage',choices=['smoke','queues','callers','clock','profiles','offered'],required=True)
    p.add_argument('--stop-at',type=float,required=True)
    a=p.parse_args();a.actor_cpus='2,3,4,5';a.driver_cpus='6,7,14,15';a.collector_cpus='8,9,16,17'
    a.workers=[3,4];a.variants=['async','tokio-16','sharded-16','tokio-pool','native-global','native-local','native-steal']
    a.seed=188;a.duration=2.;a.timeout=120.;a.smoke=a.stage=='smoke'
    c=Campaign(a);assert not c.meta['dirty']
    c.meta['controller_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    c.meta['purpose']='Separate independent experiment blocks. Queue comparison: fixed blocking caller/counter-on/slice50. Caller factorial: blocking256OSthreads vs4current-threadRTs/256logicalproducers and observationcounteron/off, common4actorCPU+4driverCPU. Clock comparison: async caller, counter on, slice0/25/50/200 with same turn16. Native3actor+R1 and native4actor+R1 compareTokio4actor; unequalactiveworkerthreads disclosed. No source mixing with prior07f.'
    jobs=[]
    policies=a.variants if a.stage in ['smoke','queues','profiles','offered'] else ['async','tokio-16','sharded-16','native-steal']
    for v in policies:
        for workers in ([3,4] if v.startswith('native') and a.stage in ['queues','profiles','offered'] else [4]):
            if a.stage=='offered':
                for layout in ['spread','concentrated']:
                    for rate in [500_000,900_000,1_200_000]:
                        for burst in [1,1000]:
                            for repeat in range(3):
                                cfg=dict(mode='offered',layout=layout,actors=240,producers=4,messages=rate*2,rate=rate,burst=burst,iterations=100,**{'hot-iterations':5000})
                                jobs.append((v,workers,cfg,repeat,dict(SPAWNED_EXPERIMENT_SLICE_US=50,SPAWNED_EXPERIMENT_REACTORS=1,SPAWNED_EXPERIMENT_LOCAL_BUDGET=3)))
                continue
            for topology in ['chain','fanout']:
                for layout in ['same','cross']:
                    caller_cases=[('blocking','on')]
                    if a.stage=='callers':caller_cases=[(x,y) for x in ['blocking','async'] for y in ['on','off']]
                    if a.stage=='clock':caller_cases=[('async','on')]
                    if a.smoke:caller_cases=[('blocking','on'),('async','on'),('async','off')]
                    for caller,counter in caller_cases:
                        slices=[0,25,50,200] if a.stage=='clock' and v!='async' else [50]
                        for us in slices:
                            for repeat in range(1 if a.stage in ['smoke','profiles'] else 5):
                                cfg=dict(mode='graph',topology=topology,layout=layout,actors=1920,width=8,producers=256,messages=4096 if a.smoke else 1_048_576,iterations=100,caller=caller,**{'caller-threads':4,'observation-counter':counter})
                                jobs.append((v,workers,cfg,repeat,dict(SPAWNED_EXPERIMENT_SLICE_US=us,SPAWNED_EXPERIMENT_REACTORS=1,SPAWNED_EXPERIMENT_LOCAL_BUDGET=3)))
    random.Random(a.seed).shuffle(jobs);c.meta['planned_samples']=len(jobs);c.save()
    for v,w,cfg,repeat,exp in jobs:
        if time.time()>=a.stop_at:c.meta['stopped_at_deadline']=True;break
        row=c.sample('profile' if a.stage=='profiles' else 'control-'+a.stage,v,w,cfg,repeat,experiment=exp)
        row['total_runtime_workers']=w+(1 if v.startswith('native') else 0);row['actor_workers']=w;c.save()
    c.meta['finished_utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime());c.save()

if __name__=='__main__':main()
