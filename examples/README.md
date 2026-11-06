# Spawned examples
Start with the [capability guide](capabilities/README.md) for finite, asserted
programs showing runtime isolation, byte-budgeted pipelines, timeout outcomes,
shutdown draining and framework metrics. Each explains why it is useful and its
limits. The [proposed example catalog](planned/README.md) maps every ranked
roadmap idea to the scenario it should eventually demonstrate; proposed APIs are
not available yet.

Run an existing package with `cargo run --locked -p <package>` from the repository
root. Some older examples below use external HTTP services or run until stopped.

Basic runtime and concurrency examples:

- ping_pong: Simple example to test Actor abstraction using `tasks` implementation.
- ping_pong_threads: ping_pong example on `threads` implementation.
- name_server: Simple example to test Actor abstraction using `tasks` implementation.
- bank: A bit more complex example for Actor using `tasks` implementation.
- bank_threads: bank example on `threads` implementation.
- chat_room: Chat room example using `tasks` implementation.
- chat_room_threads: chat_room example on `threads` implementation.
- updater: A "live" process that checks a URL periodically using `tasks` implementation.
- updater_threads: updater example on `threads` implementation.
- signal_test: Signal handling example using `tasks` implementation.
- signal_test_threads: signal_test example on `threads` implementation.
- blocking_genserver: Example demonstrating Backend::Thread to handle blocking tasks.
- busy_genserver_warning: Example showing warning detection for tasks that take too long.
- service_discovery: Example demonstrating the named registry for service discovery.

- exit_reason: Clean stop, panic, ChildHandle, monitors, links and trapped exits. This demonstrates lifecycle primitives, not automatic restart supervision.
