use crate::error::ActorError;
#[cfg(feature = "async")]
use crate::reply::{ReplyFuture, ReplyReceiver};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A cloned actor handle with an explicit timeout for generated protocol calls.
/// Create with `ActorRef::with_request_timeout`. Configure before converting to
/// a type-erased protocol reference; the underlying actor and mailbox are shared.
#[derive(Clone, Debug)]
pub struct RequestOptions<R> {
    actor: R,
    timeout: Duration,
}
impl<R> RequestOptions<R> {
    pub(crate) fn new(actor: R, timeout: Duration) -> Self {
        Self { actor, timeout }
    }
    /// Underlying handle for manual message APIs. Associated functions avoid
    /// shadowing protocol methods with the same name.
    pub fn actor_ref(options: &Self) -> &R {
        &options.actor
    }
    /// Timeout selected for generated request methods on this handle.
    pub fn timeout(options: &Self) -> Duration {
        options.timeout
    }
}

// ---------------------------------------------------------------------------
// Response<T> — unified wrapper for protocol request-response (tasks + threads)
//
// Async receiver variants compile only with the async feature. Native protocols
// retain the Ready response without compiling or depending on Tokio.
// ---------------------------------------------------------------------------

enum ResponseState<T> {
    /// The deadline is already in the receiver's slot, fixed at send.
    #[cfg(feature = "async")]
    Receiver(Pin<Box<ReplyFuture<T>>>),
    Ready(Result<T, ActorError>),
    Done,
}

/// Concrete wrapper for protocol request-response methods that works in both
/// `tasks` (async) and `threads` (blocking) modes.
///
/// - **Tasks mode**: wraps a [`ReplyReceiver`](crate::reply::ReplyReceiver);
///   `.await` returns `Result<T, ActorError>`. The request's deadline was fixed
///   when it was sent.
/// - **Threads mode**: wraps a pre-computed result; use `.unwrap()` / `.expect()` directly
///
/// Protocol methods return `Response<T>`:
/// ```ignore
/// #[protocol]
/// pub trait MyProtocol: Send + Sync {
///     fn query(&self) -> Response<String>;
/// }
///
/// // Tasks: actor.query().await.unwrap()
/// // Threads: actor.query().unwrap()
/// ```
pub struct Response<T>(
    ResponseState<T>,
    #[cfg(feature = "metrics")] Option<crate::instrumentation::RequestObservation>,
);

// The receiver is boxed and pinned, so `Response<T>` is `Unpin` whatever `T`
// is; `Future::poll` uses `self.get_mut()`.
impl<T> Unpin for Response<T> {}

impl<T> Response<T> {
    /// Create a `Response` from a pre-computed result.
    ///
    /// Used by the threads runtime where the request blocks at call time
    /// and the result is immediately available.
    pub fn ready(result: Result<T, ActorError>) -> Self {
        Self(
            ResponseState::Ready(result),
            #[cfg(feature = "metrics")]
            None,
        )
    }

    /// Extract the value, panicking on error.
    ///
    /// For ready (pre-computed) responses. Panics on pending responses —
    /// use `.await.unwrap()` in async contexts instead.
    pub fn unwrap(self) -> T {
        match self.0 {
            ResponseState::Ready(result) => result.unwrap(),
            #[cfg(feature = "async")]
            ResponseState::Receiver(_) => {
                panic!("called unwrap() on a pending Response; use .await in async contexts")
            }
            ResponseState::Done => panic!("Response already consumed"),
        }
    }

    /// Extract the value, panicking with a custom message on error.
    pub fn expect(self, msg: &str) -> T {
        match self.0 {
            ResponseState::Ready(result) => result.expect(msg),
            #[cfg(feature = "async")]
            ResponseState::Receiver(_) => {
                panic!("{msg}: called expect() on a pending Response; use .await in async contexts")
            }
            ResponseState::Done => panic!("{msg}: Response already consumed"),
        }
    }

    /// Returns `true` if the response contains `Ok`.
    /// Only meaningful for ready responses.
    /// Returns `false` for pending (`Receiver`) or consumed (`Done`) states.
    pub fn is_ok(&self) -> bool {
        matches!(&self.0, ResponseState::Ready(Ok(_)))
    }

    /// Returns `true` if the response contains `Err`.
    /// Only meaningful for ready responses.
    /// Returns `false` for pending (`Receiver`) or consumed (`Done`) states.
    pub fn is_err(&self) -> bool {
        matches!(&self.0, ResponseState::Ready(Err(_)))
    }

