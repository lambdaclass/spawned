#!/usr/bin/env python3
"""Collect a version- and warm-up-guarded Ethlambda comparison (read-only)."""
import argparse
import datetime as dt
import json
import math
from pathlib import Path
import re
import statistics
import sys
import time
import urllib.parse
import urllib.request
import urllib.error

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--start", required=True, help="Immutable ISO-8601 timestamp with timezone")
parser.add_argument("--end", required=True)
parser.add_argument("--expected-a", required=True, help="Required fragment of the reported build version")
parser.add_argument("--expected-b", required=True)
parser.add_argument("--expected-rustc", default="rustc-v1.97.1")
parser.add_argument("--expected-target", default="x86_64-unknown-linux-gnu")
parser.add_argument("--a", default="beacon-eth4")
parser.add_argument("--b", default="beacon-eth5")
parser.add_argument("--warmup-seconds", type=int, default=1800)
parser.add_argument("--minimum-pairs", type=int, default=100)
parser.add_argument("--metrics-off", action="store_true", help="Require no exported spawned counter on B")
parser.add_argument("--purpose", choices=["canary", "historical-validation"], default="canary")
parser.add_argument("--out", required=True, type=Path)
args = parser.parse_args()
BASE = "https://metrics.ethlambda.xyz"
PROM = BASE + "/api/datasources/proxy/uid/lean64-prom/api/v1/query_range?"
owns_output = False


class GateFailure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise GateFailure(message)


def epoch(value):
    parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    require(parsed.tzinfo is not None, "Times must include an explicit timezone")
    return parsed.timestamp()


def redact(value):
    if isinstance(value, dict):
        return {k: redact(v) for k, v in value.items()
                if k not in {"instance", "host", "hostname", "container", "pod"}}
    if isinstance(value, list):
        return [redact(v) for v in value]
    return value


def save(name, value):
    (args.out / name).write_text(json.dumps(redact(value), indent=2, allow_nan=False) + "\n")


def prom(name, query):
    parameters = {"query": query, "start": args.start, "end": args.end, "step": "60s"}
    with urllib.request.urlopen(PROM + urllib.parse.urlencode(parameters), timeout=30) as response:
        result = json.load(response)
    save(name + ".json", {"parameters": parameters, "response": result})
    require(result.get("status") == "success", "Prometheus query failed: " + name)
    return result["data"]["result"]


def verify_node(node, expected, letter):
    selector = '{job="beacon",node=' + json.dumps(node) + '}'
    versions = prom(letter + "-versions", "lean_node_info" + selector)
    actual = {series["metric"].get("version", "") for series in versions if series.get("values")}
    require(len(actual) == 1 and expected in next(iter(actual)),
            f"{node}: required build fragment {expected!r}, observed {sorted(actual)!r}")
    version = next(iter(actual))
    require(version.endswith("/" + args.expected_target + "/" + args.expected_rustc),
            node + ": reported compiler or target differs from the required control")
    starts = prom(letter + "-starts", "lean_node_start_time_seconds" + selector)
    timestamps = {float(value) for series in starts for _, value in series.get("values", [])}
    require(len(timestamps) == 1, node + ": missing start time or restart inside the window")
    started = next(iter(timestamps))
    require(math.isfinite(started) and started + args.warmup_seconds <= start,
            node + ": window begins before the required warm-up completes")
    up = prom(letter + "-up", "up" + selector)
    require(len(up) == 1, node + ": missing or ambiguous scrape target")
    samples = up[0].get("values", [])
    expected_count = math.floor((end - start) / 60) + 1
    require(len(samples) >= expected_count * .95 and all(float(v) == 1 for _, v in samples),
            node + ": failed scrapes or insufficient scrape coverage")
    require(samples and samples[0][0] <= start + 60 and samples[-1][0] >= end - 60,
            node + ": missing scrape coverage at a window boundary")
    return {"node": node, "version": next(iter(actual)), "process_started": started,
            "scrape_samples": len(samples), "uptime_at_window_start_s": start - started}


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[math.ceil(len(ordered) * fraction) - 1]


