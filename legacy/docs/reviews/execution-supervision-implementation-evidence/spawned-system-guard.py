import os, signal, subprocess, sys, time
limit = 2 * 1024 * 1024 # KiB RSS summed across isolated process group
start = time.monotonic()
p = subprocess.Popen(sys.argv[1:], start_new_session=True)
peak = 0
while p.poll() is None:
    rows = subprocess.check_output(['ps','-eo','pgid=,rss='], text=True).splitlines()
    rss = sum(int(r[1]) for row in rows if len(r:=row.split()) == 2 and int(r[0]) == p.pid)
    peak = max(peak,rss)
    if rss > limit or time.monotonic() - start > 1800:
        print('RESOURCE GUARD EXCEEDED',rss,flush=True)
        os.killpg(p.pid, signal.SIGTERM)
        try: p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid,signal.SIGKILL); p.wait()
        sys.exit(125)
    time.sleep(0.5)
print('RESOURCE GUARD peak_rss_kib=',peak,'exit=',p.returncode,flush=True)
sys.exit(p.returncode)
