#!/usr/bin/env python3
"""Render the completed reliability benchmark campaign; requires matplotlib."""
import argparse
import csv
import json
import statistics
from collections import defaultdict
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--input", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
args = parser.parse_args()
assert (args.input / "COMPLETE").exists(), "incomplete campaigns are excluded"
args.out.mkdir(parents=True, exist_ok=True)
records = json.loads((args.input / "runs.json").read_text())
assert all(r["returncode"] == 0 and not r["failure"] for r in records)
groups = {key: defaultdict(list) for key in ("baseline", "graph", "offered", "soak")}
for record in records:
    label = record["label"]
    if label.startswith(("rpc-", "burst-")):
        kind, case = "baseline", label.rsplit("-r", 1)[0]
    elif label.startswith("graph-"):
        kind, case = "graph", label.rsplit("-r", 1)[0]
    elif label.startswith("offered-"):
        kind, case = "offered", label
    elif label.startswith("soak-"):
        kind, case = "soak", label
    else:
        continue
    rows = list(csv.DictReader((args.input / record["stdout"]).open()))
    assert len(rows) == 1
    row = {key: float(value) if key not in ("backend", "scenario", "timing", "allocation_counting") else value
           for key, value in rows[0].items()}
    row.update(sampled_peak_rss_mib=record["rss_bytes"] / 2**20,
               sampled_peak_threads=record["threads"])
    if kind == "graph":
        assert row["succeeded"] == 65536 and row["logical_handlers"] == 8 * 65536
    if kind == "offered":
        assert row["succeeded"] + row["timeouts"] == 65536
    if kind == "soak":
        assert row["started"] == row["stopped"] == row["dropped"]
    groups[kind][case].append(row)
assert sorted(map(len, groups["baseline"].values())) == [3] * 8
assert sorted(map(len, groups["graph"].values())) == [3] * 3
assert sorted(map(len, groups["offered"].values())) == [1] * 6
assert sorted(map(len, groups["soak"].values())) == [1] * 3

def median(rows, key):
    return statistics.median(row[key] for row in rows)

summary = {}
for kind, cases in groups.items():
    summary[kind] = {}
    for case, rows in sorted(cases.items()):
        numeric = [k for k, v in rows[0].items() if isinstance(v, (int, float))]
        summary[kind][case] = {
            "n": len(rows),
            **{key: {"median": median(rows, key),
                     "min": min(row[key] for row in rows),
                     "max": max(row[key] for row in rows)} for key in numeric},
        }
    flat = []
    for case, rows in sorted(cases.items()):
        for replicate, row in enumerate(rows):
            flat.append({"case": case, "replicate": replicate, **row})
    with (args.out / (kind + ".csv")).open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(flat[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(flat)
(args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

plt.rcParams.update({"font.size": 10, "axes.spines.top": False,
                     "axes.spines.right": False, "svg.hashsalt": "spawned-reliability"})
colors = ["#2377a4", "#df8431", "#529765"]
def save(fig, name):
    fig.tight_layout()
    for extension in ("png", "svg"):
        destination = args.out / f"{name}.{extension}"
        fig.savefig(destination, dpi=160, bbox_inches="tight")
        if extension == "svg":
            destination.write_text("\n".join(line.rstrip() for line in destination.read_text().splitlines()) + "\n")
    plt.close(fig)

fig, axes = plt.subplots(1, 2, figsize=(13, 5))
for ax, kind, metric, title in (
    (axes[0], "baseline", "completed_per_s", "Requests / burst completions"),
    (axes[1], "graph", "replies_per_s", "8-stage graph: root replies"),
):
    cases = sorted(groups[kind])
    centers = [median(groups[kind][case], metric) / 1e6 for case in cases]
    lower = [center - min(row[metric] for row in groups[kind][case]) / 1e6 for center, case in zip(centers, cases)]
    upper = [max(row[metric] for row in groups[kind][case]) / 1e6 - center for center, case in zip(centers, cases)]
    ax.barh(cases, centers, xerr=[lower, upper], color=colors[0], capsize=3)
    ax.set_xlabel("Million completions/s; median and min–max (n=3)")
    ax.set_title(title)
    ax.grid(axis="x", alpha=.2)
fig.suptitle("4 shared logical CPUs • different actor counts are separate cases • raw Tokio is a lower-level baseline")
save(fig, "request-throughput")

fig, axes = plt.subplots(1, 2, figsize=(12, 5))
for index, profile in enumerate(("tokio", "sharded", "native-deque")):
    rows = [groups["offered"][f"offered-{profile}-{rate}"][0] for rate in (100000, 1000000)]
    x = [i + (index - 1) * .24 for i in range(2)]
    axes[0].bar(x, [100 * r["timeouts"] / r["messages"] for r in rows], width=.24, color=colors[index], label=profile)
    cold = [max(r["cold_other_observed_p99_ns"], r["cold_shard0_observed_p99_ns"]) / 1e6 for r in rows]
    axes[1].bar(x, cold, width=.24, color=colors[index], label=profile)
for ax in axes:
    ax.set_xticks([0, 1], ["100k/s target", "1M/s target"])
    ax.legend()
    ax.grid(axis="y", alpha=.2)
axes[0].set_ylabel("Reply timeouts (%)")
axes[1].set_ylabel("Worst cohort cold observed p99 (ms)")
fig.suptitle("Concentrated load: 4,096 actors, 256 producers • 100ms deadline • one run per cell")
save(fig, "overload-fairness")

fig, ax = plt.subplots(figsize=(9, 4))
for index, profile in enumerate(("tokio", "sharded", "native-deque")):
    rows = list(csv.DictReader((args.input / f"soak-{profile}-series.csv").open()))
    ax.plot([float(r["elapsed_s"]) for r in rows], [int(r["rss_bytes"]) / 2**20 for r in rows], label=profile, color=colors[index])
ax.set_xlabel("Seconds since startup")
ax.set_ylabel("Sampled process RSS (MiB)")
ax.set_title("30s churn/idle soaks • one run per profile • not a long-duration leak proof")
ax.legend()
ax.grid(alpha=.2)
save(fig, "lifecycle-rss")

lines = ["| Baseline case | Completions/s, median [min–max] | Completion p99 ms, median |",
         "|---|---:|---:|"]
for case, rows in sorted(groups["baseline"].items()):
    values = [r["completed_per_s"] for r in rows]
    lines.append(f"| {case} | {statistics.median(values):,.0f} [{min(values):,.0f}–{max(values):,.0f}] | {median(rows, 'completion_p99_ns') / 1e6:.3f} |")
lines += ["", "| Graph profile | Root replies/s, median [min–max] | Observed p99 ms, median |",
          "|---|---:|---:|"]
for case, rows in sorted(groups["graph"].items()):
    values = [r["replies_per_s"] for r in rows]
    lines.append(f"| {case} | {statistics.median(values):,.0f} [{min(values):,.0f}–{max(values):,.0f}] | {median(rows, 'observed_p99_ns') / 1e6:.3f} |")
lines += ["", "| Offered-load case | Reply timeouts | Worst cold cohort observed p99 ms |", "|---|---:|---:|"]
for case, rows in sorted(groups["offered"].items()):
    r = rows[0]
    cold = max(r["cold_other_observed_p99_ns"], r["cold_shard0_observed_p99_ns"])
    lines.append(f"| {case} | {r['timeouts']:.0f} / {r['messages']:.0f} | {cold / 1e6:.3f} |")
(args.out / "tables.md").write_text("\n".join(lines) + "\n")
