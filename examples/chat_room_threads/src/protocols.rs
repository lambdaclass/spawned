use spawned_concurrency::error::ActorError;
use spawned_concurrency::protocol;

#[protocol]
pub trait RoomProtocol: Send + Sync {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn add_member(&self, name: String, user: UserRef) -> Result<(), ActorError>;
    fn members(&self) -> Result<Vec<String>, ActorError>;
}

#[protocol]
pub trait UserProtocol: Send + Sync {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError>;
    fn speak(&self, text: String) -> Result<(), ActorError>;
    fn join_room(&self, room: RoomRef) -> Result<(), ActorError>;
}
