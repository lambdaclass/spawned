//! Stack-depth budgets. `future_layout.rs` bounds `size_of` of handles and
//! single futures; this suite bounds the *stack actually used* by a nested
//! request chain that mirrors a client RPC path (A awaits a protocol request to
//! B, which awaits C, which awaits D, with block-sized payloads), by supervisor
//! start/restart and by `ActorSystem::shutdown`.
//!
//! Each scenario runs in a child process with `RUST_MIN_STACK` set to the
//! budget, so every thread the framework, tokio and libtest create gets that
//! stack, and the client future is driven from a `std::thread` built with the
//! same explicit `stack_size`. Overflow aborts the child and fails the parent.
//!
//! Budgets are ~1.5x the minimum passing stack. Re-measure with
//! `cargo test -p spawned-concurrency --test stack_budget -- --ignored probe --nocapture`
//! (add `--release` for the release column) and update `BUDGETS`.
//!
//! Minimum passing stack in KiB, depth 4, 1 KiB inline payload. The async
//! chain does not grow with depth (each hop is its own task; measured equal at
//! depths 1, 16 and 64), so the budget bounds the deepest single poll: a group
//! worker (`spawned-stack`) or the client thread overflows first. Release
//! minimums sit at the platform floor (aarch64-apple-darwin, Rust 1.98):
//!
//! scenario,debug_min_kib,release_min_kib,debug_budget_kib,release_budget_kib
//! chain_async_group,68,8,104,16
//! chain_thread_group,68,8,104,16
//! chain_ambient,68,8,104,16
//! supervisor_restart_shutdown,52,20,80,32
//! chain_threads,20,8,32,16
//!
//! `--features metrics` measures the same debug minimums.
#[path = "support/drop_subprocess.rs"]
mod subprocess;

use spawned_concurrency::{protocol, Response};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(20);
const ROUNDS: usize = 16;

/// A block-shaped payload: 1 KiB inline (header and hash list) plus a heap body.
#[derive(Clone)]
pub struct Block {
    header: [u8; 512],
    hashes: [[u8; 32]; 16],
    body: Vec<u8>,
}
impl Block {
    fn new(seed: u8) -> Self {
        Self {
            header: [seed; 512],
            hashes: [[seed; 32]; 16],
            body: vec![seed; 64 * 1024],
        }
    }
}

#[protocol]
pub trait Relay: Send + Sync {
    #[request_timeout(LIMIT)]
    fn forward(&self, block: Block) -> Response<Block>;
}

/// Every hop stamps the block, so the client can check the full chain ran.
fn stamp(block: &mut Block, hop: usize) {
    block.header[hop] = block.header[hop].wrapping_add(1);
    block.hashes[hop % 16][0] ^= 0xff;
}
fn check(block: &Block, seed: u8, depth: usize) {
    for hop in 0..depth {
        assert_eq!(block.header[hop], seed.wrapping_add(1), "hop {hop} skipped");
    }
    assert_eq!(block.body.len(), 64 * 1024);
}

fn depth() -> usize {
    std::env::var("SPAWNED_STACK_DEPTH")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(4)
        .clamp(1, 512)
}

/// Run `work` on a thread with exactly `RUST_MIN_STACK` bytes of stack.
fn on_small_stack(work: impl FnOnce() + Send + 'static) {
    let size: usize = std::env::var("RUST_MIN_STACK")
        .expect("child runs with RUST_MIN_STACK")
        .parse()
        .unwrap();
    std::thread::Builder::new()
        .name("small-stack-client".into())
        .stack_size(size)
        .spawn(work)
        .unwrap()
        .join()
        .expect("small-stack scenario failed");
}

#[cfg(feature = "async")]
mod tasks_chain {
    use super::*;
    use spawned_concurrency::{
        message::Message,
        supervision::{Backoff, ChildSpec, SupervisorConfig},
        system::{ActorSystem, ExecutionGroup},
        tasks::{Actor, ActorRef, ActorStart, Context, Handler},
    };

    pub struct Hop {
        index: usize,
        next: Option<ActorRef<Hop>>,
    }
    impl Actor for Hop {}
    impl Handler<relay::Forward> for Hop {
        async fn handle(&mut self, msg: relay::Forward, _: &Context<Self>) -> Block {
            let mut block = msg.block;
            stamp(&mut block, self.index);
            match &self.next {
                Some(next) => next.forward(block).await.expect("downstream hop"),
                None => block,
            }
        }
    }

