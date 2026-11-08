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
all resolving socket2 0.5.10; and the exact lockfile checksum. No control binary
was built or deployed. An initial offline metadata attempt lacked the libssz git
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
No control binary has been built or deployed.
