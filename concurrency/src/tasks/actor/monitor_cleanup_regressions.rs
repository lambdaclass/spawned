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
        let released = crate::test_wait::wait_for(|| waiter.strong_count() == 0, |r| *r);
        observer.child_handle().stop();
        rt.block_on(observer.join());
        drop(observer);
        target.child_handle().stop();
        rt.block_on(target.join());
        assert!(released, "demonitor retained its waiter until target exit");
        return;
    }
    let released = crate::test_wait::wait_for(|| waiter.strong_count() == 0, |r| *r);
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

/// L7: an exiting observer cancels its outgoing monitors before its exit is
/// published, not after. On a current-thread runtime woken tasks run in wake
/// order, so the waiter woken by that cancellation runs (and releases its
/// handle of the still-running target) before any task woken by the
/// publication. Cancelling at or after publication lets the publication's
/// task see the waiter still holding the handle. The cleanup is shared with
/// threads mode (`actor_cleanup.rs`), whose waiters are threads.
#[cfg(not(spawned_sim))]
#[test]
fn observer_cancels_its_monitors_before_publishing_its_exit() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::observer_cancels_its_monitors_before_publishing_its_exit"
        ),
        move || {
            let rt = ::tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let target = Idle.start();
                let observer = Idle.start();
                let child = target.child_handle();
                let waiter = child.lifetime_probe();
                observer.context().monitor(&child);
                drop(child);
                // The waiter and the checker park before the observer stops.
                let published = observer.child_handle();
                let checker = ::tokio::spawn(async move {
                    published.wait_exit_async().await;
                    waiter.strong_count()
                });
                for _ in 0..8 {
                    ::tokio::task::yield_now().await;
                }
                assert!(target.child_handle().is_alive());
                observer.child_handle().stop();
                let held = ::tokio::time::timeout(crate::test_wait::WAIT, checker)
                    .await
                    .expect("the observer did not exit")
                    .unwrap();
                assert_eq!(
                    held, 0,
                    "L7: the observer's exit was published before its monitor was cancelled"
                );
                target.child_handle().stop();
                target.join().await;
                observer.join().await;
            });
        },
    );
}
