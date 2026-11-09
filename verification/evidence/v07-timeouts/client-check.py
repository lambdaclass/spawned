from pathlib import Path
import glob,hashlib,json,os,subprocess,tarfile,time,tomllib
source=Path('/tmp/ethlambda-v07-compat-20261109')
source.mkdir(exist_ok=False)
archive=Path('/tmp/ethlambda-v07-compat-source.tgz')
with tarfile.open(archive) as bundle: bundle.extractall(source,filter='data')
original=(source/'Cargo.lock').read_bytes()
manifest=source/'Cargo.toml'
text=manifest.read_text()
for name,folder in [('spawned-concurrency','concurrency'),('spawned-rt','rt')]:
 old=f'{name} = "0.5.0"'
 assert text.count(old)==1,(name,'unexpected workspace dependency')
 text=text.replace(old,f'{name} = {{ path = "/tmp/spawned-v07/{folder}" }}')
manifest.write_text(text)
tool=Path('/tmp/spawned-pr-security-review/ethlambda-control-toolchain/installed/bin')
env=os.environ.copy()
for name in list(env):
 if name.startswith(('SPAWNED_','CARGO_PROFILE_RELEASE_')) or name in {'RUSTFLAGS','RUSTDOCFLAGS','RUSTC_WRAPPER','TOKIO_WORKER_THREADS'}:env.pop(name,None)
env.update(PATH=str(tool)+':'+env['PATH'],RUSTC=str(tool/'rustc'),RUSTDOC=str(tool/'rustdoc'),CARGO_BUILD_JOBS='1',CARGO_TARGET_DIR='/tmp/spawned-pr-security-review/ethlambda-control-build/target')
clang=sorted(glob.glob('/usr/lib/llvm-*/lib/libclang.so*'))
if clang:env['LIBCLANG_PATH']=str(Path(clang[-1]).parent)
metadata={'client_base':'c3f5f566cb3a61e3d388c66ce21794c18a6618ce','spawned_source':'6ecec728 plus unreleased timeout/examples working tree','source_archive_sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'production_deployed':False,'kind':'compile compatibility only, not a runtime canary','compiler':subprocess.check_output([str(tool/'rustc'),'-V'],text=True).strip()}
log=Path('/tmp/spawned-v07-client-check.log')
start=time.monotonic()
with log.open('w') as output:
 command=[str(tool/'cargo'),'metadata','--offline','--filter-platform','x86_64-unknown-linux-gnu','--format-version','1']
 with Path('/tmp/spawned-v07-client-cargo-metadata.json').open('w') as data:
  result=subprocess.run(command,cwd=source,env=env,stdout=data,stderr=output)
 if result.returncode:metadata.update(stage='dependency resolution',returncode=result.returncode)
 else:
  before=tomllib.loads(original.decode())['package'];after=tomllib.loads((source/'Cargo.lock').read_text())['package']
  def versions(packages):
   result={}
   for p in packages:result.setdefault(p['name'],set()).add(p['version'])
   return result
  old,new=versions(before),versions(after)
  metadata['version_replacements']={n:{'old':sorted(old[n]),'new':sorted(new[n])} for n in old.keys()&new.keys() if not n.startswith('spawned-') and old[n]-new[n] and new[n]-old[n]}
  metadata['cargo_lock_sha256']=hashlib.sha256((source/'Cargo.lock').read_bytes()).hexdigest()
  subprocess.run(['git','init','-q','-b','candidate-v07'],cwd=source,check=True,stdout=output,stderr=output)
  subprocess.run(['git','add','--all'],cwd=source,check=True,stdout=output,stderr=output)
  subprocess.run(['git','-c','core.hooksPath=/dev/null','-c','commit.gpgsign=false','-c','user.name=Candidate validation','-c','user.email=candidate@localhost','commit','-q','-m','Validate client against unreleased Spawned candidate'],cwd=source,check=True,stdout=output,stderr=output)
  command=[str(tool/'cargo'),'check','--locked','--offline','--release','-p','ethlambda']
  metadata['command']=command
  result=subprocess.run(command,cwd=source,env=env,stdout=output,stderr=output)
  metadata.update(stage='client compile',returncode=result.returncode)
metadata['elapsed_seconds']=time.monotonic()-start
Path('/tmp/spawned-v07-client-check-result.json').write_text(json.dumps(metadata,indent=2)+'\n')
print(json.dumps(metadata,indent=2))
raise SystemExit(metadata['returncode'])
