use spawned_concurrency::{protocol, Response};
use std::time::Duration;
#[protocol]
trait InvalidProtocol: Send + Sync {
    #[request_timeout(Duration::ZERO)]
    #[request_timeout(Duration::from_secs(1))]
    fn query(&self) -> Response<()>;
}
fn main() {}
