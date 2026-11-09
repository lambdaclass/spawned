#!/usr/bin/env python3
"""Fail CI when native integration coverage is accidentally feature-gated away."""
from pathlib import Path
import sys

required = {
    "native_explicit_exit_is_cooperative_and_first_reason_wins",
    "generated_actor_scenarios",
    "lifecycle_race_regression",
    "controlled_failures_reach_every_required_window_on_every_backend",
    "finite_model_witnesses_replay_against_all_execution_modes",
    "concurrent_histories_have_a_legal_sequential_explanation",
    "synchronous::macro_receives_linked_exit",
    "timers_release_payloads_on_actor_exit",
    "synchronous::protocol_timeout_is_used_by_native_requests",
    "synchronous::timeout_expression_precedes_argument_moves_without_capturing_user_names",
    "synchronous::configured_handles_do_not_shadow_protocol_method_names",
    "synchronous::method_override_and_configured_type_erasure_work_without_async",
}
listed = {line.removesuffix(": test") for line in Path(sys.argv[1]).read_text().splitlines() if line.endswith(": test")}
missing = required - listed
if missing:
    raise SystemExit("native integration tests missing: " + ", ".join(sorted(missing)))
print(f"Native integration coverage present: {len(required)} required tests")
