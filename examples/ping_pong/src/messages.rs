use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::Recipient;

#[derive(Debug)]
pub struct Ping;
impl Message for Ping {
    type Result = ();
}

#[derive(Debug)]
pub struct Pong;
impl Message for Pong {
    type Result = ();
}

pub struct SetConsumer(pub Recipient<Ping>);
impl Message for SetConsumer {
    type Result = ();
}

// Debug impl for SetConsumer (Recipient doesn't implement Debug)
impl std::fmt::Debug for SetConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SetConsumer").finish()
    }
}
