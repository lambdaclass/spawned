use futures::future::select;
use std::time::Duration;

use spawned_rt::tasks::{self as rt, CancellationToken, JoinHandle};

use super::actor::{Actor, Context, Handler};
use crate::message::Message;
use core::pin::pin;

/// Handle returned by [`send_after`] and [`send_interval`].
///
/// Cancel the timer by calling `timer.cancellation_token.cancel()`.
/// Timers are also automatically cancelled when the actor stops.
pub struct TimerHandle {
    pub join_handle: JoinHandle<()>,
    pub cancellation_token: CancellationToken,
}

/// Send a single message to an actor after a delay.
pub fn send_after<A, M>(period: Duration, ctx: Context<A>, msg: M) -> TimerHandle
where
    A: Actor + Handler<M>,
    M: Message,
{
    let cancellation_token = CancellationToken::new();
    let cloned_token = cancellation_token.clone();
    let actor_cancellation_token = ctx.cancellation_token();
    let join_handle = rt::spawn(async move {
        let cancel_token_fut = pin!(cloned_token.cancelled());
        let actor_cancel_fut = pin!(actor_cancellation_token.cancelled());
        let cancel_conditions = select(cancel_token_fut, actor_cancel_fut);

        let async_block = pin!(async {
            rt::sleep(period).await;
            let _ = ctx.send(msg);
        });
        let _ = select(cancel_conditions, async_block).await;
    });
    TimerHandle {
        join_handle,
        cancellation_token,
    }
}

/// Send a message to an actor repeatedly at a fixed interval.
///
/// The message type must implement `Clone` since a copy is sent on each tick.
/// For `#[protocol]`-generated messages, unit structs (no fields) derive `Clone`
/// automatically. For structs with fields, implement `Clone` manually on the
/// generated message struct (e.g., `impl Clone for my_protocol::MyMessage { .. }`).
pub fn send_interval<A, M>(period: Duration, ctx: Context<A>, msg: M) -> TimerHandle
where
    A: Actor + Handler<M>,
    M: Message + Clone,
{
    let cancellation_token = CancellationToken::new();
    let cloned_token = cancellation_token.clone();
    let actor_cancellation_token = ctx.cancellation_token();
    let join_handle = rt::spawn(async move {
        loop {
            let cancel_token_fut = pin!(cloned_token.cancelled());
            let actor_cancel_fut = pin!(actor_cancellation_token.cancelled());
            let cancel_conditions = select(cancel_token_fut, actor_cancel_fut);

            let msg_clone = msg.clone();
            let async_block = pin!(async {
                rt::sleep(period).await;
                let _ = ctx.send(msg_clone);
            });
            let result = select(cancel_conditions, async_block).await;
            match result {
                futures::future::Either::Left(_) => break,
                futures::future::Either::Right(_) => (),
            }
        }
    });
    TimerHandle {
        join_handle,
        cancellation_token,
    }
}
