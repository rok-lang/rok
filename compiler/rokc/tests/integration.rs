//! End to end: `rokc build` each `tests/cases/*.rok` (or the package
//! `tests/cases/<dir>/`, `src/main.rok`), then run it. A case has
//! `// expect-exit: N` (optionally `// expect-stdout:`), or
//! `// expect-error: text` when it must fail to build. Each build and run
//! has a time limit (`case_timeout`); past it the case fails. Directives:
//! `common/mod.rs`.

mod common;

use std::fs;
use std::path::PathBuf;

use common::*;

#[test]
fn end_to_end_cases() {
    let Some(env) = setup() else { return };
    let cases_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    let scratch = env.workspace.join(format!(
        "target/integration_tmp{}{}",
        opt_suffix(),
        test_target().unwrap_or_default()
    ));

    let mut cases: Vec<(String, PathBuf)> = fs::read_dir(&cases_dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.is_dir() {
                let main = p.join("src/main.rok");
                let name = p.file_name()?.to_string_lossy().into_owned();
                main.exists().then_some((name, main))
            } else if p.extension().is_some_and(|x| x == "rok") {
                Some((p.file_stem()?.to_string_lossy().into_owned(), p))
            } else {
                None
            }
        })
        .collect();
    cases.sort();
    if let Some(f) = std::env::var_os("ROK_TEST_FILTER") {
        let f = f.to_string_lossy().into_owned();
        cases.retain(|(name, _)| name.contains(&f));
    }
    let cases = shard(cases);

    let results = par_map(&cases, |(name, path)| {
        let r = run_case(&env, name, path, &scratch);
        if r.is_ok() {
            println!("ok    {name}");
            // Only failures keep their outputs (CI disks are small).
            let _ = fs::remove_dir_all(scratch.join(name));
        }
        r.map_err(|e| format!("{name}: {e}"))
    });
    let failures: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    assert!(
        failures.is_empty(),
        "{} of {} integration cases failed:\n  {}",
        failures.len(),
        cases.len(),
        failures.join("\n  ")
    );
}

