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
    pub ops: Vec<Op>,
}

impl<'a> Arbitrary<'a> for Scenario {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self {
            seed: u.arbitrary()?,
            sched: u.arbitrary()?,
            ops: u.arbitrary()?,
        })
    }
    fn arbitrary_take_rest(mut u: arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let seed = u.arbitrary()?;
        let sched = u.arbitrary()?;
        let mut ops = Vec::new();
        while !u.is_empty() && ops.len() < MAX_OPS {
            ops.push(u.arbitrary()?);
        }
        Ok(Self { seed, sched, ops })
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
}

#[derive(Arbitrary, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnHow {
    /// `.start()`: unmanaged async actor.
    Unmanaged,
    /// `.start_on(group)`: managed by an `ActorSystem` async group.
    Managed { group: u8 },
    /// `.start_linked(parent)`.
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
