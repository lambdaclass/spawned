use super::*;
struct EarlyStop;
impl Actor for EarlyStop {
    fn started(&mut self, ctx: &Context<Self>) {
        ctx.stop();
    }
    fn stopped(&mut self, _: &Context<Self>) {
        panic!("stopped() must not run after a failed start");
    }
}
/// `ctx.stop()` in `started()` is a failed start (unified start, 2.3 row
/// 5): `stopped()` does not run and the start reports `InitExited(Normal)`.
#[test]
fn stop_in_started_fails_the_start_without_stopped() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::stop_in_started_fails_the_start_without_stopped"
        ),
        move || {
            let actor = EarlyStop.spawn();
            assert_eq!(actor.wait_exit(), ExitReason::Normal);
            assert!(matches!(
                actor.ready(),
                Err(crate::StartError::InitExited {
                    reason: ExitReason::Normal,
                    pending: None
                })
            ));
            assert!(matches!(
                EarlyStop.start(),
                Err(crate::StartError::InitExited {
                    reason: ExitReason::Normal,
                    pending: None
                })
            ));
        },
    );
}
