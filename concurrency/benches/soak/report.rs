//! Thresholds, trend fitting and the pass/fail summary.
use std::{collections::HashMap, fmt::Write as _, path::Path};

/// Judged series, in CSV column order after `elapsed_s,phase`. RSS and heap
/// are in MiB; the rest are counts.
pub const SERIES: &[&str] = &[
    "rss_mb",
    "heap_mb",
    "threads",
    "fds",
    "tokio_tasks",
    "registry",
    "actors_live",
    "actors_active",
    "mailbox_depth",
    "handlers_active",
    "requests_active",
];

/// Flat `key = number` lines (valid TOML with dotted keys); `#` comments.
pub struct Thresholds(HashMap<String, f64>);
impl Thresholds {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut map = HashMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("{}:{}: expected key = value", path.display(), n + 1))?;
            let value: f64 = value
                .trim()
                .parse()
                .map_err(|_| format!("{}:{}: not a number", path.display(), n + 1))?;
            map.insert(key.trim().to_string(), value);
        }
        Ok(Self(map))
    }
    pub fn get(&self, key: &str) -> Option<f64> {
        self.0.get(key).copied()
    }
    pub fn or(&self, key: &str, default: f64) -> f64 {
        self.get(key).unwrap_or(default)
    }
}

#[derive(Clone, Copy)]
pub enum Phase {
    Baseline,
    Warmup,
    Steady,
    Quiesce,
}
impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Baseline => "baseline",
            Phase::Warmup => "warmup",
            Phase::Steady => "steady",
            Phase::Quiesce => "quiesce",
        }
    }
}

#[derive(Clone)]
pub struct Sample {
    pub elapsed: f64,
    pub phase: Phase,
    pub values: [f64; 11],
    pub started: u64,
    pub scenarios: u64,
    pub errors: u64,
}

pub fn csv_header() -> String {
    format!(
        "elapsed_s,phase,{},actors_started,scenarios,errors",
        SERIES.join(",")
    )
}
pub fn csv_row(s: &Sample) -> String {
    let mut row = format!("{:.1},{}", s.elapsed, s.phase.name());
    for (i, v) in s.values.iter().enumerate() {
        if i < 2 {
            write!(row, ",{v:.3}").unwrap();
        } else {
            write!(row, ",{v}").unwrap();
        }
    }
    write!(row, ",{},{},{}", s.started, s.scenarios, s.errors).unwrap();
    row
}

/// Least-squares slope per hour and its standard error.
pub fn slope_per_hour(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = points.len() as f64;
    if points.len() < 3 {
        return None;
    }
    let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let slope = sxy / sxx;
    let residual: f64 = points
        .iter()
        .map(|p| (p.1 - my - slope * (p.0 - mx)).powi(2))
        .sum();
    let se = (residual / (n - 2.0) / sxx).sqrt();
    Some((slope * 3600.0, se * 3600.0))
}

pub struct Verdict {
    pub pass: bool,
    pub text: String,
}

pub struct RunInfo<'a> {
    pub args: &'a str,
    pub warmup_secs: f64,
    pub runs: Vec<(&'static str, u64)>,
    pub capacity: u64,
    pub errors: u64,
    pub first_errors: Vec<String>,
    pub quiesce_secs: f64,
    pub quiesce_settled: bool,
    pub lanes_joined: bool,
    pub system_shutdown: Result<(), String>,
}

/// The minimum of each of about 20 consecutive windows, at the window's
/// midpoint. Counts swing with the workload (a burst, a batch of Thread-backend
/// actors) and neighbouring samples are correlated, so a fit on raw samples
/// finds spurious trends; a leak raises the floor, which the workload does not.
fn floors(steady: &[&Sample], i: usize) -> Vec<(f64, f64)> {
    let width = (steady.len() / 20).max(5);
    steady
        .chunks(width)
        .filter(|w| w.len() * 2 >= width)
        .map(|w| {
            let mid = (w[0].elapsed + w[w.len() - 1].elapsed) / 2.0;
            (
                mid,
                w.iter().map(|s| s.values[i]).fold(f64::INFINITY, f64::min),
            )
        })
        .collect()
}

