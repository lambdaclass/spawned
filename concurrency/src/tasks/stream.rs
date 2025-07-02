use crate::tasks::{GenServer, GenServerHandle};
use futures::{Stream, StreamExt};

/// Spawns a listener that listens to a stream and sends messages to a GenServer
/// Items sent through the stream are required to be wrapped in a Result type.
pub fn spawn_listener<T, F, S, I, E>(
    mut handle: GenServerHandle<T>,
    message_builder: F,
    mut stream: S,
) where
    T: GenServer + 'static,
    F: Fn(I) -> T::CastMsg + Send + 'static,
    I: Send,
    E: std::fmt::Debug + Send,
    S: Unpin + Send + Stream<Item = Result<I, E>> + 'static,
{
    spawned_rt::tasks::spawn(async move {
        loop {
            match stream.next().await {
                Some(res) => match res {
                    Ok(i) => {
                        let _ = handle.cast(message_builder(i)).await;
                    }
                    Err(e) => {
                        tracing::trace!("Received Error in msg {e:?}");
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
}
