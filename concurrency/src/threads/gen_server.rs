//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use spawned_rt::threads::{self as rt, mpsc, oneshot};
use std::{
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
};

use crate::error::GenServerError;

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
    pub(crate) fn new(gen_server: G) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let handle = GenServerHandle { tx };
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
        match self.init(handle) {
            Ok(new_state) => {
                new_state.main_loop(handle, rx)?;
                Ok(())
            }
            Err(err) => {
                tracing::error!("Initialization failed: {err:?}");
                Err(GenServerError::Initialization)
            }
        }
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
            let (new_state, cont) = self.receive(handle, rx)?;
            if !cont {
                break;
            }
            self = new_state;
        }
        tracing::trace!("Stopping GenServer");
        Ok(())
    }

    fn receive(
        self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
    ) -> Result<(Self, bool), GenServerError> {
        let message = rx.recv().ok();

        // Save current state in case of a rollback
        let state_clone = self.clone();

        let (keep_running, new_state) = match message {
            Some(GenServerInMsg::Call { sender, message }) => {
                let (keep_running, new_state, response) =
                    match catch_unwind(AssertUnwindSafe(|| self.handle_call(message, handle))) {
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
                            tracing::trace!(
                                "Error in callback, reverting state - Error: '{error:?}'"
                            );
                            (true, state_clone, Err(GenServerError::Callback))
                        }
                    };
                // Send response back
                if sender.send(response).is_err() {
                    tracing::trace!("GenServer failed to send response back, client must have died")
                };
                (keep_running, new_state)
            }
            Some(GenServerInMsg::Cast { message }) => {
                match catch_unwind(AssertUnwindSafe(|| self.handle_cast(message, handle))) {
                    Ok(response) => match response {
                        CastResponse::NoReply(new_state) => (true, new_state),
                        CastResponse::Stop => (false, state_clone),
                        CastResponse::Unused => {
                            tracing::error!("GenServer received unexpected CastMessage");
                            (false, state_clone)
                        }
                    },
                    Err(error) => {
                        tracing::trace!("Error in callback, reverting state - Error: '{error:?}'");
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

    fn handle_call(
        self,
        _message: Self::CallMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CallResponse<Self> {
        CallResponse::Unused
    }

    fn handle_cast(
        self,
        _message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CastResponse<Self> {
        CastResponse::Unused
    }
}
