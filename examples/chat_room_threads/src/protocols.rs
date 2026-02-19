//! Protocol traits — cross-actor contracts (sync/threads version).

use spawned_concurrency::error::ActorError;
use std::sync::Arc;

pub type BroadcasterRef = Arc<dyn ChatBroadcaster>;
pub type ParticipantRef = Arc<dyn ChatParticipant>;

pub trait ChatBroadcaster: Send + Sync {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn add_member(&self, name: String, participant: ParticipantRef) -> Result<(), ActorError>;
    fn members(&self) -> Result<Vec<String>, ActorError>;
}

pub trait ChatParticipant: Send + Sync {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError>;
}

pub trait AsBroadcaster {
    fn as_broadcaster(&self) -> BroadcasterRef;
}

pub trait AsParticipant {
    fn as_participant(&self) -> ParticipantRef;
}
