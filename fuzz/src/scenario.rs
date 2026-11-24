//! The operation language. Every field is a small integer or flag that the
//! driver maps onto the API (indices are taken modulo the live population),
//! so any byte string decodes to a runnable scenario.

use arbitrary::Arbitrary;

/// Most operations a scenario runs; the rest of a long input is ignored.
pub const MAX_OPS: usize = 64;

/// One fuzz input: operations plus everything that fixes the schedule.
/// Decoding reads the seed and strategy, then operations until the input
/// (or [`MAX_OPS`]) runs out, so every byte of an input is an operation.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Simulator seed (random walk and PCT draws; supervisor jitter seed).
    pub seed: u64,
    pub sched: Sched,
    pub ops: Vec<Step>,
}

/// One step: an operation of the base language, or a fault-injection step
/// (only in the [`Format::Faults`] decoding, so committed inputs of the
/// base format decode exactly as before).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Op(Op),
    Fault(FaultOp),
}

/// How the bytes of an input map to steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Base operations only: the format of every input committed before
    /// fault injection (`fuzz/seeds`, `fuzz/regressions`).
    Base,
    /// The fuzz target's format: an operation whose 4-byte discriminant is
    /// `7 (mod 8)` is a [`FaultOp`] instead; the rest decode as in `Base`.
    Faults,
}

impl Scenario {
    pub fn decode_with(
        mut u: arbitrary::Unstructured<'_>,
        format: Format,
    ) -> arbitrary::Result<Self> {
        let seed = u.arbitrary()?;
        let sched = u.arbitrary()?;
        let mut ops = Vec::new();
        while !u.is_empty() && ops.len() < MAX_OPS {
            let fault = format == Format::Faults
                && u.peek_bytes(4)
                    .is_some_and(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) % 8 == 7);
            if fault {
                u.bytes(4)?;
                ops.push(Step::Fault(u.arbitrary()?));
            } else {
                ops.push(Step::Op(u.arbitrary()?));
            }
        }
        Ok(Self { seed, sched, ops })
    }
}

impl<'a> Arbitrary<'a> for Scenario {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let seed = u.arbitrary()?;
        let sched = u.arbitrary()?;
        let ops: Vec<Op> = u.arbitrary()?;
        Ok(Self {
            seed,
            sched,
            ops: ops.into_iter().map(Step::Op).collect(),
        })
    }
    fn arbitrary_take_rest(u: arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Self::decode_with(u, Format::Base)
    }
    fn size_hint(depth: usize) -> (usize, Option<usize>) {
        let (seed, _) = <u64 as Arbitrary>::size_hint(depth);
        let (sched, _) = <Sched as Arbitrary>::size_hint(depth);
        (seed + sched, None)
    }
}

/// Scheduling strategy of the one simulated run.
#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sched {
    /// Seeded uniform random walk (`sim::run_once`).
    Random,
    /// One PCT run: `d = 1 + depth % 4`, `k = 1 + steps % 4000` polls.
    Pct { depth: u8, steps: u16 },
    /// The canonical schedule (DFS with zero delays: smallest ready id).
    Canonical,
}

#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Normal,
    Shutdown,
    Kill,
    Panic,
}

/// Lifecycle hook behaviour of a worker. Panics need several bits set so
/// that most generated actors live long enough to do something.
#[derive(Arbitrary, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hooks {
    /// bits 0-2 all set: panic in `started()`; bits 3-5: in `stopped()`;
    /// bits 6-7: in the state destructor.
    pub flags: u8,
    /// `>= 200`: `started()` sleeps `(v - 200) * 50` ms of virtual time.
    pub started_sleep: u8,
    /// `>= 200`: `stopped()` sleeps `(v - 200) * 50` ms.
    pub stopped_sleep: u8,
}

/// What a worker does with one `Do` message. Actor operands are indices into
/// the current actor population, resolved by the driver when it sends.
#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Noop,
    Panic,
    /// Sleep `20 * v` ms of virtual time inside the handler.
    Sleep(u8),
    StopSelf(Reason),
    /// Send a `Note` to another actor (checks X7 ordering at the receiver).
    Note(u8),
    Link(u8),
    Unlink(u8),
    Monitor(u8),
    Trap(bool),
    /// Owned registration of `fz-{v % 4}` (managed actors only).
    Register(u8),
    /// Stop another actor with a reason, from inside a handler.
    Stop(u8, Reason),
    /// `ctx.spawn_helper` (managed actors): `% 3` pends forever, sleeps
    /// `10 * v` ms, or panics. Its scope must cancel and join it.
    Helper(u8),
}

#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnHow {
    /// `.spawn()`: unmanaged async actor.
    Unmanaged,
    /// `.with_options(group).spawn()`: managed by an `ActorSystem` async group.
    Managed { group: u8 },
    /// `.start_link(parent)`, its wait abandoned (the 0.8 `start_linked`).
    Linked { parent: u8 },
}

#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildCrash {
    /// Send the current generation a panicking message.
    Panic,
    /// Stop the current generation's `ChildHandle` with a reason.
    Stop(Reason),
    /// Stop a nested supervisor's current instance (escalation).
    StopNested,
}

