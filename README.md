# Spawned
Library for concurrent Rust.

# Goals:

- Provide a framework to build robust, scalable and efficient applications in concurrent Rust.
- Set coding guidelines to apply along LambdaClass repositories and codebase.
- Starting point to ideate what we may expect for Concrete runtime implementation.

# Rationale

Inspired by Erlang OTP, the main goal for `spawned` is to keep concurrency logic separated from business logic. It provides an actor-model abstraction that honoring Erlang naming we called `GenServer`. `GenServer` handle the concurrency logic, while providing callback functions to implement the business logic.

As stated in  Joe Armstrong's book Programming Erlang:
> The callback had no code for concurrency, no spawn, no send, no receive, and no register. It is pure sequential code—nothing else. *This means we can write client-server models without understanding anything about the underlying concurrency models.*

This is the most important idea behind `spawned`. You can check [bank](https://github.com/lambdaclass/spawned/tree/main/examples/bank) example that mimics the same example from chapter 22 section 1 of Armstrong's book.

The bottom line is that any project using `spawned` should be simpler and easier to read and maintain, than when not using it. If that's not the case, we are doing it wrong.

# Roadmap

While current version is mostly a PoC, we have ambitious goals:
- Implement a `GenServer` registry. This will allow namimg and accessing `GenServer`s created somewhere else in the code, where the access to the handle is not easily available.
- Implement monitors and supervision trees. A way to manage a tree of actors and provide rules to stop, restart or propagate terminations when a `GenServer` fails.
- Implement our own runtime. Currently we are reexporting tokio structs and traits. We would like to implement our own runtime, probably reusing the bits and parts that we find useful from tokio or other runtimes, as well as implementing our own improvements.
- Implement a deterministic runtime. We think that comonware.xyz's implementation of a deterministic runtime is a great idea, and we plan to do the same or use it in `spawned`.

# Threads or tasks?:

Two execution versions exist in their own submodules:
- tasks: a runtime is required to run async/await code. The runtime is used in `spawned_rt::tasks` module that abstracts it. (Currently we are using tokio)
- threads: no use of async/await. Just IO threads code.

While `tasks` version is the one we are testing in other projects, `threads` version is an exploratory work to try to figure out if a tasks based implementation can be imitated by a io::thread implementation with similar behavior. 
Performance comparisons are planned in the future and if we find some scenarios where `threads` version is useful (eg. projects with little or limited number of concurrent threads) we may maintain both, probably behind a feature flag. If we maintain both, the idea is that switching a project from one version to the other should require minimum effort. But currently we are prioritizing `tasks` version so some functionality may not be present in `threads` one.

# Inspiration:

- [Erlang](https://www.erlang.org/)
- [Commonware.xyz](https://commonware.xyz)
- [Ractor](https://slawlor.github.io/ractor/)
- [Tokio](https://tokio.rs/)
- [Actors with Tokio](https://ryhl.io/blog/actors-with-tokio/)
- [Vale.dev](https://vale.dev/)
- [Gleam](https://gleam.run/)