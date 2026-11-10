//! trybuild compile-fail and pass cases for `#[protocol]` and `#[actor]`.
//!
//! The `.stderr` snapshots in `tests/ui` are compiler-specific. They run only
//! on the toolchain pinned in `.tool-versions` (the one CI uses); on any other
//! compiler the test prints a notice and passes. Set `SPAWNED_UI_TESTS=1` to run
//! them anyway, and `TRYBUILD=overwrite` (on the pinned toolchain) to
//! regenerate the snapshots. See docs/TESTING.md, "Macro diagnostics".
#![cfg(feature = "async")]

use std::process::Command;

fn pinned_toolchain() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../.tool-versions");
    let versions = std::fs::read_to_string(path).expect("read .tool-versions");
    versions
        .lines()
        .find_map(|line| line.strip_prefix("rust "))
        .map(|version| version.trim().to_owned())
        .expect(".tool-versions pins rust")
}

fn current_toolchain() -> Option<String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let output = Command::new(rustc).arg("--version").output().ok()?;
    let version = String::from_utf8(output.stdout).ok()?;
    version.split_whitespace().nth(1).map(str::to_owned)
}

#[test]
fn protocol_diagnostics() {
    let pinned = pinned_toolchain();
    let current = current_toolchain();
    let forced = std::env::var_os("SPAWNED_UI_TESTS").is_some_and(|v| v == "1");
    if current.as_deref() != Some(pinned.as_str()) && !forced {
        eprintln!(
            "skipping macro UI tests: rustc {} is not the pinned {pinned}; \
             set SPAWNED_UI_TESTS=1 to run them anyway",
            current.as_deref().unwrap_or("<unknown>"),
        );
        return;
    }
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui-pass/*.rs");
    tests.compile_fail("tests/ui/*.rs");
}
