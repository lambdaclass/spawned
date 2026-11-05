//! Generated lifecycle scenarios. See docs/TESTING.md for replay and guarantees.
use proptest::prelude::*;
use spawned_concurrency::{
    message::Message, ActorError, ChildHandle, Down, ExitReason, MonitorRef,
};
use std::sync::{mpsc, Arc, Barrier, Mutex};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(5);
#[derive(Debug)]
enum Command {
    Work(u64),
    Watch(ChildHandle),
    Boom,
}
#[derive(Debug, PartialEq)]
enum Reply {
    Work(u64),
    Watch(MonitorRef),
}
impl Message for Command {
    type Result = Reply;
}
struct Worker {
    processed: Arc<Mutex<Vec<u64>>>,
    events: mpsc::Sender<Down>,
}

mod tasks {
    use super::*;
    use spawned_concurrency::tasks::{Actor, Context, Handler};
    pub struct TestActor(pub Worker);
    impl Actor for TestActor {}
    impl Handler<Command> for TestActor {
        async fn handle(&mut self, command: Command, ctx: &Context<Self>) -> Reply {
            match command {
                Command::Work(id) => {
                    self.0
                        .processed
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(id);
                    Reply::Work(id)
                }
                Command::Watch(target) => Reply::Watch(ctx.monitor(&target)),
                Command::Boom => panic!("scenario injected panic"),
            }
        }
    }
    impl Handler<Down> for TestActor {
        async fn handle(&mut self, event: Down, _: &Context<Self>) {
            let _ = self.0.events.send(event);
        }
    }
}

mod threads {
    use super::*;
    use spawned_concurrency::threads::{Actor, Context, Handler};
    pub struct TestActor(pub Worker);
    impl Actor for TestActor {}
    impl Handler<Command> for TestActor {
        fn handle(&mut self, command: Command, ctx: &Context<Self>) -> Reply {
            match command {
                Command::Work(id) => {
                    self.0
                        .processed
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(id);
                    Reply::Work(id)
                }
                Command::Watch(target) => Reply::Watch(ctx.monitor(&target)),
                Command::Boom => panic!("scenario injected panic"),
            }
        }
    }
    impl Handler<Down> for TestActor {
        fn handle(&mut self, event: Down, _: &Context<Self>) {
            let _ = self.0.events.send(event);
        }
    }
}

#[derive(Clone)]
enum Handle {
    Tasks(spawned_concurrency::tasks::ActorRef<tasks::TestActor>),
    Threads(spawned_concurrency::threads::ActorRef<threads::TestActor>),
}
impl Handle {
    fn start(worker: Worker, backend: u8) -> Self {
        use spawned_concurrency::tasks::Backend;
        use spawned_concurrency::{tasks::ActorStart as _, threads::ActorStart as _};
        match backend {
            0 => Self::Tasks(tasks::TestActor(worker).start_with_backend(Backend::Async)),
            1 => Self::Tasks(tasks::TestActor(worker).start_with_backend(Backend::Blocking)),
            2 => Self::Tasks(tasks::TestActor(worker).start_with_backend(Backend::Thread)),
            _ => Self::Threads(threads::TestActor(worker).start()),
        }
    }
    fn child(&self) -> ChildHandle {
        match self {
            Self::Tasks(a) => a.child_handle(),
            Self::Threads(a) => a.child_handle(),
        }
    }
    fn send(&self, cmd: Command) -> Result<(), ActorError> {
        match self {
            Self::Tasks(a) => a.send(cmd),
            Self::Threads(a) => a.send(cmd),
        }
    }
    fn request(&self, rt: &tokio::runtime::Runtime, cmd: Command) -> Result<Reply, ActorError> {
        match self {
            Self::Tasks(a) => rt.block_on(a.request_with_timeout(cmd, LIMIT)),
            Self::Threads(a) => a.request_with_timeout(cmd, LIMIT),
        }
    }
    fn abandon(&self, cmd: Command) {
        match self {
            Self::Tasks(a) => drop(a.request_raw(cmd).expect("live actor")),
            Self::Threads(a) => drop(a.request_raw(cmd).expect("live actor")),
        }
    }
}
// Always stop actors, including when a property assertion fails during shrinking.
struct Cleanup(Vec<ChildHandle>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for actor in &self.0 {
            actor.stop();
        }
    }
}
fn wait(rt: &tokio::runtime::Runtime, actor: &ChildHandle) -> ExitReason {
    rt.block_on(async {
        tokio::time::timeout(LIMIT, actor.wait_exit_async())
            .await
            .expect("actor did not terminate")
    })
}

