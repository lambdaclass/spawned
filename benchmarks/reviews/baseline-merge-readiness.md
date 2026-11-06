# Baseline merge readiness

The fresh PR review found watchdog/CLI robustness issues in the historical runner.
They are corrected without changing the archived measurements or observer policy:

- A process-group watchdog kills descendants on timeout/interruption and saves
  stdout/stderr before raising. A real parent/grandchild regression passes.
- Unknown, duplicate and valueless flags fail before actor creation; on/off and
  true/false boolean controls are explicit. Manual binary checks pass.
- Missing/unparseable process resource measurements fail with retained logs.
- New CSV output uses LF. Disabled allocation summaries are blank, and mixed
  enabled/disabled cells serialize with a union of column names. A two-case smoke
  campaign passes; historical raw and summary files remain unchanged.
- The bench-only hash is named accordingly. Compiler/Cargo versions were already
  recorded; the README now also calls out the historical mixed Nix packages.

Agy's focused follow-up found no remaining blockers for merging this baseline
infrastructure after these changes. Historical review archives are retained as
requested provenance, explicitly distinguished from adopted policy. Their numeric
thresholds are proposals, not production acceptance rules. Later studies address
observer asymmetry and additional backends separately.

Generated reports, graphs, data and validation logs have opaque text diffs to keep
automated code-review inputs bounded. They remain viewable/downloadable as files;
scientific result reviews use the summaries, raw data and provenance explicitly.
