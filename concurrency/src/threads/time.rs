use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use spawned_rt::threads::{self as rt, CancelRegistration, CancellationToken, JoinHandle};

use super::actor::{Actor, Context, Handler};
use crate::message::Message;

/// `spawned_timers_alive`: held by the timer thread until it ends.
type TimerAlive = crate::instrumentation::Live<crate::instrumentation::gauge::Timers>;

/// Handle returned by [`send_after`] and [`send_interval`].
///
/// Cancel the timer by calling `timer.cancellation_token.cancel()`.
/// Timers are also automatically cancelled when the actor stops.
pub struct TimerHandle {
    #[allow(dead_code)]
    join_handle: JoinHandle<()>,
    pub cancellation_token: CancellationToken,
}

/// A timer thread's wake-up: either token's cancellation sends on the
/// channel. The thread holds both registrations and drops them when it ends,
/// so the actor's long-lived token does not keep a callback per timer that
/// has already fired or been cancelled.
fn wake_on_cancel(
    timer: &CancellationToken,
    actor: &CancellationToken,
) -> (mpsc::Receiver<()>, [CancelRegistration; 2]) {
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let timer_tx = wake_tx.clone();
    let registrations = [
        timer.register_on_cancel(Box::new(move || {
            let _ = timer_tx.send(());
        })),
        actor.register_on_cancel(Box::new(move || {
            let _ = wake_tx.send(());
        })),
    ];
    (wake_rx, registrations)
}

/// Send a single message to an actor after a delay.
pub fn send_after<A, M>(period: Duration, ctx: Context<A>, msg: M) -> TimerHandle
where
    A: Actor + Handler<M>,
    M: Message,
{
    let cancellation_token = CancellationToken::new();
    let timer_token = cancellation_token.clone();
    let actor_token = ctx.cancellation_token();

    let (wake_rx, registrations) = wake_on_cancel(&timer_token, &actor_token);
    let alive = TimerAlive::default();

    let join_handle = rt::spawn(move || {
        let _registrations = registrations;
        let _alive = alive;
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
    let timer_token = cancellation_token.clone();
    let actor_token = ctx.cancellation_token();

    let (wake_rx, registrations) = wake_on_cancel(&timer_token, &actor_token);
    let alive = TimerAlive::default();

    let join_handle = rt::spawn(move || {
        let _registrations = registrations;
        let _alive = alive;
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
