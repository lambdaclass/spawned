# Review: PR209 linked-delivery panic test fix

**Verdict:** The fix is correct. Production logic is unchanged and the regression keeps its meaningful coverage. I'd make one optional change to strengthen the test, and there are two small wording/CI points.

## Correctness

- **Subprocess wiring is right.**
  - `#[path = "../tests/support/drop_subprocess.rs"]` resolves relative to `src/`, so it points at `concurrency/tests/support/drop_subprocess.rs`.
  - `tests/support/` isn't auto-discovered as a test target.
  - The `--exact` name `link::tests::delivery_panic_child` is correct for a lib unit-test binary, because the crate prefix is omitted.
  - `current_exe()` re-invokes the same lib test binary.
- **The marker check covers the "0 tests matched" case.** If the child name ever drifts, libtest exits successfully having run nothing. The `output.contains(marker)` assertion catches that.
- **The zero-sized payload really avoids the leak.**
  - `panic_any(())` boxes a ZST, so `Box<dyn Any + Send>` makes no allocation.
  - The unwinder's exception object is freed when `catch_unwind` catches it.
  - Nothing remains for `mem::forget` to retain.
- **No abort risk was introduced.** The secondary panic is raised inside `dispose_panic`'s `catch_unwind`, after the primary unwind has already been caught. It is not raised during an unwind. If `deliver` regresses and drops a `Payload` mid-unwind, the abort happens in the child and the parent reports it cleanly.
- **`drops == 2` still proves both primary payloads were destroyed.** One comes from the callback and one from the tracing subscriber. That is the meaningful containment property, and it is unchanged.
- **LSan is still effective.** A leak report makes the child exit non-zero, which fails `status.success()` in the parent.

## Coverage gap worth closing (optional)

`dispose_panic` never inspected the secondary payload, so changing `&'static str` to `()` loses no branch coverage. However, neither the old test nor the new one checks that the secondary payload is actually **forgotten rather than dropped**. With `()`, a regression to `drop(secondary)` would pass silently, and that is exactly the recursion bound the comment describes.

A zero-sized secondary payload with an observable `Drop` would cover this and still not leak:

```rust
static SECONDARY_DROPS: AtomicUsize = AtomicUsize::new(0);
struct Secondary;
impl Drop for Secondary {
    fn drop(&mut self) { SECONDARY_DROPS.fetch_add(1, Ordering::SeqCst); }
}
// in Payload::drop:
std::panic::panic_any(Secondary);
// after delivery:
assert_eq!(SECONDARY_DROPS.load(Ordering::SeqCst), 0, "secondary payload must be forgotten");
```

A static counter is fine here because the test runs alone in its subprocess.

## Wording nits

- **"Heap-backed" is slightly inaccurate.** The original `panic!("…")` payload was a `&'static str`, not a heap `String`. The 16 bytes were the `Box<&'static str>` fat-pointer allocation. Any non-zero-sized secondary payload leaks its box, whether it owns heap data or not. Suggested phrasing for both `docs/TESTING.md` and the `mailbox.rs` comment: "Any non-zero-sized secondary payload leaks its box allocation (the original `&str` payloads leaked 16 bytes each)." The mailbox comment's "retains any heap storage it owns" has the same imprecision.
- **The env var name is now a misnomer.** `SPAWNED_MAILBOX_DROP_CHILD` is now used by a link test. It's cosmetic, but a generic name such as `SPAWNED_SUBPROCESS_CHILD` would age better if more tests adopt the harness.

## Worth confirming

- **The documented LSan command doesn't run this test.** `docs/TESTING.md` shows the LSan command with `--test leaks`, and that target doesn't include lib unit tests like this one. Since symbolized LSan did catch it, CI presumably also runs `--lib` under LSan. Make sure the documented command and the CI job agree, so "LSan stays enabled in both processes" describes a job that actually runs.
- **The vacuous pass under `--ignored` is acceptable.** Running `cargo test -- --ignored` without the env var makes `delivery_panic_child` pass without doing anything. This matches the existing harness pattern.

## Disposition

The optional zero-sized `Secondary` with an observable Drop counter was adopted:
both outer payload destructors must run, and no secondary destructor may run.
This strengthens the original recursion-bound coverage without heap leakage.
The boxed-static-str wording was corrected and the documented LSan command now
includes `--lib`. The existing subprocess environment marker is retained to avoid
unrelated fixture churn. The sanitizer workflow and child LSan options remain
unchanged. This was static review; actual local sanitizer results are in lsan.md.
