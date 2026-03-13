# Spawned runtime

Runtime abstraction layer for the [spawned](https://github.com/lambdaclass/spawned) actor framework.

Wraps tokio and provides unified APIs for both async (`tasks`) and blocking (`threads`) execution modes. Includes runtime initialization, spawning, cancellation tokens, channels, and signal handling.

## Modules

- **`tasks`** — Tokio-based async runtime: `spawn`, `sleep`, `timeout`, `CancellationToken`, `mpsc`, `oneshot`, `watch`, `ctrl_c`
- **`threads`** — OS thread-based runtime: `spawn`, `sleep`, `CancellationToken`, `mpsc`, `oneshot`, `ctrl_c`

## Usage

Users typically don't interact with this crate directly beyond `rt::run()` and `rt::block_on()`. The `spawned-concurrency` crate builds on top of it.

```rust
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        // your actor code here
    })
}
```