fn run_case(
    env: &Env,
    name: &str,
    path: &PathBuf,
    scratch: &std::path::Path,
) -> Result<(), String> {
    let source = fs::read_to_string(path).unwrap();
    if optimized() && directives(&source, "debug-only").next().is_some() {
        return Ok(());
    }
    if skipped_on_target(&source) {
        return Ok(());
    }
    let case_dir = scratch.join(name);
    let _ = fs::create_dir_all(&case_dir);
    let plug = directive(&source, "wasm-plug:").map(str::trim);
    let wac = std::env::var_os("ROK_TEST_WAC").filter(|w| !w.is_empty());
    if plug.is_some() && wac.is_none() {
        return Ok(());
    }
    let mut exe = case_dir.join(exe_name("out"));

    let build = rokc_build(env, path, &exe).map_err(|e| format!("rokc: {e}"))?;
    let stderr = String::from_utf8_lossy(&build.stderr);
    if parse_expect_error(&source).is_some() {
        if build.status.success() {
            return Err("build succeeded but `// expect-error:` expects failure".into());
        }
        return check_expect_error(&source, &stderr);
    }
    check_lints(env, &source, path, &case_dir, &stderr)?;
    let Some(expected) = parse_expect_exit(&source) else {
        return Err("missing `// expect-exit: N` (or `// expect-error:`) directive".into());
    };
    if !build.status.success() {
        let first = stderr
            .lines()
            .find(|l| l.starts_with("error"))
            .unwrap_or("");
        return Err(format!("rokc build failed: {first}"));
    }
    if let (Some(plug), Some(wac)) = (plug, wac) {
        // Relative to the case's package directory, above `src/`.
        let dir = path
            .parent()
            .and_then(std::path::Path::parent)
            .unwrap_or_else(|| std::path::Path::new("."));
        let plugged = case_dir.join(exe_name("plug"));
        let b = rokc_build(env, &dir.join(plug), &plugged).map_err(|e| format!("rokc: {e}"))?;
        if !b.status.success() {
            return Err(format!(
                "plug build failed: {}",
                String::from_utf8_lossy(&b.stderr)
            ));
        }
        let composed = case_dir.join(exe_name("composed"));
        let mut c = std::process::Command::new(wac);
        c.arg("plug")
            .arg(&exe)
            .arg("--plug")
            .arg(&plugged)
            .arg("-o")
            .arg(&composed);
        let c = output_within(&mut c, case_timeout(&source, Step::Build))
            .map_err(|e| format!("wac: {e}"))?;
        if !c.status.success() {
            return Err(format!(
                "wac plug failed: {}",
                String::from_utf8_lossy(&c.stderr)
            ));
        }
        exe = composed;
    }
    for want in directives(&source, "expect-warning:") {
        if !stderr.contains(want) {
            return Err(format!(
                "build stderr lacks warning {want:?}\n    stderr: {stderr:?}"
            ));
        }
    }

    for unwanted in directives(&source, "forbid-warning:") {
        if stderr.contains(unwanted) {
            return Err(format!(
                "build stderr has warning {unwanted:?}\n    stderr: {stderr:?}"
            ));
        }
    }

    // A reactor has no `run`: only its exports are called.
    let reactor = directives(&source, "wasm-reactor").next().is_some();
    if !reactor {
        check_run(&source, &exe, expected, &[])?;
    }
    for inv in directives(&source, "wasm-invoke:") {
        let (call, want) = inv
            .split_once(" => ")
            .ok_or("`// wasm-invoke: call => value`")?;
        let run = output_within(
            &mut invoke_command(&exe, call.trim()),
            case_timeout(&source, Step::Run),
        )
        .map_err(|e| format!("--invoke {call}: {e}"))?;
        let got = String::from_utf8_lossy(&run.stdout);
        if !run.status.success() || got.trim_end() != want.trim() {
            return Err(format!(
                "--invoke {call}: want {want:?}, got {got:?} ({})\n    stderr: {}",
                run.status,
                String::from_utf8_lossy(&run.stderr)
            ));
        }
    }
    // Again with a collection every N allocations, each full one
    // evacuating, then with concurrent marking too.
    if let Some(n) = directive(&source, "gc-stress:") {
        let env = [("ROKR_GC_STRESS", n.trim()), ("ROKR_GC_EVAC", "always")];
        check_run(&source, &exe, expected, &env).map_err(|e| format!("gc-stress: {e}"))?;
        let env = [
            ("ROKR_GC_STRESS", n.trim()),
            ("ROKR_GC_EVAC", "always"),
            ("ROKR_GC_CONCURRENT", "always"),
        ];
        check_run(&source, &exe, expected, &env)
            .map_err(|e| format!("gc-stress concurrent: {e}"))?;
    }
    Ok(())
}

/// `// expect-lint:`: the `E503` diagnostics of `rokc lint`, as
/// `line:col headline`, are exactly the directives, and so are those
/// the build printed (`build_err`).
fn check_lints(
    env: &Env,
    source: &str,
    path: &std::path::Path,
    dir: &std::path::Path,
    build_err: &str,
) -> Result<(), String> {
    let want: Vec<&str> = directives(source, "expect-lint:").collect();
    if want.is_empty() {
        return Ok(());
    }
    let out = rokc_lint(env, path, &dir.join("lint")).map_err(|e| format!("rokc lint: {e}"))?;
    let got = lint_lines(&String::from_utf8_lossy(&out.stderr));
    if got != want {
        return Err(format!("lints mismatch\n  want: {want:?}\n  got:  {got:?}"));
    }
    let built = lint_lines(build_err);
    if built != want {
        return Err(format!(
            "build lints mismatch\n  want: {want:?}\n  got:  {built:?}"
        ));
    }
    Ok(())
}

/// The `E503` diagnostics of `err` as `line:col headline`.
fn lint_lines(err: &str) -> Vec<String> {
    let mut got = Vec::new();
    let mut lines = err.lines();
    while let Some(l) = lines.next() {
        let Some(msg) = l
            .strip_prefix("warning[E503]: ")
            .or_else(|| l.strip_prefix("error[E503]: "))
        else {
            continue;
        };
        let at = lines
            .next()
            .and_then(|w| w.rsplit(':').nth(1).zip(w.rsplit(':').next()))
            .map(|(line, col)| format!("{line}:{col}"))
            .unwrap_or_default();
        got.push(format!("{at} {msg}"));
    }
    got
}

