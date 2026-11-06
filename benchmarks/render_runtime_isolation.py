#!/usr/bin/env python3
"""Render the fixed four-core, closed-loop runtime-isolation study."""

import argparse
from collections import defaultdict
import csv
import json
from pathlib import Path
import statistics
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--data", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    a.output.mkdir(parents=True, exist_ok=True)
    meta = json.loads((a.data / "metadata.json").read_text())
    with (a.data / "raw.csv").open(newline="") as stream:
        raw = list(csv.DictReader(stream))
    assert (
        meta.get("finished_utc")
        and not meta.get("stopped_at_deadline")
        and len(raw) == meta["planned_samples"] == 32
    )
    assert all(
        r["status"] == "ok" and r["started"] == r["stopped"] == r["dropped"] == "18"
        for r in raw
    )
    groups = defaultdict(list)
    for r in raw:
        if r["phase"] == "isolation":
            groups[r["path"], r["load"], r["layout"]].append(r)
    assert len(groups) == 8 and all(len(g) == 3 for g in groups.values())
    summary = []
    for (path, load, layout), g in sorted(groups.items()):
        s = dict(path=path, load=load, layout=layout, samples=len(g))
        for metric in [
            "critical_completed",
            "critical_per_s",
            "critical_p50_ns",
            "critical_p99_ns",
            "bg_per_s",
            "cpu_s",
            "elapsed_s",
        ]:
            values = [float(r[metric]) for r in g]
            for name, value in zip(
                ["median", "min", "max"],
                [statistics.median(values), min(values), max(values)],
            ):
                s[metric + "_" + name] = value
        summary.append(s)
    with (a.output / "SUMMARY.csv").open("w", newline="") as stream:
        w = csv.DictWriter(stream, list(summary[0]), lineterminator="\n")
        w.writeheader()
        w.writerows(summary)
    plt.rcParams.update(
        {
            "svg.hashsalt": "spawned-isolation",
            "figure.dpi": 140,
            "font.size": 10,
            "axes.grid": True,
            "grid.alpha": 0.2,
        }
    )
    for path in ["direct", "relay"]:
        chosen = {(r["load"], r["layout"]): r for r in summary if r["path"] == path}
        cells = [
            ("idle", "shared"),
            ("idle", "split"),
            ("busy", "shared"),
            ("busy", "split"),
        ]
        fig, axes = plt.subplots(2, 2, figsize=(13, 9))
        for ax, (metric, scale, title) in zip(
            axes.flat,
            [
                (
                    "critical_per_s",
                    1e-3,
                    "Critical closed-loop throughput, k requests/s",
                ),
                ("critical_p99_ns", 1e-6, "Critical observed p99, ms"),
                ("bg_per_s", 1e-3, "Background throughput, k requests/s"),
                ("critical_completed", 1, "Completed critical requests per process"),
            ],
        ):
            mid = np.array([chosen[c][metric + "_median"] * scale for c in cells])
            low = np.array([chosen[c][metric + "_min"] * scale for c in cells])
            high = np.array([chosen[c][metric + "_max"] * scale for c in cells])
            ax.bar(
                range(4),
                mid,
                yerr=[mid - low, high - mid],
                capsize=4,
                color=["#477fa9", "#e49436", "#477fa9", "#e49436"],
            )
            ax.set_xticks(range(4), [f"{load}\n{layout}" for load, layout in cells])
            ax.set_title(title)
            ax.set_ylim(bottom=0)
        fig.suptitle(
            f"Runtime isolation: {path} request path; same four actor cores\nshared4 versus critical1 + general3; two-second closed loop; n=3, min/max whiskers"
        )
        fig.tight_layout(rect=(0, 0, 1, 0.93))
        fig.savefig(a.output / f"{path}.png", bbox_inches="tight")
        fig.savefig(
            a.output / f"{path}.svg", bbox_inches="tight", metadata={"Date": None}
        )
        svg = a.output / f"{path}.svg"
        svg.write_text(
            "\n".join(line.rstrip() for line in svg.read_text().splitlines()) + "\n"
        )
        plt.close(fig)
    lines = [
        "# Shared versus split actor runtimes",
        "",
        "Four actor workers/cores in both layouts; four separate driver workers/cores. One closed-loop critical producer. Three fresh two-second processes/cell, with all final in-flight work drained. Different completed request rates are part of the result, not equivalent offered traffic or an open-loop SLO. P99s are short-run sample quantiles; min/max are not confidence intervals.",
        "",
        "| Path | Background | Layout | Critical requests/s median | Critical p99 ms median [min–max] | Background requests/s median | Median critical sample count |",
        "|---|---|---|---:|---:|---:|---:|",
    ]
    for s in summary:
        lines.append(
            f"| {s['path']} | {s['load']} | {s['layout']} | {s['critical_per_s_median']:,.0f} | {s['critical_p99_ns_median'] / 1e6:.3f} [{s['critical_p99_ns_min'] / 1e6:.3f}–{s['critical_p99_ns_max'] / 1e6:.3f}] | {s['bg_per_s_median']:,.0f} | {s['critical_completed_median']:,.0f} |"
        )
    lines += [
        "",
        "![Direct path](direct.png)",
        "",
        "![General-runtime relay](relay.png)",
        "",
        "Isolating the destination helps the direct path in this synthetic load, at a background-throughput cost. When the request still traverses the busy general-runtime router, splitting the destination alone does not isolate the whole path. This is a normal Tokio shared-versus-partitioned resource comparison, not a Tokio replacement or client benchmark. Full experiment semantics and source audit are in the study README.",
    ]
    (a.output / "REPORT.md").write_text("\n".join(lines) + "\n")
    print("Rendered 2 figures; 8 separate cells; all32 processes validated")


if __name__ == "__main__":
    main()
