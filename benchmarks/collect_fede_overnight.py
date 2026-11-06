#!/usr/bin/env python3
"""Archive completed, version-separated experiment blocks without changing inputs."""

import argparse
import csv
import hashlib
import json
from pathlib import Path
import shutil
import tarfile


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--run", action="append", required=True, help="unique name=directory"
    )
    p.add_argument("--bundle", action="append", default=[], type=Path)
    p.add_argument("--evidence", action="append", default=[], type=Path)
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    if not __debug__:
        raise RuntimeError("Run without -O: validation assertions are required")
    a.output.mkdir(parents=True, exist_ok=False)
    records = []
    inputs = {}
    revisions = {}
    for spec in a.run:
        name, directory = spec.split("=", 1)
        root = Path(directory)
        assert name not in inputs and "/" not in name and name not in [".", ".."]
        meta = json.loads((root / "metadata.json").read_text())
        assert meta.get("finished_utc") and not meta["dirty"], (
            name,
            "unfinished or dirty",
        )
        assert not meta.get("stopped_at_deadline"), (name, "truncated block")
        original = list(csv.DictReader((root / "raw.csv").open()))
        assert original and all(r["status"] == "ok" for r in original)
        if meta.get("planned_samples"):
            assert len(original) == meta["planned_samples"]
        for r in original:
            if r.get("succeeded") not in [None, ""]:
                assert int(r["succeeded"]) + int(r["timeouts"]) == int(r["messages"])
                assert (
                    int(r.get("final_queued", 0))
                    == int(r.get("final_outstanding", 0))
                    == 0
                )
            else:
                assert int(r["started"]) == int(r["stopped"]) == int(r["dropped"])
            records.append(
                dict(
                    r,
                    dataset=name,
                    revision=meta["revision"],
                    original_stem=r["stem"],
                    stem=name + "/" + r["stem"],
                )
            )
        dest = a.output / "runs" / name
        dest.mkdir(parents=True)
        for file in ["metadata.json", "raw.csv", "build.log"]:
            if (root / file).exists():
                shutil.copyfile(root / file, dest / file)
        for directory in ["series", "threads", "profiles"]:
            if (root / directory).exists():
                shutil.copytree(root / directory, a.output / directory / name)
        if (root / "logs").exists():
            with tarfile.open(
                a.output / ("diagnostics-" + name + ".tar.gz"), "w:gz"
            ) as archive:
                archive.add(root / "logs", arcname="logs")
        binaries = meta.get("binaries", meta.get("binary_sha256"))
        inputs[name] = dict(
            original_directory=str(root.resolve()),
            samples=len(original),
            revision=meta["revision"],
            started_utc=meta["started_utc"],
            finished_utc=meta["finished_utc"],
            actor_cpus=meta["actor_cpus"],
            driver_cpus=meta["driver_cpus"],
            metadata_sha256=sha(root / "metadata.json"),
            raw_sha256=sha(root / "raw.csv"),
            binaries=binaries,
        )
        # Exact source and binaries remain properties of each block; never infer cross-version identity.
        revisions.setdefault(meta["revision"], []).append(name)
    fields = list(dict.fromkeys(k for r in records for k in r))
    with (a.output / "raw.csv").open("w") as f:
        writer = csv.DictWriter(f, fields, lineterminator="\n")
        writer.writeheader()
        writer.writerows(records)
    for category, files in [("source-bundles", a.bundle), ("evidence", a.evidence)]:
        (a.output / category).mkdir()
        for file in files:
            target = a.output / category / file.name
            assert not target.exists(), file.name
            shutil.copyfile(file, target)
    metadata = dict(
        inputs=inputs,
        revisions=revisions,
        samples=len(records),
        derivation="Original CSVs/metadata retained in runs/. Derived rows add dataset, revision and original_stem, prefix stem with dataset; all original numeric and config/experiment fields are unchanged. Blocks and source revisions are never pooled.",
    )
    (a.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    manifest = {
        str(f.relative_to(a.output)): sha(f)
        for f in sorted(a.output.rglob("*"))
        if f.is_file()
    }
    (a.output / "sha256.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(
        f"Archived {len(records)} successful processes in {len(inputs)} separate blocks; {len(manifest)} hashed artifacts"
    )


if __name__ == "__main__":
    main()
