#!/usr/bin/env python3
"""Qualification sampler: usage sampler.py <qual_dir>. Runs until killed.

Every SAMPLE_S seconds, per node (control, candidate):
  results/samples.csv          process CPU, RSS (anon/file), threads, fds; head, sync
                               distance, peers, API health; datadir size; the paired
                               consensus client (ethrex); jemalloc and RocksDB memory
                               (ethrex, when exported); core spawned metrics
  results/spawned_metrics.csv  every spawned_* series, long format (ts,label,metric,value)
  results/host.csv             host RAM (MemTotal - MemAvailable, the operator metric),
                               /proc/meminfo fields, anon RSS of every other process
  results/metrics/<label>-<utc>.prom   the full /metrics text every METRICS_SNAPSHOT_S
"""
import csv
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import clients  # noqa: E402

Q = Path(sys.argv[1]).resolve()
CFG = json.loads((Q / "config.json").read_text())
CFG["QUAL_DIR"] = str(Q)
CLIENT = clients.load(CFG)
R = Q / "results"
LABELS = ["control", "candidate"] if CFG["PAIRED"] not in ("", "0") else ["candidate"]
PERIOD = float(CFG["SAMPLE_S"])
SNAP = float(CFG["METRICS_SNAPSHOT_S"])
TCK = os.sysconf("SC_CLK_TCK")
BIN = {str(p.resolve()) for p in (Q / "bin").iterdir()}

SP = ["actors_active", "actors_spawned_total", "actors_exited_panic_total", "mailbox_depth",
      "messages_handled_total", "messages_rejected_total", "messages_discarded_total",
      "handlers_active", "handlers_panicked_total", "requests_active", "requests_timed_out_total",
      "requests_actor_stopped_total", "reply_receivers_dropped_total"]
JE = ["allocated", "active", "metadata", "resident", "mapped", "retained"]
FIELDS = (["ts", "epoch", "phase", "label", "pid", "uptime_s", "cpu_pct", "rss_mb", "rss_anon_mb", "rss_file_mb",
           "threads", "fds", "api_ok", "head", "head_age_s", "sync_distance", "syncing", "peers", "datadir_mb",
           "cl_pid", "cl_cpu_pct", "cl_rss_mb", "cl_rss_anon_mb", "cl_head_slot", "cl_syncing", "metrics_ok"]
          + ["je_" + k + "_mb" for k in JE] + ["rdb_block_cache_mb", "rdb_memtables_mb"]
          + ["sp_" + k for k in SP])
MI = ["MemTotal", "MemFree", "MemAvailable", "Buffers", "Cached", "Shmem", "SReclaimable", "SUnreclaim",
      "AnonPages", "PageTables", "KernelStack"]
HOST = (["ts", "epoch", "host_used_mb", "host_used_classic_mb"] + ["mi_" + k + "_mb" for k in MI]
        + ["other_procs_anon_mb", "other_procs_top", "disk_free_gb", "load1"])

prev = {}
du_cache = {}


def rd(p):
    try:
        return Path(p).read_text().strip()
    except Exception:
        return ""


def mb(v):
    return round(v / 1048576, 1)


def live_pid(proc_label):
    try:
        pid = int(rd(Q / "run" / f"{proc_label}.pid"))
        if os.readlink(f"/proc/{pid}/exe").removesuffix(" (deleted)") in BIN:
            return pid
    except Exception:
        pass
    return None


def proc(pid, key, now):
    if pid is None:
        return {}
    try:
        st = rd(f"/proc/{pid}/stat").rsplit(")", 1)[1].split()
        ticks = int(st[11]) + int(st[12])
        r = {"uptime_s": int(float(rd("/proc/uptime").split()[0]) - int(st[19]) / TCK)}
        if key in prev and prev[key][0] == pid and now > prev[key][1]:
            r["cpu_pct"] = round(100.0 * (ticks - prev[key][2]) / TCK / (now - prev[key][1]), 1)
        prev[key] = (pid, now, ticks)
        s = dict(l.split(":", 1) for l in rd(f"/proc/{pid}/status").splitlines() if ":" in l)
        kb = lambda k: int(s.get(k, "0 kB").split()[0]) * 1024
        r.update(rss_mb=mb(kb("VmRSS")), rss_anon_mb=mb(kb("RssAnon")), rss_file_mb=mb(kb("RssFile")),
                 threads=s.get("Threads", "").strip(), fds=len(os.listdir(f"/proc/{pid}/fd")))
        return r
    except Exception:
        return {}


def du_mb(path, now):
    c = du_cache.get(path)
    if c and now - c[0] < 600:
        return c[1]
    try:
        out = subprocess.run(["du", "-sb", path], capture_output=True, text=True, timeout=300).stdout
        v = round(int(out.split()[0]) / 1e6, 1)
    except Exception:
        v = ""
    du_cache[path] = (now, v)
    return v


