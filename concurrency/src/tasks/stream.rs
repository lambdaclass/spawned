use futures::{Stream, StreamExt};

use crate::tasks::{GenServer, GenServerHandle};

pub fn spawn_listener<T, F, S, I>(
    mut handle: GenServerHandle<T>,
    message_builder: F,
    mut stream: S)
where
    T: GenServer + 'static,
    F: Fn(I) -> T::CastMsg + Send + 'static,
    I: Send,
    S: Unpin + Send + Stream<Item = Result<I, T::Error>> + 'static,
{
    spawned_rt::tasks::spawn(async move {
        loop {
            match stream.next().await {
                Some(Ok(item)) => {
                    let _ = handle.cast(message_builder(item)).await;
                }
                Some(Err(e)) => {
                    tracing::trace!("Received Error in msg {e:?}");
                    break;
                }
                None => {
                    tracing::trace!("Stream finnished");
                    break;
                },
            }
        }
    });
}