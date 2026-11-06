# Generic inline actor-future control

Current custom jobs and Runnable wrappers keep the actor future behind a
Pin<Box<dyn Future>>. The backend boundary is generic before that conversion.
Test keeping concrete F inside async-task's allocation, with a pinned Option<F>
wrapper, to remove the separate actor-future allocation and dynamic poll layer
and permit monomorphization. This changes allocation/layout and compiler
optimization opportunities together, so timing cannot isolate vtable cost alone.
Ordinary message/envelope/reply allocations remain; this is not allocation-free.

Use the already locked pin-project-lite0.2.16 as an optional direct dependency.
The macro projects #[pin] future:Option<F>; poll uses as_pin_mut, disposal uses
safe Pin::set(None), and PinnedDrop performs guarded cleanup. F is never moved
after it becomes pinned. Rust assignment replaces the slot on an unwind edge;
targeted tests must confirm the Option is None and Drop runs exactly once when
F's destructor panics. No custom unsafe code or public actor API change is used.

Keep separate poll/disposal unwind boundaries and the existing normal Rust
double-panic limit. Tests cover a PhantomPinned future's address across repeated
polls and native tickets, direct disposal before polling and after Ready,
destructor panic and a direct sequential poll/disposal-panic probe. Selected
actor suites must run with inline1; all prior gates precede any measurements.
A private startup selector INLINE_FUTURE0/1 defaults0. DriverConfig groups private
startup options, without changing stable Actor/Handler/Backend signatures.

Paired graph cases hold root quantum8,1920actors/256logical producers,one million
roots and eight handlers/root. Task/Deque cross boxed/inline with joint Z0W0 and
Z1W1 scheduling settings; these two joint settings do not isolate Z versus W
(that separate factorial is V6). Tokio4/5 and custom Root4 are controls. Five
fresh repetitions of four graph cases give220processes;16profiles are separate.
No new overload/soak claim follows from these graph controls alone.

References:
- [Rust1.93 Pin::set implementation](https://github.com/rust-lang/rust/blob/1.93.0/library/core/src/pin.rs)
- [Pin projection and pinned drop](https://docs.rs/pin-project-lite/latest/pin_project_lite/macro.pin_project.html)
- [async-task pinned task allocation](https://github.com/smol-rs/async-task/blob/v4.7.1/src/raw.rs)

Compilation, Linux tests and scores are pending at this checkpoint.
