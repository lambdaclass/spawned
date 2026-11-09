#![cfg(feature = "async")]
#[test]
fn protocol_diagnostics() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui-pass/*.rs");
    tests.compile_fail("tests/ui/*.rs");
}
