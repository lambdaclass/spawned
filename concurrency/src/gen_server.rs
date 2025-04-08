//! GernServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use std::{
    fmt::Debug,
    panic::{AssertUnwindSafe, catch_unwind},
};

use spawned_rt::{self as rt, JoinHandle, mpsc, oneshot};

#[derive(Debug)]
pub struct GenServerHandle<T, U> {
    pub tx: mpsc::Sender<GenServerInMsg<T, U>>,
    #[allow(unused)]
    handle: JoinHandle<()>,
}

impl<T: Send, U: Send> GenServerHandle<T, U> {
    pub fn sender(&self) -> mpsc::Sender<GenServerInMsg<T, U>> {
        self.tx.clone()
    }

    pub async fn call(&mut self, message: T) -> Result<U, GenServerError> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<U, GenServerError>>();
        let _ = self.tx.send(GenServerInMsg::Call {
            sender: oneshot_tx,
            message,
        });
        match oneshot_rx.await {
            Ok(result) => result,
            Err(_) => Err(GenServerError::ServerError),
        }
    }

    pub async fn cast(&mut self, message: T) {
        let _ = self.tx.send(GenServerInMsg::Cast { message });
    }
}

pub enum GenServerInMsg<T, U> {
    Call {
        sender: oneshot::Sender<Result<U, GenServerError>>,
        message: T,
    },
    Cast {
        message: T,
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

pub enum GenServerError {
    CallbackError,
    ServerError,
}

pub trait GenServer
where
    Self: Send + Sized + 'static,
{
    type InMsg: Send + Sized;
    type OutMsg: Send + Sized;
    type State: Clone;
    type Error: Debug;

    fn start() -> impl Future<Output = GenServerHandle<Self::InMsg, Self::OutMsg>> + Send {
        async {
            let (tx, mut rx) = mpsc::channel::<GenServerInMsg<Self::InMsg, Self::OutMsg>>();
            let tx_clone = tx.clone();
            let handle = rt::spawn(async move {
                Self::init().run(&tx_clone, &mut rx).await;
            });
            GenServerHandle { tx, handle }
        }
    }

    fn run(
        &mut self,
        tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl Future<Output = ()> + Send {
        async {
            self.main_loop(tx, rx).await;
        }
    }

    fn main_loop(
        &mut self,
        tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl Future<Output = ()> + Send {
        async {
            loop {
                if !self.receive(tx, rx).await {
                    break;
                }
            }
            tracing::trace!("Stopping GenServer");
        }
    }

    fn receive(
        &mut self,
        tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl std::future::Future<Output = bool> + Send {
        async {
            match rx.recv().await {
                Some(GenServerInMsg::Call { sender, message }) => {
                    let state = self.state();
                    match catch_unwind(AssertUnwindSafe(|| self.handle_call(message, tx))) {
                        Ok(response) => match response {
                            CallResponse::Reply(response) => {
                                let _ = &sender.send(Ok(response));
                                true
                            }
                            CallResponse::Stop(response) => {
                                let _ = &sender.send(Ok(response));
                                false
                            }
                        },

                        Err(error) => {
                            tracing::trace!(
                                "Error in handle_call callback, reverting state - Error: '{error:?}'"
                            );
                            // Restore initial state (ie. dismiss any change)
                            self.set_state(state);
                            let _ = &sender.send(Err(GenServerError::CallbackError));
                            true
                        }
                    }
                }
                Some(GenServerInMsg::Cast { message }) => {
                    let state = self.state();
                    match catch_unwind(AssertUnwindSafe(|| self.handle_cast(message, tx))) {
                        Ok(response) => match response {
                            CastResponse::NoReply => true,
                            CastResponse::Stop => false,
                        },

                        Err(error) => {
                            tracing::trace!(
                                "Error in handle_cast callback, reverting state - Error: '{error:?}'"
                            );
                            // Restore initial state (ie. dismiss any change)
                            self.set_state(state);
                            true
                        }
                    }
                }
                None => {
                    // Channel has been closed; won't receive further messages. Stop the server.
                    false
                }
            }
        }
    }

    fn init() -> Self;

    fn state(&self) -> Self::State;

    fn set_state(&mut self, state: Self::State);

    fn handle_call(
        &mut self,
        message: Self::InMsg,
        tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> CallResponse<Self::OutMsg>;

    fn handle_cast(
        &mut self,
        _message: Self::InMsg,
        _tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> CastResponse;
}
