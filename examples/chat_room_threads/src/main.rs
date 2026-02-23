mod protocols;
mod room;
mod user;

use std::thread;
use std::time::Duration;

use protocols::{AsRoom, RoomProtocol, UserProtocol};
use room::ChatRoom;
use spawned_concurrency::threads::ActorStart;
use spawned_rt::threads as rt;
use user::User;

fn main() {
    rt::run(|| {
        let room = ChatRoom::new().start();
        let alice = User::new("Alice".into()).start();
        let bob = User::new("Bob".into()).start();

        alice.join_room(room.as_room()).unwrap();
        bob.join_room(room.as_room()).unwrap();
        thread::sleep(Duration::from_millis(10));

        let members = room.members().unwrap();
        tracing::info!("Members in room: {:?}", members);

        alice.speak("Hello everyone!".into()).unwrap();
        bob.speak("Hi Alice!".into()).unwrap();
        thread::sleep(Duration::from_millis(100));

        tracing::info!("Chat room demo complete");
    });
}
