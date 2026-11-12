//! `sleep_until` and `interval` on virtual time (determinism D3), with
//! Tokio's semantics: the first tick completes at once, and a late tick
//! follows the [`MissedTickBehavior`].

use super::{current_or_ambient, lock, Instant, Sleep};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

/// Completes once virtual time reaches `deadline` (yields once if it already
/// has, as a Tokio timer does).
pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep {
        shared: current_or_ambient(),
        deadline: deadline.0,
        key: None,
        yielded: false,
    }
}

/// What an [`Interval`] does with ticks it could not deliver on time
/// (Tokio's enum, same default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MissedTickBehavior {
    /// Deliver the missed ticks back to back, then keep the original
    /// schedule.
    #[default]
    Burst,
    /// Schedule the next tick one period after the late one.
    Delay,
    /// Skip the missed ticks and keep to multiples of the period from the
    /// original start.
    Skip,
}

/// How late a tick may be before [`MissedTickBehavior`] applies (Tokio's).
const LATE: Duration = Duration::from_millis(5);

/// Ticks every `period` of virtual time; see [`interval`].
#[derive(Debug)]
pub struct Interval {
    next: Duration,
    period: Duration,
    behavior: MissedTickBehavior,
    sleep: Option<Sleep>,
}

/// An [`Interval`] whose first tick completes immediately.
///
/// # Panics
/// If `period` is zero, as Tokio's does.
pub fn interval(period: Duration) -> Interval {
    interval_at(Instant::now(), period)
}

/// An [`Interval`] whose first tick completes at `start`.
///
/// # Panics
/// If `period` is zero, as Tokio's does.
pub fn interval_at(start: Instant, period: Duration) -> Interval {
    assert!(period > Duration::ZERO, "`period` must be non-zero.");
    Interval {
        next: start.0,
        period,
        behavior: MissedTickBehavior::default(),
        sleep: None,
    }
}

impl Interval {
    /// Completes at the next tick and returns its scheduled instant.
    pub async fn tick(&mut self) -> Instant {
        std::future::poll_fn(|cx| self.poll_tick(cx)).await
    }

    /// Poll for the next tick.
    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        let next = self.next;
        let sleep = self.sleep.get_or_insert_with(|| sleep_until(Instant(next)));
        if Pin::new(&mut *sleep).poll(cx).is_pending() {
            return Poll::Pending;
        }
        let now = lock(&sleep.shared.state).now;
        self.sleep = None;
        let fired = self.next;
        // As Tokio: a tick up to 5 ms late keeps the schedule; later than
        // that, the missed-tick behavior decides.
        self.next = if now.saturating_sub(fired) > LATE {
            match self.behavior {
                MissedTickBehavior::Burst => fired + self.period,
                MissedTickBehavior::Delay => now + self.period,
                MissedTickBehavior::Skip => {
                    let late = now.saturating_sub(fired).as_nanos();
                    let period = self.period.as_nanos();
                    now + Duration::from_nanos(
                        u64::try_from(period - late % period).unwrap_or(u64::MAX),
                    )
                }
            }
        } else {
            fired + self.period
        };
        Poll::Ready(Instant(fired))
    }

    /// Restart the schedule: the next tick is one period from now.
    pub fn reset(&mut self) {
        self.next = Instant::now().0 + self.period;
        self.sleep = None;
    }

    /// Restart the schedule with the next tick right away.
    pub fn reset_immediately(&mut self) {
        self.next = Instant::now().0;
        self.sleep = None;
    }

    /// Restart the schedule with the next tick at `deadline`.
    pub fn reset_at(&mut self, deadline: Instant) {
        self.next = deadline.0;
        self.sleep = None;
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.behavior
    }

    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.behavior = behavior;
    }
}