    /// Build the chain tail-first so each hop owns a reference to the next.
    fn chain(start: impl Fn(Hop) -> ActorRef<Hop>) -> Vec<ActorRef<Hop>> {
        let mut hops: Vec<ActorRef<Hop>> = Vec::new();
        for index in (0..depth()).rev() {
            let next = hops.last().cloned();
            hops.push(start(Hop { index, next }));
        }
        hops.reverse();
        hops
    }

    async fn drive(head: ActorRef<Hop>) {
        for round in 0..ROUNDS {
            let seed = round as u8;
            let block = head.forward(Block::new(seed)).await.expect("chain reply");
            check(&block, seed, depth());
        }
    }

    fn small_runtime() -> tokio::runtime::Runtime {
        let size = std::env::var("RUST_MIN_STACK").unwrap().parse().unwrap();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_stack_size(size)
            .enable_all()
            .build()
            .unwrap()
    }

    pub fn chain_in_group(thread_group: bool) {
        on_small_stack(move || {
            let rt = small_runtime();
            let system = ActorSystem::new().unwrap();
            let group: ExecutionGroup = if thread_group {
                system.thread_group("stack", depth() + 2)
            } else {
                system.async_group("stack", 2)
            }
            .unwrap();
            let hops = chain(|hop| group.start(hop).unwrap());
            rt.block_on(async {
                tokio::time::timeout(LIMIT, drive(hops[0].clone()))
                    .await
                    .expect("chain hung");
                drop(hops);
                let report = tokio::time::timeout(LIMIT, system.shutdown(LIMIT))
                    .await
                    .expect("shutdown hung");
                assert!(report.complete, "system shutdown incomplete");
            });
            rt.shutdown_timeout(LIMIT);
        });
    }

    pub fn chain_ambient() {
        on_small_stack(|| {
            let rt = small_runtime();
            let hops = {
                let _entered = rt.enter();
                chain(|hop| hop.start())
            };
            rt.block_on(async {
                tokio::time::timeout(LIMIT, drive(hops[0].clone()))
                    .await
                    .expect("chain hung");
                for hop in &hops {
                    hop.context().stop();
                }
                for hop in hops {
                    tokio::time::timeout(LIMIT, hop.join())
                        .await
                        .expect("stop hung");
                }
            });
            rt.shutdown_timeout(LIMIT);
        });
    }

    /// A supervised child whose first generation panics while serving a block.
    pub struct Flaky {
        generation: u64,
    }
    pub struct Crash(#[allow(dead_code)] Block);
    impl Message for Crash {
        type Result = ();
    }
    impl Actor for Flaky {}
    impl Handler<Crash> for Flaky {
        async fn handle(&mut self, _: Crash, _: &Context<Self>) {
            if self.generation == 1 {
                panic!("injected stack-budget crash");
            }
        }
    }
    impl Handler<relay::Forward> for Flaky {
        async fn handle(&mut self, msg: relay::Forward, _: &Context<Self>) -> Block {
            msg.block
        }
    }

    pub fn supervisor_restart_shutdown() {
        on_small_stack(|| {
            let rt = small_runtime();
            let system = ActorSystem::new().unwrap();
            for group in [
                system.async_group("stack-async", 2).unwrap(),
                system.thread_group("stack-thread", 4).unwrap(),
            ] {
                let config = SupervisorConfig {
                    max_restarts: 10,
                    backoff: Backoff {
                        initial: Duration::ZERO,
                        maximum: Duration::ZERO,
                        jitter: false,
                        seed: Some(1),
                        ..Backoff::default()
                    },
                    ..SupervisorConfig::default()
                };
                let mut tree = system.supervisor(config).unwrap();
                let child = tree
                    .add(ChildSpec::new("flaky", &group, |ctx| {
                        let generation = ctx.generation();
                        async move { Ok::<_, String>(Flaky { generation }) }
                    }))
                    .unwrap();
                rt.block_on(async {
                    use tokio::time::timeout as bounded;
                    let supervisor = bounded(LIMIT, tree.start())
                        .await
                        .expect("start hung")
                        .unwrap();
                    let first = child.current().unwrap();
                    let _ = first.send(Crash(Block::new(1)));
                    let next = bounded(LIMIT, child.wait_for_generation(1))
                        .await
                        .expect("restart hung")
                        .unwrap();
                    let block = next.forward(Block::new(2)).await.unwrap();
                    assert_eq!(block.header[0], 2);
                    let report = bounded(LIMIT, supervisor.shutdown(LIMIT))
                        .await
                        .expect("supervisor shutdown hung");
                    assert!(report.complete);
                    assert_eq!(report.failure, None);
                });
            }
            rt.block_on(async {
                let report = tokio::time::timeout(LIMIT, system.shutdown(LIMIT))
                    .await
                    .expect("system shutdown hung");
                assert!(report.complete, "system shutdown incomplete");
            });
            rt.shutdown_timeout(LIMIT);
        });
    }
}

mod threads_chain {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorRef, ActorStart, Context, Handler};

