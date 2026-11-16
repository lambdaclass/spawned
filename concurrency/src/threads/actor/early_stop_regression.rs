
use super::*;
struct EarlyStop;
impl Actor for EarlyStop {
    fn started(&mut self, ctx: &Context<Self>) {
        ctx.stop();
    }
    fn stopped(&mut self, _: &Context<Self>) {
        panic!("cleanup failed");
    }
}
#[test]
fn panic_during_early_stop_is_reported() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::panic_during_early_stop_is_reported"),
        move || {
            let actor = EarlyStop.start();
            assert!(
                matches!(actor.wait_exit(), ExitReason::Panic(message) if message.contains("cleanup failed"))
            );
        },
    );
}
