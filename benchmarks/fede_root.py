#!/usr/bin/env python3
"""Amortized per-worker driver versus per-poll bridge, matched resources/callers."""
import argparse,hashlib,random,time
from pathlib import Path
from fede_research import Campaign,VARIANTS
VARIANTS.update({'native-steal':('native-steal',16),'native-root':('native-root',16),'tokio-pool':('tokio-pool',0)})
def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True);p.add_argument('--stage',choices=['smoke','graphs','profiles'],required=True);p.add_argument('--stop-at',type=float,required=True);a=p.parse_args()
 a.actor_cpus='2,3,4,5';a.driver_cpus='6,7,14,15';a.collector_cpus='8,9,16,17';a.workers=[3,4,5];a.variants=['async','tokio-16','tokio-pool','sharded-16','native-steal','native-root'];a.seed=191;a.duration=2.;a.timeout=120.;a.smoke=a.stage=='smoke'
 c=Campaign(a);assert not c.meta['dirty'];c.meta['controller_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest();c.meta['purpose']='Same source,1920actors,256logicalproducers,8handlers/root. Native N3+R1 vsTokio4 equalruntimeworker;nativeN4+R1vsTokio4 equalactorworker andTokio5 equalruntimeworker,allshare4physicalactorcores. RootdriverobjectsNaddnoOSthreads,noActorTokioTasks. Blocking256threads vs4RT/256logicalcallerfutures,fullcounter. Rootquantum8. Profiledrows excludedfromperformance.'
 specs=[('async',4),('async',5),('tokio-pool',4),('tokio-16',4),('sharded-16',4),('native-steal',3),('native-steal',4),('native-root',3),('native-root',4)]
 jobs=[]
 for v,w in specs:
  for topology in ['chain','fanout']:
   for layout in ['same','cross']:
    for caller in (['blocking'] if a.stage in ['smoke','profiles'] else ['blocking','async']):
     if a.stage=='profiles' and (topology!='chain' or w==5):continue
     for repeat in range(1 if a.stage!='graphs' else 5):
      cfg=dict(mode='graph',topology=topology,layout=layout,actors=1920,width=8,producers=256,messages=4096 if a.smoke else 1_048_576,iterations=100,caller=caller,**{'caller-threads':4,'observation-counter':'on'})
      jobs.append((v,w,cfg,repeat))
 random.Random(a.seed).shuffle(jobs);c.meta['planned_samples']=len(jobs);c.save()
 for v,w,cfg,repeat in jobs:
  if time.time()>=a.stop_at:c.meta['stopped_at_deadline']=True;break
  row=c.sample('profile' if a.stage=='profiles' else 'root-'+a.stage,v,w,cfg,repeat,experiment=dict(SPAWNED_EXPERIMENT_SLICE_US=50,SPAWNED_EXPERIMENT_REACTORS=1,SPAWNED_EXPERIMENT_LOCAL_BUDGET=3));row['actor_workers']=w;row['total_runtime_workers']=w+(1 if v.startswith('native') else 0);c.save()
 c.meta['finished_utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime());c.save()
if __name__=='__main__':main()
