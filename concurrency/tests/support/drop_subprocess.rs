//! Drain both pipes concurrently; a regression must fail without hanging CI.
use std::{
    io::Read,
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

/// What an ignored child test produced when run in its own process.
pub struct Outcome {
    pub status: ExitStatus,
    pub output: String,
    pub errors: String,
}

/// Run one ignored test of this binary in a child process with `env` added,
/// killing it after `limit`. Returns `None` on timeout.
pub fn spawn(test: &str, env: &[(&str, String)], limit: Duration) -> Option<Outcome> {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SPAWNED_MAILBOX_DROP_CHILD", "1")
        .env("RUST_BACKTRACE", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    fn reader(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut text = String::new();
            pipe.read_to_string(&mut text).unwrap();
            text
        })
    }
    let output = reader(child.stdout.take().unwrap());
    let errors = reader(child.stderr.take().unwrap());
    let until = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= until {
            child.kill().unwrap();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = output.join().unwrap();
    let errors = errors.join().unwrap();
    status.map(|status| Outcome {
        status,
        output,
        errors,
    })
}

#[allow(dead_code)]
pub fn run(test: &str, marker: &str) {
    run_with(test, marker, &[]);
}

#[allow(dead_code)]
pub fn run_with(test: &str, marker: &str, env: &[(&str, String)]) {
    let Some(Outcome {
        status,
        output,
        errors,
    }) = spawn(test, env, Duration::from_secs(120))
    else {
        panic!("{test} timed out");
    };
    assert!(
        status.success(),
        "{test} failed ({status}): {output}\n{errors}"
    );
    assert!(
        output.contains(marker),
        "{test} did not complete its cases: {output}"
    );
}
