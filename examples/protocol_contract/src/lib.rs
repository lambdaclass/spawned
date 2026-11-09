//! A protocol contract shared with a separate consumer crate. Its actor and
//! handwritten fake implementations live in capabilities/tests/cross_crate_protocol.rs.
use spawned_concurrency::{protocol, Response};
use std::fmt::Debug;

#[protocol]
pub trait SharedProtocol: Send + Sync + Debug {
    fn value(&self) -> Response<u64>;
}

#[protocol]
pub trait WhereProtocol: Send + Sync
where
    Self: Debug,
{
    fn version(&self) -> Response<u64>;
}
