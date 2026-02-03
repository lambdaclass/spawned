use std::time::Duration;

use spawned_rt::threads::{self as rt, CancellationToken, JoinHandle};

use super::{Actor, ActorRef};

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
    let mut cloned_token = cancellation_token.clone();
    let mut actor_cancellation_token = handle.cancellation_token();
    let join_handle = rt::spawn(move || {
        rt::sleep(period);
        // Only send if neither the timer nor the actor has been cancelled
        if !cloned_token.is_cancelled() && !actor_cancellation_token.is_cancelled() {
            let _ = handle.send(message);
        };
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
    let mut cloned_token = cancellation_token.clone();
    let mut actor_cancellation_token = handle.cancellation_token();
    let join_handle = rt::spawn(move || loop {
        rt::sleep(period);
        // Stop if either the timer or the actor has been cancelled
        if cloned_token.is_cancelled() || actor_cancellation_token.is_cancelled() {
            break;
        } else {
            let _ = handle.send(message.clone());
        };
    });
    TimerHandle {
        join_handle,
        cancellation_token,
    }
}
