#!/usr/bin/env python3
"""Bounded, process-isolated reliability and benchmark campaign (Linux)."""
import argparse,csv,io,json,os,pathlib,random,signal,subprocess,time
parser=argparse.ArgumentParser()
parser.add_argument('--source',type=pathlib.Path,default=pathlib.Path.cwd())
parser.add_argument('--out',type=pathlib.Path,required=True)
parser.add_argument('--cpus',default='2,3,4,5')
parser.add_argument('--phase',choices=['tests','stress','startup','bench','all'],default='all')
parser.add_argument('--seconds',type=int,default=30)
parser.add_argument('--replicates',type=int,default=3)
parser.add_argument('--rss-mib',type=int,default=512)
parser.add_argument('--max-threads',type=int,default=768)
parser.add_argument('--clippy-rustc')
parser.add_argument('--clippy-rustdoc')
args=parser.parse_args()
assert 1<=args.seconds<=300 and 1<=args.replicates<=10 and 64<=args.rss_mib<=2048 and 32<=args.max_threads<=1024
source=args.source.resolve();out=args.out.resolve();out.mkdir(parents=True,exist_ok=True)
cpus=set()
for part in args.cpus.split(','):
 bounds=part.split('-');cpus.update(range(int(bounds[0]),int(bounds[-1])+1))
os.sched_setaffinity(0,cpus) # include the watchdog in the declared CPU budget
env=os.environ.copy()
for key in list(env):
 if key.startswith('SPAWNED_') or key=='TOKIO_WORKER_THREADS':del env[key]
sequence=0;records=[]
def run(label,command,extra=None,timeout=180,rss_limit=None,thread_limit=None,expected_error=None):
 global sequence
 sequence+=1;stem=f'{sequence:03d}-{label}'
 stdout=out/(stem+'.stdout');stderr=out/(stem+'.stderr')
 process_env={**env,**(extra or {})}
 adjustment=max(0,10-os.getpriority(os.PRIO_PROCESS,0))
 assert os.getpriority(os.PRIO_PROCESS,0)<=10,'run the parent at nice <=10'
 wrapped=['nice','-n',str(adjustment),'taskset','-c',args.cpus,*map(str,command)]
 began=time.monotonic();peak_rss=peak_threads=peak_fds=0;reason=None
 with stdout.open('w') as output,stderr.open('w') as error:
  process=subprocess.Popen(wrapped,cwd=source,env=process_env,stdout=output,stderr=error,start_new_session=True)
  try:
   while process.poll() is None:
    rss=threads=fds=0
    for stat in pathlib.Path('/proc').glob('[0-9]*/stat'):
     try:
      fields=stat.read_text().rsplit(')',1)[1].split()
      if int(fields[2])!=process.pid:continue # only this owned process group
      status=(stat.parent/'status').read_text()
      values={line.split(':',1)[0]:line.split(':',1)[1].strip() for line in status.splitlines() if ':' in line}
      rss+=int(values.get('VmRSS','0').split()[0])*1024
      threads+=int(values.get('Threads','0'))
      fds+=len(list((stat.parent/'fd').iterdir()))
     except (FileNotFoundError,ProcessLookupError,PermissionError):pass
    peak_rss=max(peak_rss,rss);peak_threads=max(peak_threads,threads);peak_fds=max(peak_fds,fds)
    if time.monotonic()-began>timeout:reason='watchdog timeout'
    if peak_rss>(rss_limit or args.rss_mib)*1024*1024:reason='RSS budget exceeded'
    if peak_threads>(thread_limit or args.max_threads):reason='thread budget exceeded'
    if peak_fds>2048:reason='descriptor budget exceeded'
    if reason:
     try:os.killpg(process.pid,signal.SIGKILL)
     except ProcessLookupError:pass
     break
    time.sleep(.05)
   code=process.wait()
  finally:
   if process.poll() is None:os.killpg(process.pid,signal.SIGKILL);process.wait()
 record={'label':label,'command':list(map(str,command)),'env':extra or {},'returncode':code,'failure':reason,'elapsed_s':time.monotonic()-began,'rss_bytes':peak_rss,'threads':peak_threads,'fds':peak_fds,'stdout':stdout.name,'stderr':stderr.name,'expected_error':expected_error}
 records.append(record);(out/'runs.json').write_text(json.dumps(records,indent=2)+'\n')
 print(label,code,reason or 'ok',flush=True)
 if expected_error is None:
  assert code==0 and reason is None,record
 else:
  assert code!=0 and reason is None and expected_error in stderr.read_text(),record
 return stdout
