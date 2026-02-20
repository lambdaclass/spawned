use spawned_concurrency::message::Message;

#[derive(Debug, Clone)]
pub struct Check;
impl Message for Check {
    type Result = ();
}
