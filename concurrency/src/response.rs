use crate::error::ActorError;
use spawned_rt::tasks::oneshot;
use std::future::Future;
use std::pin::Pin;

// ---------------------------------------------------------------------------
// Response<T> — unified wrapper for protocol request-response (tasks + threads)
// ---------------------------------------------------------------------------

enum ResponseState<T> {
    Receiver(oneshot::Receiver<T>),
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
pub struct Response<T>(ResponseState<T>);

impl<T> Unpin for Response<T> {}

impl<T> Response<T> {
    /// Create a `Response` from a pre-computed result.
    ///
    /// Used by the threads runtime where the request blocks at call time
    /// and the result is immediately available.
    pub fn ready(result: Result<T, ActorError>) -> Self {
        Self(ResponseState::Ready(result))
    }

    /// Extract the value, panicking on error.
    ///
    /// For threads mode where the result is already available.
    /// In tasks mode, use `.await.unwrap()` instead.
    pub fn unwrap(self) -> T {
        match self.0 {
            ResponseState::Ready(result) => result.unwrap(),
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
            ResponseState::Receiver(_) => panic!("{msg}"),
            ResponseState::Done => panic!("{msg}"),
        }
    }

    /// Returns `true` if the response contains `Ok`.
    /// Only meaningful for ready responses (threads mode).
    pub fn is_ok(&self) -> bool {
        matches!(&self.0, ResponseState::Ready(Ok(_)))
    }

    /// Returns `true` if the response contains `Err`.
    /// Only meaningful for ready responses (threads mode).
    pub fn is_err(&self) -> bool {
        matches!(&self.0, ResponseState::Ready(Err(_)))
    }

    /// Maps the inner value if the response is ready and `Ok`.
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> Response<U> {
        match self.0 {
            ResponseState::Ready(result) => Response(ResponseState::Ready(result.map(f))),
            ResponseState::Receiver(_) => {
                panic!("called map() on a pending Response; use .await in async contexts")
            }
            ResponseState::Done => panic!("Response already consumed"),
        }
    }
}

impl<T> From<Result<oneshot::Receiver<T>, ActorError>> for Response<T> {
    fn from(result: Result<oneshot::Receiver<T>, ActorError>) -> Self {
        match result {
            Ok(rx) => Self(ResponseState::Receiver(rx)),
            Err(e) => Self(ResponseState::Ready(Err(e))),
        }
    }
}

impl<T: Send + 'static> Future for Response<T> {
    type Output = Result<T, ActorError>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        match &mut this.0 {
            ResponseState::Receiver(rx) => match Pin::new(rx).poll(cx) {
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
            ResponseState::Ready(_) => {
                match std::mem::replace(&mut this.0, ResponseState::Done) {
                    ResponseState::Ready(result) => std::task::Poll::Ready(result),
                    _ => unreachable!(),
                }
            }
            ResponseState::Done => panic!("Response polled after completion"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
