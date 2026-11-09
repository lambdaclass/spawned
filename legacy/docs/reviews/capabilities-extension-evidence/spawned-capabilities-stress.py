import os,subprocess,json,time
os.chdir('/tmp/spawned-capabilities-20261108')
os.environ.update(CARGO_PROFILE_TEST_DEBUG='1',CARGO_PROFILE_DEV_DEBUG='1')
os.environ.update(CARGO_BUILD_JOBS='1',RUST_TEST_THREADS='4',CARGO_TARGET_DIR='/tmp/spawned-pr-security-review/system-target')
rows=[]
for i in range(20):
    cmd=['cargo','test','--locked','-p','spawned-concurrency','--features','lifecycle-trace','--test','supervision','--test','supervision_strategies','--test','nested_supervision','--test','execution_profiles']
    log=f'/tmp/spawned-capabilities-stress-{i+1}.log';began=time.monotonic()
    with open(log,'w') as f:p=subprocess.run(['taskset','-c','2,3,4,5','nice','-n','10','python3','/tmp/spawned-system-guard.py',*cmd],stdout=f,stderr=subprocess.STDOUT)
    rows.append(dict(round=i+1,code=p.returncode,seconds=time.monotonic()-began,log=log))
    open('/tmp/spawned-capabilities-stress.json','w').write(json.dumps(rows,indent=2));print('stress',i+1,p.returncode,flush=True)
    if p.returncode:print(open(log).read()[-15000:]);raise SystemExit(p.returncode)
