import csv, hashlib, io, json, os, pathlib, random, shutil, subprocess
out=pathlib.Path('/tmp/spawned-pr-security-review/metrics-benchmark')
out.mkdir(exist_ok=True)
root=pathlib.Path('/tmp/spawned-fede-research-25dGGmPA')
source=pathlib.Path('/tmp/spawned-pr-security-review/integrated')
baseline=pathlib.Path('/tmp/spawned-pr-security-review/metrics-baseline')
def call(args,cwd,stdout=None,env=None):
 return subprocess.run(args,cwd=cwd,env=env,stdout=stdout,stderr=subprocess.STDOUT,check=True,text=True)
if not baseline.exists(): call(['git','worktree','add','--detach',str(baseline),'bfa1bafa91c0b1ec5256fb5938dc4f2c772c48b6'],source)
shutil.copy(source/'concurrency/benches/metrics_overhead.rs',baseline/'concurrency/benches/metrics_overhead.rs')
p=baseline/'concurrency/Cargo.toml'
s=p.read_text()
if 'name = "metrics_overhead"' not in s: p.write_text(s+'\n[[bench]]\nname = "metrics_overhead"\nharness = false\n')
env=os.environ.copy()
env['CARGO_TARGET_DIR']=str(root/'fairness-target')
for key in list(env):
 if key.startswith('SPAWNED_'): del env[key]
for name,cwd,features in [('baseline',baseline,[]),('off',source,[]),('metrics',source,['--features','metrics'])]:
 log=out/f'build-{name}.jsonl'
 with log.open('w') as f:
  call(['cargo','build','--locked','--release','-p','spawned-concurrency','--bench','metrics_overhead','--message-format=json',*features],cwd,f,env)
 executable=None
 for line in log.read_text().splitlines():
  try: obj=json.loads(line)
  except ValueError: continue
  if obj.get('reason')=='compiler-artifact' and obj.get('target',{}).get('name')=='metrics_overhead' and obj.get('executable'): executable=obj['executable']
 assert executable,name
 shutil.copy(executable,out/name)
metadata={'baseline':'bfa1bafa91c0b1ec5256fb5938dc4f2c772c48b6','source':subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'driver_sha256':hashlib.sha256((source/'concurrency/benches/metrics_overhead.rs').read_bytes()).hexdigest(),'cpus':'2,3,4,5','nice':10,'seconds':2,'replicates':3,'seed':20261106,'profiles':{'baseline':'pre-metrics source plus identical benchmark driver only','off':'new source, metrics feature disabled','counts':'metrics feature, timing 0','sample16':'metrics feature, timing 16','full':'metrics feature, timing 1'}}
for key,args in [('rustc',['rustc','-Vv']),('lscpu',['lscpu']),('load_before',['uptime'])]: metadata[key]=subprocess.check_output(args,text=True)
(out/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
cases=[('async',1,1,0),('async',128,128,0),('async',128,128,800),('threads',1,1,0)]
profiles=[('baseline','baseline',0),('off','off',0),('counts','metrics',0),('sample16','metrics',16),('full','metrics',1)]
runs=[(case,profile,r) for case in cases for profile in profiles for r in range(3)]
random.Random(20261106).shuffle(runs)
with (out/'raw.csv').open('w') as raw:
 writer=None
 for i,(case,(profile,binary,sample),replicate) in enumerate(runs):
  mode,actors,producers,work=case
  result=subprocess.run(['nice','-n','10','taskset','-c','2,3,4,5',str(out/binary),mode,str(actors),str(producers),str(work),'2',str(sample)],capture_output=True,text=True,env=env,timeout=40)
  stem=f'{i:02d}-{profile}-{mode}-a{actors}-w{work}-r{replicate}'
  (out/f'{stem}.stdout').write_text(result.stdout)
  (out/f'{stem}.stderr').write_text(result.stderr)
  assert result.returncode==0,(stem,result.stderr)
  rows=list(csv.DictReader(io.StringIO(result.stdout)))
  assert len(rows)==1,stem
  row={'profile':profile,'replicate':replicate,**rows[0]}
  if writer is None: writer=csv.DictWriter(raw,fieldnames=list(row));writer.writeheader()
  writer.writerow(row);raw.flush()
  print(stem,flush=True)
metadata['load_after']=subprocess.check_output(['uptime'],text=True)
(out/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
print('COMPLETE',flush=True)
