import os, subprocess, json, time
base='/tmp/spawned-capabilities-20261108'
os.chdir(base)
os.environ.update(CARGO_PROFILE_TEST_DEBUG="1",CARGO_PROFILE_DEV_DEBUG="1")
os.environ.update(CARGO_BUILD_JOBS='1', RUST_TEST_THREADS='4', CARGO_TARGET_DIR='/tmp/spawned-pr-security-review/system-target')
checks=[('default', ['cargo','test','--locked','--workspace']),('allfeatures',['cargo','test','--locked','--workspace','--all-features']),('native-rt',['cargo','test','--locked','-p','spawned-rt','--no-default-features']),('native-bare',['cargo','test','--locked','-p','spawned-concurrency','--no-default-features']),('native-metrics',['cargo','test','--locked','-p','spawned-concurrency','--no-default-features','--features','threads,metrics']),('fixture',['cargo','run','--manifest-path','examples/thread_only/Cargo.toml','--features','metrics,async']),('graph',['python3','scripts/check-thread-only.py']),('docs',['cargo','doc','--locked','-p','spawned-rt','-p','spawned-concurrency','--no-deps']),('native-docs',['cargo','doc','--locked','-p','spawned-rt','-p','spawned-concurrency','--no-default-features','--features','spawned-concurrency/metrics','--no-deps'])]
for example in ['execution_groups','execution_profiles','supervision_recovery','supervision_tree','dedicated_thread','runtime_isolation','byte_budget_pipeline','timeout_outcomes','graceful_drain']:
    checks.append(('example-'+example,['cargo','run','--locked','-p','spawned-capability-examples','--bin',example]))
checks.append(('example-framework_metrics',['cargo','run','--locked','-p','spawned-capability-examples','--features','metrics','--bin','framework_metrics']))
results=[]
for name, cmd in checks:
    print('RUN',name,flush=True); began=time.monotonic()
    env=os.environ.copy()
    if 'docs' in name: env['RUSTDOCFLAGS']='-D warnings'
    log='/tmp/spawned-capabilities-'+name+'.log'
    with open(log,'w') as f:
        p=subprocess.run(['taskset','-c','2,3,4,5','nice','-n','10','python3','/tmp/spawned-system-guard.py',*cmd], stdout=f,stderr=subprocess.STDOUT,env=env)
    results.append(dict(name=name,command=cmd,code=p.returncode,seconds=time.monotonic()-began,log=log))
    open('/tmp/spawned-capabilities-validation.json','w').write(json.dumps(results,indent=2))
    print(name,p.returncode,flush=True)
    if p.returncode:
        print(open(log).read()[-14000:],flush=True); raise SystemExit(p.returncode)
