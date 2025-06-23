#[derive(Debug)]
pub enum GenServerError {
    Callback,
    Initialization,
    Server,
}

impl<T> From<spawned_rt::threads::mpsc::SendError<T>> for GenServerError {
    fn from(_value: spawned_rt::threads::mpsc::SendError<T>) -> Self {
        Self::Server
    }
}
