//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use spawned_rt::threads::{self as rt, mpsc, oneshot};
use std::{
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
};

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
        let _join_handle = rt::spawn(move || {
            if gen_server
                .run(&handle, &mut rx, &mut initial_state)
                .is_err()
            {
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
            Err(_) => Err(GenServerError::ServerError),
        }
    }

    pub fn cast(&mut self, message: G::CastMsg) -> Result<(), GenServerError> {
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

    /// We copy the same interface than threads, but all threads can work
    /// while blocking by default
    fn start_blocking(initial_state: Self::State) -> GenServerHandle<Self> {
        GenServerHandle::new(initial_state)
    }

    fn run(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        state: &mut Self::State,
    ) -> Result<(), GenServerError> {
        self.main_loop(handle, rx, state)?;
        Ok(())
    }

    fn main_loop(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        state: &mut Self::State,
    ) -> Result<(), GenServerError> {
        loop {
            if !self.receive(handle, rx, state)? {
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
        state: &mut Self::State,
    ) -> Result<bool, GenServerError> {
        let message = rx.recv().ok();

        // Save current state in case of a rollback
        let state_clone = state.clone();

        let (keep_running, error) = match message {
            Some(GenServerInMsg::Call { sender, message }) => {
                let (keep_running, error, response) = match catch_unwind(AssertUnwindSafe(|| {
                    self.handle_call(message, handle, state)
                })) {
                    Ok(response) => match response {
                        CallResponse::Reply(response) => (true, None, Ok(response)),
                        CallResponse::Stop(response) => (false, None, Ok(response)),
                    },
                    Err(error) => (true, Some(error), Err(GenServerError::CallbackError)),
                };
                // Send response back
                if sender.send(response).is_err() {
                    tracing::trace!("GenServer failed to send response back, client must have died")
                };
                (keep_running, error)
            }
            Some(GenServerInMsg::Cast { message }) => {
                match catch_unwind(AssertUnwindSafe(|| {
                    self.handle_cast(message, handle, state)
                })) {
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

    fn handle_call(
        &mut self,
        message: Self::CallMsg,
        handle: &GenServerHandle<Self>,
        state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg>;

    fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        handle: &GenServerHandle<Self>,
        state: &mut Self::State,
    ) -> CastResponse;
}
