use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use spawned_rt::threads::{self as rt, CancellationToken, JoinHandle};

use super::{Actor, ActorRef};

pub struct TimerHandle {
    pub join_handle: JoinHandle<()>,
    pub cancellation_token: CancellationToken,
}

/// Sends a message after a given period to the specified Actor.
///
/// The timer respects both its own cancellation token and the Actor's
/// cancellation token. If either is cancelled, the timer wakes up immediately
/// and exits without sending the message.
pub fn send_after<T>(period: Duration, mut handle: ActorRef<T>, message: T::Message) -> TimerHandle
where
    T: Actor + 'static,
{
    let cancellation_token = CancellationToken::new();
    let timer_token = cancellation_token.clone();
    let actor_token = handle.cancellation_token();

    // Channel to wake the timer thread on cancellation
    let (wake_tx, wake_rx) = mpsc::channel::<()>();

    // Register wake-up on timer cancellation
    let wake_tx1 = wake_tx.clone();
    timer_token.on_cancel(Box::new(move || {
        let _ = wake_tx1.send(());
    }));

    // Register wake-up on actor cancellation
    actor_token.on_cancel(Box::new(move || {
        let _ = wake_tx.send(());
    }));

    let join_handle = rt::spawn(move || {
        match wake_rx.recv_timeout(period) {
            Err(RecvTimeoutError::Timeout) => {
                // Timer expired - send if still valid
                if !timer_token.is_cancelled() && !actor_token.is_cancelled() {
                    let _ = handle.send(message);
                }
            }
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                // Woken early by cancellation - exit without sending
            }
        }
    });

    TimerHandle {
        join_handle,
        cancellation_token,
    }
}

/// Sends a message to the specified Actor repeatedly at the given interval.
///
/// The timer respects both its own cancellation token and the Actor's
/// cancellation token. If either is cancelled, the timer wakes up immediately
/// and exits.
pub fn send_interval<T>(
    period: Duration,
    mut handle: ActorRef<T>,
    message: T::Message,
) -> TimerHandle
where
    T: Actor + 'static,
{
    let cancellation_token = CancellationToken::new();
    let timer_token = cancellation_token.clone();
    let actor_token = handle.cancellation_token();

    // Channel to wake the timer thread on cancellation
    let (wake_tx, wake_rx) = mpsc::channel::<()>();

    // Register wake-up on timer cancellation
    let wake_tx1 = wake_tx.clone();
    timer_token.on_cancel(Box::new(move || {
        let _ = wake_tx1.send(());
    }));

    // Register wake-up on actor cancellation
    actor_token.on_cancel(Box::new(move || {
        let _ = wake_tx.send(());
    }));

    let join_handle = rt::spawn(move || {
        while let Err(RecvTimeoutError::Timeout) = wake_rx.recv_timeout(period) {
            // Timer expired - send if still valid
            if timer_token.is_cancelled() || actor_token.is_cancelled() {
                break;
            }
            let _ = handle.send(message.clone());
        }
        // If we exit the loop via Ok(()) or Disconnected, cancellation occurred
    });

    TimerHandle {
        join_handle,
        cancellation_token,
    }
}
