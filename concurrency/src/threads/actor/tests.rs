
use super::*;
use crate::message::Message;
use std::thread;

struct Counter {
    count: u64,
}

struct GetCount;
impl Message for GetCount {
    type Result = u64;
}

struct Increment;
impl Message for Increment {
    type Result = u64;
}

struct StopCounter;
impl Message for StopCounter {
    type Result = u64;
}

impl Actor for Counter {}

impl Handler<GetCount> for Counter {
    fn handle(
        &mut self,
        _msg: GetCount,
        reply: crate::reply::PendingReply<'_, u64>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.count);
    }
}

impl Handler<Increment> for Counter {
    fn handle(
        &mut self,
        _msg: Increment,
        reply: crate::reply::PendingReply<'_, u64>,
        _ctx: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            self.count += 1;
            self.count
        };
        reply.reply(__reply_value);
    }
}

impl Handler<StopCounter> for Counter {
    fn handle(
        &mut self,
        _msg: StopCounter,
        reply: crate::reply::PendingReply<'_, u64>,
        ctx: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            ctx.stop();
            self.count
        };
        reply.reply(__reply_value);
    }
}

#[test]
fn basic_send_and_request() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::basic_send_and_request"),
        move || {
            let actor = Counter { count: 0 }.start();
            assert_eq!(actor.request(GetCount).unwrap(), 0);
            assert_eq!(actor.request(Increment).unwrap(), 1);
            // Same mailbox: the request is handled after the send.
            actor.send(Increment).unwrap();
            assert_eq!(actor.request(GetCount).unwrap(), 2);
            actor.request(StopCounter).unwrap();
        },
    );
}

#[test]
fn join_waits_for_completion() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::join_waits_for_completion"),
        move || {
            struct SlowStop;
            struct StopSlow;
            impl Message for StopSlow {
                type Result = ();
            }
            impl Actor for SlowStop {
                fn stopped(&mut self, _ctx: &Context<Self>) {
                    rt::sleep(Duration::from_millis(300));
                }
            }
            impl Handler<StopSlow> for SlowStop {
                fn handle(
                    &mut self,
                    _msg: StopSlow,
                    reply: crate::reply::PendingReply<'_, ()>,
                    ctx: &Context<Self>,
                ) {
                    ctx.stop();
                    reply.reply(());
                }
            }

            let actor = SlowStop.start();
            actor.send(StopSlow).unwrap();
            actor.join();
            // If join() returned, stopped() has completed
        },
    );
}

#[test]
fn join_multiple_callers() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::join_multiple_callers"),
        move || {
            struct SlowStop2;
            struct StopSlow2;
            impl Message for StopSlow2 {
                type Result = ();
            }
            impl Actor for SlowStop2 {
                fn stopped(&mut self, _ctx: &Context<Self>) {
                    rt::sleep(Duration::from_millis(200));
                }
            }
            impl Handler<StopSlow2> for SlowStop2 {
                fn handle(
                    &mut self,
                    _msg: StopSlow2,
                    reply: crate::reply::PendingReply<'_, ()>,
                    ctx: &Context<Self>,
                ) {
                    ctx.stop();
                    reply.reply(());
                }
            }

            let actor = SlowStop2.start();
            let a1 = actor.clone();
            let a2 = actor.clone();
            let t1 = thread::spawn(move || {
                a1.join();
                1u32
            });
            let t2 = thread::spawn(move || {
                a2.join();
                2u32
            });
            actor.send(StopSlow2).unwrap();
            assert_eq!(t1.join().unwrap(), 1);
            assert_eq!(t2.join().unwrap(), 2);
        },
    );
}

