use super::*;
struct Idle;
impl Actor for Idle {}
impl Handler<Down> for Idle {
    fn handle(&mut self, _: Down, reply: crate::reply::PendingReply<'_, ()>, _: &Context<Self>) {
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
            let observer = Idle.start();
            let target = Idle.start();
            // Model cleanup during an unexpected unwind before token cancellation.
            drop(MonitorCleanup(observer.monitors.clone()));
            let child = target.child_handle();
            let waiter = child.lifetime_probe();
            observer.context().monitor(&child);
            drop(child);
            assert_eq!(waiter.strong_count(), 0);
            assert!(observer.monitors.lock().unwrap().is_none());
            observer.child_handle().stop();
            target.child_handle().stop();
            observer.join();
            target.join();
        },
    );
}
fn check_cleanup(stop_owner: bool) {
    let target = Idle.start();
    let observer = Idle.start();
    let child = target.child_handle();
    let waiter = child.lifetime_probe();
    let monitor = observer.context().monitor(&child);
    drop(child);
    if stop_owner {
        observer.child_handle().stop();
        observer.join();
        drop(observer);
    } else {
        observer.context().demonitor(monitor);
        // Only the waiter now owns this particular ChildHandle allocation.
        let released = crate::test_wait::wait_for(|| waiter.strong_count() == 0, |r| *r);
        observer.child_handle().stop();
        observer.join();
        drop(observer);
        target.child_handle().stop();
        target.join();
        assert!(released, "demonitor retained its waiter until target exit");
        return;
    }
    let released = crate::test_wait::wait_for(|| waiter.strong_count() == 0, |r| *r);
    target.child_handle().stop();
    target.join();
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
