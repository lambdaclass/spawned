mod messages;
mod room;
mod user;

use messages::{Deliver, Join, SayToRoom};
use room::ChatRoom;
use spawned_concurrency::tasks::ActorStart;
use user::User;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let room = ChatRoom::new().start();

    let alice = User {
        name: "Alice".into(),
        room: room.recipient(),
    }
    .start();

    let bob = User {
        name: "Bob".into(),
        room: room.recipient(),
    }
    .start();

    // Register users in the room
    room.send(Join {
        name: "Alice".into(),
        inbox: alice.recipient::<Deliver>(),
    })
    .unwrap();

    room.send(Join {
        name: "Bob".into(),
        inbox: bob.recipient::<Deliver>(),
    })
    .unwrap();

    // Alice says something
    alice
        .send(SayToRoom {
            text: "Hello everyone!".into(),
        })
        .unwrap();

    // Bob says something
    bob.send(SayToRoom {
        text: "Hi Alice!".into(),
    })
    .unwrap();

    // Give time for messages to propagate
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    tracing::info!("Chat room demo complete");
}
