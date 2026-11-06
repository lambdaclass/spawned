# Generic inline future review

Agy's initial consultation considered safe pin projection, in-place replacement
and generic task storage feasible, without claiming tested implementation or a
speedup. Its illustrative macro contained invalid placeholder/attribute syntax;
the implementation follows the documented pin-project-lite PinnedDrop interface.
That consultation is design input, not an implementation approval.

The implemented wrapper and paired controller were submitted for a separate
inline-code review. Linux gates, pin-address/drop-count probes and measurements
remain pending at this checkpoint. Both wrapper variants retain ordinary Rust's
double-panic limitation. No custom unsafe code or allocation-free claim is made.

Raw consultation: /private/tmp/spawned-inline-feasibility-agy.log
Implementation review: /private/tmp/spawned-inline-implementation-agy.log

The implementation review returned APPROVE FOR TESTING with no blockers, checking
pin projection/in-place disposal, startup options, typed task construction and the
paired sample matrix. Qualify its description of a named LeaseFuture with ordered
fields: the source uses an async move wrapper with a local Lease, so that named
struct/field-order statement is not source evidence. Actual lease release must
be checked by joined teardown tests. It also used a descriptive name for the
existing sequential-panic unit; the actual test remains
runnable_wrapper_separates_poll_and_disposal_unwinds. Neither nomenclature is a
claim that server tests already ran. No whole-program safety proof is asserted.
