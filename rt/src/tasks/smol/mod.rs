use smol::channel;
pub use smol::Executor as Runtime;
pub use smol::Task as JoinHandle;

use crate::tracing::init_tracing;
use std::{
    future::Future,
    num::{NonZero, NonZeroU64},
};

pub fn run<F: Future>(future: F) -> F::Output {
    init_tracing();

    smol::block_on(future)
}

pub fn block_on<F: Future>(future: F) -> F::Output {
    smol::block_on(future)
}

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (sender, receiver) = channel::bounded::<F::Output>(1);
    smol::spawn(async move { sender.send(future.await).await }).detach();
    smol::spawn(async move { receiver.recv().await.unwrap() })
}

pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    smol::unblock(f)
}

pub fn task_id() -> NonZeroU64 {
    NonZero::new(1).unwrap()
}
