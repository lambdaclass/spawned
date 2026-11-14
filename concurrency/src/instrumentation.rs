//! Internal instrumentation; disabled builds use zero-sized no-op guards.
#[cfg(feature = "metrics")]
pub(crate) use crate::metrics::{
    accepted, hooks, reply, ActorObservation, Mailbox, MessageObservation, RequestObservation,
};

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
}
#[cfg(not(feature = "metrics"))]
pub(crate) use disabled::*;
