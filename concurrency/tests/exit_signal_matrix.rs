//! Link/monitor delivery is independent of whether the source was directly killed.
use spawned_concurrency::{message::Message, ChildHandle, Down, Exit, ExitReason};
use std::{sync::mpsc, time::Duration};
const LIMIT: Duration = Duration::from_secs(30);
#[derive(Clone, Copy, Debug)]
enum Relation {
    Link,
    Monitor,
}
#[derive(Debug, PartialEq)]
enum Notice {
    Exit(ExitReason),
    Down(ExitReason),
}
struct Ping;
impl Message for Ping {
    type Result = ();
}
fn reasons() -> [ExitReason; 3] {
    [
        ExitReason::Kill,
        ExitReason::Panic("reported failure".into()),
        ExitReason::Shutdown,
    ]
}
macro_rules! actors {
    ($($asynchronous:ident)?) => {
        struct Idle;
        impl Actor for Idle {}
        impl Handler<Ping> for Idle {
            $($asynchronous)? fn handle(&mut self, _: Ping, _: &Context<Self>) {}
        }
        struct Observer(mpsc::Sender<Notice>);
        impl Actor for Observer {
            $($asynchronous)? fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
                self.0.send(Notice::Exit(exit.reason)).unwrap();
            }
        }
        impl Handler<Ping> for Observer {
            $($asynchronous)? fn handle(&mut self, _: Ping, _: &Context<Self>) {}
        }
        impl Handler<Down> for Observer {
            $($asynchronous)? fn handle(&mut self, down: Down, _: &Context<Self>) {
                self.0.send(Notice::Down(down.reason)).unwrap();
            }
        }
    };
}
mod native {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    actors!();
    fn joined(child: ChildHandle) -> ExitReason {
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || send.send(child.wait_exit_blocking()).unwrap());
        receive
            .recv_timeout(LIMIT)
            .expect("native exit did not complete")
    }
    #[test]
    fn links_monitors_trapping_and_exit_reasons_form_a_complete_native_matrix() {
        for relation in [Relation::Link, Relation::Monitor] {
            for trapping in [false, true] {
                for reason in reasons() {
                    let source = Idle.start();
                    let (send, notices) = mpsc::channel();
                    let observer = Observer(send).start();
                    source.request(Ping).unwrap();
                    observer.request(Ping).unwrap();
                    observer.context().trap_exit(trapping);
                    let _monitor = match relation {
                        Relation::Link => {
                            observer.context().link(&source.child_handle());
                            None
                        }
                        Relation::Monitor => {
                            Some(observer.context().monitor(&source.child_handle()))
                        }
                    };
                    source.child_handle().stop_with_reason(reason.clone());
                    assert_eq!(joined(source.child_handle()), reason);
                    if matches!(relation, Relation::Link) && !trapping {
                        assert_eq!(joined(observer.child_handle()), reason);
                    } else {
                        let expected = match relation {
                            Relation::Link => Notice::Exit(reason),
                            Relation::Monitor => Notice::Down(reason),
                        };
                        assert_eq!(
                            notices.recv_timeout(LIMIT).unwrap(),
                            expected,
                            "{relation:?}, trap={trapping}"
                        );
                        observer.request(Ping).unwrap();
                        assert_eq!(observer.child_handle().exit_reason(), None);
                        observer.context().stop();
                        joined(observer.child_handle());
                    }
                    assert!(
                        matches!(notices.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
                        "extra notice: {relation:?}, trap={trapping}"
                    );
                }
            }
        }
    }
}
#[cfg(feature = "async")]
mod asynchronous {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    actors!(async);
    async fn joined(child: ChildHandle) -> ExitReason {
        tokio::time::timeout(LIMIT, child.wait_exit_async())
            .await
            .expect("async exit did not complete")
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn links_monitors_trapping_and_exit_reasons_form_a_complete_async_matrix() {
        for relation in [Relation::Link, Relation::Monitor] {
            for trapping in [false, true] {
                for reason in reasons() {
                    let source = Idle.start();
                    let (send, notices) = mpsc::channel();
                    let observer = Observer(send).start();
                    source.request(Ping).await.unwrap();
                    observer.request(Ping).await.unwrap();
                    observer.context().trap_exit(trapping);
                    let _monitor = match relation {
                        Relation::Link => {
                            observer.context().link(&source.child_handle());
                            None
                        }
                        Relation::Monitor => {
                            Some(observer.context().monitor(&source.child_handle()))
                        }
                    };
                    source.child_handle().stop_with_reason(reason.clone());
                    assert_eq!(joined(source.child_handle()).await, reason);
                    let notices = if matches!(relation, Relation::Link) && !trapping {
                        assert_eq!(joined(observer.child_handle()).await, reason);
                        notices
                    } else {
                        let expected = match relation {
                            Relation::Link => Notice::Exit(reason),
                            Relation::Monitor => Notice::Down(reason),
                        };
                        let (notices, notice) = tokio::task::spawn_blocking(move || {
                            let notice = notices
                                .recv_timeout(LIMIT)
                                .expect("exit notice not delivered");
                            (notices, notice)
                        })
                        .await
                        .unwrap();
                        assert_eq!(notice, expected, "{relation:?}, trap={trapping}");
                        observer.request(Ping).await.unwrap();
                        assert_eq!(observer.child_handle().exit_reason(), None);
                        observer.context().stop();
                        joined(observer.child_handle()).await;
                        notices
                    };
                    assert!(
                        matches!(notices.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
                        "extra notice: {relation:?}, trap={trapping}"
                    );
                }
            }
        }
    }
}
