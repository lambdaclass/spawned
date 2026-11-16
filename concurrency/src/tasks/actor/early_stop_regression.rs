use super::*;
struct EarlyStop;
impl Actor for EarlyStop {
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.stop();
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        panic!("cleanup failed");
    }
}
#[test]
fn panic_during_early_stop_is_reported() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::panic_during_early_stop_is_reported"),
        move || {
            let rt = rt::Runtime::new().unwrap();
            let _enter = rt.enter();
            let actor = EarlyStop.start();
            assert!(
                matches!(rt.block_on(actor.wait_exit()), ExitReason::Panic(message) if message.contains("cleanup failed"))
            );
        },
    );
}
