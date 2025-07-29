//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use futures::future::FutureExt as _;
use spawned_rt::tasks::{self as rt, mpsc, oneshot, timeout, CancellationToken};
use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe, time::Duration};

use crate::error::GenServerError;

const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct GenServerHandle<G: GenServer + 'static> {
    pub tx: mpsc::Sender<GenServerInMsg<G>>,
    /// Cancellation token to stop the GenServer
    cancellation_token: CancellationToken,
}

impl<G: GenServer> Clone for GenServerHandle<G> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
        }
    }
}

impl<G: GenServer> GenServerHandle<G> {
    pub(crate) fn new(gen_server: G) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let cancellation_token = CancellationToken::new();
        let handle = GenServerHandle {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();

        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn(async move {
            if gen_server.run(&handle, &mut rx, None).await.is_err() {
                tracing::trace!("GenServer crashed")
            };
        });

        handle_clone
    }

    pub(crate) fn verified_new(gen_server: G) -> Result<Self, GenServerError> {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let cancellation_token = CancellationToken::new();
        let handle = GenServerHandle {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();

        // We create a channel of single use to signal when the GenServer has started.
        let (mut start_signal_tx, start_signal_rx) = std::sync::mpsc::channel();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let join_handle = rt::spawn(async move {
            if gen_server
                .run(&handle, &mut rx, Some(&mut start_signal_tx))
                .await
                .is_err()
            {
                tracing::trace!("GenServer crashed")
            };
        });

        // Wait for the GenServer to signal us that it has started
        match start_signal_rx.recv() {
            Ok(true) => Ok(handle_clone),
            _ => {
                join_handle.abort(); // Abort the task even tho we know it won't run anymore
                Err(GenServerError::Initialization)
            }
        }
    }

    pub(crate) fn new_blocking(gen_server: G) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let cancellation_token = CancellationToken::new();
        let handle = GenServerHandle {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();

        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn_blocking(|| {
            rt::block_on(async move {
                if gen_server.run(&handle, &mut rx, None).await.is_err() {
                    tracing::trace!("GenServer crashed")
                };
            })
        });

        handle_clone
    }

    pub(crate) fn verified_new_blocking(gen_server: G) -> Result<Self, GenServerError> {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let cancellation_token = CancellationToken::new();
        let handle = GenServerHandle {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();

        // We create a channel of single use to signal when the GenServer has started.
        // This channel is used in the verified method, here it's just to keep the API consistent.
        // The handle is thereby returned immediately, without waiting for the GenServer to start.
        let (mut start_signal_tx, start_signal_rx) = std::sync::mpsc::channel();
        let join_handle = rt::spawn_blocking(|| {
            rt::block_on(async move {
                if gen_server
                    .run(&handle, &mut rx, Some(&mut start_signal_tx))
                    .await
                    .is_err()
                {
                    tracing::trace!("GenServer crashed")
                };
            })
        });

        // Wait for the GenServer to signal us that it has started
        match start_signal_rx.recv() {
            Ok(true) => Ok(handle_clone),
            _ => {
                join_handle.abort(); // Abort the task even tho we know it won't run anymore
                Err(GenServerError::Initialization)
            }
        }
    }

    pub fn sender(&self) -> mpsc::Sender<GenServerInMsg<G>> {
        self.tx.clone()
    }

    pub async fn call(&mut self, message: G::CallMsg) -> Result<G::OutMsg, GenServerError> {
        self.call_with_timeout(message, DEFAULT_CALL_TIMEOUT).await
    }

    pub async fn call_with_timeout(
        &mut self,
        message: G::CallMsg,
        duration: Duration,
    ) -> Result<G::OutMsg, GenServerError> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<G::OutMsg, GenServerError>>();
        self.tx.send(GenServerInMsg::Call {
            sender: oneshot_tx,
            message,
        })?;

        match timeout(duration, oneshot_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(GenServerError::Server),
            Err(_) => Err(GenServerError::CallTimeout),
        }
    }

    pub async fn cast(&mut self, message: G::CastMsg) -> Result<(), GenServerError> {
        self.tx
            .send(GenServerInMsg::Cast { message })
            .map_err(|_error| GenServerError::Server)
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }
}

pub enum GenServerInMsg<G: GenServer> {
    Call {
        sender: oneshot::Sender<Result<G::OutMsg, GenServerError>>,
        message: G::CallMsg,
    },
    Cast {
        message: G::CastMsg,
    },
}

pub enum CallResponse<G: GenServer> {
    Reply(G, G::OutMsg),
    Unused,
    Stop(G::OutMsg),
}

pub enum CastResponse<G: GenServer> {
    NoReply(G),
    Unused,
    Stop,
}

pub trait GenServer: Send + Sized + Clone {
    type CallMsg: Clone + Send + Sized + Sync;
    type CastMsg: Clone + Send + Sized + Sync;
    type OutMsg: Send + Sized;
    type Error: Debug + Send;

