# Spawned concurrency

Actor framework for Rust, inspired by Erlang/OTP. Define protocols as traits, implement handlers on actors, and call methods directly on actor references.

This crate is part of [spawned](https://github.com/lambdaclass/spawned). See the [workspace README](https://github.com/lambdaclass/spawned/blob/main/README.md) for a full overview and the [Guide](https://github.com/lambdaclass/spawned/blob/main/docs/GUIDE.md) for detailed documentation.

## Quick Example

```rust
use spawned_concurrency::tasks::{Actor, ActorStart as _, Context, Handler};
use spawned_concurrency::{actor, protocol, Response};
use spawned_rt::tasks as rt;

#[protocol]
pub trait CounterProtocol: Send + Sync {
    fn increment(&self) -> Response<()>;
    fn get(&self) -> Response<u64>;
}

use counter_protocol::{Get, Increment};

pub struct MyCounter {
    count: u64,
}

#[actor(protocol = CounterProtocol)]
impl MyCounter {
    #[request_handler]
    async fn handle_increment(&mut self, _msg: Increment, _ctx: &Context<Self>) {
        self.count += 1;
    }

    #[request_handler]
    async fn handle_get(&mut self, _msg: Get, _ctx: &Context<Self>) -> u64 {
        self.count
    }
}

fn main() {
    rt::run(async {
        let counter = MyCounter { count: 0 }.start();
        counter.increment().await.unwrap();
        let value = counter.get().await.unwrap();
        assert_eq!(value, 1);
    })
}
```

This example is compiled and run as a doctest, so it cannot drift from the API.

## Two Execution Modes

- **tasks**: async/await with tokio. Use `spawned_concurrency::tasks`.
- **threads**: blocking, no async runtime. Use `spawned_concurrency::threads`.

Both provide the same `Actor`, `Handler<M>`, `ActorRef<A>`, and `Context<A>` types.
