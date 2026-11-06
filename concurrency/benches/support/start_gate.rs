//! One-shot startup rendezvous that fails instead of stranding scoped threads.
use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};
#[derive(Clone)]
pub struct StartGate(Arc<Inner>);
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    timeout: Duration,
}
struct State {
    remaining: usize,
    open: bool,
    aborted: bool,
}
pub struct Participant {
    gate: StartGate,
    armed: bool,
}
impl StartGate {
    pub fn new(parties: usize) -> Self {
        Self::with_timeout(parties, Duration::from_secs(30))
    }
    pub fn with_timeout(parties: usize, timeout: Duration) -> Self {
        assert!(parties > 0);
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                remaining: parties,
                open: false,
                aborted: false,
            }),
            changed: Condvar::new(),
            timeout,
        }))
    }
    /// Construct before spawning, so a failed spawn also closes the gate.
    pub fn participant(&self) -> Participant {
        Participant {
            gate: self.clone(),
            armed: true,
        }
    }
    fn abort(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.open {
            state.aborted = true;
            self.0.changed.notify_all();
        }
    }
    fn wait(&self) -> Result<(), &'static str> {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.aborted {
            return Err("startup participant failed or timed out");
        }
        if state.open {
            return Ok(());
        }
        state.remaining -= 1;
        if state.remaining == 0 {
            state.open = true;
            self.0.changed.notify_all();
            return Ok(());
        }
        let (mut state, timed) = self
            .0
            .changed
            .wait_timeout_while(state, self.0.timeout, |s| !s.open && !s.aborted)
            .unwrap_or_else(|p| p.into_inner());
        if timed.timed_out() && !state.open {
            state.aborted = true;
            self.0.changed.notify_all();
        }
        if state.open {
            Ok(())
        } else {
            Err("startup participant failed or timed out")
        }
    }
}
impl Participant {
    pub fn wait(mut self) -> Result<(), &'static str> {
        let result = self.gate.wait();
        self.armed = false;
        result
    }
}
impl Drop for Participant {
    fn drop(&mut self) {
        if self.armed {
            self.gate.abort();
        }
    }
}
