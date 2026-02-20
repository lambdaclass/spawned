use spawned_concurrency::send_messages;

send_messages! {
    Ping;
    Pong
}
