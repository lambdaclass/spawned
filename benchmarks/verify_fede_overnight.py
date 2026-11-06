#!/usr/bin/env python3
"""Verify the delivered archive, original-row identity and recorded CPU audits."""

import argparse
from collections import Counter
import csv
import hashlib
import json
from pathlib import Path


def rows(path):
    with path.open() as stream:
        return list(csv.DictReader(stream))


def cpus(mask):
    result = set()
    for part in mask.split(","):
        bounds = [int(x) for x in part.split("-")]
        result.update(range(bounds[0], bounds[-1] + 1))
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--data", type=Path, required=True)
    p.add_argument("--report", type=Path, required=True)
    a = p.parse_args()
    if not __debug__:
        raise RuntimeError("Archive verification requires assertions enabled")
    manifest = json.loads((a.data / "sha256.json").read_text())
    actual = {str(f.relative_to(a.data)) for f in a.data.rglob("*") if f.is_file()} - {
        "sha256.json"
    }
    assert set(manifest) == actual, "missing or unexpected archive files"
    for name, expected in manifest.items():
        assert hashlib.sha256((a.data / name).read_bytes()).hexdigest() == expected, (
            name
        )
    meta = json.loads((a.data / "metadata.json").read_text())
    combined = rows(a.data / "raw.csv")
    assert len(meta["inputs"]) == 45 and len(combined) == meta["samples"] == 5744
    by_key = {(r["dataset"], r["original_stem"]): r for r in combined}
    assert len(by_key) == len(combined)
    counts = Counter()
    for block, info in meta["inputs"].items():
        original = rows(a.data / "runs" / block / "raw.csv")
        assert len(original) == info["samples"]
        for r in original:
            archived = by_key[block, r["stem"]]
            assert (
                archived["revision"] == info["revision"] and archived["status"] == "ok"
            )
            for key, value in r.items():
                if key != "stem":
                    assert archived[key] == value, (block, r["stem"], key)
            counts[block] += 1
    assert sum(counts.values()) == len(combined)
    pinned = [r for r in combined if r["dataset"].startswith("V9-")]
    assert len(pinned) == 96
    for r in pinned:
        config = json.loads(r["config_json"])
        audit = rows(a.data / "series" / (r["stem"] + ".csv"))
        n, groups = int(r["workers"]), config["groups"]
        assert len(audit) == n * groups
        assert int(r["started"]) == int(r["stopped"]) == int(r["dropped"]) == n * groups
        assert int(r["messages"]) == int(r["succeeded"]) and int(r["timeouts"]) == 0
        expected = cpus(config["actor-cpus"])
        assert not (expected & cpus(config["driver-cpus"]))
        for i, record in enumerate(audit):
            if r["variant"] == "async":
                assert cpus(record["allowed_cpus"]) == expected, (r["stem"], record)
            else:
                assert record["thread"] == f"spawned-shard-{i % n}"
                assert cpus(record["allowed_cpus"]) == {2 + i % n}
    summary = rows(a.report / "SUMMARY.csv")
    final_cells = [s for s in summary if s["phase"] == "pinned-pipeline"]
    assert len(final_cells) == 24 and all(int(s["samples"]) == 3 for s in final_cells)
    assert len(rows(a.report / "RESOURCES.csv")) == 17
    assert len(list((a.report / "graphs").glob("*.png"))) == 51
    assert len(list((a.report / "graphs").glob("*.svg"))) == 51
    assert sum(r["phase"] == "profile" for r in combined) == 144
    print(
        f"PASS: {len(manifest)} hashes; 5744 original rows/45 blocks; 96 CPU audits; 24 pinned cells; 17 resource windows; 51 figures; 144 profiles"
    )


if __name__ == "__main__":
    main()
