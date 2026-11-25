//! `#[actor]` and `#[protocol]` expand with absolute paths: a module that
//! imports nothing from spawned compiles every handler kind, lifecycle hook
//! and protocol method.
#![cfg(feature = "async")]

mod tasks_mode {
    #[spawned_concurrency::protocol]
    pub trait CounterProtocol: Send + Sync {
        fn add(&self, n: u64) -> spawned_concurrency::Response<u64>;
        fn bump(&self) -> Result<(), spawned_concurrency::ActorError>;
    }

    #[derive(Default)]
    pub struct Counter(u64);

    #[spawned_concurrency::actor(protocol = CounterProtocol)]
    impl Counter {
        #[started]
        async fn started(&mut self, _ctx: &spawned_concurrency::tasks::Context<Self>) {}

        #[request_handler]
        async fn handle_add(
            &mut self,
            msg: counter_protocol::Add,
            _ctx: &spawned_concurrency::tasks::Context<Self>,
        ) -> u64 {
            self.0 += msg.n;
            self.0
        }

        #[send_handler]
        async fn handle_bump(
            &mut self,
            _msg: counter_protocol::Bump,
            _ctx: &spawned_concurrency::tasks::Context<Self>,
        ) {
            self.0 += 1;
        }

        #[stopped]
        async fn stopped(&mut self, _ctx: &spawned_concurrency::tasks::Context<Self>) {}
    }
}

mod threads_mode {
    #[spawned_concurrency::protocol]
    pub trait CounterProtocol: Send + Sync {
        fn add(&self, n: u64) -> spawned_concurrency::Response<u64>;
    }

    #[derive(Default)]
    pub struct Counter(u64);

    #[spawned_concurrency::actor(protocol = CounterProtocol)]
    impl Counter {
        #[request_handler]
        fn handle_add(
            &mut self,
            msg: counter_protocol::Add,
            _ctx: &spawned_concurrency::threads::Context<Self>,
        ) -> u64 {
            self.0 += msg.n;
            self.0
        }
    }
}

#[spawned_concurrency::sim_test]
async fn expands_without_imports() {
    use tasks_mode::CounterProtocol as _;
    let counter = spawned_concurrency::tasks::ActorStart::start(tasks_mode::Counter::default())
        .await
        .unwrap();
    counter.bump().unwrap();
    assert_eq!(counter.add(2).await.unwrap(), 3);
}

#[cfg(not(spawned_sim))]
#[test]
fn threads_expands_without_imports() {
    use threads_mode::CounterProtocol as _;
    let counter =
        spawned_concurrency::threads::ActorStart::start(threads_mode::Counter::default()).unwrap();
    assert_eq!(counter.add(2).unwrap(), 2);
}
