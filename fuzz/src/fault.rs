//! Panic injection at the user-code entry points (`scenario::site`).
//!
//! A [`Bomb`] is a user payload (a message, a reply, a job result, a job's
//! `done` captures, an emit, a helper future's locals) whose `Drop` or
//! `Clone` panics when its site was armed when it was created. Code that
//! takes a payload in a turn defuses it, so an armed bomb only goes off where
//! the framework destroys or copies it on its own: at the death point, at a
//! refused admission, in a timer task, on the deadline service, at a fenced
//! dispatch. Every bomb is accounted for (dropped exactly once, M6), and a
//! bomb that goes off on its owner's task must surface as that actor's
//! cleanup failure without changing its exit reason (L6).
//!
//! Nothing here goes off in the driver or its request tasks (the caller's
//! own context: a payload dropped there is the caller's to contain).

use crate::scenario::{site, FaultCmd};
use crate::world::{Worker, World};
use spawned_concurrency::{
    message::Message,
    tasks::{Context, Handler, JobCtx, JobOptions, JobOutcome, ReplyTo},
    DeferError, PendingReply,
};
use spawned_rt::tasks::{self as rt, sim};
use std::{sync::Arc, time::Duration};

/// One bomb's accounting.
#[derive(Debug, Clone)]
pub struct BombRec {
    pub site: u16,
    /// Instance whose payload it is.
    pub owner: Option<usize>,
    pub dropped: u32,
    /// Armed when created.
    pub armed: bool,
    /// It went off, on this instance's task (`None`: another task).
    pub fired: Option<Option<usize>>,
    /// It went off inside its owner's own `reply()` call: a panic of the
    /// actor's own, not a cleanup failure.
    pub own_panic: bool,
}

pub struct Bomb {
    world: Arc<World>,
    id: usize,
    site: u16,
    owner: Option<usize>,
    /// Armed when created (`World::armed`).
    drop_armed: bool,
    clone_armed: bool,
    fmt_armed: bool,
    defused: bool,
}

impl Bomb {
    /// A payload for `site`, armed if the site (and, for messages, also
    /// `MSG_CLONE`) is armed now.
    pub fn new(world: &Arc<World>, site: u16, owner: Option<usize>) -> Self {
        let armed = world.armed();
        let id = {
            let mut s = world.lock();
            s.bombs.push(BombRec {
                site,
                owner,
                dropped: 0,
                armed: armed & site != 0,
                fired: None,
                own_panic: false,
            });
            s.bombs.len() - 1
        };
        Self {
            world: world.clone(),
            id,
            site,
            owner,
            drop_armed: armed & site != 0,
            clone_armed: site == site::MSG_DROP && armed & site::MSG_CLONE != 0,
            fmt_armed: armed & site::FMT != 0,
            defused: false,
        }
    }
    /// The payload reached user code that takes it: never goes off.
    pub fn defuse(&mut self) {
        self.defused = true;
    }
    fn live(&self) -> bool {
        !self.defused && !std::thread::panicking() && !self.world.in_caller()
    }
    /// The destructor goes off here: record it on this payload and attribute
    /// it to the instance whose task runs now.
    fn fire(&self, what: &'static str) -> ! {
        let task_owner = self.world.task_owner();
        {
            let mut s = self.world.lock();
            s.bombs[self.id].fired = Some(task_owner);
            if let Some(i) = task_owner.filter(|i| Some(*i) == self.owner) {
                if s.insts[i].replying {
                    // Inside the actor's own `reply()` (a caller that left):
                    // a panic of its own, in a turn or in `stopped()`.
                    s.insts[i].relaxed = true;
                    s.bombs[self.id].own_panic = true;
                    if let Some(id) = s.insts[i].id {
                        s.stoppable(id);
                    }
                } else {
                    // Anywhere else on its task the payload dies past D: a
                    // cleanup failure (L6), the reason unchanged.
                    s.insts[i].drop_panicked = true;
                }
            }
        }
        panic!("{what}");
    }
}

