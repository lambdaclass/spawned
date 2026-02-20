use spawned_concurrency::message::Message;

#[derive(Debug)]
pub struct Add {
    pub key: String,
    pub value: String,
}
impl Message for Add {
    type Result = ();
}

#[derive(Debug)]
pub struct Find {
    pub key: String,
}
impl Message for Find {
    type Result = FindResult;
}

#[derive(Debug, Clone, PartialEq)]
pub enum FindResult {
    Found { value: String },
    NotFound,
}
