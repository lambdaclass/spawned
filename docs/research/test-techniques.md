# Test techniques: cancellation, allocations, latency, platforms and metamorphic relations

Research note, 2026-11-09. Informs five rows of the
[testing to add](../../ROADMAP.md#testing-to-add) table (cancellation-safety
tests, allocation budgets, open-loop load generation, platform matrix,
cross-backend testing) and their batches in the
[database-grade testing program](../../ROADMAP.md#database-grade-testing-program).
Controlled scheduling, simulation and history checking are covered in
[systematic testing](systematic-testing.md); this note covers the smaller
techniques that note does not.

## What it is

Five techniques, each aimed at a bug class that ordinary tests miss:

1. **Cancellation-safety testing.** Dropping an async Rust future part-way
   through and checking the system is still consistent.
2. **Allocation-count tests.** Asserting how many heap blocks or bytes an
   operation allocates, so per-message cost cannot regress silently.
3. **Coordinated-omission-free load.** Issuing load on a schedule, not after
   each reply, so tail latency is measured rather than hidden.
4. **Platform matrix.** Running the suite on several OSes, architectures and
   core counts.
5. **Metamorphic testing.** Checking a relation between the outputs of related
   runs instead of the output of one run against a hand-written oracle.

## How it works

### Cancellation safety in async Rust

A Rust future does nothing unless polled, and dropping it cancels it. Rain
Paharia's Oxide RFD 400 puts it plainly: "Cancellation can only happen at await
points, due to the cooperative multitasking model that futures use"
([RFD 400](https://rfd.shared.oxide.computer/rfd/400)). Every `.await` in a
public future is therefore a point where the caller (a `select!`, a timeout, a
dropped task, an aborted `JoinHandle`) may stop it for good.

Tokio's `select!` documentation defines the property: "If you have a future that
has not yet completed, then it must be a no-op to drop that future and recreate
it." Receiving from `mpsc`, `broadcast` and `watch` channels is cancel-safe;
`read_exact`, `write_all` and the lock and semaphore acquisitions are not (the
latter lose their place in a fairness queue). The docs add that cancelling a
non-cancel-safe future "is not necessarily wrong"
([`tokio::select!`](https://docs.rs/tokio/latest/tokio/macro.select.html#cancellation-safety)).

RFD 400 and the RustConf 2025 talk behind it separate two properties
([RFD 400](https://rfd.shared.oxide.computer/rfd/400);
[Cancelling async Rust](https://sunshowers.io/posts/cancelling-async-rust/)):

- **Cancel safety** is local: a future can be dropped before completion without
  side effects.
- **Cancel correctness** is global: it is violated only when a cancel-unsafe
  future exists, is actually cancelled, and the cancellation breaks a system
  property (lost data, invalid shared state, skipped cleanup, unfairness).

Its recommended patterns include splitting an operation into a cancel-safe
reserve step and a synchronous commit (reserve an `mpsc` permit, then send),
resuming pinned futures in `select!` loops instead of recreating them, moving
cancel-unsafe work into spawned tasks that finish even if the caller goes away,
and documenting cancel-unsafe APIs.

**Testing it.** No mainstream Rust crate we found enumerates await points
automatically (search, 2026-11-09; not exhaustive). The usual harness is
manual: poll the future a fixed number of times with a mock waker, drop it,
then check invariants, and repeat for N = 0, 1, 2, ... until the future
completes without being dropped. `tokio_test::task::spawn` wraps a future "in a
mocked executor" and lets a test poll it without pinning or a context, and
counts wake-ups ([tokio-test](https://docs.rs/tokio-test/latest/tokio_test/task/index.html)).
With Tokio's paused clock, a timer-driven future can be advanced to each of its
sleeps in turn. The poll count only approximates "each await point": an await
on an already-ready future does not return `Pending`, so the harness reaches
only the awaits that actually suspend. Failpoints with a `Yield` action (PR
#204) force suspension at chosen lifecycle sites, which closes much of that gap.

### Allocation-count tests with dhat

`dhat` (Nicholas Nethercote's Rust port of Valgrind's DHAT) has a testing mode:
`dhat::Profiler::builder().testing().build()` at the start of a test,
`dhat::HeapStats::get()` for `total_blocks`, `total_bytes`, `max_blocks`,
`max_bytes`, `curr_blocks` and `curr_bytes`, and `dhat::assert!`,
`dhat::assert_eq!` and `dhat::assert_ne!`, which save a heap profile when they
fail ([docs.rs/dhat](https://docs.rs/dhat/latest/dhat/)). Caveats from the same
page:

- heap profiling is global state, and `dhat` panics if two profilers run at
  once, so the docs recommend one heap-usage test per integration-test file;
- the test runner's own allocations may be counted;
- a test can pass in debug and fail in release because unused allocations are
  optimised away;
- the crate is described as experimental with low maintenance priority.

A budget test therefore measures a delta (`total_blocks` after N messages minus
after a warm-up) and divides by N, in a dedicated integration-test file, in
release mode.

### Coordinated omission and open-loop load

Gil Tene named *coordinated omission*: a load generator that sends the next
request only after the previous reply stops sending while the system is
stalled, so the stall is recorded as one slow sample instead of many
([How NOT to Measure Latency, QCon SF 2015 slides](https://videog.infoq.com/downloads/pdfdownloads/presentations/QConSF2015-GilTene-HowNOTtomeasureLatency.pdf);
[mechanical-sympathy thread](https://groups.google.com/d/msg/mechanical-sympathy/icNZJejUHfE/BfDekfBEs_sJ)).
wrk2's README describes the same flaw in wrk ("each connection will only begin
to send a request after receiving a response") and fixes it with a constant
request rate (`-R`) and by measuring latency from when each request *should*
have been sent ([wrk2](https://github.com/giltene/wrk2)). HdrHistogram offers an
after-the-fact correction when the expected interval is known; the Rust crate
exposes it as `Histogram::record_correct(value, interval)`, which back-fills the
samples a stall hid ([hdrhistogram](https://docs.rs/hdrhistogram/latest/hdrhistogram/struct.Histogram.html)).
Scheduling sends on a clock (open loop) is the primary fix; the correction is
a fallback for closed-loop tools.

### Platform matrix

Concurrency bugs and performance assumptions differ across memory models (x86
TSO versus ARM's weaker ordering), OS schedulers and core counts. GitHub-hosted
runners now cover this cheaply for public repositories: `ubuntu-24.04-arm`
(4 CPUs), arm64 macOS (`macos-15`, `macos-latest`, 3 CPUs) and `windows-11-arm`,
all "free and unlimited on public repositories"
([GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)).
Spawned's pinned shards use Linux `sched_setaffinity` only
(`rt/src/tasks/affinity.rs`), so macOS runs check the fallback path and
Linux arm64 runs check placement on a second architecture. Core count matters
separately: the single-CPU runs in batch 1 and a many-core run on fede256 bound
the range.

### Metamorphic testing beyond databases

A *metamorphic relation* is a necessary property linking several inputs and
their outputs; testing checks the relation rather than an exact expected
output, which sidesteps the oracle problem. Surveys: Chen et al., ACM
Computing Surveys 51(1), 2018 ([DOI 10.1145/3143561](https://doi.org/10.1145/3143561));
Segura et al., IEEE TSE 42(9), 2016 ([PDF](https://eprints.whiterose.ac.uk/110335/1/segura16-tse.pdf)).
Uses outside databases:

- **Compilers.** Equivalence Modulo Inputs (Le, Afshari and Su, PLDI 2014)
  mutates the parts of a program not executed on a given input; every variant
  must produce the same output on that input. The paper reports "a few
  hundred" GCC and LLVM bugs ([PDF](https://www.cs.ucdavis.edu/~su/publications/emi.pdf)).
  Secondary summaries give 147 confirmed; we did not check the figure against
  the paper body.
- **Machine learning.** Xie et al. (JSS 84(4), 2011) derive relations a
  classifier must satisfy (for example, permuting training samples or adding
  an uninformative attribute) and found real faults in an open-source
  classifier library ([DOI](https://doi.org/10.1016/j.jss.2010.11.920)).
  That the library was Weka comes from a secondary source (unverified).
- **Databases**, for comparison: CockroachDB's metamorphic testing
  (see [systematic testing](systematic-testing.md#how-databases-and-storage-systems-test)).

Spawned's planned cross-backend test is a metamorphic relation: one workload on
the async, blocking, thread, native-pool and pinned backends must produce the
same replies, exit reasons and per-sender order. Other relations available
without an oracle: adding an idle actor or an unrelated supervisor does not
change another tree's outcome; splitting a group into two groups with the same
total budget does not change replies; zero backoff versus small backoff changes
timing but not the restart count when the budget is not exhausted; running
with and without the `metrics` or `lifecycle-trace` feature yields the same
observable results.

## Who uses it

- **Cancellation safety:** Oxide documents it as an engineering practice in
  RFD 400 and RFD 397; Tokio documents per-method cancel safety.
- **dhat testing mode:** rustc's performance work is Nethercote's background;
  we found no named project publicly using dhat's testing mode in CI
  (unverified either way).
- **Coordinated omission:** wrk2 and HdrHistogram are Tene's (Azul) tools;
  adoption claims beyond that are not made here.
- **Metamorphic testing:** GCC and LLVM bug reports from the EMI work;
  CockroachDB for storage.

## What it means for Spawned

- **Cancellation-safety tests ([testing to add](../../ROADMAP.md#testing-to-add), batch 1).**
  A harness in `concurrency/tests/` that, for each public future
  (`request`, `join`, `shutdown`, `restart_child`, group starts, supervisor
  start), polls it N times under `tokio_test::task::spawn` with the clock
  paused, drops it, then lets the system settle and checks: no actor leaked
  (the leak checks of batch 1), no supervisor stuck in a restart, every
  accepted request answered or `ActorStopped`, and a second call behaves
  normally. Repeat N until the future completes. Combine with failpoint
  `Yield` at the lifecycle sites (PR #204) so awaits that would not suspend
  do. Document each public future's cancel safety in rustdoc, in Tokio's
  style; RFD 400's reserve-then-commit pattern is the fix when one is not.
  Cancel scopes (rank 15a) are the design that makes cancel correctness a
  framework property; these tests are its baseline.
- **Allocation budgets (later batch).** One integration-test file per budget,
  `dhat` testing mode, release build: warm up, send N messages, assert
  `total_blocks` per message is at most the current count (one reply channel,
  one boxed envelope, one boxed handler future today, per rank 37). Rank 37's
  generated dispatch then has a number to beat. The benchmark harness's
  allocation-counting control can feed the starting values.
- **Open-loop load (later batch).** The `benchmarks/` cases are closed-loop
  request/reply as far as we read the README; add an open-loop driver that
  sends at a fixed rate from a timer, records latency from the scheduled send
  time into an HdrHistogram, and reports when it falls behind schedule. Use it
  for rank 8a's starvation bound and rank 17's admission tests, where
  closed-loop numbers would hide exactly the stalls being measured.
- **Platform matrix (later batch).** Add `ubuntu-24.04-arm` and `macos-latest`
  jobs for the default test suite and the failpoint suite; keep TSAN and the
  pinned-shard tests on Linux x86 and Linux arm64. Free for this public
  repository.
- **Metamorphic relations (batch 1).** Extend the cross-backend test with the
  relations above; each is a few lines over existing scenarios and needs no
  oracle.

## Sources

- [Oxide RFD 400: Dealing with cancel safety in async Rust](https://rfd.shared.oxide.computer/rfd/400); [RFD 397](https://rfd.shared.oxide.computer/rfd/397)
- [Rain, Cancelling async Rust (RustConf 2025 talk, written up)](https://sunshowers.io/posts/cancelling-async-rust/)
- [`tokio::select!` cancellation safety](https://docs.rs/tokio/latest/tokio/macro.select.html#cancellation-safety)
- [`tokio_test::task`](https://docs.rs/tokio-test/latest/tokio_test/task/index.html)
- [dhat](https://docs.rs/dhat/latest/dhat/), [dhat-rs](https://github.com/nnethercote/dhat-rs)
- [Gil Tene, How NOT to Measure Latency (QCon SF 2015 slides)](https://videog.infoq.com/downloads/pdfdownloads/presentations/QConSF2015-GilTene-HowNOTtomeasureLatency.pdf); [coordinated omission thread](https://groups.google.com/d/msg/mechanical-sympathy/icNZJejUHfE/BfDekfBEs_sJ)
- [wrk2](https://github.com/giltene/wrk2); [hdrhistogram (Rust)](https://docs.rs/hdrhistogram/latest/hdrhistogram/struct.Histogram.html)
- [GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
- [Chen et al., Metamorphic Testing: A Review of Challenges and Opportunities (ACM CSUR 2018)](https://doi.org/10.1145/3143561)
- [Segura et al., A Survey on Metamorphic Testing (IEEE TSE 2016)](https://eprints.whiterose.ac.uk/110335/1/segura16-tse.pdf)
- [Le, Afshari and Su, Compiler Validation via Equivalence Modulo Inputs (PLDI 2014)](https://www.cs.ucdavis.edu/~su/publications/emi.pdf)
- [Xie et al., Testing and Validating Machine Learning Classifiers by Metamorphic Testing (JSS 2011)](https://doi.org/10.1016/j.jss.2010.11.920)