    /// Starts the GenServer, without waiting for it to finalize its `init` process.
    fn start(self) -> GenServerHandle<Self> {
        GenServerHandle::new(self)
    }

    /// Starts the GenServer, waiting for it to finalize its `init` process.
    fn verified_start(self) -> Result<GenServerHandle<Self>, GenServerError> {
        GenServerHandle::verified_new(self)
    }

    /// Tokio tasks depend on a coolaborative multitasking model. "work stealing" can't
    /// happen if the task is blocking the thread. As such, for sync compute task
    /// or other blocking tasks need to be in their own separate thread, and the OS
    /// will manage them through hardware interrupts.
    /// Start blocking provides such thread.
    ///
    /// As with `start`, it doesn't wait for the GenServer to finalize its `init` process.
    fn start_blocking(self) -> GenServerHandle<Self> {
        GenServerHandle::new_blocking(self)
    }

    /// Tokio tasks depend on a coolaborative multitasking model. "work stealing" can't
    /// happen if the task is blocking the thread. As such, for sync compute task
    /// or other blocking tasks need to be in their own separate thread, and the OS
    /// will manage them through hardware interrupts.
    /// Start blocking provides such thread.
    ///
    /// As with `verified_start`, it waits for the GenServer to finalize its `init` process.
    fn verified_start_blocking(self) -> Result<GenServerHandle<Self>, GenServerError> {
        GenServerHandle::verified_new_blocking(self)
    }

    fn run(
        self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        start_signal_tx: Option<&mut std::sync::mpsc::Sender<bool>>,
    ) -> impl Future<Output = Result<(), GenServerError>> + Send {
        async {
            let init_result = self
                .init(handle)
                .await
                .inspect_err(|err| tracing::error!("Initialization failed: {err:?}"));

            let res = match init_result {
                Ok(new_state) => {
                    if let Some(start_signal_tx) = start_signal_tx {
                        // Notify that the GenServer has started successfully
                        start_signal_tx
                            .send(true)
                            .map_err(|_| GenServerError::Initialization)?;
                    }
                    new_state.main_loop(handle, rx).await
                }
                Err(_) => {
                    if let Some(start_signal_tx) = start_signal_tx {
                        // Notify that the GenServer failed to start
                        start_signal_tx
                            .send(false)
                            .map_err(|_| GenServerError::Initialization)?;
                    }
                    Err(GenServerError::Initialization)
                }
            };

            handle.cancellation_token().cancel();
            if let Ok(final_state) = res {
                if let Err(err) = final_state.teardown(handle).await {
                    tracing::error!("Error during teardown: {err:?}");
                }
            }
            Ok(())
        }
    }

    /// Initialization function. It's called before main loop. It
    /// can be overrided on implementations in case initial steps are
    /// required.
    fn init(
        self,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        async { Ok(self) }
    }

