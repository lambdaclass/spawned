#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    #[error("Callback Error")]
    Callback,
    #[error("Initialization error")]
    Initialization,
    #[error("Server error")]
    Server,
    #[error("Unsupported Request on this Actor")]
    RequestUnused,
    #[error("Unsupported Message on this Actor")]
    MessageUnused,
    #[error("Request to Actor timed out")]
    RequestTimeout,
}

impl<T> From<spawned_rt::threads::mpsc::SendError<T>> for ActorError {
    fn from(_value: spawned_rt::threads::mpsc::SendError<T>) -> Self {
        Self::Server
    }
}

impl<T> From<spawned_rt::tasks::mpsc::SendError<T>> for ActorError {
    fn from(_value: spawned_rt::tasks::mpsc::SendError<T>) -> Self {
        Self::Server
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_into_std_error() {
        let error: &dyn std::error::Error = &ActorError::Callback;
        assert_eq!(error.to_string(), "Callback Error");
    }
}