    pub struct Hop {
        index: usize,
        next: Option<ActorRef<Hop>>,
    }
    impl Actor for Hop {}
    impl Handler<relay::Forward> for Hop {
        fn handle(&mut self, msg: relay::Forward, _: &Context<Self>) -> Block {
            let mut block = msg.block;
            stamp(&mut block, self.index);
            match &self.next {
                Some(next) => next.forward(block).unwrap(),
                None => block,
            }
        }
    }

    pub fn chain_threads() {
        on_small_stack(|| {
            let mut hops: Vec<ActorRef<Hop>> = Vec::new();
            for index in (0..depth()).rev() {
                let next = hops.last().cloned();
                hops.push(Hop { index, next }.start());
            }
            hops.reverse();
            for round in 0..ROUNDS {
                let seed = round as u8;
                let block = hops[0].forward(Block::new(seed)).unwrap();
                check(&block, seed, depth());
            }
            for hop in &hops {
                hop.context().stop();
            }
            for hop in hops {
                hop.join();
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Scenarios, budgets and the subprocess driver
// ---------------------------------------------------------------------------

type Scenario = (&'static str, fn());
#[cfg(feature = "async")]
fn task_scenarios() -> Vec<Scenario> {
    vec![
        ("chain_async_group", || tasks_chain::chain_in_group(false)),
        ("chain_thread_group", || tasks_chain::chain_in_group(true)),
        ("chain_ambient", tasks_chain::chain_ambient),
        (
            "supervisor_restart_shutdown",
            tasks_chain::supervisor_restart_shutdown,
        ),
    ]
}
#[cfg(not(feature = "async"))]
fn task_scenarios() -> Vec<Scenario> {
    Vec::new()
}
fn scenarios() -> Vec<Scenario> {
    let mut all = task_scenarios();
    all.push(("chain_threads", threads_chain::chain_threads));
    all
}

/// Budget in KiB, ~1.5x the measured minimum (see the table above).
fn budget_kib(scenario: &str) -> usize {
    let (debug, release) = match scenario {
        "chain_async_group" | "chain_thread_group" | "chain_ambient" => (104, 16),
        "supervisor_restart_shutdown" => (80, 32),
        "chain_threads" => (32, 16),
        other => panic!("no budget for {other}"),
    };
    if cfg!(debug_assertions) {
        debug
    } else {
        release
    }
}

const MARKER: &str = "stack scenario complete";

fn run_child(scenario: &str, kib: usize) -> Result<(), String> {
    let env = [
        ("SPAWNED_STACK_SCENARIO", scenario.to_string()),
        ("RUST_MIN_STACK", (kib * 1024).to_string()),
    ];
    match subprocess::spawn("stack_child", &env, Duration::from_secs(120)) {
        None => Err(format!("{scenario} at {kib} KiB timed out")),
        Some(o) if o.status.success() && o.output.contains(MARKER) => Ok(()),
        Some(o) => Err(format!(
            "{scenario} at {kib} KiB failed ({}): {}\n{}",
            o.status, o.output, o.errors
        )),
    }
}

#[test]
#[ignore = "subprocess helper exercised by enabled parent"]
fn stack_child() {
    let Ok(name) = std::env::var("SPAWNED_STACK_SCENARIO") else {
        return;
    };
    let (_, run) = scenarios()
        .into_iter()
        .find(|(n, _)| *n == name)
        .expect("known scenario");
    run();
    println!("{MARKER}: {name}");
}

#[test]
fn scenarios_fit_their_stack_budgets() {
    for (name, _) in scenarios() {
        if let Err(error) = run_child(name, budget_kib(name)) {
            panic!("stack budget regression: {error}");
        }
    }
}

/// Bisect the minimum passing stack per scenario; prints the CSV rows above.
#[test]
#[ignore = "measurement tool: --ignored probe --nocapture"]
fn probe_minimum_stacks() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_some() {
        return;
    }
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!("scenario,profile,min_kib");
    for (name, _) in scenarios() {
        // Invariant: `high` passes, `low` fails (or is below any usable stack).
        let (mut low, mut high) = (4usize, 2048usize);
        run_child(name, high).unwrap();
        while high - low > 4 {
            let mid = (low + high) / 2 / 4 * 4;
            match run_child(name, mid) {
                Ok(()) => high = mid,
                Err(_) => low = mid,
            }
        }
        println!("{name},{profile},{high}");
    }
}
