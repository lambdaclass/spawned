# Spawned runtime
Runtime wrapper to remove dependencies from code. Using this library will allow to set a tokio runtime (or any other runtime, once implemented) just by changing the enabled runtime feature. 

This crate is part of [spawned](https://github.com/lambdaclass/spawned). To understand usage, we encourage you to read the [workspace README.md](https://github.com/lambdaclass/spawned/blob/main/README.md) but we reproduce a motivating example here.

## Example: hit the ground running
Let's take a look at one of the examples in the [examples folder](https://github.com/lambdaclass/spawned/tree/main/examples), the [name server](https://github.com/lambdaclass/spawned/tree/main/examples/name_server).
The name server is a test of the `GenServer` abstraction using `tasks` implementation, and is based on Joe's Armstrong book: Programming Erlang, Second edition, Section 22.1 - The Road to the Generic Server.

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
type NameServerHandle = GenServerHandle<NameServer>;

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
        match server.call(InMessage::Add { key, value }).await {
            Ok(_) => OutMessage::Ok,
            Err(_) => OutMessage::Error,
        }
    }

    pub async fn find(server: &mut NameServerHandle, key: String) -> OutMessage {
        server
            .call(InMessage::Find { key })
            .await
            .unwrap_or(OutMessage::Error)
    }
}
```

Now that our base state type is defined, we can implement the `GenServer` trait for our name server. Since the only thing we want to do differently than the defaults is how we handle `call` messages, we implement the async `handle_call` function and it's associated types:
```rust
impl GenServer for NameServer {
    type CallMsg = InMessage;
    type CastMsg = Unused;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _handle: &NameServerHandle,
    ) -> CallResponse<Self> {
        match message.clone() {
            Self::CallMsg::Add { key, value } => {
                self.inner.insert(key, value);
                CallResponse::Reply(Self::OutMsg::Ok)
            }
            Self::CallMsg::Find { key } => match self.inner.get(&key) {
                Some(result) => {
                    let value = result.to_string();
                    CallResponse::Reply(Self::OutMsg::Found { value })
                }
                None => CallResponse::Reply(Self::OutMsg::NotFound),
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

## Notes
We may implement a `deterministic` version based on comonware.xyz's runtime:

https://github.com/commonwarexyz/monorepo/blob/main/runtime/src/deterministic.rs
 
Currently, only a very limited set of tokio functionality is reexported. We may want to extend this functionality as needed.