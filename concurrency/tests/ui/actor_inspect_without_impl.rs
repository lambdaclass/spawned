// `#[actor(inspect)]` needs an `Inspect` impl.
use spawned_concurrency::actor;
use spawned_concurrency::tasks::Actor;
pub struct Secret;
#[actor(inspect)]
impl Secret {}
fn main() {}
