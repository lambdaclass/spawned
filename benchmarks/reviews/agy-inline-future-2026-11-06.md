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
