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
    let join_handle = rt::spawn(async move {
        let _ = select(
            Box::pin(cloned_token.cancelled()),
            Box::pin(async {
                rt::sleep(period).await;
                let _ = handle.cast(message).await;
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
    T::CastMsg: Clone,
{
    let cancellation_token = CancellationToken::new();
    let cloned_token = cancellation_token.clone();
    let join_handle = rt::spawn(async move {
        loop {
            let result = select(
                Box::pin(cloned_token.cancelled()),
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
