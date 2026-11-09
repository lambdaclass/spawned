# Synchronous actors without Tokio

This independent workspace disables default features on both Spawned crates.
It exercises typed protocols, requests, links, monitors, timers, registry entries,
completion waits and optional metrics. No async actor runtime is created.

```sh
cargo run --locked --manifest-path examples/thread_only/Cargo.toml
cargo run --locked --manifest-path examples/thread_only/Cargo.toml --features metrics,async
python3 scripts/check-thread-only.py
```

The example's `async` feature is deliberately unrelated to Spawned's feature: it
checks that protocol generation follows dependency features rather than a
consumer feature of the same name. The graph check includes normal/build edges;
repository dev-dependencies and other workspace members can still enable Tokio.

Use `spawned-concurrency = { version = "...", default-features = false, features = ["threads"] }`
for this mode. Native actors are always available; `threads` is an explicit alias.
If also depending directly on `spawned-rt`, disable its defaults too. Cargo feature
unification means another dependency enabling `async` will bring Tokio back.
ActorSystem, execution groups and supervisors currently require `async`; this
example uses the separate synchronous `threads::Actor` API. Native actors still
use one OS thread per actor and native timer/monitor helpers; this is a dependency
boundary, not a small bounded worker pool.
