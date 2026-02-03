use futures::future::select;
use std::time::Duration;

use spawned_rt::tasks::{self as rt, CancellationToken, JoinHandle};

use super::{Actor, ActorRef};
use core::pin::pin;

pub struct TimerHandle {
    pub join_handle: JoinHandle<()>,
    pub cancellation_token: CancellationToken,
}

// Sends a message after a given period to the specified Actor. The task terminates
// once the send has completed
pub fn send_after<T>(period: Duration, mut handle: ActorRef<T>, message: T::Message) -> TimerHandle
where
    T: Actor + 'static,
{
    let cancellation_token = CancellationToken::new();
    let cloned_token = cancellation_token.clone();
    let actor_cancellation_token = handle.cancellation_token();
    let join_handle = rt::spawn(async move {
        // Timer action is ignored if it was either cancelled or the associated Actor is no longer running.
        let cancel_token_fut = pin!(cloned_token.cancelled());
        let actor_cancel_fut = pin!(actor_cancellation_token.cancelled());
        let cancel_conditions = select(cancel_token_fut, actor_cancel_fut);

        let async_block = pin!(async {
            rt::sleep(period).await;
            let _ = handle.send(message.clone()).await;
        });
        let _ = select(cancel_conditions, async_block).await;
    });
    TimerHandle {
        join_handle,
        cancellation_token,
    }
}

// Sends a message to the specified Actor repeatedly after `Time` milliseconds.
pub fn send_interval<T>(
    period: Duration,
    mut handle: ActorRef<T>,
    message: T::Message,
) -> TimerHandle
where
    T: Actor + 'static,
{
    let cancellation_token = CancellationToken::new();
    let cloned_token = cancellation_token.clone();
    let actor_cancellation_token = handle.cancellation_token();
    let join_handle = rt::spawn(async move {
        loop {
            // Timer action is ignored if it was either cancelled or the associated Actor is no longer running.
            let cancel_token_fut = pin!(cloned_token.cancelled());
            let actor_cancel_fut = pin!(actor_cancellation_token.cancelled());
            let cancel_conditions = select(cancel_token_fut, actor_cancel_fut);

            let async_block = pin!(async {
                rt::sleep(period).await;
                let _ = handle.send(message.clone()).await;
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
