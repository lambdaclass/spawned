use crate::threads::{GenServer, GenServerHandle};
use futures::Stream;

/// Spawns a listener that listens to a stream and sends messages to a GenServer.
///
/// Items sent through the stream are required to be wrapped in a Result type.
pub fn spawn_listener<T, F, S, I, E>(_handle: GenServerHandle<T>, _message_builder: F, _stream: S)
where
    T: GenServer + 'static,
    F: Fn(I) -> T::CastMsg + Send + 'static,
    I: Send + 'static,
    E: std::fmt::Debug + Send + 'static,
    S: Unpin + Send + Stream<Item = Result<I, E>> + 'static,
{
    unimplemented!("Unsupported function in threads mode")
}
