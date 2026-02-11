#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    #[error("Actor stopped")]
    ActorStopped,
    #[error("Request to Actor timed out")]
    RequestTimeout,
}

impl<T> From<spawned_rt::threads::mpsc::SendError<T>> for ActorError {
    fn from(_value: spawned_rt::threads::mpsc::SendError<T>) -> Self {
        Self::ActorStopped
    }
}

impl<T> From<spawned_rt::tasks::mpsc::SendError<T>> for ActorError {
    fn from(_value: spawned_rt::tasks::mpsc::SendError<T>) -> Self {
        Self::ActorStopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_into_std_error() {
        let error: &dyn std::error::Error = &ActorError::ActorStopped;
        assert_eq!(error.to_string(), "Actor stopped");
    }
}
