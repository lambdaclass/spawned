use crate::threads::{GenServer, GenServerHandle};
use std::thread;

use futures::{Stream, StreamExt}; // StreamExt gives us `next`
use std::iter;

/// Utility function to convert asynchronous streams into synchronous iterators.
///
/// This function is useful when used in conjunction with `spawn_listener`
pub fn stream_to_iter<S, T>(mut stream: S) -> impl Iterator<Item = T>
where
    S: Stream<Item = T> + Unpin,
{
    iter::from_fn(move || futures::executor::block_on(stream.next()))
}

/// Spawns a listener that iterates over a stream and sends messages to a GenServer.
///
/// Items sent through the iterator are required to be wrapped in a Result type.
pub fn spawn_listener<T, F, S, I, E>(
    mut handle: GenServerHandle<T>,
    message_builder: F,
    iterable: S,
) where
    T: GenServer + 'static,
    F: Fn(I) -> T::CastMsg + Send + 'static,
    I: Send + 'static,
    E: std::fmt::Debug + Send + 'static,
    S: Iterator<Item = Result<I, E>> + Send + 'static,
{
    thread::spawn(move || {
        for res in iterable {
            match res {
                Ok(i) => {
                    let _ = handle.cast(message_builder(i));
                }
                Err(err) => {
                    tracing::error!("Error in stream: {:?}", err);
                    break;
                }
            }
        }
        tracing::trace!("Stream finished");
    });
}
