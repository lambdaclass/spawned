//! State machine behavior similar to Erlang's gen_statem.
//!
//! This module provides a trait-based state machine implementation
//! that allows defining state transitions declaratively.
//!
//! # Example
//!
//! ```ignore
//! use spawned_concurrency::tasks::{GenStatem, StateResult, StateName};
//!
//! // Define states
//! #[derive(Clone, Debug)]
//! enum DoorState {
//!     Locked,
//!     Unlocked,
//! }
//!
//! impl StateName for DoorState {}
//!
//! // Define events
//! #[derive(Clone, Debug)]
//! enum DoorEvent {
//!     Lock,
//!     Unlock { code: String },
//! }
//!
//! // Define the state machine
//! struct Door {
//!     secret_code: String,
//! }
//!
//! impl GenStatem for Door {
//!     type State = DoorState;
//!     type Event = DoorEvent;
//!     type Reply = bool;
//!     type Error = ();
//!
//!     fn init(&mut self) -> Self::State {
//!         DoorState::Locked
//!     }
//!
//!     async fn handle_event(
//!         &mut self,
//!         state: &Self::State,
//!         event: Self::Event,
//!     ) -> StateResult<Self> {
//!         match (state, event) {
//!             (DoorState::Locked, DoorEvent::Unlock { code }) if code == self.secret_code => {
//!                 StateResult::transition(DoorState::Unlocked).reply(true)
//!             }
//!             (DoorState::Locked, DoorEvent::Unlock { .. }) => {
//!                 StateResult::keep_state().reply(false)
//!             }
//!             (DoorState::Unlocked, DoorEvent::Lock) => {
//!                 StateResult::transition(DoorState::Locked).reply(true)
//!             }
//!             _ => StateResult::keep_state().reply(false)
//!         }
//!     }
//! }
//! ```

use crate::pid::{ExitReason, HasPid, Pid};
use crate::process_table::{self, SystemMessageSender};
use core::pin::pin;
use futures::future;
use spawned_rt::{
    tasks::{self as rt, mpsc, oneshot, CancellationToken},
    threads,
};
use std::fmt::Debug;
use std::sync::Arc;

/// Backend for running the state machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// Tokio async runtime (default)
    #[default]
    Async,
    /// Tokio blocking thread pool
    Blocking,
    /// Dedicated OS thread
    Thread,
}

/// Trait for state names in the state machine.
///
/// States must be Clone, Debug, Send, and Sync.
pub trait StateName: Clone + Debug + Send + Sync + 'static {}

// Blanket implementation for all qualifying types
impl<T: Clone + Debug + Send + Sync + 'static> StateName for T {}

/// Result of handling an event.
#[derive(Debug)]
pub struct StateResult<G: GenStatem> {
    /// The next state (None means keep current state)
    pub next_state: Option<G::State>,
    /// Reply to send back (if this was a call)
    pub reply: Option<G::Reply>,
    /// Whether to stop the state machine
    pub stop: bool,
    /// Stop reason (if stopping)
    pub stop_reason: Option<ExitReason>,
}

impl<G: GenStatem> StateResult<G> {
    /// Create a result that keeps the current state.
    pub fn keep_state() -> Self {
        Self {
            next_state: None,
            reply: None,
            stop: false,
            stop_reason: None,
        }
    }

    /// Create a result that transitions to a new state.
    pub fn transition(new_state: G::State) -> Self {
        Self {
            next_state: Some(new_state),
            reply: None,
            stop: false,
            stop_reason: None,
        }
    }

    /// Create a result that stops the state machine.
    pub fn stop() -> Self {
        Self {
            next_state: None,
            reply: None,
            stop: true,
            stop_reason: Some(ExitReason::Normal),
        }
    }

    /// Create a result that stops with a specific reason.
    pub fn stop_with_reason(reason: ExitReason) -> Self {
        Self {
            next_state: None,
            reply: None,
            stop: true,
            stop_reason: Some(reason),
        }
    }

    /// Add a reply to the result.
    pub fn reply(mut self, reply: G::Reply) -> Self {
        self.reply = Some(reply);
        self
    }
}

/// Internal message type for the state machine.
pub enum StatemMsg<G: GenStatem> {
    /// Synchronous event (expects reply)
    Call {
        event: G::Event,
        reply_tx: oneshot::Sender<G::Reply>,
    },
    /// Asynchronous event (fire and forget)
    Cast { event: G::Event },
}

