//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use futures::future::FutureExt as _;
use spawned_rt::tasks::{self as rt, mpsc, oneshot, timeout, CancellationToken};
use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe, time::Duration};

use crate::{
    error::GenServerError,
    tasks::InitResult::{NoSuccess, Success},
};

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
            if gen_server.run(&handle, &mut rx).await.is_err() {
                tracing::trace!("GenServer crashed")
            };
        });
        handle_clone
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
                if gen_server.run(&handle, &mut rx).await.is_err() {
                    tracing::trace!("GenServer crashed")
                };
            })
        });
        handle_clone
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

pub enum InitResult<G: GenServer> {
    Success(G),
    NoSuccess(G),
}

pub trait GenServer: Send + Sized + Clone {
    type CallMsg: Clone + Send + Sized + Sync;
    type CastMsg: Clone + Send + Sized + Sync;
    type OutMsg: Send + Sized;
    type Error: Debug + Send;

    fn start(self) -> GenServerHandle<Self> {
        GenServerHandle::new(self)
    }

    /// Tokio tasks depend on a coolaborative multitasking model. "work stealing" can't
    /// happen if the task is blocking the thread. As such, for sync compute task
    /// or other blocking tasks need to be in their own separate thread, and the OS
    /// will manage them through hardware interrupts.
    /// Start blocking provides such thread.
    fn start_blocking(self) -> GenServerHandle<Self> {
        GenServerHandle::new_blocking(self)
    }

    fn run(
        self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
    ) -> impl Future<Output = Result<(), GenServerError>> + Send {
        async {
            let res = match self.init(handle).await {
                Ok(Success(new_state)) => new_state.main_loop(handle, rx).await,
                Ok(NoSuccess(intermediate_state)) => {
                    // new_state is NoSuccess, this means the initialization failed, but the error was handled
                    // in callback. No need to report the error.
                    // Just skip main_loop and return the state to teardown the GenServer
                    Ok(intermediate_state)
                }
                Err(err) => {
                    tracing::error!("Initialization failed with unhandled error: {err:?}");
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
    ) -> impl Future<Output = Result<InitResult<Self>, Self::Error>> + Send {
        async { Ok(Success(self)) }
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
    use crate::{messages::Unused, tasks::send_after};
    use std::{
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

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
        type CastMsg = Unused;
        type OutMsg = Unused;
        type Error = Unused;

        async fn handle_call(
            self,
            _: Self::CallMsg,
            _: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            CallResponse::Stop(Unused)
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
        type CastMsg = Unused;
        type OutMsg = OutMsg;
        type Error = Unused;

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
            send_after(Duration::from_millis(100), handle.to_owned(), Unused);
            CastResponse::NoReply(self)
        }
    }

    #[test]
    pub fn badly_behaved_thread_non_blocking() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start();
            let _ = badboy.cast(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
            let _ = goodboy.cast(Unused).await;
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
            let _ = badboy.cast(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
            let _ = goodboy.cast(Unused).await;
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
        type CastMsg = Unused;
        type OutMsg = Unused;
        type Error = Unused;

        async fn handle_call(
            self,
            message: Self::CallMsg,
            _handle: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                SomeTaskCallMsg::SlowOperation => {
                    // Simulate a slow operation that will not resolve in time
                    rt::sleep(TIMEOUT_DURATION * 2).await;
                    CallResponse::Reply(self, Unused)
                }
                SomeTaskCallMsg::FastOperation => {
                    // Simulate a fast operation that resolves in time
                    rt::sleep(TIMEOUT_DURATION / 2).await;
                    CallResponse::Reply(self, Unused)
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
            assert!(matches!(result, Ok(Unused)));

            let result = unresolving_task
                .call_with_timeout(SomeTaskCallMsg::SlowOperation, TIMEOUT_DURATION)
                .await;
            assert!(matches!(result, Err(GenServerError::CallTimeout)));
        });
    }

    #[derive(Clone)]
    struct SomeTaskThatFailsOnInit {
        sender_channel: Arc<Mutex<mpsc::Receiver<u8>>>,
    }

    impl SomeTaskThatFailsOnInit {
        pub fn new(sender_channel: Arc<Mutex<mpsc::Receiver<u8>>>) -> Self {
            Self { sender_channel }
        }
    }

    impl GenServer for SomeTaskThatFailsOnInit {
        type CallMsg = Unused;
        type CastMsg = Unused;
        type OutMsg = Unused;
        type Error = Unused;

        async fn init(
            self,
            _handle: &GenServerHandle<Self>,
        ) -> Result<InitResult<Self>, Self::Error> {
            // Simulate an initialization failure by returning NoSuccess
            Ok(NoSuccess(self))
        }

        async fn teardown(self, _handle: &GenServerHandle<Self>) -> Result<(), Self::Error> {
            self.sender_channel.lock().unwrap().close();
            Ok(())
        }
    }

    #[test]
    pub fn task_fails_with_intermediate_state() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (rx, tx) = mpsc::channel::<u8>();
            let sender_channel = Arc::new(Mutex::new(tx));
            let _task = SomeTaskThatFailsOnInit::new(sender_channel).start();

            // Wait a while to ensure the task has time to run and fail
            rt::sleep(Duration::from_secs(1)).await;

            // We assure that the teardown function has ran by checking that the receiver channel is closed
            assert!(rx.is_closed())
        });
    }
}
