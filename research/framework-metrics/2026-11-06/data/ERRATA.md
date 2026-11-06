# Metadata and excluded data

The initial `metadata.json` reports nice=10 because it recorded the outer driver
priority. The measured children also ran `nice -n 10`; Linux clamps the combined
value to **19**. An active clean-confirmation baseline process was checked with
`ps` (PID 3891223, NI 19). All measured variants shared this effective priority.
Reproduction scripts now set and record the requested effective nice=19 directly.
Builds inherited outer nice=10. RUSTFLAGS was empty; CARGO_BUILD_JOBS=4.

`confirmation-overlapped.csv` and its metadata are excluded from every table and
figure. Compilation of the release gates overlapped the final part of that run.
The entire 56-process set was discarded, then repeated after all gates finished.
`confirmation.csv` is that clean repeat, using the same frozen binaries, seed,
seven fresh processes per cell and three seconds per process. No conclusions use
the overlapped measurements. Initial `raw.csv` did not overlap our builds/tests.
