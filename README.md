# Spawned
Library for concurrent Rust.

# Goals:

- Provide a framework to build robust, scalable and efficient applications in concurrent Rust.
- Set coding guidelines to apply along LambdaClass repositories and codebase.
- Starting point to ideate what we want for Concrete.

# Versions:

Two versions exist in their own submodules:
- threads: no use of async/await. Just IO threads code.
- tasks: a runtime is required to run async/await code. The runtime is selected in `spawned_rt::tasks` module that abstracts it.

# Inspiration:

- [Erlang](https://www.erlang.org/)
- [Commonware.xyz](https://commonware.xyz)
- [Ractor](https://slawlor.github.io/ractor/)
- [Actors with Tokio](https://ryhl.io/blog/actors-with-tokio/)
- [Vale.dev](https://vale.dev/)
- [Gleam](https://gleam.run/)