def scrape(label, now, ts):
    raw = clients.http_get(CLIENT.metrics_url(label), timeout=10)
    if raw is None:
        return None
    text = raw.decode(errors="replace")
    if now - prev.get("snap-" + label, 0) >= SNAP:
        prev["snap-" + label] = now
        (R / "metrics" / f"{label}-{time.strftime('%Y%m%dT%H%M%S', time.gmtime(now))}.prom").write_text(text)
    vals = {}
    with open(R / "spawned_metrics.csv", "a", newline="") as f:
        w = csv.writer(f)
        for line in text.splitlines():
            if not line or line[0] == "#":
                continue
            k, _, v = line.rpartition(" ")
            try:
                fv = float(v)
            except ValueError:
                continue
            if k.startswith("spawned_"):
                w.writerow([ts, label, k, v])
            vals[k] = vals.get(k, 0.0) + fv if k.startswith(("ethrex_db_cf_memtable_bytes",)) else fv
    memt = sum(v for k, v in vals.items() if k.startswith("ethrex_db_cf_memtable_bytes"))
    vals["_memtables"] = memt
    vals["_text"] = text
    return vals


def jemalloc(label, now):
    p = R / "profiles" / f"jemalloc-{label}.csv"
    try:
        with open(p, "rb") as f:
            f.seek(0, 2)
            f.seek(max(0, f.tell() - 4096))
            last = [l for l in f.read().decode().splitlines() if l and l[0].isdigit()][-1].split(",")
        if now - int(last[0]) > 300:
            return {}
        return {"je_" + k + "_mb": (mb(int(v)) if v else "") for k, v in zip(JE, last[1:])}
    except Exception:
        return {}


def host(now, ts, skip):
    d = {}
    for l in rd("/proc/meminfo").splitlines():
        k, _, v = l.partition(":")
        try:
            d[k] = int(v.split()[0]) * 1024
        except Exception:
            pass
    r = {"ts": ts, "epoch": int(now)}
    r.update({"mi_" + k + "_mb": mb(d[k]) for k in MI if k in d})
    try:
        r["host_used_mb"] = mb(d["MemTotal"] - d["MemAvailable"])
        r["host_used_classic_mb"] = mb(d["MemTotal"] - d["MemFree"] - d["Buffers"] - d["Cached"] - d["SReclaimable"])
    except KeyError:
        pass
    tot, by = 0, {}
    for p in os.listdir("/proc"):
        if not p.isdigit() or p in skip:
            continue
        name, anon = "", 0
        for l in rd(f"/proc/{p}/status").splitlines():
            if l.startswith("Name:"):
                name = l.split(None, 1)[1] if len(l.split()) > 1 else ""
            elif l.startswith("RssAnon:"):
                anon = int(l.split()[1]) * 1024
                break
        tot += anon
        by[name] = by.get(name, 0) + anon
    r["other_procs_anon_mb"] = mb(tot)
    r["other_procs_top"] = ";".join(f"{n.replace(',', '_')}:{mb(v):.0f}" for n, v in sorted(by.items(), key=lambda x: -x[1])[:4])
    r["disk_free_gb"] = round(shutil.disk_usage(Q).free / 1e9, 1)
    r["load1"] = os.getloadavg()[0]
    return r


def writer(path, fields):
    new = not path.exists() or path.stat().st_size == 0
    f = open(path, "a", newline="")
    w = csv.DictWriter(f, fields, extrasaction="ignore")
    if new:
        w.writeheader()
    return f, w


def main():
    (R / "metrics").mkdir(parents=True, exist_ok=True)
    if not (R / "spawned_metrics.csv").exists():
        (R / "spawned_metrics.csv").write_text("ts,label,metric,value\n")
    sf, sw = writer(R / "samples.csv", FIELDS)
    hf, hw = writer(R / "host.csv", HOST)
    while True:
        now = time.time()
        ts = int(now)
        phase = rd(Q / "run/phase")
        ours = set()
        for label in LABELS:
            pid = live_pid(label)
            cl_pid = live_pid(label + "-cl")
            ours |= {str(p) for p in (pid, cl_pid) if p}
            row = {"ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now)), "epoch": ts, "phase": phase,
                   "label": label, "pid": pid or ""}
            row.update(proc(pid, label, now))
            if cl_pid:
                c = proc(cl_pid, label + "-cl", now)
                row.update(cl_pid=cl_pid, cl_cpu_pct=c.get("cpu_pct", ""), cl_rss_mb=c.get("rss_mb", ""),
                           cl_rss_anon_mb=c.get("rss_anon_mb", ""))
            if pid:
                try:
                    row.update(CLIENT.probe(label))
                except Exception:
                    row["api_ok"] = 0
                m = scrape(label, now, ts)
                row["metrics_ok"] = 1 if m is not None else 0
                if m:
                    for k in SP:
                        v = m.get("spawned_" + k)
                        row["sp_" + k] = "" if v is None else (int(v) if v == int(v) else v)
                    if row.get("peers", "") == "":
                        p = CLIENT.peers_from_metrics(m["_text"])
                        if p is not None:
                            row["peers"] = p
                    bc = m.get("ethrex_db_block_cache_usage_bytes")
                    if bc is not None:
                        row["rdb_block_cache_mb"] = mb(bc)
                        row["rdb_memtables_mb"] = mb(m["_memtables"])
                row.update(jemalloc(label, now))
            row["datadir_mb"] = du_mb(str(Path(CLIENT.data_dir(label))), now)
            sw.writerow(row)
        hw.writerow(host(now, time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now)), ours))
        sf.flush()
        hf.flush()
        time.sleep(max(1, PERIOD - (time.time() - now)))


if __name__ == "__main__":
    main()
