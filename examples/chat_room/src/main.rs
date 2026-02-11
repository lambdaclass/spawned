//! Chat room example demonstrating how protocol traits solve circular dependencies.
//!
//! The problem:
//!   - `ChatRoom` needs to send `Deliver` to each `User`
//!   - `User` needs to send `Say` to the `ChatRoom`
//!   - With concrete types, `room.rs` would import `User` and
//!     `user.rs` would import `ChatRoom` — circular module dependency.
//!
//! The solution:
//!   - `ChatRoom` holds `Arc<dyn ChatParticipant>` — doesn't know about `User`
//!   - `User` holds `Arc<dyn ChatBroadcaster>` — doesn't know about `ChatRoom`
//!   - Both modules only depend on the shared `protocols` module.
//!
//! Message flow:
//!   main -> SayToRoom -> User -> Say -> ChatRoom -> Deliver -> User

mod messages;
mod protocols;
mod room;
mod user;

use messages::SayToRoom;
use protocols::ChatBroadcaster;
use room::ChatRoom;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::sync::Arc;
use std::time::Duration;
use user::User;

fn main() {
    rt::run(async {
        let room = ChatRoom::new().start();

        let alice = User {
            name: "Alice".into(),
            room: Arc::new(room.clone()),
        }
        .start();

        let bob = User {
            name: "Bob".into(),
            room: Arc::new(room.clone()),
        }
        .start();

        // Register users via protocol trait — room stores Arc<dyn ChatParticipant>
        room.add_member("Alice".into(), Arc::new(alice.clone())).unwrap();
        room.add_member("Bob".into(), Arc::new(bob.clone())).unwrap();
        // Small delay to let join messages be processed
        rt::sleep(Duration::from_millis(10)).await;

        // Alice speaks: main -> alice (SayToRoom) -> room (Say) -> bob (Deliver)
        alice
            .send_request(SayToRoom {
                text: "Hello everyone!".into(),
            })
            .await
            .unwrap();

        // Bob replies: main -> bob (SayToRoom) -> room (Say) -> alice (Deliver)
        bob.send_request(SayToRoom {
            text: "Hey Alice!".into(),
        })
        .await
        .unwrap();

        // Let deliveries propagate
        rt::sleep(Duration::from_millis(50)).await;
    })
}
