// `#[sim_test]` rejects an unknown option and names the ones it takes.
#[spawned_concurrency::sim_test(timeout = 5)]
async fn unknown_option() {}

fn main() {}
