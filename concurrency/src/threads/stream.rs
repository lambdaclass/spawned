use std::thread::JoinHandle;

use crate::threads::{GenServer, GenServerHandle};

/// Spawns a listener that listens to a stream and sends messages to a GenServer.
///
/// Items sent through the stream are required to be wrapped in a Result type.
pub fn spawn_listener<T, I>(mut handle: GenServerHandle<T>, stream: I) -> JoinHandle<()>
where
    T: GenServer,
    I: IntoIterator<Item = T::CastMsg>,
    <I as IntoIterator>::IntoIter: std::marker::Send + 'static,
{
    let mut iter = stream.into_iter();
    let mut cancelation_token = handle.cancellation_token();
    let join_handle = spawned_rt::threads::spawn(move || loop {
        match iter.next() {
            Some(msg) => match handle.cast(msg) {
                Ok(_) => tracing::trace!("Message sent successfully"),
                Err(e) => {
                    tracing::error!("Failed to send message: {e:?}");
                    break;
                }
            },
            None => {
                tracing::trace!("Stream finished");
                break;
            }
        }
        if cancelation_token.is_cancelled() {
            tracing::trace!("GenServer stopped");
            break;
        }
    });
    join_handle
}
