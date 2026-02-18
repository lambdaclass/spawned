use spawned_concurrency::message::Message;

#[derive(Debug, Clone, PartialEq)]
pub enum NameServerOutMessage {
    Ok,
    Found { value: String },
    NotFound,
    Error,
}

#[derive(Debug)]
pub struct Add {
    pub key: String,
    pub value: String,
}
impl Message for Add {
    type Result = NameServerOutMessage;
}

#[derive(Debug)]
pub struct Find {
    pub key: String,
}
impl Message for Find {
    type Result = NameServerOutMessage;
}
