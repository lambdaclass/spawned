use spawned_concurrency::{lifecycle_trace, supervision::Supervisor};
use std::path::PathBuf;
pub fn record(name: &str, root: &Supervisor) {
    let events = lifecycle_trace::take_tree(root.trace_id());
    let nodes = events
        .iter()
        .map(|e| e.node)
        .chain(events.iter().filter(|e| e.peer > 0).map(|e| e.peer))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(nodes.len() >= 2);
    let directory = std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        });
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("tree-{name}.json")),
        format!(
            "{{\"schema\":\"tree-v1\",\"name\":\"{name}\",\"root\":{},\"events\":[{}]}}",
            root.trace_id(),
            events
                .iter()
                .map(|e| e.to_json())
                .collect::<Vec<_>>()
                .join(",\n")
        ),
    )
    .unwrap();
}
