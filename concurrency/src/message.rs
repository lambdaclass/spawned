/// A message that can be sent to an actor.
///
/// Each message type defines its expected reply type via `Result`.
/// The `#[protocol]` macro generates `Message` implementations automatically.
/// For standalone messages without a protocol, implement this trait manually:
///
/// ```ignore
/// struct GetCount;
/// impl Message for GetCount {
///     type Result = u64;
/// }
/// ```
pub trait Message: Send + 'static {
    type Result: Send + 'static;
}
