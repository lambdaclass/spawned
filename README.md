# Spawned
Library for concurrent Rust.

# Goals:
- Provide a framework to build robust, scalable and efficient applications in concurrent Rust.
- Set coding guidelines to apply along LambdaClass repositories and codebase.
- Starting point to ideate what we may expect for Concrete runtime implementation.

# Example: hit the ground running
Let's take a look at one of the examples in the [examples folder](https://github.com/lambdaclass/spawned/tree/main/examples), the [name server](https://github.com/lambdaclass/spawned/tree/main/examples/name_server).
The name server is a test of the `Actor` abstraction using `tasks` implementation, and is based on Joe's Armstrong book: Programming Erlang, Second edition, Section 22.1 - The Road to the Generic Server.

We would like to have a server that listens and responds to the following types of messages:

```rust
#[derive(Debug, Clone)]
pub enum NameServerInMessage {
    Add { key: String, value: String },
    Find { key: String },
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum NameServerOutMessage {
    Ok,
    Found { value: String },
    NotFound,
    Error,
}
```

To write our server code, we first need to define the type for our name server's state, and it's handle:
```rust
type NameServerHandle = ActorRef<NameServer>;

pub struct NameServer {
    inner: HashMap<String, String>,
}

impl NameServer {
    pub fn new() -> Self {
        NameServer {
            inner: HashMap::new(),
        }
    }
}
```

Our name server's API has two async functions: `add`, and `find`, which correspond to the `NameServerInMessage` variants. Note that these map to the return messages' type:

```rust
impl NameServer {
    pub async fn add(server: &mut NameServerHandle, key: String, value: String) -> OutMessage {
        match server.request(InMessage::Add { key, value }).await {
            Ok(_) => OutMessage::Ok,
            Err(_) => OutMessage::Error,
        }
    }

    pub async fn find(server: &mut NameServerHandle, key: String) -> OutMessage {
        server
            .request(InMessage::Find { key })
            .await
            .unwrap_or(OutMessage::Error)
    }
}
```

Now that our base state type is defined, we can implement the `Actor` trait for our name server. Since the only thing we want to do differently than the defaults is how we handle `request` messages, we implement the async `handle_request` function and it's associated types:
```rust
impl Actor for NameServer {
    type Request = InMessage;
    type Message = Unused;
    type Reply = OutMessage;
    type Error = std::fmt::Error;

    async fn handle_request(
        &mut self,
        message: Self::Request,
        _handle: &NameServerHandle,
    ) -> RequestResponse<Self> {
        match message.clone() {
            Self::Request::Add { key, value } => {
                self.inner.insert(key, value);
                RequestResponse::Reply(Self::Reply::Ok)
            }
            Self::Request::Find { key } => match self.inner.get(&key) {
                Some(result) => {
                    let value = result.to_string();
                    RequestResponse::Reply(Self::Reply::Found { value })
                }
                None => RequestResponse::Reply(Self::Reply::NotFound),
            },
        }
    }
}
```

Finally, we can write our `main` function:

```rust
fn main() {
    rt::run(async {
        let mut name_server = NameServer::new().start();

        let result =
            NameServer::add(&mut name_server, "Joe".to_string(), "At Home".to_string()).await;
        tracing::info!("Storing value result: {result:?}");
        assert_eq!(result, NameServerOutMessage::Ok);

        let result = NameServer::find(&mut name_server, "Joe".to_string()).await;
        tracing::info!("Retrieving value result: {result:?}");
        assert_eq!(
            result,
            NameServerOutMessage::Found {
                value: "At Home".to_string()
            }
        );

        let result = NameServer::find(&mut name_server, "Bob".to_string()).await;
        tracing::info!("Retrieving value result: {result:?}");
        assert_eq!(result, NameServerOutMessage::NotFound);
    })
}
```

If you run `cargo run --bin name_server` this should produce:
```
2025-10-17T22:33:41.004784Z  INFO name_server: Storing value result: Ok
2025-10-17T22:33:41.004902Z  INFO name_server: Retrieving value result: Found { value: "At Home" }
2025-10-17T22:33:41.004940Z  INFO name_server: Retrieving value result: NotFound
```

There are many more highlighting other features.

# Rationale

