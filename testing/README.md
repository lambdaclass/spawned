# spawned-test

Mocks, recording probes, bounded harness helpers and fault injection for
[Spawned](https://github.com/lambdaclass/spawned) actors. Test dependencies reached
through typed protocol references or `Recipient<M>` without changing production
handlers.

For the 0.7.1 release, add it as a development dependency:

```toml
[dev-dependencies]
spawned-test = "0.7.1"
```

- `tasks::Mock` and `threads::Mock` reply from per-message closures and record
  messages. Unconfigured messages fail loudly.
- `Probe<M>` records messages sent through a recipient.
- Harness helpers bound waits for replies, actor exits, generations and shutdown.
- `faults::exit` and `restart_child` exercise failures and supervision. Kill is
  direct; linked trapping observers receive a trappable exit notice.

A synchronous example, using the application's `spawned-concurrency` dependency:

```rust
use spawned_concurrency::{message::Message, ExitReason};
use spawned_test::threads::{join_within, Mock};
use std::time::Duration;

struct Lookup { key: u64 }
impl Message for Lookup { type Result = u64; }

let server = Mock::new().on::<Lookup>(|msg| msg.key + 1).start();
assert_eq!(server.request(Lookup { key: 41 }).unwrap(), 42);
let received: Lookup = server.expect(Duration::from_secs(1));
assert_eq!(received.key, 41);
server.context().stop();
assert_eq!(join_within(&server, Duration::from_secs(1)), ExitReason::Normal);
```

The default `async` feature enables Tokio-based helpers. For synchronous-only
builds, use `default-features = false`; native helpers and direct Kill injection
remain available without compiling Tokio.

Mocks and recorded logs are in-process test tools, not a deterministic scheduler.
See the [test kit guide](https://github.com/lambdaclass/spawned/blob/main/docs/GUIDE.md#testing-your-actors)
and [examples](https://github.com/lambdaclass/spawned/tree/main/examples/capabilities).
