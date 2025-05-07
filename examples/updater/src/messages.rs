#[derive(Debug, Clone)]
pub enum UpdaterInMessage {
    Check(String),
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum UpdaterOutMessage {
    Ok,
    Error,
}
