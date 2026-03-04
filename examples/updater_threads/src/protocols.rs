use spawned_concurrency::error::ActorError;
use spawned_macros::protocol;

#[protocol]
pub trait UpdaterProtocol: Send + Sync {
    fn _check(&self) -> Result<(), ActorError>;
}
