use crate::tasks::{Actor, ActorRef};
use futures::{future::select, Stream, StreamExt};
use spawned_rt::tasks::JoinHandle;

/// Spawns a listener that listens to a stream and sends messages to an Actor.
///
/// Items sent through the stream are required to be wrapped in a Result type.
///
/// This function returns a handle to the spawned task and a cancellation token
/// to stop it.
pub fn spawn_listener<T, S>(mut handle: ActorRef<T>, stream: S) -> JoinHandle<()>
where
    T: Actor,
    S: Send + Stream<Item = T::Message> + 'static,
{
    let cancellation_token = handle.cancellation_token();
    let join_handle = spawned_rt::tasks::spawn(async move {
        let mut pinned_stream = core::pin::pin!(stream);
        let is_cancelled = core::pin::pin!(cancellation_token.cancelled());
        let listener_loop = core::pin::pin!(async {
            loop {
                match pinned_stream.next().await {
                    Some(msg) => match handle.send(msg).await {
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
            }
        });
        match select(is_cancelled, listener_loop).await {
            futures::future::Either::Left(_) => tracing::trace!("Actor stopped"),
            futures::future::Either::Right(_) => (), // Stream finished or errored out
        }
    });
    join_handle
}
