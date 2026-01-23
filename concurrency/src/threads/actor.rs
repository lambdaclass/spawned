//! Actor trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use spawned_rt::threads::{
    self as rt, mpsc, oneshot, oneshot::RecvTimeoutError, CancellationToken,
};
use std::{
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
    time::Duration,
};

use crate::error::ActorError;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct ActorRef<A: Actor + 'static> {
    pub tx: mpsc::Sender<ActorInMsg<A>>,
    cancellation_token: CancellationToken,
}

impl<A: Actor> Clone for ActorRef<A> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
        }
    }
}

impl<A: Actor> ActorRef<A> {
    pub(crate) fn new(actor: A) -> Self {
        let (tx, mut rx) = mpsc::channel::<ActorInMsg<A>>();
        let cancellation_token = CancellationToken::new();
        let handle = ActorRef {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn(move || {
            if actor.run(&handle, &mut rx).is_err() {
                tracing::trace!("Actor crashed")
            };
        });
        handle_clone
    }

    pub fn sender(&self) -> mpsc::Sender<ActorInMsg<A>> {
        self.tx.clone()
    }

    pub fn request(&mut self, message: A::Request) -> Result<A::Reply, ActorError> {
        self.request_with_timeout(message, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn request_with_timeout(
        &mut self,
        message: A::Request,
        duration: Duration,
    ) -> Result<A::Reply, ActorError> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<A::Reply, ActorError>>();
        self.tx.send(ActorInMsg::Request {
            sender: oneshot_tx,
            message,
        })?;
        match oneshot_rx.recv_timeout(duration) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(ActorError::RequestTimeout),
            Err(RecvTimeoutError::Disconnected) => Err(ActorError::Server),
        }
    }

    pub fn send(&mut self, message: A::Message) -> Result<(), ActorError> {
        self.tx
            .send(ActorInMsg::Message { message })
            .map_err(|_error| ActorError::Server)
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    /// Blocks until the actor has stopped.
    ///
    /// This method blocks the current thread until the actor has finished
    /// processing and exited its main loop.
    pub fn join(&self) {
        let mut token = self.cancellation_token.clone();
        while !token.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

pub enum ActorInMsg<A: Actor> {
    Request {
        sender: oneshot::Sender<Result<A::Reply, ActorError>>,
        message: A::Request,
    },
    Message {
        message: A::Message,
    },
}

pub enum RequestResponse<A: Actor> {
    Reply(A::Reply),
    Unused,
    Stop(A::Reply),
}

pub enum MessageResponse {
    NoReply,
    Unused,
    Stop,
}

pub enum InitResult<A: Actor> {
    Success(A),
    NoSuccess(A),
}

pub trait Actor: Send + Sized {
    type Request: Clone + Send + Sized + Sync;
    type Message: Clone + Send + Sized + Sync;
    type Reply: Send + Sized;
    type Error: Debug + Send;

    fn start(self) -> ActorRef<Self> {
        ActorRef::new(self)
    }

    fn run(
        self,
        handle: &ActorRef<Self>,
        rx: &mut mpsc::Receiver<ActorInMsg<Self>>,
    ) -> Result<(), ActorError> {
        let mut cancellation_token = handle.cancellation_token.clone();

        let res = match self.init(handle) {
            Ok(InitResult::Success(new_state)) => {
                let final_state = new_state.main_loop(handle, rx)?;
                Ok(final_state)
            }
            Ok(InitResult::NoSuccess(intermediate_state)) => {
                // Initialization failed but error was handled in callback.
                // Skip main_loop and return state for teardown.
                Ok(intermediate_state)
            }
            Err(err) => {
                tracing::error!("Initialization failed with unhandled error: {err:?}");
                Err(ActorError::Initialization)
            }
        };

        cancellation_token.cancel();

        if let Ok(final_state) = res {
            if let Err(err) = final_state.teardown(handle) {
                tracing::error!("Error during teardown: {err:?}");
            }
        }

        Ok(())
    }

    /// Initialization function. It's called before main loop. It
    /// can be overrided on implementations in case initial steps are
    /// required.
    fn init(self, _handle: &ActorRef<Self>) -> Result<InitResult<Self>, Self::Error> {
        Ok(InitResult::Success(self))
    }

    fn main_loop(
        mut self,
        handle: &ActorRef<Self>,
        rx: &mut mpsc::Receiver<ActorInMsg<Self>>,
    ) -> Result<Self, ActorError> {
        loop {
            if !self.receive(handle, rx)? {
                break;
            }
        }
        tracing::trace!("Stopping Actor");
        Ok(self)
    }

    fn receive(
        &mut self,
        handle: &ActorRef<Self>,
        rx: &mut mpsc::Receiver<ActorInMsg<Self>>,
    ) -> Result<bool, ActorError> {
        let message = rx.recv().ok();

        let keep_running = match message {
            Some(ActorInMsg::Request { sender, message }) => {
                let (keep_running, response) = match catch_unwind(AssertUnwindSafe(|| {
                    self.handle_request(message, handle)
                })) {
                    Ok(response) => match response {
                        RequestResponse::Reply(response) => (true, Ok(response)),
                        RequestResponse::Stop(response) => (false, Ok(response)),
                        RequestResponse::Unused => {
                            tracing::error!("Actor received unexpected Request");
                            (false, Err(ActorError::RequestUnused))
                        }
                    },
                    Err(error) => {
                        tracing::trace!("Error in callback, reverting state - Error: '{error:?}'");
                        (true, Err(ActorError::Callback))
                    }
                };
                // Send response back
                if sender.send(response).is_err() {
                    tracing::trace!("Actor failed to send response back, client must have died")
                };
                keep_running
            }
            Some(ActorInMsg::Message { message }) => {
                match catch_unwind(AssertUnwindSafe(|| self.handle_message(message, handle))) {
                    Ok(response) => match response {
                        MessageResponse::NoReply => true,
                        MessageResponse::Stop => false,
                        MessageResponse::Unused => {
                            tracing::error!("Actor received unexpected Message");
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

    fn handle_request(
        &mut self,
        _message: Self::Request,
        _handle: &ActorRef<Self>,
    ) -> RequestResponse<Self> {
        RequestResponse::Unused
    }

    fn handle_message(
        &mut self,
        _message: Self::Message,
        _handle: &ActorRef<Self>,
    ) -> MessageResponse {
        MessageResponse::Unused
    }

    /// Teardown function. It's called after the stop message is received.
    /// It can be overrided on implementations in case final steps are required,
    /// like closing streams, stopping timers, etc.
    fn teardown(self, _handle: &ActorRef<Self>) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Spawns a thread that runs a blocking operation and sends a message to an Actor
/// on completion. This is the sync equivalent of tasks::send_message_on.
/// This function returns a handle to the spawned thread.
pub fn send_message_on<T, F>(handle: ActorRef<T>, f: F, message: T::Message) -> rt::JoinHandle<()>
where
    T: Actor,
    F: FnOnce() + Send + 'static,
{
    let mut cancellation_token = handle.cancellation_token();
    let mut handle_clone = handle.clone();
    rt::spawn(move || {
        f();
        if !cancellation_token.is_cancelled() {
            if let Err(e) = handle_clone.send(message) {
                tracing::error!("Failed to send message: {e:?}")
            }
        }
    })
}
