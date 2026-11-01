//! The `rok bench` runner (spec 9.1).
//!
//! Each benchmark runs in its own process with inherited stdout, so the
//! library builds a `rok::core::bench::Bench` from the arguments and the
//! measured code prints itself: calibration, sampling, statistics and
//! formatting all live in the library. This runner only selects the
//! benchmarks, passes the sampling options, the output form and the name
//! width, and reports which ran. Its own lines go to `out`. For `format
//! json` it reads each child's stdout instead: the `#rokbench` line
//! becomes a record, and the rest goes to `out` with the runner's lines.

use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

use rokc_core::term::{self, Role, Stream};
use rokc_diagnostic::Json;

use crate::{count, ms, re_match, record};

/// A benchmark process's exit code when it never called `b.iter`.
const MEASURED_NOTHING: i32 = 3;

pub struct Options<'a> {
    pub filter: Option<&'a str>,
    /// Calls per sample; `None`: the library calibrates (passed as 0).
    pub iters: Option<u64>,
    pub samples: u64,
    /// `--format=rokbench`: `#rokbench` lines instead of the human ones.
    pub rokbench: bool,
}

/// Run `benches` from `exe`, each in its own process with inherited stdout
/// so the library prints directly; whether all ran. The runner's own lines
/// go to `out`.
pub fn run(exe: &Path, benches: &[String], opts: &Options<'_>, out: &mut dyn Write) -> bool {
    run_to(exe, benches, opts, out, None)
}

/// [`run`]; with `records`, a `format json` record per benchmark and a
/// summary go there, and everything else the children print goes to
/// `out`.
pub fn run_to(
    exe: &Path,
    benches: &[String],
    opts: &Options<'_>,
    out: &mut dyn Write,
    mut records: Option<&mut (dyn Write + '_)>,
) -> bool {
    let started = Instant::now();
    let selected: Vec<&String> = benches
        .iter()
        .filter(|b| opts.filter.is_none_or(|f| re_match(f, b)))
        .collect();
    let width = selected.iter().map(|b| b.len()).max().unwrap_or(0);
    let _ = writeln!(
        out,
        "\nbenchmarking {} bench{}",
        selected.len(),
        if selected.len() == 1 { "" } else { "es" }
    );
    let iters = opts.iters.unwrap_or(0).to_string();
    let samples = opts.samples.to_string();
    let width_s = width.to_string();
    let machine = opts.rokbench || records.is_some();
    // Where `out` goes, for its colour.
    let stream = if records.is_some() {
        Stream::Stderr
    } else {
        Stream::Stdout
    };
    let mut failed = 0usize;
    let mut idle = 0usize;
    for name in &selected {
        // The child inherits stdout; flush ours so lines stay in order.
        let _ = out.flush();
        let t = Instant::now();
        let mut c = Command::new(exe);
        rokc_core::strip_handoffs(&mut c);
        c.args([
            name.as_str(),
            iters.as_str(),
            samples.as_str(),
            if machine { "0" } else { "1" },
            width_s.as_str(),
        ]);
        let mut line = None;
        let status = if records.is_some() {
            c.stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .output()
                .map(|o| {
                    line = protocol(&String::from_utf8_lossy(&o.stdout), out);
                    o.status
                })
        } else {
            c.status()
        };
        let result = match status {
            Ok(s) if s.success() => "ok",
            Ok(s) if s.code() == Some(MEASURED_NOTHING) => {
                idle += 1;
                let _ = writeln!(
                    out,
                    "  {name:<width$}  {} measured nothing: time the code with `b.iter(|| ..)`",
                    term::label(stream, Role::Warning)
                );
                "ignored"
            }
            Ok(s) => {
                failed += 1;
                let _ = writeln!(out, "  {name:<width$}  FAILED ({})", how(s));
                "failed"
            }
            Err(e) => {
                failed += 1;
                let _ = writeln!(out, "  {name:<width$}  FAILED ({e})");
                "failed"
            }
        };
        if let Some(rec) = records.as_deref_mut() {
            let mut fields = vec![
                ("name", Json::str(*name)),
                ("result", Json::str(result)),
                ("ms", ms(t.elapsed())),
            ];
            if result == "ok" {
                fields.extend(line.iter().flatten().map(|(k, v)| (*k, v.clone())));
            }
            record(rec, "case", "bench", fields);
        }
    }
    let idle_s = if idle > 0 {
        format!("{idle} measured nothing; ")
    } else {
        String::new()
    };
    let ran = selected.len() - failed - idle;
    let filtered_out = benches.len() - selected.len();
    let _ = writeln!(
        out,
        "\nbench result: {}. {ran} run; {failed} failed; {idle_s}{filtered_out} filtered out; \
         finished in {:.2}s\n",
        if failed == 0 { "ok" } else { "FAILED" },
        started.elapsed().as_secs_f64()
    );
    if let Some(rec) = records {
        let result = if failed == 0 { "ok" } else { "failed" };
        record(
            rec,
            "summary",
            "bench",
            vec![
                ("result", Json::str(result)),
                ("run", count(ran)),
                ("failed", count(failed)),
                ("ignored", count(idle)),
                ("filtered_out", count(filtered_out)),
                ("ms", ms(started.elapsed())),
            ],
        );
    }
    failed == 0
}

fn how(s: ExitStatus) -> String {
    s.code()
        .map_or_else(|| "signal".to_owned(), |c| format!("exit {c}"))
}

/// The `#rokbench` fields a record carries; `name` comes from the runner.
const STATS: [&str; 7] = [
    "median_ns",
    "mad_ns",
    "samples",
    "outliers",
    "gc",
    "bytes",
    "items",
];

/// The statistics of a child's last `#rokbench` line; every other line
/// goes to `out`.
fn protocol(stdout: &str, out: &mut dyn Write) -> Option<Vec<(&'static str, Json)>> {
    let lines: Vec<&str> = stdout.lines().collect();
    let last = lines.iter().rposition(|l| l.starts_with("#rokbench "));
    for (i, l) in lines.iter().enumerate() {
        if Some(i) != last {
            let _ = writeln!(out, "{l}");
        }
    }
    let line = lines[last?];
    let mut stats = Vec::new();
    for pair in line.split_whitespace().skip(1) {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if let (Some(k), Ok(v)) = (STATS.iter().find(|s| **s == k), v.parse::<f64>()) {
            stats.push((*k, Json::num(v)));
        }
    }
    Some(stats)
}
