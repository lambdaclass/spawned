# Spawned examples
Some examples to test runtime and concurrency:

- ping_pong: Simple example to test Process abstraction using `tasks` implementation.
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
