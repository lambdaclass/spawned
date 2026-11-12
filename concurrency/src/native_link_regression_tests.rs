use crate::{link, ActorId, ExitReason};
use std::time::Duration;
#[test]
fn start_linked_registers_before_threads_started() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::start_linked_registers_before_threads_started"
        ),
        move || {
            use crate::threads::{
                Actor as ThreadActor, ActorStart as ThreadStart, Context as ThreadContext,
            };
            struct Parent;
            impl ThreadActor for Parent {}
            struct Child {
                parent: ActorId,
                seen: std::sync::mpsc::Sender<bool>,
            }
            impl ThreadActor for Child {
                fn started(&mut self, ctx: &ThreadContext<Self>) {
                    let h = ctx.actor_ref().child_handle();
                    self.seen
                        .send(link::has_link(h.links(), self.parent))
                        .unwrap();
                    ctx.stop();
                }
            }
            let parent = Parent.start();
            for _ in 0..32 {
                let (tx, rx) = std::sync::mpsc::channel();
                let child = Child {
                    parent: parent.id(),
                    seen: tx,
                }
                .start_linked(&parent.context());
                assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap());
                assert_eq!(
                    child.child_handle().wait_exit_blocking(),
                    ExitReason::Normal
                );
            }
            parent.child_handle().stop();
            parent.child_handle().wait_exit_blocking();
        },
    );
}
