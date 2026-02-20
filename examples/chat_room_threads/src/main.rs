mod room;
mod user;

use std::thread;
use std::time::Duration;

use room::{ChatRoom, ChatRoomApi};
use spawned_concurrency::threads::ActorStart;
use spawned_rt::threads as rt;
use user::{User, UserApi};

fn main() {
    rt::run(|| {
        let room = ChatRoom::new().start();

        let alice = User::new("Alice".into()).start();
        let bob = User::new("Bob".into()).start();

        // Register users in the room (send — fire-and-forget)
        alice.join_room(room.clone()).unwrap();
        bob.join_room(room.clone()).unwrap();

        // Let join messages propagate (user -> room)
        thread::sleep(Duration::from_millis(10));

        // Query members (request — blocking)
        let members = room.members().unwrap();
        tracing::info!("Members in room: {:?}", members);

        // Chat (send — fire-and-forget)
        alice.say("Hello everyone!".into()).unwrap();
        bob.say("Hi Alice!".into()).unwrap();

        // Give time for messages to propagate
        thread::sleep(Duration::from_millis(100));

        tracing::info!("Chat room demo complete");
    });
}