#[test]
fn panic_in_started_stops_actor() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::panic_in_started_stops_actor"),
        move || {
            struct PanicOnStart;
            struct PingThread;
            impl Message for PingThread {
                type Result = ();
            }
            impl Actor for PanicOnStart {
                fn started(&mut self, _ctx: &Context<Self>) {
                    panic!("boom in started");
                }
            }
            impl Handler<PingThread> for PanicOnStart {
                fn handle(
                    &mut self,
                    _msg: PingThread,
                    reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    reply.reply(());
                }
            }

            let actor = PanicOnStart.start();
            actor.join();
            let result = actor.send(PingThread);
            assert!(result.is_err());
        },
    );
}

#[test]
fn panic_in_handler_stops_actor() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::panic_in_handler_stops_actor"),
        move || {
            struct PanicOnMsg;
            struct ExplodeThread;
            impl Message for ExplodeThread {
                type Result = ();
            }
            struct CheckThread;
            impl Message for CheckThread {
                type Result = u32;
            }
            impl Actor for PanicOnMsg {}
            impl Handler<ExplodeThread> for PanicOnMsg {
                fn handle(
                    &mut self,
                    _msg: ExplodeThread,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("boom in handler");
                    }
                }
            }
            impl Handler<CheckThread> for PanicOnMsg {
                fn handle(
                    &mut self,
                    _msg: CheckThread,
                    reply: crate::reply::PendingReply<'_, u32>,
                    _ctx: &Context<Self>,
                ) {
                    reply.reply(42);
                }
            }

            let actor = PanicOnMsg.start();
            actor.send(ExplodeThread).unwrap();
            actor.join();
            let result = actor.request(CheckThread);
            assert!(result.is_err());
        },
    );
}

#[test]
fn panic_in_stopped_still_completes() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::panic_in_stopped_still_completes"),
        move || {
            struct PanicOnStop;
            struct StopMeThread;
            impl Message for StopMeThread {
                type Result = ();
            }
            impl Actor for PanicOnStop {
                fn stopped(&mut self, _ctx: &Context<Self>) {
                    panic!("boom in stopped");
                }
            }
            impl Handler<StopMeThread> for PanicOnStop {
                fn handle(
                    &mut self,
                    _msg: StopMeThread,
                    reply: crate::reply::PendingReply<'_, ()>,
                    ctx: &Context<Self>,
                ) {
                    ctx.stop();
                    reply.reply(());
                }
            }

            let actor = PanicOnStop.start();
            actor.send(StopMeThread).unwrap();
            actor.join();
        },
    );
}

#[test]
fn recipient_type_erasure() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::recipient_type_erasure"),
        move || {
            let actor = Counter { count: 42 }.start();
            let recipient: Recipient<GetCount> = actor.recipient();
            let result = request(&*recipient, GetCount, Duration::from_secs(5)).unwrap();
            assert_eq!(result, 42);
        },
    );
}

#[test]
fn send_message_on_delivers() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::send_message_on_delivers"),
        move || {
            let actor = Counter { count: 0 }.start();
            let ctx = actor.context();
            send_message_on(ctx, || rt::sleep(Duration::from_millis(10)), Increment);
            let count =
                crate::test_wait::wait_for(|| actor.request(GetCount), |c| matches!(c, Ok(1)))
                    .unwrap();
            assert_eq!(count, 1);
        },
    );
}

// --- ExitReason tests ---

#[test]
fn exit_reason_normal_on_clean_stop() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::exit_reason_normal_on_clean_stop"),
        move || {
            let actor = Counter { count: 0 }.start();
            actor.request(StopCounter).unwrap();
            let reason = actor.wait_exit();
            assert!(matches!(reason, ExitReason::Normal));
        },
    );
}

#[test]
fn exit_reason_panic_in_started() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::exit_reason_panic_in_started"),
        move || {
            struct PanicStartThread;
            struct PingThread2;
            impl Message for PingThread2 {
                type Result = ();
            }
            impl Actor for PanicStartThread {
                fn started(&mut self, _ctx: &Context<Self>) {
                    panic!("boom in started");
                }
            }
            impl Handler<PingThread2> for PanicStartThread {
                fn handle(
                    &mut self,
                    _msg: PingThread2,
                    reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    reply.reply(());
                }
            }

            let actor = PanicStartThread.start();
            let reason = actor.wait_exit();
            assert!(
                matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in started"))
            );
        },
    );
}

