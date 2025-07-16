use futures::future::select;
use std::time::Duration;

use spawned_rt::tasks::{self as rt, CancellationToken, JoinHandle};

use super::{GenServer, GenServerHandle};

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
        let cancel_conditions = select(
            Box::pin(cloned_token.cancelled()),
            Box::pin(gen_server_cancellation_token.cancelled()),
        );

        let _ = select(
            cancel_conditions,
            Box::pin(async {
                rt::sleep(period).await;
                let _ = handle.cast(message.clone()).await;
            }),
        )
        .await;
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
{
    let cancellation_token = CancellationToken::new();
    let cloned_token = cancellation_token.clone();
    let gen_server_cancellation_token = handle.cancellation_token();
    let join_handle = rt::spawn(async move {
        loop {
            // Timer action is ignored if it was either cancelled or the associated GenServer is no longer running.
            let cancel_conditions = select(
                Box::pin(cloned_token.cancelled()),
                Box::pin(gen_server_cancellation_token.cancelled()),
            );

            let result = select(
                Box::pin(cancel_conditions),
                Box::pin(async {
                    rt::sleep(period).await;
                    let _ = handle.cast(message.clone()).await;
                }),
            )
            .await;
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
