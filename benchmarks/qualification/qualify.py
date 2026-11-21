#!/usr/bin/env python3
"""Release qualification: run a client built on a spawned candidate next to the
client's main on one host, and judge the result against fixed thresholds.

    qualify.py init    DIR [--config FILE] [KEY=VALUE ...]   write DIR/config.json
    qualify.py build   DIR      clone, build candidate (+ control), check spawned versions
    qualify.py launch  DIR      preflight, then start the controller detached (setsid nohup)
    qualify.py run     DIR      the controller in the foreground (launch uses this)
    qualify.py status  DIR
    qualify.py stop    DIR      bounded stop: STOP file, wait for the controller
    qualify.py analyze DIR      (re)write results/SUMMARY.md; exit 0 pass, 1 fail, 2 incomplete
    qualify.py check-binary BIN [--lock Cargo.lock] [--expect VERSION]

Every run is started by hand. Nothing here installs cron, systemd or any other
schedule. Nodes run in a tmux session named after DIR; the controller runs
outside it. Configuration is env/KEY=VALUE only (see DEFAULTS and PRESETS);
host paths, secrets and node keys never live in this directory: jwt secrets and
node keys are generated per run under DIR/secrets (mode 0600) unless given.

The harness never deletes or regenerates chain data. Data directories default to
fresh ones under DIR/data; an existing synced directory may be passed in
DATA_CONTROL / DATA_CANDIDATE and is used as is.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import clients  # noqa: E402

REPO_ROOT = HERE.parent.parent
COMPAT_PATCH = REPO_ROOT / ".github/scripts/client-compat-patch.sh"

DEFAULTS = {
    # What to build
    "CLIENT": "ethlambda",                 # ethlambda | ethrex
    "CLIENT_REPO": "",                     # default https://github.com/lambdaclass/<CLIENT>.git
    "CANDIDATE_REF": "",                   # commit, branch or pull/<N>/head
    "CONTROL_REF": "main",
    "SPAWNED_REPO": "https://github.com/lambdaclass/spawned.git",
    "SPAWNED_REV": "",                     # empty: the spawned the candidate's Cargo.lock pins
    "EXPECT_SPAWNED": "",                  # optional crates.io version the candidate must contain
    "CANDIDATE_BIN": "", "CONTROL_BIN": "",    # prebuilt binaries instead of building
    "CANDIDATE_LOCK": "", "CONTROL_LOCK": "",  # their Cargo.lock files, for the lock check
    "SPAWNED_CHECK": "fail",               # fail | warn: a second spawned in the candidate binary
    "TOOLCHAIN": "",                       # rustup toolchain; empty: the client's rust-toolchain.toml
    "BUILD_ENV": "",                       # file sourced before cargo (LIBCLANG_PATH, OPENSSL_DIR, ...)
    "CARGO_FEATURES": "", "CARGO_JOBS": "",
    "KEEP_TARGET": "0",                    # 0: delete DIR/target after the build
    # What to run
    "PAIRED": "1",                         # 1: run the control next to the candidate
    "NETWORK": "",                         # default mainnet (ethlambda), hoodi (ethrex)
    "CHECKPOINT_URLS": "https://beaconstate.ethstaker.cc,https://mainnet-checkpoint-sync.attestant.io,https://sync-mainnet.beaconcha.in",
    "CL_CHECKPOINT_URL": "https://hoodi.checkpoint.sigp.io",
    "LIGHTHOUSE_BIN": "",                  # ethrex: consensus client binary (copied into DIR/bin)
    "SYNCMODE": "snap",
    "DATA_CONTROL": "", "DATA_CANDIDATE": "",
    "NODE_KEY_CONTROL": "", "NODE_KEY_CANDIDATE": "", "JWT_FILE": "",
    "PUBLIC_IP": "",                       # advertised address; empty leaves it to the client
    "PORT_PREFIX_CONTROL": "1", "PORT_PREFIX_CANDIDATE": "2",
    "NICE": "10",
    # Plan
    "DURATION_S": "86400",                 # total, counted from SOAK_ANCHOR
    "SOAK_ANCHOR": "launch",               # launch | sync (candidate synced)
    "READY_TIMEOUT_S": "600",
    "WARMUP_S": "900",
    "WAIT_SYNC": "0", "SYNC_TIMEOUT_S": "43200",
    "SHUTDOWN_SIGNALS": "TERM INT TERM INT TERM INT",
    "CYCLE_GAP_S": "120",
    "STOP_TIMEOUT_S": "40",
    "FAULTS": "1",
    # r<N>: inject, expect a supervised restart, sleep N s; s<N>: sleep; g: inject, expect a give-up
    "FAULT_PLAN": "r25 r25 r25 s75 r1 r1 r1 g",
    "HEAP_PROFILE": "0",                   # ethrex: jemalloc_profiling build, heap profiles, owner attribution
    "HEAP_EVERY_S": "14400",
    "GO": "go",
    "SAMPLE_S": "30",
    "METRICS_SNAPSHOT_S": "600",
    "DISK_MIN_GB": "20",
    # Analysis window and thresholds
    "SETTLE_S": "3600",                    # skipped at the start of the soak window
    "MIN_TREND_S": "3600",                 # shorter windows report trends but do not judge them
    "TH_RSS_SLOPE_MB_H": "25",             # unpaired: candidate RSS slope
    "TH_RSS_SLOPE_MARGIN_MB_H": "10",      # paired: candidate slope - control slope
    "TH_FD_SLOPE_H": "5", "TH_FD_SLOPE_MARGIN_H": "5",
    "TH_SLOPE_MARGIN_PCT": "10",           # paired: or this % of the control's slope, if larger
    "TH_SYNC_DISTANCE": "2",               # slots (ethlambda) or blocks behind the CL (ethrex)
    "TH_SYNCED_PCT": "99", "TH_SYNCED_MARGIN_PCT": "1",
    "TH_API_OK_PCT": "99",
    "TH_PANICS": "0", "TH_REJECTED": "0", "TH_TIMEOUTS": "0", "TH_HANDLER_PANICS": "0",
}

PRESETS = {
    # Release gate: 24 h ethlambda candidate next to ethlambda main.
    "ethlambda-paired-24h": {"CLIENT": "ethlambda", "PAIRED": "1", "DURATION_S": "86400"},
    # Release gate: 48 h after sync, one ethrex node, jemalloc heap profiles every 4 h.
    "ethrex-memory-48h": {"CLIENT": "ethrex", "PAIRED": "0", "HEAP_PROFILE": "1", "WAIT_SYNC": "1",
                          "SOAK_ANCHOR": "sync", "DURATION_S": "172800", "SAMPLE_S": "60",
                          "SHUTDOWN_SIGNALS": "", "FAULTS": "0", "SETTLE_S": "1800", "DISK_MIN_GB": "30"},
    # Every phase, shortened: a harness check, not a qualification.
    "smoke": {"DURATION_S": "2700", "WARMUP_S": "300", "SHUTDOWN_SIGNALS": "TERM INT", "CYCLE_GAP_S": "60",
              "SETTLE_S": "300", "MIN_TREND_S": "600", "HEAP_EVERY_S": "600"},
}

CLIENT_DEFAULT_NETWORK = {"ethlambda": "mainnet", "ethrex": "hoodi"}


# ---------------------------------------------------------------- config

def parse_kv_file(path):
    out = {}
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[7:]
        k, _, v = line.partition("=")
        out[k.strip()] = os.path.expanduser(os.path.expandvars(shlex.split(v)[0] if v.strip() else ""))
    return out


def init_config(qdir, args):
    qdir = Path(qdir).expanduser().resolve()
    given = {}
    rest = list(args)
    while rest:
        a = rest.pop(0)
        if a == "--config":
            given.update(parse_kv_file(rest.pop(0)))
        elif "=" in a:
            k, _, v = a.partition("=")
            given[k] = os.path.expanduser(v)
        else:
            die(f"unexpected argument {a!r}")
    env = {k: os.environ[k] for k in list(DEFAULTS) + ["PRESET"] if k in os.environ}
    merged = {**env, **given}
    unknown = sorted(set(merged) - set(DEFAULTS) - {"PRESET"})
    if unknown:
        die(f"unknown settings: {', '.join(unknown)}")
    old = load_config(qdir) if (qdir / "config.json").exists() else {}
    preset = merged.get("PRESET", old.get("PRESET", ""))
    if preset and preset not in PRESETS:
        die(f"unknown PRESET {preset}; one of {', '.join(PRESETS)}")
    cfg = {**DEFAULTS, **PRESETS.get(preset, {}), **{k: v for k, v in old.items() if k in DEFAULTS},
           **{k: v for k, v in merged.items() if k in DEFAULTS}}
    cfg["PRESET"] = preset
    if cfg["CLIENT"] not in clients.PROFILES:
        die(f"CLIENT must be one of {', '.join(clients.PROFILES)}")
    cfg["NETWORK"] = cfg["NETWORK"] or CLIENT_DEFAULT_NETWORK[cfg["CLIENT"]]
    cfg["CLIENT_REPO"] = cfg["CLIENT_REPO"] or f"https://github.com/lambdaclass/{cfg['CLIENT']}.git"
    if (qdir / "run/launched.json").exists() and any(cfg.get(k) != old.get(k) for k in DEFAULTS):
        die("this run was already launched; its configuration is fixed")
    cfg["QUAL_DIR"] = str(qdir)
    qdir.mkdir(parents=True, exist_ok=True)
    (qdir / "config.json").write_text(json.dumps(cfg, indent=1, sort_keys=True) + "\n")
    return cfg


def load_config(qdir):
    p = Path(qdir).expanduser().resolve() / "config.json"
    if not p.exists():
        die(f"{p} missing: run `qualify.py init {qdir} ...` first")
    cfg = json.loads(p.read_text())
    cfg["QUAL_DIR"] = str(p.parent)
    return cfg


def on(cfg, key):
    return str(cfg.get(key, "")).strip() not in ("", "0", "false", "no")


def labels(cfg):
    return ["control", "candidate"] if on(cfg, "PAIRED") else ["candidate"]


def session_name(qdir):
    name = re.sub(r"[^A-Za-z0-9_-]", "-", Path(qdir).name)
    return name if name.startswith("qual-") else "qual-" + name


def die(msg):
    print(f"qualify: {msg}", file=sys.stderr)
    sys.exit(1)


def now_utc():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def append_jsonl(path, entry):
    with open(path, "a") as f:
        f.write(json.dumps(entry) + "\n")


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


# ---------------------------------------------------------------- spawned version check

CRATES_RE = re.compile(rb"/spawned-(concurrency|rt)-(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)/")
GIT_RE = re.compile(rb"/checkouts/spawned-[0-9a-f]+/([0-9a-f]{7,40})/(?:concurrency|rt)/")
PATH_RE = re.compile(rb"(/[\w./+-]*/[\w.+-]*spawned[\w.+-]*)/(?:concurrency|rt)/src/")


def binary_spawned(path):
    """The distinct spawned copies a binary was built from, read from the source
    paths its panic locations embed (what `strings | grep spawned` finds)."""
    data = Path(path).read_bytes()
    found = set()
    for m in CRATES_RE.finditer(data):
        found.add("crates.io " + m.group(2).decode())
    for m in GIT_RE.finditer(data):
        found.add("git " + m.group(1).decode())
    for m in PATH_RE.finditer(data):
        root = m.group(1).decode(errors="replace")
        if "/checkouts/" not in root and "/index.crates.io-" not in root:
            found.add("path " + root)
    return sorted(found)


def lock_spawned(lock):
    import tomllib
    pkgs = tomllib.loads(Path(lock).read_text()).get("package", [])
    return sorted({f"{p['name']} {p['version']} {p.get('source', 'path')}" for p in pkgs
                   if p["name"] in ("spawned-concurrency", "spawned-rt")})


def check_spawned(binary, lock=None, expect=""):
    """One spawned in the binary (and in the lock). A second copy hid for a
    release: ethlambda pulled spawned 0.5.0 through ethrex-p2p next to 0.8.0."""
    in_bin = binary_spawned(binary)
    in_lock = lock_spawned(lock) if lock and Path(lock).exists() else []
    lock_versions = sorted({e.split()[1] + " " + e.split()[2] for e in in_lock
                            if e.startswith("spawned-concurrency ")})
    problems = []
    if not in_bin:
        problems.append("no spawned source path found in the binary")
    if len(in_bin) > 1:
        problems.append(f"{len(in_bin)} spawned copies in the binary: {', '.join(in_bin)}")
    if len(lock_versions) > 1:
        problems.append(f"{len(lock_versions)} spawned-concurrency entries in Cargo.lock: {'; '.join(lock_versions)}")
    if expect and in_bin != [f"crates.io {expect}"]:
        problems.append(f"expected only crates.io spawned {expect}, found {in_bin}")
    return {"binary": in_bin, "lock": in_lock, "ok": not problems, "problems": problems}


# ---------------------------------------------------------------- build

def run(cmd, cwd=None, env=None, log=None, check=True):
    print("+ " + " ".join(shlex.quote(str(c)) for c in cmd), flush=True)
    out = open(log, "a") if log else None
    try:
        r = subprocess.run([str(c) for c in cmd], cwd=cwd, env=env, stdout=out, stderr=subprocess.STDOUT if out else None)
    finally:
        if out:
            out.close()
    if check and r.returncode != 0:
        die(f"command failed ({r.returncode}): {' '.join(map(str, cmd))}" + (f"; see {log}" if log else ""))
    return r.returncode


def git_out(args, cwd):
    return subprocess.check_output(["git", *args], cwd=cwd, text=True).strip()


def checkout(repo_url, ref, dest, mirror):
    """A detached worktree of `ref` (branch, sha or pull/N/head) from one shared clone."""
    if not (mirror / ".git").exists():
        run(["git", "clone", "--filter=blob:none", "--no-checkout", repo_url, mirror])
    run(["git", "fetch", "--filter=blob:none", "origin", ref], cwd=mirror)
    sha = git_out(["rev-parse", "FETCH_HEAD"], mirror)
    if not dest.exists():
        run(["git", "worktree", "add", "--detach", dest, sha], cwd=mirror)
    run(["git", "checkout", "--detach", sha], cwd=dest)
    return sha


def cargo_env(cfg, qdir, heap):
    env = dict(os.environ, CARGO_INCREMENTAL="0", CARGO_TARGET_DIR=str(qdir / "target"))
    if cfg["CARGO_JOBS"]:
        env["CARGO_BUILD_JOBS"] = cfg["CARGO_JOBS"]
    if heap:
        # Symbolized heap profiles; debug info does not change codegen.
        env.update(CARGO_PROFILE_RELEASE_DEBUG="line-tables-only", CARGO_PROFILE_RELEASE_STRIP="none")
    return env


def cargo_cmd(cfg, client, features):
    cmd = ["cargo"]
    if cfg["TOOLCHAIN"]:
        cmd = ["rustup", "run", cfg["TOOLCHAIN"], "cargo"]
    cmd += ["build", "--release", "--locked", "--bin", client.bin_target]
    if features:
        cmd += ["--features", ",".join(features)]
    if cfg["BUILD_ENV"]:
        # Host-specific build variables (LIBCLANG_PATH, OPENSSL_*) stay in a local file.
        cmd = ["bash", "-c", 'set -a; . "$0"; set +a; exec "$@"', os.path.expanduser(cfg["BUILD_ENV"]), *cmd]
    return cmd


def cmd_build(qdir):
    cfg = load_config(qdir)
    qdir = Path(cfg["QUAL_DIR"])
    client = clients.load(cfg)
    if (qdir / "run/launched.json").exists():
        die("already launched; build into a new directory")
    for d in ("bin", "src", "locks", "results/build"):
        (qdir / d).mkdir(parents=True, exist_ok=True)
    blog = qdir / "results/build/build.log"
    manifest = {"client": cfg["CLIENT"], "built": now_utc(), "labels": {}}
    heap = on(cfg, "HEAP_PROFILE")
    if heap and cfg["CLIENT"] != "ethrex":
        die("HEAP_PROFILE needs a client with a jemalloc heap-profile endpoint (ethrex)")
    for label in labels(cfg):
        prebuilt = cfg[label.upper() + "_BIN"]
        entry = {}
        if prebuilt:
            shutil.copy2(os.path.expanduser(prebuilt), client.bin(label))
            lock = cfg[label.upper() + "_LOCK"]
            if lock:
                shutil.copy2(os.path.expanduser(lock), qdir / "locks" / f"{label}.Cargo.lock")
            entry["prebuilt"] = "yes"
        else:
            ref = cfg[label.upper() + "_REF"]
            if not ref:
                die(f"{label.upper()}_REF (or {label.upper()}_BIN) is required")
            src = qdir / "src" / label
            entry["ref"] = ref
            entry["commit"] = checkout(cfg["CLIENT_REPO"], ref, src, qdir / "src/.client")
            if label == "candidate" and cfg["SPAWNED_REV"]:
                sp = qdir / "src/spawned"
                entry["spawned_rev"] = checkout(cfg["SPAWNED_REPO"], cfg["SPAWNED_REV"], sp, qdir / "src/.spawned")
                (qdir / "tmp").mkdir(exist_ok=True)
                run(["bash", COMPAT_PATCH, sp, src], env=dict(os.environ, RUNNER_TEMP=str(qdir / "tmp")), log=blog)
            features = [f for f in cfg["CARGO_FEATURES"].split(",") if f]
            if heap:
                features.append("jemalloc_profiling")
                patch = HERE / "patches/ethrex-jemalloc-stats.patch"
                if run(["git", "apply", "--check", patch], cwd=src, check=False) == 0:
                    run(["git", "apply", patch], cwd=src)
                    entry["local_patch"] = patch.name
                else:
                    print(f"warning: {patch.name} does not apply; heap profiles only, no jemalloc stats series")
            entry["features"] = features
            entry["rustc"] = subprocess.run((["rustup", "run", cfg["TOOLCHAIN"]] if cfg["TOOLCHAIN"] else []) + ["rustc", "--version"],
                                            cwd=src, capture_output=True, text=True).stdout.strip()
            started = time.time()
            run(cargo_cmd(cfg, client, features), cwd=src, env=cargo_env(cfg, qdir, heap), log=blog)
            entry["build_s"] = round(time.time() - started)
            shutil.copy2(qdir / "target/release" / client.bin_target, client.bin(label))
            shutil.copy2(src / "Cargo.lock", qdir / "locks" / f"{label}.Cargo.lock")
        lock = qdir / "locks" / f"{label}.Cargo.lock"
        entry["sha256"] = sha256(client.bin(label))
        entry["spawned"] = check_spawned(client.bin(label), lock if lock.exists() else None,
                                         cfg["EXPECT_SPAWNED"] if label == "candidate" else "")
        manifest["labels"][label] = entry
        print(f"{label}: spawned in binary {entry['spawned']['binary']}; ok={entry['spawned']['ok']}", flush=True)
    if not on(cfg, "KEEP_TARGET") and (qdir / "target").exists():
        shutil.rmtree(qdir / "target")
    (qdir / "manifest.json").write_text(json.dumps(manifest, indent=1) + "\n")
    cand = manifest["labels"]["candidate"]["spawned"]
    if not cand["ok"]:
        print("candidate spawned check FAILED: " + "; ".join(cand["problems"]), file=sys.stderr)
        sys.exit(1 if cfg["SPAWNED_CHECK"] == "fail" else 0)


# ---------------------------------------------------------------- preflight

def port_free(port, udp=False):
    with socket.socket(type=socket.SOCK_DGRAM if udp else socket.SOCK_STREAM) as s:
        try:
            s.bind(("0.0.0.0", port))
            return True
        except OSError:
            return False


def write_secret(path, text):
    path.parent.mkdir(mode=0o700, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(text)


def preflight(cfg):
    """Everything that can be checked before any node starts. Returns notes."""
    qdir = Path(cfg["QUAL_DIR"])
    client = clients.load(cfg)
    notes = []
    if (qdir / "run/launched.json").exists():
        die("already launched (run/launched.json); one run per directory")
    if (qdir / "STOP").exists():
        die(f"{qdir / 'STOP'} exists; remove it to launch")
    if shutil.which("tmux") is None:
        die("tmux not found")
    if not (qdir / "manifest.json").exists():
        die("manifest.json missing: run `qualify.py build` first")
    manifest = json.loads((qdir / "manifest.json").read_text())
    cand = manifest["labels"]["candidate"]["spawned"]
    if not cand["ok"]:
        msg = "candidate spawned check failed: " + "; ".join(cand["problems"])
        if cfg["SPAWNED_CHECK"] == "fail":
            die(msg)
        notes.append("WARNING " + msg)
    free_gb = shutil.disk_usage(qdir).free / 1e9
    if free_gb < float(cfg["DISK_MIN_GB"]) + 5:
        die(f"{free_gb:.0f} GB free; need DISK_MIN_GB + 5 = {float(cfg['DISK_MIN_GB']) + 5:.0f}")
    for label in labels(cfg):
        if not os.access(client.bin(label), os.X_OK):
            die(f"missing binary {client.bin(label)}")
        for p in client.tcp_ports(label):
            if not port_free(p):
                die(f"TCP port {p} ({label}) is in use")
        for p in client.udp_ports(label):
            if not port_free(p, udp=True):
                die(f"UDP port {p} ({label}) is in use")
        ver = subprocess.run([client.bin(label), "--version"], capture_output=True, text=True, timeout=30)
        notes.append(f"{label} --version: {(ver.stdout or ver.stderr).strip().splitlines()[0] if (ver.stdout or ver.stderr).strip() else ver.returncode}")
    sec = qdir / "secrets"
    if cfg["CLIENT"] == "ethlambda":
        for label in labels(cfg):
            given = cfg["NODE_KEY_" + label.upper()]
            key = sec / f"{label}.key"
            if not key.exists():
                write_secret(key, (Path(given).read_text().strip() if given else secrets.token_hex(32)) + "\n")
    if cfg["CLIENT"] == "ethrex":
        if not cfg["LIGHTHOUSE_BIN"]:
            die("LIGHTHOUSE_BIN is required for ethrex")
        lh = qdir / "bin/lighthouse"
        if not lh.exists():
            shutil.copy2(os.path.expanduser(cfg["LIGHTHOUSE_BIN"]), lh)
        if not (sec / "jwt.hex").exists():
            write_secret(sec / "jwt.hex", (Path(cfg["JWT_FILE"]).read_text().strip() if cfg["JWT_FILE"] else secrets.token_hex(32)) + "\n")
    for label in labels(cfg):
        d = Path(client.data_dir(label))
        notes.append(f"{label} data: {d} ({'reused, not modified by the harness' if d.exists() and any(d.iterdir()) else 'fresh'})")
        d.mkdir(parents=True, exist_ok=True)
    if on(cfg, "FAULTS"):
        if client.fault_env.encode() not in Path(client.bin("candidate")).read_bytes():
            notes.append(f"WARNING candidate binary has no {client.fault_env} hook: fault phase skipped")
            cfg["FAULTS"] = "0"
    if on(cfg, "HEAP_PROFILE") and shutil.which(os.path.expanduser(cfg["GO"])) is None:
        notes.append(f"WARNING {cfg['GO']} not found: heap profiles are taken but not attributed (set GO=)")
    for d in ("run", "results/logs", "results/profiles", "results/metrics"):
        (qdir / d).mkdir(parents=True, exist_ok=True)
    return notes


# ---------------------------------------------------------------- controller

class Stop(Exception):
    pass


class Controller:
    def __init__(self, cfg):
        self.cfg = cfg
        self.q = Path(cfg["QUAL_DIR"])
        self.R = self.q / "results"
        self.client = clients.load(cfg)
        self.session = session_name(self.q)
        self.stopping = False
        self.stop_reason = ""
        self.running = set()  # process labels started and not stopped by the plan
        self.sync_epoch = None

    # -- bookkeeping
    def log(self, msg, **kw):
        line = f"{now_utc()} {msg}"
        print(line, flush=True)
        with open(self.R / "plan.log", "a") as f:
            f.write(line + "\n")
        append_jsonl(self.R / "events.jsonl", {"epoch": time.time(), "event": msg, **kw})

    def phase(self, name):
        (self.q / "run/phase").write_text(name + "\n")
        self.log(f"phase {name}", phase=name)

    def check(self):
        if self.stopping:
            raise Stop(self.stop_reason)
        if (self.q / "STOP").exists():
            self.stop_reason = "STOP file"
            raise Stop(self.stop_reason)
        free = shutil.disk_usage(self.q).free
        if free < float(self.cfg["DISK_MIN_GB"]) * 1e9:
            self.stop_reason = f"disk guard: {free / 1e9:.1f} GB free"
            (self.q / "ALERT").write_text(self.stop_reason + "\n")
            raise Stop(self.stop_reason)
        for proc_label in sorted(self.running):
            if self.pid(proc_label) is None:
                self.running.discard(proc_label)
                self.log(f"UNEXPECTED EXIT {proc_label}", label=proc_label, unexpected_exit=True)
                if proc_label.startswith("candidate"):
                    self.stop_reason = f"{proc_label} exited unexpectedly"
                    (self.q / "ALERT").write_text(self.stop_reason + "\n")
                    raise Stop(self.stop_reason)

    def sleep(self, seconds):
        end = time.time() + seconds
        while True:
            self.check()
            left = end - time.time()
            if left <= 0:
                return
            time.sleep(min(5, left))

    # -- processes
    def allowed_exes(self):
        return {str(Path(p).resolve()) for p in (self.q / "bin").iterdir()}

    def pid(self, proc_label):
        """PID of a live process this run started, or None."""
        f = self.q / "run" / f"{proc_label}.pid"
        try:
            pid = int(f.read_text())
            exe = os.readlink(f"/proc/{pid}/exe")
            stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        except (OSError, ValueError):
            return None
        if stat[0] == "Z" or exe.removesuffix(" (deleted)") not in self.allowed_exes():
            return None
        return pid

    def start(self, label, all_processes=False):
        procs = self.client.processes(label)
        if not all_processes:
            procs = procs[-1:]
        for proc_label, argv in procs:
            self.check()
            if self.pid(proc_label) is not None:
                raise RuntimeError(f"{proc_label} already running")
            env = []
            if proc_label == "candidate" and on(self.cfg, "FAULTS"):
                env.append(f"{self.client.fault_env}=1")
            if proc_label in labels(self.cfg) and on(self.cfg, "HEAP_PROFILE"):
                env += [f"ETHREX_JEMALLOC_STATS_FILE={self.R}/profiles/jemalloc-{proc_label}.csv",
                        "ETHREX_JEMALLOC_STATS_INTERVAL_S=60"]
            (self.q / "run" / f"{proc_label}.pid").unlink(missing_ok=True)
            cmd = [str(HERE / "node.sh"), str(self.q), proc_label, "nice", "-n", str(self.cfg["NICE"]), "env", *env, *argv]
            # argv form: tmux execs it directly, whatever the user's default shell is
            subprocess.run(["tmux", "new-window", "-d", "-t", self.session, "-n", proc_label, *cmd], check=True)
            for _ in range(100):
                if self.pid(proc_label) is not None:
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError(f"{proc_label} did not start; see results/logs/{proc_label}.*.log")
            self.running.add(proc_label)
            self.log(f"started {proc_label}", label=proc_label, pid=self.pid(proc_label))

    def latest_log(self, proc_label):
        logs = sorted((self.R / "logs").glob(f"{proc_label}.*.log"), key=lambda p: int(p.name.split(".")[-2]))
        return logs[-1] if logs else None

    def stop(self, proc_label, sig="TERM", reason="plan"):
        """Bounded stop of one process this run started: signal, wait STOP_TIMEOUT_S, then SIGKILL."""
        self.running.discard(proc_label)
        pid = self.pid(proc_label)
        if pid is None:
            self.log(f"stop {proc_label}: not running", label=proc_label)
            return None
        try:
            start_time = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
        except OSError:
            self.log(f"stop {proc_label}: exited before the signal", label=proc_label)
            return None
        log = self.latest_log(proc_label)
        offset = log.stat().st_size if log else 0

        def alive():
            try:
                f = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                return f[0] != "Z" and f[19] == start_time
            except OSError:
                return False
        t0 = time.monotonic()
        os.kill(pid, getattr(signal, "SIG" + sig))
        limit = float(self.cfg["STOP_TIMEOUT_S"])
        while alive() and time.monotonic() - t0 < limit:
            time.sleep(0.02)
        timed_out = alive()
        if timed_out:
            if os.readlink(f"/proc/{pid}/exe").removesuffix(" (deleted)") in self.allowed_exes():
                os.kill(pid, signal.SIGKILL)
            while alive():
                time.sleep(0.05)
        elapsed = time.monotonic() - t0
        rc = None
        for _ in range(100):
            ex = self.q / "run" / f"{proc_label}.exit"
            if ex.exists():
                rc = int(ex.read_text().split()[1])
                break
            time.sleep(0.1)
        logged_ms, deadline = None, 0
        if log:
            with open(log, errors="replace") as f:
                f.seek(offset)
                tail = re.sub(r"\x1b\[[0-9;]*m", "", f.read())
            m = re.findall(r"Shutdown complete.*?elapsed_ms=(\d+)", tail)
            logged_ms = int(m[-1]) if m else None
            deadline = len(re.findall(r"Shutdown deadline exceeded", tail))
        entry = {"utc": now_utc(), "label": proc_label, "pid": pid, "signal": sig, "reason": reason,
                 "elapsed_s": round(elapsed, 3), "timed_out": timed_out, "rc": rc,
                 "logged_elapsed_ms": logged_ms, "deadline_exceeded": deadline}
        append_jsonl(self.R / "shutdown.jsonl", entry)
        self.log(f"stopped {proc_label} {sig} in {elapsed:.2f}s" + (" TIMED OUT, killed" if timed_out else ""), **entry)
        return entry

    def wait_ready(self, which):
        deadline = time.time() + float(self.cfg["READY_TIMEOUT_S"])
        pending = list(which)
        while pending:
            self.check()
            for label in list(pending):
                if self.client.ready(label):
                    append_jsonl(self.R / "readiness.jsonl", {"epoch": time.time(), "label": label})
                    pending.remove(label)
            if pending and time.time() > deadline:
                self.stop_reason = f"not ready after READY_TIMEOUT_S: {pending}"
                raise Stop(self.stop_reason)
            if pending:
                time.sleep(2)

    def wait_synced(self):
        """WAIT_SYNC: every node synced (distance <= TH_SYNC_DISTANCE, not syncing) for 10 probes in a row."""
        need = int(self.cfg["TH_SYNC_DISTANCE"])
        streak = {label: 0 for label in labels(self.cfg)}
        deadline = time.time() + float(self.cfg["SYNC_TIMEOUT_S"])
        while any(v < 10 for v in streak.values()):
            for label in streak:
                if streak[label] >= 10:
                    continue
                p = self.client.probe(label)
                ok = (p.get("sync_distance") not in (None, "") and int(p["sync_distance"]) <= need
                      and str(p.get("syncing")) in ("False", "false", "0")
                      and int(p.get("head_age_s", 0) or 0) < 120)
                streak[label] = streak[label] + 1 if ok else 0
                if streak[label] == 10:
                    self.log(f"{label} synced", label=label)
            if time.time() > deadline:
                self.stop_reason = "not synced after SYNC_TIMEOUT_S"
                raise Stop(self.stop_reason)
            self.sleep(30)
        self.sync_epoch = time.time()
        (self.q / "run/sync.epoch").write_text(f"{int(self.sync_epoch)}\n")

    # -- faults
    def faults(self):
        label = "candidate"
        pid = self.pid(label)
        log = self.latest_log(label)
        start_offset = log.stat().st_size
        append_jsonl(self.R / "faults.jsonl", {"event": "phase_start", "pid": pid, "log": log.name,
                                               "offset": start_offset, "epoch": time.time()})
        attempt = 0
        for step in self.cfg["FAULT_PLAN"].split():
            kind, arg = step[0], step[1:]
            if kind == "s":
                self.sleep(float(arg))
                continue
            attempt += 1
            expect = self.client.restart_re if kind == "r" else self.client.give_up_re
            if self.pid(label) != pid:
                raise Stop("candidate PID changed during the fault phase")
            offset = log.stat().st_size
            t0 = time.monotonic()
            os.kill(pid, signal.SIGUSR1)
            seen = False
            while time.monotonic() - t0 < 25:
                self.check()
                with open(log, errors="replace") as f:
                    f.seek(offset)
                    if re.search(expect, re.sub(r"\x1b\[[0-9;]*m", "", f.read())):
                        seen = True
                        break
                time.sleep(0.1)
            entry = {"event": "inject", "attempt": attempt, "expect": "restart" if kind == "r" else "give_up",
                     "observed": seen, "elapsed_s": round(time.monotonic() - t0, 3), "pid": pid}
            if seen and kind == "r":
                t1 = time.monotonic()
                while not self.client.recovered(label) and time.monotonic() - t1 < 20:
                    time.sleep(0.2)
                entry["recovered"] = self.client.recovered(label)
            append_jsonl(self.R / "faults.jsonl", entry)
            self.log(f"fault {attempt}: expect {entry['expect']}, observed={seen}", **entry)
            if kind == "r" and arg:
                self.sleep(float(arg))
        append_jsonl(self.R / "faults.jsonl", {"event": "phase_end", "pid": pid, "log": log.name,
                                               "offset": log.stat().st_size, "epoch": time.time()})

    # -- heap profiles
    def heapdump(self, tag):
        P = self.R / "profiles"
        for label in labels(self.cfg):
            pid = self.pid(label)
            if pid is None:
                continue
            base = P / f"heap-{label}-{tag}"
            entry = {"utc": now_utc(), "epoch": time.time(), "label": label, "tag": tag, "pid": pid}
            for suffix, url in ((".pb.gz", self.client.heap_url(label)), (".svg", self.client.heap_url(label) + "/flamegraph")):
                r = subprocess.run(["curl", "-s", "-m", "300", "-o", f"{base}{suffix}", "-w", "%{http_code}", url],
                                   capture_output=True, text=True)
                entry["http" + suffix] = r.stdout
            stats = P / f"jemalloc-{label}.csv"
            if stats.exists():
                req, res = Path(f"{stats}.dump-request"), Path(f"{stats}.dump-result")
                res.unlink(missing_ok=True)
                req.write_text(f"{base}.heap\n")
                for _ in range(150):
                    if res.exists():
                        break
                    time.sleep(1)
                entry["raw"] = res.read_text().strip() if res.exists() else "no result"
            try:
                entry["rss_kb"] = int(re.search(r"VmRSS:\s+(\d+)", Path(f"/proc/{pid}/status").read_text()).group(1))
            except Exception:
                pass
            append_jsonl(P / "index.jsonl", entry)
            self.log(f"heap profile {label} {tag}", **entry)

    # -- plan
    def plan(self):
        cfg = self.cfg
        both = labels(cfg)
        self.phase("start")
        for label in both:
            self.start(label, all_processes=True)
        self.wait_ready(both)
        self.phase("warmup")
        self.sleep(float(cfg["WARMUP_S"]))
        if on(cfg, "WAIT_SYNC"):
            self.phase("sync-wait")
            self.wait_synced()
        sigs = cfg["SHUTDOWN_SIGNALS"].split()
        if sigs:
            self.phase("shutdown-cycles")
            for sig in sigs:
                self.log(f"paired shutdown cycle {sig}")
                for label in both:
                    self.stop(label, sig, reason="cycle")
                for label in both:
                    self.start(label)
                self.wait_ready(both)
                self.sleep(float(cfg["CYCLE_GAP_S"]))
        if on(cfg, "FAULTS"):
            self.phase("faults")
            self.faults()
            self.stop("candidate", "TERM", reason="after-faults")
            self.start("candidate")
            self.wait_ready(["candidate"])
        self.phase("soak")
        soak_start = time.time()
        anchor = self.sync_epoch if cfg["SOAK_ANCHOR"] == "sync" and self.sync_epoch else float((self.q / "run/launch.epoch").read_text())
        end = anchor + float(cfg["DURATION_S"])
        self.log(f"soak until {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(end))}", end_epoch=end)
        heap = on(cfg, "HEAP_PROFILE")
        every = float(cfg["HEAP_EVERY_S"])
        taken = set()
        if heap:
            self.heapdump("00h")
            taken.add(0)
        while time.time() < end:
            self.check()
            if heap:
                k = int((time.time() - soak_start) // every)
                if k not in taken and time.time() < end - 600:
                    taken.add(k)
                    self.heapdump(f"{round(k * every / 3600):02d}h")
                if time.time() >= end - 300 and "final" not in taken:
                    taken.add("final")
                    self.heapdump(f"{round((time.time() - soak_start) / 3600):02d}h-final")
            time.sleep(min(10, max(0, end - time.time())))

    def shutdown_all(self, reason):
        self.phase("stopping")
        for label in reversed(labels(self.cfg)):
            for proc_label, _ in reversed(self.client.processes(label)):
                try:
                    self.stop(proc_label, "TERM", reason=reason)
                except Exception as e:  # keep stopping the others
                    self.log(f"stop {proc_label} failed: {e}")

    def main(self):
        q = self.q
        lock = open(q / "run/run.lock", "w")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            die("another controller holds run/run.lock")
        (q / "run/controller.pid").write_text(f"{os.getpid()}\n")
        (q / "run/launched.json").write_text(json.dumps({"epoch": time.time(), "utc": now_utc(),
                                                          "duration_s": self.cfg["DURATION_S"]}) + "\n")
        (q / "run/launch.epoch").write_text(f"{int(time.time())}\n")

        def on_signal(signum, _frame):
            self.stopping = True
            self.stop_reason = f"controller got {signal.Signals(signum).name}"
        signal.signal(signal.SIGTERM, on_signal)
        signal.signal(signal.SIGINT, on_signal)
        subprocess.run(["tmux", "new-session", "-d", "-s", self.session, "-n", "sampler", "bash", "-c",
                        'exec "$0" -I "$1" "$2" 2>> "$3"', sys.executable, str(HERE / "sampler.py"), str(q),
                        str(self.R / "sampler.err")], check=True)
        outcome = "complete"
        try:
            self.plan()
        except Stop as e:
            outcome = f"stopped early: {e}"
        except Exception as e:
            outcome = f"error: {e!r}"
        self.log(f"plan ended: {outcome}", outcome=outcome)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        self.shutdown_all("end" if outcome == "complete" else "early-stop")
        time.sleep(float(self.cfg["SAMPLE_S"]) + 5)  # one sample with everything stopped
        subprocess.run(["tmux", "kill-session", "-t", self.session], capture_output=True)
        self.phase("done")
        (q / "run/outcome").write_text(outcome + "\n")
        rc = subprocess.run([sys.executable, "-I", str(HERE / "analyze.py"), str(q)],
                            stdout=open(self.R / "analyze.log", "w"), stderr=subprocess.STDOUT).returncode
        self.log(f"RESULTS READY: results/SUMMARY.md (analyze exit {rc})")
        (q / "run/controller.pid").unlink(missing_ok=True)
        return rc


# ---------------------------------------------------------------- commands

def cmd_launch(qdir):
    cfg = load_config(qdir)
    notes = preflight(cfg)
    q = Path(cfg["QUAL_DIR"])
    (q / "config.json").write_text(json.dumps(cfg, indent=1, sort_keys=True) + "\n")
    (q / "results/preflight.txt").write_text("\n".join(notes) + "\n")
    print("\n".join(notes))
    out = open(q / "results/controller.log", "a")
    p = subprocess.Popen([sys.executable, "-I", str(HERE / "qualify.py"), "run", str(q)],
                         stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
                         start_new_session=True, cwd=q)
    print(f"controller pid {p.pid} (setsid; log results/controller.log); tmux session {session_name(q)}")
    print(f"status: {sys.argv[0]} status {q}\nstop:   {sys.argv[0]} stop {q}   (or touch {q}/STOP)")


def cmd_run(qdir):
    cfg = load_config(qdir)
    sys.exit(Controller(cfg).main())


def cmd_status(qdir):
    cfg = load_config(qdir)
    q = Path(cfg["QUAL_DIR"])
    ctl = q / "run/controller.pid"
    live = ctl.exists() and Path(f"/proc/{ctl.read_text().strip()}").exists()
    print(f"controller: {'running' if live else 'not running'}; phase: "
          f"{(q / 'run/phase').read_text().strip() if (q / 'run/phase').exists() else '-'}; "
          f"outcome: {(q / 'run/outcome').read_text().strip() if (q / 'run/outcome').exists() else '-'}")
    for f in ("plan.log", "samples.csv"):
        p = q / "results" / f
        if p.exists():
            print(f"--- {f}")
            print("\n".join(p.read_text().splitlines()[-6:]))
    for f in ("ALERT", "results/SUMMARY.md"):
        if (q / f).exists():
            print(f"--- {f} exists")


def cmd_stop(qdir):
    cfg = load_config(qdir)
    q = Path(cfg["QUAL_DIR"])
    (q / "STOP").touch()
    ctl = q / "run/controller.pid"
    if not ctl.exists():
        print("no controller running; STOP file written")
        return
    pid = ctl.read_text().strip()
    n = sum(len(clients.load(cfg).processes(lbl)) for lbl in labels(cfg))
    limit = time.time() + n * (float(cfg["STOP_TIMEOUT_S"]) + 5) + float(cfg["SAMPLE_S"]) + 120
    while Path(f"/proc/{pid}").exists() and time.time() < limit:
        time.sleep(2)
    if Path(f"/proc/{pid}").exists():
        die(f"controller {pid} still running after the bounded wait; check results/plan.log")
    print("stopped; summary in results/SUMMARY.md")


def cmd_check_binary(args):
    binary = args[0]
    lock = args[args.index("--lock") + 1] if "--lock" in args else None
    expect = args[args.index("--expect") + 1] if "--expect" in args else ""
    r = check_spawned(binary, lock, expect)
    print(json.dumps(r, indent=1))
    sys.exit(0 if r["ok"] else 1)


def main():
    if len(sys.argv) < 3 or sys.argv[1] in ("-h", "--help"):
        print(__doc__)
        sys.exit(0 if len(sys.argv) > 1 else 2)
    cmd, arg = sys.argv[1], sys.argv[2]
    if cmd == "init":
        cfg = init_config(arg, sys.argv[3:])
        print(json.dumps(cfg, indent=1, sort_keys=True))
    elif cmd == "check-binary":
        cmd_check_binary(sys.argv[2:])
    elif cmd in ("build", "launch", "run", "status", "stop"):
        {"build": cmd_build, "launch": cmd_launch, "run": cmd_run, "status": cmd_status, "stop": cmd_stop}[cmd](arg)
    elif cmd == "analyze":
        os.execv(sys.executable, [sys.executable, "-I", str(HERE / "analyze.py"), arg])
    else:
        die(f"unknown command {cmd}")


if __name__ == "__main__":
    main()
