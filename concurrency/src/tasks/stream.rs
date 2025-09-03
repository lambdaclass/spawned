use crate::tasks::{GenServer, GenServerHandle};
use futures::{future::select, Stream, StreamExt};
use spawned_rt::tasks::JoinHandle;

/// Spawns a listener that listens to a stream and sends messages to a GenServer.
///
/// Items sent through the stream are required to be wrapped in a Result type.
///
/// This function returns a handle to the spawned task and a cancellation token
/// to stop it.
pub fn spawn_listener<T, F, S, I>(
    mut handle: GenServerHandle<T>,
    message_builder: F,
    mut stream: S,
) -> JoinHandle<()>
where
    T: GenServer + 'static,
    F: Fn(I) -> T::CastMsg + Send + 'static + Sync,
    I: Send,
    S: Unpin + Send + Stream<Item = I> + 'static,
{
    let cancelation_token = handle.cancellation_token();
    let join_handle = spawned_rt::tasks::spawn(async move {
        let is_cancelled = core::pin::pin!(cancelation_token.cancelled());
        let listener_loop = core::pin::pin!(async {
            loop {
                match stream.next().await {
                    // Stream has a new valid Item
                    Some(i) => match handle.cast(message_builder(i)).await {
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
            futures::future::Either::Left(_) => tracing::trace!("GenServer stopped"),
            futures::future::Either::Right(_) => (), // Stream finished or errored out
        }
    });
    join_handle
}
