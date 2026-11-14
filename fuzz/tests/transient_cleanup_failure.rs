//! CI fuzz seed 14836864006339617: a Transient child stopped normally, then
//! its state destructor panicked. The supervisor correctly reconstructed it;
//! the old V1/V2 oracle ignored cleanup failure when the reason stayed Normal.
//! Keep the original CI input and decision tape, including failed factories.
#![cfg(spawned_sim)]
const INPUT: &[u8] = include_bytes!("../regressions/transient_cleanup_failure.input");
const TAPE: &str = include_str!("../regressions/transient_cleanup_failure.tape");
#[test]
fn original_ci_schedule() {
    let scenario = spawned_fuzz::decode(INPUT).unwrap();
    let tape = spawned_rt::tasks::sim::Tape::parse(TAPE).unwrap();
    let replay = spawned_fuzz::execute(&scenario, Some(&tape));
    assert_eq!(
        replay.failure, None,
        "the original CI tape must replay strictly"
    );
    spawned_fuzz::assert_passes(INPUT, None);
}
#[spawned_macros::sim_test(seeds = 64)]
async fn transient_cleanup_across_schedules(seed: u64) {
    spawned_fuzz::init();
    spawned_fuzz::run_body(spawned_fuzz::decode(INPUT).unwrap(), seed).await;
}
