use spawned_concurrency::error::ActorError;
use spawned_concurrency::protocol;

#[protocol]
pub trait UpdaterProtocol: Send + Sync {
    fn _check(&self) -> Result<(), ActorError>;
}
