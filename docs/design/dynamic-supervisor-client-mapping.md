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

Revision 2 of the design changes three things for ethrex: the supervisor
serialises starts like OTP (so the RLPx handshake must leave `started()`),
rejections return the arguments, and a connection whose cleanup gets stuck
fails only itself.

| ethrex today | With the dynamic supervisor |
|---|---|
| `.start()`, unmanaged, on the caller's runtime | `start_child(args)` on a `DynamicSupervisor<PeerConnectionServer, ConnArgs>`, placed on the `p2p` group |
| handshake inside `started()` (up to `HANDSHAKE_TIMEOUT`, 10 s) | **required change:** the handshake moves to the post-start continuation of the unified-start design (0.9, OTP's `handle_continue`): it runs right after `started()` returns and before any message, still under `HANDSHAKE_TIMEOUT`, and a failure ends the actor (a Temporary exit, not a start failure). Otherwise one silent peer would hold the supervisor's control loop, and with it every accept and dial, for the whole `startup_timeout`. A self-sent first message is not equivalent: a message enqueued by another actor first would run before it. The continuation precedes messages but cannot prevent termination: `terminate_child` or shutdown can end the connection before or during the handshake, which then ends it without registering in the peer table (nothing to clean up there) |
| no owner; shutdown does not stop connections | the supervisors are owned by the system, nested under the p2p supervisor (recommended) or as roots admitted after the peer table; connections stop before the table, all at once, each killed after its `shutdown_timeout` |
| `Semaphore(150)` plus `_admission_permit` in actor state | `max_children = Some(150)` on an inbound supervisor; `Rejected { MaxChildren, args }` returns the `ConnArgs`, so the accept loop can still send an RLPx Disconnect or just drop the stream, as today |
| outbound uncapped | a second supervisor for dials without `max_children` |
| `ConnectionId` (process-global counter) | the table's `MonitorRef` (or the connection's `ActorId`, never reused, M4) |
| `stopped()` sends `remove_peer(node_id, connection_id)` | the peer table monitors the connection when it registers it; `Handler<Down>` removes the entry only if it still holds that monitor. `Down` arrives exactly once whatever ends the actor (X1), including kills and a skipped `stopped()` |
| timed-out registration undone by an unconditional `remove_peer` | the table's monitor fires for that registration too; a monitor installed after the connection died gets `Down { NoProc }` at once (E4) |
| connections never restarted | `DynamicChildSpec::temporary(..)`, chosen deliberately over the Permanent default: a dropped peer must not be redialled by the supervisor (the TCP stream is gone and discovery decides whom to dial), so the connection is one-shot. No restarts, no budget use, arguments (the `TcpStream`, the `Node`) moved without `Clone` |
| first-wins on duplicates | unchanged: domain policy in the table (rank 12 may later own it) |
| `teardown()` and `tx_broadcaster.remove_peer` in `stopped()` | moved to owners that act on the `Down` (below): `stopped()` is skipped for a non-trapping connection on `terminate_child`, on shutdown and after a kill |
| raw `tokio::spawn` outbound writer holding the socket sink ([server.rs:1530-1557](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L1530-L1557)) | **required change:** an owned helper (`ctx.spawn_helper`), cancelled at exit and joined within the cleanup deadline, with an explicit policy for frames still queued (drop them; the peer is being disconnected). Until then the writer can outlive the actor and keep the socket open after the supervisor freed the slot, so `max_children` would bound slots, not sockets |

### After (sketch)

