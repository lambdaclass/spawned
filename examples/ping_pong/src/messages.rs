use spawned_concurrency::message::Message;

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
