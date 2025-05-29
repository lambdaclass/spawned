# Spawned examples
Some examples to test runtime and concurrency:

- ping_pong: Simple example to test Process abstraction using `tasks` implementation.
- ping_pong_threads: ping_pong example on `threads` implementation.
- name_server: Simple example to test GenServer abstraction using `tasks` implementation.
- name_server_with_error: Same name_server example with a deliverate error to check catching mechanism to prevent panicking on callback code.
- bank: A bit more complex example for GenServer using `tasks` implementation.
- bank_threads: bank example on `threads` implementation.
- updater: A "live" process that checks an url periodicaly using `tasks` implementation.
- updater_threads: updater example on `threads` implementation.