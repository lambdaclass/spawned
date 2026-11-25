// `inspect` takes no other form than `inspect = debug`.
use spawned_concurrency::actor;
#[allow(unused_imports)]
use spawned_concurrency::tasks::Actor;
pub struct Shown;
#[actor(inspect = display)]
impl Shown {}
fn main() {}
