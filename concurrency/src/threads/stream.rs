use crate::threads::{GenServer, GenServerHandle};
use std::thread;

use futures::{Stream, StreamExt};
use std::iter;

/// Spawns a listener that iterates over an iterable and sends messages to a GenServer.
///
/// Items sent through the iterator are required to be wrapped in a Result type.
pub fn spawn_listener_from_iter<T, F, S, I, E>(
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

/// Spawns a listener that listens to a stream and sends messages to a GenServer.
///
/// Items sent through the stream are required to be wrapped in a Result type.
pub fn spawn_listener<T, F, S, I, E>(
    mut handle: GenServerHandle<T>,
    message_builder: F,
    mut stream: S,
) where
    T: GenServer + 'static,
    F: Fn(I) -> T::CastMsg + Send + 'static,
    I: Send + 'static,
    E: std::fmt::Debug + Send + 'static,
    S: Unpin + Send + Stream<Item = Result<I, E>> + 'static,
{
    // Convert the stream into an iterator that blocks on each item.
    let iterable = iter::from_fn(move || futures::executor::block_on(stream.next()));
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
