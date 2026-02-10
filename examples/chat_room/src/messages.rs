//! Shared message types — the only thing both actors need to agree on.
//!
//! Neither `ChatRoom` nor `User` appears here. The circular dependency
//! is broken because each actor holds a `Recipient<M>` (type-erased)
//! instead of a concrete `ActorRef<OtherActor>`.

use spawned_concurrency::message::Message;
use spawned_concurrency::messages;
use spawned_concurrency::tasks::Recipient;

// Join carries a Recipient, so we define it manually (Recipient doesn't impl Debug)
pub struct Join {
    pub name: String,
    pub inbox: Recipient<Deliver>,
}

impl Message for Join {
    type Result = ();
}

impl std::fmt::Debug for Join {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Join").field("name", &self.name).finish()
    }
}

// The rest use the macro
messages! {
    Say { from: String, text: String } -> ();
    SayToRoom { text: String } -> ();
    Deliver { from: String, text: String } -> ();
}
