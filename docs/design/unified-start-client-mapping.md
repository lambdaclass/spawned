# Unified start (0.9): client mapping

Every start call in the production clients, mapped from the 0.8 API to the
[unified start design](unified-start.md). Sources: shallow clones on fede of
`lambdaclass/ethrex` `spawned-v0.8` at `c1163b7` and `lambdaclass/ethlambda`
`spawned-v0.8` at `85990aa`.

Method: grep for `.start(`, `start_on(`, `start_with_backend(`,
`start_linked(`, `ExecutionGroup::start`, `ChildSpec::new`, `.ready()`,
`register_name`, `TestSystem` and `Mock` in files that use spawned. Each hit
was then checked by hand. The enclosing function, and whether it runs inside
an actor's handler, come from the source.

## Summary

| | ethrex | ethlambda | Total |
|---|---|---|---|
| `.start()` → `.spawn()` | 20 | 4 | 24 |
| `start_with_backend(b)` → `with_options(b).spawn()` | 7 | 0 | 7 |
| `start_on(g)` → `with_options(g).spawn()` | 4 | 2 | 6 |
| `Mock::..start()` → `Mock::..spawn()` | 6 | 8 | 14 |
| **Sites that change** | **37** | **14** | **51** |
| `start_linked`, `ExecutionGroup::start`, `ready()`, `register_name`, `TestSystem` | 0 | 0 | 0 |
| `ChildSpec::new` and supervisor `start()` (unchanged) | 1 (+2 test fixtures) | 2 | 3 |

- **Behaviour.** The rewrite in the "0.9" column keeps the current behaviour
  at every site.
- **Inheritance changes behaviour at no site.** Neither client uses
  `start_linked`, and `spawn`/`start` never inherit.
- **Error types.** Six sites (marked E) sit in functions whose error type
  comes from `start_on`'s `SystemError`. They return or convert `StartError`
  instead.
- **Optional upgrades** (marked U): an async site may use
  `.start().await?` instead of `.spawn()` to get init failures (today they
  are only logged or invisible). This is a behaviour change: the caller waits
  for `started()`.
- **Handler sites** (marked H) start a child from inside an actor's turn.
  They stay unmanaged, which is the S6 footgun. `start_link` (or
  `placement_of(ctx)`) would admit the child to the parent's group, so that
  it joins ordered shutdown and counts against the group's capacity. That
  is a client decision.

## ethrex (37)

