#!/usr/bin/env python3
"""Validate framework_metrics stdout with Prometheus's Python client parser.

Install prometheus-client==0.21.1, then pipe the metrics example into this tool.
"""
import math
import sys

from prometheus_client.parser import text_string_to_metric_families


def check(text):
    families = list(text_string_to_metric_families(text))
    by_name = {family.name: family for family in families}
    assert len(by_name) == len(families), "duplicate metric family"
    assert all(f.type != "unknown" for f in families), "undeclared metric family"
    for name in ("spawned_queue_wait_seconds", "spawned_handler_duration_seconds",
                 "spawned_request_latency_seconds"):
        histogram = by_name[name]
        maximum = by_name[name + "_max"]
        assert histogram.type == "histogram", name
        assert maximum.type == "gauge", maximum.name
        assert [s.name for s in maximum.samples] == [name + "_max"]
        assert all(s.name in {name + "_bucket", name + "_sum", name + "_count"}
                   for s in histogram.samples), "foreign histogram sample"
        counts = [s.value for s in histogram.samples if s.name == name + "_count"]
        buckets = [s.value for s in histogram.samples if s.name == name + "_bucket"]
        assert len(counts) == 1 and buckets[-1] == counts[0] > 0
        assert buckets == sorted(buckets), "noncumulative histogram buckets"
        assert maximum.samples[0].value > 0, "example did not exercise timing"
    assert all(math.isfinite(s.value) and s.value >= 0
               for f in families for s in f.samples)


if __name__ == "__main__":
    exposition = sys.stdin.read()
    check(exposition)
    # Regression control: v0.7.0's undeclared maxima must fail this check.
    old = "\n".join(line for line in exposition.splitlines()
                    if not (line.startswith("# TYPE spawned_") and "_max gauge" in line))
    try:
        check(old)
    except AssertionError:
        pass
    else:
        raise AssertionError("negative control accepted undeclared maxima")
    print("Prometheus histogram/gauge exposition and regression control passed")
