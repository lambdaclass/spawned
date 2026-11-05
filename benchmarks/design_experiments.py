#!/usr/bin/env python3
"""Matched runtime/reply/scheduler experiments; failures remain in the dataset."""
import argparse,csv,hashlib,json,os,platform,random,re,subprocess,time,signal,resource
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
V={
 'async':('async',{},{}), 'threads':('threads',{},{}), 'plain-tokio':('tokio',{},{}),
 'dedicated-default':('dedicated',{},{}),
 'dedicated-one':('dedicated',{}, {'SPAWNED_EXPERIMENT_RUNTIME':'one-worker'}),
 'dedicated-current':('dedicated',{}, {'SPAWNED_EXPERIMENT_RUNTIME':'current'}),
 'dedicated-shared':('dedicated',{}, {'SPAWNED_EXPERIMENT_RUNTIME':'shared'}),
 'dedicated-managed':('dedicated',{}, {'SPAWNED_EXPERIMENT_RUNTIME':'managed'}),
 'native-block':('native-block',{'reply':'bounded'},{}),
 'native-hybrid':('native-hybrid',{'reply':'bounded'},{}),
 'native-spin':('native-spin',{'reply':'bounded'},{}),
 'sharded-1':('native-sharded',{'reply':'bounded','batch':1},{}),
 'sharded-16':('native-sharded',{'reply':'bounded','batch':16},{}),
 'ready-1':('native-ready',{'reply':'bounded','batch':1},{}),
 'ready-16':('native-ready',{'reply':'bounded','batch':16},{}),
 'ready-adaptive':('native-ready',{'reply':'bounded','batch':0},{}),
 'local-ready-16':('native-ready',{'reply':'bounded','batch':16,'scheduler':'local'},{}),
}
for base in ['async','threads']:
 for reply in ['std','bounded','tokio']:
  V[f'{base}-reply-{reply}']=(base,{'reply':reply},{})
C={
 'rpc1':dict(scenario='rpc',actors=1,producers=1,messages=32768),
 'burst32':dict(scenario='burst',actors=1,producers=32,messages=131072),
 'active128_p32':dict(scenario='rpc',actors=128,producers=32,messages=131072,routing='round-robin'),
 'active128_p128':dict(scenario='rpc',actors=128,producers=128,messages=131072,routing='round-robin'),
 'active512_p128':dict(scenario='rpc',actors=512,producers=128,messages=262144,routing='round-robin'),
 'active1024_p256':dict(scenario='rpc',actors=1024,producers=256,messages=262144,routing='round-robin'),
 'hotspot128':dict(scenario='rpc',actors=128,producers=128,messages=131072,routing='hotspot',iterations=1000),
 'lowload':dict(scenario='paced',actors=4,producers=32,messages=10000,rate=5000,iterations=5000),
 'overload':dict(scenario='paced',actors=4,producers=32,messages=32768,rate=1000000,iterations=5000),
 'idle4':dict(scenario='rpc',actors=4,producers=1,messages=1,warmup=4,**{'idle-ms':1000}),
 'alloc1':dict(scenario='rpc',actors=1,producers=1,messages=32768,allocations='on'),
}
def allowed(variant,case):
 actors=C[case]['actors']
 if variant=='dedicated-default' and actors>4:return False
 if variant=='dedicated-one' and actors>512:return False
 if variant in ['native-spin','native-hybrid'] and actors>4:return False
 if '-reply-' in variant:return case in ['rpc1','burst32','alloc1']
 return True

