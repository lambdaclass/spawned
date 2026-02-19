//! Chat room example — Approach B (protocol traits + protocol_impl!, sync/threads).

mod protocols;
mod room;
mod user;

use std::thread;
use std::time::Duration;

use protocols::{AsBroadcaster, ChatBroadcaster};
use room::ChatRoom;
use spawned_concurrency::threads::ActorStart;
use spawned_rt::threads as rt;
use user::{User, UserActions};

fn main() {
    rt::run(|| {
        let room = ChatRoom::new().start();
        let alice = User::new("Alice".into()).start();
        let bob = User::new("Bob".into()).start();

        alice.join_room(room.as_broadcaster()).unwrap();
        bob.join_room(room.as_broadcaster()).unwrap();
        thread::sleep(Duration::from_millis(10));

        let members = room.members().unwrap();
        tracing::info!("Members in room: {:?}", members);

        alice.say("Hello everyone!".into()).unwrap();
        bob.say("Hi Alice!".into()).unwrap();
        thread::sleep(Duration::from_millis(100));

        tracing::info!("Chat room demo complete");
    });
}