#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerDef {
    /// `% 3`: Permanent, Transient, Temporary.
    pub restart: u8,
    /// Index into every group of every system (a foreign group is rejected).
    pub group: u8,
    /// Bit `g % 8` set: the factory returns `Err` for generation `g`.
    pub fail_mask: u8,
    /// Bit `g % 8` set (and not failing): the factory panics.
    pub panic_mask: u8,
    /// Bit `g % 8` set: generation `g` gets `hooks`, otherwise plain hooks.
    pub hook_mask: u8,
    pub hooks: Hooks,
    /// `< 200`: default 5 s; else `10 + (v - 200) * 50` ms.
    pub startup_timeout: u8,
    /// `< 200`: default 5 s; else `10 + (v - 200) * 50` ms.
    pub shutdown_timeout: u8,
}

#[derive(Arbitrary, Debug, Clone, PartialEq, Eq)]
pub enum ChildDef {
    Worker(WorkerDef),
    /// A nested supervisor (`add_supervisor`) with worker children.
    Nested {
        restart: u8,
        group: u8,
        policy: Policy,
        children: Vec<WorkerDef>,
    },
}

#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// `% 3`: OneForOne, OneForAll, RestForOne.
    pub strategy: u8,
    /// `% 7`.
    pub max_restarts: u8,
    /// Restart window `1 + 2 * v` ms.
    pub window: u16,
    /// Backoff initial `v` ms, maximum `4 * v` ms.
    pub backoff: u8,
    pub jitter: bool,
    /// `stable_after` `10 * v + 1` ms.
    pub stable: u8,
}

#[derive(Arbitrary, Debug, Clone, PartialEq, Eq)]
pub struct SupSpec {
    pub system: u8,
    pub policy: Policy,
    /// At most four children are declared.
    pub children: Vec<ChildDef>,
}

#[derive(Arbitrary, Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Spawn {
        how: SpawnHow,
        hooks: Hooks,
    },
    Send {
        to: u8,
        cmd: Cmd,
        via_recipient: bool,
    },
    /// `timeout: None` uses the 5 s default. `detached` runs the request in
    /// its own task (another sender), collected at the next fence.
    Request {
        to: u8,
        cmd: Cmd,
        timeout: Option<u16>,
        detached: bool,
    },
    Link {
        a: u8,
        b: u8,
    },
    Unlink {
        a: u8,
        b: u8,
    },
    Monitor {
        watcher: u8,
        target: u8,
    },
    Demonitor {
        which: u8,
    },
    TrapExit {
        who: u8,
        on: bool,
    },
    Stop {
        who: u8,
        reason: Reason,
    },
    /// `send_after` (or `send_interval`) of `cmd` after `ms` ms.
    Timer {
        to: u8,
        ms: u16,
        cmd: Cmd,
        interval: bool,
    },
    CancelTimer {
        which: u8,
    },
    /// Sleep the driver `ms` ms of virtual time.
    Advance {
        ms: u16,
    },
    Yield {
        n: u8,
    },
    Supervisor(SupSpec),
    CrashChild {
        sup: u8,
        child: u8,
        how: ChildCrash,
    },
    /// `mode % 3`: 0 `stop()`, 1 `shutdown(ms)`, 2 quiesce then
    /// `shutdown(600 s)` with the reverse-order check.
    StopSupervisor {
        sup: u8,
        mode: u8,
        ms: u16,
    },
    NewSystem,
    /// `mode % 3`: 0 `shutdown(600 s)`, 1 `shutdown(ms)`, 2 quiesce then
    /// `shutdown(600 s)` with the reverse-admission-order check.
    ShutdownSystem {
        sys: u8,
        mode: u8,
        ms: u16,
    },
    DropSystem {
        sys: u8,
    },
    /// `send_request`, then drop the reply receiver (R4: not a cancellation).
    DropReply {
        to: u8,
        cmd: Cmd,
    },
    /// `wait_for_generation(current)` on a supervised worker, bounded by
    /// `ms` of virtual time (V4).
    WaitGeneration {
        sup: u8,
        child: u8,
        ms: u16,
    },
    /// Quiesce and check the eventual properties (Down/Exit delivery, link
    /// propagation, no spurious stop) now.
    Fence,
}

impl Hooks {
    pub fn started_panic(&self) -> bool {
        self.flags & 0b0000_0111 == 0b0000_0111
    }
    pub fn stopped_panic(&self) -> bool {
        self.flags & 0b0011_1000 == 0b0011_1000
    }
    pub fn drop_panic(&self) -> bool {
        self.flags & 0b1100_0000 == 0b1100_0000
    }
    pub fn started_sleep_ms(&self) -> u64 {
        sleep_ms(self.started_sleep)
    }
    pub fn stopped_sleep_ms(&self) -> u64 {
        sleep_ms(self.stopped_sleep)
    }
}

fn sleep_ms(v: u8) -> u64 {
    if v >= 200 {
        (v as u64 - 200) * 50
    } else {
        0
    }
}

/// `< 200`: the 5 s default; otherwise `10 + (v - 200) * 50` ms.
pub fn limit_ms(v: u8) -> u64 {
    if v >= 200 {
        10 + (v as u64 - 200) * 50
    } else {
        5_000
    }
}

