// The production mailbox and Loom test supply their synchronization types.
use super::{AtomicUsize, Condvar, Mutex, Ordering};
const CLOSED: usize = 1 << (usize::BITS - 1);
pub(crate) struct SendGate {
    sends: AtomicUsize,
    wait: Mutex<()>,
    quiet: Condvar,
}
impl Default for SendGate {
    fn default() -> Self {
        Self {
            sends: AtomicUsize::new(0),
            wait: Mutex::new(()),
            quiet: Condvar::new(),
        }
    }
}
impl SendGate {
    pub(crate) fn enter(&self) -> Option<SendPermit<'_>> {
        self.sends
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n < CLOSED - 1 {
                    Some(n + 1)
                } else {
                    None
                }
            })
            .ok()
            .map(|_| SendPermit(self))
    }
    pub(crate) fn close(&self) {
        self.sends.fetch_or(CLOSED, Ordering::AcqRel);
        let mut wait = self.wait.lock().unwrap_or_else(|p| p.into_inner());
        while self.sends.load(Ordering::Acquire) != CLOSED {
            wait = self.quiet.wait(wait).unwrap_or_else(|p| p.into_inner());
        }
    }
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.sends.load(Ordering::Acquire) & CLOSED != 0
    }
}
pub(crate) struct SendPermit<'a>(&'a SendGate);
impl Drop for SendPermit<'_> {
    fn drop(&mut self) {
        if self.0.sends.fetch_sub(1, Ordering::AcqRel) == CLOSED + 1 {
            let _wait = self.0.wait.lock().unwrap_or_else(|p| p.into_inner());
            self.0.quiet.notify_all();
        }
    }
}