    fn main_loop(
        mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
    ) -> impl Future<Output = Result<Self, GenServerError>> + Send {
        async {
            loop {
                let (new_state, cont) = self.receive(handle, rx).await?;
                self = new_state;
                if !cont {
                    break;
                }
            }
            tracing::trace!("Stopping GenServer");
            Ok(self)
        }
    }

    fn receive(
        self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
    ) -> impl Future<Output = Result<(Self, bool), GenServerError>> + Send {
        async move {
            let message = rx.recv().await;

            // Save current state in case of a rollback
            let state_clone = self.clone();

            let (keep_running, new_state) = match message {
                Some(GenServerInMsg::Call { sender, message }) => {
                    let (keep_running, new_state, response) =
                        match AssertUnwindSafe(self.handle_call(message, handle))
                            .catch_unwind()
                            .await
                        {
                            Ok(response) => match response {
                                CallResponse::Reply(new_state, response) => {
                                    (true, new_state, Ok(response))
                                }
                                CallResponse::Stop(response) => (false, state_clone, Ok(response)),
                                CallResponse::Unused => {
                                    tracing::error!("GenServer received unexpected CallMessage");
                                    (false, state_clone, Err(GenServerError::CallMsgUnused))
                                }
                            },
                            Err(error) => {
                                tracing::error!(
                                    "Error in callback, reverting state - Error: '{error:?}'"
                                );
                                (true, state_clone, Err(GenServerError::Callback))
                            }
                        };
                    // Send response back
                    if sender.send(response).is_err() {
                        tracing::error!(
                            "GenServer failed to send response back, client must have died"
                        )
                    };
                    (keep_running, new_state)
                }
                Some(GenServerInMsg::Cast { message }) => {
                    match AssertUnwindSafe(self.handle_cast(message, handle))
                        .catch_unwind()
                        .await
                    {
                        Ok(response) => match response {
                            CastResponse::NoReply(new_state) => (true, new_state),
                            CastResponse::Stop => (false, state_clone),
                            CastResponse::Unused => {
                                tracing::error!("GenServer received unexpected CastMessage");
                                (false, state_clone)
                            }
                        },
                        Err(error) => {
                            tracing::trace!(
                                "Error in callback, reverting state - Error: '{error:?}'"
                            );
                            (true, state_clone)
                        }
                    }
                }
                None => {
                    // Channel has been closed; won't receive further messages. Stop the server.
                    (false, self)
                }
            };
            Ok((new_state, keep_running))
        }
    }

    fn handle_call(
        self,
        _message: Self::CallMsg,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = CallResponse<Self>> + Send {
        async { CallResponse::Unused }
    }

    fn handle_cast(
        self,
        _message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = CastResponse<Self>> + Send {
        async { CastResponse::Unused }
    }

    /// Teardown function. It's called after the stop message is received.
    /// It can be overrided on implementations in case final steps are required,
    /// like closing streams, stopping timers, etc.
    fn teardown(
        self,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async { Ok(()) }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::tasks::send_after;
    use std::{thread, time::Duration};

    #[derive(Clone)]
    struct BadlyBehavedTask;

    #[derive(Clone)]
    pub enum InMessage {
        GetCount,
        Stop,
    }
    #[derive(Clone)]
    pub enum OutMsg {
        Count(u64),
    }

    impl GenServer for BadlyBehavedTask {
        type CallMsg = InMessage;
        type CastMsg = ();
        type OutMsg = ();
        type Error = ();

        async fn handle_call(
            self,
            _: Self::CallMsg,
            _: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            CallResponse::Stop(())
        }

        async fn handle_cast(
            self,
            _: Self::CastMsg,
            _: &GenServerHandle<Self>,
        ) -> CastResponse<Self> {
            rt::sleep(Duration::from_millis(20)).await;
            thread::sleep(Duration::from_secs(2));
            CastResponse::Stop
        }
    }

    #[derive(Clone)]
    struct WellBehavedTask {
        pub count: u64,
    }

    impl GenServer for WellBehavedTask {
        type CallMsg = InMessage;
        type CastMsg = ();
        type OutMsg = OutMsg;
        type Error = ();

        async fn handle_call(
            self,
            message: Self::CallMsg,
            _: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                InMessage::GetCount => {
                    let count = self.count;
                    CallResponse::Reply(self, OutMsg::Count(count))
                }
                InMessage::Stop => CallResponse::Stop(OutMsg::Count(self.count)),
            }
        }

        async fn handle_cast(
            mut self,
            _: Self::CastMsg,
            handle: &GenServerHandle<Self>,
        ) -> CastResponse<Self> {
            self.count += 1;
            println!("{:?}: good still alive", thread::current().id());
            send_after(Duration::from_millis(100), handle.to_owned(), ());
            CastResponse::NoReply(self)
        }
    }

    #[test]
    pub fn badly_behaved_thread_non_blocking() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start();
            let _ = badboy.cast(()).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
            let _ = goodboy.cast(()).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.call(InMessage::GetCount).await.unwrap();

            match count {
                OutMsg::Count(num) => {
                    assert_ne!(num, 10);
                }
            }
            goodboy.call(InMessage::Stop).await.unwrap();
        });
    }

