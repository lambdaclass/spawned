import concurrent.futures,datetime,glob,hashlib,json,os,pathlib,re,signal,subprocess,tarfile,time,tomllib,urllib.request
base=pathlib.Path('/tmp/spawned-pr-security-review')
source=base/'ethlambda-spawned-baseline'
root=base/'ethlambda-baseline-build';root.mkdir(exist_ok=True)
target=base/'ethlambda-control-build/target'
tools=base/'ethlambda-control-toolchain';prefix=tools/'installed'
os.sched_setaffinity(0,{2,3,4,5})
os.nice(max(0,10-os.getpriority(os.PRIO_PROCESS,0)))
manifest=tomllib.loads((tools/'manifest.toml').read_text())
def component(name):
 entry=manifest['pkg'][name]['target']['x86_64-unknown-linux-gnu']
 destination=tools/entry['xz_url'].rsplit('/',1)[-1]
 if not destination.exists() or hashlib.sha256(destination.read_bytes()).hexdigest()!=entry['xz_hash']:
  part=destination.with_suffix(destination.suffix+'.part');digest=hashlib.sha256()
  with urllib.request.urlopen(entry['xz_url'],timeout=60) as response,part.open('wb') as output:
   while chunk:=response.read(1024*1024):output.write(chunk);digest.update(chunk)
  assert digest.hexdigest()==entry['xz_hash'],name
  part.replace(destination)
 print('Verified official toolchain component:',name,flush=True)
 return destination
if not (prefix/'bin/rustc').exists():
 with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:archives=list(pool.map(component,['rustc','cargo','rust-std']))
 for archive in archives:
  with tarfile.open(archive) as bundle:bundle.extractall(tools,filter='data')
  directory=tools/archive.name.removesuffix('.tar.xz')
  with (root/('install-'+archive.name.split('-')[0]+'.log')).open('w') as output:
   subprocess.run(['bash',str(directory/'install.sh'),'--prefix='+str(prefix),'--disable-ldconfig'],stdout=output,stderr=subprocess.STDOUT,check=True,timeout=180)
assert subprocess.check_output([str(prefix/'bin/rustc'),'-V'],text=True).startswith('rustc 1.97.1')
print('Matching Rust 1.97.1 toolchain installed.',flush=True)
# Give the archive an exact Git base and a private diagnostic commit for vergen.
def git(*args,**kw):return subprocess.check_output(['git',*args],cwd=source,text=True,**kw).strip()
with (root/'source-preparation.log').open('w') as output:
 if not (source/'.git').exists():
  subprocess.run(['git','init','-q'],cwd=source,stdout=output,stderr=output,check=True)
  subprocess.run(['git','remote','add','origin','https://github.com/lambdaclass/ethlambda.git'],cwd=source,stdout=output,stderr=output,check=True)
  subprocess.run(['git','fetch','-q','--depth=1','--filter=blob:none','origin','4117c5aef5ca3b916c3d871cea042805b57a4e88'],cwd=source,stdout=output,stderr=output,check=True,timeout=120)
  subprocess.run(['git','reset','--mixed','FETCH_HEAD'],cwd=source,stdout=output,stderr=output,check=True)
 if git('branch','--show-current')!='diagnostic/spawned05-baseline':
  subprocess.run(['git','checkout','-b','diagnostic/spawned05-baseline'],cwd=source,stdout=output,stderr=output,check=True)
assert not git('status','--porcelain')
lock=hashlib.sha256((source/'Cargo.lock').read_bytes()).hexdigest()
assert lock=='d992254ee18b49e4714b08c07c4db9a2ccdc265f617ee21e3465150692509610'
env=os.environ.copy()
for key in list(env):
 if key.startswith(('SPAWNED_','CARGO_PROFILE_RELEASE_')) or key in ['RUSTFLAGS','RUSTDOCFLAGS','RUSTC_WRAPPER','TOKIO_WORKER_THREADS']:env.pop(key,None)
