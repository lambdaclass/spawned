#!/usr/bin/env python3
"""Qualification summary: usage analyze.py <qual_dir>. Safe to rerun at any time.

Writes results/SUMMARY.md and results/verdict.json (and results/supervision.csv)
and prints the summary. Exit status: 0 PASS, 1 FAIL, 2 INCOMPLETE (stopped early
or a check could not be judged).

The soak window starts SETTLE_S after the soak phase began and ends when the
plan started stopping nodes. Trends (OLS slopes) are judged only on windows of
at least MIN_TREND_S and only on the process that ran the whole window.
"""
import csv
import json
import os
from pathlib import Path
import re
import shutil
import statistics as st
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import clients  # noqa: E402

ANSI = re.compile(r"\x1b\[[0-9;]*m")


def num(v):
    try:
        return float(v)
    except Exception:
        return None


def read_jsonl(p):
    if not Path(p).exists():
        return []
    out = []
    for line in Path(p).read_text().splitlines():
        try:
            out.append(json.loads(line))
        except ValueError:
            pass
    return out


def read_csv(p):
    if not Path(p).exists():
        return []
    with open(p, newline="") as f:
        return list(csv.DictReader(f))


def slope(pts):
    """OLS slope per hour of [(epoch, value)]."""
    pts = [(t / 3600, v) for t, v in pts if v is not None]
    if len(pts) < 10:
        return None
    mx = st.fmean(p[0] for p in pts)
    my = st.fmean(p[1] for p in pts)
    sxx = sum((p[0] - mx) ** 2 for p in pts)
    return sum((p[0] - mx) * (p[1] - my) for p in pts) / sxx if sxx else 0.0


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, max(0, int(round(p * len(xs))) - 1))] if xs else None


def fmt(v, nd=1):
    if v is None:
        return "-"
    return f"{v:.{nd}f}" if isinstance(v, float) else str(v)


