//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use futures::future::FutureExt as _;
use spawned_rt::tasks::{self as rt, mpsc, oneshot};
use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe};

use super::error::GenServerError;

#[derive(Debug)]
pub struct GenServerHandle<G: GenServer + 'static> {
    pub tx: mpsc::Sender<GenServerInMsg<G>>,
}

impl<G: GenServer> Clone for GenServerHandle<G> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<G: GenServer> GenServerHandle<G> {
    pub(crate) fn new(mut initial_state: G::State) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let handle = GenServerHandle { tx };
        let mut gen_server: G = GenServer::new();
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn(async move {
            if gen_server
                .run(&handle, &mut rx, &mut initial_state)
                .await
                .is_err()
            {
                tracing::trace!("GenServer crashed")
            };
        });
        handle_clone
    }

    pub(crate) fn new_blocking(mut initial_state: G::State) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let handle = GenServerHandle { tx };
        let mut gen_server: G = GenServer::new();
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn_blocking(|| {
            rt::block_on(async move {
                if gen_server
                    .run(&handle, &mut rx, &mut initial_state)
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
            Err(_) => Err(GenServerError::ServerError),
        }
    }

    pub async fn cast(&mut self, message: G::CastMsg) -> Result<(), GenServerError> {
        tracing::info!("Sending");
        self.tx
            .send(GenServerInMsg::Cast { message })
            .map_err(|_error| GenServerError::ServerError)
    }
}

pub enum GenServerInMsg<A: GenServer> {
    Call {
        sender: oneshot::Sender<Result<A::OutMsg, GenServerError>>,
        message: A::CallMsg,
    },
    Cast {
        message: A::CastMsg,
    },
}

pub enum CallResponse<U> {
    Reply(U),
    Stop(U),
}

pub enum CastResponse {
    NoReply,
    Stop,
}

pub trait GenServer
where
    Self: Send + Sized,
{
    type CallMsg: Send + Sized;
    type CastMsg: Send + Sized;
    type OutMsg: Send + Sized;
    type State: Clone + Send;
    type Error: Debug;

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
        state: &mut Self::State,
    ) -> impl Future<Output = Result<(), GenServerError>> + Send {
        async {
            self.main_loop(handle, rx, state).await?;
            Ok(())
        }
    }

    fn main_loop(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        state: &mut Self::State,
    ) -> impl Future<Output = Result<(), GenServerError>> + Send {
        async {
            loop {
                if !self.receive(handle, rx, state).await? {
                    break;
                }
            }
            tracing::trace!("Stopping GenServer");
            Ok(())
        }
    }

    fn receive(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        state: &mut Self::State,
    ) -> impl std::future::Future<Output = Result<bool, GenServerError>> + Send {
        async {
            let message = rx.recv().await;
            tracing::info!("received");

            // Save current state in case of a rollback
            let state_clone = state.clone();

            let (keep_running, error) = match message {
                Some(GenServerInMsg::Call { sender, message }) => {
                    let (keep_running, error, response) =
                        match AssertUnwindSafe(self.handle_call(message, handle, state))
                            .catch_unwind()
                            .await
                        {
                            Ok(response) => match response {
                                CallResponse::Reply(response) => (true, None, Ok(response)),
                                CallResponse::Stop(response) => (false, None, Ok(response)),
                            },
                            Err(error) => (true, Some(error), Err(GenServerError::CallbackError)),
                        };
                    // Send response back
                    if sender.send(response).is_err() {
                        tracing::trace!(
                            "GenServer failed to send response back, client must have died"
                        )
                    };
                    (keep_running, error)
                }
                Some(GenServerInMsg::Cast { message }) => {
                    match AssertUnwindSafe(self.handle_cast(message, handle, state))
                        .catch_unwind()
                        .await
                    {
                        Ok(response) => match response {
                            CastResponse::NoReply => (true, None),
                            CastResponse::Stop => (false, None),
                        },
                        Err(error) => (true, Some(error)),
                    }
                }
                None => {
                    // Channel has been closed; won't receive further messages. Stop the server.
                    (false, None)
                }
            };
            if let Some(error) = error {
                tracing::trace!("Error in callback, reverting state - Error: '{error:?}'");
                // Restore initial state (ie. dismiss any change)
                *state = state_clone;
            };
            Ok(keep_running)
        }
    }

    fn handle_call(
        &mut self,
        message: Self::CallMsg,
        handle: &GenServerHandle<Self>,
        state: &mut Self::State,
    ) -> impl std::future::Future<Output = CallResponse<Self::OutMsg>> + Send;

    fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        handle: &GenServerHandle<Self>,
        state: &mut Self::State,
    ) -> impl std::future::Future<Output = CastResponse> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::send_after;
    use std::{process::exit, thread, time::Duration};
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
            _: &mut Self::State,
        ) -> CallResponse<Self::OutMsg> {
            CallResponse::Stop(())
        }

        async fn handle_cast(
            &mut self,
            _: Self::CastMsg,
            _: &GenServerHandle<Self>,
            _: &mut Self::State,
        ) -> CastResponse {
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
            state: &mut Self::State,
        ) -> CallResponse<Self::OutMsg> {
            match message {
                InMessage::GetCount => CallResponse::Reply(OutMsg::Count(state.count)),
                InMessage::Stop => CallResponse::Stop(OutMsg::Count(state.count)),
            }
        }

        async fn handle_cast(
            &mut self,
            _: Self::CastMsg,
            handle: &GenServerHandle<Self>,
            state: &mut Self::State,
        ) -> CastResponse {
            state.count += 1;
            println!("{:?}: good still alive", thread::current().id());
            send_after(Duration::from_millis(100), handle.to_owned(), ());
            CastResponse::NoReply
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
