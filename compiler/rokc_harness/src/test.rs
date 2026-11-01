//! The `rok test` runner (spec 9.1).
//!
//! Each `#[test]` runs in its own process so a panic or crash fails only
//! that test; a thread pool runs them in parallel. The compiler-built
//! executable runs the named test when it is its only argument and exits
//! non-zero if it fails.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rokc_diagnostic::Json;

use crate::{count, ms, re_match, record};

pub struct Options<'a> {
    pub filter: Option<&'a str>,
    pub threads: usize,
    pub show_output: bool,
}

/// A test, how its process ended and how long it took.
type Ran<'a> = (&'a String, Result<Output, String>, Duration);

/// Run `tests` from `exe`, each in its own process; whether all passed.
/// Progress and the summary are written to `out`.
pub fn run(exe: &Path, tests: &[String], opts: &Options<'_>, out: &mut dyn Write) -> bool {
    run_to(exe, tests, opts, out, None)
}

/// [`run`]; with `records`, also a `format json` record per test and a
/// summary there.
pub fn run_to(
    exe: &Path,
    tests: &[String],
    opts: &Options<'_>,
    out: &mut dyn Write,
    mut records: Option<&mut (dyn Write + '_)>,
) -> bool {
    let started = Instant::now();
    let selected: Vec<&String> = tests
        .iter()
        .filter(|t| opts.filter.is_none_or(|f| re_match(f, t)))
        .collect();
    let filtered_out = tests.len() - selected.len();
    let _ = writeln!(
        out,
        "\nrunning {} test{}",
        selected.len(),
        if selected.len() == 1 { "" } else { "s" }
    );

    // Each test runs in its own process across a thread pool; the threads
    // only collect results, and reporting stays single-threaded (`out` is
    // not shared) so a test prints once it and the ones before it finish.
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Ran<'_>>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..opts.threads.clamp(1, selected.len().max(1)) {
            s.spawn(|| {
                while let Some(&name) = selected.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let t = Instant::now();
                    let mut c = Command::new(exe);
                    c.arg(name);
                    rokc_core::strip_handoffs(&mut c);
                    let r = c
                        .output()
                        .map_err(|e| format!("cannot run {}: {e}", exe.display()));
                    results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((name, r, t.elapsed()));
                }
            });
        }
    });
    let mut results = results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    results.sort_by_key(|(n, _, _)| *n);

    let passed = |r: &Result<Output, String>| r.as_ref().is_ok_and(|o| o.status.success());
    for (name, r, took) in &results {
        let _ = writeln!(
            out,
            "test {name} ... {}",
            if passed(r) { "ok" } else { "FAILED" }
        );
        if let Some(rec) = records.as_deref_mut() {
            let result = if passed(r) { "ok" } else { "failed" };
            record(
                rec,
                "case",
                "test",
                vec![
                    ("name", Json::str(*name)),
                    ("result", Json::str(result)),
                    ("ms", ms(*took)),
                ],
            );
        }
    }
    let failed: Vec<_> = results.iter().filter(|(_, r, _)| !passed(r)).collect();
    if opts.show_output {
        let ok: Vec<_> = results.iter().filter(|(_, r, _)| passed(r)).collect();
        section(out, "successes", &ok);
    }
    section(out, "failures", &failed);
    let _ = writeln!(
        out,
        "\ntest result: {}. {} passed; {} failed; {filtered_out} filtered out; finished in {:.2}s\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        results.len() - failed.len(),
        failed.len(),
        started.elapsed().as_secs_f64()
    );
    if let Some(rec) = records {
        let result = if failed.is_empty() { "ok" } else { "failed" };
        record(
            rec,
            "summary",
            "test",
            vec![
                ("result", Json::str(result)),
                ("passed", count(results.len() - failed.len())),
                ("failed", count(failed.len())),
                ("filtered_out", count(filtered_out)),
                ("ms", ms(started.elapsed())),
            ],
        );
    }
    failed.is_empty()
}

fn section(out: &mut dyn Write, title: &str, rows: &[&Ran<'_>]) {
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n{title}:");
    for (name, r, _) in rows {
        let _ = writeln!(out, "\n---- {name} ----");
        match r {
            Ok(o) => {
                let _ = out.write_all(&o.stdout);
                let _ = out.write_all(&o.stderr);
                if !o.status.success() {
                    let _ = writeln!(out, "({})", describe(o));
                }
            }
            Err(e) => {
                let _ = writeln!(out, "{e}");
            }
        }
    }
    let _ = writeln!(out, "\n{title}:");
    for (name, _, _) in rows {
        let _ = writeln!(out, "    {name}");
    }
}

fn describe(o: &Output) -> String {
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(&o.status) {
        return format!("killed by signal {sig}");
    }
    match o.status.code() {
        Some(101) => "panicked (exit 101)".to_owned(),
        Some(c) => format!("exit {c}"),
        None => "no exit code".to_owned(),
    }
}
