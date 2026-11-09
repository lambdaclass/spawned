use spawned_concurrency::{protocol, Response};
#[protocol(unknown = 10)]
trait InvalidProtocol: Send + Sync {
    fn query(&self) -> Response<()>;
}
fn main() {}