class Analysis:
    def __init__(self, qdir):
        self.q = Path(qdir).resolve()
        self.cfg = json.loads((self.q / "config.json").read_text())
        self.cfg["QUAL_DIR"] = str(self.q)
        self.R = self.q / "results"
        self.client = clients.load(self.cfg)
        self.paired = self.cfg["PAIRED"] not in ("", "0")
        self.labels = ["control", "candidate"] if self.paired else ["candidate"]
        self.manifest = json.loads((self.q / "manifest.json").read_text()) if (self.q / "manifest.json").exists() else {}
        self.events = read_jsonl(self.R / "events.jsonl")
        self.samples = read_csv(self.R / "samples.csv")
        self.host = read_csv(self.R / "host.csv")
        self.shutdowns = read_jsonl(self.R / "shutdown.jsonl")
        self.faults = read_jsonl(self.R / "faults.jsonl")
        self.outcome = (self.q / "run/outcome").read_text().strip() if (self.q / "run/outcome").exists() else "running"
        self.checks = []
        self.out = []

    def th(self, k):
        return float(self.cfg[k])

    def p(self, line=""):
        self.out.append(line)

    def check(self, name, status, detail):
        self.checks.append((name, status, detail))

    # ---- windows
    def phase_epoch(self, name):
        for e in self.events:
            if e.get("phase") == name:
                return e["epoch"]
        return None

    def window(self):
        soak = self.phase_epoch("soak")
        end = self.phase_epoch("stopping") or (max(int(r["epoch"]) for r in self.samples) if self.samples else None)
        if soak is None or end is None:
            return None, None
        return soak + float(self.cfg["SETTLE_S"]), end

    def rows(self, label, t0=None, t1=None):
        rs = [r for r in self.samples if r["label"] == label and r["pid"]]
        if t0 is not None:
            rs = [r for r in rs if t0 <= int(r["epoch"]) <= t1]
        return rs

    def soak_rows(self, label):
        """Samples of the one process that ran the soak window (the PID with most samples in it)."""
        t0, t1 = self.window()
        if t0 is None:
            return []
        rs = self.rows(label, t0, t1)
        if not rs:
            return []
        pids = {}
        for r in rs:
            pids[r["pid"]] = pids.get(r["pid"], 0) + 1
        pid = max(pids, key=pids.get)
        return [r for r in rs if r["pid"] == pid]

    # ---- stats
    def stats(self, label):
        rs = self.soak_rows(label)
        s = {"samples": len(rs)}
        if not rs:
            return s
        s["window_s"] = int(rs[-1]["epoch"]) - int(rs[0]["epoch"])
        s["pid"] = rs[0]["pid"]
        col = lambda k: [num(r[k]) for r in rs if num(r.get(k)) is not None]
        series = lambda k: [(int(r["epoch"]), num(r.get(k))) for r in rs]
        cpu = col("cpu_pct")
        if cpu:
            s.update(cpu_mean=st.fmean(cpu), cpu_p95=pct(cpu, .95))
        for k in ("rss_mb", "fds", "threads"):
            v = col(k)
            if v:
                n = max(1, len(v) // 10)
                s[k] = {"first10": st.fmean(v[:n]), "last10": st.fmean(v[-n:]), "max": max(v), "slope_h": slope(series(k))}
        peers = col("peers")
        if peers:
            s["peers"] = {"min": min(peers), "mean": st.fmean(peers), "last": peers[-1]}
        dist = col("sync_distance")
        if dist:
            lim = self.th("TH_SYNC_DISTANCE")
            s["sync"] = {"p50": st.median(dist), "p95": pct(dist, .95), "max": max(dist),
                         "synced_pct": 100 * sum(1 for d in dist if d <= lim) / len(rs)}
        head = [(t, v) for t, v in series("head") if v is not None]
        if len(head) >= 2:
            s["head"] = {"first": head[0][1], "last": head[-1][1], "per_h": (head[-1][1] - head[0][1]) / max(1, head[-1][0] - head[0][0]) * 3600}
        ok = [r.get("api_ok") for r in rs]
        s["api_ok_pct"] = 100 * ok.count("1") / len(ok)
        dd = col("datadir_mb")
        if dd:
            s["datadir_mb"] = (dd[0], dd[-1])
        for k in ("sp_messages_rejected_total", "sp_requests_timed_out_total", "sp_handlers_panicked_total",
                  "sp_messages_handled_total"):
            v = col(k)
            if v:
                s[k] = v[-1] - v[0]
        mailbox = col("sp_mailbox_depth")
        if mailbox:
            s["mailbox"] = {"mean": st.fmean(mailbox), "max": max(mailbox), "last": mailbox[-1]}
        actors = col("sp_actors_active")
        if actors:
            s["actors_active"] = {"first": actors[0], "last": actors[-1], "max": max(actors)}
        s["has_spawned_metrics"] = bool(col("sp_messages_handled_total") or col("sp_actors_active"))
        return s

    def fault_pid(self):
        for f in self.faults:
            if f.get("event") == "phase_start":
                return str(f["pid"])
        return None

    def per_pid_counter(self, label, key, exclude_pid=None):
        """Sum over processes of the counter's last value (counters start at zero with each process)."""
        last = {}
        for r in self.rows(label):
            v = num(r.get(key))
            if v is not None and r["pid"] != exclude_pid:
                last[r["pid"]] = max(v, last.get(r["pid"], 0))
        return sum(last.values()) if last else None

    # ---- logs
    def panics(self):
        """Panic lines per label, excluding the fault phase's process log from the phase start on."""
        excl = None
        for f in self.faults:
            if f.get("event") == "phase_start":
                excl = (f["log"], f["offset"])
        rx = re.compile(self.client.panic_re)
        out = {}
        for log in sorted((self.R / "logs").glob("*.log")):
            label = log.name.split(".")[0]
            with open(log, "rb") as fh:
                data = fh.read()
            if excl and log.name == excl[0]:
                data = data[:excl[1]]
            hits = [ANSI.sub("", l) for l in data.decode(errors="replace").splitlines() if rx.search(ANSI.sub("", l))]
            if hits:
                out.setdefault(label, []).append((log.name, len(hits), hits[0][:240]))
        return out

    def supervision(self):
        rules = [("inject", r"Fault injection: crashing"), ("restart_scheduled", r'event="?restart_scheduled'),
                 ("restart_started", r'event="?restart_started'), ("client_restarted", r"Supervisor restarted the"),
                 ("budget_exhausted", r'event="?restart_budget_exhausted'), ("give_up", r'event="?supervision_give_up'),
                 ("client_gave_up", r"supervisor gave up"), ("panic", r"Panic in message handler|panic in handler")]
        rules = [(e, re.compile(x)) for e, x in rules]
        kv = lambda k, s: (re.search(k + r"=([^\s,]+)", s) or [None, ""])[1].strip('"')
        rows = []
        for log in sorted((self.R / "logs").glob("*.log")):
            with open(log, errors="replace") as fh:
                for line in fh:
                    line = ANSI.sub("", line)
                    for ev, rx in rules:
                        if rx.search(line):
                            rows.append({"log": log.name, "event": ev, "child": kv("child", line),
                                         "generation": kv("generation", line), "line": line.strip()[:300]})
                            break
        with open(self.R / "supervision.csv", "w", newline="") as fh:
            w = csv.DictWriter(fh, fieldnames=["log", "event", "child", "generation", "line"])
            w.writeheader()
            w.writerows(rows)
        by = {}
        for r in rows:
            by.setdefault(r["log"], []).append(r)
        out = []
        for log, rs in sorted(by.items()):
            c = lambda e: sum(1 for r in rs if r["event"] == e)
            if not any(c(e) for e in ("inject", "restart_started", "client_restarted", "budget_exhausted", "give_up", "client_gave_up")):
                continue
            gens = [int(r["generation"]) for r in rs if r["generation"].isdigit()]
            out.append(f"{log}: injected={c('inject')} panics={c('panic')} restart_scheduled={c('restart_scheduled')} "
                       f"restart_started={c('restart_started')} client_restarted={c('client_restarted')} "
                       f"max_generation={max(gens) if gens else '-'} budget_exhausted={c('budget_exhausted')} "
                       f"give_up={c('give_up')} client_gave_up={c('client_gave_up')}")
        return out

    # ---- spawned metrics (long format)
    def spawned_long(self, label, pid_rows):
        if not pid_rows:
            return {}
        t0, t1 = int(pid_rows[0]["epoch"]), int(pid_rows[-1]["epoch"])
        m = {}
        path = self.R / "spawned_metrics.csv"
        if not path.exists():
            return m
        with open(path, newline="") as f:
            for r in csv.DictReader(f):
                if r["label"] == label and t0 <= int(r["ts"]) <= t1:
                    m.setdefault(r["metric"], []).append((int(r["ts"]), float(r["value"])))
        return m

    # ---- heap profiles
    def heap(self):
        P = self.R / "profiles"
        go = shutil.which(os.path.expanduser(self.cfg.get("GO", "go")))
        profs = {}
        for p in P.glob("heap-*.pb.gz"):
            m = re.match(r"heap-(control|candidate)-(\d+)h", p.name)
            if m:
                profs.setdefault(m.group(1), []).append((int(m.group(2)), p))
        if not profs:
            return
        self.p("## Heap profiles: growth by owner\n")
        if go is None:
            self.p(f"`{self.cfg.get('GO')}` not found; profiles are in `results/profiles/` "
                   "(`go tool pprof -base heap-<label>-00h.pb.gz <later>.pb.gz`).\n")
            return
        skip = re.compile(r"tikv-jemalloc-sys|tikv-jemallocator-|^__r(ust|g|dl)_|/rustc/[0-9a-f]+/library/|"
                          r"^\S+ (nptl|sysdeps|libio|malloc|stdlib|string)/|"
                          r"index\.crates\.io-[0-9a-f]+/(hashbrown|bytes|smallvec|indexmap)-|^\S+\s*$|\?\?")
        spawned = re.compile(r"/checkouts/spawned-[0-9a-f]+/[0-9a-f]+/|index\.crates\.io-[0-9a-f]+/spawned-|"
                             + re.escape(str(self.q / "src/spawned")) + "/")
        units = {"B": 1, "kB": 1e3, "KB": 1e3, "MB": 1e6, "GB": 1e9, "TB": 1e12}
        for label, ps in sorted(profs.items()):
            ps.sort()
            src = str(self.q / "src" / label) + "/"

            def owner(frames):
                for fr in frames:
                    if skip.search(fr):
                        continue
                    m = re.search(r" (\S+):\d+", fr)
                    path = m.group(1) if m else ""
                    if re.search(r"librocksdb-sys|/rocksdb/|/rust-rocksdb|index\.crates\.io-[0-9a-f]+/rocksdb-", path):
                        return "rocksdb"
                    if spawned.search(path):
                        return "spawned"
                    if path.startswith(src):
                        return self.cfg["CLIENT"]
                    m = re.search(r"index\.crates\.io-[0-9a-f]+/([A-Za-z0-9_\-]+?)-\d+\.\d+", path)
                    if m and m.group(1).startswith("tokio"):
                        return "tokio"
                    return "other"
                return "unknown"
            base_h, base = ps[0]
            self.p(f"### {label}: base `{base.name}`\n")
            self.p("| profile | net growth MB | rocksdb | " + self.cfg["CLIENT"] + " | spawned | tokio | other |")
            self.p("|---|---|---|---|---|---|---|")
            for h, cur in ps[1:]:
                out = subprocess.run([go, "tool", "pprof", "-symbolize=none", "-traces", "-lines",
                                      "-sample_index=inuse_space", "-base", str(base), str(cur)],
                                     capture_output=True, text=True, timeout=900).stdout
                grp, tot = {}, 0.0
                for blk in out.split("-----------+-------------------------------------------------------")[1:]:
                    lines = [l for l in blk.splitlines() if l.strip() and not l.strip().startswith("bytes:")]
                    if not lines:
                        continue
                    m = re.match(r"\s*(-?[\d.]+)([kKMGT]?B)\s+(.*)", lines[0])
                    if not m:
                        continue
                    val = float(m.group(1)) * units[m.group(2)]
                    g = owner([m.group(3).strip()] + [l.strip() for l in lines[1:]])
                    grp[g] = grp.get(g, 0) + val
                    tot += val
                cells = [grp.get(k, 0) / 1e6 for k in ("rocksdb", self.cfg["CLIENT"], "spawned", "tokio")]
                other = (tot - sum(grp.get(k, 0) for k in ("rocksdb", self.cfg["CLIENT"], "spawned", "tokio"))) / 1e6
                self.p(f"| {cur.name} | {tot / 1e6:+.1f} | " + " | ".join(f"{c:+.1f}" for c in cells) + f" | {other:+.1f} |")
            self.p("\nOwner = first frame outside the allocator, std and generic containers, by source path. "
                   "Sampled in-use bytes.\n")

    # ---- host memory
    def host_memory(self):
        t0, t1 = self.window()
        if t0 is None or not self.host:
            return
        hs = [r for r in self.host if t0 <= int(r["epoch"]) <= t1]
        if len(hs) < 2:
            return
        self.p("## Host memory over the soak window\n")
        self.p("| series | first | last | OLS slope MB/h |\n|---|---|---|---|")
        for k in ("host_used_mb", "host_used_classic_mb", "mi_Cached_mb", "mi_SUnreclaim_mb", "other_procs_anon_mb"):
            v = [(int(r["epoch"]), num(r.get(k))) for r in hs]
            vv = [x for _, x in v if x is not None]
            if vv:
                self.p(f"| {k} | {vv[0]:.0f} | {vv[-1]:.0f} | {fmt(slope(v))} |")
        for label in self.labels:
            rs = self.soak_rows(label)
            for k in ("rss_anon_mb", "cl_rss_anon_mb", "je_allocated_mb", "je_resident_mb", "je_retained_mb",
                      "rdb_block_cache_mb", "rdb_memtables_mb"):
                v = [(int(r["epoch"]), num(r.get(k))) for r in rs]
                vv = [x for _, x in v if x is not None]
                if vv:
                    self.p(f"| {label} {k} | {vv[0]:.0f} | {vv[-1]:.0f} | {fmt(slope(v))} |")
        self.p("\nThe host may be shared: `other_procs_anon_mb` is every process except this run's nodes, so "
               "host growth can be split into nodes, other processes and kernel/page cache.\n")

    # ---- main
    def run(self):
        cfg = self.cfg
        t0, t1 = self.window()
        st_ = {label: self.stats(label) for label in self.labels}
        cand, ctrl = st_["candidate"], st_.get("control")
        trend_ok = t0 is not None and cand.get("window_s", 0) >= float(cfg["MIN_TREND_S"])
        # Missing soak data fails a finished run; mid-run it is only pending.
        missing = "FAIL" if t0 is not None and self.outcome != "running" else "SKIP"

        # 1. single spawned version
        sp = self.manifest.get("labels", {}).get("candidate", {}).get("spawned")
        if sp is None:
            self.check("single spawned version", "FAIL", "no build manifest")
        else:
            self.check("single spawned version", "PASS" if sp["ok"] else "FAIL",
                       ", ".join(sp["binary"]) + ("" if sp["ok"] else " — " + "; ".join(sp["problems"])))
        # 2. bounded shutdowns
        cs = [s for s in self.shutdowns if s["label"] == "candidate"]
        slow = [s for s in cs if s["timed_out"]]
        self.check("bounded shutdown", "FAIL" if slow else ("PASS" if cs else "SKIP"),
                   f"{len(cs)} candidate stops, max {max((s['elapsed_s'] for s in cs), default=0):.2f} s, "
                   f"{len(slow)} over STOP_TIMEOUT_S={cfg['STOP_TIMEOUT_S']} s")
        # 3. faults
        inj = [f for f in self.faults if f.get("event") == "inject"]
        if not inj:
            self.check("fault injection", "SKIP", "not run (FAULTS=0 or no hook in the candidate)")
        else:
            missed = [f for f in inj if not f["observed"] or f.get("recovered") is False]
            self.check("fault injection", "FAIL" if missed else "PASS",
                       f"{len(inj)} injected, {len(inj) - len(missed)} observed as planned "
                       f"({sum(1 for f in inj if f['expect'] == 'restart')} restarts, "
                       f"{sum(1 for f in inj if f['expect'] == 'give_up')} give-up)")
        # 4. panics outside faults
        pan = self.panics()
        cp = sum(n for k, v in pan.items() if k == "candidate" for _, n, _ in v)
        self.check("panics outside injected faults", "PASS" if cp <= self.th("TH_PANICS") else "FAIL",
                   f"candidate {cp} panic lines; control "
                   f"{sum(n for k, v in pan.items() if k == 'control' for _, n, _ in v)} (TH_PANICS={cfg['TH_PANICS']})")
        # 5. unexpected exits
        ue = [e for e in self.events if e.get("unexpected_exit")]
        cue = [e for e in ue if e["label"].startswith("candidate")]
        self.check("no unexpected exits", "FAIL" if cue else "PASS",
                   f"candidate {len(cue)}, control {len(ue) - len(cue)}")
        # 6. RSS drift
        def trend_check(name, key, abs_th, margin_th, unit):
            c = (cand.get(key) or {}).get("slope_h")
            k = (ctrl.get(key) or {}).get("slope_h") if ctrl else None
            if c is None or not trend_ok:
                self.check(name, "SKIP", f"window {cand.get('window_s', 0) / 3600:.1f} h < MIN_TREND_S or no data; "
                                         f"candidate slope {fmt(c, 2)} {unit}")
                return
            if k is not None:
                ok = c - k <= self.th(margin_th) or c <= 0
                self.check(name, "PASS" if ok else "FAIL",
                           f"candidate {c:+.2f} {unit}, control {k:+.2f} {unit}; margin {margin_th}={cfg[margin_th]}")
            else:
                self.check(name, "PASS" if c <= self.th(abs_th) else "FAIL", f"candidate {c:+.2f} {unit}; {abs_th}={cfg[abs_th]}")
        trend_check("RSS drift", "rss_mb", "TH_RSS_SLOPE_MB_H", "TH_RSS_SLOPE_MARGIN_MB_H", "MB/h")
        trend_check("fd slope", "fds", "TH_FD_SLOPE_H", "TH_FD_SLOPE_MARGIN_H", "fds/h")
        # 7. sync
        cs_ = cand.get("sync")
        if not cs_:
            self.check("sync distance", missing, "no sync samples in the soak window")
        else:
            ks = (ctrl or {}).get("sync")
            ok = cs_["synced_pct"] >= self.th("TH_SYNCED_PCT") or (
                ks is not None and cs_["synced_pct"] >= ks["synced_pct"] - self.th("TH_SYNCED_MARGIN_PCT"))
            self.check("sync distance", "PASS" if ok else "FAIL",
                       f"candidate {cs_['synced_pct']:.1f}% of samples within {cfg['TH_SYNC_DISTANCE']} (p95 {cs_['p95']:.0f}, "
                       f"max {cs_['max']:.0f})" + (f"; control {ks['synced_pct']:.1f}%" if ks else "")
                       + f"; TH_SYNCED_PCT={cfg['TH_SYNCED_PCT']}")
        # 8. API health
        if cand.get("samples"):
            self.check("API answers", "PASS" if cand["api_ok_pct"] >= self.th("TH_API_OK_PCT") else "FAIL",
                       f"candidate {cand['api_ok_pct']:.1f}%" + (f", control {ctrl['api_ok_pct']:.1f}%" if ctrl and ctrl.get("samples") else ""))
        # 9. spawned counters
        if not cand.get("has_spawned_metrics"):
            self.check("spawned counters", missing,
                       "no spawned_* metrics scraped from the candidate (build without the `metrics` feature?)")
        else:
            fp = self.fault_pid()
            rej = self.per_pid_counter("candidate", "sp_messages_rejected_total")
            hp = self.per_pid_counter("candidate", "sp_handlers_panicked_total", exclude_pid=fp)
            to = cand.get("sp_requests_timed_out_total")
            bad = []
            if rej is not None and rej > self.th("TH_REJECTED"):
                bad.append("rejected")
            if hp is not None and hp > self.th("TH_HANDLER_PANICS"):
                bad.append("handler panics")
            if to is not None and to > self.th("TH_TIMEOUTS"):
                bad.append("timeouts")
            self.check("spawned counters", "FAIL" if bad else "PASS",
                       f"messages_rejected {fmt(rej, 0)} (whole run), handlers_panicked {fmt(hp, 0)} (whole run, "
                       f"fault process excluded), requests_timed_out {fmt(to, 0)} (soak window)")
        # 10. completion
        complete = self.outcome == "complete"
        self.check("run completed", "PASS" if complete else "INCOMPLETE", self.outcome)

        statuses = [s for _, s, _ in self.checks]
        verdict = "FAIL" if "FAIL" in statuses else ("INCOMPLETE" if "INCOMPLETE" in statuses or self.outcome == "running" else "PASS")

        # ---- report
        m = self.manifest.get("labels", {})
        self.p(f"# Qualification: {cfg['CLIENT']} — {verdict}\n")
        if cfg.get("PRESET") == "smoke":
            self.p("**Smoke preset: every phase, shortened. A harness check, not a release qualification.**\n")
        self.p(f"- run: `{self.q.name}`, preset `{cfg.get('PRESET') or '-'}`, paired: {'yes' if self.paired else 'no'}, outcome: {self.outcome}")
        for label in self.labels:
            e = m.get(label, {})
            self.p(f"- {label}: {e.get('ref', 'prebuilt')} `{e.get('commit', '-')[:12]}`, spawned {', '.join(e.get('spawned', {}).get('binary', [])) or '-'}"
                   + (f" (patched to spawned `{e['spawned_rev'][:12]}`)" if e.get("spawned_rev") else "")
                   + f", sha256 `{e.get('sha256', '-')[:16]}`")
        if t0:
            self.p(f"- soak window: {(t1 - t0) / 3600:.2f} h (SETTLE_S={cfg['SETTLE_S']} skipped after the soak began)")
        self.p("")
        self.p("## Checks\n\n| check | result | detail |\n|---|---|---|")
        for name, status, detail in self.checks:
            self.p(f"| {name} | **{status}** | {detail} |")
        self.p("\n## Soak window, per node\n")
        self.p("| | " + " | ".join(self.labels) + " |\n|---|" + "---|" * len(self.labels))

        def row(name, f):
            self.p(f"| {name} | " + " | ".join(f(st_[label]) for label in self.labels) + " |")
        row("samples / pid", lambda s: f"{s.get('samples', 0)} / {s.get('pid', '-')}")
        row("CPU % mean / p95", lambda s: f"{fmt(s.get('cpu_mean'))} / {fmt(s.get('cpu_p95'))}")
        for k, u in (("rss_mb", "MB"), ("fds", ""), ("threads", "")):
            row(f"{k} first10% → last10% (max)", lambda s, k=k: f"{fmt((s.get(k) or {}).get('first10'), 0)} → {fmt((s.get(k) or {}).get('last10'), 0)} ({fmt((s.get(k) or {}).get('max'), 0)})")
            row(f"{k} slope /h", lambda s, k=k: fmt((s.get(k) or {}).get("slope_h"), 2))
        row("peers min / mean / last", lambda s: "/".join(fmt(x, 0) for x in ((s.get("peers") or {}).get(k) for k in ("min", "mean", "last"))))
        row("head first → last (per h)", lambda s: f"{fmt((s.get('head') or {}).get('first'), 0)} → {fmt((s.get('head') or {}).get('last'), 0)} ({fmt((s.get('head') or {}).get('per_h'), 0)})")
        row("sync distance p50 / p95 / max", lambda s: "/".join(fmt(x, 0) for x in ((s.get("sync") or {}).get(k) for k in ("p50", "p95", "max"))))
        row("API ok %", lambda s: fmt(s.get("api_ok_pct")))
        row("datadir MB", lambda s: f"{fmt((s.get('datadir_mb') or (None, None))[0], 0)} → {fmt((s.get('datadir_mb') or (None, None))[1], 0)}")
        row("spawned messages handled", lambda s: fmt(s.get("sp_messages_handled_total"), 0))
        row("spawned mailbox depth mean / max", lambda s: f"{fmt((s.get('mailbox') or {}).get('mean'))} / {fmt((s.get('mailbox') or {}).get('max'), 0)}")
        row("spawned actors active first → last", lambda s: f"{fmt((s.get('actors_active') or {}).get('first'), 0)} → {fmt((s.get('actors_active') or {}).get('last'), 0)}")
        self.p("")

        # spawned metric detail (candidate, soak process)
        long = self.spawned_long("candidate", self.soak_rows("candidate"))
        if long:
            self.p("## spawned metrics, candidate soak process\n")
            self.p("| metric | first | last | max |\n|---|---|---|---|")
            for k in sorted(long):
                if "_bucket" in k or k.endswith(("_sum", "_count")):
                    continue
                v = [x for _, x in long[k]]
                self.p(f"| {k} | {v[0]:g} | {v[-1]:g} | {max(v):g} |")
            for h in ("queue_wait_seconds", "handler_duration_seconds", "request_latency_seconds"):
                base = "spawned_" + h
                bk = sorted((k for k in long if k.startswith(base + "_bucket")),
                            key=lambda k: float("inf") if "+Inf" in k else float(k.split('le="')[1].split('"')[0]))
                if not bk or base + "_count" not in long:
                    continue
                d = [(k, long[k][-1][1] - long[k][0][1]) for k in bk]
                tot = d[-1][1]

                def q(p):
                    for k, c in d:
                        if c >= p * tot:
                            return k.split('le="')[1].split('"')[0]
                cnt = long[base + "_count"][-1][1] - long[base + "_count"][0][1]
                self.p(f"\n{h}: n={cnt:.0f} p50<={q(.5)}s p99<={q(.99)}s p99.9<={q(.999)}s")
            self.p("")

        self.p("## Shutdowns\n\n| utc | process | signal | reason | elapsed s | logged ms | deadline exceeded | rc | timed out |\n|---|---|---|---|---|---|---|---|---|")
        for s in self.shutdowns:
            self.p(f"| {s['utc']} | {s['label']} | {s['signal']} | {s.get('reason', '')} | {s['elapsed_s']:.2f} | "
                   f"{fmt(s.get('logged_elapsed_ms'))} | {s.get('deadline_exceeded', 0)} | {fmt(s.get('rc'))} | {s['timed_out']} |")
        self.p("")
        if inj:
            self.p("## Fault injection\n\n| attempt | expected | observed | s | recovered |\n|---|---|---|---|---|")
            for f in inj:
                self.p(f"| {f['attempt']} | {f['expect']} | {f['observed']} | {f['elapsed_s']:.2f} | {f.get('recovered', '-')} |")
            self.p("")
        sup = self.supervision()
        if sup:
            self.p("## Supervision events (from logs)\n\n```\n" + "\n".join(sup) + "\n```\n")
        if pan:
            self.p("## Panic lines outside injected faults\n")
            for label, v in sorted(pan.items()):
                for log, n, first in v:
                    self.p(f"- {log}: {n} — `{first}`")
            self.p("")
        self.host_memory()
        self.heap()

        text = "\n".join(self.out) + "\n"
        (self.R / "SUMMARY.md").write_text(text)
        (self.R / "verdict.json").write_text(json.dumps(
            {"verdict": verdict, "outcome": self.outcome,
             "checks": [{"check": n, "result": s, "detail": d} for n, s, d in self.checks]}, indent=1) + "\n")
        print(text)
        return {"PASS": 0, "FAIL": 1}.get(verdict, 2)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(2)
    sys.exit(Analysis(sys.argv[1]).run())
