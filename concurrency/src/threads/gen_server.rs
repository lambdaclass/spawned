//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use spawned_rt::threads::{self as rt, mpsc, oneshot, CancellationToken};
use std::{
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
};

use crate::error::GenServerError;

#[derive(Debug)]
pub struct GenServerHandle<G: GenServer + 'static> {
    pub tx: mpsc::Sender<GenServerInMsg<G>>,
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
        let _join_handle = rt::spawn(move || {
            if gen_server.run(&handle, &mut rx).is_err() {
                tracing::trace!("GenServer crashed")
            };
        });
        handle_clone
    }

    pub fn sender(&self) -> mpsc::Sender<GenServerInMsg<G>> {
        self.tx.clone()
    }

    pub fn call(&mut self, message: G::CallMsg) -> Result<G::OutMsg, GenServerError> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<G::OutMsg, GenServerError>>();
        self.tx.send(GenServerInMsg::Call {
            sender: oneshot_tx,
            message,
        })?;
        match oneshot_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(GenServerError::Server),
        }
    }

    pub fn cast(&mut self, message: G::CastMsg) -> Result<(), GenServerError> {
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
    Reply(G::OutMsg),
    Unused,
    Stop(G::OutMsg),
}

pub enum CastResponse {
    NoReply,
    Unused,
    Stop,
}

pub trait GenServer: Send + Sized {
    type CallMsg: Clone + Send + Sized;
    type CastMsg: Clone + Send + Sized;
    type OutMsg: Send + Sized;
    type Error: Debug;

    fn start(self) -> GenServerHandle<Self> {
        GenServerHandle::new(self)
    }

    /// We copy the same interface as tasks, but all threads can work
    /// while blocking by default
    fn start_blocking(self) -> GenServerHandle<Self> {
        GenServerHandle::new(self)
    }

    fn run(
        self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
    ) -> Result<(), GenServerError> {
        let mut cancellation_token = handle.cancellation_token.clone();
        let res = match self.init(handle) {
            Ok(new_state) => Ok(new_state.main_loop(handle, rx)?),
            Err(err) => {
                tracing::error!("Initialization failed: {err:?}");
                Err(GenServerError::Initialization)
            }
        };
        cancellation_token.cancel();
        res
    }

    /// Initialization function. It's called before main loop. It
    /// can be overrided on implementations in case initial steps are
    /// required.
    fn init(self, _handle: &GenServerHandle<Self>) -> Result<Self, Self::Error> {
        Ok(self)
    }

    fn main_loop(
        mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
    ) -> Result<(), GenServerError> {
        loop {
            if !self.receive(handle, rx)? {
                break;
            }
        }
        tracing::trace!("Stopping GenServer");
        Ok(())
    }

    fn receive(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
    ) -> Result<bool, GenServerError> {
        let message = rx.recv().ok();

        let keep_running = match message {
            Some(GenServerInMsg::Call { sender, message }) => {
                let (keep_running, response) = match catch_unwind(AssertUnwindSafe(|| {
                    self.handle_call(message, handle)
                })) {
                    Ok(response) => match response {
                        CallResponse::Reply(response) => (true, Ok(response)),
                        CallResponse::Stop(response) => (false, Ok(response)),
                        CallResponse::Unused => {
                            tracing::error!("GenServer received unexpected CallMessage");
                            (false, Err(GenServerError::CallMsgUnused))
                        }
                    },
                    Err(error) => {
                        tracing::trace!("Error in callback, reverting state - Error: '{error:?}'");
                        (true, Err(GenServerError::Callback))
                    }
                };
                // Send response back
                if sender.send(response).is_err() {
                    tracing::trace!("GenServer failed to send response back, client must have died")
                };
                keep_running
            }
            Some(GenServerInMsg::Cast { message }) => {
                match catch_unwind(AssertUnwindSafe(|| self.handle_cast(message, handle))) {
                    Ok(response) => match response {
                        CastResponse::NoReply => true,
                        CastResponse::Stop => false,
                        CastResponse::Unused => {
                            tracing::error!("GenServer received unexpected CastMessage");
                            false
                        }
                    },
                    Err(error) => {
                        tracing::trace!("Error in callback, reverting state - Error: '{error:?}'");
                        true
                    }
                }
            }
            None => {
                // Channel has been closed; won't receive further messages. Stop the server.
                false
            }
        };
        Ok(keep_running)
    }

    fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CallResponse<Self> {
        CallResponse::Unused
    }

    fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CastResponse {
        CastResponse::Unused
    }
}
