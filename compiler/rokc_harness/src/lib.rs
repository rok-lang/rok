//! Running the `#[test]` and `#[bench]` harness that `rokc` builds. The
//! compiler produces a self-dispatching executable and the list of cases
//! in it; this crate selects cases, runs each in its own process and
//! reports the result. It is the `rok test` / `rok bench` runner, kept out
//! of the compiler because it needs none of it: only a path and some
//! names. Output goes to an injected writer so a caller can capture it.

use std::io::Write;
use std::path::PathBuf;

use rokc_diagnostic::Json;

pub mod bench;
pub mod test;

/// The manifest format `rokc` writes and this runner reads. 2 adds
/// `format json`; `rokc` writes 1 when it does not use it.
pub const VERSION: u32 = 2;

/// Check a harness manifest's `version` line: none (an older rokc's) and
/// up to `VERSION` are read; a newer one is refused.
pub fn check_version(text: &str) -> Result<(), String> {
    let first = text.lines().next().unwrap_or("");
    let Some(v) = first.strip_prefix("version ") else {
        return Ok(());
    };
    match v.trim().parse::<u32>() {
        Ok(n) if n <= VERSION => Ok(()),
        _ => Err(format!(
            "the test harness is from a newer rokc (format version {}); run the rok of the \
             same toolchain, e.g. `rok +<version> test`",
            v.trim()
        )),
    }
}

/// Run the cases in a harness manifest; the exit code (0: all passed).
///
/// The manifest is what `rokc test` / `rokc bench` write with
/// `--emit-harness`: a `version N` line, then simple lines of
/// `kind test|bench`, the resolved options
/// (`threads`/`show_output` or `iters`/`samples`, and `format`), an
/// optional `filter` (raw to end of line), then an `exe <path>` per built
/// executable each followed by its `case <name>` lines. A package with
/// both roots lists the library first; a failing library run ends the
/// command (spec 9.1). With `format json`, `out` gets the JSON records
/// and the human text goes to stderr.
pub fn run_manifest(text: &str, out: &mut dyn Write) -> i32 {
    run_manifest_to(text, out, &mut std::io::stderr())
}

/// [`run_manifest`], with `format json`'s human text going to `human`.
pub fn run_manifest_to(text: &str, out: &mut dyn Write, human: &mut dyn Write) -> i32 {
    let mut kind = "test";
    let mut threads = 1usize;
    let mut show_output = false;
    let mut iters: Option<u64> = None;
    let mut samples = 10u64;
    let mut rokbench = false;
    let mut json = false;
    let mut filter: Option<String> = None;
    let mut harnesses: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "kind" => kind = if rest == "bench" { "bench" } else { "test" },
            "threads" => threads = rest.parse().unwrap_or(1),
            "show_output" => show_output = true,
            "iters" => iters = rest.parse().ok(),
            "samples" => samples = rest.parse().unwrap_or(10),
            "format" => (rokbench, json) = (rest == "rokbench", rest == "json"),
            "filter" => filter = Some(rest.to_owned()),
            "exe" => harnesses.push((PathBuf::from(rest), Vec::new())),
            "case" => {
                if let Some((_, cases)) = harnesses.last_mut() {
                    cases.push(rest.to_owned());
                }
            }
            _ => {}
        }
    }
    let (text_out, mut records): (&mut dyn Write, Option<&mut dyn Write>) = if json {
        (human, Some(out))
    } else {
        (out, None)
    };
    let mut ok = true;
    for (exe, cases) in &harnesses {
        let passed = if kind == "bench" {
            let opts = bench::Options {
                filter: filter.as_deref(),
                iters,
                samples,
                rokbench,
            };
            bench::run_to(exe, cases, &opts, text_out, records.as_deref_mut())
        } else {
            let opts = test::Options {
                filter: filter.as_deref(),
                threads,
                show_output,
            };
            test::run_to(exe, cases, &opts, text_out, records.as_deref_mut())
        };
        let _ = text_out.flush();
        if let Some(r) = records.as_deref_mut() {
            let _ = r.flush();
        }
        if !passed {
            // A failing run (the library's, when it is first) ends it.
            ok = false;
            break;
        }
    }
    i32::from(!ok)
}

