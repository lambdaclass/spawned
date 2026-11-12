//! A `JoinSet` over simulated tasks (determinism D3), with Tokio's API.
//!
//! Tasks are spawned on the simulator, so their schedule is the seed's.
//! [`JoinSet::join_next`] returns a finished task's result; when several have
//! finished since the last call it returns the one with the smallest task id
//! (Tokio returns them in completion order), so the order is deterministic.

use super::{spawn, spawn_blocking, Id, JoinError, JoinHandle};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

/// A set of simulated tasks whose results are collected as they finish.
/// Dropping the set aborts every task still in it.
pub struct JoinSet<T> {
    tasks: BTreeMap<u64, JoinHandle<T>>,
}

/// Aborts one task of a [`JoinSet`] without removing it.
#[derive(Clone)]
pub struct AbortHandle {
    abort: Arc<dyn Fn() + Send + Sync>,
    finished: Arc<dyn Fn() -> bool + Send + Sync>,
    id: Id,
}

impl AbortHandle {
    pub fn abort(&self) {
        (self.abort)();
    }
    pub fn is_finished(&self) -> bool {
        (self.finished)()
    }
    pub fn id(&self) -> Id {
        self.id
    }
}

impl fmt::Debug for AbortHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("sim::AbortHandle")
            .field("id", &self.id)
            .finish()
    }
}

impl<T> fmt::Debug for JoinSet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("sim::JoinSet")
            .field("len", &self.tasks.len())
            .finish()
    }
}

impl<T> Default for JoinSet<T> {
    fn default() -> Self {
        Self {
            tasks: BTreeMap::new(),
        }
    }
}

impl<T> JoinSet<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Abort every task; their results (`Cancelled`, or a value if they
    /// had already finished) are still returned by `join_next`.
    pub fn abort_all(&mut self) {
        for handle in self.tasks.values() {
            handle.abort();
        }
    }

    /// Remove every task without aborting it.
    pub fn detach_all(&mut self) {
        self.tasks.clear();
    }
}

impl<T: Send + 'static> JoinSet<T> {
    fn insert(&mut self, handle: JoinHandle<T>) -> AbortHandle {
        let (id, join, shared) = (handle.id, handle.join.clone(), handle.shared.clone());
        let finished = join.clone();
        self.tasks.insert(id, handle);
        AbortHandle {
            abort: Arc::new(move || super::abort_task(&join, &shared, id)),
            finished: Arc::new(move || super::lock(&finished.inner).finished),
            id: Id(id),
        }
    }

    /// Spawn `future` on the simulator as a member of the set.
    pub fn spawn<F>(&mut self, future: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
    {
        self.insert(spawn(future))
    }

    /// Run `f` as a member of the set: inline, as one simulator step (see
    /// [`spawn_blocking`]).
    pub fn spawn_blocking<F>(&mut self, f: F) -> AbortHandle
    where
        F: FnOnce() -> T + Send + 'static,
    {
        self.insert(spawn_blocking(f))
    }

    /// The next finished task's result, or `None` if the set is empty.
    pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
        std::future::poll_fn(|cx| self.poll_join_next(cx)).await
    }

    /// [`JoinSet::join_next`] with the task's id.
    pub async fn join_next_with_id(&mut self) -> Option<Result<(Id, T), JoinError>> {
        std::future::poll_fn(|cx| {
            self.poll_next_entry(cx)
                .map(|entry| entry.map(|(id, result)| result.map(|value| (id, value))))
        })
        .await
    }

    /// Poll for the next finished task.
    pub fn poll_join_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, JoinError>>> {
        self.poll_next_entry(cx)
            .map(|entry| entry.map(|(_, result)| result))
    }

    /// Polls every member in task-id order (O(n) per wake; sets in tests
    /// are small).
    fn poll_next_entry(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<(Id, Result<T, JoinError>)>> {
        if self.tasks.is_empty() {
            return Poll::Ready(None);
        }
        let mut done = None;
        for (id, handle) in self.tasks.iter_mut() {
            if let Poll::Ready(result) = Pin::new(handle).poll(cx) {
                done = Some((*id, result));
                break;
            }
        }
        match done {
            Some((id, result)) => {
                self.tasks.remove(&id);
                Poll::Ready(Some((Id(id), result)))
            }
            None => Poll::Pending,
        }
    }

    /// A finished task's result without waiting, if there is one. Polls
    /// only a finished handle, so the wakers `join_next` registered on the
    /// others stay in place.
    pub fn try_join_next(&mut self) -> Option<Result<T, JoinError>> {
        let id = *self.tasks.iter().find(|(_, h)| h.is_finished())?.0;
        let mut handle = self.tasks.remove(&id)?;
        match Pin::new(&mut handle).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => Some(result),
            // Finished means its result is stored (or it was cancelled).
            Poll::Pending => None,
        }
    }

    /// Abort every task and wait for all of them to stop.
    pub async fn shutdown(&mut self) {
        self.abort_all();
        while self.join_next().await.is_some() {}
    }

    /// Wait for every task and return their values (in the order
    /// `join_next` yields them). A task that panicked resumes the panic, as
    /// Tokio does.
    pub async fn join_all(mut self) -> Vec<T> {
        let mut values = Vec::with_capacity(self.len());
        while let Some(result) = self.join_next().await {
            match result {
                Ok(value) => values.push(value),
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Err(error) => panic!("{error}"),
            }
        }
        values
    }
}

impl<T> Drop for JoinSet<T> {
    fn drop(&mut self) {
        self.abort_all();
    }
}

impl<T: Send + 'static, F: Future<Output = T> + Send + 'static> FromIterator<F> for JoinSet<T> {
    fn from_iter<I: IntoIterator<Item = F>>(iter: I) -> Self {
        let mut set = Self::new();
        for future in iter {
            set.spawn(future);
        }
        set
    }
}
