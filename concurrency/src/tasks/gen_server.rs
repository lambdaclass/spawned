//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use futures::future::FutureExt as _;
use spawned_rt::tasks::{self as rt, mpsc, oneshot, CancellationToken};
use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe};

use crate::error::GenServerError;

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
    pub(crate) fn new(initial_state: G::State) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let cancellation_token = CancellationToken::new();
        let handle = GenServerHandle {
            tx,
            cancellation_token,
        };
        let mut gen_server: G = GenServer::new();
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn(async move {
            if gen_server
                .run(&handle, &mut rx, initial_state)
                .await
                .is_err()
            {
                tracing::trace!("GenServer crashed")
            };
        });
        handle_clone
    }

    pub(crate) fn new_blocking(initial_state: G::State) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let cancellation_token = CancellationToken::new();
        let handle = GenServerHandle {
            tx,
            cancellation_token,
        };
        let mut gen_server: G = GenServer::new();
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn_blocking(|| {
            rt::block_on(async move {
                if gen_server
                    .run(&handle, &mut rx, initial_state)
                    .await
                    .is_err()
                {
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
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<G::OutMsg, GenServerError>>();
        self.tx.send(GenServerInMsg::Call {
            sender: oneshot_tx,
            message,
        })?;
        match oneshot_rx.await {
            Ok(result) => result,
            Err(_) => Err(GenServerError::Server),
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
    Reply(G::State, G::OutMsg),
    Unused,
    Stop(G::OutMsg),
}

pub enum CastResponse<G: GenServer> {
    NoReply(G::State),
    Unused,
    Stop,
}

pub trait GenServer
where
    Self: Send + Sized,
{
    type CallMsg: Clone + Send + Sized + Sync;
    type CastMsg: Clone + Send + Sized + Sync;
    type OutMsg: Send + Sized;
    type State: Clone + Send;
    type Error: Debug + Send;

    fn new() -> Self;

    fn start(initial_state: Self::State) -> GenServerHandle<Self> {
        GenServerHandle::new(initial_state)
    }

    /// Tokio tasks depend on a coolaborative multitasking model. "work stealing" can't
    /// happen if the task is blocking the thread. As such, for sync compute task
    /// or other blocking tasks need to be in their own separate thread, and the OS
    /// will manage them through hardware interrupts.
    /// Start blocking provides such thread.
    fn start_blocking(initial_state: Self::State) -> GenServerHandle<Self> {
        GenServerHandle::new_blocking(initial_state)
    }

    fn run(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        state: Self::State,
    ) -> impl Future<Output = Result<(), GenServerError>> + Send {
        async {
            match self.init(handle, state).await {
                Ok(new_state) => {
                    self.main_loop(handle, rx, new_state).await?;
                    Ok(())
                }
                Err(err) => {
                    tracing::error!("Initialization failed: {err:?}");
                    Err(GenServerError::Initialization)
                }
            }
        }
    }

    /// Initialization function. It's called before main loop. It
    /// can be overrided on implementations in case initial steps are
    /// required.
    fn init(
        &mut self,
        _handle: &GenServerHandle<Self>,
        state: Self::State,
    ) -> impl Future<Output = Result<Self::State, Self::Error>> + Send {
        async { Ok(state) }
    }

    fn main_loop(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        mut state: Self::State,
    ) -> impl Future<Output = Result<(), GenServerError>> + Send {
        async {
            loop {
                let (new_state, cont) = self.receive(handle, rx, state).await?;
                state = new_state;
                if !cont {
                    break;
                }
            }
            tracing::trace!("Stopping GenServer");
            handle.cancellation_token().cancel();
            if let Err(err) = self.teardown(handle, state).await {
                tracing::error!("Error during teardown: {err:?}");
            }
            Ok(())
        }
    }

    fn receive(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        state: Self::State,
    ) -> impl Future<Output = Result<(Self::State, bool), GenServerError>> + Send {
        async move {
            let message = rx.recv().await;

            // Save current state in case of a rollback
            let state_clone = state.clone();

            let (keep_running, new_state) = match message {
                Some(GenServerInMsg::Call { sender, message }) => {
                    let (keep_running, new_state, response) =
                        match AssertUnwindSafe(self.handle_call(message, handle, state))
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
                    match AssertUnwindSafe(self.handle_cast(message, handle, state))
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
                    (false, state)
                }
            };
            Ok((new_state, keep_running))
        }
    }

    fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &GenServerHandle<Self>,
        _state: Self::State,
    ) -> impl Future<Output = CallResponse<Self>> + Send {
        async { CallResponse::Unused }
    }

    fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
        _state: Self::State,
    ) -> impl Future<Output = CastResponse<Self>> + Send {
        async { CastResponse::Unused }
    }

    /// Teardown function. It's called after the stop message is received.
    /// It can be overrided on implementations in case final steps are required,
    /// like closing streams, stopping timers, etc.
    fn teardown(
        &mut self,
        _handle: &GenServerHandle<Self>,
        _state: Self::State,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async { Ok(()) }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::tasks::send_after;
    use std::{thread, time::Duration};
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
        type State = ();
        type Error = ();

        fn new() -> Self {
            Self {}
        }

        async fn handle_call(
            &mut self,
            _: Self::CallMsg,
            _: &GenServerHandle<Self>,
            _: Self::State,
        ) -> CallResponse<Self> {
            CallResponse::Stop(())
        }

        async fn handle_cast(
            &mut self,
            _: Self::CastMsg,
            _: &GenServerHandle<Self>,
            _: Self::State,
        ) -> CastResponse<Self> {
            rt::sleep(Duration::from_millis(20)).await;
            thread::sleep(Duration::from_secs(2));
            CastResponse::Stop
        }
    }

    struct WellBehavedTask;

    #[derive(Clone)]
    struct CountState {
        pub count: u64,
    }

    impl GenServer for WellBehavedTask {
        type CallMsg = InMessage;
        type CastMsg = ();
        type OutMsg = OutMsg;
        type State = CountState;
        type Error = ();

        fn new() -> Self {
            Self {}
        }

        async fn handle_call(
            &mut self,
            message: Self::CallMsg,
            _: &GenServerHandle<Self>,
            state: Self::State,
        ) -> CallResponse<Self> {
            match message {
                InMessage::GetCount => {
                    let count = state.count;
                    CallResponse::Reply(state, OutMsg::Count(count))
                }
                InMessage::Stop => CallResponse::Stop(OutMsg::Count(state.count)),
            }
        }

        async fn handle_cast(
            &mut self,
            _: Self::CastMsg,
            handle: &GenServerHandle<Self>,
            mut state: Self::State,
        ) -> CastResponse<Self> {
            state.count += 1;
            println!("{:?}: good still alive", thread::current().id());
            send_after(Duration::from_millis(100), handle.to_owned(), ());
            CastResponse::NoReply(state)
        }
    }

    #[test]
    pub fn badly_behaved_thread_non_blocking() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask::start(());
            let _ = badboy.cast(()).await;
            let mut goodboy = WellBehavedTask::start(CountState { count: 0 });
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
            let mut badboy = BadlyBehavedTask::start_blocking(());
            let _ = badboy.cast(()).await;
            let mut goodboy = WellBehavedTask::start(CountState { count: 0 });
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
}
