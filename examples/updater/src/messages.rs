#[derive(Debug, Clone)]
pub enum UpdaterInMessage {
<<<<<<< HEAD
    Check(String),
=======
    Check,
>>>>>>> async_handlers
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum UpdaterOutMessage {
    Ok,
    Error,
}
