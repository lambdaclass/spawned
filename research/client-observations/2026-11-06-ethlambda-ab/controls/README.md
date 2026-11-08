# Controls for the committed eth4 / eth5 test branch

This is diagnostic preparation, not a latency fix, release or deployment.
The live build labels remained `4117c5a` and `5259dee` on the follow-up check.
A version label identifies the reported commit; an image digest/build provenance
is still needed to exclude uncommitted build-time patches or differing flags.

The deployed eth5 commit is three commits ahead of eth4:

1. `a2d9661b99de58baaa43a2316a16546f1d43c159`: Spawned 0.5 → 0.6, with metrics off.
2. `df038cebd7fe1020838053895af4f087b1d1838b`: full framework timing and the RPC
   metrics exporter, plus its test/docs.
3. `5259dee25b81cc5275a4b0f441f24361d4f4b940`: preset-dependent unit-test fix.

The third change is inside test code. No unrelated mainnet state-transition
algorithm change was identified in that committed diff. The lockfile also changes
three Linux-relevant networking edges: hyper-util, quinn and quinn-udp resolve
socket2 0.6.3 instead of eth4's 0.5.10. Their own versions stay unchanged. Tokio
and libp2p-tcp use socket2 0.6.3 in both builds. This is a confound, not a known
socket2 defect; source review found no immediate Linux Nagle/blocking explanation.

## Prepared control

Start from the first commit (`a2d9661b`) and apply
[`socket2-baseline.patch`](socket2-baseline.patch). This keeps published Spawned
0.6.0 with no metrics feature, and restores only the three networking edges to
eth4's choices. It does not include Spawned main's unreleased one-worker fix.

In an existing Ethlambda clone, use an isolated worktree:

```sh
git fetch origin a2d9661b99de58baaa43a2316a16546f1d43c159
git worktree add --detach /tmp/ethlambda-06-control a2d9661b99de58baaa43a2316a16546f1d43c159
git -C /tmp/ethlambda-06-control apply /path/to/socket2-baseline.patch
cargo metadata --manifest-path /tmp/ethlambda-06-control/Cargo.toml --locked --filter-platform x86_64-unknown-linux-gnu --format-version 1 > /tmp/ethlambda-06-control-metadata.json
```

Cargo's locked Linux graph was validated on fede using Rust/Cargo 1.98.1, CPUs
2–5 and nice 10. [The extracted validation](metadata-validation.json) records:
Spawned 0.6.0 features `[]`; hyper-util 0.1.20, quinn 0.11.9 and quinn-udp 0.5.14
all resolving socket2 0.5.10; and the exact lockfile checksum. This was graph
validation, separate from the matching-toolchain builds below. An initial offline metadata attempt lacked the libssz git
source; normal locked resolution succeeded without changing the lockfile.

## Comparison matrix

| Variant | Client source | Spawned metrics | Networking graph |
|---|---|---|---|
| Original eth4 baseline | 4117c5a | off, 0.5 | eth4 |
| Prepared control | a2d9661b + supplied patch | off, 0.6 | eth4 |
| Metrics-only comparison | 5259dee + supplied patch | full, 0.6 | eth4 |
| Existing eth5 build | 5259dee | full, 0.6 | changed |

The patch targets the Cargo.lock sections shared by the two 0.6 commits. Preserve
actual Rust version, release/allocator/CPU flags, data directory, P2P identity,
custody columns, peer configuration and EL endpoint/configuration across variants.
Compare the same node in a crossover or use a fixed replay; matching block roots
on different custody identities alone does not normalize network work. Exclude
restart warm-up and rolling-window overlap. Capture EL attempts, errors, response
arrival and answer application, not only completed-call percentiles.

The no-metrics control compiles out the framework collector and therefore has no
`spawned_*` series. The existing application metrics remain the comparison basis.
A 0.6 metrics-off run resembling 0.5 would implicate the added instrumentation or
other controlled factors; it would not by itself identify a faulty line of code.

## Built comparison artifacts

