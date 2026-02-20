use std::thread::JoinHandle;

use crate::message::Message;

use super::actor::{Actor, Context, Handler};

pub fn spawn_listener<A, M, I>(ctx: Context<A>, stream: I) -> JoinHandle<()>
where
    A: Actor + Handler<M>,
    M: Message,
    I: IntoIterator<Item = M>,
    <I as IntoIterator>::IntoIter: std::marker::Send + 'static,
{
    let mut iter = stream.into_iter();
    let cancellation_token = ctx.cancellation_token();
    let join_handle = spawned_rt::threads::spawn(move || loop {
        match iter.next() {
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
        if cancellation_token.is_cancelled() {
            tracing::trace!("Actor stopped");
            break;
        }
    });
    join_handle
}