#[test]
fn exit_reason_panic_in_handler() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::exit_reason_panic_in_handler"),
        move || {
            struct PanicHandlerThread;
            struct ExplodeThread2;
            impl Message for ExplodeThread2 {
                type Result = ();
            }
            impl Actor for PanicHandlerThread {}
            impl Handler<ExplodeThread2> for PanicHandlerThread {
                fn handle(
                    &mut self,
                    _msg: ExplodeThread2,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("boom in handler");
                    }
                }
            }

            let actor = PanicHandlerThread.start();
            let _ = actor.send(ExplodeThread2);
            let reason = actor.wait_exit();
            assert!(
                matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in handler"))
            );
        },
    );
}

#[test]
fn exit_reason_poll_returns_none_while_running() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::exit_reason_poll_returns_none_while_running"
        ),
        move || {
            let actor = Counter { count: 0 }.start();
            assert!(actor.exit_reason().is_none());
            actor.request(StopCounter).unwrap();
            actor.join();
            assert!(actor.exit_reason().is_some());
        },
    );
}

// --- Monitor tests ---

struct GetDowns;
impl Message for GetDowns {
    type Result = Vec<crate::monitor::Down>;
}

struct Watcher {
    downs: Arc<Mutex<Vec<crate::monitor::Down>>>,
}

struct StartMonitor(crate::ChildHandle);
impl Message for StartMonitor {
    type Result = crate::monitor::MonitorRef;
}
struct CallDemonitor(crate::monitor::MonitorRef);
impl Message for CallDemonitor {
    type Result = ();
}
/// Monitor, then demonitor in the same turn.
struct MonitorThenDemonitor(crate::ChildHandle);
impl Message for MonitorThenDemonitor {
    type Result = ();
}

impl Actor for Watcher {}

impl Handler<StartMonitor> for Watcher {
    fn handle(
        &mut self,
        msg: StartMonitor,
        reply: crate::reply::PendingReply<'_, crate::monitor::MonitorRef>,
        ctx: &Context<Self>,
    ) {
        reply.reply(ctx.monitor(&msg.0));
    }
}

impl Handler<MonitorThenDemonitor> for Watcher {
    fn handle(
        &mut self,
        msg: MonitorThenDemonitor,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        let monitor = ctx.monitor(&msg.0);
        ctx.demonitor(monitor);
        reply.reply(());
    }
}

impl Handler<CallDemonitor> for Watcher {
    fn handle(
        &mut self,
        msg: CallDemonitor,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.demonitor(msg.0);
        reply.reply(());
    }
}

impl Handler<crate::monitor::Down> for Watcher {
    fn handle(
        &mut self,
        msg: crate::monitor::Down,
        reply: crate::reply::PendingReply<'_, ()>,
        _ctx: &Context<Self>,
    ) {
        self.downs.lock().unwrap().push(msg);
        reply.reply(());
    }
}

impl Handler<GetDowns> for Watcher {
    fn handle(
        &mut self,
        _msg: GetDowns,
        reply: crate::reply::PendingReply<'_, Vec<crate::monitor::Down>>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.downs.lock().unwrap().clone());
    }
}

fn make_watcher() -> ActorRef<Watcher> {
    Watcher {
        downs: Arc::new(Mutex::new(Vec::new())),
    }
    .start()
}

#[test]
fn monitor_running_actor_delivers_down_on_exit() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::monitor_running_actor_delivers_down_on_exit"
        ),
        move || {
            let target = Counter { count: 0 }.start();
            let target_handle = target.child_handle();
            let watcher = make_watcher();

            let monitor_ref = watcher.request(StartMonitor(target_handle)).unwrap();

            target.request(StopCounter).unwrap();
            target.join();
            let downs = crate::test_wait::wait_for(
                || watcher.request(GetDowns),
                |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
            )
            .unwrap();
            assert_eq!(downs.len(), 1);
            assert_eq!(downs[0].monitor_ref, monitor_ref);
            assert!(matches!(downs[0].reason, ExitReason::Normal));
        },
    );
}

