//! Small executor-independent watch channel for thread-only lifecycle signals.
//! Values are read under a mutex. Each pending `changed` future owns a removable
//! waker registration; cancelling a wait never retains its executor or actor.
//!
//! This is a minimal lifecycle implementation, not the full Tokio watch API.
//! A Ref holds an exclusive mutex: copy/clone the value and release it before
//! calling another operation on the channel (including a second borrow).
use std::{
    collections::BTreeMap,
    future::Future,
    ops::Deref,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, Waker},
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}
struct State<T> {
    value: T,
    revision: u64,
    senders: usize,
    next_waiter: u64,
    waiters: BTreeMap<u64, Waker>,
}
struct Inner<T> {
    state: Mutex<State<T>>,
}
pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}
pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
    seen: u64,
}
pub struct Ref<'a, T>(MutexGuard<'a, State<T>>);
impl<T> Deref for Ref<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.value
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvError;
impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("watch sender closed")
    }
}
impl std::error::Error for RecvError {}

pub fn channel<T>(value: T) -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner {
        state: Mutex::new(State {
            value,
            revision: 0,
            senders: 1,
            next_waiter: 0,
            waiters: BTreeMap::new(),
        }),
    });
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner, seen: 0 },
    )
}
fn wake(waiters: BTreeMap<u64, Waker>) {
    for waker in waiters.into_values() {
        waker.wake();
    }
}
impl<T> Sender<T> {
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref(lock(&self.inner.state))
    }
    pub fn subscribe(&self) -> Receiver<T> {
        let state = lock(&self.inner.state);
        Receiver {
            inner: self.inner.clone(),
            seen: state.revision,
        }
    }
    pub fn send_replace(&self, value: T) -> T {
        let (old, waiters) = {
            let mut state = lock(&self.inner.state);
            state.revision = state.revision.wrapping_add(1);
            (
                std::mem::replace(&mut state.value, value),
                std::mem::take(&mut state.waiters),
            )
        };
        wake(waiters);
        old
    }
}
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        lock(&self.inner.state).senders += 1;
        Self {
            inner: self.inner.clone(),
        }
    }
}
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waiters = {
            let mut state = lock(&self.inner.state);
            state.senders -= 1;
            if state.senders == 0 {
                std::mem::take(&mut state.waiters)
            } else {
                BTreeMap::new()
            }
        };
        wake(waiters);
    }
}
impl<T> Receiver<T> {
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref(lock(&self.inner.state))
    }
    pub fn borrow_and_update(&mut self) -> Ref<'_, T> {
        let state = lock(&self.inner.state);
        self.seen = state.revision;
        Ref(state)
    }
    pub fn has_changed(&self) -> Result<bool, RecvError> {
        let state = lock(&self.inner.state);
        if state.senders == 0 {
            Err(RecvError)
        } else {
            Ok(state.revision != self.seen)
        }
    }
    pub fn changed(&mut self) -> Changed<'_, T> {
        Changed {
            receiver: self,
            registration: None,
        }
    }
}
impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            seen: self.seen,
        }
    }
}
pub struct Changed<'a, T> {
    receiver: &'a mut Receiver<T>,
    registration: Option<u64>,
}
impl<T> Future for Changed<'_, T> {
    type Output = Result<(), RecvError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let waker = cx.waker().clone();
        let (result, old_waker) = {
            let mut state = lock(&this.receiver.inner.state);
            if state.revision != this.receiver.seen {
                this.receiver.seen = state.revision;
                (
                    Poll::Ready(Ok(())),
                    this.registration
                        .take()
                        .and_then(|id| state.waiters.remove(&id)),
                )
            } else if state.senders == 0 {
                (
                    Poll::Ready(Err(RecvError)),
                    this.registration
                        .take()
                        .and_then(|id| state.waiters.remove(&id)),
                )
            } else {
                let id = *this.registration.get_or_insert_with(|| {
                    let id = state.next_waiter;
                    state.next_waiter = state
                        .next_waiter
                        .checked_add(1)
                        .expect("watch waiter IDs exhausted");
                    id
                });
                (Poll::Pending, state.waiters.insert(id, waker))
            }
        };
        drop(old_waker); // Never drop/wake executor-owned data under the mutex.
        result
    }
}
impl<T> Drop for Changed<'_, T> {
    fn drop(&mut self) {
        if let Some(id) = self.registration.take() {
            let old = { lock(&self.receiver.inner.state).waiters.remove(&id) };
            drop(old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Wake,
    };
    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn cancelled_wait_removes_registration_and_publish_wakes_every_remaining_reader() {
        let (tx, mut a) = channel(0);
        let mut b = a.clone();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);
        let mut first = Box::pin(a.changed());
        let mut second = Box::pin(b.changed());
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        drop(first);
        assert_eq!(lock(&tx.inner.state).waiters.len(), 1);
        tx.send_replace(42);
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert_eq!(second.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        drop(second);
        assert_eq!(*b.borrow(), 42);
        assert_eq!(lock(&tx.inner.state).waiters.len(), 0);
    }
    #[test]
    fn last_sender_closure_preserves_unread_final_value() {
        let (tx, mut rx) = channel(None);
        let other = tx.clone();
        drop(tx);
        assert_eq!(rx.has_changed(), Ok(false));
        other.send_replace(Some(7));
        drop(other);
        assert!(rx.has_changed().is_err());
        assert_eq!(*rx.borrow(), Some(7));
        crate::threads::block_on(async {
            assert!(rx.changed().await.is_ok());
            assert!(rx.changed().await.is_err());
        });
    }
    #[test]
    fn repeated_publish_races_do_not_lose_completion_wakeups() {
        for _ in 0..100 {
            let (tx, mut rx) = channel(false);
            let publisher = std::thread::spawn(move || {
                tx.send_replace(true);
            });
            crate::threads::block_on(async {
                while !*rx.borrow_and_update() {
                    let _ = rx.changed().await;
                }
            });
            publisher.join().unwrap();
        }
    }
}
