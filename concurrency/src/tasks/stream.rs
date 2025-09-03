use crate::tasks::{GenServer, GenServerHandle};
use futures::{future::select, Stream, StreamExt};
use spawned_rt::tasks::JoinHandle;

/// Spawns a listener that listens to a stream and sends messages to a GenServer.
///
/// Items sent through the stream are required to be wrapped in a Result type.
///
/// This function returns a handle to the spawned task and a cancellation token
/// to stop it.
pub fn spawn_listener<T, F, S, I, E>(
    mut handle: GenServerHandle<T>,
    message_builder: F,
    mut stream: S,
) -> JoinHandle<()>
where
    T: GenServer + 'static,
    F: Fn(I) -> T::CastMsg + Send + 'static + std::marker::Sync,
    I: Send,
    E: std::fmt::Debug + Send,
    S: Unpin + Send + Stream<Item = Result<I, E>> + 'static,
{
    let cancelation_token = handle.cancellation_token();
    let join_handle = spawned_rt::tasks::spawn(async move {
        let result = select(
            Box::pin(cancelation_token.cancelled()),
            Box::pin(async {
                loop {
                    match stream.next().await {
                        Some(Ok(i)) => match handle.cast(message_builder(i)).await {
                            Ok(_) => tracing::trace!("Message sent successfully"),
                            Err(e) => {
                                tracing::error!("Failed to send message: {e:?}");
                                break;
                            }
                        },
                        Some(Err(e)) => {
                            tracing::trace!("Error in stream: {e:?}");
                        }
                        None => {
                            tracing::trace!("Stream finished");
                            break;
                        }
                    }
                }
            }),
        )
        .await;
        match result {
            futures::future::Either::Left(_) => tracing::trace!("GenServer stopped"),
            futures::future::Either::Right(_) => (), // Stream finished or errored out
        }
    });
    join_handle
}
