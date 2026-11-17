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
            let parent = Parent.spawn();
            for _ in 0..32 {
                let (tx, rx) = std::sync::mpsc::channel();
                // The child stops itself in started(): a failed start, answered
                // after its publication with its final reason.
                let result = Child {
                    parent: parent.id(),
                    seen: tx,
                }
                .start_link(&parent.context());
                assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap());
                assert!(matches!(
                    result,
                    Err(crate::StartError::InitExited {
                        reason: ExitReason::Normal,
                        pending: None
                    })
                ));
            }
            parent.child_handle().stop();
            parent.child_handle().wait_exit_blocking();
        },
    );
}
