//! Internal instrumentation; disabled builds use zero-sized no-op guards.
#[cfg(feature = "metrics")]
pub(crate) use crate::metrics::{
    accepted, reply, ActorObservation, Mailbox, MessageObservation, RequestObservation,
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
}
#[cfg(not(feature = "metrics"))]
pub(crate) use disabled::*;
