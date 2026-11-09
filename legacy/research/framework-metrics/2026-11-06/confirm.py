import argparse,csv,io,json,os,pathlib,random,subprocess
parser = argparse.ArgumentParser(description='Confirm baseline versus compiled-out metrics')
parser.add_argument('--out', type=pathlib.Path, required=True)
parser.add_argument('--cpus', default='2,3,4,5')
parser.add_argument('--nice', type=int, default=19)
args = parser.parse_args()
assert 0 <= args.nice <= 19 and os.getpriority(os.PRIO_PROCESS, 0) <= args.nice
out = args.out.resolve()
env=os.environ.copy()
for key in list(env):
 if key.startswith('SPAWNED_'): del env[key]
cases=[('async',1,1,0),('async',128,128,0),('async',128,128,800),('threads',1,1,0)]
runs=[(case,profile,r) for case in cases for profile in ['baseline','off'] for r in range(7)]
random.Random(20261107).shuffle(runs)
with (out/'confirmation.csv').open('w') as f:
 writer=None
 for i,(case,profile,replicate) in enumerate(runs):
  mode,actors,producers,work=case
  result=subprocess.run(['nice','-n',str(max(0,args.nice-os.getpriority(os.PRIO_PROCESS,0))),'taskset','-c',args.cpus,str(out/profile),mode,str(actors),str(producers),str(work),'3','0'],capture_output=True,text=True,env=env,timeout=40)
  assert result.returncode==0,(profile,result.stderr)
  row={'profile':profile,'replicate':replicate,**list(csv.DictReader(io.StringIO(result.stdout)))[0]}
  if writer is None: writer=csv.DictWriter(f,fieldnames=list(row));writer.writeheader()
  writer.writerow(row);f.flush()
  print(i,profile,case,flush=True)
(out/'confirmation-metadata.json').write_text(json.dumps({'nice':args.nice,'seed':20261107,'seconds':3,'replicates':7,'cpus':args.cpus,'profiles':['baseline','off'],'purpose':'resolve variable feature-off measurements; frozen binaries from initial builds','load_after':subprocess.check_output(['uptime'],text=True)},indent=2)+'\n')
