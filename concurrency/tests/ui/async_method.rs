use spawned_concurrency::protocol;

#[protocol]
trait Invalid {
    async fn read(&self);
}

fn main() {}
