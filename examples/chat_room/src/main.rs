mod protocols;
mod room;
mod user;

use protocols::{AsRoom, RoomProtocol, UserProtocol};
use room::ChatRoom;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::time::Duration;
use user::User;

fn main() {
    rt::run(async {
        let room = ChatRoom::new().start();
        let alice = User::new("Alice".into()).start();
        let bob = User::new("Bob".into()).start();

        alice.join_room(room.as_room()).unwrap();
        bob.join_room(room.as_room()).unwrap();
        rt::sleep(Duration::from_millis(10)).await;

        let members = room.members().await.unwrap();
        tracing::info!("Members: {:?}", members);

        alice.speak("Hello everyone!".into()).unwrap();
        bob.speak("Hey Alice!".into()).unwrap();

        rt::sleep(Duration::from_millis(50)).await;
    })
}
