use futures::future::select;
use std::time::Duration;

use spawned_rt::tasks::{self as rt, CancellationToken, JoinHandle};

use super::actor::{Actor, Context, Handler};
use crate::message::Message;
use core::pin::pin;

pub struct TimerHandle {
    pub join_handle: JoinHandle<()>,
    pub cancellation_token: CancellationToken,
}

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
