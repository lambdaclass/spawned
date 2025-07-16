# Spawned concurrency
Some traits and structs to implement à-la-Erlang concurrent code.

Currently two versions:

- threads: no use of async/await. Just IO threads code
- tasks: a runtime is required to run async/await code. It uses `spawned_rt::tasks` module that abstracts the runtime.
