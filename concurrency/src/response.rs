use crate::error::ActorError;
#[cfg(feature = "async")]
use spawned_rt::tasks::oneshot;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A cloned actor handle with an explicit timeout for generated protocol calls.
/// Create with `ActorRef::with_request_timeout`. Configure before converting to
/// a type-erased protocol reference; the underlying actor and mailbox are shared.
#[derive(Clone)]
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
    #[cfg(feature = "async")]
    Receiver(oneshot::Receiver<T>),
    #[cfg(feature = "async")]
    TimedReceiver(Pin<Box<dyn Future<Output = Result<T, ActorError>> + Send>>),
    Ready(Result<T, ActorError>),
    Done,
}

/// Concrete wrapper for protocol request-response methods that works in both
/// `tasks` (async) and `threads` (blocking) modes.
///
/// - **Tasks mode**: wraps a oneshot receiver; `.await` returns `Result<T, ActorError>`
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

// Safety: tokio::sync::oneshot::Receiver<T> implements Unpin, so pinning
// Response<T> is safe. This is required for the Future::poll implementation
// which uses self.get_mut().
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
            ResponseState::Receiver(_) | ResponseState::TimedReceiver(_) => {
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
            ResponseState::Receiver(_) | ResponseState::TimedReceiver(_) => {
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
            ResponseState::Receiver(_) | ResponseState::TimedReceiver(_) => {
                panic!("called map() on a pending Response; use .await in async contexts")
            }
            ResponseState::Done => panic!("Response already consumed"),
        }
    }
}

#[cfg(feature = "async")]
impl<T: Send + 'static> Response<T> {
    /// Create a `Response` from a oneshot receiver with a timeout.
    ///
    /// Used by tasks-mode protocol blanket impls. Returns
    /// `Err(ActorError::RequestTimeout)` if the timeout expires.
    pub fn from_with_timeout(
        result: Result<oneshot::Receiver<T>, ActorError>,
        duration: Duration,
    ) -> Self {
        #[cfg(feature = "metrics")]
        let observation = crate::instrumentation::RequestObservation::new();
        match result {
            Ok(rx) => {
                let fut = Box::pin(async move {
                    match spawned_rt::tasks::timeout(duration, rx).await {
                        Ok(Ok(val)) => Ok(val),
                        Ok(Err(_)) => Err(ActorError::ActorStopped),
                        Err(_) => Err(ActorError::RequestTimeout),
                    }
                });
                Self(
                    ResponseState::TimedReceiver(fut),
                    #[cfg(feature = "metrics")]
                    Some(observation),
                )
            }
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

#[cfg(feature = "async")]
impl<T> From<Result<oneshot::Receiver<T>, ActorError>> for Response<T> {
    fn from(result: Result<oneshot::Receiver<T>, ActorError>) -> Self {
        #[cfg(feature = "metrics")]
        let observation = crate::instrumentation::RequestObservation::new();
        match result {
            Ok(rx) => Self(
                ResponseState::Receiver(rx),
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
            ResponseState::Receiver(rx) => match Pin::new(rx).poll(_cx) {
                std::task::Poll::Ready(Ok(val)) => {
                    this.0 = ResponseState::Done;
                    std::task::Poll::Ready(Ok(val))
                }
                std::task::Poll::Ready(Err(_)) => {
                    this.0 = ResponseState::Done;
                    std::task::Poll::Ready(Err(ActorError::ActorStopped))
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
            #[cfg(feature = "async")]
            ResponseState::TimedReceiver(fut) => match fut.as_mut().poll(_cx) {
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
    use spawned_rt::tasks::oneshot;

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
            let (tx, rx) = oneshot::channel::<i32>();
            let resp: Response<i32> = Response::from(Ok(rx));
            tx.send(99).unwrap();
            let val = resp.await.unwrap();
            assert_eq!(val, 99);
        });
    }

    #[test]
    #[cfg(feature = "async")]
    fn future_err_on_dropped_sender() {
        let rt = spawned_rt::tasks::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<i32>();
            let resp: Response<i32> = Response::from(Ok(rx));
            drop(tx);
            let result = resp.await;
            assert!(matches!(result, Err(ActorError::ActorStopped)));
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
    fn timed_receiver_resolves() {
        let rt = spawned_rt::tasks::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<i32>();
            let resp = Response::from_with_timeout(Ok(rx), Duration::from_secs(5));
            tx.send(42).unwrap();
            assert_eq!(resp.await.unwrap(), 42);
        });
    }

    #[test]
    #[cfg(feature = "async")]
    fn timed_receiver_times_out() {
        let rt = spawned_rt::tasks::Runtime::new().unwrap();
        rt.block_on(async {
            let (_tx, rx) = oneshot::channel::<i32>();
            let resp = Response::from_with_timeout(Ok(rx), Duration::from_millis(50));
            let result = resp.await;
            assert!(matches!(result, Err(ActorError::RequestTimeout)));
        });
    }
}
