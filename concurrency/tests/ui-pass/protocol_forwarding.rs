use spawned_concurrency::{protocol, Response};
use std::sync::Arc;

#[protocol]
trait Store: Send + Sync {
    fn read(&self) -> Response<u64>;
}
impl<T: Store + ?Sized> Store for Box<T> {
    fn read(&self) -> Response<u64> { <T as Store>::read(self.as_ref()) }
}
impl<T: Store + ?Sized> Store for &T {
    fn read(&self) -> Response<u64> { <T as Store>::read(*self) }
}
impl<T: ToStoreRef + ?Sized> ToStoreRef for Box<T> {
    fn to_store_ref(&self) -> StoreRef { <T as ToStoreRef>::to_store_ref(self.as_ref()) }
}
impl<T: ToStoreRef + ?Sized> ToStoreRef for &T {
    fn to_store_ref(&self) -> StoreRef { <T as ToStoreRef>::to_store_ref(*self) }
}
struct Fake;
impl Store for Fake { fn read(&self) -> Response<u64> { Response::ready(Ok(42)) } }
impl ToStoreRef for Fake { fn to_store_ref(&self) -> StoreRef { Arc::new(Fake) } }
fn main() {
    let boxed = Box::new(Fake);
    assert_eq!(Store::read(&boxed).unwrap(), 42);
    assert_eq!(Store::read(&&Fake).unwrap(), 42);
    assert_eq!(ToStoreRef::to_store_ref(&boxed).read().unwrap(),42);
    assert_eq!(ToStoreRef::to_store_ref(&&Fake).read().unwrap(),42);
}