def main():
    global start, end, owns_output
    require(args.expected_a.strip() and args.expected_b.strip(), "Expected build fragments cannot be empty")
    require(args.a != args.b, "A and B must be different node labels")
    require(all(re.fullmatch(r"[a-zA-Z0-9_.:-]{1,64}", node) for node in [args.a, args.b]),
            "Unsupported node-label characters")
    start, end = epoch(args.start), epoch(args.end)
    require(0 < end - start <= 6 * 3600, "The comparison window must be at most six hours")
    require(end <= time.time() - 120, "End the window at least two minutes ago for completion telemetry")
    require(0 <= args.warmup_seconds <= 86400 and 1 <= args.minimum_pairs <= 5000, "Invalid limits")
    require(not args.out.exists() or not any(args.out.iterdir()), "Output directory must be empty")
    args.out.mkdir(parents=True, exist_ok=True)
    owns_output = True
    save("request.json", {**vars(args), "out": str(args.out), "read_only": True,
                          "limitation": "Build labels do not prove image provenance or EL/hardware parity"})
    builds = {"A": verify_node(args.a, args.expected_a, "A"),
              "B": verify_node(args.b, args.expected_b, "B")}
    if args.metrics_off:
        samples = prom("B-framework-counter", 'spawned_messages_handled_total{job="beacon",node='
                       + json.dumps(args.b) + '}')
        require(not samples, "B still exports framework counters; the requested metrics-off control is not present")
    template = Path(__file__).resolve().parent.parent / "data" / "paired-block-query.json"
    body = json.loads(template.read_text())
    body.update({"from": str(int(start * 1000)), "to": str(int(end * 1000))})
    for query, original, node in zip(body["queries"], ["beacon-eth4", "beacon-eth5"], [args.a, args.b]):
        sql = query["rawSql"].replace("'" + original + "'", "'" + node + "'")
        # received is non-null by the WHERE clause, so a null total interval
        # identifies a missing attestable event without another time projection.
        sql = sql.replace(" AND attestable IS NOT NULL", "")
        sql = re.sub(r"LIMIT \d+", "LIMIT 5000", sql)
        query["rawSql"] = sql + "\nSETTINGS max_execution_time=10, max_rows_to_read=2000000"
    save("block-query.json", body)
    request = urllib.request.Request(BASE + "/api/ds/query", data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = json.load(response)
    save("block-response.json", payload)
    nodes = {}
    for letter in ["A", "B"]:
        result = payload.get("results", {}).get(letter, {})
        require(not result.get("error") and result.get("status") == 200, "ClickHouse query failed: " + letter)
        frames = result.get("frames", [])
        require(len(frames) == 1, "Unexpected ClickHouse frame count: " + letter)
        frame = frames[0]
        names = [field["name"] for field in frame["schema"]["fields"]]
        rows = [dict(zip(names, values)) for values in zip(*frame["data"]["values"])]
        require(len(rows) < 5000, "Row limit reached; shorten the window")
        nodes[letter] = {row["block_root"]: row for row in rows}
        require(len(nodes[letter]) == len(rows), "Duplicate block roots in query response")
    received = sorted(nodes["A"].keys() & nodes["B"].keys())
    completed = [key for key in received if all(nodes[letter][key]["total_ms"] is not None for letter in ["A", "B"])]
    require(len(completed) >= args.minimum_pairs, "Insufficient matching completed blocks")
    summary = {"purpose": args.purpose, "window": [args.start, args.end], "builds": builds,
               "received": {k: len(v) for k, v in nodes.items()}, "matched_received": len(received),
               "matched_completed": len(completed),
               "received_without_attestable": {k: sum(r["total_ms"] is None for r in v.values()) for k, v in nodes.items()},
               "only_one_node_received": {k: len(set(v) - set(received)) for k, v in nodes.items()},
               "metrics": {}, "interpretation": "Observational measurement; causal attribution requires matched configuration/replay"}
    for field in ["total_ms", "before_ms", "columns_wait_ms", "exec_ms", "head_ms", "el_ms", "stf_ms", "get_head_ms"]:
        keys = [key for key in completed if all(isinstance(nodes[k][key][field], (int, float)) for k in ["A", "B"])]
        if not keys:
            continue
        values = {k: [nodes[k][key][field] for key in keys] for k in ["A", "B"]}
        summary["metrics"][field] = {k: {"n": len(v), "mean": statistics.mean(v),
                                            "p50": percentile(v, .5), "p99": percentile(v, .99)} for k, v in values.items()}
        summary["metrics"][field]["paired_mean_B_minus_A"] = statistics.mean(values["B"]) - statistics.mean(values["A"])
    save("summary.json", summary)
    (args.out / "COMPLETE").write_text("collection gates and matched-block extraction passed\n")
    print(json.dumps({"purpose": args.purpose, "matched_completed": len(completed),
                      "total_ms": summary["metrics"].get("total_ms")}, indent=2))


if __name__ == "__main__":
    try:
        main()
    except urllib.error.HTTPError as error:
        if owns_output:
            save("http-failure.json", {"status": error.code, "response": error.read().decode(errors="replace"), "complete": False})
        print("Collection failed: HTTP " + str(error.code), file=sys.stderr)
        raise SystemExit(1)
    except GateFailure as error:
        if owns_output:
            save("gate-failure.json", {"error": str(error), "complete": False})
        print("Comparison refused: " + str(error), file=sys.stderr)
        raise SystemExit(2)
