# Owner deque and batch-steal experiment

The next private native-deque policy keeps Runnable task state, the amortized
root driver, shared Tokio I/O, message turns and ordinary addressed mailboxes.
It replaces only ready-work transport/policy: owner-thread LIFO deques plus a
shared external Injector, using optional pinned crossbeam-deque 0.8.8 through
safe APIs. There is no custom unsafe queue code or new public Backend variant.

A Worker is owned by exactly one native worker thread in a TLS slot. Other
threads retain only its Stealer. The worker is installed before startup succeeds
and its guard clears TLS before dropping the Worker handle. Shared Stealers can
retain the buffer/tickets until the pool drops. Queue borrows end before
polling jobs, so actor handlers/destructors can schedule more work. External
wakes enter a global injection queue; internal continuations enter the current
owner's queue. Periodic turns alternate injection-first and local-oldest-first priority, using
the owner's Stealer to inspect oldest work. Owner pop is a progress fallback
after bounded self-steal retries; this is not a hard fairness bound. Idle workers batch-steal
from injection or peer queues. Four Retry attempts are bounded; a retry budget
is not proof of emptiness, and the existing register/recheck/requested 1 ms fallback remains.

External targets rotate and may wake an idle peer when the target is busy.
This changes injection and stealing policy as well as the queue primitive;
performance changes cannot be attributed solely to removing mutex operations.
Local LIFO/oldest accounting, root quanta, Tokio cooperative budgeting and running
handler non-preemption remain separate scheduling limits. No hard latency bound
is promised. Crossbeam's internal synchronization/allocation is not free.

Extend all executor tests, including stale wakers, concurrent teardown,
helper-thread retirement, real TCP/timers and actor-worker blocking, to Deque.
Single-worker cooperative progress covers quanta 1/8/32/128. Future destructor
recovery and subsequent-job tests also cover Deque. Gate and measure on a new
common source, with controls compiled from that same source; never pool V4 or
older observations into new medians. Public Actor/Handler/protocol signatures,
unbounded envelope delivery, reply ownership and Tokio default remain.

Research references:
- [Crossbeam deque API](https://docs.rs/crossbeam-deque/latest/crossbeam_deque/)
- [Crossbeam owner/stealer/injector implementation](https://github.com/crossbeam-rs/crossbeam/blob/main/crossbeam-deque/src/deque.rs)
- [Tokio scheduler design](https://tokio.rs/blog/2019-10-scheduler)

The queue is an executor ready queue, not a message mailbox. Actors still own
ordinary unbounded Tokio mailboxes; this does not adopt Silver's lossy broadcast
rings or Copy-only transport. Historical source329006e completed its initial gates, smokes,560 graphs and16
profiles. Review then found local-bottom starvation under continuous injection
plus hot local continuations. V5/V6/V7 Deque observations remain historical; their
unstarted follow-ups were cancelled. Root/Runnable are unaffected by this defect.

A strengthened deterministic regression fails with the starvation assertion on
old95b4ca6 and passes on corrected e633e59. The first probe omitted hot local
continuations and passed; it is retained as a test-design limitation. The fix
alternates injection/local-oldest priority, adds owner-pop fallback, checks TLS
worker identity and clarifies buffer lifetime. Reverse progress, cross-pool
injection and four-slot peer-batch ticket conservation are also tested.

Frozen e633e59 passed18 spawned-rt unit tests, selected four-worker Deque full
actor suites at Z0W0/Z1W1 (the existing badly_behaved_thread_non_blocking exclusion),
workspace all-feature tests, three release benchmark builds and all-target/
all-feature Clippy on fede. Optional Z/W controls default to zero and remain
research settings; generic inline futures are not retained. New same-source
smokes/graph/scale/offered/resource blocks execute separately before interpreting
performance. No production/default promotion or formal fairness proof follows.
