//! Checks semantics guarantee P2 (`docs/design/semantics.md`): with
//! `panic = "abort"` a panicking handler ends the process, with no `stopped()`,
//! no exit reason and no reply, while supervision of `Kill`, `Shutdown` and
//! `Normal` exits keeps working. Build it with the abort profile:
//!
//! ```sh
//! cargo run --locked -p spawned-concurrency --example panic_abort --profile panic-abort
//! ```
//!
//! Without arguments it is the parent: it runs itself once per scenario in a
//! child process and checks how each child ended. Test harnesses always unwind,
//! so this is an example binary rather than a `#[test]`.
use spawned_concurrency::{
    message::Message,
    supervision::{Backoff, ChildSpec, RestartMode, SupervisorConfig},
    system::ActorSystem,
    ExitReason,
};
use std::{
    io::Read,
    process::{Command, ExitStatus, Stdio},
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(20);
const PANIC_TEXT: &str = "handler panic under abort";

struct Boom;
impl Message for Boom {
    type Result = u64;
}

mod native {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    struct Victim;
    impl Actor for Victim {
        fn stopped(&mut self, _: &Context<Self>) {
            println!("stopped hook ran");
        }
    }
    impl Handler<Boom> for Victim {
        fn handle(&mut self, _: Boom, _: &Context<Self>) -> u64 {
            panic!("{PANIC_TEXT}")
        }
    }
    pub fn handler_panic() {
        let actor = Victim.start();
        println!("request sent");
        let result = actor.request_with_timeout(Boom, LIMIT);
        println!("caller observed {result:?}");
        println!("exit reason {:?}", actor.wait_exit());
    }
}

mod tasks {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    struct Victim;
    impl Actor for Victim {
        async fn stopped(&mut self, _: &Context<Self>) {
            println!("stopped hook ran");
        }
    }
    impl Handler<Boom> for Victim {
        async fn handle(&mut self, _: Boom, _: &Context<Self>) -> u64 {
            panic!("{PANIC_TEXT}")
        }
    }
    pub fn handler_panic() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let actor = Victim.start();
            println!("request sent");
            let result = actor.request_with_timeout(Boom, LIMIT).await;
            println!("caller observed {result:?}");
            println!("exit reason {:?}", actor.wait_exit().await);
        });
    }

    struct Child;
    impl Actor for Child {
        async fn stopped(&mut self, _: &Context<Self>) {
            println!("stopped hook ran");
        }
    }

    /// No panic anywhere: a Transient child stopped with `Kill` restarts, one
    /// stopped with `Shutdown` stays stopped, and shutdown completes.
    pub fn supervised_exits() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("abort", 1).unwrap();
            let config = SupervisorConfig {
                backoff: Backoff {
                    initial: Duration::from_millis(1),
                    maximum: Duration::from_millis(1),
                    jitter: false,
                    ..Backoff::default()
                },
                ..SupervisorConfig::default()
            };
            let mut tree = system.supervisor(config).unwrap();
            let child = tree
                .add(
                    ChildSpec::new("child", &group, |_| async { Ok::<_, String>(Child) })
                        .restart(RestartMode::Transient),
                )
                .unwrap();
            let supervisor = tree.start().await.unwrap();
            let first = child.snapshot().generation;
            let old = child.current().unwrap();
            old.child_handle().stop_with_reason(ExitReason::Kill);
            assert_eq!(old.wait_exit().await, ExitReason::Kill);
            let new = tokio::time::timeout(LIMIT, child.wait_for_generation(first))
                .await
                .expect("no restart after Kill")
                .expect("child unavailable after Kill");
            new.child_handle().shutdown();
            assert_eq!(new.wait_exit().await, ExitReason::Shutdown);
            let stayed = tokio::time::timeout(LIMIT, child.wait_for_generation(first + 1)).await;
            assert!(
                matches!(stayed, Ok(Err(_))),
                "Transient child restarted after Shutdown"
            );
            assert!(supervisor.shutdown(LIMIT).await.complete);
            assert!(system.shutdown(LIMIT).await.complete);
            println!("supervised exits ok");
        });
    }
}

struct Outcome {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_child(scenario: &str) -> Outcome {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg(scenario)
        .env("RUST_BACKTRACE", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        err.read_to_string(&mut text).unwrap();
        text
    });
    let (done, waited) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(child.wait().unwrap()));
    let status = waited
        .recv_timeout(LIMIT * 2)
        .unwrap_or_else(|_| panic!("scenario {scenario} hung"));
    Outcome {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    }
}

fn aborted(status: &ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(6) // SIGABRT
    }
    #[cfg(not(unix))]
    {
        !status.success()
    }
}

fn parent() {
    for scenario in ["threads-handler-panic", "tasks-handler-panic"] {
        let outcome = run_child(scenario);
        assert!(
            aborted(&outcome.status),
            "{scenario}: expected an abort, got {:?}\nstdout:\n{}\nstderr:\n{}",
            outcome.status,
            outcome.stdout,
            outcome.stderr
        );
        assert!(outcome.stdout.contains("request sent"), "{scenario}");
        for absent in ["stopped hook ran", "caller observed", "exit reason"] {
            assert!(
                !outcome.stdout.contains(absent),
                "{scenario}: `{absent}` printed under abort:\n{}",
                outcome.stdout
            );
        }
        assert!(outcome.stderr.contains(PANIC_TEXT), "{scenario}: {}", outcome.stderr);
        println!("{scenario}: process aborted, no stopped(), no reply");
    }
    let outcome = run_child("supervised-exits");
    assert!(
        outcome.status.success(),
        "supervised-exits failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        outcome.status,
        outcome.stdout,
        outcome.stderr
    );
    assert!(outcome.stdout.contains("supervised exits ok"));
    assert!(outcome.stdout.contains("stopped hook ran"));
    println!("supervised-exits: Kill restarted, Shutdown stayed stopped");
}

fn main() {
    assert!(
        cfg!(panic = "abort"),
        "build with --profile panic-abort (panic = \"abort\")"
    );
    match std::env::args().nth(1).as_deref() {
        None => parent(),
        Some("threads-handler-panic") => native::handler_panic(),
        Some("tasks-handler-panic") => tasks::handler_panic(),
        Some("supervised-exits") => tasks::supervised_exits(),
        Some(other) => panic!("unknown scenario {other}"),
    }
}
