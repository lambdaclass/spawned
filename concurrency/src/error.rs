#[derive(Debug, thiserror::Error)]
pub enum GenServerError {
    #[error("Callback Error")]
    Callback,
    #[error("Initialization error")]
    Initialization,
    #[error("Server error")]
    Server,
    #[error("Unsupported Call Messages on this GenServer")]
    CallMsgUnused,
    #[error("Unsupported Cast Messages on this GenServer")]
    CastMsgUnused,
}

impl<T> From<spawned_rt::threads::mpsc::SendError<T>> for GenServerError {
    fn from(_value: spawned_rt::threads::mpsc::SendError<T>) -> Self {
        Self::Server
    }
}

impl<T> From<spawned_rt::tasks::mpsc::SendError<T>> for GenServerError {
    fn from(_value: spawned_rt::tasks::mpsc::SendError<T>) -> Self {
        Self::Server
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_into_std_error() {
        let error: &dyn std::error::Error = &GenServerError::Callback;
        assert_eq!(error.to_string(), "Callback Error");
    }
}
