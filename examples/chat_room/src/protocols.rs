use spawned_concurrency::error::ActorError;
use spawned_concurrency::Response;
use spawned_concurrency::protocol;

#[protocol]
pub trait RoomProtocol: Send + Sync {
    /// Broadcast a message from a user to all other members in the room.
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    /// Add a new member to the room.
    fn add_member(&self, name: String, user: UserRef) -> Result<(), ActorError>;
    /// Return the list of member names currently in the room.
    fn members(&self) -> Response<Vec<String>>;
}

#[protocol]
pub trait UserProtocol: Send + Sync {
    /// Deliver a message to this user's inbox.
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError>;
    /// Ask this user to say something in the room.
    fn say(&self, text: String) -> Result<(), ActorError>;
    /// Tell this user to join a room.
    fn join_room(&self, room: RoomRef) -> Result<(), ActorError>;
}