/// Handle to a running state machine.
#[derive(Debug)]
pub struct GenStatemHandle<G: GenStatem + 'static> {
    pid: Pid,
    tx: mpsc::Sender<StatemMsg<G>>,
    cancellation_token: CancellationToken,
}

impl<G: GenStatem> Clone for GenStatemHandle<G> {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            tx: self.tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
        }
    }
}

impl<G: GenStatem> HasPid for GenStatemHandle<G> {
    fn pid(&self) -> Pid {
        self.pid
    }
}

/// Internal system message sender for the state machine.
struct StatemSystemSender {
    cancellation_token: CancellationToken,
}

impl SystemMessageSender for StatemSystemSender {
    fn send_down(&self, _pid: Pid, _monitor_ref: crate::link::MonitorRef, _reason: ExitReason) {
        // State machines don't handle monitors by default
    }

    fn send_exit(&self, _pid: Pid, _reason: ExitReason) {
        // Could be extended to handle exits
    }

    fn kill(&self, _reason: ExitReason) {
        self.cancellation_token.cancel();
    }

    fn is_alive(&self) -> bool {
        !self.cancellation_token.is_cancelled()
    }
}

impl<G: GenStatem + 'static> GenStatemHandle<G> {
    /// Send a synchronous event and wait for a reply.
    pub async fn call(&mut self, event: G::Event) -> Result<G::Reply, StatemError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(StatemMsg::Call { event, reply_tx })
            .map_err(|_| StatemError::NotRunning)?;
        reply_rx.await.map_err(|_| StatemError::ChannelClosed)
    }

    /// Send an asynchronous event (fire and forget).
    pub async fn cast(&mut self, event: G::Event) -> Result<(), StatemError> {
        self.tx
            .send(StatemMsg::Cast { event })
            .map_err(|_| StatemError::NotRunning)
    }

    /// Get the current state of the state machine.
    ///
    /// Note: This requires a call to the state machine.
    pub async fn get_state(&mut self) -> Result<G::State, StatemError>
    where
        G::Event: From<GetStateEvent>,
        G::Reply: Into<Option<G::State>>,
    {
        let reply = self.call(GetStateEvent.into()).await?;
        reply.into().ok_or(StatemError::InvalidReply)
    }

    /// Stop the state machine gracefully.
    pub fn stop(&self) {
        self.cancellation_token.cancel();
    }

    /// Check if the state machine is still running.
    pub fn is_alive(&self) -> bool {
        !self.cancellation_token.is_cancelled()
    }

    /// Get the cancellation token.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }
}

/// Marker event for getting current state.
#[derive(Clone, Debug)]
pub struct GetStateEvent;

/// Error type for state machine operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatemError {
    /// The state machine is not running.
    NotRunning,
    /// The communication channel was closed.
    ChannelClosed,
    /// Invalid reply received.
    InvalidReply,
}

impl std::fmt::Display for StatemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatemError::NotRunning => write!(f, "state machine is not running"),
            StatemError::ChannelClosed => write!(f, "communication channel closed"),
            StatemError::InvalidReply => write!(f, "invalid reply received"),
        }
    }
}

impl std::error::Error for StatemError {}

