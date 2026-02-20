//! Chat room example — Approach A (Recipient<M> + actor_api!).
//!
//! User depends on Room (one-way). Room doesn't know about User.
//! Cross-boundary erasure is via `Recipient<Deliver>`.

mod room;
mod user;

use room::{ChatRoom, ChatRoomApi};
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::time::Duration;
use user::{User, UserApi};

fn main() {
    rt::run(async {
        let room = ChatRoom::new().start();
        let alice = User::new("Alice".into()).start();
        let bob = User::new("Bob".into()).start();

        alice.join_room(room.clone()).unwrap();
        bob.join_room(room.clone()).unwrap();
        rt::sleep(Duration::from_millis(10)).await;

        let members = room.members().await.unwrap();
        tracing::info!("Members: {:?}", members);

        alice.say("Hello everyone!".into()).unwrap();
        bob.say("Hey Alice!".into()).unwrap();

        rt::sleep(Duration::from_millis(50)).await;
    })
}