def run(args):
 out=args.output.resolve();out.mkdir(parents=True,exist_ok=False);(out/'logs').mkdir()
 benches=['designs','io_designs']
 built=subprocess.run(['cargo','bench','--locked','-p','spawned-concurrency','--features','backend-experiments,io-experiments','--no-run','--message-format=json']+sum((['--bench',b] for b in benches),[]),cwd=ROOT,text=True,capture_output=True)
 (out/'build.log').write_text(built.stderr);built.check_returncode()
 binaries={}
 for line in built.stdout.splitlines():
  item=json.loads(line)
  if item.get('reason')=='compiler-artifact' and item['target']['name'] in benches and item.get('executable'):binaries[item['target']['name']]=item['executable']
 cells=[]
 if args.group in ['all','messages']:
  for v in args.variants:
   for c in args.cases:
    if allowed(v,c):
     for r in range(args.repetitions):cells.append(('messages',v,c,r))
 if args.group in ['all','io']:
  for v in ['std-io','tokio-io','monoio-legacy','tokio-mux','monoio-legacy-mux']+(['monoio-uring','monoio-uring-mux'] if platform.system()=='Linux' else []):
   for n in ([1,16] if args.smoke else [1,16,128]):
    for r in range(args.repetitions):cells.append(('io',v,str(n),r))
 random.Random(args.seed).shuffle(cells)
 git=lambda *a:subprocess.check_output(['git',*a],cwd=ROOT,text=True).strip()
 meta=dict(started_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),revision=git('rev-parse','HEAD'),dirty=bool(git('status','--porcelain')),platform=platform.platform(),architecture=platform.machine(),cpu_count=os.cpu_count(),rustc=subprocess.check_output(['rustc','--version'],text=True).strip(),repetitions=args.repetitions,seed=args.seed,smoke=args.smoke,observer='direct (process watchdog)',nofile=resource.getrlimit(resource.RLIMIT_NOFILE),requested_fd_limit=args.fd_limit,cgroup={str(p):p.read_text().strip() for p in [Path('/sys/fs/cgroup/cpuset.cpus.effective'),Path('/sys/fs/cgroup/cpu.max'),Path('/sys/fs/cgroup/memory.max'),Path('/sys/fs/cgroup/pids.max')] if p.exists()},schedule=cells,definitions={'variants':V,'cases':C},environment={k:os.environ.get(k) for k in ['TOKIO_WORKER_THREADS','SPAWNED_EXPERIMENT_RUNTIME','RUSTFLAGS']},binaries={k:hashlib.sha256(Path(v).read_bytes()).hexdigest() for k,v in binaries.items()})
 (out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
 rows=[]
 for index,(group,v,c,repeat) in enumerate(cells):
  env=os.environ.copy();env.pop('TOKIO_WORKER_THREADS',None);env.pop('SPAWNED_EXPERIMENT_RUNTIME',None)
  if group=='messages':
   backend,options,extra=V[v];env.update(extra)
   config=dict(C[c],**options,backend=backend,workers=args.workers,capacity=args.capacity,observer='direct')
   if args.smoke:config['messages']=max(config['actors']*2,2048);config['warmup']=min(128,config['actors'])
   if args.smoke and c=='idle4':config['messages']=1
   exe=binaries['designs']
  else:config=dict(backend=v,connections=int(c),messages=16 if args.smoke else 1000);exe=binaries['io_designs']
  invocation=[exe]+[f'--{k}={value}' for k,value in config.items()]
  prefix=['/usr/bin/time','-l' if platform.system()=='Darwin' else '-v']
  start=time.monotonic()
  status='ok';stdout='';stderr=''
  process=subprocess.Popen(prefix+invocation,cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True,preexec_fn=(lambda: resource.setrlimit(resource.RLIMIT_NOFILE,(args.fd_limit,resource.getrlimit(resource.RLIMIT_NOFILE)[1]))) if args.fd_limit else None)
  try:
   stdout,stderr=process.communicate(timeout=90)
   if process.returncode:status=f'exit_{process.returncode}'
  except subprocess.TimeoutExpired:
   os.killpg(process.pid,signal.SIGKILL)
   stdout,stderr=process.communicate()
   status='timeout'
  stem=f'{index:04d}-{group}-{v}-{c}-{repeat}'
  (out/'logs'/f'{stem}.stdout').write_text(stdout);(out/'logs'/f'{stem}.stderr').write_text(stderr)
  row=dict(group=group,variant=v,case=c,repeat=repeat,status=status,process_wall_s=time.monotonic()-start,config_json=json.dumps(config,sort_keys=True))
  if status=='ok':
   parsed=list(csv.DictReader(stdout.splitlines()))
   if len(parsed)!=1 or None in parsed[0] or any(v is None for v in parsed[0].values()):row['status']='invalid_output'
   else:row.update(parsed[0])
  if platform.system()=='Darwin':
   rss=re.search(r'(\d+)\s+maximum resident set size',stderr);cpu=re.search(r'([\d.]+)\s+user\s+([\d.]+)\s+sys',stderr)
   row['rss_bytes']=int(rss[1]) if rss else ''
   row['cpu_s']=sum(map(float,cpu.groups())) if cpu else ''
  else:
   rss=re.search(r'Maximum resident set size \(kbytes\):\s*(\d+)',stderr)
   u=re.search(r'User time \(seconds\):\s*([\d.]+)',stderr);s=re.search(r'System time \(seconds\):\s*([\d.]+)',stderr)
   row['rss_bytes']=int(rss[1])*1024 if rss else '';row['cpu_s']=float(u[1])+float(s[1]) if u and s else ''
  rows.append(row)
  fields=list(dict.fromkeys(k for row in rows for k in row))
  with (out/'raw.csv').open('w') as f:
   writer=csv.DictWriter(f,fieldnames=fields);writer.writeheader();writer.writerows(rows)
  print(f'{index+1}/{len(cells)} {group} {v} {c} {repeat}: {row["status"]}',flush=True)
 if any(r['status']!='ok' for r in rows):print('Some samples failed; raw.csv retains failures. Do not treat failures as throughput samples.',flush=True)
if __name__=='__main__':
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True);p.add_argument('--repetitions',type=int,default=3);p.add_argument('--seed',type=int,default=178);p.add_argument('--workers',type=int,default=2);p.add_argument('--capacity',type=int,default=256);p.add_argument('--group',choices=['all','messages','io'],default='all');p.add_argument('--smoke',action='store_true');p.add_argument('--fd-limit',type=int,default=0);p.add_argument('--cases',nargs='+',choices=C,default=list(C));p.add_argument('--variants',nargs='+',choices=V,default=list(V));args=p.parse_args();assert args.repetitions>0;run(args)
