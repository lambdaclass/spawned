# Dynamic supervisor: client mapping

Companion to the [dynamic supervisor design](dynamic-supervisor.md) (ROADMAP
rank 12a). Maps the one runtime-spawned actor population in the clients,
ethrex's per-peer connection actors, onto the proposed API, and records that
ethlambda has none. Sketches are illustrative, not patches; nothing in either
client was changed.

Revisions read (source reading only, nothing built or run):

| Repository | Ref | Commit | spawned |
|---|---|---|---|
| ethrex | `main` | [`fd5c57dd`](https://github.com/lambdaclass/ethrex/tree/fd5c57ddada097952660f6b749c739119e4de852) | 0.5.0 |
| ethrex | `spawned-v0.7` | [`0dd4c105`](https://github.com/lambdaclass/ethrex/tree/0dd4c105f65d544b0fb5e92d8634a1699127f826) | 0.7.3 |
| ethrex | PR [#7435](https://github.com/lambdaclass/ethrex/pull/7435) (`ConnectionId` guard) | merge [`fc44a9fb`](https://github.com/lambdaclass/ethrex/commit/fc44a9fb25451a17656e88e8e15b4443f747f661) | |
| ethlambda | `main` | [`c3f5f566`](https://github.com/lambdaclass/ethlambda/tree/c3f5f566cb3a61e3d388c66ce21794c18a6618ce) | 0.5.0 |
| ethlambda | `spawned-v0.7` | [`6a12bc75`](https://github.com/lambdaclass/ethlambda/tree/6a12bc753ae18c9d63f3dbf930edc700c4c4704c) | 0.7.3 |

`crates/networking/p2p/rlpx/connection/server.rs` and `handshake.rs` are
identical on both ethrex branches; their links below use `spawned-v0.7`.
Short forms: **V** = `https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826`,
**M** = `https://github.com/lambdaclass/ethrex/blob/fd5c57ddada097952660f6b749c739119e4de852`.

## ethrex today

### Who starts connections

One `PeerConnectionServer` actor per TCP connection, started with a bare
`.start()` (unmanaged, S6) and owned by nothing:

- **Accept:** `PeerConnection::spawn_as_receiver(context, peer_addr, stream, permit)`
  ([V server.rs:191-209](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L191-L209)),
  called from the accept loop `serve_p2p_requests`, which discards the result
  ([V network.rs:253-290](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/network.rs#L253-L290)).
- **Dial:** `PeerConnection::spawn_as_initiator(context, &node)`
  ([V server.rs:211-224](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L211-L224)),
  called by the `RLPxInitiator` actor
  ([V initiator.rs:91-120](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/initiator.rs#L91-L120)),
  result also discarded.

```rust
// server.rs:191-224 (both branches)
pub fn spawn_as_receiver(context: P2PContext, peer_addr: SocketAddr, stream: TcpStream,
    admission_permit: tokio::sync::OwnedSemaphorePermit) -> PeerConnection {
    let state = ConnectionState::Receiver(Receiver { context, peer_addr, stream: Arc::new(stream) });
    let connection = PeerConnectionServer { state, _admission_permit: Some(admission_permit) };
    Self { handle: connection.start() }
}
pub fn spawn_as_initiator(context: P2PContext, node: &Node) -> PeerConnection {
    let state = ConnectionState::Initiator(Initiator { context, node: node.clone() });
    // Outbound dials are not admission-capped (we initiate them); inbound is the attack surface.
    let connection = PeerConnectionServer { state, _admission_permit: None };
    Self { handle: connection.start() }
}
```

On `spawned-v0.7` the singletons (peer table, initiator, broadcaster,
subscription manager) moved onto an `ActorSystem` group and discovery under a
OneForOne supervisor, but connections did not; the comment says why:
"Per-connection actors (`PeerConnectionServer`) are not admitted: they are
created and dropped per peer, and the system only owns a fixed set"
([V initializers.rs:1041-1042](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/cmd/ethrex/initializers.rs#L1041-L1042)).
So connections run on whatever runtime is current at the call site, and
system shutdown does not stop them.

### How they are tracked

After the handshake the connection registers itself with the peer table,
`IndexMap<H256, PeerData>`
([V peer_table.rs:546](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/peer_table.rs#L546)),
passing its own `ActorRef` and a `ConnectionId`
([server.rs:1074-1101](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L1074-L1101)).
The first connection to a node wins; a duplicate gets `false`, sends
Disconnect(AlreadyConnected) and stops.

`ConnectionId` is a process-global counter added by #7435 after a duplicate
connection's cleanup removed the live entry
([server.rs:176-188](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L176-L188)):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionId(u64);
impl ConnectionId {
    pub(crate) fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}
```

### How they are cleaned up

Nothing monitors or links a connection. It reports its own death from its
`stopped()` hook
([server.rs:623-671](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L623-L671)):

```rust
// The peer table removes the node only if it lists this connection for it, so
// a duplicate connection to an already-connected node leaves the live one
// listed. Sent even when registration failed: a registration the peer table
// applied after this connection stopped waiting for it is undone here.
if let Err(e) = established_state.peer_table.remove_peer(
    established_state.node.node_id(),
    established_state.connection_id,
) { debug!("Failed to remove peer from table: {e}"); }
if established_state.registered_in_peer_table
    && let Err(e) = established_state.tx_broadcaster.remove_peer(established_state.node.node_id())
{ debug!("Failed to remove peer from tx broadcaster: {e}"); }
established_state.teardown().await;
```

and the table checks the guard
([V peer_table.rs:693-711](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/peer_table.rs#L693-L711)):

```rust
if let Some(peer) = self.peers.get(&msg.node_id) {
    if peer.connection_id != Some(msg.connection_id) { return; }
    self.peers.swap_remove(&msg.node_id);
}
self.sessions.remove(&msg.node_id);
```

Gaps in this scheme:

- If `stopped()` does not run, the entry is never removed. Today that is a
  panic in `started()`; under v0.8 it is also a kill, an untrapped exit
  signal and the supervisor's or system's shutdown of a non-trapping actor
  (exit-signals design, decision 2).
- A registration that timed out on the connection side but was applied by
  the table is undone only because `stopped()` sends `remove_peer`
  unconditionally; the comment above exists for this case.
- At node shutdown nothing stops connections; their `remove_peer` sends fail
  silently once the table is gone ([V shutdown.rs:81-153](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/cmd/ethrex/shutdown.rs#L81-L153)
  stops the system's actors only).

### Limits

- Inbound: a semaphore of `MAX_INBOUND_CONNECTIONS = 150`
  ([V network.rs:68](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/network.rs#L68)),
  `try_acquire_owned()` in the accept loop; when full the stream is dropped
  before any actor exists. The permit lives in actor state
  (`_admission_permit`) and is released when the state drops.
- Outbound dials are not capped by it; `target_peers` (100) gates dialing and
  registration (`TooManyPeers`).

### Restarts

None. A dead connection is removed; the node may be re-dialed later through
discovery. In supervision terms every connection is **Temporary**.

## Mapping

| ethrex today | With the dynamic supervisor |
|---|---|
| `.start()`, unmanaged, on the caller's runtime | `start_child(args)` on a `DynamicSupervisor<PeerConnectionServer, ConnArgs>`, placed on the `p2p` group |
| no owner; shutdown does not stop connections | the supervisor is a shutdown unit of the existing `ActorSystem`; admitted after the peer table, so in reverse admission order connections stop before the table, all at once, each killed after its `shutdown_timeout` |
| `Semaphore(150)` plus `_admission_permit` in actor state | `max_children = Some(150)` on an inbound supervisor; `Err(MaxChildren)` instead of `try_acquire_owned()` failing. The slot is freed after the actor's cleanup, as the permit is freed when the state drops |
| outbound uncapped | a second supervisor for dials without `max_children` (or one supervisor if a shared cap is wanted; that would change behaviour) |
| `ConnectionId` (process-global counter) | the connection's `ActorId` (never reused, M4) or the table's `MonitorRef` |
| `stopped()` sends `remove_peer(node_id, connection_id)` | the peer table monitors the connection when it registers it; `Handler<Down>` removes the entry whose monitor matches. `Down` arrives exactly once whatever ends the actor (X1), including kills and skipped `stopped()` |
| `handle_remove_peer` guard | the monitor match in `Handler<Down>` |
| timed-out registration undone by an unconditional `remove_peer` | the table's monitor fires for that registration too; a monitor installed after the connection died gets `Down { NoProc }` at once (E4) |
| connections never restarted | `DynamicChildSpec::temporary(..)`: no restarts, no budget use, args (the `TcpStream`, the `Node`) moved without `Clone` |
| first-wins on duplicates (`new_connected_peer` returns `false`) | unchanged: domain policy in the table (rank 12 may later own it) |
| `teardown()` mempool cleanup, `tx_broadcaster.remove_peer` | unchanged code, but see the note on `stopped()` below |

### After (sketch)

```rust
// Wiring (initializers.rs), after the peer table so connections stop first.
let inbound = system
    .dynamic_supervisor(
        DynamicSupervisorConfig { max_children: Some(MAX_INBOUND_CONNECTIONS), ..Default::default() },
        DynamicChildSpec::temporary("rlpx-inbound", &p2p_group, |_ctx, args: ConnArgs| async move {
            Ok::<_, Infallible>(PeerConnectionServer::new(args))
        })
        .shutdown_timeout(Duration::from_secs(2)),
    )?
    .start()
    .await?;
let outbound = system
    .dynamic_supervisor(DynamicSupervisorConfig::default(), /* same template */)?
    .start()
    .await?;

// Accept loop (network.rs): the semaphore and the permit plumbing go away.
match context.inbound.start_child(ConnArgs::receiver(context.clone(), peer_addr, stream)).await {
    Ok(_) => {}
    Err(StartChildError::MaxChildren) => debug!(peer = %peer_addr, "Inbound connection cap reached"),
    Err(e) => debug!(peer = %peer_addr, "Inbound connection not started: {e}"),
}

// Peer table (peer_table.rs): registration installs the monitor...
async fn handle_new_connected_peer(&mut self, msg: NewConnectedPeer, ctx: &Context<Self>) -> bool {
    let id = msg.node.node_id();
    if self.peers.contains_key(&id) { return false; }
    let monitor = ctx.monitor(&msg.connection.handle.child_handle());
    self.by_monitor.insert(monitor, id);
    self.peers.insert(id, PeerData::new(/* … */).with_monitor(monitor));
    true
}
// ...and removal is the Down, matched by monitor, not reported by the peer.
impl Handler<Down> for PeerTableServer {
    async fn handle(&mut self, down: Down, _ctx: &Context<Self>) {
        if let Some(id) = self.by_monitor.remove(&down.monitor_ref) {
            self.peers.swap_remove(&id);
            self.sessions.remove(&id);
        }
    }
}
```

`start_child` resolves when the connection's `started()` returns. ethrex's
`started()` runs the whole RLPx handshake today, under its own
`HANDSHAKE_TIMEOUT`, and calls `ctx.stop()` when it fails
([server.rs:537-620](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L537-L620)).
Under the dynamic supervisor that is a first-start failure: `start_child`
returns `FailedToStart`, the slot is freed, nothing is restarted or charged,
which is the behaviour ethrex wants. Two consequences: the accept loop would
wait for the handshake, and `startup_timeout` (default 5 s) must exceed
`HANDSHAKE_TIMEOUT` (10 s) or the supervisor cuts handshakes short. Either the
accept loop spawns the `start_child` future (it does not need the result),
or the handshake moves from `started()` into a first self-sent message, the
Spawned counterpart of OTP's `handle_continue`. The second is better:
`startup_timeout` then bounds only construction, and a slow peer never holds
a start; a failed handshake then becomes an ordinary Temporary exit.

### What disappears

About 45-60 of the ~135 hand-written lines (counted on `main`):

| Code | Lines | Fate |
|---|---|---|
| `ConnectionId` type and `next()` ([server.rs:176-188](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L176-L188)) | 13 | removed |
| `connection_id` in `Established`, `PeerData` and the handshake ([server.rs:432-437](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L432-L437), [handshake.rs:177](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/handshake.rs#L177), [M peer_table.rs:324-326](https://github.com/lambdaclass/ethrex/blob/fd5c57ddada097952660f6b749c739119e4de852/crates/networking/p2p/peer_table.rs#L324-L326)) | ~8 | removed (replaced by the monitor) |
| `remove_peer` protocol method and `handle_remove_peer` with its guard ([V peer_table.rs:693-711](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/peer_table.rs#L693-L711)) | ~22 | replaced by a ~10-line `Handler<Down>` |
| `remove_peer` send in `stopped()` | ~6 | removed |
| inbound semaphore, `try_acquire_owned`, `_admission_permit` field and parameter | ~15 | replaced by `max_children` |
| "connections are not admitted" comment and unmanaged `.start()` | | connections owned by the system |

What stays: first-wins registration (domain policy), `teardown()`'s mempool
cleanup, the tx-broadcaster index, `target_peers`. #7435's integration test
(171 lines) stays as a regression test, now exercising the monitor match.

### Bugs and gaps it closes

- **Stale entries when `stopped()` does not run** (panic in `started()`
  today; kills and supervisor shutdown under v0.8): the `Down` still comes.
- **The #7435 class** (an old connection's cleanup removing a new entry):
  impossible by construction, because removal is keyed by the monitor of the
  registered connection, not by node id.
- **Unowned connections at shutdown**: they are stopped, within the system
  deadline, before the peer table, instead of dying with the process.
- **Placement**: connections run on the `p2p` group instead of whichever
  runtime started them.

### Things to watch in a migration

- **`stopped()` is skipped on supervisor shutdown under v0.8** for a
  non-trapping actor. `teardown()` and `tx_broadcaster.remove_peer` live in
  `stopped()`; at node shutdown that is harmless (everything is going away),
  but on a `kill` during normal operation (a connection stuck past its
  `shutdown_timeout`) the broadcaster index would leak. Fix: the broadcaster
  also monitors connections, or the peer table forwards removal from its
  `Down` handler. Same reasoning as the exit-signals migration note.
- **Caps change meaning slightly**: the semaphore counted live actor states;
  `max_children` counts slots until owned cleanup ends (helpers included).
  For these actors (no helpers) that is the same.
- **Rejected arguments are dropped**: on `MaxChildren` the `TcpStream` inside
  `ConnArgs` is dropped, closing the socket without an RLPx Disconnect,
  exactly as today. Returning the arguments with the error is open question
  7 of the design.
- **Budget**: Temporary children never consume it, so the supervisor fails
  only on a stuck cleanup or a closed group. A connection whose cleanup
  hangs past twice its `shutdown_timeout` would fail the whole connection
  supervisor; set `shutdown_timeout` with that in mind.

## ethlambda

No dynamic per-X actors on either branch.

- `main`: two singletons, `BlockChainServer` (`start_with_backend(Backend::Thread)`,
  [crates/blockchain/src/lib.rs:434](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/blockchain/src/lib.rs#L434))
  and `P2PServer` (`start()`,
  [crates/net/p2p/src/lib.rs:1271](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/net/p2p/src/lib.rs#L1271)),
  plus one `PeerTableServer` and one `DiscoveryServer` from ethrex. Peers
  are libp2p connections handled inside the swarm, not actors. No maps of
  `ActorRef`.
- `spawned-v0.7`: the same singletons placed with `start_on(group)`
  ([lib.rs:1268](https://github.com/lambdaclass/ethlambda/blob/6a12bc753ae18c9d63f3dbf930edc700c4c4704c/crates/net/p2p/src/lib.rs#L1268),
  [blockchain lib.rs:449](https://github.com/lambdaclass/ethlambda/blob/6a12bc753ae18c9d63f3dbf930edc700c4c4704c/crates/blockchain/src/lib.rs#L449))
  and the HTTP server under a static supervisor
  ([main.rs:977-989](https://github.com/lambdaclass/ethlambda/blob/6a12bc753ae18c9d63f3dbf930edc700c4c4704c/bin/ethlambda/src/main.rs#L977-L989)).

Per-request and per-check concurrency is plain `tokio::spawn`/`spawn_blocking`
(gossip checks, aggregation), which belongs to ranks 11b and 15, not to a
dynamic supervisor.
