# LSan follow-up to 8b95117e

The [PR #209 LSan job](https://github.com/lambdaclass/spawned/actions/runs/42731824144/job/129551466143)
passed its 170 unit assertions, then reported 32 leaked bytes in two allocations.
The focused unit reproduces exactly those two allocations with symbols enabled.
Both are secondary string panic payloads retained intentionally by
`mailbox::dispose_panic`, rather than leaked actor/mailbox ownership.

The two symbolized allocation stacks have this shared prefix (allocator and
standard-library plumbing elided):

```text
StaticStrPayload::take_box
<link::tests::delivery_contains_callback_payload_and_subscriber_panics::Payload as Drop>::drop
mailbox::dispose_panic::{closure#0}
mailbox::dispose_panic
link::deliver
```

One stack reaches the callback-payload cleanup at link.rs:203 and the other the
subscriber-payload cleanup at link.rs:207 (line numbers at 8b95117e). Each retained
secondary payload is 16 bytes; LSan exits with status 23. This is the intentional
last-resort `mem::forget`: recursively destroying arbitrary secondary panic
payloads could create an unbounded chain of destructor panics.

The correction moves the regression to the existing bounded subprocess harness.
Its two outer panic payloads still hold heap-backed Arc counters and have
panicking destructors; both destructors must run and delivery must not unwind.
Those destructors now raise `panic_any(Secondary)`, a zero-sized secondary payload
with an observable Drop counter. The counter must stay zero, proving that the
recursion bound forgets the secondary payload rather than dropping it. This
exercises the defensive branch without allocating leaked heap storage.
The parent requires the child to exit successfully and emit its final marker.
Neither process disables LSan or overrides its options. No workflow change or
suppression is introduced; production panic disposal is unchanged.

## Validation

On fede, Linux x86_64, Rust 1.93.0 with `RUSTC_BOOTSTRAP=1`, the system Clang 18
LSan runtime and LLVM 21 symbolizer:

```sh
export RUSTC_BOOTSTRAP=1
export RUSTFLAGS="-Zsanitizer=leak -Zexternal-clangrt -Cdebuginfo=2 -Clinker=clang -Clink-arg=-fsanitize=leak -Cforce-frame-pointers=yes"
export LSAN_OPTIONS="symbolize=1:external_symbolizer_path=$(command -v llvm-symbolizer):report_objects=1:malloc_context_size=40:fast_unwind_on_malloc=0"
export PROPTEST_CASES=8
cargo test --locked --target x86_64-unknown-linux-gnu -p spawned-concurrency \
  --lib --test supervision --test supervision_strategies --test nested_supervision \
  --test supervisor_policies --test execution_groups --test execution_profiles \
  --test shutdown_publication --test resource_cleanup --test linked_exit_macro --test leaks
cargo test --locked --target x86_64-unknown-linux-gnu -p spawned-concurrency \
  --no-default-features --lib
```

The first command uses the same test selection as CI: **254 passed across 11
binaries, one ignored subprocess helper invoked by its parent, no LSan reports**.
The native-only library run: **74 passed, one invoked subprocess helper ignored
by the outer harness, no LSan reports**. The focused unmodified test had reproduced
the original 32-byte leak before this change. These local runs use prebuilt std
and external Clang's LSan runtime; they do not substitute for the unchanged
nightly `-Zbuild-std` CI job on the pushed commit.

Claude Code's [focused review](lsan-claude.md) approved the test correction and
suggested the observable zero-sized secondary destructor. After adding that
counter assertion, the exact parent/subprocess regression passed again under
symbolized LSan in both default and native-only builds. Strict Clippy passed in
both profiles for the initial subprocess/ZST correction.

The PR remains draft. This follow-up neither validates Ethrex's pending 2 MiB
RPC rerun nor authorizes a merge, tag or publication.
