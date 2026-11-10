//! Text explanations and an HTML timeline for reports.
use super::{Model, Operation, PartitionReport, Refusal, Report, Verdict};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Prefix steps shown before eliding the start of a long prefix.
const SHOWN_PREFIX: usize = 24;

fn describe_op<I: std::fmt::Debug, O: std::fmt::Debug>(
    index: usize,
    op: &Operation<I, O>,
) -> String {
    match &op.returned {
        Some((ret, output)) => format!(
            "#{index} client {} {:?} -> {:?} [{}, {}]",
            op.client, op.input, output, op.call, ret
        ),
        None => format!(
            "#{index} client {} {:?} -> pending [{}, -]",
            op.client, op.input, op.call
        ),
    }
}

pub(super) fn explain<M: Model>(
    report: &Report,
    model: &M,
    history: &[Operation<M::Input, M::Output>],
) -> String {
    let mut out = String::new();
    let checked: usize = report.partitions.iter().map(|p| p.operations.len()).sum();
    let _ = writeln!(
        out,
        "{:?}: {} operations ({} checked) in {} partition(s), {} states, {:?}",
        report.verdict(),
        history.len(),
        checked,
        report.partitions.len(),
        report.states(),
        report.elapsed
    );
    if let Some(reason) = &report.unknown {
        let _ = writeln!(out, "undecided: {reason}");
    }
    for (number, partition) in report.partitions.iter().enumerate() {
        let Some(failure) = &partition.failure else {
            continue;
        };
        let _ = writeln!(
            out,
            "\npartition {number}: {} operations, not linearizable",
            partition.operations.len()
        );
        let _ = writeln!(
            out,
            "  longest linearizable prefix: {} of {} operations",
            failure.prefix.len(),
            partition.operations.len()
        );
        let mut state = model.init();
        let skip = failure.prefix.len().saturating_sub(SHOWN_PREFIX);
        if skip > 0 {
            let _ = writeln!(out, "    ... {skip} earlier steps elided");
        }
        for (step, &index) in failure.prefix.iter().enumerate() {
            let op = &history[index];
            state = model
                .step(&state, &op.input, op.output())
                .expect("prefix replays");
            if step >= skip {
                let _ = writeln!(
                    out,
                    "    {:>4}. {}  => {:?}",
                    step + 1,
                    describe_op(index, op),
                    state
                );
            }
        }
        let blocked = &history[failure.blocked];
        let _ = writeln!(
            out,
            "  cannot linearize {} before its return: in state {:?} no candidate can go next",
            describe_op(failure.blocked, blocked),
            state
        );
        for &(index, refusal) in &failure.candidates {
            let why = match refusal {
                Refusal::Rejected => "model rejects this output",
                Refusal::DeadEnd => "accepted, but every continuation fails",
            };
            let _ = writeln!(out, "    {}: {why}", describe_op(index, &history[index]));
        }
    }
    out
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(super) fn html<M: Model>(
    report: &Report,
    model: &M,
    history: &[Operation<M::Input, M::Output>],
) -> String {
    let shown: Vec<&PartitionReport> = if report.verdict() == Verdict::Illegal {
        report
            .partitions
            .iter()
            .filter(|p| p.failure.is_some())
            .collect()
    } else {
        report.partitions.iter().collect()
    };
    let mut out = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>History timeline</title><style>\
body{font:13px system-ui,sans-serif;margin:16px;background:#fff;color:#222}\
pre{background:#f4f4f4;padding:8px;overflow-x:auto}\
.lane{position:relative;height:26px;border-bottom:1px solid #eee}\
.label{position:absolute;left:0;width:70px;top:5px;color:#666}\
.track{position:absolute;left:76px;right:8px;top:0;bottom:0}\
.op{position:absolute;top:3px;height:18px;border-radius:3px;background:#cfd8dc;font-size:11px;\
overflow:hidden;white-space:nowrap;padding:0 3px;box-sizing:border-box;border:1px solid #90a4ae}\
.lin{background:#c8e6c9;border-color:#43a047}.blocked{background:#ffcdd2;border-color:#e53935}\
.pending{border-style:dashed}\
</style></head><body>",
    );
    let _ = write!(
        out,
        "<h1>{:?}</h1><pre>{}</pre>",
        report.verdict(),
        escape(&explain(report, model, history))
    );
    for (number, partition) in shown.iter().enumerate() {
        let ops = &partition.operations;
        // Compress time to ranks so sparse timestamps stay legible.
        let times: BTreeSet<u64> = ops
            .iter()
            .flat_map(|&i| [Some(history[i].call), history[i].ret()])
            .flatten()
            .collect();
        let rank: BTreeMap<u64, usize> = times.iter().enumerate().map(|(r, &t)| (t, r)).collect();
        let span = (times.len() + 1) as f64;
        let order: BTreeMap<usize, usize> = partition
            .failure
            .iter()
            .flat_map(|f| f.prefix.iter().enumerate().map(|(n, &i)| (i, n + 1)))
            .collect();
        let blocked = partition.failure.as_ref().map(|f| f.blocked);
        let mut lanes: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for &i in ops {
            lanes.entry(history[i].client).or_default().push(i);
        }
        let _ = write!(
            out,
            "<h2>Partition {number} ({:?}, {} operations)</h2><div style=\"min-width:{}px\">",
            partition.verdict,
            ops.len(),
            (times.len() * 14).clamp(600, 20_000)
        );
        for (client, members) in lanes {
            let _ = write!(
                out,
                "<div class=\"lane\"><span class=\"label\">client {client}</span><div class=\"track\">"
            );
            for i in members {
                let op = &history[i];
                let start = rank[&op.call] as f64;
                let end = op.ret().map_or(span, |t| rank[&t] as f64 + 1.0);
                let mut class = String::from("op");
                let mut label = String::new();
                if let Some(n) = order.get(&i) {
                    class.push_str(" lin");
                    label = format!("{n}. ");
                }
                if Some(i) == blocked {
                    class.push_str(" blocked");
                    label = "✗ ".into();
                }
                if op.returned.is_none() {
                    class.push_str(" pending");
                }
                let text = describe_op(i, op);
                let _ = write!(
                    out,
                    "<div class=\"{class}\" style=\"left:{:.3}%;width:{:.3}%\" title=\"{}\">{}{}</div>",
                    start / span * 100.0,
                    (end - start).max(0.5) / span * 100.0,
                    escape(&text),
                    label,
                    escape(&format!("{:?}", op.input))
                );
            }
            out.push_str("</div></div>");
        }
        out.push_str("</div>");
    }
    out.push_str("</body></html>");
    out
}
