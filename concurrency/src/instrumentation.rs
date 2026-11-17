//! Internal instrumentation; disabled builds use zero-sized no-op guards.

/// A field counting one live `G` structure (`gauge::*`) from construction to
/// drop; zero-sized, and a no-op without `metrics`.
pub(crate) type Live<G> = live::Live<G>;
#[cfg(feature = "metrics")]
pub(crate) use crate::metrics::{
    accepted, hooks, live, reply, ActorObservation, Mailbox, MessageObservation, RequestObservation,
};

/// Markers of the retained-structure gauges (`metrics::live`): which
/// `spawned_*` gauge a [`Live`](live::Live) field counts.
pub(crate) mod gauge {
    /// `reply_slots_alive`: request reply slots.
    pub(crate) enum ReplySlots {}
    /// `reply_tokens_alive`: `ReplyTo` tokens (deferred obligations).
    pub(crate) enum ReplyTokens {}
    /// `reply_registries_alive`: per-instance reply registries.
    pub(crate) enum ReplyRegistries {}
    /// `pending_replies_entries`: entries in every `PendingReplies`.
    pub(crate) enum PendingEntries {}
    /// `deadline_heap_entries`: per-instance deadline heap entries.
    pub(crate) enum DeadlineEntries {}
    /// `jobs_alive`: job states (runs, handles, held results).
    pub(crate) enum Jobs {}
    /// `monitors_alive`: entries in monitor tables.
    pub(crate) enum Monitors {}
    /// `link_records_alive`: link records.
    pub(crate) enum LinkRecords {}
    /// `registered_names`: names in the global registry.
    pub(crate) enum Names {}
    /// `timers_alive`: `send_after` / `send_interval` timers.
    pub(crate) enum Timers {}
}

#[cfg(not(feature = "metrics"))]
mod disabled {
    use crate::{ActorError, ExitReason};
    #[derive(Clone, Default)]
    pub(crate) struct Mailbox {
        _private: (),
    }
    pub(crate) struct ActorObservation;
    impl ActorObservation {
        #[inline]
        pub(crate) fn new() -> Self {
            Self
        }
        #[inline]
        pub(crate) fn finish(&mut self, _: &ExitReason) {}
    }
    pub(crate) struct RequestObservation;
    impl RequestObservation {
        #[inline]
        pub(crate) fn new() -> Self {
            Self
        }
        #[inline]
        pub(crate) fn finish<T>(self, _: &Result<T, ActorError>) {}
    }

    /// Deferred-reply and job accounting: no-ops without `metrics`.
    pub(crate) mod hooks {
        pub(crate) type DeferStamp = ();
        #[inline(always)]
        pub(crate) fn deferred() -> DeferStamp {}
        #[inline(always)]
        pub(crate) fn deferred_replied(_: &DeferStamp) {}
        #[inline(always)]
        pub(crate) fn left_open(_: &'static str, _: bool) {}
        #[inline(always)]
        pub(crate) fn job_started() {}
        #[inline(always)]
        pub(crate) fn job_ended() {}
        #[inline(always)]
        pub(crate) fn job_notified(_: &'static str) {}
        #[inline(always)]
        pub(crate) fn job_cancelled() {}
        #[inline(always)]
        pub(crate) fn job_fenced() {}
        #[inline(always)]
        pub(crate) fn job_rejected() {}
        #[inline(always)]
        pub(crate) fn job_emitted() {}
        #[inline(always)]
        pub(crate) fn jobs_unfinished(_: usize) {}
    }

    /// Retained-structure gauges: zero-sized, no-op guards without `metrics`.
    pub(crate) mod live {
        use std::marker::PhantomData;
        pub(crate) struct Live<G>(PhantomData<fn() -> G>);
        impl<G> Default for Live<G> {
            #[inline(always)]
            fn default() -> Self {
                Self(PhantomData)
            }
        }
        #[inline(always)]
        pub(crate) fn added<G>(_: usize) {}
        #[inline(always)]
        pub(crate) fn removed<G>(_: usize) {}
    }
}
#[cfg(not(feature = "metrics"))]
pub(crate) use disabled::*;
