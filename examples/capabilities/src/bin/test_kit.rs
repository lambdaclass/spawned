//! `spawned-test`: unit-test an actor by mocking its dependency, probe what it
//! sends, and inject faults (slow reply, crashed dependency, supervised crash).
use spawned_concurrency::{
    actor,
    error::ActorError,
    message::Message,
    protocol,
    supervision::{Backoff, ChildSpec, SupervisorConfig},
    tasks::{Actor, ActorStart as _, Context, Handler, Recipient},
    ExitReason, Response,
};
use spawned_test::{faults, tasks as kit, threads as native_kit};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(2);

// The dependency, reached through a protocol reference.
#[protocol(timeout = Duration::from_millis(100))]
pub trait RatesProtocol: Send + Sync {
    fn rate(&self, currency: String) -> Response<u64>;
}

pub struct Converted {
    pub cents: u64,
}
impl Message for Converted {
    type Result = ();
}

// The actor under test: production code, unchanged for the test.
pub struct Converter {
    rates: RatesRef,
    out: Recipient<Converted>,
}

#[protocol]
pub trait ConverterProtocol: Send + Sync {
    fn convert(&self, currency: String, amount: u64) -> Response<Result<u64, String>>;
}

#[actor(protocol = ConverterProtocol)]
impl Converter {
    #[request_handler]
    async fn convert(
        &mut self,
        msg: converter_protocol::Convert,
        _: &Context<Self>,
    ) -> Result<u64, String> {
        let rate = self
            .rates
            .rate(msg.currency)
            .await
            .map_err(|e| e.to_string())?;
        let cents = rate * msg.amount;
        let _ = self.out.send(Converted { cents });
        Ok(cents)
    }
}

fn converter(rates: RatesRef, out: &kit::Probe<Converted>) -> ConverterRef {
    Converter {
        rates,
        out: out.recipient(),
    }
    .start()
    .to_converter_ref()
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    // 1. Mock the dependency and probe the output.
    let rates = kit::Mock::new()
        .on::<rates_protocol::Rate>(|msg| if msg.currency == "EUR" { 110 } else { 100 })
        .start();
    let out = kit::Probe::<Converted>::new();
    let under_test = converter(rates.to_rates_ref(), &out);
    assert_eq!(under_test.convert("EUR".into(), 3).await.unwrap(), Ok(330));
    assert_eq!(out.expect(WAIT).await.cents, 330);
    let asked: rates_protocol::Rate = rates.expect(WAIT).await;
    assert_eq!(asked.currency, "EUR");
    out.expect_no_message(Duration::from_millis(20)).await;

    // 2. Fault: the dependency is slower than the protocol's 100 ms timeout.
    let slow = kit::Mock::new()
        .on::<rates_protocol::Rate>(|_| 1)
        .delay::<rates_protocol::Rate>(Duration::from_millis(300))
        .start();
    let under_test = converter(slow.to_rates_ref(), &out);
    let error = under_test
        .convert("USD".into(), 1)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error, ActorError::RequestTimeout.to_string());
    out.expect_no_message(Duration::from_millis(20)).await;

    // 3. Fault: the dependency crashes; unmocked messages would crash it too.
    let crashing = kit::Mock::new()
        .crash_on::<rates_protocol::Rate>("rates feed lost")
        .start();
    let under_test = converter(crashing.to_rates_ref(), &out);
    assert!(under_test.convert("USD".into(), 1).await.unwrap().is_err());
    assert_eq!(
        kit::join_within(&crashing, WAIT).await,
        ExitReason::Panic("panic in handler: rates feed lost".into())
    );

    // 4. Fault: crash a supervised child and wait for its replacement; bounded
    //    shutdown fails with a report instead of hanging.
    let test = kit::TestSystem::new();
    let mut tree = test
        .system()
        .supervisor(SupervisorConfig {
            backoff: Backoff {
                initial: Duration::ZERO,
                maximum: Duration::ZERO,
                jitter: false,
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let rates_ref = rates.to_rates_ref();
    let probe = out.clone();
    let child = tree
        .add(ChildSpec::new("converter", test.group(), move |_| {
            let actor = Converter {
                rates: rates_ref.clone(),
                out: probe.recipient(),
            };
            async move { Ok::<_, String>(actor) }
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let before = child.wait_ready().await.unwrap();
    let after = kit::restart_child(&child, ExitReason::Kill, WAIT).await;
    assert_ne!(before.id(), after.id());
    assert_eq!(after.convert("GBP".into(), 2).await.unwrap(), Ok(200));
    assert_eq!(out.expect(WAIT).await.cents, 200);
    assert!(kit::shutdown_supervisor(&supervisor, WAIT).await.complete);
    test.shutdown(WAIT).await;

    // 5. The same mock in threads mode, with an injected stop reason.
    let native = native_kit::Mock::new()
        .on::<rates_protocol::Rate>(|_| 7)
        .start();
    assert_eq!(native.rate("X".into()).unwrap(), 7);
    faults::exit(&native.child_handle(), ExitReason::Shutdown);
    assert_eq!(native_kit::join_within(&native, WAIT), ExitReason::Shutdown);
    rates.assert_no_unexpected();

    println!("Mocks, probes and injected faults verified without touching production code.");
}