    /// Maps the inner value if the response is ready and `Ok`.
    ///
    /// Panics on pending or consumed responses.
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> Response<U> {
        match self.0 {
            ResponseState::Ready(result) => Response(
                ResponseState::Ready(result.map(f)),
                #[cfg(feature = "metrics")]
                None,
            ),
            #[cfg(feature = "async")]
            ResponseState::Receiver(_) => {
                panic!("called map() on a pending Response; use .await in async contexts")
            }
            ResponseState::Done => panic!("Response already consumed"),
        }
    }
}

#[cfg(feature = "async")]
impl<T> From<Result<ReplyReceiver<T>, ActorError>> for Response<T> {
    fn from(result: Result<ReplyReceiver<T>, ActorError>) -> Self {
        #[cfg(feature = "metrics")]
        let observation = crate::instrumentation::RequestObservation::new();
        match result {
            Ok(rx) => Self(
                ResponseState::Receiver(Box::pin(rx.into_timed_future())),
                #[cfg(feature = "metrics")]
                Some(observation),
            ),
            Err(e) => {
                let result = Err(e);
                #[cfg(feature = "metrics")]
                observation.finish(&result);
                Self(
                    ResponseState::Ready(result),
                    #[cfg(feature = "metrics")]
                    None,
                )
            }
        }
    }
}

impl<T: Send + 'static> Future for Response<T> {
    type Output = Result<T, ActorError>;

    fn poll(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        let result = match &mut this.0 {
            #[cfg(feature = "async")]
            ResponseState::Receiver(rx) => match rx.as_mut().poll(_cx) {
                std::task::Poll::Ready(result) => {
                    this.0 = ResponseState::Done;
                    std::task::Poll::Ready(result)
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
            ResponseState::Ready(_) => match std::mem::replace(&mut this.0, ResponseState::Done) {
                ResponseState::Ready(result) => std::task::Poll::Ready(result),
                _ => unreachable!(),
            },
            ResponseState::Done => panic!("Response polled after completion"),
        };
        #[cfg(feature = "metrics")]
        if let std::task::Poll::Ready(ref outcome) = result {
            if let Some(observation) = this.1.take() {
                observation.finish(outcome);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "async")]
    use crate::reply::Deadline;

    #[test]
    fn ready_ok_unwrap() {
        let r: Response<i32> = Response::ready(Ok(42));
        assert_eq!(r.unwrap(), 42);
    }

    #[test]
    fn ready_err_is_err() {
        let r: Response<i32> = Response::ready(Err(ActorError::ActorStopped));
        assert!(r.is_err());
    }

    #[test]
    #[should_panic(expected = "ActorStopped")]
    fn ready_err_unwrap_panics() {
        let r: Response<i32> = Response::ready(Err(ActorError::ActorStopped));
        r.unwrap();
    }

    #[test]
    #[cfg(feature = "async")]
    fn future_resolves_from_receiver() {
        let rt = spawned_rt::tasks::Runtime::new().unwrap();
        rt.block_on(async {
            let (reply, rx) = crate::reply::reply_pair::<i32>(Deadline::Never);
            let resp: Response<i32> = Response::from(Ok(rx));
            reply.reply(99);
            assert_eq!(resp.await.unwrap(), 99);
        });
    }

    #[test]
    #[cfg(feature = "async")]
    fn future_err_on_dropped_reply() {
        let rt = spawned_rt::tasks::Runtime::new().unwrap();
        rt.block_on(async {
            let (reply, rx) = crate::reply::reply_pair::<i32>(Deadline::Never);
            let resp: Response<i32> = Response::from(Ok(rx));
            drop(reply);
            assert!(matches!(resp.await, Err(ActorError::ReplyDropped)));
        });
    }

    #[test]
    fn map_transforms_value() {
        let r: Response<i32> = Response::ready(Ok(2));
        let mapped = r.map(|x| x * 3);
        assert_eq!(mapped.unwrap(), 6);
    }

    #[test]
    #[cfg(feature = "async")]
    fn receiver_times_out_at_its_send_deadline() {
        let rt = spawned_rt::tasks::Runtime::new().unwrap();
        rt.block_on(async {
            let (reply, rx) =
                crate::reply::reply_pair::<i32>(Deadline::after(Duration::from_millis(50)));
            let resp: Response<i32> = Response::from(Ok(rx));
            assert!(matches!(resp.await, Err(ActorError::RequestTimeout)));
            assert_eq!(reply.reply(1), crate::reply::Delivery::Expired);
        });
    }
}