impl Drop for Bomb {
    fn drop(&mut self) {
        {
            let mut s = self.world.lock();
            let rec = &mut s.bombs[self.id];
            rec.dropped += 1;
            if rec.dropped > 1 {
                let detail = format!("bomb {} (site {:#x})", self.id, self.site);
                s.violate("M6: a user payload was destroyed twice", detail);
            }
        }
        if self.drop_armed && self.live() {
            self.fire("fuzz: injected payload destructor panic");
        }
    }
}

impl Clone for Bomb {
    fn clone(&self) -> Self {
        if self.clone_armed && self.live() {
            // Counted as `MSG_CLONE` only: this payload's destructor (its
            // `MSG_DROP` record) has not gone off. Clones happen in the timer
            // task (`send_interval`), never in the owner's turn or cleanup.
            self.world.count(site::MSG_CLONE);
            panic!("fuzz: injected message clone panic");
        }
        let mut copy = Bomb::new(&self.world, self.site, self.owner);
        // A copy keeps the original's arming, not the current one, and a
        // defused original's copy stays defused.
        copy.world.lock().bombs[copy.id].armed = self.drop_armed;
        copy.drop_armed = self.drop_armed;
        copy.clone_armed = self.clone_armed;
        copy.fmt_armed = self.fmt_armed;
        copy.defused = self.defused;
        copy
    }
}

impl std::fmt::Debug for Bomb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.fmt_armed && self.live() {
            self.world.count(site::FMT);
            panic!("fuzz: injected Debug panic");
        }
        write!(f, "Bomb({})", self.id)
    }
}

impl std::fmt::Display for Bomb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

/// A panic payload whose destructor panics (`PAYLOAD_DROP`): the framework
/// must dispose of it (`dispose_panic`) without unwinding further.
struct PayloadBomb {
    world: Arc<World>,
    id: usize,
}

impl Drop for PayloadBomb {
    fn drop(&mut self) {
        {
            let mut s = self.world.lock();
            let rec = &mut s.bombs[self.id];
            rec.dropped += 1;
            if rec.dropped > 1 {
                let detail = format!("panic payload {}", self.id);
                s.violate("M6: a user payload was destroyed twice", detail);
            }
        }
        if !std::thread::panicking() && !self.world.in_caller() {
            let task_owner = self.world.task_owner();
            self.world.lock().bombs[self.id].fired = Some(task_owner);
            panic!("fuzz: injected panic payload destructor panic");
        }
    }
}

/// Panic with `what`, or with a [`PayloadBomb`] when `PAYLOAD_DROP` is armed
/// (now, or for the instance: `faults`).
pub fn inject_panic(world: &Arc<World>, faults: u16, what: &'static str) -> ! {
    if (world.armed() | faults) & site::PAYLOAD_DROP != 0 {
        let id = {
            let mut s = world.lock();
            s.bombs.push(BombRec {
                site: site::PAYLOAD_DROP,
                owner: None,
                dropped: 0,
                armed: true,
                fired: None,
                own_panic: false,
            });
            s.bombs.len() - 1
        };
        std::panic::panic_any(PayloadBomb {
            world: world.clone(),
            id,
        });
    }
    panic!("{what}");
}

/// A subscriber that panics on every event outside the driver while
/// `TRACING` is armed (a user subscriber is user code: P1).
pub struct Subscriber {
    pub world: Arc<World>,
}

