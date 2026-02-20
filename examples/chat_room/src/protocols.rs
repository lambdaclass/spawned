//! Protocol traits — cross-actor contracts.
//!
//! Neither `ChatRoom` nor `User` appears here. Both actors depend only
//! on these traits, breaking circular dependencies completely.

use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::Response;
use std::sync::Arc;

pub type BroadcasterRef = Arc<dyn ChatBroadcaster>;
pub type ParticipantRef = Arc<dyn ChatParticipant>;

pub trait ChatBroadcaster: Send + Sync {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn add_member(&self, name: String, participant: ParticipantRef) -> Result<(), ActorError>;
    fn members(&self) -> Response<Vec<String>>;
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
