"""Client profiles for the release qualification harness.

One place for everything that differs between ethlambda and ethrex: ports,
node command lines, health probes, fault-injection hooks and the log lines a
supervised restart prints. The controller, sampler and analyzer import it.

Ports are `prefix * 10000 + base`, so control (prefix 1) and candidate
(prefix 2) never collide on one host. The prefix-1 ports are the ones every
earlier canary used, so old and new runs stay comparable.
"""
import json
import os
import urllib.request

LABELS = ("control", "candidate")


def http_get(url, timeout=3):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return r.read()
    except Exception:
        return None


def json_rpc(url, method, params=(), timeout=5):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": list(params)}).encode()
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read())["result"]
    except Exception:
        return None


def beacon_syncing(api):
    raw = http_get(f"http://127.0.0.1:{api}/eth/v1/node/syncing")
    if raw is None:
        return None
    try:
        return json.loads(raw)["data"]
    except Exception:
        return None


def as_int(v):
    try:
        return int(v)
    except Exception:
        return None


class Client:
    name = ""
    bin_target = ""
    fault_env = ""
    # Log lines (regex) a supervised restart, and a supervisor giving up, print.
    restart_re = r"Supervisor restarted|event=\"?restart_started"
    give_up_re = r"restart_budget_exhausted|supervision_give_up|supervisor gave up"
    # Lines counted as panics. Injected faults are excluded by log offset.
    panic_re = r"panicked at|Panic in message handler|panic in handler"

    def __init__(self, cfg):
        self.cfg = cfg

    def prefix(self, label):
        return int(self.cfg["PORT_PREFIX_" + label.upper()])

    def data_dir(self, label):
        return self.cfg.get("DATA_" + label.upper()) or os.path.join(self.cfg["QUAL_DIR"], "data", label)

    def bin(self, label):
        return os.path.join(self.cfg["QUAL_DIR"], "bin", f"{self.name}-{label}")

    def nice(self):
        return ["nice", "-n", str(self.cfg["NICE"])]


class Ethlambda(Client):
    name = "ethlambda"
    bin_target = "ethlambda"
    fault_env = "ETHLAMBDA_FAULT_INJECTION"

    def ports(self, label):
        p = self.prefix(label) * 10000
        return {"api": p + 5052, "metrics": p + 5054, "discovery": p + 9000, "gossipsub": p + 9001}

    def tcp_ports(self, label):
        return list(self.ports(label).values())

    def udp_ports(self, label):
        q = self.ports(label)
        return [q["discovery"], q["gossipsub"]]

    def processes(self, label):
        """[(process label, argv, extra env)], started in order; the last is the node under test."""
        q, c = self.ports(label), self.cfg
        argv = [self.bin(label), "beacon", "--network", c["NETWORK"],
                "--checkpoint-sync-url", c["CHECKPOINT_URLS"],
                "--node-key", os.path.join(c["QUAL_DIR"], "secrets", f"{label}.key"),
                "--data-dir", self.data_dir(label),
                "--discovery.port", str(q["discovery"]), "--gossipsub-port", str(q["gossipsub"]),
                "--api-port", str(q["api"]), "--metrics-port", str(q["metrics"])]
        if c.get("PUBLIC_IP"):
            argv += ["--discovery.advertise-ip", c["PUBLIC_IP"]]
        return [(label, argv)]

    def metrics_url(self, label):
        return f"http://127.0.0.1:{self.ports(label)['metrics']}/metrics"

    def ready(self, label):
        return beacon_syncing(self.ports(label)["api"]) is not None

    def probe(self, label):
        """Head, sync distance, syncing flag and peers. sync_distance is in slots."""
        api = self.ports(label)["api"]
        row = {}
        d = beacon_syncing(api)
        if d is not None:
            row["api_ok"] = 1
            row["head"] = d.get("head_slot", "")
            row["sync_distance"] = d.get("sync_distance", "")
            row["syncing"] = d.get("is_syncing", "")
        else:
            row["api_ok"] = 0
        raw = http_get(f"http://127.0.0.1:{api}/eth/v1/node/peer_count")
        if raw:
            try:
                row["peers"] = int(json.loads(raw)["data"]["connected"])
            except Exception:
                pass
        return row

    def peers_from_metrics(self, text):
        vals = [float(l.rpartition(" ")[2]) for l in text.splitlines() if l.startswith("lean_connected_peers")]
        return int(sum(vals)) if vals else None

    def recovered(self, label):
        """The supervised HTTP actor answers again (not the separate metrics listener)."""
        return self.ready(label)


