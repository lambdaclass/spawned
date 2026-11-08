import os,subprocess,sys,json,time
from pathlib import Path
root=Path('/tmp/spawned-system-20261108');os.chdir(root)
env=os.environ.copy();env.update(CARGO_BUILD_JOBS='1',RUST_TEST_THREADS='4',CARGO_TARGET_DIR='/tmp/spawned-pr-security-review/system-target')
commands=[
 ('default-tests',['cargo','test','--locked','--workspace']),
 ('allfeatures-tests',['cargo','test','--locked','--workspace','--all-features']),
 ('clippy',['cargo','clippy','--locked','--workspace','--all-targets','--','-D','warnings']),
 ('clippy-allfeatures',['cargo','clippy','--locked','--workspace','--all-targets','--all-features','--','-D','warnings']),
]
for binary in ['execution_groups','supervision_recovery','dedicated_thread','runtime_isolation','byte_budget_pipeline','timeout_outcomes','graceful_drain','framework_metrics']:
 cmd=['cargo','run','--locked','-p','spawned-capability-examples','--bin',binary]
 if binary=='framework_metrics':cmd+=['--features','metrics']
 commands.append(('example-'+binary,cmd))
results=json.loads(Path('/tmp/spawned-system-validation.json').read_text())[:2]
for label,cmd in commands[2:]:
 selected_env = env.copy()
 if cmd[1] == 'clippy':
  selected_env.update(RUSTC='/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustc',RUSTDOC='/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustdoc',CARGO_TARGET_DIR='/tmp/spawned-pr-security-review/system-clippy198-target', PATH='/nix/store/n88pdyarvdpyw98fahxcb03733nsclhq-clippy-1.98.1/bin:'+env['PATH'])
 log=Path('/tmp/spawned-system-final-'+label+'.log'); print('START',label,flush=True)
 with log.open('w') as output:
  result=subprocess.run(['python3','/tmp/spawned-system-guard.py','taskset','-c','2,3,4,5','nice','-n','10',*cmd],env=selected_env,stdout=output,stderr=subprocess.STDOUT)
 results.append({'gate':label,'exit':result.returncode,'command':cmd,'log':str(log)})
 Path('/tmp/spawned-system-validation.json').write_text(json.dumps(results,indent=2)+'\n')
 print('FINISH',label,'exit',result.returncode,flush=True)
 if result.returncode:
  print(log.read_text()[-10000:],flush=True);sys.exit(result.returncode)
print('ALL GATES PASS',flush=True)
