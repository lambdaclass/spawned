use spawned_concurrency::error::ActorError;
use spawned_macros::protocol;
use std::sync::Arc;

pub type UpdaterRef = Arc<dyn UpdaterProtocol>;

#[protocol]
pub trait UpdaterProtocol: Send + Sync {
    fn _check(&self) -> Result<(), ActorError>;
}
