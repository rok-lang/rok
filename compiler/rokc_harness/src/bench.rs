//! The `rok bench` runner (spec 9.1).
//!
//! Each benchmark runs in its own process with inherited stdout, so the
//! library builds a `rok::core::bench::Bench` from the arguments and the
//! measured code prints itself: calibration, sampling, statistics and
//! formatting all live in the library. This runner only selects the
//! benchmarks, passes the sampling options, the output form and the name
//! width, and reports which ran. Its own lines go to `out`.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use rokc_core::term::{self, Role, Stream};

use crate::re_match;

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
    let mut failed = 0usize;
    let mut idle = 0usize;
    for name in &selected {
        // The child inherits stdout; flush ours so lines stay in order.
        let _ = out.flush();
        let status = Command::new(exe)
            .args([
                name.as_str(),
                iters.as_str(),
                samples.as_str(),
                if opts.rokbench { "0" } else { "1" },
                width_s.as_str(),
            ])
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) if s.code() == Some(MEASURED_NOTHING) => {
                idle += 1;
                let _ = writeln!(
                    out,
                    "  {name:<width$}  {} measured nothing: time the code with `b.iter(|| ..)`",
                    term::label(Stream::Stdout, Role::Warning)
                );
            }
            Ok(s) => {
                failed += 1;
                let how = s
                    .code()
                    .map_or_else(|| "signal".to_owned(), |c| format!("exit {c}"));
                let _ = writeln!(out, "  {name:<width$}  FAILED ({how})");
            }
            Err(e) => {
                failed += 1;
                let _ = writeln!(out, "  {name:<width$}  FAILED ({e})");
            }
        }
    }
    let idle_s = if idle > 0 {
        format!("{idle} measured nothing; ")
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "\nbench result: {}. {} run; {failed} failed; {idle_s}{} filtered out; finished in {:.2}s\n",
        if failed == 0 { "ok" } else { "FAILED" },
        selected.len() - failed - idle,
        benches.len() - selected.len(),
        started.elapsed().as_secs_f64()
    );
    failed == 0
}
