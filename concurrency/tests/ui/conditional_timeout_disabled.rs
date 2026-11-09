use spawned_concurrency::protocol;
#[protocol]
trait Probe {
    #[cfg_attr(any(), request_timeout(std::time::Duration::ZERO))]
    fn value(&self) -> spawned_concurrency::Response<()>;
}
fn main() {}
