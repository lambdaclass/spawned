use crate::tasks::{GenServer, GenServerHandle};
use futures::{Stream, StreamExt};
use spawned_rt::tasks::{CancellationToken, JoinHandle};

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
) -> (JoinHandle<()>, CancellationToken)
where
    T: GenServer + 'static,
    F: Fn(I) -> T::CastMsg + Send + 'static,
    I: Send,
    E: std::fmt::Debug + Send,
    S: Unpin + Send + Stream<Item = Result<I, E>> + 'static,
{
    let cancelation_token = CancellationToken::new();
    let cloned_token = cancelation_token.clone();
    let join_handle = spawned_rt::tasks::spawn(async move {
        loop {
            if cloned_token.is_cancelled() {
                tracing::trace!("Received signal to stop listener, stopping");
                break;
            }

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
    (join_handle, cancelation_token)
}
