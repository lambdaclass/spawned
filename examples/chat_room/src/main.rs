mod room;
mod user;

use std::time::Duration;

use room::{ChatRoom, ChatRoomApi};
use spawned_concurrency::tasks::ActorStart;
use spawned_rt::tasks as rt;
use user::{User, UserApi};

fn main() {
    rt::run(async {
        let room = ChatRoom::new().start();

        let alice = User::new("Alice".into()).start();
        let bob = User::new("Bob".into()).start();

        // Register users in the room (send — fire-and-forget)
        alice.join_room(room.clone()).unwrap();
        bob.join_room(room.clone()).unwrap();

        // Let join messages propagate (user → room)
        rt::sleep(Duration::from_millis(10)).await;

        // Query members (request — awaits a response)
        let members = room.members().await.unwrap();
        tracing::info!("Members in room: {:?}", members);

        // Chat (send — fire-and-forget)
        alice.say("Hello everyone!".into()).unwrap();
        bob.say("Hi Alice!".into()).unwrap();

        // Give time for messages to propagate
        rt::sleep(Duration::from_millis(100)).await;

        tracing::info!("Chat room demo complete");
    });
}