fn scenario(backend: u8, count: usize, operations: &[(u8, u8)], endings: &[u8]) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _entered = rt.enter();
    let (tx, rx) = mpsc::channel();
    let mut cleanup = Cleanup(Vec::new());
    let observer = Handle::start(
        Worker {
            processed: Default::default(),
            events: tx.clone(),
        },
        backend,
    );
    cleanup.0.push(observer.child());
    let mut logs = Vec::new();
    let mut actors = Vec::new();
    for _ in 0..count {
        let log = Arc::new(Mutex::new(Vec::new()));
        let actor = Handle::start(
            Worker {
                processed: log.clone(),
                events: tx.clone(),
            },
            backend,
        );
        cleanup.0.push(actor.child());
        actors.push(actor);
        logs.push(log);
    }
    let mut expected = vec![Vec::new(); count];
    for (id, &(target, kind)) in operations.iter().enumerate() {
        let target = target as usize % count;
        let id = id as u64;
        expected[target].push(id);
        match kind % 3 {
            0 => actors[target].send(Command::Work(id)).unwrap(),
            1 => assert_eq!(
                actors[target].request(&rt, Command::Work(id)).unwrap(),
                Reply::Work(id)
            ),
            _ => actors[target].abandon(Command::Work(id)),
        }
    }
    // A same-sender request is a fence: all preceding work must be processed,
    // including work whose reply receiver was dropped. Dropping a caller is
    // deliberately NOT modeled as cancelling an already enqueued message.
    for (i, actor) in actors.iter().enumerate() {
        let fence = operations.len() as u64 + i as u64;
        expected[i].push(fence);
        assert_eq!(
            actor.request(&rt, Command::Work(fence)).unwrap(),
            Reply::Work(fence)
        );
        assert_eq!(*logs[i].lock().unwrap(), expected[i]);
    }
    let mut expected_down = Vec::new();
    for (i, actor) in actors.iter().enumerate() {
        let child = actor.child();
        let ending = endings[i % endings.len()] % 3;
        // Compete monitor registration and a request with termination. A
        // monitor must observe termination even if registration loses the race.
        let gate = Arc::new(Barrier::new(2));
        let racer = actor.clone();
        let gate2 = gate.clone();
        let stopper = std::thread::spawn(move || {
            gate2.wait();
            if ending == 2 {
                racer.send(Command::Boom).unwrap();
            } else {
                racer.child().stop();
            }
        });
        if ending == 0 {
            gate.wait();
        }
        let Reply::Watch(monitor) = observer
            .request(&rt, Command::Watch(child.clone()))
            .unwrap()
        else {
            panic!("wrong reply")
        };
        if ending != 0 {
            gate.wait();
        }
        let race_id = operations.len() as u64 + count as u64 + i as u64;
        let result = actor.request(&rt, Command::Work(race_id));
        stopper.join().unwrap();
        let reason = wait(&rt, &child);
        if ending == 2 {
            assert!(matches!(reason, ExitReason::Panic(_)));
        } else {
            assert_eq!(reason, ExitReason::Normal);
        }
        match result {
            Ok(reply) => {
                assert_eq!(reply, Reply::Work(race_id));
                assert!(logs[i].lock().unwrap().contains(&race_id));
            }
            Err(ActorError::ActorStopped) => {}
            Err(other) => panic!("request failed to resolve during shutdown: {other:?}"),
        }
        let actual = logs[i].lock().unwrap();
        assert!(
            actual.as_slice() == expected[i]
                || actual.as_slice() == [expected[i].as_slice(), &[race_id]].concat()
        );
        assert!(matches!(
            actor.request(&rt, Command::Work(u64::MAX)),
            Err(ActorError::ActorStopped)
        ));
        expected_down.push((monitor, reason));
    }
    for _ in 0..count {
        let event = rx.recv_timeout(LIMIT).expect("missing Down notification");
        let index = expected_down
            .iter()
            .position(|(id, _)| *id == event.monitor_ref)
            .expect("duplicate or unknown monitor");
        let (_, reason) = expected_down.remove(index);
        assert_eq!(event.reason, reason);
    }
    assert!(expected_down.is_empty());
    observer.child().stop();
    wait(&rt, &observer.child());
    assert!(rx.try_recv().is_err(), "unexpected extra Down");
}

fn scenario_config() -> ProptestConfig {
    let mut config = ProptestConfig {
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::WithSource("proptest-regressions"),
        )),
        ..ProptestConfig::default()
    };
    if std::env::var_os("PROPTEST_CASES").is_none() {
        config.cases = 24;
    }
    config
}

proptest! {
    #![proptest_config(scenario_config())]
    #[test]
    fn generated_actor_scenarios(
        count in 2usize..9,
        operations in prop::collection::vec((any::<u8>(), any::<u8>()), 0..48),
        endings in prop::collection::vec(0u8..3, 1..8),
    ) {
        for backend in 0..4 { scenario(backend, count, &operations, &endings); }
    }
}

#[test]
fn lifecycle_race_regression() {
    for backend in 0..4 {
        scenario(backend, 3, &[(0, 0), (1, 1), (2, 2), (0, 2)], &[0, 1, 2]);
    }
}
