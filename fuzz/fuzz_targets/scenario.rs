//! One generated scenario per input: the input fixes the operations, the
//! simulator seed and the scheduling strategy, so every execution replays.
#![no_main]

use libfuzzer_sys::fuzz_target;

// The fault format: base operations plus fault-injection steps.
fuzz_target!(|data: &[u8]| {
    if let Some(scenario) = spawned_fuzz::decode_faults(data) {
        spawned_fuzz::fuzz_one(&scenario);
    }
});
