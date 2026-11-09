//! Drain both pipes concurrently; a regression must fail without hanging CI.
use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub fn run(test: &str, marker: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--ignored", "--nocapture"])
        .env("SPAWNED_MAILBOX_DROP_CHILD", "1")
        .env("RUST_BACKTRACE", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    fn reader(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut text = String::new();
            pipe.read_to_string(&mut text).unwrap();
            text
        })
    }
    let output = reader(child.stdout.take().unwrap());
    let errors = reader(child.stderr.take().unwrap());
    let until = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= until {
            child.kill().unwrap();
            let _ = child.wait();
            panic!(
                "{test} timed out: {}\n{}",
                output.join().unwrap(),
                errors.join().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = output.join().unwrap();
    let errors = errors.join().unwrap();
    assert!(
        status.success(),
        "{test} failed ({status}): {output}\n{errors}"
    );
    assert!(
        output.contains(marker),
        "{test} did not complete its cases: {output}"
    );
}