Inspired by Erlang OTP, the main goal for `spawned` is to keep concurrency logic separated from business logic. It provides an actor-model abstraction called `Actor` (inspired by Erlang's GenServer). The `Actor` trait handles the concurrency logic, while providing callback functions to implement the business logic.

As stated in  Joe Armstrong's book Programming Erlang:
> The callback had no code for concurrency, no spawn, no send, no receive, and no register. It is pure sequential code—nothing else. *This means we can write client-server models without understanding anything about the underlying concurrency models.*

This is the most important idea behind `spawned`. You can check [bank](https://github.com/lambdaclass/spawned/tree/main/examples/bank) example that mimics the same example from chapter 22 section 1 of Armstrong's book.

The bottom line is that any project using `spawned` should be simpler and easier to read and maintain, than when not using it. If that's not the case, we are doing it wrong.

# Erlang to Spawned Terminology

For developers familiar with Erlang/OTP, here's how common concepts map to `spawned`:

| Erlang/OTP | Spawned | Description |
|------------|---------|-------------|
| `gen_server` | `Actor` | The main abstraction for a stateful process that handles messages |
| `gen_server` (module) | `ActorRef<T>` | A handle to communicate with a running actor |
| `call/2,3` | `actor_ref.request(msg)` | Synchronous request that waits for a reply |
| `cast/2` | `actor_ref.send(msg)` | Asynchronous message (fire-and-forget) |
| `handle_call/3` | `handle_request()` | Callback for synchronous requests |
| `handle_cast/2` | `handle_message()` | Callback for asynchronous messages |
| `init/1` | `init()` | Initialization callback before the actor starts processing |
| `terminate/2` | `teardown()` | Cleanup callback when the actor stops |
| Request message | `type Request` | Associated type for call/request messages |
| Cast message | `type Message` | Associated type for cast/fire-and-forget messages |
| Reply | `type Reply` | Associated type for responses to calls |
| `{reply, Reply, State}` | `RequestResponse::Reply(value)` | Response that sends a reply and continues |
| `{noreply, State}` | `MessageResponse::NoReply` | Response that continues without reply |
| `{stop, Reason, Reply, State}` | `RequestResponse::Stop(value)` | Response that sends reply and stops actor |
| `start_link/0,1` | `actor.start()` | Start the actor |
| `Pid` | `ActorRef<T>` | Reference to communicate with a process/actor |

Example comparison:

**Erlang:**
```erlang
-module(my_server).
-behaviour(gen_server).

handle_call({add, Key, Value}, _From, State) ->
    NewState = maps:put(Key, Value, State),
    {reply, ok, NewState}.
```

**Spawned:**
```rust
impl Actor for MyServer {
    type Request = MyRequest;
    type Reply = MyReply;
    // ...

    async fn handle_request(&mut self, msg: Self::Request, _handle: &ActorRef<Self>) -> RequestResponse<Self> {
        match msg {
            MyRequest::Add { key, value } => {
                self.data.insert(key, value);
                RequestResponse::Reply(MyReply::Ok)
            }
        }
    }
}
```

# Roadmap

While current version is mostly a PoC, we have ambitious goals:
- Implement an `Actor` registry. This will allow naming and accessing `Actor`s created somewhere else in the code, where the access to the handle is not easily available.
- Implement monitors and supervision trees. A way to manage a tree of actors and provide rules to stop, restart or propagate terminations when an `Actor` fails.
- Implement our own runtime. Currently we are reexporting tokio structs and traits. We would like to implement our own runtime, probably reusing the bits and parts that we find useful from tokio or other runtimes, as well as implementing our own improvements.
- Implement a deterministic runtime. We think that comonware.xyz's implementation of a deterministic runtime is a great idea, and we plan to do the same or use it in `spawned`.

# Versions

Two execution versions exist in their own submodules:
- **tasks**: Requires an async runtime (currently Tokio). Use `spawned_rt::tasks` module.
- **threads**: No async/await. Uses native OS threads via `spawned_rt::threads` module.

Both modules provide the same Actor API. Switching between them requires only changing imports and removing `async`/`.await` syntax. The `tasks` module additionally provides a `Backend` enum to control execution (async runtime, blocking thread pool, or dedicated OS thread).

# Inspiration:

- [Erlang](https://www.erlang.org/)
- [Commonware.xyz](https://commonware.xyz)
- [Ractor](https://slawlor.github.io/ractor/)
- [Tokio](https://tokio.rs/)
- [Actors with Tokio](https://ryhl.io/blog/actors-with-tokio/)
- [Vale.dev](https://vale.dev/)
- [Gleam](https://gleam.run/)