/// OTP `demonitor(Ref, [flush])`: a `Down` already in the mailbox
/// (here the `NoProc` a monitor of a dead target queues at once) is
/// never handled once `demonitor` returned.
#[test]
fn demonitor_flushes_a_queued_down() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::demonitor_flushes_a_queued_down"),
        move || {
            let target = Counter { count: 0 }.start();
            target.request(StopCounter).unwrap();
            target.join();
            let watcher = make_watcher();
            watcher
                .request(MonitorThenDemonitor(target.child_handle()))
                .unwrap();
            // FIFO: the queued Down is dispatched before this request.
            let downs = watcher.request(GetDowns).unwrap();
            assert!(downs.is_empty(), "a flushed Down was handled: {downs:?}");
        },
    );
}

#[test]
fn monitor_already_dead_actor_delivers_down_immediately() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::monitor_already_dead_actor_delivers_down_immediately"
        ),
        move || {
            let target = Counter { count: 0 }.start();
            target.request(StopCounter).unwrap();
            target.join();
            let target_handle = target.child_handle();

            let watcher = make_watcher();
            let _ = watcher.request(StartMonitor(target_handle)).unwrap();
            let downs = crate::test_wait::wait_for(
                || watcher.request(GetDowns),
                |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
            )
            .unwrap();
            assert_eq!(downs.len(), 1);
        },
    );
}

#[test]
fn demonitor_before_target_dies_suppresses_down() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::demonitor_before_target_dies_suppresses_down"
        ),
        move || {
            let target = Counter { count: 0 }.start();
            let target_handle = target.child_handle();
            let watcher = make_watcher();

            let monitor_ref = watcher.request(StartMonitor(target_handle)).unwrap();
            watcher.request(CallDemonitor(monitor_ref)).unwrap();

            target.request(StopCounter).unwrap();
            target.join();
            rt::sleep(Duration::from_millis(150));

            let downs = watcher.request(GetDowns).unwrap();
            assert!(downs.is_empty());
        },
    );
}

#[test]
fn multiple_monitors_each_get_own_ref_and_down() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::multiple_monitors_each_get_own_ref_and_down"
        ),
        move || {
            let target = Counter { count: 0 }.start();
            let target_handle = target.child_handle();
            let watcher = make_watcher();

            let r1 = watcher
                .request(StartMonitor(target_handle.clone()))
                .unwrap();
            let r2 = watcher.request(StartMonitor(target_handle)).unwrap();
            assert_ne!(r1, r2);

            target.request(StopCounter).unwrap();
            target.join();
            let downs = crate::test_wait::wait_for(
                || watcher.request(GetDowns),
                |downs| downs.as_ref().map_or(true, |d| d.len() >= 2),
            )
            .unwrap();
            assert_eq!(downs.len(), 2);
            let refs: Vec<_> = downs.iter().map(|d| d.monitor_ref).collect();
            assert!(refs.contains(&r1));
            assert!(refs.contains(&r2));
        },
    );
}

#[test]
fn monitor_observes_panic_reason() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::monitor_observes_panic_reason"),
        move || {
            struct PanicMsg;
            impl Message for PanicMsg {
                type Result = ();
            }
            struct PanicMe;
            impl Actor for PanicMe {}
            impl Handler<PanicMsg> for PanicMe {
                fn handle(
                    &mut self,
                    _msg: PanicMsg,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("intentional panic");
                    }
                }
            }

            let target = PanicMe.start();
            let target_handle = target.child_handle();
            let watcher = make_watcher();

            let _ = watcher.request(StartMonitor(target_handle)).unwrap();
            let _ = target.send(PanicMsg);

            let downs = crate::test_wait::wait_for(
                || watcher.request(GetDowns),
                |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
            )
            .unwrap();
            assert_eq!(downs.len(), 1);
            assert!(matches!(downs[0].reason, ExitReason::Panic(_)));
        },
    );
}

