use std::{error::Error as StdError, fmt::Display};

#[derive(Debug)]
pub enum GenServerError {
    Callback,
    Initialization,
    Server,
    CallMsgUnused,
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

impl Display for GenServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Callback => write!(f, "Callback Error"),
            Self::Initialization => write!(f, "Initialization Error"),
            Self::Server => write!(f, "Server Error"),
            Self::CallMsgUnused => write!(f, "Unsupported Call Messages on this GenServer"),
            Self::CastMsgUnused => write!(f, "Unsupported Cast Messages on this GenServer"),
        }
    }
}

impl StdError for GenServerError {}
