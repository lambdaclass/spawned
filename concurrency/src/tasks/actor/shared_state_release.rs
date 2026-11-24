//! `ActorRef` and `Context` share one `Arc<ActorShared>`. Once the actor has
//! stopped and every handle is dropped, that block must be freed: nothing
//! the runtime keeps (links, monitors, child handles, the run task) may hold it.
use super::*;
use std::sync::Weak;

async fn freed<A: Actor>(weak: &Weak<ActorShared<A>>) -> bool {
    for _ in 0..500 {
        if weak.upgrade().is_none() {
            return true;
        }
        rt::sleep(Duration::from_millis(10)).await;
    }
    false
}

struct Plain;
impl Actor for Plain {}
impl Handler<Down> for Plain {
    async fn handle(
        &mut self,
        _: Down,
        reply: crate::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        reply.reply(());
    }
}

/// Keeps a clone of its own `Context` in its state.
struct Holder(Option<Context<Holder>>);
impl Actor for Holder {
    async fn started(&mut self, ctx: &Context<Self>) {
        self.0 = Some(ctx.clone());
    }
}

#[test]
fn freed_after_stop_and_last_handle_dropped() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::freed_after_stop_and_last_handle_dropped"),
        move || {
            rt::Runtime::new().unwrap().block_on(async {
                let actor = Plain.spawn();
                let weak = actor.shared_weak();
                let extra = (actor.clone(), actor.context(), actor.child_handle());
                actor.context().stop();
                actor.join().await;
                drop(extra);
                drop(actor);
                assert!(freed(&weak).await, "ActorShared outlived every handle");
            });
        },
    );
}

#[test]
fn freed_when_context_was_stored_in_actor_state() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::freed_when_context_was_stored_in_actor_state"
        ),
        move || {
            rt::Runtime::new().unwrap().block_on(async {
                let actor = Holder(None).spawn();
                let weak = actor.shared_weak();
                actor.context().stop();
                actor.join().await;
                drop(actor);
                assert!(freed(&weak).await, "Context kept in actor state leaked");
            });
        },
    );
}

#[test]
fn freed_with_links_and_monitors() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::freed_with_links_and_monitors"),
        move || {
            rt::Runtime::new().unwrap().block_on(async {
                let a = Plain.spawn();
                let b = Plain.spawn();
                let watcher = Plain.spawn();
                a.context().trap_exit(true);
                a.context().link(&b.child_handle());
                let _monitor = watcher.context().monitor(&a.child_handle());
                watcher.context().monitor(&b.child_handle());
                let child = Plain.start_link(&a.context()).actor_ref().unwrap();
                let weaks = [
                    a.shared_weak(),
                    b.shared_weak(),
                    watcher.shared_weak(),
                    child.shared_weak(),
                ];
                for actor in [&b, &a, &watcher] {
                    actor.context().stop();
                    actor.join().await;
                }
                child.context().stop();
                child.join().await;
                drop((a, b, watcher, child));
                for weak in &weaks {
                    assert!(freed(weak).await, "linked or monitored actor leaked");
                }
            });
        },
    );
}
