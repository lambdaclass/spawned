// A `ReplyTo` answers exactly once: it is not `Clone`.
use spawned_concurrency::ReplyTo;
fn duplicate(token: &ReplyTo<u64>) -> ReplyTo<u64> {
    token.clone()
}
fn main() {}
