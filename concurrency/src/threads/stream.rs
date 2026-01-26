use std::thread::JoinHandle;

use crate::threads::{Actor, ActorRef};

/// Spawns a listener that listens to a stream and sends messages to an Actor.
///
/// Items sent through the stream are required to be wrapped in a Result type.
pub fn spawn_listener<T, I>(mut handle: ActorRef<T>, stream: I) -> JoinHandle<()>
where
    T: Actor,
    I: IntoIterator<Item = T::Message>,
    <I as IntoIterator>::IntoIter: std::marker::Send + 'static,
{
    let mut iter = stream.into_iter();
    let mut cancelation_token = handle.cancellation_token();
    let join_handle = spawned_rt::threads::spawn(move || loop {
        match iter.next() {
            Some(msg) => match handle.send(msg) {
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
            tracing::trace!("Actor stopped");
            break;
        }
    });
    join_handle
}