impl tracing::Subscriber for Subscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        if self.world.armed() & site::TRACING != 0
            && !std::thread::panicking()
            && !self.world.in_caller()
        {
            // No state lock here: an event may be emitted while it is held.
            self.world.count(site::TRACING);
            inject_panic(&self.world, 0, "fuzz: injected tracing subscriber panic");
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// A fault command (and its nonce: 0 for sends and timers).
#[derive(Clone)]
pub struct Fx {
    pub nonce: u64,
    pub cmd: FaultCmd,
    pub bomb: Bomb,
}
impl Message for Fx {
    type Result = Answer;
}

/// The reply to an `Fx` request (or to a deferred one).
pub struct Answer {
    pub nonce: u64,
    pub bomb: Bomb,
}

/// A job's terminal message (built by `done`, handled in the same turn).
pub struct JobDone {
    result: Option<Bomb>,
    token: Option<(u64, ReplyTo<Answer>)>,
}
impl Message for JobDone {
    type Result = ();
}

/// A job's emitted message.
pub struct Emitted {
    bomb: Bomb,
}
impl Message for Emitted {
    type Result = ();
}

impl Worker {
    /// Answer a deferred token. If the caller left, `reply` drops the
    /// payload here, in this turn (or in `stopped()`): a panic of the
    /// actor's own (attributed as `relaxed`), like any user code in a turn.
    /// (Only requests defer: a send or timer `Fx` has no caller, so its
    /// `defer()` fails and no nonce-0 token exists.)
    fn answer(&self, nonce: u64, token: ReplyTo<Answer>) {
        let bomb = Bomb::new(&self.world, site::REPLY_DROP, Some(self.instance));
        self.replying(|| token.reply(Answer { nonce, bomb }));
    }

    /// Run `f`, the actor's own `reply()` call, marked as such (`relaxed`).
    fn replying<T>(&self, f: impl FnOnce() -> T) -> T {
        struct Reset<'a>(&'a World, usize);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.lock().insts[self.1].replying = false;
            }
        }
        self.world.lock().insts[self.instance].replying = true;
        let _reset = Reset(&self.world, self.instance);
        f()
    }

    /// `stopped()`: answer every deferred token.
    pub(crate) fn answer_all(&mut self) {
        while let Some((nonce, token)) = self.deferred.pop_front() {
            self.answer(nonce, token);
        }
    }

    fn pipe(&mut self, cmd: FaultCmd, ctx: &Context<Self>) {
        let FaultCmd::Pipe {
            blocking,
            sleep,
            emits,
            at,
            deadline,
            keyed,
            answer,
        } = cmd
        else {
            return;
        };
        let world = self.world.clone();
        let instance = self.instance;
        let armed = world.armed();
        let mut opts = JobOptions::new();
        if keyed {
            opts = opts.key("fx");
        }
        if let Some(d) = deadline {
            opts = opts.deadline(Duration::from_millis(5 * d as u64));
        }
        let mut done_bomb = Bomb::new(&world, site::DONE_DROP, Some(instance));
        let token = if answer {
            self.deferred.pop_front()
        } else {
            None
        };
        let done_panic = armed & site::DONE_PANIC != 0;
        let work_panic = armed & site::WORK_PANIC != 0;
        let faults = self.faults;
        let done = {
            let world = world.clone();
            move |outcome: JobOutcome<Bomb>| {
                done_bomb.defuse();
                drop(done_bomb);
                if done_panic {
                    world.count(site::DONE_PANIC);
                    {
                        let mut s = world.lock();
                        s.insts[instance].panicked = true;
                        if let Some(id) = s.insts[instance].id {
                            s.stoppable(id);
                        }
                    }
                    inject_panic(&world, faults, "fuzz: injected done panic");
                }
                JobDone {
                    result: outcome.ok(),
                    token,
                }
            }
        };
        let emits = emits % 4;
        // A refused job's `work` and `done` drop inside this call (R7: "on
        // `Err`, `work` and `done` are dropped"): the handler's own context.
        let caller = self.world.clone();
        let _ = caller.calling(|| {
            if blocking {
                ctx.pipe_blocking(
                    opts,
                    move |job: JobCtx<Self>| {
                        for _ in 0..emits {
                            let e = Emitted {
                                bomb: Bomb::new(&world, site::EMIT_DROP, Some(instance)),
                            };
                            let _ = if at {
                                job.emit_at_blocking(
                                    spawned_concurrency::Instant::now() + Duration::from_millis(5),
                                    e,
                                )
                            } else {
                                job.emit_blocking(e)
                            };
                        }
                        if work_panic {
                            world.count(site::WORK_PANIC);
                            inject_panic(&world, faults, "fuzz: injected job panic");
                        }
                        Bomb::new(&world, site::RESULT_DROP, Some(instance))
                    },
                    done,
                )
            } else {
                ctx.pipe(
                    opts,
                    move |job: JobCtx<Self>| async move {
                        for _ in 0..emits {
                            let e = Emitted {
                                bomb: Bomb::new(&world, site::EMIT_DROP, Some(instance)),
                            };
                            let _ = if at {
                                job.emit_at(
                                    spawned_concurrency::Instant::now() + Duration::from_millis(5),
                                    e,
                                )
                                .await
                            } else {
                                job.emit(e).await
                            };
                        }
                        if sleep > 0 {
                            rt::sleep(Duration::from_millis(10 * sleep as u64)).await;
                        }
                        if work_panic {
                            world.count(site::WORK_PANIC);
                            inject_panic(&world, faults, "fuzz: injected job panic");
                        }
                        Bomb::new(&world, site::RESULT_DROP, Some(instance))
                    },
                    done,
                )
            }
        });
    }
}

