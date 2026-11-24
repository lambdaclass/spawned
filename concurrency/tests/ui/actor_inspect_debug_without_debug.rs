// `#[actor(inspect = debug)]` needs `Debug`.
use spawned_concurrency::actor;
use spawned_concurrency::threads::Actor;
pub struct Opaque;
#[actor(inspect = debug)]
impl Opaque {}
fn main() {}
