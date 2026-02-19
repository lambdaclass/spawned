//! Protocol traits — cross-actor contracts.
//!
//! Neither `ChatRoom` nor `User` appears here. The circular dependency
//! is broken because each actor holds an `Arc<dyn ProtocolTrait>`
//! instead of a concrete `ActorRef<OtherActor>`.

use spawned_concurrency::error::ActorError;
use std::sync::Arc;

pub trait ChatParticipant: Send + Sync {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError>;
}

pub trait ChatBroadcaster: Send + Sync {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn add_member(&self, name: String, inbox: Arc<dyn ChatParticipant>) -> Result<(), ActorError>;
}
