# Framework metrics

Enable the optional `metrics` feature on `spawned-concurrency` (available in v0.6.0):

```toml
spawned-concurrency = { version = "0.6", features = ["metrics"] }
```

The default build compiles out collection, timing metadata and response observers.
No handler, protocol, message or result type needs changing. Metrics do not change
request timeouts, cancel queued work, restart actors or change mailbox capacity.

Configure before starting actors or constructing observed requests:

```rust
use spawned_concurrency::metrics::{self, Config};

metrics::configure(Config { timing_sample_every: 16 }).unwrap();
// Counters include every operation; one in 16 observations gets timing samples.
let snapshot = metrics::snapshot();
let prometheus_text = metrics::render_prometheus();
```

Default sampling is 1 (all timings). Use 0 for counters/gauges without clocks or
histogram observations. Configuration is process-wide and immutable after first
instrumentation; `configure` returns `Err(config)` if already initialized.

## What is measured

| Measurement | Definition |
|---|---|
| Mailbox depth | Waiting user envelopes, including submissions in progress; excludes the active handler and internal Exit/Shutdown controls. Read an individual actor with `actor.mailbox_depth()` or `actor.context().mailbox_depth()`. The snapshot gauge sums all actors. |
| Queue wait | Sampled submission to handler start, including transport/scheduler delay. Submission timing begins before channel publication. |
| Handler duration | Sampled dispatch to completion, panic or future destruction; includes awaited time, not just CPU time. Lifecycle hooks and internal exit callbacks are excluded. |
| Request latency | Caller-wrapper lifetime to observed reply, timeout, actor-stop error or caller cancellation. Includes time awaiting a reply, including scheduling. |
| Request outcomes | Started, active, replied, timed out, actor stopped, and caller cancelled. Transport replies are successful even if the application result itself contains an error. |
| Lifecycle | Actor instances admitted, active, and exited by Normal/Shutdown/Panic/Kill; handler starts, active handlers, completions, panics and interrupted futures. |
| Delivery accounting | Enqueue acceptance/rejection, discarded queued messages, replies sent to channels, and reply receivers already dropped. Enqueue success does not promise handling or durable delivery. |

Counters end in `_total`. Duration histograms use fixed nanosecond buckets
internally and seconds in Prometheus export; their count/sum/max describe sampled
observations only. Counts are not multiplied by the sampling rate. Do not compare
sampled histogram counts to total request counters as if every request was timed.
Sampling is deterministic round-robin per message/request category, not random;
periodic traffic can bias samples. Quantiles are bucket-based estimates.

Mailbox depth reserves accounting before publication to avoid consumer races.
A stalled producer may temporarily appear in depth before send acceptance. A
failed send rolls that reservation back and increments rejection. This is not a
transactional channel-length API. Counters/gauges are independent atomic reads;
conservation relationships apply after operations quiesce:

- accepted messages = handled + discarded + waiting;
- handled = completed handlers + panicked handlers + aborted handlers + active;
- observed requests = replied + timed out + actor stopped + cancelled + active;
- spawned actor instances = exited + active.

An actor is active from creation of its lifecycle guard until terminal cleanup,
including waiting for a blocking-pool slot, `started()` and `stopped()`. A runtime
abort before its first poll still closes lifecycle and mailbox accounting. A
handler future destroyed without returning an outcome counts as aborted; this
does not imply its business effects were rolled back. Native blocking handlers
still stop cooperatively.

## Caller and raw receiver boundaries

Direct `ActorRef::request`, `Context::request`, and erased-recipient `request`
measure from their execution to return. Async methods start when first polled.
Protocol-generated task requests use `Response`: observation starts at response
construction, after the raw send, and ends when the caller polls the result or
drops the wrapper. An unpolled wrapper dropped by its caller counts as cancelled.
A completed reply left unpolled is not a caller-observed reply yet. Timed Response
waits retain their existing first-poll timeout behavior. Synchronous protocol
requests are observed by their blocking request; `Response::ready` does not count
that result a second time. Manually constructed ready results are not requests.

`request_raw` returns the existing mode-specific raw receiver. It cannot observe
custom timeouts or how the caller waits without changing that API. Such requests
still contribute message, handler, queue and reply-delivery metrics, but only a
framework request/Response wrapper contributes caller-outcome/latency metrics.
Reply delivery means a successful channel send, not proof the caller consumed it.
Dropping/timing out a caller does not cancel accepted actor work.

## Collection and exporting limits

Collection uses fixed aggregate atomic counters and three fixed 32-bucket
histograms, plus one shared depth counter per live actor/reference set. There is
no global actor registry, label map, per-message metrics allocation, retained
payload/span, callback or exporter queue. Envelopes contain an inline observation
and reuse the actor's shared depth counter. The disabled build has no shared
metrics counter or timing metadata. Internal control messages are not reported
as application traffic.

Call `snapshot` or `render_prometheus` from your own exporter/HTTP task. Export is
pull-based: collection never calls an exporter, performs network I/O or waits for
an exporter lock. Text output has a fixed set of series without actor/message
names, payloads or actor IDs. An application can add bounded, explicit labels
outside the library. Histogram sums saturate at `u64::MAX` nanoseconds; counters
can wrap after `u64::MAX` events. Scrapes are approximate concurrent observations.

There is no built-in HTTP server, OpenTelemetry exporter, causal tracing,
oldest-queued-message age, per-actor histogram or active-monitor gauge in this
initial implementation. These can be evaluated independently of the existing
metrics and without inserting exporter work into handlers.

[Overhead measurements and reproduction](../research/framework-metrics/2026-11-06/README.md)
compare pre-metrics source, the default build, counters-only, sampled and full
timing. Use application measurements before choosing a sampling rate; shared
atomic counters themselves can contend under high actor/producer concurrency.
