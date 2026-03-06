# PR #153 Review Findings

## Bugs (fix before merge)

- [x] **B1** `todo!()` panic in `process.rs` — replaced with graceful loop termination via `Option<T>` return
- [x] **B2** CI badge URL — fixed `ci.yaml` → `ci.yml`
- [x] **B3** Release workflow — added `sleep 30` between publish steps for crates.io index propagation
- [x] **B4** `init_tracing()` — replaced `.unwrap()` with `let _ =` to silently ignore double-init
- [x] **B5** Actor loop — added `future::select(recv, cancelled)` so `ctx.stop()` takes effect immediately
- [x] **B6** Lifecycle panics — wrapped `started()`/`stopped()` in `catch_unwind`, `completion_tx` always fires

## Documentation Inaccuracies (fixed)

- [x] **D1** README quick example — added missing imports (`ActorStart as _`, `rt`, message structs, protocol trait)
- [x] **D2** `Result<(), ActorError>` — documented as send (fire-and-forget) special case in README and migration guide
- [x] **D3** README examples table — added `blocking_genserver` and `busy_genserver_warning`, fixed count to 14

## Example Fixes (fixed)

- [x] **E3** `updater/src/main.rs` — replaced `thread::sleep` with `rt::sleep(...).await`

## Macro Issues (fixed)

- [x] **M1** `to_snake_case` — fixed acronym handling (`HTTPServer` → `http_server`)
- [x] **M2** Unrecognized return types — now emits compile error instead of silent `Send` classification
- [x] **M3** `qualify_type_with_super` — now skips `std::`, `core::`, `alloc::` paths
- [x] **M4** Generic protocol traits — now rejected with clear compile error
- [x] **M5** `async fn` in protocol traits — now rejected with clear compile error
- [x] **M6** Mixed `Response<T>` + `Result<T, ActorError>` — now rejected with clear compile error
- [ ] **M7** Generated message structs derive `Clone` but not `Debug` — can't add universally because `Arc<dyn Protocol>` fields don't impl `Debug`

## Example Inconsistencies (fixed)

- [x] **E1** `blocking_genserver` and `busy_genserver_warning` — migrated to `#[protocol]`/`#[actor]` macros
- [x] **E2** `chat_room_threads` — renamed `speak()` to `say()` to match `chat_room`
- [x] **E4** `updater_threads` — switched from manual `send_after` rescheduling to `send_interval` (matches `updater`)
- [x] **E5** Cargo.toml — normalized all to `edition = "2021"`, `version = "0.1.0"`, added missing `[[bin]]` sections
- [x] **E6** `_check` — kept leading underscore (method exists only to define the `Check` message struct, never called directly)
- [x] **E7** Removed unused `futures` from both updaters, unused `tokio` from `signal_test`
- [x] **E8** `println!` → `tracing::info!` in `blocking_genserver`

## Second-pass fixes

- [x] **P1** Send-only methods with `ReturnType::Default` — emit `let _ = self.send(...)` to avoid type mismatch
- [x] **P2** `#[cfg(...)]` on protocol methods — now propagated to generated structs, Message impls, and blanket impls
- [x] **P3** Non-ident parameter patterns — now rejected with clear compile error
- [x] **P4** `qualify_type_with_super` — added Array, Slice, Paren type handling
- [x] **P5** Duplicate `#[started]`/`#[stopped]` — now produces compile error instead of silently overwriting
- [x] **P6** Threads `run_actor` — wrapped `started()`/`stopped()` in `catch_unwind` (parity with tasks)
- [x] **P7** Threads actor loop — `recv_timeout(100ms)` + cancellation check so `ctx.stop()` takes effect promptly
- [x] **P8** README + macro docs — `Result<(), ActorError>` correctly labeled as both-modes send

## Design/Architecture (post-merge tickets)

- [ ] **A1** Unbounded mailbox channels (both tasks and threads) — no backpressure, can OOM under fast producers
- [ ] **A2** Massive code duplication between `tasks/actor.rs` and `threads/actor.rs` — `Context`, `ActorRef`, `Receiver`, `Envelope` are ~90% identical
- [ ] **A3** ~~`threads` module imports tokio `Runtime` for `block_on`~~ — intentional: `block_on` is an escape hatch for calling async libraries (e.g., `reqwest`) from sync actor handlers
- [x] **A4** ~~`oneshot` module aliases `crossbeam::unbounded` (MPMC) as oneshot~~ — replaced with `std::sync::mpsc`
- [x] **A5** ~~`crossbeam` pinned to 0.7.3 (2019)~~ — removed, replaced by `std::sync::mpsc` (uses crossbeam internally since Rust 1.67)
- [ ] **A6** `ActorError` can't distinguish graceful stop from handler panic
- [ ] **A7** `stopped()` runs on potentially corrupted state after handler panic (wrapped in `AssertUnwindSafe`)
- [ ] **A8** CI `build` job has no cache and provides no artifacts to downstream jobs — adds latency for no benefit
- [ ] **A9** Missing test coverage: no tests for `join()` (threads), panic recovery, `send_message_on`, `Response<T>` future, `Recipient` (threads), stream listener
