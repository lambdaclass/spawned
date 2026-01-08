# Spawned examples

Examples demonstrating the spawned concurrency library. All examples support
different backends (`Backend::Async`, `Backend::Blocking`, `Backend::Thread`)
through the unified GenServer API.

## Examples

- **ping_pong**: Simple example demonstrating the Process abstraction for message passing.
- **name_server**: Simple GenServer example for key-value storage (based on Armstrong's Erlang book).
- **bank**: More complex GenServer example with multiple operations and error handling.
- **updater**: A periodic GenServer that fetches a URL at regular intervals.
- **blocking_genserver**: Demonstrates handling of blocking operations across backends.
- **busy_genserver_warning**: Shows debug warnings when GenServer blocks the async runtime.

## Backend Selection

All examples use `Backend::Async` by default, but you can modify them to use:

```rust
// For async workloads (default)
let handle = MyServer::new().start(Backend::Async);

// For blocking operations
let handle = MyServer::new().start(Backend::Blocking);

// For dedicated OS thread
let handle = MyServer::new().start(Backend::Thread);
```
