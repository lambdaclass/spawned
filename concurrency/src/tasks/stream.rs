use futures::{Stream, StreamExt};
use spawned_rt::tasks::{
    mpsc::{Receiver, UnboundedReceiver},
    ReceiverStream, UnboundedReceiverStream,
};

use crate::tasks::{GenServer, GenServerHandle};

/// Converts an unbounded receiver into a stream that can be used with async tasks.
///
/// This function is useful in conjunction with the [`spawn_listener`] function.
pub fn unbounded_receiver_to_stream<T>(receiver: UnboundedReceiver<T>) -> UnboundedReceiverStream<T>
where
    T: 'static + Clone + Send,
{
    UnboundedReceiverStream::new(receiver)
}

/// Converts a (bounded) receiver into a stream that can be used with async tasks.
///
/// This function is useful in conjunction with the [`spawn_listener`] function.
pub fn receiver_to_stream<T>(receiver: Receiver<T>) -> ReceiverStream<T>
where
    T: 'static + Clone + Send,
{
    ReceiverStream::new(receiver)
}

/// Spawns a listener that listens to a stream and sends messages to a GenServer
pub fn spawn_listener<T, F, S, I>(mut handle: GenServerHandle<T>, message_builder: F, mut stream: S)
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
                }
            }
        }
    });
}
