import os,subprocess,json
from pathlib import Path
os.chdir('/tmp/spawned-system-20261108');env=os.environ.copy();env.update(CARGO_BUILD_JOBS='1',RUST_TEST_THREADS='4',CARGO_TARGET_DIR='/tmp/spawned-pr-security-review/system-target')
commands=[('final-test-style',['cargo','test','--locked','-p','spawned-concurrency','--test','execution_groups'])]
for i in range(20):
 commands.append(('race-'+str(i+1),['cargo','test','--locked','-p','spawned-concurrency','--test','supervision','repeated_crash_producer_and_shutdown_races_never_overlap_generations','--','--exact']))
results=[]
for label,cmd in commands:
 log=Path('/tmp/spawned-system-'+label+'.log')
 with log.open('w') as out:
  done=subprocess.run(['python3','/tmp/spawned-system-guard.py','taskset','-c','2,3,4,5','nice','-n','10',*cmd],env=env,stdout=out,stderr=subprocess.STDOUT)
 assert done.returncode==0,log.read_text()
 results.append({'gate':label,'command':cmd,'exit':0,'log':str(log)})
 print(label,'PASS',flush=True)
Path('/tmp/spawned-system-stress.json').write_text(json.dumps({'process_repeats':20,'iterations_per_repeat':32,'total_race_iterations':640,'producer_tasks_per_iteration':8,'request_attempts_per_producer':16,'schedules':'OS/Tokio scheduling; not seeded deterministic delivery','results':results},indent=2)+'\n')
