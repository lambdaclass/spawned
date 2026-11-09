# Independent read-only review

The background concurrency-ideas agent reviewed the five capability binaries,
their README and the planned catalog. It reported no blocking correctness or API
availability issue. It checked permit-before-allocation ordering and reclamation,
deterministic timeout gates, exclusive producer admission before a drain fence,
stop/join before runtime destruction, and the distinction between existing APIs
and proposed capabilities. It did not run builds or tests.

Two suggestions were implemented: assert the router's background worker placement
and describe the critical actor's returned thread as where it handled the
request; also assert metrics counters, empty active gauges and nonzero histogram
samples after 32 requests. All five final binaries then ran successfully on fede.

The same reviewer found no material defect in StartGate startup unwinding and
retained the outer-watchdog requirement: a rendezvous timeout does not itself
bound taskset subprocess execution or acquisition of the setup mutex.