impl Handler<Fx> for Worker {
    async fn handle(&mut self, mut msg: Fx, reply: PendingReply<'_, Answer>, ctx: &Context<Self>) {
        msg.bomb.defuse();
        {
            let mut s = self.world.lock();
            self.turn(&mut s, "Fx");
            if msg.nonce != 0 {
                if let Some(other) = s.handled.insert(msg.nonce, self.instance) {
                    let detail = format!("nonce {} by w{other} and w{}", msg.nonce, self.instance);
                    s.violate("D2: a message was handled twice", detail);
                }
            }
        }
        match msg.cmd {
            FaultCmd::Defer => {
                // A send or timer has no caller (`NoCaller`): nothing to
                // defer or answer. A deferred token may later be dropped
                // (`ReplyDropped`); at the credit bound the caller is
                // answered `Overloaded` now. Either makes that error valid.
                match reply.defer() {
                    Ok(token) => {
                        self.world.lock().deferred.insert(msg.nonce);
                        self.deferred.push_back((msg.nonce, token));
                    }
                    Err(DeferError::Overloaded) => {
                        self.world.lock().deferred.insert(msg.nonce);
                    }
                    Err(_) => {}
                }
                return;
            }
            FaultCmd::Answer(n) => {
                for _ in 0..=n % 4 {
                    let Some((nonce, token)) = self.deferred.pop_front() else {
                        break;
                    };
                    self.answer(nonce, token);
                }
            }
            FaultCmd::DropToken => drop(self.deferred.pop_front()),
            FaultCmd::Pipe { .. } => self.pipe(msg.cmd, ctx),
            FaultCmd::Panic => {
                {
                    let mut s = self.world.lock();
                    s.insts[self.instance].panicked = true;
                }
                inject_panic(&self.world, self.faults, "fuzz: injected handler panic");
            }
            FaultCmd::Helper(v) => {
                let world = self.world.clone();
                let owner = self.instance;
                let caller = self.world.clone();
                let _ = caller.calling(|| {
                    ctx.spawn_helper(async move {
                        // Created in the first poll: a future dropped unpolled
                        // (a refused helper) carries no bomb.
                        let mut bomb = Bomb::new(&world, site::HELPER_DROP, Some(owner));
                        rt::sleep(Duration::from_millis(10 * v as u64)).await;
                        bomb.defuse();
                    })
                });
            }
            FaultCmd::Reply => {}
        }
        let mut bomb = Bomb::new(&self.world, site::REPLY_DROP, Some(self.instance));
        // A send has no caller: its reply is always dropped here. A request
        // whose caller left drops it here too, a panic in this turn.
        if msg.nonce == 0 {
            bomb.defuse();
        }
        self.replying(|| {
            reply.reply(Answer {
                nonce: msg.nonce,
                bomb,
            })
        });
    }
}

impl Handler<JobDone> for Worker {
    async fn handle(&mut self, msg: JobDone, _: PendingReply<'_, ()>, _: &Context<Self>) {
        {
            let mut s = self.world.lock();
            self.turn(&mut s, "JobDone");
        }
        let JobDone { result, token } = msg;
        if let Some(mut b) = result {
            b.defuse();
        }
        if let Some((nonce, token)) = token {
            self.answer(nonce, token);
        }
    }
}

impl Handler<Emitted> for Worker {
    async fn handle(&mut self, mut msg: Emitted, _: PendingReply<'_, ()>, _: &Context<Self>) {
        msg.bomb.defuse();
        let mut s = self.world.lock();
        self.turn(&mut s, "Emitted");
    }
}

/// The simulated task running now; `None` outside the simulation (a
/// thread that is not a run's, where `task_id` would read the root's id).
pub fn task() -> Option<sim::Id> {
    sim::Handle::try_current().is_ok().then(sim::task_id)
}
