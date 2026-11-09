use spawned_concurrency::protocol;
use std::time::Duration;
#[protocol]
trait InvalidProtocol: Send + Sync {
    #[request_timeout(Duration::ZERO)]
    fn notify(&self);
}
fn main() {}
