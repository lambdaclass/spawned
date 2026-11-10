// Valid protocol shapes that are easy to break: a user type with the same name
// as a generated message, an acronym trait name without the Protocol suffix,
// explicit unit and Result<(), ActorError> sends, const and inline timeouts,
// 'static borrows, fn-pointer fields and a default method body.
use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
use spawned_concurrency::{actor, protocol, ActorError, Response};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct Event(u32);

const SLOW: Duration = Duration::from_secs(60);

#[protocol(timeout = Duration::from_secs(30))]
pub trait HTTPStore: Send + Sync {
    /// Explicit unit return is a fire-and-forget send.
    #[allow(clippy::unused_unit)]
    fn event(&self, event: Event) -> ();
    fn record(&self, value: u32) -> Result<(), ActorError>;
    #[request_timeout(SLOW)]
    fn events(&self) -> Response<Vec<Event>>;
    #[request_timeout(Duration::from_millis(SLOW.as_millis() as u64 * 2))]
    fn name_len(&self, name: &'static str) -> Response<usize>;
    fn callback(&self, f: fn(&str) -> usize) -> Response<usize>;
    /// A default body is kept for other implementors; ActorRef dispatches.
    fn total(&self) -> Response<u64> {
        Response::ready(Ok(0))
    }
}

#[derive(Default)]
pub struct Store {
    events: Vec<Event>,
    total: u32,
}

#[actor(protocol = HTTPStore)]
impl Store {
    #[send_handler]
    fn on_event(&mut self, msg: http_store::Event, _: &Context<Self>) {
        self.events.push(msg.event);
    }
    #[send_handler]
    fn on_record(&mut self, msg: http_store::Record, _: &Context<Self>) {
        self.total += msg.value;
    }
    #[request_handler]
    fn on_events(&mut self, _: http_store::Events, _: &Context<Self>) -> Vec<Event> {
        self.events.clone()
    }
    #[request_handler]
    fn on_name_len(&mut self, msg: http_store::NameLen, _: &Context<Self>) -> usize {
        msg.name.len()
    }
    #[request_handler]
    fn on_callback(&mut self, msg: http_store::Callback, _: &Context<Self>) -> usize {
        (msg.f)("abc")
    }
    #[handler]
    fn on_total(&mut self, _: http_store::Total, _: &Context<Self>) -> u64 {
        u64::from(self.total)
    }
}

fn main() {
    let store = Store::default().start();
    store.event(Event(1));
    store.record(2).unwrap();
    assert_eq!(store.events().unwrap(), vec![Event(1)]);
    assert_eq!(store.name_len("four").unwrap(), 4);
    assert_eq!(store.callback(str::len).unwrap(), 3);
    let erased: HTTPStoreRef = store.to_http_store_ref();
    assert_eq!(erased.total().unwrap(), 2);
    store.context().stop();
    store.join();
}