/// Judge every series: a significant trend above its per-hour limit after
/// warm-up fails, and so does a final quiesce value above baseline + slack.
pub fn judge(t: &Thresholds, samples: &[Sample], info: &RunInfo) -> Verdict {
    let sigmas = t.or("trend.sigmas", 3.0);
    let baseline = samples
        .iter()
        .rev()
        .find(|s| matches!(s.phase, Phase::Baseline))
        .expect("baseline sample");
    let last = samples.last().expect("final sample");
    let steady: Vec<&Sample> = samples
        .iter()
        .filter(|s| matches!(s.phase, Phase::Steady))
        .collect();
    let mut pass = true;
    let mut table = String::new();
    writeln!(
        table,
        "{:<16} {:>10} {:>10} {:>10} {:>12} {:>10} {:>10}  {:<6} {:<6}",
        "series",
        "baseline",
        "steady-max",
        "quiesce",
        "slope/h",
        "±se/h",
        "limit/h",
        "trend",
        "settle"
    )
    .unwrap();
    for (i, name) in SERIES.iter().enumerate() {
        let points = if *name == "rss_mb" {
            steady.iter().map(|s| (s.elapsed, s.values[i])).collect()
        } else {
            floors(&steady, i)
        };
        let fit = slope_per_hour(&points);
        let limit = t.get(&format!("trend.{name}"));
        let trend_ok = match (fit, limit) {
            (Some((slope, se)), Some(limit)) => !(slope > limit && slope > sigmas * se),
            _ => true,
        };
        let slack = t.get(&format!("quiesce.{name}"));
        let settle_ok = slack.is_none_or(|slack| last.values[i] <= baseline.values[i] + slack);
        pass &= trend_ok && settle_ok;
        let max = steady.iter().map(|s| s.values[i]).fold(f64::NAN, f64::max);
        let (slope, se) = fit.map_or(("n/a".into(), "n/a".into()), |(s, e)| {
            (format!("{s:.3}"), format!("{e:.3}"))
        });
        writeln!(
            table,
            "{:<16} {:>10} {:>10} {:>10} {:>12} {:>10} {:>10}  {:<6} {:<6}",
            name,
            num(baseline.values[i]),
            num(max),
            num(last.values[i]),
            slope,
            se,
            limit.map_or("-".into(), num),
            if limit.is_none() {
                "-"
            } else if trend_ok {
                "ok"
            } else {
                "FAIL"
            },
            match slack {
                None => "-".to_string(),
                Some(_) if settle_ok => "ok".into(),
                Some(slack) => format!("FAIL(+{})", num(slack)),
            },
        )
        .unwrap();
    }
    let workload_ok = info.errors == 0 && info.lanes_joined && info.system_shutdown.is_ok();
    pass &= workload_ok;
    let mut text = String::new();
    writeln!(text, "spawned soak: {}", if pass { "PASS" } else { "FAIL" }).unwrap();
    writeln!(text, "args: {}", info.args).unwrap();
    writeln!(
        text,
        "elapsed {:.0}s, warm-up {:.0}s, {} steady samples, quiesce {:.1}s ({})",
        last.elapsed,
        info.warmup_secs,
        steady.len(),
        info.quiesce_secs,
        if info.quiesce_settled {
            "settled"
        } else {
            "did not settle"
        }
    )
    .unwrap();
    writeln!(
        text,
        "actors started {}, scenarios {}, capacity rejections {}, workload errors {}",
        last.started,
        info.runs.iter().map(|r| r.1).sum::<u64>(),
        info.capacity,
        info.errors
    )
    .unwrap();
    if !info.lanes_joined {
        writeln!(
            text,
            "FAIL: a lane did not finish within the limit (stuck scenario)"
        )
        .unwrap();
    }
    if let Err(err) = &info.system_shutdown {
        writeln!(text, "FAIL: long-lived system shutdown: {err}").unwrap();
    }
    for err in &info.first_errors {
        writeln!(text, "error: {err}").unwrap();
    }
    text.push('\n');
    text.push_str(&table);
    text.push_str("\nscenario runs:\n");
    for (name, count) in &info.runs {
        writeln!(text, "  {name:<28} {count}").unwrap();
    }
    Verdict { pass, text }
}

fn num(v: f64) -> String {
    if v.is_nan() {
        "n/a".into()
    } else if v.fract() == 0.0 && v.abs() < 1e12 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}