env.update(PATH=str(prefix/'bin')+':'+env['PATH'],RUSTC=str(prefix/'bin/rustc'),RUSTDOC=str(prefix/'bin/rustdoc'),CARGO_BUILD_JOBS='2',CARGO_TARGET_DIR=str(target),CARGO_NET_RETRY='1',CARGO_NET_GIT_FETCH_WITH_CLI='true')
clang=sorted(glob.glob('/usr/lib/llvm-*/lib/libclang.so*'))
if clang:env['LIBCLANG_PATH']=str(pathlib.Path(clang[-1]).parent)
command=[str(prefix/'bin/cargo'),'build','--locked','--release','--bin','ethlambda']
meta={'canonical_base':'4117c5aef5ca3b916c3d871cea042805b57a4e88','build_commit':git('rev-parse','HEAD'),'source_tree':git('rev-parse','HEAD^{tree}'),'cargo_lock_sha256':lock,'command':command,'rustc':subprocess.check_output([str(prefix/'bin/rustc'),'-Vv'],text=True),'cargo':subprocess.check_output([str(prefix/'bin/cargo'),'-V'],text=True),'cpus':sorted(os.sched_getaffinity(0)),'nice':os.getpriority(os.PRIO_PROCESS,0),'build_jobs':2,'rss_limit_bytes':16*1024**3,'thread_limit':128,'timeout_s':2700,'production_deployed':False,'profile':'repository release (fat LTO, one codegen unit)','target_flags':(source/'.cargo/config.toml').read_text(),'libclang_path':env.get('LIBCLANG_PATH'),'host_cc':subprocess.check_output(['cc','--version'],text=True).splitlines()[0]}
(root/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
print('Starting bounded release build:',meta['build_commit'],flush=True)
start=time.monotonic();peak_rss=peak_threads=peak_fds=0;reason=None
with (root/'build.log').open('w') as output:
 process=subprocess.Popen(command,cwd=source,env=env,stdout=output,stderr=subprocess.STDOUT,start_new_session=True)
 try:
  while process.poll() is None:
   rss=threads=fds=0
   for stat in pathlib.Path('/proc').glob('[0-9]*/stat'):
    try:
     fields=stat.read_text().rsplit(')',1)[1].split()
     if int(fields[2])!=process.pid:continue
     status=(stat.parent/'status').read_text();values={x.split(':',1)[0]:x.split(':',1)[1].strip() for x in status.splitlines() if ':' in x}
     rss+=int(values.get('VmRSS','0').split()[0])*1024;threads+=int(values.get('Threads','0'));fds+=len(list((stat.parent/'fd').iterdir()))
    except (FileNotFoundError,PermissionError,ProcessLookupError):pass
   peak_rss=max(rss,peak_rss);peak_threads=max(threads,peak_threads);peak_fds=max(fds,peak_fds)
   if rss>meta['rss_limit_bytes']:reason='build RSS budget exceeded'
   if threads>128:reason='build thread budget exceeded'
   if fds>2048:reason='build descriptor budget exceeded'
   if time.monotonic()-start>2700:reason='build watchdog timeout'
   if reason:os.killpg(process.pid,signal.SIGKILL);break
   time.sleep(.5)
  code=process.wait()
 finally:
  if process.poll() is None:os.killpg(process.pid,signal.SIGKILL);process.wait()
result={'returncode':code,'failure':reason,'elapsed_s':time.monotonic()-start,'sampled_peak_rss_bytes':peak_rss,'sampled_peak_threads':peak_threads,'sampled_peak_fds':peak_fds}
(root/'result.json').write_text(json.dumps(result,indent=2)+'\n')
assert code==0 and reason is None,result
binary=target/'release/ethlambda'
version=subprocess.check_output([str(binary),'--version'],env=env,text=True,timeout=15)
(root/'version.txt').write_text(version)
result.update(binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),binary_bytes=binary.stat().st_size,version=version.strip())
(root/'result.json').write_text(json.dumps(result,indent=2)+'\n')
print('Release baseline binary built:',version.strip(),flush=True)