    #[test]
    pub fn badly_behaved_thread() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start_blocking();
            let _ = badboy.cast(()).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
            let _ = goodboy.cast(()).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.call(InMessage::GetCount).await.unwrap();

            match count {
                OutMsg::Count(num) => {
                    assert_eq!(num, 10);
                }
            }
            goodboy.call(InMessage::Stop).await.unwrap();
        });
    }

    const TIMEOUT_DURATION: Duration = Duration::from_millis(100);

    #[derive(Debug, Default, Clone)]
    struct SomeTask;

    #[derive(Clone)]
    enum SomeTaskCallMsg {
        SlowOperation,
        FastOperation,
    }

    impl GenServer for SomeTask {
        type CallMsg = SomeTaskCallMsg;
        type CastMsg = ();
        type OutMsg = ();
        type Error = ();

        async fn handle_call(
            self,
            message: Self::CallMsg,
            _handle: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                SomeTaskCallMsg::SlowOperation => {
                    // Simulate a slow operation that will not resolve in time
                    rt::sleep(TIMEOUT_DURATION * 2).await;
                    CallResponse::Reply(self, ())
                }
                SomeTaskCallMsg::FastOperation => {
                    // Simulate a fast operation that resolves in time
                    rt::sleep(TIMEOUT_DURATION / 2).await;
                    CallResponse::Reply(self, ())
                }
            }
        }
    }

    #[test]
    pub fn unresolving_task_times_out() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut unresolving_task = SomeTask.start();

            let result = unresolving_task
                .call_with_timeout(SomeTaskCallMsg::FastOperation, TIMEOUT_DURATION)
                .await;
            assert!(matches!(result, Ok(())));

            let result = unresolving_task
                .call_with_timeout(SomeTaskCallMsg::SlowOperation, TIMEOUT_DURATION)
                .await;
            assert!(matches!(result, Err(GenServerError::CallTimeout)));
        });
    }

    #[derive(Clone)]
    struct FailsOnInitTask;

    impl GenServer for FailsOnInitTask {
        type CallMsg = ();
        type CastMsg = ();
        type OutMsg = ();
        type Error = ();

        async fn init(self, _handle: &GenServerHandle<Self>) -> Result<Self, Self::Error> {
            Err(())
        }
    }

    #[test]
    pub fn failing_on_init_task() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Attempt to start a GenServer that fails on initialization
            let result = FailsOnInitTask.verified_start();
            assert!(matches!(result, Err(GenServerError::Initialization)));

            // Other tasks should start correctly
            let result = WellBehavedTask { count: 0 }.verified_start();
            assert!(result.is_ok());
        });
    }
}
