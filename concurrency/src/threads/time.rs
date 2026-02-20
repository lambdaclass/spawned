use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use spawned_rt::threads::{self as rt, CancellationToken, JoinHandle};

use super::actor::{Actor, Context, Handler};
use crate::message::Message;

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
    let timer_token = cancellation_token.clone();
    let actor_token = ctx.cancellation_token();

    let (wake_tx, wake_rx) = mpsc::channel::<()>();

    let wake_tx1 = wake_tx.clone();
    timer_token.on_cancel(Box::new(move || {
        let _ = wake_tx1.send(());
    }));

    actor_token.on_cancel(Box::new(move || {
        let _ = wake_tx.send(());
    }));

    let join_handle = rt::spawn(move || {
        match wake_rx.recv_timeout(period) {
            Err(RecvTimeoutError::Timeout) => {
                if !timer_token.is_cancelled() && !actor_token.is_cancelled() {
                    let _ = ctx.send(msg);
                }
            }
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
        }
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
    let timer_token = cancellation_token.clone();
    let actor_token = ctx.cancellation_token();

    let (wake_tx, wake_rx) = mpsc::channel::<()>();

    let wake_tx1 = wake_tx.clone();
    timer_token.on_cancel(Box::new(move || {
        let _ = wake_tx1.send(());
    }));

    actor_token.on_cancel(Box::new(move || {
        let _ = wake_tx.send(());
    }));

    let join_handle = rt::spawn(move || {
        while let Err(RecvTimeoutError::Timeout) = wake_rx.recv_timeout(period) {
            if timer_token.is_cancelled() || actor_token.is_cancelled() {
                break;
            }
            let _ = ctx.send(msg.clone());
        }
    });

    TimerHandle {
        join_handle,
        cancellation_token,
    }
}
