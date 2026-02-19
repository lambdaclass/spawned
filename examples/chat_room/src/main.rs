//! Chat room example — Approach B (protocol traits + protocol_impl!).
//!
//! Room and User have ZERO direct dependencies on each other.
//! Both depend only on the protocol traits in `protocols.rs`.
//! Cross-boundary erasure is via `BroadcasterRef` and `ParticipantRef`.

mod protocols;
mod room;
mod user;

use protocols::{AsBroadcaster, ChatBroadcaster};
use room::ChatRoom;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::time::Duration;
use user::{User, UserActions};

fn main() {
    rt::run(async {
        let room = ChatRoom::new().start();
        let alice = User::new("Alice".into()).start();
        let bob = User::new("Bob".into()).start();

        alice.join_room(room.as_broadcaster()).unwrap();
        bob.join_room(room.as_broadcaster()).unwrap();
        rt::sleep(Duration::from_millis(10)).await;

        let members = room.members().await.unwrap();
        tracing::info!("Members: {:?}", members);

        alice.say("Hello everyone!".into()).unwrap();
        bob.say("Hey Alice!".into()).unwrap();

        rt::sleep(Duration::from_millis(50)).await;
    })
}
