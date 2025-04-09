#[derive(Debug, Clone)]
pub enum NameServerInMessage {
    Add { key: String, value: String },
    Find { key: String },
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum NameServerOutMessage {
    Ok,
    Found { value: String },
    NotFound,
    Error,
}
