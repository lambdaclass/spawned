use super::*;
struct Idle;
impl Actor for Idle {}
impl Handler<Down> for Idle {
    async fn handle(
        &mut self,
        _: Down,
        reply: crate::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        reply.reply(());
    }
}
#[test]
fn closed_monitor_table_rejects_late_registration() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::closed_monitor_table_rejects_late_registration"
        ),
        move || {
            let rt = rt::Runtime::new().unwrap();
            let _entered = rt.enter();
            let observer = Idle.start();
            let target = Idle.start();
            // Model cleanup during an unexpected unwind before token cancellation.
            drop(MonitorCleanup(observer.shared.monitors.clone()));
            let child = target.child_handle();
            let waiter = child.lifetime_probe();
            observer.context().monitor(&child);
            drop(child);
            assert_eq!(waiter.strong_count(), 0);
            assert!(observer.shared.monitors.lock().unwrap().is_none());
            observer.child_handle().stop();
            target.child_handle().stop();
            rt.block_on(observer.join());
            rt.block_on(target.join());
        },
    );
}
/// Poll `done` while driving `rt`: the waiter is released by a task on that
/// runtime. A blocking `test_wait::wait_for` here would hang under
/// spawned_sim, where nothing runs the (single-threaded) simulated runtime
/// outside `block_on` and the entered thread's clock is virtual, so neither
/// the condition nor the deadline would ever change.
fn released(rt: &rt::Runtime, done: impl Fn() -> bool) -> bool {
    rt.block_on(crate::test_wait::wait_for_async(
        || std::future::ready(done()),
        |r| *r,
    ))
}
fn check_cleanup(stop_owner: bool) {
    let rt = rt::Runtime::new().unwrap();
    let _entered = rt.enter();
    let target = Idle.start();
    let observer = Idle.start();
    let child = target.child_handle();
    let waiter = child.lifetime_probe();
    let monitor = observer.context().monitor(&child);
    drop(child);
    if stop_owner {
        observer.child_handle().stop();
        rt.block_on(observer.join());
        drop(observer);
    } else {
        observer.context().demonitor(monitor);
        // Only the waiter now owns this particular ChildHandle allocation.
        let released = released(&rt, || waiter.strong_count() == 0);
        observer.child_handle().stop();
        rt.block_on(observer.join());
        drop(observer);
        target.child_handle().stop();
        rt.block_on(target.join());
        assert!(released, "demonitor retained its waiter until target exit");
        return;
    }
    let released = released(&rt, || waiter.strong_count() == 0);
    target.child_handle().stop();
    rt.block_on(target.join());
    assert!(released, "stopped observer retained its monitor waiter");
}
#[test]
fn demonitor_releases_waiter() {
    check_cleanup(false);
}
#[test]
fn stopped_observer_releases_waiter() {
    check_cleanup(true);
}