```rust
// Wiring (initializers.rs): nested under the p2p supervisor, so a failure
// of the connection supervisor is restarted instead of disabling accepts.
let inbound = p2p_tree.add_dynamic_supervisor(DynamicSupervisorSpec::new(
    "rlpx-inbound",
    DynamicSupervisorConfig { max_children: Some(MAX_INBOUND_CONNECTIONS), ..Default::default() },
    DynamicChildSpec::temporary("rlpx-inbound", &p2p_group, |_ctx, args: ConnArgs| async move {
        Ok::<_, Infallible>(PeerConnectionServer::new(args))
    })
    .shutdown_timeout(Duration::from_secs(2)),
))?;
// `outbound` likewise, without max_children.

// Accept loop (network.rs): the semaphore and the permit plumbing go away.
// start_child resolves as soon as started() returns, which is now immediate.
match inbound.current()?.start_child(ConnArgs::receiver(context.clone(), peer_addr, stream)).await {
    Ok(_) => {}
    Err(StartChildError::Rejected { reason: RejectReason::MaxChildren, args }) => {
        debug!(peer = %peer_addr, "Inbound connection cap reached");
        drop(args); // closes the stream, as today
    }
    Err(e) => debug!(peer = %peer_addr, "Inbound connection not started: {}", e.without_args()),
}

// Connection actor (server.rs): the handshake leaves started().
// Handshake as the post-start continuation (unified start #259, 0.9):
// runs after the readiness commit and before any message.
async fn started(&mut self, _ctx: &Context<Self>) -> Init<Self> {
    Init::continue_with(Handshake)
}
impl Handler<Handshake> for PeerConnectionServer {
    async fn handle(&mut self, _: Handshake, ctx: &Context<Self>) { /* existing handshake code */ }
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
// ...and removal is the Down, guarded by the entry's own monitor (agy 8):
impl Handler<Down> for PeerTableServer {
    async fn handle(&mut self, down: Down, _ctx: &Context<Self>) {
        let Some(id) = self.by_monitor.remove(&down.monitor_ref) else { return };
        if self.peers.get(&id).is_some_and(|p| p.monitor == down.monitor_ref) {
            self.peers.swap_remove(&id);
            self.sessions.remove(&id);
            self.on_connection_gone(id, down.monitor_ref); // see "Skipped teardown"
        }
    }
}
```

### Skipped teardown: who releases what

`stopped()` does not run for a non-trapping connection that is terminated,
shut down or killed (v0.8, decision 2), and `trap_exit` does not cover a
kill. So each resource the hook releases today needs an owner whose
lifetime covers it. Two kinds:

