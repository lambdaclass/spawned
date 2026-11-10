//! A small key-value and per-key FIFO store actor in both modes, with
//! switchable bugs for negative controls, and its sequential model.
#![allow(dead_code)]
use spawned_concurrency::message::Message;
use spawned_test::linearizability::{partition_by_key, Model, Operation};
use std::collections::{HashMap, VecDeque};

/// Register keys take `Put`/`Get`; queue keys take `Push`/`Pop`. The two key
/// spaces are separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StoreOp {
    /// Replace the register's value; replies with the previous value.
    Put { key: u8, value: u64 },
    /// Read the register.
    Get { key: u8 },
    /// Append to the queue; replies `None`.
    Push { key: u8, value: u64 },
    /// Remove the queue's front.
    Pop { key: u8 },
}

impl StoreOp {
    pub fn key(&self) -> (bool, u8) {
        match *self {
            StoreOp::Put { key, .. } | StoreOp::Get { key } => (false, key),
            StoreOp::Push { key, .. } | StoreOp::Pop { key } => (true, key),
        }
    }
}

impl Message for StoreOp {
    type Result = Option<u64>;
}

/// Deliberate defects, each triggered deterministically by an operation
/// counter so negative controls are reproducible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    None,
    /// Every 5th put is acknowledged but not stored.
    LostWrite,
    /// Every 3rd get returns the value from before the latest put.
    StaleRead,
    /// Every 4th push jumps the queue.
    Reorder,
}

pub struct Store {
    bug: Bug,
    registers: HashMap<u8, u64>,
    previous: HashMap<u8, Option<u64>>,
    queues: HashMap<u8, VecDeque<u64>>,
    puts: u64,
    gets: u64,
    pushes: u64,
}

impl Store {
    pub fn new(bug: Bug) -> Self {
        Self {
            bug,
            registers: HashMap::new(),
            previous: HashMap::new(),
            queues: HashMap::new(),
            puts: 0,
            gets: 0,
            pushes: 0,
        }
    }

    fn apply(&mut self, op: StoreOp) -> Option<u64> {
        match op {
            StoreOp::Put { key, value } => {
                self.puts += 1;
                let before = self.registers.get(&key).copied();
                if self.bug == Bug::LostWrite && self.puts.is_multiple_of(5) {
                    return before;
                }
                self.previous.insert(key, before);
                self.registers.insert(key, value);
                before
            }
            StoreOp::Get { key } => {
                self.gets += 1;
                match self.previous.get(&key) {
                    Some(stale) if self.bug == Bug::StaleRead && self.gets.is_multiple_of(3) => {
                        *stale
                    }
                    _ => self.registers.get(&key).copied(),
                }
            }
            StoreOp::Push { key, value } => {
                self.pushes += 1;
                let queue = self.queues.entry(key).or_default();
                if self.bug == Bug::Reorder && self.pushes.is_multiple_of(4) {
                    queue.push_front(value);
                } else {
                    queue.push_back(value);
                }
                None
            }
            StoreOp::Pop { key } => self.queues.entry(key).or_default().pop_front(),
        }
    }
}

#[cfg(feature = "async")]
pub struct AsyncStore(pub Store);

#[cfg(feature = "async")]
impl spawned_concurrency::tasks::Actor for AsyncStore {}

#[cfg(feature = "async")]
impl spawned_concurrency::tasks::Handler<StoreOp> for AsyncStore {
    async fn handle(
        &mut self,
        op: StoreOp,
        reply: spawned_concurrency::reply::PendingReply<'_, Option<u64>>,
        _: &spawned_concurrency::tasks::Context<Self>,
    ) {
        let __reply_value: Option<u64> = {
            // Yield inside the turn so concurrent requests really queue up.
            if op.key().1.is_multiple_of(2) {
                tokio::task::yield_now().await;
            }
            self.0.apply(op)
        };
        let _ = reply.reply(__reply_value);
    }
}

pub struct NativeStore(pub Store);

impl spawned_concurrency::threads::Actor for NativeStore {}

impl spawned_concurrency::threads::Handler<StoreOp> for NativeStore {
    fn handle(
        &mut self,
        op: StoreOp,
        reply: spawned_concurrency::reply::PendingReply<'_, Option<u64>>,
        _: &spawned_concurrency::threads::Context<Self>,
    ) {
        let __reply_value: Option<u64> = { self.0.apply(op) };
        let _ = reply.reply(__reply_value);
    }
}

/// The sequential specification: one register or one FIFO queue per
/// partition, partitioned by key (P-compositionality).
pub struct StoreModel;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct KeyState {
    register: Option<u64>,
    queue: VecDeque<u64>,
}

impl Model for StoreModel {
    type State = KeyState;
    type Input = StoreOp;
    type Output = Option<u64>;

    fn init(&self) -> KeyState {
        KeyState::default()
    }

    fn step(
        &self,
        state: &KeyState,
        input: &StoreOp,
        output: Option<&Option<u64>>,
    ) -> Option<KeyState> {
        let mut next = state.clone();
        let produced = match *input {
            StoreOp::Put { value, .. } => next.register.replace(value),
            StoreOp::Get { .. } => next.register,
            StoreOp::Push { value, .. } => {
                next.queue.push_back(value);
                None
            }
            StoreOp::Pop { .. } => next.queue.pop_front(),
        };
        output.is_none_or(|seen| *seen == produced).then_some(next)
    }

    fn partition(&self, history: &[Operation<StoreOp, Option<u64>>]) -> Vec<Vec<usize>> {
        partition_by_key(history, StoreOp::key)
    }
}
