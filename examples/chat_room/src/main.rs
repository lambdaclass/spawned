mod room;
mod user;

use room::{ChatRoom, ChatRoomApi};
use spawned_concurrency::tasks::ActorStart;
use user::{User, UserApi};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let room = ChatRoom::new().start();

    let alice = User::new("Alice".into()).start();
    let bob = User::new("Bob".into()).start();

    // Register users in the room (send — fire-and-forget)
    alice.join_room(&room).unwrap();
    bob.join_room(&room).unwrap();

    // Let join messages propagate (user → room)
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Query members (request — awaits a response)
    let members = room.members().await.unwrap();
    tracing::info!("Members in room: {:?}", members);

    // Chat (send — fire-and-forget)
    alice.say("Hello everyone!".into()).unwrap();
    bob.say("Hi Alice!".into()).unwrap();

    // Give time for messages to propagate
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    tracing::info!("Chat room demo complete");
}