/// Write one `format json` record: `{"schema":1,"type":ty,"kind":kind,..fields}`.
fn record(to: &mut dyn Write, ty: &str, kind: &str, fields: Vec<(&str, Json)>) {
    let mut all = vec![
        ("schema".to_owned(), Json::num(rokc_diagnostic::JSON_SCHEMA)),
        ("type".to_owned(), Json::str(ty)),
        ("kind".to_owned(), Json::str(kind)),
    ];
    all.extend(fields.into_iter().map(|(k, v)| (k.to_owned(), v)));
    let _ = writeln!(to, "{}", Json::Obj(all));
}

fn count(n: usize) -> Json {
    Json::num(u32::try_from(n).unwrap_or(u32::MAX))
}

/// `d` in milliseconds, to the microsecond.
fn ms(d: std::time::Duration) -> Json {
    Json::num((d.as_secs_f64() * 1e6).round() / 1e3)
}

/// Test / bench filter (spec 9.1: "regex or plain substring"): unanchored,
/// with `^ $ . * + ?` and `\`; no classes, groups or alternation.
#[must_use]
pub fn re_match(re: &str, text: &str) -> bool {
    let re: Vec<char> = re.chars().collect();
    let text: Vec<char> = text.chars().collect();
    if re.first() == Some(&'^') {
        return here(&re[1..], &text);
    }
    (0..=text.len()).any(|i| here(&re, &text[i..]))
}

fn here(re: &[char], t: &[char]) -> bool {
    let Some(&c) = re.first() else { return true };
    if c == '$' && re.len() == 1 {
        return t.is_empty();
    }
    let (atom, len) = match (c, re.get(1)) {
        ('\\', Some(&l)) => (Some(l), 2),
        ('.', _) => (None, 1),
        _ => (Some(c), 1),
    };
    let hit = |t: &[char]| t.first().is_some_and(|&x| atom.is_none_or(|a| a == x));
    let rest = &re[len..];
    match rest.first() {
        Some('*') => star(hit, &rest[1..], t),
        Some('+') => hit(t) && star(hit, &rest[1..], &t[1..]),
        Some('?') => here(&rest[1..], t) || (hit(t) && here(&rest[1..], &t[1..])),
        _ => hit(t) && here(rest, &t[1..]),
    }
}

fn star(hit: impl Fn(&[char]) -> bool, re: &[char], mut t: &[char]) -> bool {
    loop {
        if here(re, t) {
            return true;
        }
        if !hit(t) {
            return false;
        }
        t = &t[1..];
    }
}

#[cfg(test)]
mod tests {
    use super::{check_version, re_match};

    #[test]
    fn version_line_accepted() {
        assert!(check_version("version 1\nkind test\n").is_ok());
        assert!(check_version("version 2\nkind test\nformat json\n").is_ok());
        // An older rokc writes none.
        assert!(check_version("kind test\nthreads 1\n").is_ok());
    }

    #[test]
    fn newer_version_refused() {
        let e = check_version("version 3\nkind test\n").unwrap_err();
        assert!(e.contains("newer rokc (format version 3)"), "{e}");
        assert!(e.contains("run the rok of the same toolchain"), "{e}");
        assert!(check_version("version x\n").is_err());
    }

    #[test]
    fn filter_regex() {
        assert!(re_match("add", "math::adds"));
        assert!(!re_match("sub", "math::adds"));
        assert!(re_match("^math::", "math::adds"));
        assert!(!re_match("^adds", "math::adds"));
        assert!(re_match("s$", "math::adds"));
        assert!(re_match("a.*d", "math::adds"));
        assert!(re_match("ad+s", "math::adds"));
        assert!(re_match("^x?math", "math::adds"));
        assert!(re_match("a\\.b", "a.b"));
        assert!(!re_match("a\\.b", "axb"));
    }
}
