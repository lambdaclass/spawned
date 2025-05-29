#[derive(Debug, Clone)]
pub enum UpdaterInMessage {
    Check,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum UpdaterOutMessage {
    Ok,
    Error,
}