Both optimized binaries have now been built on fede with **Rust 1.97.1**, the
compiler reported by the deployed nodes. They use the same repository release
profile (fat LTO, one codegen unit), x86-64-v3 flags and host C compiler. The
[paired build record](builds/paired-builds.json) contains paths, SHA-256 hashes,
versions, lock hashes and compiler details. These are build/smoke results, not
performance results; neither binary has been deployed.

| Artifact | Source | Build | Startup checks |
|---|---|---|---|
| `ethlambda-baseline05` | `4117c5a` / Spawned 0.5.0 | passed | version and beacon help passed |
| `ethlambda-control06` | `a2d9661b` + lock-only patch / Spawned 0.6.0 | passed | version and beacon help passed |

Stable binaries are on fede under
`/tmp/spawned-pr-security-review/ethlambda-control-build/artifacts/`.
Their sizes are about 47.6 MB each; binaries and Cargo targets are not committed.
The control's private diagnostic commit is `c585f3d2b8e816e24e7029e418a484ee3396c30a`.
It changes only Cargo.lock relative to `a2d9661b`; the
[source bundle](builds/control-source.bundle) preserves that commit with the
public base as a prerequisite. Its reported build version includes `c585f3d`.

The [control build log](builds/control06/build.log) and
[baseline build log](builds/baseline05/build.log) retain compilation evidence.
Both binaries run `--version` and `beacon --help` without `LD_LIBRARY_PATH`, and
`ldd` reports no missing libraries or Nix store dependency. Compatibility with
the canary host/image must still be checked there. Build scripts are archived in
[build_scripts](build_scripts/); they assume the source trees, downloaded Rust
manifest and native build dependencies already exist on fede. They limit builds
to CPUs 2–5, nice 10, two build jobs, and monitor their own process group with a
16 GiB RSS and 45-minute time limit. Recorded peak build RSS was below 3 GiB.
The faster second build reused Cargo artifacts, so build times are not a version
performance comparison.

## Collect a controlled canary window

The next step requires eth5's SSH/deployment location and existing service
configuration. No deployment inventory was found in the inspected repositories.
Preserve the data directory, P2P identity/custody assignment, peers, resource
limits and execution-client endpoint/configuration. Save the current executable
or image digest and launch configuration for rollback. Check artifact SHA-256
and host compatibility before starting a variant. Record the actual deployed
digest/configuration and collect correlated EL attempt logs.

Run the 0.5 baseline and then the metrics-off 0.6 control on the **same canary**,
keeping eth4 as a continuous reference; repeat/reverse the order to distinguish
version effects from time-varying load. Stop cleanly between variants. For each
run, wait at least 30 minutes after startup, then collect a window containing at
least 100 matched blocks. Do not overlap restart warm-up or rolling metrics
from the previous run. Matching block roots on two different node identities
still does not equalize their custody workload.

The read-only [collector](collect_comparison.py) checks reported versions and
compiler/target, process start time, scrape coverage, and (with `--metrics-off`)
absence of exported framework counters. Replace both timestamps with an actual
completed, warm control interval:

```sh
python3 controls/collect_comparison.py \
  --start '<warm-window-start-UTC>' --end '<warm-window-end-UTC>' \
  --expected-a 4117c5a --expected-b c585f3d --metrics-off \
  --out /tmp/ethlambda-control06-observation
```

Run from this study directory. For the rebuilt 0.5 canary use
`--expected-b diagnostic/spawned05-baseline-4117c5a`. Windows must be at most six
hours and end at least two minutes before collection. The output directory must
be empty; the collector refuses to overwrite evidence. It saves raw query
responses, missing-completion counts, per-phase sample counts and empirical
percentiles. `COMPLETE` means collection gates passed, not causal attribution.
Build labels cannot establish image provenance, hardware or EL parity.

[Collector validation](collector-validation/validation.json) records a successful
historical extraction of 599 matched completed blocks, refusal of the wrong
control build, refusal of the wrong compiler and preservation of an existing
output directory. Its historical output uses the **original deployed builds**;
it is not a run of either newly built binary. Full validation inputs and outputs
are archived alongside that record. Release remains held while the latency
cause is unresolved.
