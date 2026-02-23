use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::Response;
use spawned_macros::protocol;
use std::sync::Arc;

pub type RoomRef = Arc<dyn RoomProtocol>;
pub type UserRef = Arc<dyn UserProtocol>;

#[protocol]
pub trait RoomProtocol: Send + Sync {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn add_member(&self, name: String, user: UserRef) -> Result<(), ActorError>;
    fn members(&self) -> Response<Vec<String>>;
}

#[protocol]
pub trait UserProtocol: Send + Sync {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError>;
    fn speak(&self, text: String) -> Result<(), ActorError>;
    fn join_room(&self, room: RoomRef) -> Result<(), ActorError>;
}
