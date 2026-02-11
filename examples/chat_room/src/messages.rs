use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::Recipient;

spawned_concurrency::messages! {
    Say { from: String, text: String } -> ();
    Deliver { from: String, text: String } -> ();
    SayToRoom { text: String } -> ()
}

// Join has a Recipient field — define manually since messages! doesn't support it
pub struct Join {
    pub name: String,
    pub inbox: Recipient<Deliver>,
}

impl Message for Join {
    type Result = ();
}
