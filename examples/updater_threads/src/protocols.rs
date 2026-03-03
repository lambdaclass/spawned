use spawned_concurrency::error::ActorError;
use spawned_macros::protocol;
use std::sync::Arc;

#[allow(dead_code)]
pub type UpdaterRef = Arc<dyn UpdaterProtocol>;

#[protocol]
pub trait UpdaterProtocol: Send + Sync {
    fn _check(&self) -> Result<(), ActorError>;
}