| # | Site | Enclosing function | 0.8 | 0.9 | Notes |
|---|---|---|---|---|---|
| 1 | `crates/l2/sequencer/native_rollup/l1_watcher.rs:334` | `NativeL1Watcher::spawn` (sync) | `watcher.start()` | `watcher.spawn()` | |
| 2 | `crates/l2/sequencer/native_rollup/l1_advancer.rs:488` | `NativeL1Advancer::spawn` (sync) | `advancer.start()` | `advancer.spawn()` | |
| 3 | `crates/l2/sequencer/native_rollup/block_producer.rs:504` | `NativeBlockProducer::spawn` (sync) | `producer.start()` | `producer.spawn()` | |
| 4 | `crates/l2/sequencer/l1_watcher.rs:194` | `L1Watcher::spawn` (sync) | `Ok(state.start())` | `Ok(state.spawn())` | |
| 5 | `crates/l2/sequencer/state_updater.rs:110` | `StateUpdater::spawn` (async) | `state_updater.start()` | `state_updater.spawn()` | U |
| 6 | `crates/l2/sequencer/proof_coordinator.rs:110` | `ProofCoordinator::spawn` (async) | `state.start()` | `state.spawn()` | U |
| 7 | `crates/l2/sequencer/proof_coordinator.rs:385` | `ConnectionHandler::spawn` (async), called from `ProofCoordinator::handle_incoming_connection` | `connection_handler.start()` | `connection_handler.spawn()` | U, H |
| 8 | `crates/l2/sequencer/l1_committer.rs:253` | `L1Committer::spawn` (async) | `state.start_with_backend(Backend::Blocking)` | `state.with_options(Backend::Blocking).spawn()?` | U |
| 9 | `crates/l2/sequencer/block_producer.rs:350` | `BlockProducer::spawn` (async) | `block_producer.start_with_backend(Backend::Blocking)` | `block_producer.with_options(Backend::Blocking).spawn()?` | U |
| 10 | `crates/l2/sequencer/metrics.rs:69` | `MetricsGatherer::spawn` (async) | `Ok(metrics.start())` | `Ok(metrics.spawn())` | U |
| 11 | `crates/l2/sequencer/l1_proof_sender.rs:176` | `L1ProofSender::spawn` (async) | `state.start()` | `state.spawn()` | U |
| 12 | `crates/l2/based/block_fetcher.rs:337` | `BlockFetcher::spawn` (async) | `state.start()` | `state.spawn()` | U |
| 13 | `crates/networking/rpc/subscription_manager.rs:98` | `SubscriptionManager::spawn` (sync) | `..default().start()` | `..default().spawn()` | |
| 14 | `crates/networking/rpc/subscription_manager.rs:104` | `SubscriptionManager::spawn_on` (sync) | `..default().start_on(group)` | `..default().with_options(group).spawn()` | E |
| 15 | `crates/networking/p2p/tx_broadcaster.rs:206` | `TxBroadcaster::spawn_in` (sync) | `Ok(state.start_on(group)?)` | `Ok(state.with_options(group).spawn()?)` | E |
| 16 | `crates/networking/p2p/tx_broadcaster.rs:207` | same | `Ok(state.start())` | `Ok(state.spawn())` | |
| 17 | `crates/networking/p2p/rlpx/connection/server.rs:213` | `PeerConnection::spawn_as_receiver` (sync), called from the accept loop in `network.rs:288` | `connection.start()` | `connection.spawn()` | |
| 18 | `crates/networking/p2p/rlpx/connection/server.rs:228` | `PeerConnection::spawn_as_initiator` (sync), called from `RLPxInitiator` handlers (`initiator.rs:97`, `:113`) | `connection.start()` | `connection.spawn()` | H; the dynamic supervisor (#245) replaces this site |
| 19 | `crates/networking/p2p/rlpx/connection/server.rs:292` | `PeerConnection::unestablished` (sync) | `connection.start()` | `connection.spawn()` | |
| 20 | `crates/networking/p2p/rlpx/connection/server.rs:4024` | test helper `connection()` | `server.start()` | `server.spawn()` | test |
| 21 | `crates/networking/p2p/rlpx/initiator.rs:50` | `RLPxInitiator::spawn_on` (sync) | `..new(context).start_on(group)?` | `..new(context).with_options(group).spawn()?` | E |
| 22 | `crates/networking/p2p/rlpx/initiator.rs:66` | `RLPxInitiator::spawn_with_backend` (sync, returns `ActorRef`) | `state.start_with_backend(b)` | `state.with_options(b).spawn().expect("executor construction failed")` | it fails on invalid options or an executor construction failure (for example, a thread that cannot be spawned), which bare 0.8 `start_with_backend` also panicked on |
| 23 | `crates/networking/p2p/rlpx/initiator.rs:67` | same | `state.start()` | `state.spawn()` | |
| 24 | `crates/networking/p2p/peer_table.rs:616` | `PeerTableServer::spawn_with_filter` (sync) | `..start()` | `..spawn()` | |
| 25 | `crates/networking/p2p/peer_table.rs:635` | `PeerTableServer::spawn_on` (sync) | `.start_on(group)` | `.with_options(group).spawn()` | E |
| 26 | `crates/networking/p2p/discovery/server.rs:156` | `DiscoveryServer::spawn` (async) | `.start()` | `.spawn()` | U |
| 27 | `crates/prover/src/prover.rs:251` | `start_prover` (async) | `prover.start_with_backend(Backend::Blocking)` | `prover.with_options(Backend::Blocking).spawn()?` | U |
| 28 | `crates/prover/src/prover.rs:260` | same | same | same | U |
| 29 | `crates/prover/src/prover.rs:268` | same | same | same | U |
| 30 | `crates/prover/src/prover.rs:276` | same | same | same | U |
| 31 | `tooling/monitor/src/app.rs:103` | `EthrexMonitor::spawn` (async) | `monitor.start();` | `monitor.spawn();` | U; the 0.8 statement would still start the actor (eager stage 1), with a `#[must_use]` warning that clippy `-D warnings` rejects |
| 32 | `test/tests/p2p/peer_table_timeout_tests.rs:33` | test (async) | `Mock::new()..start()` | `Mock::new()..spawn()` | test |
| 33 | `test/tests/p2p/peer_table_timeout_tests.rs:47` | test | same | same | test |
| 34 | `test/tests/p2p/peer_table_timeout_tests.rs:59` | test | same | same | test |
| 35 | `test/tests/p2p/shutdown_order_tests.rs:141` | test | `Mock::new()..start()` | `Mock::new()..spawn()` | test |
| 36 | `test/tests/p2p/shutdown_order_tests.rs:169` | test | same | same | test |
| 37 | `test/tests/p2p/shutdown_order_tests.rs:196` | test | `Mock::new().start()` | `Mock::new().spawn()` | test |

Unchanged:

- `crates/networking/p2p/discovery/supervised.rs:145`: `ChildSpec::new("discovery", group, ..)`,
  no `startup_timeout` and no bounds. The supervisor's `start()` is
  unchanged. Two test fixtures (`:244`, `:271`) build an `ActorSystem` and a
  group.
- `cmd/ethrex/initializers.rs:1043`: `ActorSystem::new`, `async_group`.

E sites: #14 and #25 return `Result<_, SystemError>`, which becomes
`StartError`. #15 and #21 propagate with `?` into the caller's error type,
which needs `From<StartError>`.

## ethlambda (14)

| # | Site | Enclosing function | 0.8 | 0.9 | Notes |
|---|---|---|---|---|---|
| 1 | `crates/net/p2p/src/lib.rs:1277` | `P2P::spawn` (sync) | `server.start_on(group)?` | `server.with_options(group).spawn()?` | E |
| 2 | `crates/blockchain/src/lib.rs:452` | `BlockChain::start_actor` (sync; the `chain` thread group) | `.start_on(group)?` | `.with_options(group).spawn()?` | E |
| 3 | `crates/blockchain/src/lib.rs:6393` | test | `beacon_server(..).start()` | `beacon_server(..).spawn()` | test |
| 4 | `crates/blockchain/src/lib.rs:6428` | test | same | same | test |
| 5 | `crates/blockchain/src/lib.rs:6450` | test | same | same | test |
| 6 | `crates/net/p2p/src/beacon/verdict.rs:1832` | test helper `run_checks` (async) | `..await.start()` | `..await.spawn()` | test |
| 7 | `crates/net/p2p/src/beacon/column_checks.rs:133` | test | `Mock::new().allow_sends().start()` | `Mock::new().allow_sends().spawn()` | test |
| 8 | `crates/net/p2p/src/beacon/column_checks.rs:164` | test | same | same | test |
| 9 | `crates/net/p2p/src/beacon/envelope_checks.rs:191` | test | same | same | test |
| 10 | `crates/net/p2p/src/beacon/verdict.rs:764` | test helper `recording_chain` (sync) | same | same | test |
| 11 | `crates/net/p2p/src/beacon/verdict.rs:771` | test helper `silent_chain` (sync) | `Mock::new().start()` | `Mock::new().spawn()` | test |
| 12 | `crates/net/p2p/src/req_resp/envelope_client.rs:615` | test | `Mock::new().start()` | `Mock::new().spawn()` | test |
| 13 | `crates/net/p2p/src/req_resp/envelope_client.rs:702` | test | `Mock::new().allow_sends().start()` | `Mock::new().allow_sends().spawn()` | test |
| 14 | `crates/net/p2p/src/gossipsub/handler.rs:1736` | test | same | same | test |

Unchanged:

- `bin/ethlambda/src/main.rs:981` and `bin/ethlambda/src/http_server.rs:216`:
  `ChildSpec::new("http", ..)` with `shutdown_timeout` only.
- The supervisor `start().await` calls.
- `#[actor(max_native_jobs = ..)]` (`crates/net/p2p/src/lib.rs:1572`): an
  `Actor` constant, unchanged.

## Related work

- **ethrex.** Site #18 is the per-connection actor that the dynamic
  supervisor design (#245) moves under a `DynamicSupervisor`. With unified
  start, its RLPx handshake becomes `Init::continue_with(Handshake)`, so no
  peer-table message is handled before it (gap report 3.2.1). This is not a
  barrier against termination: readiness commits before the continuation
  runs, so a `terminate_child`, a kill or supervisor shutdown can end the
  connection before or during the handshake (design section 8). The
  connection's resource owners must therefore handle a peer that never
  finished its handshake, as they do for one that fails it.
- **ethlambda.** Site #2 is the chain actor on its own one-worker
  `thread_group` (0.8 replaced `Backend::Thread` with it). Unified start
  keeps that placement, and a `start_link` from the chain actor would
  inherit the `chain` group: it would need a second worker, or `Capacity` is
  returned.