// --- Link tests ---

struct TrapActor {
    exits: Arc<Mutex<Vec<Exit>>>,
    trap: bool,
}

struct GetExits;
impl Message for GetExits {
    type Result = Vec<Exit>;
}
struct LinkTo(crate::ChildHandle);
impl Message for LinkTo {
    type Result = ();
}

impl Actor for TrapActor {
    fn started(&mut self, ctx: &Context<Self>) {
        ctx.trap_exit(self.trap);
    }
    fn exit_received(&mut self, exit: Exit, _ctx: &Context<Self>) {
        self.exits.lock().unwrap().push(exit);
    }
}

impl Handler<LinkTo> for TrapActor {
    fn handle(
        &mut self,
        msg: LinkTo,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.link(&msg.0);
        reply.reply(());
    }
}

impl Handler<GetExits> for TrapActor {
    fn handle(
        &mut self,
        _msg: GetExits,
        reply: crate::reply::PendingReply<'_, Vec<Exit>>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.exits.lock().unwrap().clone());
    }
}

fn make_trapper(trap: bool) -> ActorRef<TrapActor> {
    TrapActor {
        exits: Arc::new(Mutex::new(Vec::new())),
        trap,
    }
    .start()
}

#[test]
fn link_propagates_panic_to_non_trapping_peer_threads() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::link_propagates_panic_to_non_trapping_peer_threads"
        ),
        move || {
            struct Boom;
            impl Message for Boom {
                type Result = ();
            }
            impl Handler<Boom> for TrapActor {
                fn handle(
                    &mut self,
                    _msg: Boom,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("boom");
                    }
                }
            }

            let a = make_trapper(false);
            let b = make_trapper(false);
            a.request(LinkTo(b.child_handle())).unwrap();

            let _ = a.send(Boom);
            let reason_a = a.wait_exit();
            let reason_b = b.wait_exit();
            assert!(matches!(reason_a, ExitReason::Panic(_)));
            assert!(matches!(reason_b, ExitReason::Panic(_)));
        },
    );
}

#[test]
fn link_delivers_exit_to_trapping_peer_threads() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::link_delivers_exit_to_trapping_peer_threads"
        ),
        move || {
            struct Boom;
            impl Message for Boom {
                type Result = ();
            }
            impl Handler<Boom> for TrapActor {
                fn handle(
                    &mut self,
                    _msg: Boom,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("boom");
                    }
                }
            }

            let a = make_trapper(false);
            let b = make_trapper(true);
            b.request(LinkTo(a.child_handle())).unwrap();

            let _ = a.send(Boom);
            a.wait_exit();
            let exits = crate::test_wait::wait_for(
                || b.request(GetExits),
                |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
            )
            .unwrap();
            assert!(b.exit_reason().is_none());
            assert_eq!(exits.len(), 1);
            assert_eq!(exits[0].from, a.id());
            assert!(matches!(exits[0].reason, ExitReason::Panic(_)));

            let bh = b.child_handle();
            bh.stop();
            bh.wait_exit_blocking();
        },
    );
}

#[test]
fn start_linked_threads() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::start_linked_threads"),
        move || {
            struct Parent;
            struct PanicParent;
            impl Message for PanicParent {
                type Result = ();
            }
            impl Actor for Parent {}
            impl Handler<PanicParent> for Parent {
                fn handle(
                    &mut self,
                    _msg: PanicParent,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("parent boom");
                    }
                }
            }

            struct Child;
            impl Actor for Child {}

            let parent = Parent.start();
            let child = Child.start_linked(&parent.context());

            let _ = parent.send(PanicParent);
            parent.wait_exit();
            let reason = child.wait_exit();
            assert!(matches!(reason, ExitReason::Panic(_)));
        },
    );
}
