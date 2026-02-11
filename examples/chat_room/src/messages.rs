use spawned_concurrency::messages;

messages! {
    Say { from: String, text: String } -> ();
    SayToRoom { text: String } -> ();
    Deliver { from: String, text: String } -> ();
}