/// Trait for implementing state machines.
///
/// This is the main trait you implement to create a state machine.
pub trait GenStatem: Send + Sized + 'static {
    /// The state type (enum of possible states).
    type State: StateName;

    /// The event type (enum of possible events).
    type Event: Clone + Debug + Send + Sync + 'static;

    /// The reply type for synchronous calls.
    type Reply: Clone + Debug + Send + Sync + 'static;

    /// Error type for the state machine.
    type Error: Debug + Send + 'static;

    /// Initialize the state machine and return the initial state.
    fn init(&mut self) -> Self::State;

    /// Handle an event in the given state.
    ///
    /// Returns a `StateResult` indicating:
    /// - Whether to transition to a new state
    /// - What reply to send (if any)
    /// - Whether to stop the state machine
    fn handle_event(
        &mut self,
        state: &Self::State,
        event: Self::Event,
    ) -> impl std::future::Future<Output = StateResult<Self>> + Send;

    /// Called when entering a new state.
    ///
    /// Override to perform actions on state entry.
    fn on_enter(
        &mut self,
        _old_state: &Self::State,
        _new_state: &Self::State,
    ) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    /// Called when the state machine terminates.
    ///
    /// Override to perform cleanup.
    fn terminate(&mut self, _state: &Self::State, _reason: ExitReason) {}

    /// Start the state machine with the specified backend.
    ///
    /// # Arguments
    ///
    /// * `backend` - The execution backend to use:
    ///   - `Backend::Async` - Run on tokio async runtime (default)
    ///   - `Backend::Blocking` - Run on tokio's blocking thread pool
    ///   - `Backend::Thread` - Run on a dedicated OS thread
    ///
    /// # Example
    ///
    /// ```ignore
    /// use spawned_concurrency::tasks::{GenStatem, Backend};
    ///
    /// // Use default (async) backend
    /// let handle = my_statem.start(Backend::default());
    ///
    /// // Use blocking backend for CPU-intensive work
    /// let handle = my_statem.start(Backend::Blocking);
    ///
    /// // Use dedicated thread for singleton state machines
    /// let handle = my_statem.start(Backend::Thread);
    /// ```
    fn start(self, backend: Backend) -> GenStatemHandle<Self> {
        let (handle, inner_future) = Self::prepare_start(self);
        match backend {
            Backend::Async => {
                let _join_handle = rt::spawn(inner_future);
            }
            Backend::Blocking => {
                let _join_handle = rt::spawn_blocking(move || rt::block_on(inner_future));
            }
            Backend::Thread => {
                let _join_handle = threads::spawn(move || threads::block_on(inner_future));
            }
        }
        handle
    }

    /// Internal helper to prepare a state machine for starting.
    #[doc(hidden)]
    fn prepare_start(
        statem: Self,
    ) -> (
        GenStatemHandle<Self>,
        impl std::future::Future<Output = ()> + Send,
    ) {
        let pid = Pid::new();
        let (tx, mut rx) = mpsc::channel::<StatemMsg<Self>>();
        let cancellation_token = CancellationToken::new();

        // Register with process table
        let system_sender = Arc::new(StatemSystemSender {
            cancellation_token: cancellation_token.clone(),
        });
        process_table::register(pid, system_sender);

        let handle = GenStatemHandle {
            pid,
            tx,
            cancellation_token: cancellation_token.clone(),
        };

        let inner_future = async move {
            let mut statem = statem;
            let mut current_state = statem.init();
            #[allow(unused_assignments)]
            let mut exit_reason = ExitReason::Normal;

            loop {
                let cancelled = pin!(cancellation_token.cancelled());
                let recv = pin!(rx.recv());

                match future::select(cancelled, recv).await {
                    future::Either::Left(_) => {
                        // Cancelled
                        exit_reason = ExitReason::Shutdown;
                        statem.terminate(&current_state, exit_reason.clone());
                        break;
                    }
                    future::Either::Right((msg, _)) => {
                        match msg {
                            Some(StatemMsg::Call { event, reply_tx }) => {
                                let result = statem.handle_event(&current_state, event).await;

                                // Send reply if present
                                if let Some(reply) = result.reply {
                                    let _ = reply_tx.send(reply);
                                }

                                // Handle state transition
                                if let Some(new_state) = result.next_state {
                                    statem.on_enter(&current_state, &new_state).await;
                                    current_state = new_state;
                                }

                                // Check for stop
                                if result.stop {
                                    exit_reason =
                                        result.stop_reason.unwrap_or(ExitReason::Normal);
                                    statem.terminate(&current_state, exit_reason.clone());
                                    break;
                                }
                            }
                            Some(StatemMsg::Cast { event }) => {
                                let result = statem.handle_event(&current_state, event).await;

                                // Handle state transition
                                if let Some(new_state) = result.next_state {
                                    statem.on_enter(&current_state, &new_state).await;
                                    current_state = new_state;
                                }

                                // Check for stop
                                if result.stop {
                                    exit_reason =
                                        result.stop_reason.unwrap_or(ExitReason::Normal);
                                    statem.terminate(&current_state, exit_reason.clone());
                                    break;
                                }
                            }
                            None => {
                                // Channel closed, terminate
                                exit_reason = ExitReason::Normal;
                                statem.terminate(&current_state, exit_reason.clone());
                                break;
                            }
                        }
                    }
                }
            }

            // Unregister from process table
            process_table::unregister(pid, exit_reason);
        };

        (handle, inner_future)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc as StdArc;

    // Helper to start with default backend
    fn default_backend() -> Backend {
        Backend::default()
    }

    // Simple traffic light state machine for testing
    #[derive(Clone, Debug, PartialEq)]
    enum LightState {
        Red,
        Yellow,
        Green,
    }

    #[derive(Clone, Debug)]
    enum LightEvent {
        Next,
        Reset,
        GetState,
    }

    #[derive(Clone, Debug)]
    enum LightReply {
        Ok,
        State(LightState),
    }

    struct TrafficLight {
        transitions: StdArc<AtomicU32>,
    }

    impl GenStatem for TrafficLight {
        type State = LightState;
        type Event = LightEvent;
        type Reply = LightReply;
        type Error = ();

        fn init(&mut self) -> Self::State {
            LightState::Red
        }

        async fn handle_event(
            &mut self,
            state: &Self::State,
            event: Self::Event,
        ) -> StateResult<Self> {
            match event {
                LightEvent::GetState => StateResult::keep_state().reply(LightReply::State(state.clone())),
                LightEvent::Reset => StateResult::transition(LightState::Red).reply(LightReply::Ok),
                LightEvent::Next => {
                    self.transitions.fetch_add(1, Ordering::SeqCst);
                    match state {
                        LightState::Red => {
                            StateResult::transition(LightState::Green).reply(LightReply::Ok)
                        }
                        LightState::Green => {
                            StateResult::transition(LightState::Yellow).reply(LightReply::Ok)
                        }
                        LightState::Yellow => {
                            StateResult::transition(LightState::Red).reply(LightReply::Ok)
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn test_basic_state_machine() {
        let transitions = StdArc::new(AtomicU32::new(0));
        let light = TrafficLight {
            transitions: transitions.clone(),
        };

        let mut handle = light.start(default_backend());

        // Should start in Red
        let reply = handle.call(LightEvent::GetState).await.unwrap();
        assert!(matches!(reply, LightReply::State(LightState::Red)));

        // Transition to Green
        let reply = handle.call(LightEvent::Next).await.unwrap();
        assert!(matches!(reply, LightReply::Ok));

        let reply = handle.call(LightEvent::GetState).await.unwrap();
        assert!(matches!(reply, LightReply::State(LightState::Green)));

        // Transition to Yellow
        let reply = handle.call(LightEvent::Next).await.unwrap();
        assert!(matches!(reply, LightReply::Ok));

        let reply = handle.call(LightEvent::GetState).await.unwrap();
        assert!(matches!(reply, LightReply::State(LightState::Yellow)));

        // Transition back to Red
        let reply = handle.call(LightEvent::Next).await.unwrap();
        assert!(matches!(reply, LightReply::Ok));

        let reply = handle.call(LightEvent::GetState).await.unwrap();
        assert!(matches!(reply, LightReply::State(LightState::Red)));

        // Check transition count
        assert_eq!(transitions.load(Ordering::SeqCst), 3);

        handle.stop();
    }

    #[tokio::test]
    async fn test_cast_events() {
        let transitions = StdArc::new(AtomicU32::new(0));
        let light = TrafficLight {
            transitions: transitions.clone(),
        };

        let mut handle = light.start(default_backend());

        // Use cast (fire and forget)
        handle.cast(LightEvent::Next).await.unwrap();
        handle.cast(LightEvent::Next).await.unwrap();

        // Give time for events to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Should be Yellow now (Red -> Green -> Yellow)
        let reply = handle.call(LightEvent::GetState).await.unwrap();
        assert!(matches!(reply, LightReply::State(LightState::Yellow)));

        handle.stop();
    }

    #[tokio::test]
    async fn test_reset() {
        let transitions = StdArc::new(AtomicU32::new(0));
        let light = TrafficLight {
            transitions: transitions.clone(),
        };

        let mut handle = light.start(default_backend());

        // Go to Green
        handle.call(LightEvent::Next).await.unwrap();

        // Reset to Red
        handle.call(LightEvent::Reset).await.unwrap();

        let reply = handle.call(LightEvent::GetState).await.unwrap();
        assert!(matches!(reply, LightReply::State(LightState::Red)));

        handle.stop();
    }

    #[tokio::test]
    async fn test_has_pid() {
        let light = TrafficLight {
            transitions: StdArc::new(AtomicU32::new(0)),
        };

        let handle = light.start(default_backend());
        let pid = handle.pid();

        // Should have a valid PID
        assert!(pid.id() > 0);

        handle.stop();
    }

    #[tokio::test]
    async fn test_is_alive() {
        let light = TrafficLight {
            transitions: StdArc::new(AtomicU32::new(0)),
        };

        let handle = light.start(default_backend());
        assert!(handle.is_alive());

        handle.stop();

        // Give time for shutdown
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(!handle.is_alive());
    }

    // Door lock example from documentation
    #[derive(Clone, Debug, PartialEq)]
    enum DoorState {
        Locked,
        Unlocked,
    }

    #[derive(Clone, Debug)]
    enum DoorEvent {
        Lock,
        Unlock { code: String },
        GetState,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum DoorReply {
        Success,
        WrongCode,
        State(DoorState),
    }

    struct Door {
        secret_code: String,
    }

    impl GenStatem for Door {
        type State = DoorState;
        type Event = DoorEvent;
        type Reply = DoorReply;
        type Error = ();

        fn init(&mut self) -> Self::State {
            DoorState::Locked
        }

        async fn handle_event(
            &mut self,
            state: &Self::State,
            event: Self::Event,
        ) -> StateResult<Self> {
            match (&state, event) {
                (_, DoorEvent::GetState) => {
                    StateResult::keep_state().reply(DoorReply::State(state.clone()))
                }
                (DoorState::Locked, DoorEvent::Unlock { code }) if code == self.secret_code => {
                    StateResult::transition(DoorState::Unlocked).reply(DoorReply::Success)
                }
                (DoorState::Locked, DoorEvent::Unlock { .. }) => {
                    StateResult::keep_state().reply(DoorReply::WrongCode)
                }
                (DoorState::Unlocked, DoorEvent::Lock) => {
                    StateResult::transition(DoorState::Locked).reply(DoorReply::Success)
                }
                _ => StateResult::keep_state().reply(DoorReply::WrongCode),
            }
        }
    }

    #[tokio::test]
    async fn test_door_example() {
        let door = Door {
            secret_code: "1234".to_string(),
        };

        let mut handle = door.start(default_backend());

        // Initially locked
        let reply = handle.call(DoorEvent::GetState).await.unwrap();
        assert_eq!(reply, DoorReply::State(DoorState::Locked));

        // Wrong code
        let reply = handle
            .call(DoorEvent::Unlock {
                code: "wrong".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(reply, DoorReply::WrongCode);

        // Still locked
        let reply = handle.call(DoorEvent::GetState).await.unwrap();
        assert_eq!(reply, DoorReply::State(DoorState::Locked));

        // Correct code
        let reply = handle
            .call(DoorEvent::Unlock {
                code: "1234".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(reply, DoorReply::Success);

        // Now unlocked
        let reply = handle.call(DoorEvent::GetState).await.unwrap();
        assert_eq!(reply, DoorReply::State(DoorState::Unlocked));

        // Lock again
        let reply = handle.call(DoorEvent::Lock).await.unwrap();
        assert_eq!(reply, DoorReply::Success);

        let reply = handle.call(DoorEvent::GetState).await.unwrap();
        assert_eq!(reply, DoorReply::State(DoorState::Locked));

        handle.stop();
    }

    // Test on_enter callback
    #[derive(Clone, Debug, PartialEq)]
    enum CountState {
        Low,
        Medium,
        High,
    }

    #[derive(Clone, Debug)]
    #[allow(dead_code)]
    enum CountEvent {
        Increment,
        Decrement,
        GetCount,
        GetEnterCount,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum CountReply {
        Value(i32),
        EnterCount(u32),
        Ok,
    }

    struct Counter {
        value: i32,
        enter_count: StdArc<AtomicU32>,
    }

    impl GenStatem for Counter {
        type State = CountState;
        type Event = CountEvent;
        type Reply = CountReply;
        type Error = ();

        fn init(&mut self) -> Self::State {
            CountState::Low
        }

        async fn handle_event(
            &mut self,
            state: &Self::State,
            event: Self::Event,
        ) -> StateResult<Self> {
            match event {
                CountEvent::GetCount => StateResult::keep_state().reply(CountReply::Value(self.value)),
                CountEvent::GetEnterCount => {
                    StateResult::keep_state().reply(CountReply::EnterCount(
                        self.enter_count.load(Ordering::SeqCst),
                    ))
                }
                CountEvent::Increment => {
                    self.value += 1;
                    let new_state = Self::value_to_state(self.value);
                    if &new_state != state {
                        StateResult::transition(new_state).reply(CountReply::Ok)
                    } else {
                        StateResult::keep_state().reply(CountReply::Ok)
                    }
                }
                CountEvent::Decrement => {
                    self.value -= 1;
                    let new_state = Self::value_to_state(self.value);
                    if &new_state != state {
                        StateResult::transition(new_state).reply(CountReply::Ok)
                    } else {
                        StateResult::keep_state().reply(CountReply::Ok)
                    }
                }
            }
        }

        async fn on_enter(&mut self, _old_state: &Self::State, _new_state: &Self::State) {
            self.enter_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Counter {
        fn value_to_state(value: i32) -> CountState {
            if value < 5 {
                CountState::Low
            } else if value < 10 {
                CountState::Medium
            } else {
                CountState::High
            }
        }
    }

    #[tokio::test]
    async fn test_on_enter_callback() {
        let enter_count = StdArc::new(AtomicU32::new(0));
        let counter = Counter {
            value: 0,
            enter_count: enter_count.clone(),
        };

        let mut handle = counter.start(default_backend());

        // Initial state is Low, no on_enter yet
        assert_eq!(enter_count.load(Ordering::SeqCst), 0);

        // Increment to 5 -> transition to Medium
        for _ in 0..5 {
            handle.call(CountEvent::Increment).await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(enter_count.load(Ordering::SeqCst), 1);

        // Increment to 10 -> transition to High
        for _ in 0..5 {
            handle.call(CountEvent::Increment).await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(enter_count.load(Ordering::SeqCst), 2);

        // Decrement to 9 -> transition back to Medium
        handle.call(CountEvent::Decrement).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(enter_count.load(Ordering::SeqCst), 3);

        handle.stop();
    }

    // Test terminate callback
    struct TerminatingStatem {
        terminated: StdArc<std::sync::Mutex<Option<ExitReason>>>,
    }

    #[derive(Clone, Debug)]
    enum TermState {
        Running,
    }

    #[derive(Clone, Debug)]
    enum TermEvent {
        StopNormal,
        StopWithReason,
    }

    #[derive(Clone, Debug)]
    enum TermReply {
        Ok,
    }

    impl GenStatem for TerminatingStatem {
        type State = TermState;
        type Event = TermEvent;
        type Reply = TermReply;
        type Error = ();

        fn init(&mut self) -> Self::State {
            TermState::Running
        }

        async fn handle_event(
            &mut self,
            _state: &Self::State,
            event: Self::Event,
        ) -> StateResult<Self> {
            match event {
                TermEvent::StopNormal => StateResult::stop().reply(TermReply::Ok),
                TermEvent::StopWithReason => StateResult::stop_with_reason(ExitReason::Error(
                    "custom error".to_string(),
                ))
                .reply(TermReply::Ok),
            }
        }

        fn terminate(&mut self, _state: &Self::State, reason: ExitReason) {
            let mut guard = self.terminated.lock().unwrap();
            *guard = Some(reason);
        }
    }

    #[tokio::test]
    async fn test_stop_normal() {
        let terminated = StdArc::new(std::sync::Mutex::new(None));
        let statem = TerminatingStatem {
            terminated: terminated.clone(),
        };

        let mut handle = statem.start(default_backend());

        // Stop normally
        handle.call(TermEvent::StopNormal).await.unwrap();

        // Give time for shutdown
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Check terminate was called with Normal reason
        let reason = terminated.lock().unwrap().clone();
        assert!(matches!(reason, Some(ExitReason::Normal)));
    }

    #[tokio::test]
    async fn test_stop_with_reason() {
        let terminated = StdArc::new(std::sync::Mutex::new(None));
        let statem = TerminatingStatem {
            terminated: terminated.clone(),
        };

        let mut handle = statem.start(default_backend());

        // Stop with custom reason
        handle.call(TermEvent::StopWithReason).await.unwrap();

        // Give time for shutdown
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Check terminate was called with Error reason
        let reason = terminated.lock().unwrap().clone();
        assert!(matches!(reason, Some(ExitReason::Error(_))));
    }

    #[tokio::test]
    async fn test_external_stop() {
        let terminated = StdArc::new(std::sync::Mutex::new(None));
        let statem = TerminatingStatem {
            terminated: terminated.clone(),
        };

        let handle = statem.start(default_backend());

        // Stop from outside
        handle.stop();

        // Give time for shutdown
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Check terminate was called with Shutdown reason
        let reason = terminated.lock().unwrap().clone();
        assert!(matches!(reason, Some(ExitReason::Shutdown)));
    }

    #[tokio::test]
    async fn test_multiple_state_machines() {
        let light1 = TrafficLight {
            transitions: StdArc::new(AtomicU32::new(0)),
        };
        let light2 = TrafficLight {
            transitions: StdArc::new(AtomicU32::new(0)),
        };

        let mut handle1 = light1.start(default_backend());
        let mut handle2 = light2.start(default_backend());

        // They should have different PIDs
        assert_ne!(handle1.pid(), handle2.pid());

        // Advance light1 to Green
        handle1.call(LightEvent::Next).await.unwrap();

        // Light1 should be Green, Light2 should be Red
        let reply1 = handle1.call(LightEvent::GetState).await.unwrap();
        let reply2 = handle2.call(LightEvent::GetState).await.unwrap();

        assert!(matches!(reply1, LightReply::State(LightState::Green)));
        assert!(matches!(reply2, LightReply::State(LightState::Red)));

        handle1.stop();
        handle2.stop();
    }

    #[tokio::test]
    async fn test_clone_handle() {
        let light = TrafficLight {
            transitions: StdArc::new(AtomicU32::new(0)),
        };

        let mut handle1 = light.start(default_backend());
        let mut handle2 = handle1.clone();

        // Both handles should have same PID
        assert_eq!(handle1.pid(), handle2.pid());

        // Both should be able to communicate
        handle1.call(LightEvent::Next).await.unwrap();
        let reply = handle2.call(LightEvent::GetState).await.unwrap();
        assert!(matches!(reply, LightReply::State(LightState::Green)));

        handle1.stop();
    }

    #[tokio::test]
    async fn test_error_not_running() {
        let light = TrafficLight {
            transitions: StdArc::new(AtomicU32::new(0)),
        };

        let mut handle = light.start(default_backend());
        handle.stop();

        // Give time for shutdown
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Should get error when trying to call stopped state machine
        let result = handle.call(LightEvent::GetState).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_state_result_builders() {
        // Test keep_state
        let result: StateResult<TrafficLight> = StateResult::keep_state();
        assert!(result.next_state.is_none());
        assert!(result.reply.is_none());
        assert!(!result.stop);

        // Test transition
        let result: StateResult<TrafficLight> = StateResult::transition(LightState::Green);
        assert!(matches!(result.next_state, Some(LightState::Green)));
        assert!(!result.stop);

        // Test stop
        let result: StateResult<TrafficLight> = StateResult::stop();
        assert!(result.stop);
        assert!(matches!(result.stop_reason, Some(ExitReason::Normal)));

        // Test stop_with_reason
        let result: StateResult<TrafficLight> =
            StateResult::stop_with_reason(ExitReason::Shutdown);
        assert!(result.stop);
        assert!(matches!(result.stop_reason, Some(ExitReason::Shutdown)));

        // Test reply chaining
        let result: StateResult<TrafficLight> =
            StateResult::transition(LightState::Yellow).reply(LightReply::Ok);
        assert!(matches!(result.next_state, Some(LightState::Yellow)));
        assert!(matches!(result.reply, Some(LightReply::Ok)));
    }

    #[tokio::test]
    async fn test_statem_error_display() {
        assert_eq!(
            format!("{}", StatemError::NotRunning),
            "state machine is not running"
        );
        assert_eq!(
            format!("{}", StatemError::ChannelClosed),
            "communication channel closed"
        );
        assert_eq!(
            format!("{}", StatemError::InvalidReply),
            "invalid reply received"
        );
    }

    // Test rapid state transitions
    #[tokio::test]
    async fn test_rapid_transitions() {
        let transitions = StdArc::new(AtomicU32::new(0));
        let light = TrafficLight {
            transitions: transitions.clone(),
        };

        let mut handle = light.start(default_backend());

        // Rapidly cycle through states
        for _ in 0..100 {
            handle.call(LightEvent::Next).await.unwrap();
        }

        // 100 transitions
        assert_eq!(transitions.load(Ordering::SeqCst), 100);

        // 100 transitions = 33 complete cycles + 1 extra
        // Red -> Green -> Yellow -> Red is one cycle (3 transitions)
        // After 100 transitions: 100 % 3 = 1, so we're at Green
        let reply = handle.call(LightEvent::GetState).await.unwrap();
        assert!(matches!(reply, LightReply::State(LightState::Green)));

        handle.stop();
    }
}
