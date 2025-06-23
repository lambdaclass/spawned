use std::time::Duration;

use spawned_rt::threads::{self as rt, CancellationToken, JoinHandle};

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
) -> JoinHandle<()>
where
    T: GenServer + 'static,
{
    rt::spawn(move || {
        rt::sleep(period);
        let _ = handle.cast(message);
    })
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
    let mut cloned_token = cancellation_token.clone();
    let join_handle = rt::spawn(move || loop {
        rt::sleep(period);
        if cloned_token.is_cancelled() {
            break;
        } else {
            let _ = handle.cast(message.clone());
        };
    });
    TimerHandle {
        join_handle,
        cancellation_token,
    }
}
