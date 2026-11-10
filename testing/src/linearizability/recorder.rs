//! Record histories from concurrent clients with a shared logical clock.
use super::Operation;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct Shared<I, O> {
    clock: AtomicU64,
    operations: Mutex<Vec<Operation<I, O>>>,
}

/// A thread-safe history log. Clone it into each client.
///
/// [`Recorder::call`] stamps the call before the request is sent;
/// [`Call::complete`] stamps the return after the reply arrives. A [`Call`]
/// dropped without completing (a timeout, a cancelled future, a panicking
/// client) is recorded as pending, which the checker treats as "may or may not
/// have taken effect".
pub struct Recorder<I, O> {
    shared: Arc<Shared<I, O>>,
}

impl<I, O> Clone for Recorder<I, O> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<I, O> Default for Recorder<I, O> {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared {
                clock: AtomicU64::new(0),
                operations: Mutex::new(Vec::new()),
            }),
        }
    }
}

impl<I, O> Recorder<I, O> {
    /// An empty history.
    pub fn new() -> Self {
        Self::default()
    }

    fn tick(&self) -> u64 {
        self.shared.clock.fetch_add(1, Ordering::SeqCst)
    }

    /// Stamp a call by `client`; send the request after this returns.
    pub fn call(&self, client: usize, input: I) -> Call<I, O> {
        Call {
            recorder: self.clone(),
            client,
            call: self.tick(),
            input: Some(input),
        }
    }

    /// Take the operations recorded so far, ordered by call time.
    pub fn take(&self) -> Vec<Operation<I, O>> {
        let mut operations = std::mem::take(
            &mut *self
                .shared
                .operations
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        operations.sort_by_key(|op| op.call);
        operations
    }

    fn push(&self, operation: Operation<I, O>) {
        self.shared
            .operations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(operation);
    }
}

/// An in-flight request. Complete it with the reply; dropping it records a
/// pending operation.
pub struct Call<I, O> {
    recorder: Recorder<I, O>,
    client: usize,
    call: u64,
    input: Option<I>,
}

impl<I, O> Call<I, O> {
    /// The request.
    pub fn input(&self) -> &I {
        self.input.as_ref().expect("input present until recorded")
    }

    /// Stamp the return and record the reply.
    pub fn complete(mut self, output: O) {
        let ret = self.recorder.tick();
        let input = self.input.take().expect("input present until recorded");
        self.recorder.push(Operation::complete(
            self.client,
            input,
            self.call,
            output,
            ret,
        ));
    }
}

impl<I, O> Drop for Call<I, O> {
    fn drop(&mut self) {
        if let Some(input) = self.input.take() {
            self.recorder
                .push(Operation::pending(self.client, input, self.call));
        }
    }
}
