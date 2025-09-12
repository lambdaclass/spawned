use std::net::SocketAddr;

use crate::tasks::{GenServer, GenServerHandle};
use futures::{future::select, Stream, StreamExt};
use spawned_rt::tasks::JoinHandle;

/// Spawns a listener that listens to a stream and sends messages to a GenServer.
///
/// Items sent through the stream are required to be wrapped in a Result type.
///
/// This function returns a handle to the spawned task and a cancellation token
/// to stop it.
pub fn spawn_listener<T, S>(mut handle: GenServerHandle<T>, stream: S) -> JoinHandle<()>
where
    T: GenServer,
    S: Send + Stream<Item = T::CastMsg> + 'static,
{
    let cancelation_token = handle.cancellation_token();
    let join_handle = spawned_rt::tasks::spawn(async move {
        let mut pinned_stream = core::pin::pin!(stream);
        let is_cancelled = core::pin::pin!(cancelation_token.cancelled());
        let listener_loop = core::pin::pin!(async {
            loop {
                match pinned_stream.next().await {
                    Some(msg) => match handle.cast(msg).await {
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

/// Spawns a task that listens to a stream of streams and spawns a GenServer that
/// handles messages from that stream.
/// Each GenServer is created using the provided factory function, and spawned
/// using [`GenServer::start`].
pub fn spawn_spawner<L, S, T, F>(listener: L, mut factory: F) -> JoinHandle<()>
where
    L: Send + Stream<Item = (S, SocketAddr)> + 'static,
    S: Send + Stream<Item = T::CastMsg> + 'static,
    T: GenServer + 'static,
    F: FnMut(SocketAddr) -> T + Send + 'static,
{
    spawned_rt::tasks::spawn(async move {
        let mut listener = core::pin::pin!(listener);
        // TODO: listener can't be a stream since tokio::net::TcpListener does not implement Stream
        while let Some((stream, addr)) = listener.next().await {
            let handle = factory(addr).start();
            // TODO: handle error
            spawn_listener(handle, stream).await.unwrap();
        }
    })
}
