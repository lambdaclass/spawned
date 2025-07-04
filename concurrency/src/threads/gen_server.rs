//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use spawned_rt::threads::{self as rt, mpsc, oneshot};
use std::{
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
};

use crate::error::GenServerError;

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
    pub(crate) fn new(initial_state: G::State) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let handle = GenServerHandle { tx };
        let mut gen_server: G = GenServer::new();
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn(move || {
            if gen_server.run(&handle, &mut rx, initial_state).is_err() {
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
    type CallMsg: Clone + Send + Sized;
    type CastMsg: Clone + Send + Sized;
    type OutMsg: Send + Sized;
    type State: Clone + Send;
    type Error: Debug;

    fn new() -> Self;

    fn start(initial_state: Self::State) -> GenServerHandle<Self> {
        GenServerHandle::new(initial_state)
    }

    /// We copy the same interface as tasks, but all threads can work
    /// while blocking by default
    fn start_blocking(initial_state: Self::State) -> GenServerHandle<Self> {
        GenServerHandle::new(initial_state)
    }

    fn run(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        state: Self::State,
    ) -> Result<(), GenServerError> {
        match self.init(handle, state) {
            Ok(new_state) => {
                self.main_loop(handle, rx, new_state)?;
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
    fn init(
        &mut self,
        _handle: &GenServerHandle<Self>,
        state: Self::State,
    ) -> Result<Self::State, Self::Error> {
        Ok(state)
    }

    fn main_loop(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        mut state: Self::State,
    ) -> Result<(), GenServerError> {
        loop {
            let (new_state, cont) = self.receive(handle, rx, state)?;
            if !cont {
                break;
            }
            state = new_state;
        }
        tracing::trace!("Stopping GenServer");
        Ok(())
    }

    fn receive(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        state: Self::State,
    ) -> Result<(Self::State, bool), GenServerError> {
        let message = rx.recv().ok();

        // Save current state in case of a rollback
        let state_clone = state.clone();

        let (keep_running, new_state) = match message {
            Some(GenServerInMsg::Call { sender, message }) => {
                let (keep_running, new_state, response) =
                    match catch_unwind(AssertUnwindSafe(|| {
                        self.handle_call(message, handle, state)
                    })) {
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
                match catch_unwind(AssertUnwindSafe(|| {
                    self.handle_cast(message, handle, state)
                })) {
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
                (false, state)
            }
        };
        Ok((new_state, keep_running))
    }

    fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &GenServerHandle<Self>,
        _state: Self::State,
    ) -> CallResponse<Self> {
        CallResponse::Unused
    }

    fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
        _state: Self::State,
    ) -> CastResponse<Self> {
        CastResponse::Unused
    }
}
