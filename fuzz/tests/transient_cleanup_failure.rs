//! CI fuzz seed 14836864006339617: a Transient child stopped normally, then
//! its state destructor panicked. The supervisor correctly reconstructed it;
//! the old V1/V2 oracle ignored cleanup failure when the reason stayed Normal.
//! Keep the original CI input and decision tape, including failed factories.
//! The child controller's wait gained the round's stop token (V3, #310),
//! which changed the task structure: the tape is followed as far as it
//! applies (permissively) once it no longer replays strictly.
#![cfg(spawned_sim)]
const INPUT: &[u8] = include_bytes!("../regressions/transient_cleanup_failure.input");
const TAPE: &str = include_str!("../regressions/transient_cleanup_failure.tape");
#[test]
fn original_ci_schedule() {
    spawned_fuzz::assert_passes(INPUT, Some(TAPE));
}
#[spawned_macros::sim_test(seeds = 64)]
async fn transient_cleanup_across_schedules(seed: u64) {
    spawned_fuzz::init();
    spawned_fuzz::run_body(spawned_fuzz::decode(INPUT).unwrap(), seed).await;
}