- **Records an actor keeps in its own state** (peer-table entries, the
  broadcaster's index) live and die with that actor. Their owner monitors
  the connection and releases on `Down`, guarded by the identity it stored.
  If the owner itself stops or restarts first, the records go with it, so
  nothing leaks.
- **Persistent reservations outside any actor** (the mempool's in-flight
  hashes, cell availability, the announced-hash budget) would outlive a
  monitor whose owner stops before handling `Down` (Codex round 3: the peer
  table can begin a non-trapping shutdown with the `Down` still queued).
  They are held by an **owning registration**: a value the mempool hands
  out when the connection reserves, kept in the connection's state, whose
  destruction releases exactly that connection's entries synchronously,
  under the mempool's lock, with no message and no `.await`. State
  destruction runs on every exit path, kills and runtime drops included
  (exit phase 5), before the connection's slot is freed, so the release
  cannot be lost whoever else is alive.

Nothing is sent from `Drop`: a message would reach the peer table before the
`Down` (phase 5 before 6) and could pick the dying peer as an alternate
(agy round 2), and an enqueued message is not a completed release.

| Released in `stopped()` today | Owner and registration | Release |
|---|---|---|
| peer-table entry and discv5 session | peer table; `new_connected_peer` stores the monitor reference in `PeerData` | on `Down`, only if the entry still holds that monitor; gone anyway if the peer table stops |
| tx-broadcaster index | broadcaster; its `add_peer` records the connection's `ActorId` and monitors it | on `Down`, only if the index is still owned by that `ActorId`; gone anyway if the broadcaster stops |
| in-flight transaction reservations (`clear_in_flight_txs`) | mempool registration `InFlightLease`, held in the connection's state; the mempool records each in-flight hash against the lease's `ActorId` | the lease's destruction removes the hashes still recorded against it, synchronously |
| cell availability and announced-hash budget (`clear_peer_cell_availability`, `forget_peer_announcements`) | the same lease, keyed by (node id, connection `ActorId`) instead of node id plus a "started session" check | the lease's destruction, for that `ActorId` only; a second session of the same node keeps its own |
| re-request to alternates (`retry_on_alternates`) | peer table, best effort | on `Down`, after removing the entry, with a local lookup of live peers (no request to itself). Suppressed by explicit shutdown state, the node's shutdown token that `shutdown_l1` cancels first, not by `Down.reason`: a connection killed at its shutdown deadline ends `Killed`. Lost if the peer table stops first, which is acceptable for a retry |
| disconnect metrics | peer table's `Down` handler (it has the final reason) | best effort |

### What disappears

About 45-60 of the ~135 hand-written lines (counted on `main`):

| Code | Lines | Fate |
|---|---|---|
| `ConnectionId` type and `next()` ([server.rs:176-188](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L176-L188)) | 13 | removed |
| `connection_id` in `Established`, `PeerData` and the handshake ([server.rs:432-437](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L432-L437), [handshake.rs:177](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/handshake.rs#L177), [M peer_table.rs:324-326](https://github.com/lambdaclass/ethrex/blob/fd5c57ddada097952660f6b749c739119e4de852/crates/networking/p2p/peer_table.rs#L324-L326)) | ~8 | removed (replaced by the monitor) |
| `remove_peer` protocol method and `handle_remove_peer` with its guard ([V peer_table.rs:693-711](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/peer_table.rs#L693-L711)) | ~22 | replaced by a ~12-line `Handler<Down>` |
| `remove_peer` send in `stopped()` | ~6 | removed |
| inbound semaphore, `try_acquire_owned`, `_admission_permit` field and parameter | ~15 | replaced by `max_children` |
| "connections are not admitted" comment and unmanaged `.start()` | | connections owned by the system |

Added: the handshake as a post-start continuation (moving existing code),
the writer as an owned helper, the mempool's `InFlightLease` (an owning
registration released by its destruction), the broadcaster's identity check
and the release logic in the `Down` handlers. Net lines are
roughly even; what changes is that every path out of a connection is
covered. #7435's integration test (171 lines) stays as a regression test,
now exercising the monitor guard.

### Bugs and gaps it closes

- **Stale entries when `stopped()` does not run** (panic in `started()`
  today; kills, `terminate_child` and shutdown under v0.8): the `Down` still
  comes.
- **The #7435 class**: removal is keyed by the registered connection's
  monitor and re-checked against the entry, not by node id.
- **Unowned connections at shutdown**: stopped within the system deadline,
  before the peer table, instead of dying with the process.
- **Unowned writer tasks**: joined before the slot frees.
- **Placement**: connections run on the `p2p` group.

### Things to watch in a migration

- **Serialised starts.** Anything slow left in `started()` holds every
  accept and dial. Keep `started()` to state setup; the handshake is the continuation.
- **Stuck cleanup.** A connection's own exit gets one `shutdown_timeout`
  for its owned cleanup (E3 row 4, no kill); a terminated or shut-down one
  gets the signal, the kill after `shutdown_timeout` and a second interval.
  A connection that misses them is stuck: its slot stays counted until its
  code returns, it is reported, and **nothing else is affected**, since
  connections are Temporary (design: stuck children). Only budget
  exhaustion (impossible for Temporary children) or a closed group fails
  the supervisor; nesting it under the p2p supervisor still restarts it in
  that case instead of leaving accepts disabled (agy P0).
- **Caps count slots until owned cleanup ends**, writer helper included,
  so a slow socket close keeps its slot a little longer than the permit did.
- **Rejected arguments come back**, so the accept loop decides what to do
  with the stream.

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
