#[derive(Debug)]
pub enum GenServerError {
    CallbackError,
    ServerError,
}

impl<T> From<spawned_rt::tasks::mpsc::SendError<T>> for GenServerError {
    fn from(_value: spawned_rt::tasks::mpsc::SendError<T>) -> Self {
        Self::ServerError
    }
}
