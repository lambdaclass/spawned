# Spawned Examples

Examples demonstrating the spawned concurrency library's features.

## Examples

### `showcase` - Comprehensive Feature Demo
Demonstrates all major features in one place:
- GenServer with different Backends (Async, Blocking, Thread)
- Process registry (Pid and typed handle registration)
- Process linking and monitoring
- Process groups (pg)
- Supervisor and DynamicSupervisor
- Process introspection

```bash
cargo run -p showcase
```

### `bank` - GenServer with Multiple Backends
A bank server demonstrating:
- GenServer pattern for stateful services
- Running the same server with different backends:
  - `Backend::Async` - for I/O-bound work
  - `Backend::Thread` - dedicated OS thread
  - `Backend::Blocking` - blocking thread pool
- Registry integration for named process lookup

```bash
cargo run -p bank
```

### `ping_pong` - Process Communication
Simple ping-pong example showing:
- Process spawning
- Inter-process messaging

```bash
cargo run -p ping_pong
```

### `name_server` - Basic GenServer
Simple name server demonstrating:
- GenServer call/cast patterns
- State management

```bash
cargo run -p name_server
```

### `updater` - Periodic Tasks
A process that periodically checks URLs:
- Timer-based periodic execution
- Async HTTP operations

```bash
cargo run -p updater
```

### `blocking_genserver` - Blocking Operations
Example of handling blocking operations:
- Using `Backend::Blocking` for CPU-intensive work
- Keeping async tasks responsive

```bash
cargo run -p blocking_genserver
```

## Feature Coverage

| Feature | Example |
|---------|---------|
| GenServer | bank, name_server, showcase |
| Backend::Async | bank, showcase |
| Backend::Thread | bank, showcase |
| Backend::Blocking | bank, blocking_genserver, showcase |
| Registry (Pid) | showcase |
| Registry (typed handle) | bank, showcase |
| Process groups (pg) | showcase |
| Process linking | showcase |
| Process monitoring | showcase |
| Supervisor | showcase |
| DynamicSupervisor | showcase |
| Process introspection | showcase |
| Timers | updater |
