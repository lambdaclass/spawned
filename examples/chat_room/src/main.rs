//! Chat room example demonstrating how `Recipient<M>` solves circular dependencies.
//!
//! The problem:
//!   - `ChatRoom` needs to send `Deliver` to each `User`
//!   - `User` needs to send `Say` to the `ChatRoom`
//!   - With concrete types, `room.rs` would import `User` and
//!     `user.rs` would import `ChatRoom` — circular module dependency.
//!
//! The solution:
//!   - `ChatRoom` holds `Recipient<Deliver>` — doesn't know about `User`
//!   - `User` holds `Recipient<Say>` — doesn't know about `ChatRoom`
//!   - Both modules only depend on the shared `messages` module.
//!
//! Message flow:
//!   main -> SayToRoom -> User -> Say -> ChatRoom -> Deliver -> User

mod messages;
mod room;
mod user;

use messages::{Deliver, Join, SayToRoom};
use room::ChatRoom;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::time::Duration;
use user::User;

fn main() {
    rt::run(async {
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

        // Register users — room stores Recipient<Deliver>, not ActorRef<User>
        room.send_request(Join {
            name: "Alice".into(),
            inbox: alice.recipient::<Deliver>(),
        })
        .await
        .unwrap();

        room.send_request(Join {
            name: "Bob".into(),
            inbox: bob.recipient::<Deliver>(),
        })
        .await
        .unwrap();

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
