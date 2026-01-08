//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! This is the threads-based (blocking) version.
//! See examples/name_server for a usage example.
use crate::{
    error::GenServerError,
    link::{MonitorRef, SystemMessage},
    pid::{ExitReason, HasPid, Pid},
    process_table::{self, LinkError, SystemMessageSender},
    registry::{self, RegistryError},
};
use spawned_rt::threads::{self as rt, mpsc, oneshot};
use std::{
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::RecvTimeoutError,
        Arc,
    },
    time::Duration,
};

const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle to a running GenServer (threads version).
///
/// This handle can be used to send messages to the GenServer and to
/// obtain its unique process identifier (`Pid`).
#[derive(Debug)]
pub struct GenServerHandle<G: GenServer + 'static> {
    /// Unique process identifier for this GenServer.
    pid: Pid,
    /// Channel sender for messages to the GenServer.
    pub tx: mpsc::Sender<GenServerInMsg<G>>,
    /// Shared cancellation flag
    is_cancelled: Arc<AtomicBool>,
    /// Channel for system messages (internal use).
    system_tx: mpsc::Sender<SystemMessage>,
}

impl<G: GenServer> Clone for GenServerHandle<G> {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            tx: self.tx.clone(),
            is_cancelled: self.is_cancelled.clone(),
            system_tx: self.system_tx.clone(),
        }
    }
}

impl<G: GenServer> HasPid for GenServerHandle<G> {
    fn pid(&self) -> Pid {
        self.pid
    }
}

/// Internal sender for system messages, implementing SystemMessageSender trait.
struct GenServerSystemSender {
    system_tx: mpsc::Sender<SystemMessage>,
    /// Shared cancellation flag
    is_cancelled: Arc<AtomicBool>,
}

impl SystemMessageSender for GenServerSystemSender {
    fn send_down(&self, pid: Pid, monitor_ref: MonitorRef, reason: ExitReason) {
        let _ = self.system_tx.send(SystemMessage::Down {
            pid,
            monitor_ref,
            reason,
        });
    }

    fn send_exit(&self, pid: Pid, reason: ExitReason) {
        let _ = self.system_tx.send(SystemMessage::Exit { pid, reason });
    }

    fn kill(&self, _reason: ExitReason) {
        // Kill the process by setting cancellation flag
        self.is_cancelled.store(true, Ordering::SeqCst);
    }

    fn is_alive(&self) -> bool {
        !self.is_cancelled.load(Ordering::SeqCst)
    }
}

impl<G: GenServer> GenServerHandle<G> {
    pub(crate) fn new(gen_server: G) -> Self {
        let pid = Pid::new();
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let (system_tx, mut system_rx) = mpsc::channel::<SystemMessage>();
        let is_cancelled = Arc::new(AtomicBool::new(false));

        // Create the system message sender and register with process table
        let system_sender = Arc::new(GenServerSystemSender {
            system_tx: system_tx.clone(),
            is_cancelled: is_cancelled.clone(),
        });
        process_table::register(pid, system_sender);

        let handle = GenServerHandle {
            pid,
            tx,
            is_cancelled,
            system_tx,
        };
        let handle_clone = handle.clone();

        // Spawn the GenServer on a thread
        let _join_handle = rt::spawn(move || {
            let result = gen_server.run(&handle, &mut rx, &mut system_rx);
            // Unregister from process table on exit
            let exit_reason = match &result {
                Ok(_) => ExitReason::Normal,
                Err(_) => ExitReason::Error("GenServer crashed".to_string()),
            };
            process_table::unregister(pid, exit_reason);
            if let Err(error) = result {
                tracing::trace!(%error, "GenServer crashed")
            }
        });

        handle_clone
    }

    pub fn sender(&self) -> mpsc::Sender<GenServerInMsg<G>> {
        self.tx.clone()
    }

    pub fn call(&mut self, message: G::CallMsg) -> Result<G::OutMsg, GenServerError> {
        self.call_with_timeout(message, DEFAULT_CALL_TIMEOUT)
    }

    pub fn call_with_timeout(
        &mut self,
        message: G::CallMsg,
        duration: Duration,
    ) -> Result<G::OutMsg, GenServerError> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<G::OutMsg, GenServerError>>();
        self.tx.send(GenServerInMsg::Call {
            sender: oneshot_tx,
            message,
        })?;