class Ethrex(Client):
    """ethrex execution client, each node paired with its own Lighthouse."""
    name = "ethrex"
    bin_target = "ethrex"
    fault_env = "ETHREX_FAULT_INJECTION"

    def ports(self, label):
        p = self.prefix(label) * 10000
        return {"authrpc": p + 8551, "http": p + 8545, "metrics": p + 9090, "p2p": 30303 + 10 * self.prefix(label),
                "cl_p2p": p + 9100, "cl_quic": p + 9101, "cl_http": p + 5152, "cl_metrics": p + 5154}

    def tcp_ports(self, label):
        q = self.ports(label)
        return [q[k] for k in ("authrpc", "http", "metrics", "p2p", "cl_p2p", "cl_http", "cl_metrics")]

    def udp_ports(self, label):
        q = self.ports(label)
        return [q["p2p"], q["cl_p2p"], q["cl_quic"]]

    def data_dir(self, label):
        return os.path.join(super().data_dir(label), "ethrex")

    def cl_data_dir(self, label):
        return os.path.join(super().data_dir(label), "lighthouse")

    def processes(self, label):
        q, c = self.ports(label), self.cfg
        jwt = os.path.join(c["QUAL_DIR"], "secrets", "jwt.hex")
        cl = [os.path.join(c["QUAL_DIR"], "bin", "lighthouse"), "bn", "--network", c["NETWORK"],
              "--datadir", self.cl_data_dir(label),
              "--execution-endpoint", f"http://127.0.0.1:{q['authrpc']}", "--execution-jwt", jwt,
              "--checkpoint-sync-url", c["CL_CHECKPOINT_URL"],
              "--port", str(q["cl_p2p"]), "--quic-port", str(q["cl_quic"]),
              "--http", "--http-address", "127.0.0.1", "--http-port", str(q["cl_http"]),
              "--metrics", "--metrics-address", "127.0.0.1", "--metrics-port", str(q["cl_metrics"])]
        el = [self.bin(label), "--network", c["NETWORK"], "--datadir", self.data_dir(label),
              "--authrpc.addr", "127.0.0.1", "--authrpc.port", str(q["authrpc"]), "--authrpc.jwtsecret", jwt,
              "--http.addr", "127.0.0.1", "--http.port", str(q["http"]),
              "--p2p.port", str(q["p2p"]), "--discovery.port", str(q["p2p"]),
              "--metrics", "--metrics.addr", "127.0.0.1", "--metrics.port", str(q["metrics"]),
              "--syncmode", c["SYNCMODE"], "--log.color", "never"]
        if c.get("PUBLIC_IP"):
            cl += ["--enr-address", c["PUBLIC_IP"]]
            el += ["--nat.extip", c["PUBLIC_IP"]]
        return [(label + "-cl", cl), (label, el)]

    def metrics_url(self, label):
        return f"http://127.0.0.1:{self.ports(label)['metrics']}/metrics"

    def rpc(self, label, method, params=()):
        return json_rpc(f"http://127.0.0.1:{self.ports(label)['http']}", method, params)

    def ready(self, label):
        return self.rpc(label, "eth_blockNumber") is not None

    def probe(self, label):
        """EL head, lag behind its CL in blocks (sync_distance), syncing flag, peers."""
        import time
        row = {}
        bn = self.rpc(label, "eth_blockNumber")
        row["api_ok"] = 1 if bn is not None else 0
        if bn is not None:
            row["head"] = int(bn, 16)
            blk = self.rpc(label, "eth_getBlockByNumber", ["latest", False])
            if blk:
                row["head_age_s"] = int(time.time() - int(blk["timestamp"], 16))
            sy = self.rpc(label, "eth_syncing")
            row["syncing"] = "" if sy is None else (sy is not False)
            pc = self.rpc(label, "net_peerCount")
            if pc:
                row["peers"] = int(pc, 16)
        cl_api = self.ports(label)["cl_http"]
        d = beacon_syncing(cl_api)
        if d is not None:
            row["cl_head_slot"] = d.get("head_slot", "")
            row["cl_syncing"] = d.get("is_syncing", "")
        raw = http_get(f"http://127.0.0.1:{cl_api}/eth/v2/beacon/blocks/head", timeout=10)
        if raw and "head" in row:
            try:
                exec_block = int(json.loads(raw)["data"]["message"]["body"]["execution_payload"]["block_number"])
                row["sync_distance"] = abs(exec_block - row["head"])
            except Exception:
                pass
        return row

    def peers_from_metrics(self, text):
        return None

    def recovered(self, label):
        return self.ready(label)

    def heap_url(self, label):
        return f"http://127.0.0.1:{self.ports(label)['http']}/debug/pprof/allocs"


PROFILES = {"ethlambda": Ethlambda, "ethrex": Ethrex}


def load(cfg):
    return PROFILES[cfg["CLIENT"]](cfg)