metadata={'source':subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'cpus':args.cpus,'effective_nice':10,'seconds':args.seconds,'replicates':args.replicates,'hardware':subprocess.check_output(['lscpu'],text=True),'rustc':subprocess.check_output(['rustc','-Vv'],text=True),'shared_host':True,'memory_sampling':'sampled aggregate RSS/threads/fds of the owned process group; shared pages may be counted more than once; sampling may miss brief peaks'}
(out/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
if args.phase in ['tests','all']:
 for features in [[],['--all-features']]:
  tag='all' if features else 'default'
  run('workspace-'+tag,['cargo','test','--locked','--workspace',*features],timeout=600,rss_limit=2048,thread_limit=1024)
  clippy_env={}
  if args.clippy_rustc:clippy_env['RUSTC']=args.clippy_rustc
  if args.clippy_rustdoc:clippy_env['RUSTDOC']=args.clippy_rustdoc
  run('clippy-'+tag,['cargo','clippy','--locked','--workspace','--all-targets',*features,'--','-D','warnings'],clippy_env,timeout=600,rss_limit=2048)
if args.phase in ['tests','stress','all']:
 test_names=['request_histories','delivery_simulation','failure_windows','payload_stress','actor_scenarios']
 build_command=['cargo','test','--locked','-p','spawned-concurrency','--no-run','--message-format=json']
 for name in test_names:build_command.extend(['--test',name])
 built_tests=run('build-reliability-tests',build_command,timeout=600,rss_limit=2048)
 tests={}
 for line in built_tests.read_text().splitlines():
  try:obj=json.loads(line)
  except ValueError:continue
  if obj.get('reason')=='compiler-artifact' and obj.get('executable'):tests[obj['target']['name']]=obj['executable']
 assert set(test_names)<=tests.keys()
 run('histories',[tests['request_histories'],'--nocapture'],{'SPAWNED_HISTORY_CASES':'128','SPAWNED_HISTORY_SEED':'20261106'},timeout=180)
 run('simulations',[tests['delivery_simulation'],'--nocapture'],{'SPAWNED_SIM_CASES':'256','SPAWNED_SIM_SEED':'20261106'},timeout=180)
 run('failure-windows',[tests['failure_windows'],'--nocapture'],{'SPAWNED_FAILURE_REPORT':str(out/'failure-coverage.txt')})
 run('payloads',[tests['payload_stress'],'--ignored','--nocapture'],{'SPAWNED_STRESS_PRODUCERS':'128','SPAWNED_STRESS_MESSAGES':'256','SPAWNED_STRESS_PAYLOAD':'1024'})
 run('scenarios',[tests['actor_scenarios']],{'PROPTEST_CASES':'256','PROPTEST_RNG_SEED':'20261106'},timeout=300)
 run('churn',[tests['actor_scenarios'],'sustained_actor_churn','--ignored'],{'SPAWNED_STRESS_ROUNDS':'100'},timeout=300)
 run('cancellation-model',['cargo','test','--locked','-p','spawned-rt','--test','cancellation_loom'],{'RUSTFLAGS':'--cfg spawned_loom'},timeout=300,rss_limit=2048)
 run('ready-model',['cargo','test','--locked','-p','spawned-concurrency','--features','backend-experiments','--test','ready_gate_loom'],{'RUSTFLAGS':'--cfg spawned_scheduler_loom'},timeout=300,rss_limit=2048)
if args.phase in ['startup','bench','all']:
 built=run('build-drivers',['cargo','build','--locked','--release','-p','spawned-concurrency','--features','backend-experiments','--bench','baseline','--bench','workload_research','--bench','lifecycle_soak','--message-format=json'],timeout=600,rss_limit=2048)
 binaries={}
 for line in built.read_text().splitlines():
  try:obj=json.loads(line)
  except ValueError:continue
  if obj.get('reason')=='compiler-artifact' and obj.get('executable'):binaries[obj['target']['name']]=obj['executable']
 assert {'baseline','workload_research','lifecycle_soak'}<=binaries.keys()
 # Invalid worker affinity must fail setup promptly, never strand a barrier.
 run('startup-invalid-affinity',[binaries['workload_research'],'--mode=graph','--actors=32','--producers=8','--messages=64','--workers=4','--driver-cpus=999999','--collector-cpus='+args.cpus],timeout=10,expected_error='thread affinity:')
 # Lower only this child's FD limit; an actual EMFILE must also terminate.
 run('startup-low-fd',['sh','-c','ulimit -n 8; exec "$@"','startup-fixture',binaries['workload_research'],'--mode=graph','--actors=32','--producers=8','--messages=64','--workers=4','--driver-cpus='+args.cpus,'--collector-cpus='+args.cpus],timeout=10,expected_error='Too many open files')
 if args.phase=='startup':
  (out/'COMPLETE').write_text('startup failure regressions passed\n')
  raise SystemExit(0)
 cases=[]
 for backend in ['async','blocking','dedicated','threads','tokio']:
  cases.append((f'rpc-{backend}-a8-p128',['--backend='+backend,'--scenario=rpc','--actors=8','--producers=128']))
 cases.extend([
  ('rpc-async-a512-p256',['--backend=async','--scenario=rpc','--actors=512','--producers=256']),
  ('rpc-native-a128-p128',['--backend=threads','--scenario=rpc','--actors=128','--producers=128']),
  ('burst-async-a128-p128',['--backend=async','--scenario=burst','--actors=128','--producers=128']),
 ])
 runs=[(label,flags,replicate) for label,flags in cases for replicate in range(args.replicates)]
 random.Random(20261106).shuffle(runs)
 for label,flags,replicate in runs:
  run(f'{label}-r{replicate}',[binaries['baseline'],*flags,'--messages=65536','--warmup=512','--workers=4','--payload=64','--iterations=800','--timing=on','--allocations=off'],timeout=120)
 for scheduler in ['tokio','sharded','native-deque']:
  extra={'SPAWNED_EXPERIMENT_SCHEDULER':scheduler,'SPAWNED_EXPERIMENT_WORKERS':'4','SPAWNED_EXPERIMENT_BATCH':'16'}
  for replicate in range(args.replicates):
   stem=f'graph-{scheduler}-r{replicate}'
   run(stem,[binaries['workload_research'],'--mode=graph','--topology=chain','--layout=cross','--actors=4096','--producers=256','--messages=65536','--width=8','--iterations=800','--caller=async','--caller-threads=4','--workers=4','--driver-cpus='+args.cpus,'--collector-cpus='+args.cpus,'--series='+str(out/(stem+'-series.csv')),'--threads='+str(out/(stem+'-threads.csv'))],extra,timeout=120)
  for rate in [100_000,1_000_000]:
   stem=f'offered-{scheduler}-{rate}'
   run(stem,[binaries['workload_research'],'--mode=offered','--layout=concentrated','--actors=4096','--producers=256','--messages=65536','--rate='+str(rate),'--hot-actors=4','--iterations=800','--hot-iterations=50000','--deadline-ms=100','--workers=4','--driver-cpus='+args.cpus,'--collector-cpus='+args.cpus,'--series='+str(out/(stem+'-series.csv')),'--threads='+str(out/(stem+'-threads.csv'))],extra,timeout=120)
  stem=f'soak-{scheduler}'
  run(stem,[binaries['lifecycle_soak'],'--seconds='+str(args.seconds),'--workers=4','--idle=4096','--churn=256','--steps=16','--driver-cpus='+args.cpus,'--sampler-cpus='+args.cpus,'--series='+str(out/(stem+'-series.csv'))],extra,timeout=args.seconds+60)
(out/'COMPLETE').write_text('all selected campaign phases passed\n')