/// User-code entry points where a fault step can arm an injected panic
/// (bit positions in [`FaultOp::Arm`]'s mask). Each is user code the
/// framework runs or destroys; docs/TESTING.md ("Panic injection") maps them
/// to the contract.
pub mod site {
    /// Drop of a message the framework destroys unhandled: queued at the
    /// death point, refused at admission, or held by a cancelled timer.
    pub const MSG_DROP: u16 = 1 << 0;
    /// `Clone` of a `send_interval` message (in the timer task).
    pub const MSG_CLONE: u16 = 1 << 1;
    /// Drop of a reply payload the caller never takes.
    pub const REPLY_DROP: u16 = 1 << 2;
    /// Drop of a job's result the framework discards (fenced, cancelled,
    /// replaced, past its deadline, or the owner exited).
    pub const RESULT_DROP: u16 = 1 << 3;
    /// Drop of a job's `done` callback (its captures) without running it.
    pub const DONE_DROP: u16 = 1 << 4;
    /// A panic in a job's `done` callback (the owner's turn).
    pub const DONE_PANIC: u16 = 1 << 5;
    /// A panic in a job's work (async or blocking).
    pub const WORK_PANIC: u16 = 1 << 6;
    /// A panic in the `Down` handler.
    pub const DOWN_PANIC: u16 = 1 << 7;
    /// A panic in `exit_received` (a trapped `Exit`).
    pub const EXIT_PANIC: u16 = 1 << 8;
    /// Injected panics carry a payload whose own destructor panics.
    pub const PAYLOAD_DROP: u16 = 1 << 9;
    /// `Debug` and `Display` of user payloads panic (the framework formats
    /// none today; armed for future entry points such as introspection).
    pub const FMT: u16 = 1 << 10;
    /// The `tracing` subscriber panics on every event outside the driver.
    pub const TRACING: u16 = 1 << 11;
    /// A panic in a nested supervisor's `configure` callback.
    pub const CONFIGURE_PANIC: u16 = 1 << 12;
    /// Drop of a job emit the framework discards (fenced, or its scheduled
    /// `emit_at` expired with the job).
    pub const EMIT_DROP: u16 = 1 << 13;
    /// A panic in a `Note` handler.
    pub const NOTE_PANIC: u16 = 1 << 14;
    /// A panic in a helper future's destructor (`ctx.spawn_helper`).
    pub const HELPER_DROP: u16 = 1 << 15;

    /// Site names by bit index.
    pub const NAMES: [&str; 16] = [
        "MSG_DROP",
        "MSG_CLONE",
        "REPLY_DROP",
        "RESULT_DROP",
        "DONE_DROP",
        "DONE_PANIC",
        "WORK_PANIC",
        "DOWN_PANIC",
        "EXIT_PANIC",
        "PAYLOAD_DROP",
        "FMT",
        "TRACING",
        "CONFIGURE_PANIC",
        "EMIT_DROP",
        "NOTE_PANIC",
        "HELPER_DROP",
    ];
}

/// A fault-injection step (decoded only in [`Format::Faults`]).
#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultOp {
    /// Arm `sites & mask` (each site about one time in four) for every
    /// payload, callback and actor created from now on; replaces the
    /// previous arming.
    Arm { sites: u16, mask: u16 },
    /// Send `cmd` to an actor as an `Fx` message, or request it in its own
    /// task (`request`; collected at the next fence once resolved, a
    /// deferred one at the latest after teardown), with no or any timeout.
    Send {
        to: u8,
        cmd: FaultCmd,
        request: bool,
        timeout: Option<u16>,
    },
    /// `send_after` / `send_interval` of an `Fx` message.
    Timer {
        to: u8,
        ms: u16,
        interval: bool,
        cmd: FaultCmd,
    },
}

/// What a worker does with one `Fx` message.
#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultCmd {
    /// Answer now.
    Reply,
    /// `defer()` the reply into the actor's state: answered by a later
    /// `Answer`, a job's `done` or `stopped()`, or dropped with the state.
    Defer,
    /// Answer up to `n % 4 + 1` deferred tokens, oldest first.
    Answer(u8),
    /// Drop the oldest deferred token (its caller gets `ReplyDropped`).
    DropToken,
    /// Start a job: `ctx.pipe` (or `pipe_blocking`). `sleep` is the async
    /// work's virtual time (`10 * v` ms); `emits % 4` messages are emitted
    /// first (`emit_at` 5 ms ahead when `at`); `deadline` is the job
    /// deadline (`5 * v` ms); `keyed` replaces the previous keyed job;
    /// `answer` hands the oldest deferred token to `done`.
    Pipe {
        blocking: bool,
        sleep: u8,
        emits: u8,
        at: bool,
        deadline: Option<u8>,
        keyed: bool,
        answer: bool,
    },
    /// Panic in the handler (with the armed payload).
    Panic,
    /// `ctx.spawn_helper` of a future that sleeps `10 * v` ms (managed
    /// actors only), armed for `HELPER_DROP`.
    Helper(u8),
}
