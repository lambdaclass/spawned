use spawned_concurrency::protocol;

#[protocol]
trait Invalid {
    fn read();
}

fn main() {}
