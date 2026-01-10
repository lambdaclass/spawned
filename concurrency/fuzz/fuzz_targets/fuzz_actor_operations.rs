#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use spawned_concurrency::{
    Backend, RequestResult, MessageResult, Actor, ActorRef,
};
use spawned_rt::tasks as rt;
use std::time::Duration;

/// A simple counter Actor for fuzzing
struct FuzzCounter {
    count: i64,
}

#[derive(Clone)]
enum CounterCall {
    Get,
    Increment,
    Decrement,
    Add(i64),
    Stop,
}

#[derive(Clone)]
enum CounterCast {
    Increment,
    Decrement,
    Add(i64),
}

impl Actor for FuzzCounter {
    type Request = CounterCall;
    type Message = CounterCast;
    type Reply = i64;
    type Error = ();

    async fn handle_request(
        &mut self,
        message: Self::Request,
        _: &ActorRef<Self>,
    ) -> RequestResult<Self> {
        match message {
            CounterCall::Get => RequestResult::Reply(self.count),
            CounterCall::Increment => {
                self.count = self.count.saturating_add(1);
                RequestResult::Reply(self.count)
            }
            CounterCall::Decrement => {
                self.count = self.count.saturating_sub(1);
                RequestResult::Reply(self.count)
            }
            CounterCall::Add(n) => {
                self.count = self.count.saturating_add(n);
                RequestResult::Reply(self.count)
            }
            CounterCall::Stop => RequestResult::Stop(self.count),
        }
    }

    async fn handle_message(
        &mut self,
        message: Self::Message,
        _: &ActorRef<Self>,
    ) -> MessageResult {
        match message {
            CounterCast::Increment => {
                self.count = self.count.saturating_add(1);
            }
            CounterCast::Decrement => {
                self.count = self.count.saturating_sub(1);
            }
            CounterCast::Add(n) => {
                self.count = self.count.saturating_add(n);
            }
        }
        MessageResult::NoReply
    }
}

/// Operations that can be performed on a Actor
#[derive(Arbitrary, Debug, Clone)]
enum Operation {
    CallGet,
    CallIncrement,
    CallDecrement,
    CallAdd(i64),
    CastIncrement,
    CastDecrement,
    CastAdd(i64),
    Sleep(u8), // Sleep for 0-255 microseconds
}

/// Input for the fuzzer
#[derive(Arbitrary, Debug)]
struct FuzzInput {
    initial_count: i64,
    backend: u8, // 0 = Async, 1 = Blocking, 2 = Thread
    operations: Vec<Operation>,
}

fn backend_from_u8(n: u8) -> Backend {
    match n % 3 {
        0 => Backend::Async,
        1 => Backend::Blocking,
        _ => Backend::Thread,
    }
}

fuzz_target!(|input: FuzzInput| {
    // Limit operations to prevent timeouts
    let operations: Vec<_> = input.operations.into_iter().take(100).collect();
    if operations.is_empty() {
        return;
    }

    let backend = backend_from_u8(input.backend);
    let initial_count = input.initial_count;

    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut counter = FuzzCounter { count: initial_count }.start(backend);

        // Track expected state for verification
        let mut expected_count = initial_count;
        let mut cast_adjustments: i64 = 0;

        for op in operations {
            match op {
                Operation::CallGet => {
                    if let Ok(result) = counter.call(CounterCall::Get).await {
                        // After casts process, count should match
                        // We can't assert exact equality due to async cast timing
                        let _ = result;
                    }
                }
                Operation::CallIncrement => {
                    if let Ok(result) = counter.call(CounterCall::Increment).await {
                        expected_count = expected_count.saturating_add(1).saturating_add(cast_adjustments);
                        cast_adjustments = 0;
                        assert_eq!(result, expected_count, "Increment mismatch");
                    }
                }
                Operation::CallDecrement => {
                    if let Ok(result) = counter.call(CounterCall::Decrement).await {
                        expected_count = expected_count.saturating_sub(1).saturating_add(cast_adjustments);
                        cast_adjustments = 0;
                        assert_eq!(result, expected_count, "Decrement mismatch");
                    }
                }
                Operation::CallAdd(n) => {
                    if let Ok(result) = counter.call(CounterCall::Add(n)).await {
                        expected_count = expected_count.saturating_add(n).saturating_add(cast_adjustments);
                        cast_adjustments = 0;
                        assert_eq!(result, expected_count, "Add mismatch");
                    }
                }
                Operation::CastIncrement => {
                    let _ = counter.cast(CounterCast::Increment).await;
                    cast_adjustments = cast_adjustments.saturating_add(1);
                }
                Operation::CastDecrement => {
                    let _ = counter.cast(CounterCast::Decrement).await;
                    cast_adjustments = cast_adjustments.saturating_sub(1);
                }
                Operation::CastAdd(n) => {
                    let _ = counter.cast(CounterCast::Add(n)).await;
                    cast_adjustments = cast_adjustments.saturating_add(n);
                }
                Operation::Sleep(micros) => {
                    rt::sleep(Duration::from_micros(micros as u64)).await;
                }
            }
        }

        // Clean shutdown
        let _ = counter.call(CounterCall::Stop).await;
    });
});
