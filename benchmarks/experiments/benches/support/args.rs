//! Validate experiment controls before creating threads or measuring anything.
pub fn validate(arguments: &[String], allowed: &[&str]) {
    let mut seen = std::collections::HashSet::new();
    for argument in arguments {
        if argument == "--bench" {
            continue; // Cargo supplies this marker to harness=false benchmarks.
        }
        let (key, value) = argument
            .strip_prefix("--")
            .and_then(|s| s.split_once('='))
            .expect("benchmark controls must use --key=value");
        assert!(allowed.contains(&key), "unknown benchmark argument: {key}");
        assert!(!value.is_empty(), "missing value for: {key}");
        assert!(seen.insert(key), "duplicate benchmark argument: {key}");
    }
}
