use futures::future::select;
use std::time::Duration;

use spawned_rt::tasks::{self as rt, CancellationToken, JoinHandle};

use super::{GenServer, GenServerHandle};
use core::pin::pin;

pub struct TimerHandle {
    pub join_handle: JoinHandle<()>,
    pub cancellation_token: CancellationToken,
}

// Sends a message after a given period to the specified GenServer. The task terminates
// once the send has completed
pub fn send_after<T>(
    period: Duration,
    mut handle: GenServerHandle<T>,
    message: T::CastMsg,
) -> TimerHandle
where
    T: GenServer + 'static,
{
    let cancellation_token = CancellationToken::new();
    let cloned_token = cancellation_token.clone();
    let gen_server_cancellation_token = handle.cancellation_token();
    let join_handle = rt::spawn(async move {
        // Timer action is ignored if it was either cancelled or the associated GenServer is no longer running.
        let cancel_token_fut = pin!(cloned_token.cancelled());
        let genserver_cancel_fut = pin!(gen_server_cancellation_token.cancelled());
        let cancel_conditions = select(cancel_token_fut, genserver_cancel_fut);

        let async_block = pin!(async {
            rt::sleep(period).await;
            let _ = handle.cast(message).await;
        });
        let _ = select(cancel_conditions, async_block).await;
    });
    TimerHandle {
        join_handle,
        cancellation_token,
    }
}

// Sends a message to the specified GenServe repeatedly after `Time` milliseconds.
pub fn send_interval<T>(
    period: Duration,
    mut handle: GenServerHandle<T>,
    message: T::CastMsg,
) -> TimerHandle
where
    T: GenServer + 'static,
    T::CastMsg: Clone,
{
    let cancellation_token = CancellationToken::new();
    let cloned_token = cancellation_token.clone();
    let gen_server_cancellation_token = handle.cancellation_token();
    let join_handle = rt::spawn(async move {
        loop {
            // Timer action is ignored if it was either cancelled or the associated GenServer is no longer running.
            let cancel_token_fut = pin!(cloned_token.cancelled());
            let genserver_cancel_fut = pin!(gen_server_cancellation_token.cancelled());
            let cancel_conditions = select(cancel_token_fut, genserver_cancel_fut);

            let message_clone = message.clone();
            let handle_reference = &mut handle;
            let async_block = pin!(async move {
                rt::sleep(period).await;
                let _ = handle_reference.cast(message_clone).await;
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