        // oneshot uses crossbeam_channel which has recv_timeout
        // We match on the error kind since crossbeam's error types aren't directly exported
        match oneshot_rx.recv_timeout(duration) {
            Ok(result) => result,
            Err(err) => {
                // crossbeam_channel::RecvTimeoutError has is_timeout() and is_disconnected() methods
                if err.is_timeout() {
                    Err(GenServerError::CallTimeout)
                } else {
                    Err(GenServerError::Server)
                }
            }
        }
    }

    pub fn cast(&mut self, message: G::CastMsg) -> Result<(), GenServerError> {
        self.tx
            .send(GenServerInMsg::Cast { message })
            .map_err(|_error| GenServerError::Server)
    }

    /// Check if this GenServer has been cancelled/stopped.
    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::SeqCst)
    }

    /// Stop the GenServer.
    ///
    /// The GenServer will exit and call its `teardown` method.
    pub fn stop(&self) {
        self.is_cancelled.store(true, Ordering::SeqCst);
    }

    // ==================== Linking & Monitoring ====================

    /// Create a bidirectional link with another process.
    ///
    /// When either process exits abnormally, the other will be notified.
    /// If the other process is not trapping exits and this process crashes,
    /// the other process will also crash.
    pub fn link(&self, other: &impl HasPid) -> Result<(), LinkError> {
        process_table::link(self.pid, other.pid())
    }

    /// Remove a bidirectional link with another process.
    pub fn unlink(&self, other: &impl HasPid) {
        process_table::unlink(self.pid, other.pid())
    }

    /// Monitor another process.
    ///
    /// When the monitored process exits, this process will receive a DOWN message.
    /// Unlike links, monitors are unidirectional and don't cause the monitoring
    /// process to crash.
    ///
    /// Returns a `MonitorRef` that can be used to cancel the monitor.
    pub fn monitor(&self, other: &impl HasPid) -> Result<MonitorRef, LinkError> {
        process_table::monitor(self.pid, other.pid())
    }

    /// Stop monitoring a process.
    pub fn demonitor(&self, monitor_ref: MonitorRef) {
        process_table::demonitor(monitor_ref)
    }

    /// Set whether this process traps exits.
    ///
    /// When trap_exit is true, EXIT messages from linked processes are delivered
    /// as messages instead of causing this process to crash.
    pub fn trap_exit(&self, trap: bool) {
        process_table::set_trap_exit(self.pid, trap)
    }

    /// Check if this process is trapping exits.
    pub fn is_trapping_exit(&self) -> bool {
        process_table::is_trapping_exit(self.pid)
    }

    /// Check if another process is alive.
    pub fn is_alive(&self, other: &impl HasPid) -> bool {
        process_table::is_alive(other.pid())
    }

    /// Get all processes linked to this process.
    pub fn get_links(&self) -> Vec<Pid> {
        process_table::get_links(self.pid)
    }

    // ==================== Registry ====================

    /// Register this process with a unique name.
    ///
    /// Once registered, other processes can find this process using
    /// `registry::whereis("name")`.
    pub fn register(&self, name: impl Into<String>) -> Result<(), RegistryError> {
        registry::register(name, self.pid)
    }

    /// Unregister this process from the registry.
    ///
    /// After this, the process can no longer be found by name.
    pub fn unregister(&self) {
        registry::unregister_pid(self.pid)
    }

    /// Get the registered name of this process, if any.
    pub fn registered_name(&self) -> Option<String> {
        registry::name_of(self.pid)
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

/// Response from handle_info callback.
pub enum InfoResponse {
    /// Continue running, message was handled.
    NoReply,
    /// Stop the GenServer.
    Stop,
}

pub enum InitResult<G: GenServer> {
    Success(G),
    NoSuccess(G),
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
        system_rx: &mut mpsc::Receiver<SystemMessage>,
    ) -> Result<(), GenServerError> {
        let res = match self.init(handle) {
            Ok(InitResult::Success(new_state)) => Ok(new_state.main_loop(handle, rx, system_rx)),
            Ok(InitResult::NoSuccess(intermediate_state)) => {
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

        handle.stop();

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
    fn init(self, _handle: &GenServerHandle<Self>) -> Result<InitResult<Self>, Self::Error> {
        Ok(InitResult::Success(self))
    }

    fn main_loop(
        mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        system_rx: &mut mpsc::Receiver<SystemMessage>,
    ) -> Self {
        loop {
            if !self.receive(handle, rx, system_rx) {
                break;
            }
        }
        tracing::trace!("Stopping GenServer");
        self
    }

    fn receive(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        system_rx: &mut mpsc::Receiver<SystemMessage>,
    ) -> bool {
        // Check for cancellation
        if handle.is_cancelled() {
            return false;
        }

        // Try to receive a system message first (priority)
        if let Ok(system_msg) = system_rx.try_recv() {
            return match catch_unwind(AssertUnwindSafe(|| self.handle_info(system_msg, handle))) {
                Ok(response) => match response {
                    InfoResponse::NoReply => true,
                    InfoResponse::Stop => false,
                },
                Err(error) => {
                    tracing::error!("Error in handle_info: '{error:?}'");
                    false
                }
            };
        }

        // Try to receive a regular message with a short timeout to allow checking cancellation
        let message = rx.recv_timeout(Duration::from_millis(100));

        match message {
            Ok(GenServerInMsg::Call { sender, message }) => {
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
                        tracing::error!("Error in callback: '{error:?}'");
                        (false, Err(GenServerError::Callback))
                    }
                };
                // Send response back
                if sender.send(response).is_err() {
                    tracing::trace!(
                        "GenServer failed to send response back, client must have died"
                    )
                };
                keep_running
            }
            Ok(GenServerInMsg::Cast { message }) => {
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
                        tracing::trace!("Error in callback: '{error:?}'");
                        false
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // No message yet, continue looping (will check cancellation at top)
                true
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Channel has been closed; won't receive further messages. Stop the server.
                false
            }
        }
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

    /// Handle system messages (DOWN, EXIT, Timeout).
    ///
    /// This is called when:
    /// - A monitored process exits (receives `SystemMessage::Down`)
    /// - A linked process exits and trap_exit is enabled (receives `SystemMessage::Exit`)
    /// - A timer fires (receives `SystemMessage::Timeout`)
    ///
    /// Default implementation ignores all system messages.
    fn handle_info(
        &mut self,
        _message: SystemMessage,
        _handle: &GenServerHandle<Self>,
    ) -> InfoResponse {
        InfoResponse::NoReply
    }

    /// Teardown function. It's called after the stop message is received.
    /// It can be overrided on implementations in case final steps are required,
    /// like closing streams, stopping timers, etc.
    fn teardown(self, _handle: &GenServerHandle<Self>) -> Result<(), Self::Error> {
        Ok(())
    }
}