/// Run `exe` (with the case's `run-env` and `env`) and check its exit
/// code, stderr and stdout against the case's directives.
fn check_run(
    source: &str,
    exe: &std::path::Path,
    expected: i32,
    env: &[(&str, &str)],
) -> Result<(), String> {
    // `// run-env: KEY=VALUE`: the case's own environment.
    let own = directives(source, "run-env:").filter_map(|d| d.split_once('='));
    let mut c = run_command(exe);
    c.envs(own).envs(env.iter().copied());
    let run = output_within(&mut c, case_timeout(source, Step::Run))?;
    let exit = run.status.code().ok_or("no exit code")?;
    if !exit_matches(exit, expected) {
        return Err(format!("exit={exit} (expected {expected})"));
    }
    // Library panic locations are absolute: relative to the workspace.
    let root = rokc_core::real_path(workspace()).unwrap();
    let root = format!("{}/", root.display()).replace('\\', "/");
    let got = String::from_utf8_lossy(&run.stderr)
        .replace('\\', "/")
        .replace(&root, "");
    // wasmtime's report of a preview 1 exit code of 126 or more (run as
    // 1, `exit_matches`) is not the program's.
    let got = match got.split_once("Error: failed to run main module") {
        Some((program, _)) if exit != expected => program.to_owned(),
        _ => got,
    };
    // wasmtime's notice on `-S threads` (wasm32-wasip1-threads) and the
    // blank padding around it are not the program's.
    let got = match got.split_once('\n') {
        Some((first, rest)) if first.starts_with("WARNING: the `-Sthreads` flag") => {
            rest.trim().to_owned()
        }
        _ => got,
    };
    let got: Vec<&str> = got.lines().map(str::trim).collect();
    let want: Vec<&str> = directives(source, "expect-stderr:").collect();
    if got.len() != want.len() || !got.iter().zip(&want).all(|(g, w)| glob(w, g)) {
        return Err(format!(
            "stderr mismatch\n  want: {want:?}\n  got:  {got:?}"
        ));
    }
    let want = parse_expect_stdout(source).ok_or("missing `// expect-stdout:` directive")?;
    let got = String::from_utf8_lossy(&run.stdout);
    if got.trim_end() != want.trim_end() {
        return Err(format!(
            "stdout mismatch\n  want: {want:?}\n  got:  {got:?}"
        ));
    }
    Ok(())
}

/// `line` matches `pat`, where each `*` stands for any text (a library
/// panic's `line:column`, which moves as the library changes).
fn glob(pat: &str, line: &str) -> bool {
    let mut parts = pat.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = line.strip_prefix(first) else {
        return false;
    };
    let mut parts: Vec<&str> = parts.collect();
    let Some(last) = parts.pop() else {
        return rest.is_empty();
    };
    for p in parts {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// A case that never ends fails at its limit instead of holding the job.
#[test]
fn hung_case_times_out() {
    let Some(env) = setup() else { return };
    let dir = scratch("hung");
    let src = dir.join("hung.rok");
    put(&src, "fn main() {\n    loop {}\n}\n").unwrap();
    let exe = dir.join(exe_name("hung"));
    let build = rokc_build(&env, &src, &exe).unwrap();
    assert_exit(&build, 0);
    let limit = case_timeout_in(Some("1"), "", Step::Run);
    assert_eq!(limit, std::time::Duration::from_secs(1));
    let start = std::time::Instant::now();
    let got = output_within(&mut run_command(&exe), limit);
    assert_eq!(got.err().as_deref(), Some("timed out after 1s"));
    assert!(start.elapsed() < std::time::Duration::from_secs(30));
}

#[test]
fn case_timeout_sources() {
    let d = std::time::Duration::from_secs;
    assert_eq!(case_timeout_in(None, "", Step::Build), d(600));
    assert_eq!(case_timeout_in(None, "", Step::Run), d(300));
    assert_eq!(case_timeout_in(Some("900"), "", Step::Run), d(900));
    assert_eq!(case_timeout_in(Some(""), "", Step::Run), d(300));
    assert_eq!(
        case_timeout_in(Some("900"), "// timeout: 5\n", Step::Build),
        d(5)
    );
}
