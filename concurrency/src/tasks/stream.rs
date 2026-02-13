use crate::message::Message;
use futures::{future::select, Stream, StreamExt};
use spawned_rt::tasks::JoinHandle;

use super::actor::{Actor, Context, Handler};

pub fn spawn_listener<A, M, S>(ctx: Context<A>, stream: S) -> JoinHandle<()>
where
    A: Actor + Handler<M>,
    M: Message,
    S: Send + Stream<Item = M> + 'static,
{
    let cancellation_token = ctx.cancellation_token();
    let join_handle = spawned_rt::tasks::spawn(async move {
        let mut pinned_stream = core::pin::pin!(stream);
        let is_cancelled = core::pin::pin!(cancellation_token.cancelled());
        let listener_loop = core::pin::pin!(async {
            loop {
                match pinned_stream.next().await {
                    Some(msg) => match ctx.send(msg) {
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
            futures::future::Either::Right(_) => (),
        }
    });
    join_handle
}
