use spawned_concurrency::tasks::Response;
use spawned_macros::protocol;
use std::sync::Arc;

pub type NameServerRef = Arc<dyn NameServerProtocol>;

#[derive(Debug, Clone, PartialEq)]
pub enum FindResult {
    Found { value: String },
    NotFound,
}

#[protocol]
pub trait NameServerProtocol: Send + Sync {
    fn add(&self, key: String, value: String) -> Response<()>;
    fn find(&self, key: String) -> Response<FindResult>;
}
