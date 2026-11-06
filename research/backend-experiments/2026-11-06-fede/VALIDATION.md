# Validation of the delivered study

The source-specific correctness gates and expected negative controls are archived
under data/evidence/validation-and-orchestration.tar.gz. V8 source `2cec0a6` passes 20
RT tests, selected Deque actor suites, workspace all-feature tests, three release
bench builds and Clippy. V9 measured source `a8471f7` and retained source `20260cd` each
pass 21 RT tests, both selected pinned park/spin actor suites, workspace all-feature
tests, release bench build and all-target/all-feature Clippy. The inherited
badly_behaved_thread_non_blocking exclusion remains explicit in selected suites.
These are finite gates, not a proof of every possible runtime schedule.

After measurement, on fede:

- The collector accepts all 45 planned blocks and 5,744 successful process rows,
  requiring finished/clean metadata, exact counts and result conservation.
- verify_fede_overnight.py verifies 11,889 artifact hashes and exact original-row
  identity, all 96 V9 actor CPU audits, 24 full pinned cells with three fresh
  processes each, 17 resource windows, 51 figures and 144 independent profiles.
- The final renderer rejects clipped plotted data and overlapping title/legend
  bounds. Its bound check handles inverted axes. Visual inspection covers the
  corrected V8 graph/scale/offered/resource views and both V9 profile figures;
  earlier representative caller/clock/factorial figures were also inspected.
- A second render from the archived data reproduces all 107 report files byte for
  byte, including PNG/SVG graphs, CSV summaries and Markdown/HTML reports.

The raw [verification output](verification.log), [regeneration output](reproduction.log),
[data manifest](data/sha256.json), [report hashes](report-sha256.json) and
[analysis source hashes](analysis-source-sha256.json) accompany the archive.
The first generic bound guard incorrectly treated inverted bar axes as clipping;
that analysis-only check was corrected before the successful regeneration.
No workload scores changed. Visual QA also corrected the V9 per-series axis
bound before publication; the final graphs include the spin CPU curve.

From the repository root, verify the delivered archive:

```sh
python benchmarks/verify_fede_overnight.py \
  --data research/backend-experiments/2026-11-06-fede/data \
  --report research/backend-experiments/2026-11-06-fede/report
```

The independent Agy review approved the supplied final interpretation with no
blockers. It reviewed tables and methodology; it did not reproduce the raw runs.
See [review scope and qualifications](reviews/AGY.md). Runtime/profile changes
were merged only after final-source gates, exact-head technical CI and the
user-authorized independent review path passed. The research commit adds analysis
and documentation, with no runtime or public API change.
