//! One generated scenario per input: the input fixes the operations, the
//! simulator seed and the scheduling strategy, so every execution replays.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|scenario: spawned_fuzz::Scenario| {
    spawned_fuzz::fuzz_one(&scenario);
});
