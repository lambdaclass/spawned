# Rust verification tools: Kani, Prusti, Verus, Creusot, Flux and Miri

Research note, 2026-11-09. Informs the [verification rule](../../ROADMAP.md#verification-rule),
the Kani row of the [database-grade testing program](../../ROADMAP.md#database-grade-testing-program)
(batch 1) and ranks 1-4 of the [roadmap](../../ROADMAP.md#ranked-backlog). The
earlier [verification strategy](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/docs/research/verification-strategy.md)
chose TLA+ plus trace validation for lifecycle protocols; this note covers the
tools that check Rust code itself, and where each fits next to TLA+, plus the
tools that prove the TLA+-level protocol invariants for unbounded configurations
(Apalache, TLAPS, Veil).

## What it is

Three kinds of tool check Rust source rather than a separate model:

1. **Bounded model checking** (Kani): every input up to a bound, explored
   symbolically; a property either holds for all of them or comes back with a
   concrete counterexample.
2. **Deductive verification** (Prusti, Verus, Creusot) and **refinement types**
   (Flux): the programmer writes contracts (pre- and postconditions,
   invariants), and an SMT solver proves them for all inputs, with no bound.
3. **Undefined-behavior detection by interpretation** (Miri): runs the real
   tests in an interpreter that checks every memory access and aliasing rule.

None of them models an actor protocol across asynchronous turns as directly as
TLA+ does. They complement it at the level of single functions.

## How it works

### Kani

Kani "automatically checks panics (e.g. `unwrap()` on `None`), arithmetic
overflows, and custom correctness properties", and "many kinds of undefined
behavior" ([Kani](https://model-checking.github.io/kani/)). A proof harness
looks like a test (`#[kani::proof]`) but draws inputs from `kani::any()`, so
the property covers every value of the type; loops are unwound to a stated
bound. Kani compiles the harness from MIR into CBMC's intermediate language and
lets CBMC's bit-precise engine decide it
([Kani paper, ICSE-SEIP 2022 preprint](https://cs.wellesley.edu/~avh/dyn-trait-icse-seip-2022-preprint.pdf)).
The result is a proof, a counterexample, or running out of resources. Function
contracts and stubbing (replacing a function, for example a clock, by a
nondeterministic model) extend it past small bounds. Kani lists concurrency
among the Rust features it does not support.

### Prusti

"A prototype verifier for Rust" built on ETH Zurich's Viper infrastructure. By
default it proves "absence of integer overflows and panics"; with annotations it
checks pre- and postconditions and loop invariants
([prusti-dev](https://github.com/viperproject/prusti-dev)). Its README calls it
a prototype; we could not confirm how actively it is developed today (not
verified).

### Verus

"A tool for verifying the correctness of code written in Rust." Specifications
and proofs are written in Rust syntax (`requires`, `ensures`, `forall`, the
mathematical types `int` and `nat`), checked by Z3, with no run-time checks;
Rust's ownership handles most aliasing reasoning
([Verus guide](https://verus-lang.github.io/verus/guide/)). The guide covers
proofs about concurrent code. Supporting all of Rust and its libraries is an
explicit non-goal, so verified code is written in Verus's subset.

### Creusot

"A deductive verifier for Rust code" that "verifies your code is safe from
panics, overflows, and assertion failures" by translating Rust to Coma, an
intermediate language of the Why3 platform
([creusot](https://github.com/creusot-rs/creusot)). Mutable borrows are
handled with prophecies (from the Creusot papers; not stated on the README).

### Flux

"A refinement type checker for Rust" ([flux](https://github.com/flux-rs/flux),
[site](https://flux-rs.github.io)): types carry logical predicates (for
example, an index type refined to be less than a vector's length) and the
checker discharges them with an SMT solver. Lighter than full contracts, but
limited to what refinements can state.

### Miri

"An Undefined Behavior detection tool for Rust" that runs test suites and
detects out-of-bounds and use-after-free accesses, invalid values, data races
with *some* weak-memory effects, Stacked or Tree Borrows aliasing violations
(experimental) and leaks ([Miri README](https://github.com/rust-lang/miri)).
It checks only the executions the tests reach, on the schedules its
interpreter picks.

### When each fits

| Tool | Bound | Effort | Fits |
|---|---|---|---|
| Kani | Inputs up to a bound; loops unwound | Harnesses only; no annotations in the code | Small arithmetic and parsing functions; unsafe blocks; "no panic for any input" |
| Prusti / Creusot | Unbounded | Contracts and loop invariants in the code | Functional correctness of sequential algorithms |
| Verus | Unbounded | Code written in Verus's subset, with proofs | New components designed for proof (allocators, storage, OS code) |
| Flux | Unbounded | Refined types on signatures | Index and range safety across a module |
| Miri | The executions tests reach | None | Unsafe code, aliasing |
| TLA+ (for comparison) | Model states up to a bound | A separate model, kept in sync by trace replay | Protocols across turns, actors, failures and time |

## Who uses it

- **Firecracker (AWS) runs Kani in CI.** Kani checked the I/O rate limiter
  (with `clock_gettime` stubbed by nondeterministic monotonic time) and the
  VirtIO transport. It found five rate-limiter bugs, among them an integer
  division rounding down that let a guest "do up to 0.01% more I/O than
  configured", and a VirtIO crash a malicious guest could trigger at boot.
  Firecracker runs "27 Kani harnesses across 3 verification suites" in CI,
  about 15 minutes, on every change
  ([Kani blog, 2023](https://model-checking.github.io/kani-verifier-blog/2023/08/31/using-kani-to-validate-security-boundaries-in-aws-firecracker.html)).
  The rounding bug is the same kind of bug a backoff or budget window has.
- **Verifying the Rust standard library.** AWS launched a challenge-based effort
  in late 2024, hosted with the Rust Foundation, with awards per challenge; Kani
  is the main tool ([AWS blog](https://aws.amazon.com/blogs/opensource/verify-the-safety-of-the-rust-standard-library),
  [Rust Foundation](https://foundation.rust-lang.org/news/rust-foundation-collaborates-with-aws-initiative-to-verify-rust-standard-libraries),
  [model-checking/verify-rust-std](https://github.com/model-checking/verify-rust-std)).
  A progress report counts 361 of 9,078 unsafe functions and safe abstractions
  in `core` annotated with Kani contracts
  ([Le Blanc and Lam, arXiv 2510.01072](https://arxiv.org/pdf/2510.01072v1)).
- **s2n-quic.** AWS has described Kani use in s2n-quic; we did not find it in
  the project's README and did not confirm it from a primary source (not
  verified).
- Prusti, Verus, Creusot and Flux: academic and research users are reported in
  their papers; we did not verify named production users and claim none here.

## What it means for Spawned

- **Kani for small arithmetic (batch 1 of the
  [database-grade testing program](../../ROADMAP.md#database-grade-testing-program)).**
  The targets are pure functions with arithmetic that tests sample sparsely:
  `Backoff::delay` ([`supervision.rs:69`](../../concurrency/src/supervision.rs)),
  the jitter step, the restart-budget window (`RetryBudget::take`,
  [`supervision.rs:491`](../../concurrency/src/supervision.rs), a `VecDeque`
  of attempt times, so its harness bounds `max_restarts` and the number of
  calls) and generation counters. Harness
  properties, all over `kani::any()` inputs: no panic or overflow for any
  `failures`, `initial`, `maximum` and seed; the result never exceeds
  `maximum` nor `initial * 2^failures`; without jitter the bound is monotone in
  `failures`; with jitter the delay never exceeds the unjittered bound; the
  budget window admits at most `max_restarts` restarts per window and forgets
  failures older than the window; a generation never repeats. These close the
  surviving `Backoff::delay` and jitter mutants listed in
  [testing to add](../../ROADMAP.md#testing-to-add), and each harness should be
  shown to fail on a mutant before it is trusted (the same negative-control
  rule the TLA+ models follow).
- **TLA+ stays the tool for protocols.** Supervision, shutdown and links are
  races across actors and time; Kani does not support concurrency, and
  deductive tools would need the whole lifecycle in a verifiable subset. The
  [verification rule](../../ROADMAP.md#verification-rule) (model, trace replay,
  negative controls) is unchanged; Kani adds proofs underneath it.
- **Miri waits** (rank 3): all crates are `#![forbid(unsafe_code)]`, so Miri
  would find nothing Rust's type system has not already excluded. It becomes a
  CI job the day an unsafe envelope or reply-slot optimisation is proposed, as
  phase 2 states.
- **Verus, Creusot, Prusti, Flux: not now.** Their payoff is unbounded proofs
  of functional correctness, at the cost of annotating or rewriting the code.
  Spawned's risky code is concurrent lifecycle logic, where they help least. A
  possible later target is a small, self-contained primitive (a bounded
  mailbox or reply slot for ranks 11 and 17) if it gains unsafe code; revisit
  then.

## Beyond TLC: proving protocol invariants (Apalache, TLAPS, Veil)

TLC checks the TLA+ models in small configurations (two or three children, a
few restarts). A bug that needs a larger configuration than the model is
invisible to it; the shutdown-order race CI caught on PR #204 needed a specific
interleaving that a small model can miss. Three tools prove invariants for any
number of children and restarts instead:

- **Apalache** (symbolic model checker for TLA+, SMT-based). Checks that an
  invariant is *inductive*: it holds initially and every step preserves it. If
  so, it holds for all reachable states, with no bound on the model's size for
  that check. Works on the existing `Supervision.tla`, `Cohorts` and
  `NestedSupervision` specs, so trace validation keeps working unchanged. It
  needs type annotations and finite domains for the check itself.
- **TLAPS** (TLA+ proof system). Machine-checked proofs over the same specs;
  more manual than Apalache. Used for a few critical invariants only.
- **Veil** (verse-lab, Apache-2.0, [veil.dev](https://veil.dev)). A framework
  on Lean 4 for specifying transition systems, model checking them and proving
  safety and liveness. Descends from Ivy: specifications are written in a style
  that admits push-button SMT proofs of safety, with Lean as the fallback.
  Finds inductive invariants interactively (counterexamples to induction) or
  automatically (UPDR). Veil 2.0 adds verified model checkers for safety and
  liveness (the authors report 2–20x faster than TLC), an LTL proof mode, a
  PlusCal-style DSL and an LLM skill that translates TLA+ into Veil. Costs: a
  second specification language beside TLA+, which our trace validation does
  not read; Lean when automation fails; a young academic tool whose language
  may change.

Candidate invariants for any of them: a stopped generation is never readmitted
(`AtMostOneGeneration`); granted plus reserved restarts never exceed
`max_restarts` in the window (`BudgetBounded`); shutdown stops admissions in
reverse order (`ReverseShutdown`); a published child is live (`PublishedIsLive`).

**Order.** Apalache inductive checks on the existing models first (phase 4,
batch 2): no new language. Veil next, as a time-boxed spike (about two days):
translate `Supervision.tla` with its TLA+ skill, prove `AtMostOneGeneration` and
`BudgetBounded` for unbounded children, and compare its model checker against
TLC on the same configuration. Adopt Veil only if Apalache cannot handle our
models or we need liveness proofs (every accepted request is eventually answered
or `ActorStopped`, rank 11). TLA+ stays the source of truth for trace validation
either way.

## Sources

- [Kani Rust Verifier](https://model-checking.github.io/kani/); [Kani: dyn trait paper, ICSE-SEIP 2022 preprint](https://cs.wellesley.edu/~avh/dyn-trait-icse-seip-2022-preprint.pdf)
- [Using Kani to validate security boundaries in AWS Firecracker (Kani blog, 2023)](https://model-checking.github.io/kani-verifier-blog/2023/08/31/using-kani-to-validate-security-boundaries-in-aws-firecracker.html)
- [Verify the safety of the Rust standard library (AWS Open Source blog)](https://aws.amazon.com/blogs/opensource/verify-the-safety-of-the-rust-standard-library); [Rust Foundation announcement](https://foundation.rust-lang.org/news/rust-foundation-collaborates-with-aws-initiative-to-verify-rust-standard-libraries); [verify-rust-std](https://github.com/model-checking/verify-rust-std); [Lessons learned so far from verifying the Rust standard library (arXiv 2510.01072)](https://arxiv.org/pdf/2510.01072v1)
- [Prusti](https://github.com/viperproject/prusti-dev)
- [Verus guide](https://verus-lang.github.io/verus/guide/)
- [Creusot](https://github.com/creusot-rs/creusot)
- [Flux](https://github.com/flux-rs/flux), [Flux site](https://flux-rs.github.io)
- [Miri](https://github.com/rust-lang/miri)
- [Apalache](https://apalache-mc.org/), [inductive invariants in Apalache](https://apalache-mc.org/docs/apalache/running.html)
- [TLAPS](https://proofs.tlapl.us/)
- [Veil](https://veil.dev), [source](https://github.com/verse-lab/veil), [playground](https://try.veil.dev); UPDR: [Karbyshev et al., doi:10.1145/3022187](https://doi.org/10.1145/3022187)
