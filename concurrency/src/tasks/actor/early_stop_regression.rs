use super::*;
struct EarlyStop;
impl Actor for EarlyStop {
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.stop();
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        panic!("stopped() must not run after a failed start");
    }
}
/// `ctx.stop()` in `started()` is a failed start (unified start, 2.3 row
/// 5, OTP `{stop, normal}` from `init/1`): `stopped()` does not run, the
/// actor ends `Normal`, and the start's result reports it.
#[test]
fn stop_in_started_fails_the_start_without_stopped() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::stop_in_started_fails_the_start_without_stopped"
        ),
        move || {
            let rt = rt::Runtime::new().unwrap();
            let _enter = rt.enter();
            let actor = EarlyStop.spawn();
            assert_eq!(rt.block_on(actor.wait_exit()), ExitReason::Normal);
            assert!(matches!(
                rt.block_on(actor.ready()),
                Err(crate::StartError::InitExited {
                    reason: ExitReason::Normal,
                    pending: None
                })
            ));
            let waited = rt.block_on(async { EarlyStop.start().await });
            assert!(matches!(
                waited,
                Err(crate::StartError::InitExited {
                    reason: ExitReason::Normal,
                    pending: None
                })
            ));
        },
    );
}
