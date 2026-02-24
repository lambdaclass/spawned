use spawned_concurrency::error::ActorError;
use spawned_macros::protocol;
use std::sync::Arc;

pub type UpdaterRef = Arc<dyn UpdaterProtocol>;

#[protocol]
#[allow(dead_code)]
pub trait UpdaterProtocol: Send + Sync {
    fn check(&self) -> Result<(), ActorError>;
}
