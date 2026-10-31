//! CLI behavior (spec 9.1 / 9.2 / 9.5): spawn the built `rokc` on
//! scratch package directories. Tests that build native code skip
//! when rokc is built without LLVM (see `common::setup`).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{Env, ROKC, assert_exit, exe_name, put, scratch, setup, text, workspace};
use rokc_target::{Libc, ObjectFormat, TargetSpec};

/// `rokc <args>` in `cwd`, with the repo's library and (when given) the
/// freshly built runtime.
fn rokc(env: Option<&Env>, cwd: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(ROKC);
    cmd.current_dir(cwd)
        .env("ROK_HOME", workspace())
        .env_remove("CLICOLOR_FORCE")
        .args(args);
    if let Some(env) = env {
        cmd.arg("--rokr").arg(&env.rokr);
    }
    cmd.output().unwrap()
}

/// Build the `kind` (`test` | `bench`) harness in `cwd` with `rokc`, then
/// run it the way `rok` does (the `rokc_harness` runner). Returns the exit
/// code, the runner's stdout, and the build's stderr (its warnings, or a
/// build error when the harness does not build).
#[track_caller]
fn harness(env: &Env, cwd: &Path, kind: &str, extra: &[&str]) -> (i32, String, String) {
    let (code, out, _, err) = harness_json(env, cwd, kind, extra);
    (code, out, err)
}

/// [`harness`], and the human text a `--format json` run keeps off its
/// stdout (third).
#[track_caller]
fn harness_json(
    env: &Env,
    cwd: &Path,
    kind: &str,
    extra: &[&str],
) -> (i32, String, String, String) {
    let man = cwd.join("harness.manifest");
    let mut args = vec![kind, "--emit-harness", man.to_str().unwrap()];
    args.extend_from_slice(extra);
    let build = rokc(Some(env), cwd, &args);
    let build_err = text(&build.stderr);
    if !build.status.success() {
        return (
            build.status.code().unwrap_or(1),
            text(&build.stdout),
            String::new(),
            build_err,
        );
    }
    let manifest = fs::read_to_string(&man).unwrap();
    // The format version comes first: 2 only for `format json`.
    let json = manifest.lines().any(|l| l == "format json");
    let want = if json { "version 2\n" } else { "version 1\n" };
    assert!(manifest.starts_with(want), "{manifest}");
    let (mut out, mut human) = (Vec::new(), Vec::new());
    let code = rokc_harness::run_manifest_to(&manifest, &mut out, &mut human);
    (code, text(&out), text(&human), build_err)
}

/// `dir/name/src/main.rok` (or `src/lib.rok` with a test), as `rok new`
/// makes it.
fn new_package(dir: &Path, name: &str, lib: bool) -> PathBuf {
    let pkg = dir.join(name);
    fs::create_dir_all(&pkg).unwrap();
    if lib {
        let src = "pub fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n\n#[test]\n\
                   fn adds() {\n    if add(2, 2) != 4 {\n        panic(\"add(2, 2) != 4\");\n    }\n}\n";
        put(pkg.join("src/lib.rok"), src).unwrap();
    } else {
        put(pkg.join("src/main.rok"), "println(\"Hello, world!\");\n").unwrap();
    }
    pkg
}

// spec: §6.8.12
#[test]
fn declarative_macros_run_end_to_end() {
    let Some(env) = setup() else { return };
    let dir = scratch("macros");
    let app = new_package(&dir, "app", false);
    // An item macro (repetition + @concat name synthesis) and an expression
    // macro (with a hygienic local), both used from the same program.
    put(
        app.join("src/main.rok"),
        "#[impl_macro(sq)] pub @macro { (@e: expr) => { { let v = @e; v * v } } }\n\
         #[impl_macro(mk)] pub @macro { (@rep[sep = \",\"] { @n: ident }) => { \
         @for @n { fn @concat(f_, @n)() -> i64 { 10i64 } } } }\n\
         mk!(a, b);\n\
         fn main() {\n    let v = 1i64;\n    println(f\"{sq!(f_a() + f_b())} {v}\");\n}\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &app, &["-q", "run"]);
    assert_exit(&out, 0);
    // (10 + 10)^2 = 400; the caller's `v` is untouched by `sq!`'s own `v`.
    assert_eq!(text(&out.stdout), "400 1\n");
}

// WrappingFrom (the as-cast conversion matrix) is macro-generated in
// convert.rok (spec 6.8.12); check a couple of wrapping conversions.
#[test]
fn macro_generated_wrapping_casts_work() {
    let Some(env) = setup() else { return };
    let dir = scratch("wrap");
    let app = new_package(&dir, "app", false);
    put(
        app.join("src/main.rok"),
        "fn main() {\n\
         \x20   let a = (300i32).wrapping_into::<u8>();\n\
         \x20   let b = (200i32).wrapping_into::<i8>();\n\
         \x20   println(f\"{a} {b}\");\n\
         }\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &app, &["-q", "run"]);
    assert_exit(&out, 0);
    // 300 as u8 = 44;  200 as i8 = -56
    assert_eq!(text(&out.stdout), "44 -56\n");
}

// The integer operator impls (Add, Sub, .. Ord) are macro-generated in
// num.rok (spec 6.8.12); check arithmetic across signed and unsigned.
#[test]
fn macro_generated_integer_ops_work() {
    let Some(env) = setup() else { return };
    let dir = scratch("intops");
    let app = new_package(&dir, "app", false);
    put(
        app.join("src/main.rok"),
        "fn main() {\n\
         \x20   let a = 7i32 + 3i32; let b = 7i32 - 3i32; let c = 7i32 * 3i32;\n\
         \x20   let d = 7i32 / 3i32; let e = 7i32 % 3i32;\n\
         \x20   let f = 6i32 & 3i32; let g = 6i32 | 1i32; let h = 6i32 ^ 3i32;\n\
         \x20   let i = 1i32 << 3i32; let j = 16i32 >> 2i32; let k = -5i32;\n\
         \x20   let eq = if 3i32 == 3i32 { 1i32 } else { 0i32 };\n\
         \x20   let lt = if 2i32 < 3i32 { 1i32 } else { 0i32 };\n\
         \x20   let u = 200u8 + 1u8;\n\
         \x20   println(f\"{a} {b} {c} {d} {e} {f} {g} {h} {i} {j} {k} {eq} {lt} {u}\");\n\
         }\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &app, &["-q", "run"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "10 4 21 2 1 2 7 5 8 4 -5 1 1 201\n");
}

// The integer `*Assign` impls are macro-generated (spec 6.8.12); check a
// few compound assignments across types actually work at runtime.
#[test]
fn macro_generated_compound_assigns_work() {
    let Some(env) = setup() else { return };
    let dir = scratch("assigns");
    let app = new_package(&dir, "app", false);
    put(
        app.join("src/main.rok"),
        "fn main() {\n    let a = 10i64;\n    a += 5i64;\n    a *= 2i64;\n    a -= 1i64;\n\
         \x20   let b = 12u8;\n    b <<= 1u8;\n    b &= 254u8;\n    println(f\"{a} {b}\");\n}\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &app, &["-q", "run"]);
    assert_exit(&out, 0);
    // a: ((10+5)*2)-1 = 29;  b: (12<<1) & 254 = 24
    assert_eq!(text(&out.stdout), "29 24\n");
}

// spec: §9.1.1 §9.1.2 §9.1.4 §9.1.5 §9.2.2 §9.2.15
#[test]
fn new_bin_package_runs_from_its_directory() {
    let Some(env) = setup() else { return };
    let dir = scratch("bin");
    let app = new_package(&dir, "app", false);
    let out = rokc(Some(&env), &app, &["run"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "Hello, world!\n");
    assert!(text(&out.stderr).contains("Compiling app"));

    // A file inside the package builds the package (9.2.2), and `-q`
    // silences progress.
    put(app.join("src/util/x.rok"), "fn unused() {}\n").unwrap();
    let out = rokc(Some(&env), &dir, &["-q", "build", "app/src/util/x.rok"]);
    assert_exit(&out, 0);
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
    assert!(app.join("target/debug").join(exe_name("app")).is_file());

    assert_exit(&rokc(None, &dir, &["--cwd=app", "clean"]), 0);
    assert!(!app.join("target").exists());
}

/// 0.3.1: the package is `src/`; `.rok` files beside it are not part of
/// it (not compiled, not build inputs, not formatted), and one of them
/// runs as a script of its own.
// spec: §9.1.1 §9.2.2 §9.1.11
#[test]
fn files_outside_src_are_not_the_package() {
    let Some(env) = setup() else { return };
    let dir = scratch("stray");
    let app = new_package(&dir, "app", false);
    put(app.join("old.rok"), "fn (\n").unwrap();
    put(app.join("scratch.rok"), "println(\"scratch\");\n").unwrap();
    put(app.join("tests/t.rok"), "fn t() {\n  1;\n}\n").unwrap();
    let out = rokc(Some(&env), &app, &["run"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "Hello, world!\n");
    put(app.join("old.rok"), "fn ((\n").unwrap();
    assert!(text(&rokc(Some(&env), &app, &["build"]).stderr).contains("Fresh app"));
    let out = rokc(Some(&env), &app, &["run", "scratch.rok"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "scratch\n");
    // `fmt` of a package: `src/` and `tests/`, not `old.rok`.
    let out = rokc(None, &app, &["fmt", "--check"]);
    assert_exit(&out, 1);
    assert!(text(&out.stdout).contains("t.rok"), "{}", text(&out.stdout));
    assert!(
        !text(&out.stderr).contains("old.rok"),
        "{}",
        text(&out.stderr)
    );
}

/// A package with `src/lib.rok` and `src/main.rok` is both, the executable
/// using the library by the package's name.
// spec: §0.3.1 §9.1.2 §9.2.15 §9.2.16
#[test]
fn lib_and_main_package() {
    let Some(env) = setup() else { return };
    let dir = scratch("libmain");
    let app = new_package(&dir, "app", true);
    put(
        app.join("src/main.rok"),
        "use app::add;\n\nprintln(f\"{add(2, 3)}\");\n\n#[test]\nfn main_adds() {\n    if \
         add(1, 1) != 2 {\n        panic(\"main\");\n    }\n}\n",
    )
    .unwrap();

    let out = rokc(Some(&env), &app, &["build"]);
    assert_exit(&out, 0);
    let debug = app.join("target/debug");
    for f in [exe_name("app"), "libapp.a".into(), "app.rokhir".into()] {
        assert!(debug.join(&f).is_file(), "{f}");
    }
    // Each target keeps its own front-end and codegen-unit state.
    let incr: Vec<String> = fs::read_dir(debug.join("incremental"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(incr.len(), 4, "{incr:?}");
    for unit in ["app", "libapp"] {
        assert!(incr.iter().any(|n| n == unit), "{incr:?}");
    }
    let out = rokc(Some(&env), &app, &["build"]);
    assert_eq!(text(&out.stderr).matches("Fresh app").count(), 2);

    let out = rokc(Some(&env), &app, &["-q", "run"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "5\n");

    // The library's tests run before the executable's (manifest order).
    let (code, stdout, _) = harness(&env, &app, "test", &[]);
    assert_eq!(code, 0, "{stdout}");
    let lib_at = stdout.find("test adds ... ok").expect(&stdout);
    let main_at = stdout.find("test main_adds ... ok").expect(&stdout);
    assert!(lib_at < main_at, "{stdout}");
    // A failing library test stops before the executable's.
    let lib = fs::read_to_string(app.join("src/lib.rok")).unwrap();
    put(
        app.join("src/lib.rok"),
        lib.replace("add(2, 2) != 4", "add(2, 2) != 5"),
    )
    .unwrap();
    let (code, stdout, _) = harness(&env, &app, "test", &[]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("test adds ... FAILED"), "{stdout}");
    assert!(!stdout.contains("main_adds"), "{stdout}");
    put(app.join("src/lib.rok"), &lib).unwrap();

    // `check`: the library's lints and the executable's, once each.
    put(
        app.join("src/lib.rok"),
        format!("{lib}\nfn lib_unused() {{}}\n"),
    )
    .unwrap();
    put(
        app.join("src/main.rok"),
        "use app::add;\n\nfn main_unused() {}\n\nprintln(f\"{add(2, 3)}\");\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &app, &["check"]);
    assert_exit(&out, 0);
    let stderr = text(&out.stderr);
    assert_eq!(
        stderr.matches("`lib_unused` is never used").count(),
        1,
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("`main_unused` is never used").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("Checked app: 0 errors, 2 warnings"),
        "{stderr}"
    );

    // `doc`: the library.
    let out = rokc(Some(&env), &app, &["doc"]);
    assert_exit(&out, 0);
    let index = fs::read_to_string(app.join("target/doc/app/index.html")).unwrap();
    assert!(index.contains("add"), "{index}");
}

/// `--emit=source` (9.9.10): the package after meta expansion, as rok
/// source (derived impls, `if let` as `match`; no comments).
// spec: §9.9.1
#[test]
fn emit_ast_shows_derives_and_desugaring() {
    let dir = scratch("ast");
    put(
        dir.join("src/main.rok"),
        "// gone\n#[derive(Clone)]\nstruct P { x: i32; }\n\
         fn main() {\n    let v: i32 | none = 3;\n    if let n: i32 = v { println(f\"{n}\"); }\n}\n",
    )
    .unwrap();
    let out = rokc(None, &dir, &["build", "--emit=source", "-o", "x.rok"]);
    assert_exit(&out, 0);
    let text = fs::read_to_string(dir.join("x.rok")).unwrap();
    assert!(
        text.contains("impl rok::core::marker::Clone for P {")
            && text.contains("return P { x: self.x.clone() };")
            && text.contains("match v {\n        n: i32 => {")
            && !text.contains("gone"),
        "{text}"
    );
}

/// 9.2.8 in a library build: every function of the package is a root
/// (even a private one nothing calls), library code it does not reach
/// is dropped.
// spec: §9.3.2
#[test]
fn library_build_drops_code_its_package_does_not_reach() {
    let dir = scratch("libprune");
    let lib = dir.join("plib");
    fs::create_dir_all(&lib).unwrap();
    put(
        lib.join("src/lib.rok"),
        "pub fn f() -> f64 { 2.0.sqrt() }\nfn unused_private() -> i32 { 2 }\n",
    )
    .unwrap();
    let out = rokc(None, &dir, &["emit", "plib", "--emit=mir", "-o", "p.mir"]);
    assert_exit(&out, 0);
    let mir = fs::read_to_string(dir.join("p.mir")).unwrap();
    assert!(mir.contains("fn plib::unused_private()"), "{mir}");
    assert!(mir.contains("fn plib::f()"), "{mir}");
    assert!(!mir.contains("fn rok_f64_cbrt("), "{mir}");
    assert!(mir.matches("\nfn ").count() < 46, "{mir}");
}

/// 9.2.16: an unchanged package is not rebuilt; a changed source, a
/// different option or a replaced output rebuilds it.
// spec: §9.1.2
#[test]
fn unchanged_package_is_fresh() {
    let Some(env) = setup() else { return };
    let dir = scratch("fresh");
    let app = new_package(&dir, "app", false);
    let built = |args: &[&str]| {
        let out = rokc(Some(&env), &app, args);
        assert_exit(&out, 0);
        let stderr = text(&out.stderr);
        assert!(
            stderr.contains("Fresh app") != stderr.contains("Compiling app"),
            "{stderr}"
        );
        (stderr.contains("Compiling app"), text(&out.stdout))
    };
    assert!(built(&["build"]).0);
    assert_eq!(built(&["run"]), (false, "Hello, world!\n".to_owned()));
    put(app.join("src/main.rok"), "println(\"changed\");\n").unwrap();
    assert_eq!(built(&["run"]), (true, "changed\n".to_owned()));
    assert!(!built(&["build"]).0);
    // A debug build has debug info already.
    assert!(!built(&["build", "-g"]).0);
    let other = ["build", "-C", "overflow-checks=no"];
    assert!(built(&other).0);
    assert!(!built(&other).0);
    // A file the package does not use is still one of its sources.
    put(app.join("src/other.rok"), "fn f() {}\n").unwrap();
    assert!(built(&other).0);
    put(app.join("target/debug").join(exe_name("app")), "").unwrap();
    assert!(built(&other).0);
    // A `#[path]` module outside the package is a source too.
    put(dir.join("shared.rok"), "pub fn v() -> i32 {\n    1\n}\n").unwrap();
    put(
        app.join("src/main.rok"),
        "#[path(\"../../shared.rok\")]\nmod shared;\n\nprintln(f\"{shared::v()}\");\n",
    )
    .unwrap();
    assert_eq!(built(&["run"]), (true, "1\n".to_owned()));
    assert_eq!(built(&["run"]), (false, "1\n".to_owned()));
    put(dir.join("shared.rok"), "pub fn v() -> i32 {\n    2\n}\n").unwrap();
    assert_eq!(built(&["run"]), (true, "2\n".to_owned()));
}

/// 6.2.9: `build` / `run` / `test` lint as `check` does: warnings do not
/// fail the build, `-D` and `#[deny]` do, `-A` and `#[allow]` silence
/// them, and a fresh build or cached script prints them again.
// spec: §6.2.9 §9.1.5 §9.10.7
#[test]
fn builds_print_lints() {
    let Some(env) = setup() else { return };
    let dir = scratch("build-lints");
    let app = new_package(&dir, "app", false);
    let src = "fn main() {\n    let x = 1;\n    println(\"hi\");\n}\n";
    put(app.join("src/main.rok"), src).unwrap();
    let warn = "warning[E503]: unused variable `x`";
    let out = rokc(Some(&env), &app, &["run"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "hi\n");
    assert!(text(&out.stderr).contains(warn), "{}", text(&out.stderr));
    let out = rokc(Some(&env), &app, &["run"]);
    let err = text(&out.stderr);
    assert!(err.contains("Fresh app") && err.contains(warn), "{err}");
    let out = rokc(Some(&env), &app, &["-D", "unused", "build"]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains("error[E503]: unused variable `x`")
            && err.contains("`-D unused` on the command line"),
        "{err}"
    );
    let out = rokc(Some(&env), &app, &["-A", "unused_variables", "build"]);
    assert_exit(&out, 0);
    let err = text(&out.stderr);
    assert!(
        err.contains("Compiling app") && !err.contains("E503"),
        "{err}"
    );
    put(
        app.join("src/main.rok"),
        format!("#![allow(unused_variables)]\n{src}"),
    )
    .unwrap();
    let out = rokc(Some(&env), &app, &["build"]);
    assert!(!text(&out.stderr).contains("E503"), "{}", text(&out.stderr));
    put(
        app.join("src/main.rok"),
        format!("#![deny(unused_variables)]\n{src}"),
    )
    .unwrap();
    assert_exit(&rokc(Some(&env), &app, &["build"]), 1);
    // A test build.
    put(
        app.join("src/main.rok"),
        "fn main() {}\n\n#[test]\nfn t() {\n    let y = 2;\n}\n",
    )
    .unwrap();
    let (code, _, err) = harness(&env, &app, "test", &[]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("warning[E503]: unused variable `y`"), "{err}");
    // A lone file's cached executable.
    put(dir.join("s.rok"), src).unwrap();
    let run = || rokc(Some(&env), &dir, &["--target-dir", "t", "run", "s.rok"]);
    let first = text(&run().stderr);
    let second = text(&run().stderr);
    assert!(
        first.contains("Compiling s") && first.contains(warn),
        "{first}"
    );
    assert!(
        second.contains("Fresh s") && second.contains(warn),
        "{second}"
    );
}

/// 6.2.9: `-D warnings` denies every `unclosed_resource` finding, the
/// ones 0.1.6 added (a value closed on one path only) too.
// spec: §6.2.9
#[test]
fn deny_warnings_takes_new_unclosed_findings() {
    let dir = scratch("lint-unclosed-since");
    let app = new_package(&dir, "app", false);
    let new = "use rok::std::fs::File;\n\n\
               pub fn one_path(p: str, c: bool) -> str {\n    \
               let f = try! File::open(p);\n    \
               let s = try! f.read_to_str();\n    \
               if c { try! f.close(); }\n    \
               s\n}\n\n\
               fn main() {}\n";
    put(app.join("src/main.rok"), new).unwrap();
    let out = rokc(None, &app, &["lint"]);
    assert_exit(&out, 0);
    let err = text(&out.stderr);
    assert!(
        err.contains("warning[E503]: `f` holds a `File` that is not closed on every path"),
        "{err}"
    );
    for deny in ["warnings", "unclosed_resource"] {
        let out = rokc(None, &app, &["lint", "-D", deny]);
        assert_exit(&out, 1);
        let err = text(&out.stderr);
        assert!(
            err.contains("error[E503]: `f` holds a `File` that is not closed on every path"),
            "{err}"
        );
    }
    let old = "use rok::std::fs::File;\n\n\
               pub fn never(p: str) -> str {\n    \
               let f = try! File::open(p);\n    \
               try! f.read_to_str()\n}\n\n\
               fn main() {}\n";
    put(app.join("src/main.rok"), old).unwrap();
    let out = rokc(None, &app, &["lint", "-D", "warnings"]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains("error[E503]: `f` holds a `File` that is never closed"),
        "{err}"
    );
}

/// 6.2.9: `warnings` holds every warning-level lint, later ones too, as
/// a flag and as an attribute; `lint --list` shows each lint's release.
// spec: §6.2.9
#[test]
fn deny_warnings_takes_new_lints() {
    let dir = scratch("lint-warnings");
    let app = new_package(&dir, "app", false);
    let src = "fn main() {\n    let x = 1;\n}\n";
    put(app.join("src/main.rok"), src).unwrap();
    let out = rokc(None, &app, &["lint", "-D", "warnings"]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains("error[E503]: unused variable `x`")
            && err.contains("`-D warnings` on the command line"),
        "{err}"
    );
    let out = rokc(None, &app, &["lint", "-D", "warnings", "-A", "unused"]);
    assert_exit(&out, 0);
    put(
        app.join("src/main.rok"),
        format!("#![deny(warnings)]\n{src}"),
    )
    .unwrap();
    assert_exit(&rokc(None, &app, &["lint"]), 1);
    let out = rokc(None, &app, &["lint", "--list"]);
    let list = text(&out.stdout);
    assert!(
        list.contains("warnings (every warning-level lint)")
            && list.contains("unused_variables         warn  0.1.0"),
        "{list}"
    );
    let out = rokc(None, &app, &["lint", "-W", "unused_varibles"]);
    let err = text(&out.stderr);
    assert!(
        err.contains(
            "unknown lint `unused_varibles` on the command line; did you mean \
             `unused_variables`? (`rokc lint --list` shows every lint and group)"
        ),
        "{err}"
    );
    // A deprecation of any release is denied under `warnings`.
    put(
        app.join("src/main.rok"),
        "#[deprecated(since = \"0.1.6\")]\nfn new_old() {}\n\
         #[deprecated(since = \"0.1.5\")]\nfn old() {}\n\
         fn main() {\n    new_old();\n    old();\n}\n",
    )
    .unwrap();
    for deny in ["warnings", "all"] {
        let out = rokc(None, &app, &["lint", "-D", deny]);
        assert_exit(&out, 1);
        let err = text(&out.stderr);
        assert!(
            err.contains("error[E503]: use of deprecated function `new_old`")
                && err.contains("error[E503]: use of deprecated function `old`"),
            "{err}"
        );
    }
    let out = rokc(None, &app, &["lint", "--list"]);
    let list = text(&out.stdout);
    assert!(
        list.contains("deprecated_safe          warn  0.1.6"),
        "{list}"
    );
}

/// 6.2.9: `rok` names the manifest's `lints` as where a level came from;
/// the command line's own flags after them still say so.
// spec: §6.2.9 §9.10.3
#[test]
fn manifest_lint_note_names_rok_rokon() {
    let dir = scratch("lint-manifest");
    let app = new_package(&dir, "app", false);
    put(
        app.join("src/main.rok"),
        "fn BadName() {}\nfn main() {\n    let x = 1;\n    BadName();\n}\n",
    )
    .unwrap();
    let manifest = app.join("rok.rokon");
    put(&manifest, "Package { name: \"app\" }\n").unwrap();
    let from = format!("--lints-from-manifest={}", manifest.display());
    let args = [
        "lint",
        "-D",
        "unused_variables",
        "-W",
        "nope",
        &from,
        "-D",
        "non_snake_case",
    ];
    let out = rokc(None, &app, &args);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    for want in [
        "`\"unused_variables\": \"deny\"` in `lints` of rok.rokon",
        "unknown lint `nope` in `lints` of rok.rokon",
        "`-D non_snake_case` on the command line",
    ] {
        assert!(err.contains(want), "missing {want:?}:\n{err}");
    }
}

/// 6.2.9: a compiler warning that belongs to a lint takes the lint's
/// level: `-A`, `#[allow]` and `-D` reach it.
// spec: §6.2.9 §1.4.2
#[test]
fn compiler_warning_with_lint_can_be_allowed() {
    let dir = scratch("lint-compiler-warning");
    let app = new_package(&dir, "app", false);
    let src = "fn main() {\n    /// not an item\n    let x = 1;\n    println(f\"{x}\");\n}\n";
    put(app.join("src/main.rok"), src).unwrap();
    let warn = "warning[E503]: unused doc comment";
    let out = rokc(None, &app, &["lint"]);
    assert_exit(&out, 0);
    let err = text(&out.stderr);
    assert!(
        err.contains(warn) && err.contains("`#[warn(unused_doc_comments)]` on by default"),
        "{err}"
    );
    let out = rokc(None, &app, &["lint", "-A", "unused_doc_comments"]);
    assert!(
        !text(&out.stderr).contains("doc comment"),
        "{}",
        text(&out.stderr)
    );
    let out = rokc(None, &app, &["lint", "-D", "unused"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("error[E503]: unused doc comment"),
        "{}",
        text(&out.stderr)
    );
    put(
        app.join("src/main.rok"),
        format!("#![allow(unused_doc_comments)]\n{src}"),
    )
    .unwrap();
    let out = rokc(None, &app, &["lint"]);
    assert!(
        !text(&out.stderr).contains("doc comment"),
        "{}",
        text(&out.stderr)
    );
    // A confusable pair, from the lexer.
    put(
        app.join("src/main.rok"),
        "fn main() {\n    let σ² = 1;\n    let σ2 = 2;\n    println(f\"{σ²} {σ2}\");\n}\n",
    )
    .unwrap();
    let out = rokc(None, &app, &["lint"]);
    assert!(
        text(&out.stderr).contains("(confusable spelling)"),
        "{}",
        text(&out.stderr)
    );
    let out = rokc(None, &app, &["lint", "-A", "confusable_idents"]);
    assert!(
        !text(&out.stderr).contains("confusable"),
        "{}",
        text(&out.stderr)
    );
}

/// A lint added in 0.1.6: `-D warnings`, `-D all` and its own name deny
/// it.
#[test]
fn literal_spread_mismatch_is_in_warnings() {
    let dir = scratch("lint-spread");
    let app = new_package(&dir, "app", false);
    let src = "fn add(a: i32, b: i32) -> i32 { a + b }\n\
               fn main() {\n    println(f\"{add(..[1, 2, 3])}\");\n}\n";
    put(app.join("src/main.rok"), src).unwrap();
    let warn = "warning[E503]: spread of 3 elements into 2 parameters; this call panics";
    let out = rokc(None, &app, &["lint"]);
    assert_exit(&out, 0);
    assert!(text(&out.stderr).contains(warn), "{}", text(&out.stderr));
    for deny in ["warnings", "all", "literal_spread_mismatch"] {
        let out = rokc(None, &app, &["lint", "-D", deny]);
        assert_exit(&out, 1);
    }
    let out = rokc(None, &app, &["lint", "-A", "literal_spread_mismatch"]);
    assert!(
        !text(&out.stderr).contains("spread"),
        "{}",
        text(&out.stderr)
    );
    let out = rokc(None, &app, &["lint", "--list"]);
    assert!(
        text(&out.stdout).contains("literal_spread_mismatch  warn  0.1.6"),
        "{}",
        text(&out.stdout)
    );
}

/// The semantic warnings added in 0.1.6: `-D warnings` and `-D all` deny
/// them, `#[allow]` and `-A` silence them.
#[test]
fn semantic_warnings_are_in_warnings() {
    let dir = scratch("lint-semantic");
    let app = new_package(&dir, "app", false);
    let src = "const LIMIT: i32 = 5;\n\
               fn main() {\n    let b = 0.5 as bool;\n    match 3 { LIMIT => println(f\"{b} {quiet()}\") };\n}\n\
               #[allow(float_to_bool)]\n\
               fn quiet() -> bool { 1.5 as bool }\n";
    put(app.join("src/main.rok"), src).unwrap();
    let cast = "warning[E503]: `0.5 as bool` truncates first";
    let pat = "warning[E503]: `LIMIT` here is a new variable that matches every value";
    let out = rokc(None, &app, &["lint"]);
    assert_exit(&out, 0);
    let err = text(&out.stderr);
    assert!(err.contains(cast) && err.contains(pat), "{err}");
    assert!(!err.contains("`1.5 as bool` truncates"), "{err}");
    for deny in ["warnings", "all"] {
        assert_exit(&rokc(None, &app, &["lint", "-D", deny]), 1);
    }
    let out = rokc(None, &app, &["lint", "-A", "pattern_binds_const"]);
    let err = text(&out.stderr);
    assert!(err.contains(cast) && !err.contains(pat), "{err}");
    let out = rokc(None, &app, &["lint", "--list"]);
    let list = text(&out.stdout);
    for name in [
        "capture_field_write",
        "const_item_mutation",
        "pattern_binds_const",
        "discarded_ref_write",
        "ambiguous_int_literal",
        "trait_not_in_scope",
        "finally_jump",
        "float_to_bool",
        "implicit_core_fn",
    ] {
        assert!(list.contains(&format!("{name:<24} warn  0.1.6")), "{list}");
    }
}

/// 9.10.7: a cached script is linked again when the runtime changes.
// spec: §9.10.7 §9.2.16
#[test]
fn runtime_change_relinks_script() {
    let Some(env) = setup() else { return };
    let dir = scratch("script-runtime");
    let rokr = dir.join("rt").join(env.rokr.file_name().unwrap());
    fs::create_dir_all(rokr.parent().unwrap()).unwrap();
    fs::copy(&env.rokr, &rokr).unwrap();
    put(dir.join("s.rok"), "println(\"hi\");\n").unwrap();
    let run = || {
        let out = Command::new(ROKC)
            .current_dir(&dir)
            .env("ROK_HOME", workspace())
            .args(["--target-dir", "t", "run", "s.rok", "--rokr"])
            .arg(&rokr)
            .output()
            .unwrap();
        assert_exit(&out, 0);
        assert_eq!(text(&out.stdout), "hi\n");
        text(&out.stderr).contains("Compiling s")
    };
    assert!(run());
    assert!(!run());
    // A rebuilt runtime: same bytes, a new file.
    fs::remove_file(&rokr).unwrap();
    fs::copy(&env.rokr, &rokr).unwrap();
    assert!(run());
    assert!(!run());
}

/// 9.2.15: the `debug` profile has debug info and symbols; the `release`
/// one neither (unless `-g`), and a panic still names its location.
#[test]
fn profiles_differ() {
    let Some(env) = setup() else { return };
    if TargetSpec::host().object_format == ObjectFormat::Coff {
        return; // a PE executable has no symbol table either way
    }
    let dir = scratch("profiles");
    let app = new_package(&dir, "app", false);
    put(
        app.join("src/main.rok"),
        "use rok::std::env;\n#[inline(never)]\nfn distinctive_name(n: i64) -> i64 {\n    if n > 2 {\n        panic(\"too big\");\n    }\n    \
         n\n}\n\nfn main() {\n    println(f\"{distinctive_name(env::args().len() as i64)}\");\n}\n",
    )
    .unwrap();
    let exe = |profile: &str| app.join("target").join(profile).join(exe_name("app"));
    let named = |p: &Path| {
        let b = fs::read(p).unwrap();
        b.windows(13).any(|w| w == b"distinctive_0")
    };
    assert_exit(&rokc(Some(&env), &app, &["build"]), 0);
    assert!(named(&exe("debug")));
    let mut dsym = exe("debug").into_os_string();
    dsym.push(".dSYM");
    let debug_info = if cfg!(target_vendor = "apple") {
        Path::new(&dsym).is_dir()
    } else {
        let b = fs::read(exe("debug")).unwrap();
        b.windows(11).any(|w| w == b".debug_info")
    };
    assert!(debug_info, "no debug info in the debug profile");
    assert_exit(&rokc(Some(&env), &app, &["build", "--release"]), 0);
    assert!(!named(&exe("release")));
    let out = Command::new(exe("release"))
        .args(["a", "b"])
        .output()
        .unwrap();
    assert!(
        text(&out.stderr).contains("panicked at src/main.rok:5:9:\ntoo big"),
        "{}",
        text(&out.stderr)
    );
    assert_exit(&rokc(Some(&env), &app, &["build", "--release", "-g"]), 0);
    assert!(named(&exe("release")));
    let out = rokc(Some(&env), &app, &["build", "--debug"]);
    assert_exit(&out, 2);
    assert!(text(&out.stderr).contains("unknown option `--debug`"));
}

/// A file `include_str!` / `include_bytes!` reads is an input: editing
/// it rebuilds the package, parses the including file and lowers its
/// body again, and recompiles its unit alone.
#[test]
fn included_file_is_a_build_input() {
    let Some(env) = setup() else { return };
    let dir = scratch("include-input");
    let app = dir.join("app");
    fs::create_dir_all(app.join("res")).unwrap();
    put(
        app.join("src/main.rok"),
        "mod a;\nmod b;\n\nfn main() {\n    println(f\"{a::text()} {a::BYTES.len()} {b::two()}\");\n}\n",
    )
    .unwrap();
    put(
        app.join("src/a.rok"),
        "pub const BYTES: [u8; 2] = include_bytes!(\"res/b.bin\");\n\n\
         pub fn text() -> str {\n    include_str!(\"res/t.txt\")\n}\n",
    )
    .unwrap();
    put(app.join("src/b.rok"), "pub fn two() -> i64 {\n    2\n}\n").unwrap();
    put(app.join("src/res/t.txt"), "one").unwrap();
    put(app.join("src/res/b.bin"), [0u8, 255]).unwrap();
    // (rebuilt, files parsed afresh, units compiled, output).
    let build = || {
        let out = rokc(Some(&env), &app, &["build", "-vv"]);
        assert_exit(&out, 0);
        let stderr = text(&out.stderr);
        let parsed = stderr
            .lines()
            .find_map(|l| l.trim().strip_prefix("Reused: "))
            .map(|l| {
                let n: Vec<usize> = l
                    .split(|c: char| !c.is_ascii_digit())
                    .filter_map(|w| w.parse().ok())
                    .collect();
                n[1] - n[0]
            });
        let mut units: Vec<String> = stderr
            .lines()
            .filter_map(|l| l.trim().strip_prefix("Compiled unit "))
            .map(str::to_owned)
            .collect();
        units.sort();
        let run = rokc(Some(&env), &app, &["run", "-q"]);
        assert_exit(&run, 0);
        (parsed, units, text(&run.stdout))
    };
    assert_eq!(build().2, "one 2 2\n");
    let (parsed, units, out) = build();
    assert_eq!((parsed, units.len(), out.as_str()), (None, 0, "one 2 2\n"));
    put(app.join("src/res/t.txt"), "two").unwrap();
    assert_eq!(
        build(),
        (Some(1), vec!["a".to_owned()], "two 2 2\n".to_owned())
    );
    put(app.join("src/res/b.bin"), b"").unwrap();
    let out = rokc(Some(&env), &app, &["build"]);
    assert!(
        text(&out.stderr).contains("expects `[u8; 2]`, found `[u8; 0]`"),
        "{}",
        text(&out.stderr)
    );
}

/// An edit recompiles only the units it changed (and callers' units for a
/// signature); the result is a clean build's, byte for byte.
#[test]
fn edited_module_recompiles_its_unit_alone() {
    let Some(env) = setup() else { return };
    for opt in ["-O0", "-O2"] {
        let dir = scratch(&format!("units{opt}"));
        let app = dir.join("app");
        fs::create_dir_all(&app).unwrap();
        put(
            app.join("src/main.rok"),
            "mod a;\nmod b;\n\nfn main() -> i32 {\n    let xs: List<i64> = [b::three(), 4];\n    \
             println(f\"{a::twice(xs[0])} {b::greet()} {xs.len()}\");\n    return 0;\n}\n",
        )
        .unwrap();
        put(
            app.join("src/a.rok"),
            "pub fn twice(x: i64) -> i64 {\n    x * 2\n}\n",
        )
        .unwrap();
        // A struct's method and a closure: their names must not move
        // when `a` gains some.
        let b = "struct B { v: i64; }\n\nimpl B {\n    fn get(self) -> i64 { self.v }\n}\n\n\
                 pub fn three() -> i64 {\n    B { v: 3 }.get()\n}\n\n\
                 pub fn greet() -> str {\n    let f = |s: str| s;\n    f(\"hi\")\n}\n";
        put(app.join("src/b.rok"), b).unwrap();
        // The units compiled, and what the program prints.
        let build = || {
            let out = rokc(Some(&env), &app, &["build", "-vv", opt]);
            assert_exit(&out, 0);
            let stderr = text(&out.stderr);
            let mut units: Vec<String> = stderr
                .lines()
                .filter_map(|l| l.trim().strip_prefix("Compiled unit "))
                .map(str::to_owned)
                .collect();
            units.sort();
            let run = rokc(Some(&env), &app, &["run", "-q", opt]);
            assert_exit(&run, 0);
            (units, text(&run.stdout))
        };
        let (units, out) = build();
        assert!(
            units.iter().any(|u| u == "a") && units.len() > 3,
            "{units:?}"
        );
        assert_eq!(out, "6 hi 2\n");
        // A body, and a new struct, method and closure (lowered before
        // `b`'s).
        put(
            app.join("src/a.rok"),
            "struct A { v: i64; }\n\nimpl A {\n    fn one(self) -> i64 { self.v }\n}\n\n\
             pub fn extra() -> i64 {\n    let f = |y: i64| y * 2;\n    f(A { v: 1 }.one())\n}\n\n\
             pub fn twice(x: i64) -> i64 {\n    x * 2 + 1\n}\n",
        )
        .unwrap();
        assert_eq!(
            build(),
            (vec!["a".to_owned()], "7 hi 2\n".to_owned()),
            "{opt}"
        );
        // A signature: its callers' unit too.
        put(
            app.join("src/b.rok"),
            b.replace("three() -> i64", "three(k: i64) -> i64")
                .replace("B { v: 3 }.get()", "B { v: 3 }.get() * k"),
        )
        .unwrap();
        let main = fs::read_to_string(app.join("src/main.rok")).unwrap();
        put(
            app.join("src/main.rok"),
            main.replace("b::three()", "b::three(1)"),
        )
        .unwrap();
        let (units, out) = build();
        assert_eq!(units, ["<root>", "b"], "{opt}");
        assert_eq!(out, "7 hi 2\n");
        // A new GC type, then gone again: the type ids are a clean
        // build's.
        let a = fs::read_to_string(app.join("src/a.rok")).unwrap();
        put(
            app.join("src/a.rok"),
            format!(
                "{a}\npub fn rows() -> i64 {{\n    \
                 let t: List<(str, str, i64, str, i64)> = [(\"x\", \"y\", 1, \"z\", 2)];\n    \
                 t.len() as i64\n}}\n"
            ),
        )
        .unwrap();
        let main = fs::read_to_string(app.join("src/main.rok")).unwrap();
        put(
            app.join("src/main.rok"),
            main.replace("{xs.len()}", "{xs.len()} {a::rows()}"),
        )
        .unwrap();
        assert_eq!(build().1, "7 hi 2 1\n");
        put(app.join("src/a.rok"), a).unwrap();
        put(app.join("src/main.rok"), main).unwrap();
        assert_eq!(build().1, "7 hi 2\n");
        // The same executable as a clean build's.
        let exe = |name: &str| {
            let profile = if opt == "-O2" { "release" } else { "debug" };
            fs::read(app.join("target").join(profile).join(name)).unwrap()
        };
        let name = if cfg!(windows) { "app.exe" } else { "app" };
        let incremental = exe(name);
        fs::remove_dir_all(app.join("target")).unwrap();
        let (units, _) = build();
        assert!(units.len() > 3, "{units:?}");
        assert!(
            incremental == exe(name),
            "{opt}: not the clean build's executable"
        );
    }
}

/// Unchanged parses and lowerings replay from the incremental directory;
/// `ROK_INCREMENTAL_VERIFY` fails a replay that differs from a clean lowering.
// spec: §9.1.7
#[test]
fn front_end_reuses_parses_and_bodies() {
    let Some(env) = setup() else { return };
    let dir = scratch("incr-front");
    let app = dir.join("app");
    fs::create_dir_all(&app).unwrap();
    put(
        app.join("src/main.rok"),
        "mod m;\n\nfn pure(x: i64) -> i64 { x }\n\nfn main() {\n    \
         let a = try pure(1) catch _ { 0 };\n    println(f\"{a} {m::f(2)} {m::g()}\");\n}\n",
    )
    .unwrap();
    let m = "pub fn f(x: i64) -> i64 {\n    x * 2\n}\n\n\
             pub fn g() -> str {\n    let h = |s: str| s;\n    h(\"g\")\n}\n";
    put(app.join("src/m.rok"), m).unwrap();
    let rokc_with = |vars: &[(&str, &str)], args: &[&str]| {
        let mut cmd = Command::new(ROKC);
        cmd.current_dir(&app)
            .env("ROK_HOME", workspace())
            .env("ROK_INCREMENTAL_VERIFY", "1")
            .envs(vars.iter().copied())
            .args(args)
            .arg("--rokr")
            .arg(&env.rokr);
        let out = cmd.output().unwrap();
        assert_exit(&out, 0);
        out
    };
    // (files reused, files, bodies replayed, bodies), the warnings, the
    // program's output.
    let build = || {
        let out = rokc_with(&[], &["check", "-v"]);
        let stderr = text(&out.stderr);
        let line = stderr
            .lines()
            .find_map(|l| l.trim().strip_prefix("Reused: "))
            .unwrap_or_else(|| panic!("{stderr}"))
            .to_owned();
        let n: Vec<usize> = line
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|w| w.parse().ok())
            .collect();
        let warnings: Vec<String> = stderr
            .lines()
            .filter(|l| l.starts_with("warning"))
            .map(str::to_owned)
            .collect();
        let run = rokc_with(&[], &["run", "-q"]);
        ((n[0], n[1], n[2], n[3]), warnings, text(&run.stdout))
    };
    let clean = || {
        let out = rokc_with(&[("ROK_INCREMENTAL", "0")], &["check"]);
        let stderr = text(&out.stderr);
        let warnings: Vec<String> = stderr
            .lines()
            .filter(|l| l.starts_with("warning"))
            .map(str::to_owned)
            .collect();
        warnings
    };
    let ((files0, files, bodies0, bodies), warnings, out) = build();
    assert_eq!((files0, bodies0), (0, 0));
    assert!(files > 2 && bodies > 3, "{files} {bodies}");
    assert_eq!(out, "1 4 g\n");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("this `try` covers no throwing call")),
        "{warnings:?}"
    );
    assert_eq!(warnings, clean());
    // A body, with a closure more: that file is parsed and that body
    // lowered again, all else is taken.
    put(
        app.join("src/m.rok"),
        m.replace("x * 2", "let k = |y: i64| y + 1;\n    k(x * 2)"),
    )
    .unwrap();
    let (n, w, out) = build();
    assert_eq!((n.0, n.1, n.3), (files - 1, files, bodies), "{n:?}");
    assert!(n.2 + 2 >= bodies && n.2 < bodies, "{n:?}");
    assert_eq!((w, out), (clean(), "1 5 g\n".to_owned()));
    // Nothing changed: everything is taken, though a build of other
    // library roots ran between (its pre-pass summaries are its own).
    put(dir.join("bare.rok"), "fn main() {}\n").unwrap();
    let core = workspace().join("library/core/src/lib.rok");
    let out = Command::new(ROKC)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .args(["check", "bare.rok", "--bootstrap"])
        .arg(&core)
        .output()
        .unwrap();
    assert_exit(&out, 0);
    let (n, w, _) = build();
    assert_eq!(n, (files, files, bodies, bodies));
    assert_eq!(w, warnings);
    // A signature: that function, its caller (`main`, which read it)
    // and `g` (moved) are lowered again.
    let sig = m
        .replace("-> i64 {", "-> i32 {")
        .replace("x * 2", "(x * 2) as i32");
    put(app.join("src/m.rok"), &sig).unwrap();
    let (n, w, out) = build();
    assert_eq!((n.0, n.1, n.3, bodies - n.2), (files - 1, files, bodies, 3));
    assert_eq!((w, out), (clean(), "1 4 g\n".to_owned()));
    // Another item no body reads: every body is taken.
    let item = sig + "\npub struct S {\n    v: i64;\n}\n";
    put(app.join("src/m.rok"), &item).unwrap();
    let (n, w, out) = build();
    assert_eq!((n.0, n.2, n.3), (files - 1, bodies, bodies), "{n:?}");
    assert_eq!((w, out), (clean(), "1 4 g\n".to_owned()));
    // A new method: it is lowered, every other body is taken.
    put(
        app.join("src/m.rok"),
        item + "\nimpl S {\n    pub fn get(self) -> i64 {\n        self.v\n    }\n}\n",
    )
    .unwrap();
    let (n, w, out) = build();
    assert_eq!((n.0, n.3 - n.2), (files - 1, 1), "{n:?}");
    assert_eq!((w, out), (clean(), "1 4 g\n".to_owned()));
}

/// `rokc check` reads the library's items from the cached stage too.
// spec: §9.1.2 §9.2.16
#[test]
fn check_takes_the_library_stage() {
    let Some(_) = setup() else { return };
    let dir = scratch("check-stage");
    put(
        dir.join("dep/src/lib.rok"),
        "#[must_use]\npub fn token() -> i64 {\n    7\n}\n\npub trait Mark {}\n\n\
         pub fn plain() -> i64 {\n    1\n}\n",
    )
    .unwrap();
    put(
        dir.join("app/src/main.rok"),
        "use dep::Mark;\nuse dep::plain;\nuse rok::core::ops::Add;\n\n\
         fn main() {\n    dep::token();\n}\n",
    )
    .unwrap();
    let check = || {
        let out = Command::new(ROKC)
            .current_dir(&dir)
            .env("ROK_HOME", workspace())
            .env("ROK_TIME_PASSES", "1")
            .args(["check", "app", "--extern", "dep=dep"])
            .output()
            .unwrap();
        assert_exit(&out, 0);
        let err = text(&out.stderr);
        let lints: Vec<&str> = err.lines().filter(|l| l.starts_with("warning")).collect();
        assert_eq!(
            lints,
            [
                "warning[E503]: unused import `dep::plain`",
                "warning[E503]: unused result of `token`, which is `#[must_use]`",
            ],
            "{err}"
        );
        err.contains("hir library write")
    };
    // Collected and kept, then taken.
    assert!(check());
    assert!(!check());
}

/// The collected library stage is cached across processes and taken or
/// redone per edit; a corrupt store is a miss.
// spec: §9.1.7
#[test]
fn library_stage_persists_across_builds() {
    let Some(env) = setup() else { return };
    let dir = scratch("incr-library");
    let dep = "pub const LIMIT: i64 = 3;\n\n\
               pub trait Shape {\n    fn area(self) -> i64;\n}\n\n\
               pub fn scale(x: i64) -> i64 {\n    x * LIMIT\n}\n";
    put(dir.join("dep/src/lib.rok"), dep).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "mod m;\n\nfn main() {\n    let s = m::Sq { side: 2 };\n    \
         let w = try dep::scale(0) catch _ { 1 };\n    \
         println(f\"{m::f(1)} {dep::scale(m::K)} {m::total(s)} {w}\");\n}\n",
    )
    .unwrap();
    let m = "pub const K: i64 = 5;\n\n\
             pub struct Sq {\n    pub side: i64;\n}\n\n\
             impl dep::Shape for Sq {\n    fn area(self) -> i64 {\n        self.side * self.side\n    }\n}\n\n\
             pub fn total<T: dep::Shape>(x: T) -> i64 {\n    x.area()\n}\n\n\
             pub fn f(x: i64) -> i64 {\n    x + 1\n}\n";
    put(dir.join("app/src/m.rok"), m).unwrap();
    let run = |vars: &[(&str, &str)], args: &[&str]| {
        let out = Command::new(ROKC)
            .current_dir(&dir)
            .env("ROK_HOME", workspace())
            .env("ROK_INCREMENTAL_VERIFY", "1")
            .envs(vars.iter().copied())
            .args(["run", "-v", "app", "--extern", "dep=dep"])
            .args(args)
            .arg("--rokr")
            .arg(&env.rokr)
            .output()
            .unwrap();
        assert_exit(&out, 0);
        let err = text(&out.stderr);
        let warnings: Vec<String> = err
            .lines()
            .filter(|l| l.starts_with("warning"))
            .map(str::to_owned)
            .collect();
        let kept = err.contains("the library's items collected");
        (kept, warnings, text(&out.stdout))
    };
    // Whether the library stage was taken, and the program's output; the
    // warnings and output are a clean build's.
    let build = || {
        let (kept, w, out) = run(&[], &[]);
        let clean = dir.join("clean");
        let target = ["--target-dir", clean.to_str().unwrap()];
        let (_, clean_w, clean_out) = run(&[("ROK_INCREMENTAL", "0")], &target);
        assert_eq!((&w, &out), (&clean_w, &clean_out));
        assert!(
            w.iter().any(|l| l.contains("covers no throwing call")),
            "{w:?}"
        );
        (kept, out)
    };
    assert_eq!(build(), (false, "2 15 4 0\n".to_owned()));
    let edit = |from: &str, to: &str| {
        let path = dir.join("app/src/m.rok");
        let src = fs::read_to_string(&path).unwrap();
        assert!(src.contains(from), "{from}");
        put(path, src.replace(from, to)).unwrap();
    };
    // A body.
    edit("x + 1", "x + 2");
    assert_eq!(build(), (true, "3 15 4 0\n".to_owned()));
    // A signature.
    edit(
        "pub fn f(x: i64) -> i64 {\n    x + 2",
        "pub fn f(x: i32) -> i64 {\n    (x as i64) + 2",
    );
    assert_eq!(build(), (true, "3 15 4 0\n".to_owned()));
    // A const the package uses.
    edit("K: i64 = 5", "K: i64 = 7");
    assert_eq!(build(), (true, "3 21 4 0\n".to_owned()));
    // A trait impl.
    edit("self.side * self.side", "self.side * self.side * self.side");
    assert_eq!(build(), (true, "3 21 8 0\n".to_owned()));
    // A dependency's item the package uses: collected again, then kept.
    put(
        dir.join("dep/src/lib.rok"),
        dep.replace("LIMIT: i64 = 3", "LIMIT: i64 = 4"),
    )
    .unwrap();
    assert_eq!(build(), (false, "3 28 8 0\n".to_owned()));
    edit(") + 2", ") + 3");
    assert_eq!(build(), (true, "4 28 8 0\n".to_owned()));
    // A corrupt store: a miss, written anew. The stages are the
    // toolchain's (beside a repository build of rokc); this one's name
    // the dependency's files.
    let stages_dir = Path::new(ROKC).parent().unwrap().join("rok-cache/library");
    let dep_dir = rokc_core::real_path(dir.join("dep"))
        .unwrap()
        .display()
        .to_string();
    let stages: Vec<PathBuf> = fs::read_dir(&stages_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("library-"))
                && fs::read(p)
                    .is_ok_and(|b| b.windows(dep_dir.len()).any(|w| w == dep_dir.as_bytes()))
        })
        .collect();
    assert!(
        !stages.is_empty(),
        "no library stage of {} under {}",
        dir.display(),
        stages_dir.display()
    );
    for p in &stages {
        let mut bytes = fs::read(p).unwrap();
        let n = bytes.len();
        bytes.truncate(n / 2);
        fs::write(p, bytes).unwrap();
    }
    edit(") + 3", ") + 4");
    assert_eq!(build(), (false, "5 28 8 0\n".to_owned()));
    edit(") + 4", ") + 5");
    assert_eq!(build(), (true, "6 28 8 0\n".to_owned()));
}

/// 2.10.9 / 9.2.16: a generic body is checked against its bounds in
/// every build, incremental or not: breaking it fails the build while
/// the plain bodies replay, and fixing it builds again.
#[test]
fn incremental_checks_generic_bodies() {
    let Some(env) = setup() else { return };
    let dir = scratch("incr-generic");
    let app = dir.join("app");
    let src = app.join("src");
    fs::create_dir_all(&src).unwrap();
    let main = "mod m;\n\nfn main() {\n    println(f\"{m::twice(3)} {m::one()}\");\n}\n";
    fs::write(src.join("main.rok"), main).unwrap();
    let ok = "use rok::core::ops::Add;\n\n\
              pub fn twice<T: Add<T, Output = T> + Copy>(x: T) -> T {\n    x + x\n}\n\n\
              pub fn one() -> i64 {\n    1\n}\n";
    fs::write(src.join("m.rok"), ok).unwrap();
    let check = || {
        let out = Command::new(ROKC)
            .current_dir(&app)
            .env("ROK_HOME", workspace())
            .env("ROK_INCREMENTAL_VERIFY", "1")
            .args(["check"])
            .arg("--rokr")
            .arg(&env.rokr)
            .output()
            .unwrap();
        (out.status.success(), text(&out.stderr))
    };
    assert!(check().0, "{}", check().1);
    // The bound gone: the body no longer checks.
    fs::write(
        src.join("m.rok"),
        ok.replace(": Add<T, Output = T> + Copy", ""),
    )
    .unwrap();
    let want = "operator `+` on `T` needs a `Add` bound on `T`";
    let (pass, err) = check();
    assert!(!pass && err.contains(want), "{err}");
    // Another body edited: still reported.
    fs::write(src.join("main.rok"), main.replace("one()}", "one() + 1}")).unwrap();
    let (pass, err) = check();
    assert!(!pass && err.contains(want), "{err}");
    fs::write(src.join("m.rok"), ok).unwrap();
    let (pass, err) = check();
    assert!(pass, "{err}");
    let out = rokc(Some(&env), &app, &["run", "-q"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "6 2\n");
}

/// A mark added to or removed from an item in another file reaches the
/// uses in bodies kept from the last build.
// spec: §6.2.7 §9.2.16
#[test]
fn incremental_deprecation_marks() {
    let Some(_env) = setup() else { return };
    let dir = scratch("incr-deprecated");
    let app = dir.join("app");
    let src = app.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("main.rok"),
        "mod m;\n\nfn main() {\n    let s = m::S { x: 1 };\n    \
         println(f\"{m::old()} {s.x} {m::C}\");\n}\n",
    )
    .unwrap();
    let plain = "pub struct S { pub x: i32; }\npub const C: i32 = 2;\n\
                 pub fn old() -> i32 {\n    1\n}\n";
    let marked = "pub struct S { #[deprecated] pub x: i32; }\n\
                  #[deprecated(\"use D\")]\npub const C: i32 = 2;\n\
                  #[deprecated(since = \"0.1.6\", note = \"use new\")]\n\
                  pub fn old() -> i32 {\n    1\n}\n";
    fs::write(src.join("m.rok"), plain).unwrap();
    let check = || {
        let out = Command::new(ROKC)
            .current_dir(&app)
            .env("ROK_HOME", workspace())
            .env("ROK_INCREMENTAL_VERIFY", "1")
            .args(["check"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        text(&out.stderr)
    };
    let warned = [
        "use of deprecated function `old`",
        "use of deprecated field `S.x`",
        "use of deprecated constant `C`",
    ];
    let err = check();
    assert!(!err.contains("deprecated"), "{err}");
    fs::write(src.join("m.rok"), marked).unwrap();
    for _ in 0..2 {
        let err = check();
        for w in warned {
            assert!(err.contains(w), "missing {w:?}:\n{err}");
        }
        assert!(err.contains("deprecated since 0.1.6"), "{err}");
    }
    fs::write(src.join("m.rok"), plain).unwrap();
    let err = check();
    assert!(!err.contains("deprecated"), "{err}");
}

/// A library builds by units, links with another holding the same
/// instances, and names no symbol of a dependency it does not use.
// spec: §9.2.13 §9.3.1 §9.3.3
#[test]
fn library_builds_by_units() {
    let Some(env) = setup() else { return };
    let targets = Path::new(ROKC).parent().unwrap().join("../lib/rok/targets");
    let cross = ["x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"]
        .into_iter()
        .filter(|t| targets.join(t).is_dir());
    for target in std::iter::once(None).chain(cross.map(Some)) {
        for opt in ["-O0", "-O2"] {
            library_units(&env, target, opt);
        }
    }
}

fn library_units(env: &Env, target: Option<&str>, opt: &str) {
    let dir = scratch(&format!("units_lib{}{opt}", target.unwrap_or("")));
    let (a, b) = (dir.join("liba"), dir.join("libb"));
    let app = dir.join("app");
    for d in [&a, &b, &app] {
        fs::create_dir_all(d).unwrap();
    }
    put(
        a.join("src/lib.rok"),
        "mod util;\n\nstatic NAMES: [str; 2] = [\"x\", \"yy\"];\n\n\
         pub fn total(n: i64) -> i64 {\n    let t: i64 = 0;\n    for i in 0..n {\n        \
         t += util::weight(f\"p{i}\");\n    }\n    t + NAMES.len() as i64\n}\n",
    )
    .unwrap();
    let util = "pub fn weight(s: str) -> i64 {\n    let d: Dict<str, i64> = Dict::new();\n    \
                d[s] = s.len() as i64;\n    d.get(s) ?? 0\n}\n";
    put(a.join("src/util.rok"), util).unwrap();
    put(
        b.join("src/lib.rok"),
        "pub fn words(n: i64) -> i64 {\n    let d: Dict<str, i64> = Dict::new();\n    \
         for i in 0..n {\n        d[f\"w{i}\"] = i;\n    }\n    d.get(\"w2\") ?? 0\n}\n",
    )
    .unwrap();
    put(
        app.join("src/main.rok"),
        "use liba::total;\nuse libb::words;\n\nprintln(f\"{total(3)} {words(4)}\");\n",
    )
    .unwrap();
    let profile = if opt == "-O2" { "release" } else { "debug" };
    let out_dir = target.map_or(profile.to_owned(), |t| format!("{t}/{profile}"));
    let archive = |d: &Path, n: &str| d.join(format!("target/{out_dir}/lib{n}.a"));
    let (lib_a, lib_b) = (archive(&a, "liba"), archive(&b, "libb"));
    let extern_a = format!("liba={}", lib_a.display());
    let target_arg = target.map(|t| format!("--target={t}"));
    // The host's runtime, or the target's beside rokc.
    let rokc_t = |args: &[&str]| {
        let args: Vec<&str> = args.iter().copied().chain(target_arg.as_deref()).collect();
        rokc(target.is_none().then_some(env), &dir, &args)
    };
    let what = format!("{target:?} {opt}");
    let build = |args: &[&str]| {
        let out = rokc_t(&[&["build", "-vv", opt], args].concat());
        assert_exit(&out, 0);
        let mut units: Vec<String> = text(&out.stderr)
            .lines()
            .filter_map(|l| l.trim().strip_prefix("Compiled unit "))
            .map(str::to_owned)
            .collect();
        units.sort();
        units
    };
    // The program's output (a cross target's links only).
    let run = |want: &str| {
        let extern_b = format!("libb={}", lib_b.display());
        let exe = dir.join("app.bin");
        let exe_s = exe.display().to_string();
        let out = rokc_t(&[
            "build", "-q", opt, "app", "--extern", &extern_a, "--extern", &extern_b, "-o", &exe_s,
        ]);
        assert_exit(&out, 0);
        if target.is_none() {
            let got = text(&Command::new(&exe).output().unwrap().stdout);
            assert_eq!(got, want, "{what}");
        }
    };
    assert!(build(&["liba"]).len() > 2);
    // Built after `liba`: its GC types follow `liba`'s.
    build(&["libb", "--extern", &extern_a]);
    run("8 2\n");
    put(
        a.join("src/util.rok"),
        util.replace("as i64;", "as i64 + 1;"),
    )
    .unwrap();
    assert_eq!(build(&["liba"]), ["liba::util"], "{what}");
    run("11 2\n");
    let old_b = fs::read(&lib_b).unwrap();
    let units: Vec<String> = Vec::new();
    assert_eq!(build(&["libb", "--extern", &extern_a]), units, "{what}");
    assert!(old_b == fs::read(&lib_b).unwrap(), "{what}");
    let incremental = fs::read(&lib_a).unwrap();
    fs::remove_dir_all(a.join("target")).unwrap();
    build(&["liba"]);
    assert!(
        incremental == fs::read(&lib_a).unwrap(),
        "{what}: not the clean build's archive"
    );
}

/// An anonymous file builds by units too, kept per file in the target
/// directory without a package (`ROK_TARGET_DIR`, else the system's
/// temporary one), not in a `target/` beside the script.
// spec: §9.1.2 §9.1.7
#[test]
fn anonymous_file_reuses_its_units() {
    let Some(env) = setup() else { return };
    let dir = scratch("units_anon");
    put(dir.join("hello.rok"), "println(f\"{[1, 2].len()}\");\n").unwrap();
    let cache = dir.join("cache");
    let run = || {
        let out = Command::new(ROKC)
            .current_dir(&dir)
            .env("ROK_HOME", workspace())
            .env("ROK_TARGET_DIR", &cache)
            .args(["run", "-v", "hello.rok", "--rokr"])
            .arg(&env.rokr)
            .output()
            .unwrap();
        assert_exit(&out, 0);
        assert_eq!(text(&out.stdout), "2\n");
        text(&out.stderr)
    };
    assert!(!run().contains(" 0 compiled"));
    // The cached executable runs again without a build (spec 9.10.7).
    assert!(run().contains("Fresh hello"));
    // Built again, it reuses every unit.
    fs::remove_dir_all(cache.join("debug/scripts")).unwrap();
    assert!(run().contains(" 0 compiled"));
    let units = fs::read_dir(cache.join("debug/incremental")).unwrap();
    let names: Vec<String> = units
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.len() == 1 && names[0].starts_with("hello-"),
        "{names:?}"
    );
    assert!(!dir.join("target").exists());
}

/// Under `-g` a unit's debug info names only its own files: a new file
/// recompiles what imports it (and the C `main`, whose line moved), not
/// every unit.
#[test]
fn new_file_under_debug_info_recompiles_its_importers() {
    let Some(env) = setup() else { return };
    let app = scratch("units_g").join("app");
    fs::create_dir_all(&app).unwrap();
    let main = "fn main() -> i32 {\n    println(f\"{a::one()} {b::two()}\");\n    return 0;\n}\n";
    put(
        app.join("src/main.rok"),
        format!("mod a;\nmod b;\n\n{main}"),
    )
    .unwrap();
    put(app.join("src/a.rok"), "pub fn one() -> i64 {\n    1\n}\n").unwrap();
    put(app.join("src/b.rok"), "pub fn two() -> i64 {\n    2\n}\n").unwrap();
    let build = || {
        let out = rokc(Some(&env), &app, &["build", "-vv", "-g"]);
        assert_exit(&out, 0);
        let mut units: Vec<String> = text(&out.stderr)
            .lines()
            .filter_map(|l| l.trim().strip_prefix("Compiled unit "))
            .map(str::to_owned)
            .collect();
        units.sort();
        units
    };
    assert!(build().len() > 3);
    // `a` < `aa` < `b`: the new file comes between the others.
    put(
        app.join("src/aa.rok"),
        "pub fn three() -> i64 {\n    3\n}\n",
    )
    .unwrap();
    let main = main.replace("b::two()}", "b::two()} {aa::three()}");
    put(
        app.join("src/main.rok"),
        format!("mod a;\nmod aa;\nmod b;\n\n{main}"),
    )
    .unwrap();
    assert_eq!(build(), ["<glue>", "<root>", "aa"]);
    let run = rokc(Some(&env), &app, &["run", "-q", "-g"]);
    assert_eq!(text(&run.stdout), "1 2 3\n");
}

/// `-vv` (9.9.5): the files read and the generic instances made.
// spec: §9.1.5 §9.9.5
#[test]
fn very_verbose_lists_files_and_instances() {
    let dir = scratch("very_verbose");
    let app = dir.join("app");
    fs::create_dir_all(&app).unwrap();
    put(
        app.join("src/main.rok"),
        "fn id<T>(x: T) -> T {\n    x\n}\n\nprintln(f\"{id(1)}\");\n",
    )
    .unwrap();
    let out = rokc(None, &app, &["emit", "-vv", "--emit=hir"]);
    assert_exit(&out, 0);
    let stderr = text(&out.stderr);
    assert!(stderr.contains("     Reading src/main.rok\n"), "{stderr}");
    assert!(
        stderr.contains(" Specialized id(x: i32) -> i32\n"),
        "{stderr}"
    );
    let out = rokc(None, &app, &["emit", "-v", "--emit=hir"]);
    assert!(!text(&out.stderr).contains("Specialized"));
}

/// A nested fn's item (and symbol) name: the enclosing fn's (`Type.m`
/// for a method), `/`, its own; `/2` .. for a repeated name.
// spec: §9.5.2
#[test]
fn nested_fn_item_names() {
    let dir = scratch("nested_fn_names");
    put(
        dir.join("src/main.rok"),
        "struct S {}\nimpl S {\n    fn m(self) -> i32 {\n        fn h() -> i32 { 1 }\n        h()\n    }\n}\n\
         fn f() -> i32 {\n    let a = { fn g() -> i32 { 2 } g() };\n    let b = { fn g() -> i32 { 3 } g() };\n    a + b\n}\n\
         fn main() {\n    println(f\"{S {}.m() + f()}\");\n}\n",
    )
    .unwrap();
    let out = rokc(None, &dir, &["emit", "--emit=hir", "-o", "h.txt"]);
    assert_exit(&out, 0);
    let hir = fs::read_to_string(dir.join("h.txt")).unwrap();
    for f in ["fn S.m/h()", "fn f/g()", "fn f/g/2()"] {
        assert!(hir.contains(f), "{f}");
    }
}

/// A fresh future kept in a local nothing else names is spawned with no
/// hand-off; one with a second handle still goes through the copy
/// (stdlib 2.18.1).
// spec: §2.18.1
#[test]
fn spawn_of_a_sole_local_future_skips_the_hand_off() {
    let hir = |name: &str, body: &str| {
        let dir = scratch(name);
        put(
            dir.join("src/main.rok"),
            format!(
                "async fn f(a: i32, b: i32) -> i32 {{ a + b }}\nasync fn main() {{\n{body}}}\n"
            ),
        )
        .unwrap();
        let out = rokc(None, &dir, &["emit", "--emit=hir", "-o", "h.txt"]);
        assert_exit(&out, 0);
        fs::read_to_string(dir.join("h.txt")).unwrap()
    };
    let copy = "fn rok::core::task::spawn__mono__";
    let plain = "fn rok::core::task::spawn_plain__mono__";
    let sole = hir(
        "spawn_sole",
        "    let ts: List<Task<i32>> = [];\n    for i in 0..3 { let g = f(i, 1); ts.push(task::spawn(g)); }\n    \
         for t in ts { println(f\"{await t}\"); }\n",
    );
    assert!(sole.contains(plain) && !sole.contains(copy));
    let aliased = hir(
        "spawn_aliased",
        "    let h = f(9, 1);\n    let k = h;\n    println(f\"{await task::spawn(k)}\");\n",
    );
    assert!(aliased.contains(copy) && !aliased.contains(plain));
}

/// Stdlib 2.5, 2.9.3: where a `Dict` puts a key depends on a per-process
/// seed, but what it prints and writes (insertion order) does not; a
/// dict built at compile time keeps its own seed.
// spec: std §2.5.6 §2.9.3
#[test]
fn dict_seed_changes_placement_not_output() {
    let Some(env) = setup() else { return };
    let dir = scratch("dict_seed");
    put(
        dir.join("src/main.rok"),
        "extern \"rok\" { fn rok_hash_seed() -> u64; }\n\
         const C: Dict<str, i32> = {\"q\": 1, \"r\": 2};\n\
         fn main() {\n\
         \x20   let d: Dict<str, i32> = Dict::new();\n\
         \x20   for i in 0..40 { d[f\"k{i}\"] = i; }\n\
         \x20   d.remove(\"k3\");\n\
         \x20   d[\"k3\"] = 3;\n\
         \x20   println(f\"{d:?} {C[\"r\"]} {C.contains_key(\"q\")}\");\n\
         \x20   println(rok::std::rokon::to_string(d));\n\
         \x20   println(f\"{unsafe { rok_hash_seed() }}\");\n\
         }\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["build"]);
    assert_exit(&out, 0);
    let exe = dir.join("target/debug").join(exe_name("dict_seed"));
    let run = || {
        let o = Command::new(&exe).output().unwrap();
        assert_exit(&o, 0);
        let s = text(&o.stdout);
        let (body, seed) = s.trim_end().rsplit_once('\n').unwrap();
        (body.to_owned(), seed.to_owned())
    };
    let (a, seed_a) = run();
    let (b, seed_b) = run();
    assert_eq!(a, b);
    assert!(
        a.starts_with("{\"k0\": 0, \"k1\": 1, \"k2\": 2, \"k4\": 4,"),
        "{a}"
    );
    assert!(a.contains("\"k39\": 39, \"k3\": 3} 2 true"), "{a}");
    assert_ne!(seed_a, seed_b);
}

// spec: §9.1.2 §9.2.13 §9.2.15 §6.2.5 §9.9.12
#[test]
fn lib_package_builds_archive_and_runs_tests() {
    let Some(env) = setup() else { return };
    let dir = scratch("lib");
    let lib = new_package(&dir, "mylib", true);

    assert_exit(&rokc(Some(&env), &dir, &["build", "mylib"]), 0);
    assert!(lib.join("target/debug/libmylib.a").is_file());
    // A library has no entry point.
    assert_exit(&rokc(Some(&env), &dir, &["run", "mylib"]), 1);

    let (code, stdout, _) = harness(&env, &lib, "test", &[]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("test adds ... ok"), "{stdout}");
    assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");

    let mut src = fs::read_to_string(lib.join("src/lib.rok")).unwrap();
    src.push_str(
        "\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn boom() {\n        \
         println(\"from boom\");\n        panic(\"boom\");\n    }\n}\n",
    );
    put(lib.join("src/lib.rok"), src).unwrap();
    let (code, stdout, _) = harness(&env, &lib, "test", &[]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("test tests::boom ... FAILED"), "{stdout}");
    assert!(stdout.contains("from boom"), "{stdout}");
    assert!(stdout.contains("1 passed; 1 failed"), "{stdout}");

    // Filter (a lone non-path argument), regex anchors included.
    let (code, stdout, _) = harness(&env, &lib, "test", &["^add"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("1 passed; 0 failed; 1 filtered out"),
        "{stdout}"
    );

    // `#[test]` functions are not part of a normal build (6.2.5).
    assert_exit(&rokc(Some(&env), &lib, &["build"]), 0);
}

// spec: §6.2.5
#[test]
fn tests_in_private_modules_run() {
    let Some(env) = setup() else { return };
    let dir = scratch("privtests");
    let lib = new_package(&dir, "privlib", true);
    let mut src = fs::read_to_string(lib.join("src/lib.rok")).unwrap();
    src.push_str(
        "mod util;\nmod inner {\n    mod deeper {\n        #[test]\n        fn f() {}\n    }\n\
         \x20   #[test]\n    fn g() {\n        panic(\"g\");\n    }\n}\n",
    );
    put(lib.join("src/lib.rok"), src).unwrap();
    put(
        lib.join("src/util.rok"),
        "mod checks {\n    #[test]\n    fn h() {}\n}\n",
    )
    .unwrap();
    let (code, stdout, _) = harness(&env, &lib, "test", &[]);
    assert_eq!(code, 1, "{stdout}");
    for line in [
        "test inner::deeper::f ... ok",
        "test inner::g ... FAILED",
        "test util::checks::h ... ok",
        "3 passed; 1 failed",
    ] {
        assert!(stdout.contains(line), "{line}\n{stdout}");
    }

    // User code in a test build keeps the visibility rules.
    put(
        lib.join("src/util.rok"),
        "mod checks {\n    #[test]\n    fn h() {}\n}\npub fn leak() { checks::h(); }\n",
    )
    .unwrap();
    let (code, _, err) = harness(&env, &lib, "test", &[]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("`util::checks::h` is private"), "{err}");
}

/// Each line of `out` as JSON.
#[track_caller]
fn records(out: &str) -> Vec<rokc_diagnostic::Json> {
    out.lines()
        .map(|l| rokc_diagnostic::Json::parse(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

/// `test --format json`: a versioned record per test and a summary on
/// stdout; the human text and the tests' output elsewhere.
// spec: §9.9.12
#[test]
fn test_format_json() {
    let Some(env) = setup() else { return };
    let dir = scratch("test-json");
    let lib = new_package(&dir, "jlib", true);
    let mut src = fs::read_to_string(lib.join("src/lib.rok")).unwrap();
    src.push_str("\n#[test]\nfn boom() {\n    println(\"noise\");\n    panic(\"boom\");\n}\n");
    put(lib.join("src/lib.rok"), src).unwrap();
    let (code, out, human, _) = harness_json(&env, &lib, "test", &["--format", "json"]);
    assert_eq!(code, 1, "{out}\n{human}");
    let recs = records(&out);
    assert_eq!(recs.len(), 3, "{out}");
    for r in &recs {
        assert_eq!(r.get("schema").as_u32(), Some(1), "{out}");
        assert_eq!(r.get("kind").as_str(), Some("test"), "{out}");
    }
    assert_eq!(recs[0].get("type").as_str(), Some("case"), "{out}");
    assert_eq!(recs[0].get("name").as_str(), Some("adds"), "{out}");
    assert_eq!(recs[0].get("result").as_str(), Some("ok"), "{out}");
    assert_eq!(recs[1].get("name").as_str(), Some("boom"), "{out}");
    assert_eq!(recs[1].get("result").as_str(), Some("failed"), "{out}");
    assert!(
        matches!(recs[1].get("ms"), rokc_diagnostic::Json::Num(_)),
        "{out}"
    );
    assert_eq!(recs[2].get("type").as_str(), Some("summary"), "{out}");
    assert_eq!(recs[2].get("result").as_str(), Some("failed"), "{out}");
    assert_eq!(recs[2].get("passed").as_u32(), Some(1), "{out}");
    assert_eq!(recs[2].get("failed").as_u32(), Some(1), "{out}");
    assert!(!out.contains("noise"), "{out}");
    assert!(human.contains("test boom ... FAILED"), "{human}");
    assert!(human.contains("noise"), "{human}");

    // Another format is named, with the ones there are.
    let out = rokc(Some(&env), &lib, &["test", "--format", "xml"]);
    assert_exit(&out, 2);
    assert!(
        text(&out.stderr).contains("`--format` takes human, json or rokbench, not `xml`"),
        "{}",
        text(&out.stderr)
    );
}

/// `bench --format json`: the `#rokbench` line becomes a record; what the
/// bench prints itself stays off stdout.
// spec: §9.9.11 §9.9.12
#[test]
fn bench_format_json_separates_noise() {
    let Some(env) = setup() else { return };
    let dir = scratch("bench-json");
    let app = new_package(&dir, "japp", false);
    put(
        app.join("src/main.rok"),
        "use rok::core::bench::Bench;\nuse rok::core::hint::black_box;\nfn main() {}\n\
         #[bench]\nfn sum(b: Bench) {\n    println(\"noise from bench\");\n    \
         b.iter(|| black_box(1i64 + 2i64));\n}\n#[bench]\nfn idle(b: Bench) { let _ = b; }\n",
    )
    .unwrap();
    let opts = ["--iters=3", "--samples=2"];
    let (code, out, human, _) = harness_json(
        &env,
        &app,
        "bench",
        &[&opts[..], &["--format=json"]].concat(),
    );
    assert_eq!(code, 0, "{out}\n{human}");
    let recs = records(&out);
    assert_eq!(recs.len(), 3, "{out}");
    let sum = recs.iter().find(|r| r.get("name").as_str() == Some("sum"));
    let sum = sum.unwrap_or_else(|| panic!("{out}"));
    assert_eq!(sum.get("kind").as_str(), Some("bench"), "{out}");
    assert_eq!(sum.get("result").as_str(), Some("ok"), "{out}");
    assert_eq!(sum.get("samples").as_u32(), Some(2), "{out}");
    assert!(
        matches!(sum.get("median_ns"), rokc_diagnostic::Json::Num(_)),
        "{out}"
    );
    let idle = recs.iter().find(|r| r.get("name").as_str() == Some("idle"));
    assert_eq!(
        idle.unwrap().get("result").as_str(),
        Some("ignored"),
        "{out}"
    );
    let last = recs.last().unwrap();
    assert_eq!(last.get("type").as_str(), Some("summary"), "{out}");
    assert_eq!(last.get("run").as_u32(), Some(1), "{out}");
    assert_eq!(last.get("ignored").as_u32(), Some(1), "{out}");
    assert!(
        !out.contains("noise") && !out.contains("#rokbench"),
        "{out}"
    );
    assert!(human.contains("noise from bench\n"), "{human}");
    assert!(human.contains("bench result: ok. 1 run;"), "{human}");
}

/// The `#rokbench` line is frozen as 0.1.5 prints it; new fields go only
/// in the JSON records.
// spec: §9.1.2 §9.9.11
#[test]
fn rokbench_line_unchanged() {
    let Some(env) = setup() else { return };
    let dir = scratch("rokbench-frozen");
    let app = new_package(&dir, "vapp", false);
    put(
        app.join("src/main.rok"),
        "use rok::core::bench::Bench;\nfn main() {}\n\
         #[bench]\nfn one(b: Bench) { b.iter(|| 1i64); }\n",
    )
    .unwrap();
    let man = app.join("harness.manifest");
    let args = ["bench", "--iters=3", "--samples=2", "--format", "rokbench"];
    let (code, _, err) = harness(&env, &app, args[0], &args[1..]);
    assert_eq!(code, 0, "{err}");
    let manifest = fs::read_to_string(&man).unwrap();
    let exe = manifest
        .lines()
        .find_map(|l| l.strip_prefix("exe "))
        .unwrap();
    let o = Command::new(exe)
        .args(["one", "3", "2", "0", "3"])
        .output()
        .unwrap();
    let s = text(&o.stdout);
    let line = s.lines().next().unwrap_or_default();
    let keys: Vec<&str> = line
        .split(' ')
        .skip(1)
        .filter_map(|kv| kv.split_once('=').map(|(k, _)| k))
        .collect();
    assert!(line.starts_with("#rokbench name=one "), "{s}");
    assert_eq!(
        keys,
        [
            "name",
            "median_ns",
            "mad_ns",
            "samples",
            "outliers",
            "gc",
            "bytes",
            "items"
        ],
        "{s}"
    );
}

// The day's eviction of the shared lone-file cache runs inside a build:
// it must not take the directory that build just made, and its stamp
// must move (Windows keeps an empty file's time when it is rewritten).
// spec: §9.2.16
#[test]
fn lone_file_survives_the_cache_eviction() {
    let Some(env) = setup() else { return };
    let dir = scratch("anon_evict");
    let tmp = dir.join("tmp");
    let stamp = lone_cache(&tmp).join(".evicted");
    put(&stamp, "").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
    fs::File::options()
        .write(true)
        .open(&stamp)
        .unwrap()
        .set_modified(old)
        .unwrap();
    put(dir.join("hello.rok"), "println(\"hi\");\n").unwrap();
    for _ in 0..2 {
        let out = Command::new(ROKC)
            .current_dir(&dir)
            .env("ROK_HOME", workspace())
            .env("TMPDIR", &tmp)
            .env("TMP", &tmp)
            .env("TEMP", &tmp)
            .args(["run", "hello.rok", "--rokr"])
            .arg(&env.rokr)
            .output()
            .unwrap();
        assert_exit(&out, 0);
        assert_eq!(text(&out.stdout), "hi\n");
    }
    let age = fs::metadata(&stamp)
        .unwrap()
        .modified()
        .unwrap()
        .elapsed()
        .unwrap();
    assert!(age.as_secs() < 3600, "stamp not renewed: {age:?}");
}

/// The lone-file cache under temp directory `tmp`, made as rokc makes it.
fn lone_cache(tmp: &Path) -> PathBuf {
    fs::create_dir_all(tmp).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let uid = fs::metadata(tmp).unwrap().uid();
        let d = tmp.join(format!("rokc-target-{uid}"));
        fs::DirBuilder::new().mode(0o700).create(&d).unwrap();
        d
    }
    #[cfg(not(unix))]
    {
        let d = tmp.join("rokc-target");
        fs::create_dir_all(&d).unwrap();
        d
    }
}

/// `rokc <args>` in `dir` with `tmp` as the temp directory.
#[cfg(unix)]
fn rokc_in_tmp(env: &Env, dir: &Path, tmp: &Path, args: &[&str]) -> Output {
    Command::new(ROKC)
        .current_dir(dir)
        .env("ROK_HOME", workspace())
        .env_remove("ROK_TARGET_DIR")
        .env("TMPDIR", tmp)
        .env("TMP", tmp)
        .env("TEMP", tmp)
        .args(args)
        .arg("--rokr")
        .arg(&env.rokr)
        .output()
        .unwrap()
}

// The lone-file cache runs what it finds: it is the user's alone.
// spec: §9.2.16
#[cfg(unix)]
#[test]
fn lone_cache_is_private() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(env) = setup() else { return };
    let dir = scratch("anon_private");
    let tmp = dir.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    put(dir.join("hello.rok"), "println(\"hi\");\n").unwrap();
    let out = rokc_in_tmp(&env, &dir, &tmp, &["run", "-q", "hello.rok"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "hi\n");
    let uid = fs::metadata(&dir).unwrap().uid();
    let m = fs::symlink_metadata(tmp.join(format!("rokc-target-{uid}"))).unwrap();
    assert!(m.is_dir());
    assert_eq!(m.uid(), uid);
    assert_eq!(m.permissions().mode() & 0o777, 0o700);
}

// spec: §9.2.16
#[cfg(unix)]
#[test]
fn lone_cache_refuses_foreign_dir() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(env) = setup() else { return };
    let dir = scratch("anon_foreign");
    let tmp = dir.join("tmp");
    put(dir.join("hello.rok"), "println(\"hi\");\n").unwrap();
    let cache = lone_cache(&tmp);
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
    let out = rokc_in_tmp(&env, &dir, &tmp, &["run", "-q", "hello.rok"]);
    assert_exit(&out, 1);
    let uid = fs::metadata(&dir).unwrap().uid();
    let err = text(&out.stderr);
    assert!(
        err.contains(&format!(
            "{} is not private to you (owner uid {uid}, mode 0755); remove it or set \
             ROK_TARGET_DIR",
            cache.display()
        )),
        "{err}"
    );
    assert_eq!(text(&out.stdout), "");
    // A link to a directory of the user's is refused too.
    fs::remove_dir(&cache).unwrap();
    let real = lone_cache(&dir.join("elsewhere"));
    std::os::unix::fs::symlink(&real, &cache).unwrap();
    let out = rokc_in_tmp(&env, &dir, &tmp, &["run", "-q", "hello.rok"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("(a symbolic link)"),
        "{}",
        text(&out.stderr)
    );
    // `ROK_TARGET_DIR` is the way out.
    let out = Command::new(ROKC)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .env("ROK_TARGET_DIR", dir.join("t"))
        .env("TMPDIR", &tmp)
        .args(["run", "-q", "hello.rok", "--rokr"])
        .arg(&env.rokr)
        .output()
        .unwrap();
    assert_exit(&out, 0);
}

// A program's arguments after `--` reach it as given, any bytes.
// spec: §9.1.1
#[cfg(unix)]
#[test]
fn non_utf8_script_arg_passes_through() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let Some(env) = setup() else { return };
    let dir = scratch("anon_os_args");
    put(
        dir.join("args.rok"),
        "fn main() {\n    let a = rok::std::env::args();\n    println(f\"{a.len()}\");\n}\n",
    )
    .unwrap();
    let out = Command::new(ROKC)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .args(["run", "-q", "args.rok", "--rokr"])
        .arg(&env.rokr)
        .args(["--", "a"])
        .arg(OsStr::from_bytes(b"b\xffc"))
        .output()
        .unwrap();
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "3\n");
}

// `env::args` / `env::vars` replace bytes that are not UTF-8 with U+FFFD;
// `args_os` keeps them.
// spec: std 306-env §3.6.1
#[cfg(unix)]
#[test]
fn non_utf8_args_and_vars_are_lossy() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let Some(env) = setup() else { return };
    let dir = scratch("anon_lossy_args");
    let case = workspace().join("compiler/rokc/tests/cases/std_args_lossy.rok");
    fs::copy(case, dir.join("lossy.rok")).unwrap();
    let out = Command::new(ROKC)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .env("ROK_LOSSY", OsStr::from_bytes(b"v\xffw"))
        .args(["run", "-q", "lossy.rok", "--rokr"])
        .arg(&env.rokr)
        .args(["--", "a"])
        .arg(OsStr::from_bytes(b"b\xff\xfec"))
        .arg(OsStr::from_bytes(b"\xe2\x82"))
        .output()
        .unwrap();
    assert_exit(&out, 0);
    assert_eq!(
        text(&out.stdout),
        "args: 4\n\
         1: a [61] 1 bytes, lossy true\n\
         2: b\u{fffd}\u{fffd}c [62 fffd fffd 63] 4 bytes, lossy true\n\
         3: \u{fffd} [fffd] 2 bytes, lossy true\n\
         var: v\u{fffd}w\n"
    );
}

// Windows: a lone surrogate (WTF-8 in `args_os`) is one U+FFFD.
// spec: std 306-env §3.6.1
#[cfg(windows)]
#[test]
fn lone_surrogate_args_and_vars_are_lossy() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let Some(env) = setup() else { return };
    let dir = scratch("anon_lossy_args");
    let case = workspace().join("compiler/rokc/tests/cases/std_args_lossy.rok");
    fs::copy(case, dir.join("lossy.rok")).unwrap();
    let out = Command::new(ROKC)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .env("ROK_LOSSY", OsString::from_wide(&[0x76, 0xDC00, 0x77]))
        .args(["run", "-q", "lossy.rok", "--rokr"])
        .arg(&env.rokr)
        .args(["--", "a"])
        .arg(OsString::from_wide(&[0x62, 0xD800, 0x63]))
        .output()
        .unwrap();
    assert_exit(&out, 0);
    assert_eq!(
        text(&out.stdout),
        "args: 3\n\
         1: a [61] 1 bytes, lossy true\n\
         2: b\u{fffd}c [62 fffd 63] 5 bytes, lossy true\n\
         var: v\u{fffd}w\n"
    );
}

// Options, commands and paths are UTF-8; the error says where bytes may go.
// spec: §9.1.1
#[test]
fn non_utf8_option_is_usage_error() {
    let dir = scratch("anon_os_opt");
    #[cfg(unix)]
    let bad = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b"b\xffc".to_vec())
    };
    #[cfg(windows)]
    let bad = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0x62, 0xD800, 0x63])
    };
    let out = Command::new(ROKC)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .args(["run", "-q"])
        .arg(&bad)
        .output()
        .unwrap();
    assert_exit(&out, 2);
    let err = text(&out.stderr);
    assert!(
        err.contains("argument 3 (`b\u{fffd}c`) is not valid UTF-8; only a program's arguments, after `--`, may be other bytes"),
        "{err}"
    );
}

// spec: §9.1.1 §9.1.2 §9.2.2 §9.2.15
#[test]
fn lone_file_is_an_anonymous_package() {
    let Some(env) = setup() else { return };
    let dir = scratch("anon");
    put(dir.join("hello.rok"), "println(\"hi\");\n").unwrap();
    let out = rokc(Some(&env), &dir, &["run", "hello.rok"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "hi\n");
    // A lone file leaves no target/ behind; `build` puts the executable
    // beside it.
    assert!(!dir.join("target").exists());
    assert_exit(&rokc(Some(&env), &dir, &["build", "hello.rok"]), 0);
    assert!(!dir.join("target").exists());
    assert!(dir.join(exe_name("hello")).is_file());

    // Spec 4.16.8: `mod m;` loads m.rok beside the file, as in a
    // package's src/, and a change to it is a new build.
    put(dir.join("m.rok"), "pub fn f() { println(\"m\"); }\n").unwrap();
    put(dir.join("sub.rok"), "mod m;\nm::f();\n").unwrap();
    let out = rokc(Some(&env), &dir, &["run", "sub.rok"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "m\n");
    put(dir.join("m.rok"), "pub fn f() { println(\"m2\"); }\n").unwrap();
    let out = rokc(Some(&env), &dir, &["run", "sub.rok"]);
    assert_eq!(text(&out.stdout), "m2\n");
    put(dir.join("sub.rok"), "mod gone;\n").unwrap();
    let out = rokc(Some(&env), &dir, &["build", "sub.rok"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("module `gone` body not found"),
        "{}",
        text(&out.stderr)
    );

    // A directory without src/main.rok / src/lib.rok is not a package.
    assert_exit(&rokc(Some(&env), &dir, &["build"]), 1);
}

// spec: §9.1.5 §9.1.8 §9.9.4 §9.9.12
#[test]
fn json_and_colored_diagnostics() {
    let Some(env) = setup() else { return };
    let dir = scratch("diag");
    put(dir.join("bad.rok"), "let x: i32 = nope;\n").unwrap();
    let out = rokc(
        Some(&env),
        &dir,
        &["build", "--message-format=json", "bad.rok"],
    );
    assert_exit(&out, 1);
    let stdout = text(&out.stdout);
    let line = stdout.lines().next().unwrap_or_default();
    assert!(
        line.starts_with("{\"type\":\"error\",\"code\":\"E"),
        "{stdout}"
    );
    assert!(line.contains("\"file_name\":\"bad.rok\""), "{line}");
    assert!(line.contains("\"line_start\":1"), "{line}");

    // Forced colour at each depth; none when piped, with NO_COLOR, with
    // TERM=dumb or with --color=never.
    let stderr = |vars: &[(&str, &str)], args: &[&str]| {
        let mut c = Command::new(ROKC);
        c.current_dir(&dir)
            .env("ROK_HOME", workspace())
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("COLORTERM")
            .env("TERM", "xterm")
            .envs(vars.iter().copied())
            .args(args)
            .args(["build", "bad.rok", "--rokr"])
            .arg(&env.rokr);
        text(&c.output().unwrap().stderr)
    };
    let always = ["--color=always"];
    let err = stderr(&[], &always);
    assert!(err.contains("\x1b[1;91merror[E100]\x1b[0m"), "{err:?}");
    assert!(err.contains("\x1b[1;96m-->\x1b[0m"), "{err:?}");
    let err = stderr(&[("TERM", "xterm-256color")], &always);
    assert!(
        err.contains("\x1b[1;38;5;203merror[E100]\x1b[0m"),
        "{err:?}"
    );
    let err = stderr(&[("COLORTERM", "truecolor")], &always);
    assert!(
        err.contains("\x1b[1;38;2;255;95;95merror[E100]\x1b[0m"),
        "{err:?}"
    );
    let err = stderr(&[("CLICOLOR_FORCE", "1")], &[]);
    assert!(err.contains("\x1b[1;91merror[E100]\x1b[0m"), "{err:?}");
    for (vars, args) in [
        (&[][..], &[][..]),
        (&[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")][..], &[][..]),
        (&[("TERM", "dumb")][..], &[][..]),
        (&[("CLICOLOR_FORCE", "1")][..], &["--color=never"][..]),
        (&[("CLICOLOR_FORCE", "1")][..], &["--no-color"][..]),
    ] {
        let err = stderr(vars, args);
        assert!(err.contains("error[E100]"), "{err:?}");
        assert!(!err.contains('\x1b'), "{vars:?} {args:?}: {err:?}");
    }
}

// spec: §9.9.4
#[test]
fn fixes_in_json() {
    let Some(env) = setup() else { return };
    let dir = scratch("diag-fix");
    put(
        dir.join("bad.rok"),
        "let xs: List<i32> = [1];\nlet n = xs.length();\n",
    )
    .unwrap();
    let out = rokc(
        Some(&env),
        &dir,
        &["check", "--message-format=json", "bad.rok"],
    );
    assert_exit(&out, 1);
    let stdout = text(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.contains("no method `length`"))
        .unwrap_or_default();
    // `length` at line 2, col 12, bytes 36..42.
    assert!(
        line.contains(
            r#""fixes":[{"file_name":"bad.rok","line_start":2,"col_start":12,"byte_start":36,"byte_end":42,"text":"len","title":"use `len`","line_end":2,"col_end":18}]"#
        ),
        "{stdout}"
    );
    assert!(line.ends_with(r#""lint":null,"schema":1}"#), "{line}");
}

/// JSON diagnostics name the lint a warning belongs to, where its span
/// ends, and its help on its own.
// spec: §9.9.4
#[test]
fn json_names_lint_and_end() {
    let Some(env) = setup() else { return };
    let dir = scratch("diag-lint");
    put(dir.join("w.rok"), "fn main() {\n    let unused = 1;\n}\n").unwrap();
    let out = rokc(
        Some(&env),
        &dir,
        &["check", "--message-format=json", "w.rok"],
    );
    assert_exit(&out, 0);
    let stdout = text(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.contains("unused_variables"))
        .unwrap_or_else(|| panic!("{stdout}"));
    let d = rokc_diagnostic::Json::parse(line).unwrap();
    assert_eq!(d.get("schema").as_u32(), Some(1), "{line}");
    assert_eq!(d.get("type").as_str(), Some("warning"), "{line}");
    assert_eq!(d.get("lint").as_str(), Some("unused_variables"), "{line}");
    let rokc_diagnostic::Json::Arr(spans) = d.get("spans") else {
        panic!("{line}")
    };
    let primary = &spans[0];
    assert_eq!(primary.get("line_start").as_u32(), Some(2), "{line}");
    assert_eq!(primary.get("line_end").as_u32(), Some(2), "{line}");
    let (s, e) = (primary.get("col_start"), primary.get("col_end"));
    assert!(e.as_u32() > s.as_u32(), "{line}");
    assert!(
        matches!(d.get("help"), rokc_diagnostic::Json::Arr(_)),
        "{line}"
    );
}

// spec: §9.1.2 §9.1.11 / std §3.9.4
#[test]
fn fmt_check_and_rewrite() {
    let dir = scratch("fmt");
    let messy = "fn f() {\n  // keep me\n      let x = 1;   \n\n\n  x;\n}\n";
    let clean = "fn f() {\n    // keep me\n    let x = 1;\n\n    x;\n}\n";
    fs::create_dir_all(dir.join("target")).unwrap();
    put(dir.join("a.rok"), messy).unwrap();
    put(dir.join("target/skip.rok"), messy).unwrap();

    let out = rokc(None, &dir, &["fmt", "--check", "."]);
    assert_exit(&out, 1);
    assert!(text(&out.stdout).contains("a.rok"));
    assert!(
        text(&out.stderr).contains("1 file needs formatting; `rokc fmt` without `--check`"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(fs::read_to_string(dir.join("a.rok")).unwrap(), messy);

    let out = rokc(None, &dir, &["fmt", "."]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stderr), "   Formatted 1 file\n");
    assert_eq!(fs::read_to_string(dir.join("a.rok")).unwrap(), clean);
    assert_eq!(
        fs::read_to_string(dir.join("target/skip.rok")).unwrap(),
        messy
    );
    assert_exit(&rokc(None, &dir, &["fmt", "--check", "a.rok"]), 0);

    // `.rokon` data files too: re-indented, and checked to be ROKON.
    // With a rok.rokon, the directory is a package: `fmt` alone is it.
    put(dir.join("rok.rokon"), "Package {\n  deps: {}, // none\n}\n").unwrap();
    assert_exit(&rokc(None, &dir, &["fmt"]), 0);
    assert_eq!(
        fs::read_to_string(dir.join("rok.rokon")).unwrap(),
        "Package {\n    deps: {}, // none\n}\n"
    );
    put(dir.join("bad.rokon"), "Package { name: x }\n").unwrap();
    let out = rokc(None, &dir, &["fmt", "--check", "."]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("error[E002]: bare identifier `x`")
            && text(&out.stderr).contains("bad.rokon:1:17\n"),
        "{}",
        text(&out.stderr)
    );
}

/// A file saved with CRLF (spec 1.1.3) is formatted to LF line ends;
/// a line break inside a string literal is its value and stays.
// spec: §9.1.11
#[test]
fn fmt_writes_lf_and_keeps_literal_crlf() {
    let dir = scratch("fmt-crlf");
    let crlf = "fn f() {\r\n    let s = \"a\r\nb\";\r\n}\r\n";
    put(dir.join("w.rok"), crlf).unwrap();
    assert_exit(&rokc(None, &dir, &["fmt", "--check", "w.rok"]), 1);
    assert_exit(&rokc(None, &dir, &["fmt", "w.rok"]), 0);
    assert_eq!(
        fs::read_to_string(dir.join("w.rok")).unwrap(),
        "fn f() {\n    let s = \"a\r\nb\";\n}\n"
    );
}

/// `rokc <args>` in `cwd` with `HOME` (and `USERPROFILE`) at `home`;
/// panics if it runs past a minute.
fn rokc_within(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(ROKC)
        .current_dir(cwd)
        .env("ROK_HOME", workspace())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("CLICOLOR_FORCE")
        .env_remove("ROK_CLI")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > std::time::Duration::from_secs(60) {
            let _ = child.kill();
            panic!(
                "rokc {args:?} in {} still running after 60 s",
                cwd.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// A home-like directory: no package, a big `node_modules/`, and
/// `.rok` files deep down.
fn home_like(dir: &Path) {
    for i in 0..200 {
        put(
            dir.join(format!("node_modules/m{i}/x.rok")),
            "fn f() {\n  1;\n}\n",
        )
        .unwrap();
    }
    put(dir.join("Library/Caches/a/b/c.rok"), "fn f() {}\n").unwrap();
    put(dir.join(".cache/d.rok"), "fn f() {\n  1;\n}\n").unwrap();
    put(dir.join("work/target/e.rok"), "fn f() {\n  1;\n}\n").unwrap();
}

/// Outside a package `fmt` needs a path: from a home directory or an
/// empty one it says so at once, and writes nothing.
// spec: §9.1.2
#[test]
fn fmt_outside_package_needs_path() {
    let root = scratch("fmt-outside");
    let home = root.join("home");
    home_like(&home);
    let empty = root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    for (cwd, args) in [
        (&home, &["fmt"][..]),
        (&home, &["fmt", "--check"]),
        (&empty, &["fmt"]),
    ] {
        let out = rokc_within(cwd, &home, args);
        assert_exit(&out, 1);
        assert_eq!(
            text(&out.stderr),
            "rokc: error: not inside a package; pass a file or directory: `rokc fmt <path>`\n"
        );
        assert_eq!(text(&out.stdout), "");
    }
    // Through rok, the message names `rok`.
    let out = Command::new(ROKC)
        .current_dir(&empty)
        .env("ROK_CLI", "rok")
        .arg("fmt")
        .output()
        .unwrap();
    assert_exit(&out, 1);
    assert_eq!(
        text(&out.stderr),
        "rok: error: not inside a package; pass a file or directory: `rok fmt <path>`\n"
    );
    assert_eq!(
        fs::read_to_string(home.join("node_modules/m0/x.rok")).unwrap(),
        "fn f() {\n  1;\n}\n"
    );
}

/// A directory walk skips dot directories, `target/`, `node_modules/`
/// and what the `.gitignore` there or at the package root ignores.
// spec: §9.1.2
#[test]
fn fmt_skips_node_modules_and_ignored() {
    let root = scratch("fmt-skips");
    let home = root.join("home");
    home_like(&home);
    let messy = "fn f() {\n  1;\n}\n";
    put(home.join("work/a.rok"), messy).unwrap();
    put(home.join("work/gen/b.rok"), messy).unwrap();
    put(home.join("work/c.gen.rok"), messy).unwrap();
    put(home.join("work/.gitignore"), "# made\ngen/\n*.gen.rok\n").unwrap();
    let out = rokc_within(&home, &home, &["fmt", "--check", "."]);
    assert_exit(&out, 1);
    let diff = text(&out.stdout);
    assert!(diff.contains("a.rok"), "{diff}");
    for skipped in ["node_modules", "Caches", ".cache", "target"] {
        assert!(!diff.contains(skipped), "{skipped}: {diff}");
    }
    // `.gitignore` at the walk root.
    let out = rokc_within(&home.join("work"), &home, &["fmt", "--check", "."]);
    let diff = text(&out.stdout);
    assert!(diff.contains("a.rok") && !diff.contains("b.rok") && !diff.contains("c.gen.rok"));
    // At the package root, for a walk below it.
    let pkg = root.join("pkg");
    put(pkg.join("src/main.rok"), "fn main() {}\n").unwrap();
    put(pkg.join("src/gen/x.rok"), messy).unwrap();
    put(pkg.join("src/y.rok"), messy).unwrap();
    put(pkg.join(".gitignore"), "/target\ngen/\n").unwrap();
    let out = rokc_within(&pkg, &home, &["fmt", "--check", "src"]);
    let diff = text(&out.stdout);
    assert!(diff.contains("y.rok") && !diff.contains("x.rok"), "{diff}");
}

/// `fmt` says what it did: files formatted, already formatted, or none
/// found; inside a package, without a path, it formats that package from
/// any directory in it.
// spec: §9.1.2
#[test]
fn fmt_reports_counts() {
    let pkg = scratch("fmt-counts");
    let messy = "fn f() {\n  1;\n}\n";
    put(pkg.join("src/main.rok"), "fn main() {}\n").unwrap();
    put(pkg.join("src/m/a.rok"), messy).unwrap();
    put(pkg.join("tests/t.rok"), messy).unwrap();
    put(pkg.join("benches/b.rok"), messy).unwrap();
    put(pkg.join("examples/e.rok"), "fn main() {}\n").unwrap();
    put(pkg.join("old.rok"), messy).unwrap();
    let out = rokc_within(&pkg.join("src/m"), &pkg, &["fmt"]);
    assert_exit(&out, 0);
    assert_eq!(
        text(&out.stderr),
        "   Formatted 3 files, 2 already formatted\n"
    );
    assert_eq!(fs::read_to_string(pkg.join("old.rok")).unwrap(), messy);
    let out = rokc_within(&pkg, &pkg, &["fmt"]);
    assert_eq!(
        text(&out.stderr),
        "        note: 5 files already formatted\n"
    );
    let out = rokc_within(&pkg, &pkg, &["-q", "fmt"]);
    assert_eq!(text(&out.stderr), "");
    // `--check` keeps exit 1 and says how to fix it.
    put(pkg.join("src/m/a.rok"), messy).unwrap();
    let out = rokc_within(&pkg, &pkg, &["fmt", "--check"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr)
            .ends_with("note: 1 file needs formatting; `rokc fmt` without `--check` rewrites it\n"),
        "{}",
        text(&out.stderr)
    );
}

// spec: §9.1.2
#[test]
fn fmt_empty_dir_says_so() {
    let dir = scratch("fmt-empty");
    fs::create_dir_all(dir.join("sub")).unwrap();
    let out = rokc_within(&dir, &dir, &["fmt", "sub"]);
    assert_exit(&out, 0);
    assert_eq!(
        text(&out.stderr),
        "        note: no .rok or .rokon files under sub\n"
    );
    let out = rokc_within(&dir, &dir, &["fmt", "nope.rok"]);
    assert_exit(&out, 1);
    assert!(text(&out.stderr).contains("`nope.rok` does not exist"));
}

/// Symbol mangling in a host executable's symbol table.
// spec: §9.5.1 §9.5.2 §9.5.5 §9.5.7 §9.5.9
#[test]
fn symbols_follow_the_mangling_scheme() {
    let Some(env) = setup() else { return };
    if cfg!(windows) {
        eprintln!("skipped: a PE executable's symbols are in its PDB");
        return;
    }
    if Command::new("nm").arg("--version").output().is_err() {
        eprintln!("skipped: no nm");
        return;
    }
    let dir = scratch("mangle");
    let ident = "mod geom {\n    pub fn ident<T>(x: T) -> T { x }\n}\n";
    for (name, body) in [
        (
            "a",
            "#[export_name(\"rok_marker_fn\")]\npub fn marker() -> i32 { 3 }\n\
             println(f\"{geom::ident(2)} {geom::ident(\"s\")} {marker()}\");\n",
        ),
        ("b", "println(f\"{geom::ident(5)}\");\n"),
    ] {
        fs::create_dir_all(dir.join(name)).unwrap();
        put(
            dir.join(name).join("src/main.rok"),
            format!("{ident}{body}"),
        )
        .unwrap();
        assert_exit(&rokc(Some(&env), &dir, &["-q", "build", name, "-O0"]), 0);
    }
    // `(pkg hash, typehash)` of each `geom::ident` instance, and every
    // symbol (Mach-O's leading `_` dropped).
    let symbols = |name: &str| {
        let exe = dir.join(name).join("target/debug").join(name);
        let out = Command::new("nm").arg(&exe).output().unwrap();
        let all: Vec<String> = text(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().nth(2))
            .map(|s| {
                s.strip_prefix('_')
                    .filter(|_| cfg!(target_os = "macos"))
                    .unwrap_or(s)
            })
            .map(str::to_owned)
            .collect();
        let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
        let mut inst: Vec<(String, String)> = all
            .iter()
            .filter_map(|s| {
                let rest = s.strip_prefix("_Y")?;
                let (pkg, rest) = rest.split_once('_')?;
                let th = rest.strip_prefix("geom_ident_")?;
                (hex(pkg, 8) && hex(th, 16)).then(|| (pkg.to_owned(), th.to_owned()))
            })
            .collect();
        inst.sort();
        (inst, all)
    };
    let (a, a_all) = symbols("a");
    let (b, b_all) = symbols("b");
    assert_eq!(a.len(), 2, "{a_all:?}");
    assert_eq!(b.len(), 1, "{b_all:?}");
    assert_ne!(a[0].0, b[0].0, "package hashes");
    assert!(a.iter().any(|(_, th)| *th == b[0].1), "{a:?} {b:?}");
    for want in ["rok_marker_fn", "main", &format!("_Y{}__main_", a[0].0)] {
        assert!(a_all.iter().any(|s| s == want), "no {want}: {a_all:?}");
    }
    // 9.5.5: `_Y` is followed by the lowercase hex package hash, never
    // by the reserved `_YK` / `_YL` / `_YI` / `_YM`.
    for s in a_all.iter().filter_map(|s| s.strip_prefix("_Y")) {
        assert!(
            s.get(..8)
                .is_some_and(|h| h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))),
            "_Y{s}"
        );
    }
}

// spec: §9.1.1 §9.1.3 §9.1.4 §9.1.6 §9.5.4 §9.9.3 §9.9.12
#[test]
fn demangle_and_usage_errors() {
    let dir = scratch("misc");
    let out = rokc(
        None,
        &dir,
        &["demangle", "_Y3f8a1c2b_std_vec_push_a7c91d8e44b2f190"],
    );
    assert_exit(&out, 0);
    assert_eq!(
        text(&out.stdout),
        "package:  3f8a1c2b\npath:     rok::std::vec\nname:     push\n\
         generics: a7c91d8e44b2f190 (type tuple hash)\n"
    );
    assert_exit(&rokc(None, &dir, &["demangle", "_ZN3foo"]), 1);
    assert_exit(&rokc(None, &dir, &["build", "--bogus"]), 2);
    assert_exit(&rokc(None, &dir, &["frobnicate"]), 2);
    // An unknown command or option says what to write instead.
    let out = rokc(None, &dir, &["fc.rok"]);
    assert_exit(&out, 2);
    assert_eq!(
        text(&out.stderr),
        "rokc: unknown command `fc.rok`; did you mean `rokc run fc.rok`?\nrun `rokc help` for usage\n"
    );
    let out = rokc(None, &dir, &["biuld"]);
    assert!(text(&out.stderr).contains("did you mean `build`?"));
    let out = rokc(None, &dir, &["build", "--relase"]);
    assert_exit(&out, 2);
    assert_eq!(
        text(&out.stderr),
        "rokc: unknown option `--relase`; did you mean `--release`?\nrun `rokc help build` for usage\n"
    );
    // One path (`test`: and a filter); the REPL reads cells from stdin.
    assert_exit(&rokc(None, &dir, &["build", "a", "b"]), 2);
    assert_exit(&rokc(None, &dir, &["test", "a", "b", "c"]), 2);
    assert_exit(&rokc(None, &dir, &["repl", "-"]), 2);
    assert_exit(&rokc(None, &dir, &["--version"]), 0);
    let out = rokc(None, &dir, &["-V", "-v"]);
    assert!(
        text(&out.stdout).contains("\nhost: "),
        "{}",
        text(&out.stdout)
    );
    // Per-command help: the entry and its options only.
    let out = rokc(None, &dir, &["test", "--help"]);
    assert_exit(&out, 0);
    let help = text(&out.stdout);
    assert!(help.contains("test [path] [filter]") && help.contains("--threads=N"));
    assert!(
        !help.contains("demangle") && !help.contains("--iters"),
        "{help}"
    );
}

/// rokc's library and runtime are under its own sysroot (`bin/..`),
/// whatever `ROK_HOME` says; a missing one is named.
// spec: §9.1.6 §9.2.17 §9.2.19
#[test]
fn sysroot_is_beside_rokc() {
    let dir = scratch("sysroot");
    let sys = dir.join("sys");
    fs::create_dir_all(sys.join("bin")).unwrap();
    let rokc_exe = sys
        .join("bin")
        .join(format!("rokc{}", std::env::consts::EXE_SUFFIX));
    fs::copy(ROKC, &rokc_exe).unwrap();
    put(sys.join("lib/rok/core/src/lib.rok"), "fn (\n").unwrap();
    put(dir.join("x.rok"), "let x = 1;\n").unwrap();
    let run = |args: &[&str]| {
        Command::new(&rokc_exe)
            .current_dir(&dir)
            .env("ROK_HOME", workspace())
            .args(args)
            .output()
            .unwrap()
    };
    // Paths as `/`-separated, whatever the platform's separator.
    let stderr = |o: &std::process::Output| text(&o.stderr).replace('\\', "/");
    let out = run(&["check", "x.rok"]);
    assert_exit(&out, 1);
    assert!(stderr(&out).contains("sys/lib/rok/core/src/lib.rok"));
    put(sys.join("lib/rok/core/src/lib.rok"), "").unwrap();
    let out = run(&["run", "x.rok"]);
    assert_exit(&out, 1);
    let err = stderr(&out);
    let rt = format!("sys/lib/{}", rokc_target::TargetSpec::host().runtime_lib());
    assert!(
        err.contains("the runtime library is missing: no ") && err.contains(&rt),
        "{err}"
    );
    // A copy of rokc is hundreds of MB.
    let _ = fs::remove_dir_all(&dir);
}

/// `--target` links what the sysroot ships and runs what this host can
/// (Rosetta, `ROK_TEST_WINE`, node, `ROK_TEST_WASM_RUNNER`).
// spec: §0.2.4 §9.1.2 §9.2.14 §9.2.15 §9.2.18 §9.5.9
#[test]
fn cross_targets_link_from_the_sysroot() {
    let dir = scratch("cross");
    put(
        dir.join("h.rok"),
        "fn main() -> i32 {\n    println(\"ok\");\n    0\n}\n",
    )
    .unwrap();
    let out = rokc(
        None,
        &dir,
        &[
            "build",
            "h.rok",
            "--target",
            "powerpc64le-unknown-linux-gnu",
        ],
    );
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains("no runtime for powerpc64le-unknown-linux-gnu"),
        "{err}"
    );

    if setup().is_none() {
        return;
    }
    let targets = Path::new(ROKC).parent().unwrap().join("../lib/rok/targets");
    if !targets.is_dir() {
        eprintln!("skipped: no {} (targets/build.sh)", targets.display());
        return;
    }
    let rosetta = cfg!(all(target_os = "macos", target_arch = "aarch64"))
        && Command::new("arch")
            .args(["-x86_64", "/usr/bin/true"])
            .status()
            .is_ok_and(|s| s.success());
    let wine = std::env::var_os("ROK_TEST_WINE").filter(|w| !w.is_empty());
    for t in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "x86_64-apple-darwin",
        "i686-unknown-linux-gnu",
        "armv7-unknown-linux-gnueabihf",
        "riscv64gc-unknown-linux-gnu",
        "i686-pc-windows-msvc",
        "wasm32-wasi",
        "wasm32-wasi-threads",
        "wasm32-unknown-unknown",
    ] {
        if !targets.join(t).is_dir() {
            continue;
        }
        let spec = TargetSpec::builtin(t).unwrap();
        if spec.object_format == ObjectFormat::Wasm {
            wasm_target_runs(&dir, &targets, &spec);
            continue;
        }
        if spec.libc == Libc::Glibc {
            check_glibc_runtime(&spec, &fs::read(targets.join(t).join("librokr.a")).unwrap());
        }
        for o in ["-O0", "-O2"] {
            let exe = dir.join(format!("h-{t}{o}"));
            let exe_s = exe.display().to_string();
            let out = rokc(
                None,
                &dir,
                &["-q", "build", "h.rok", "--target", t, o, "-o", &exe_s],
            );
            assert_exit(&out, 0);
            let b = fs::read(&exe).unwrap();
            if spec.object_format == ObjectFormat::Elf {
                check_linux_elf(&spec, &b);
            } else if spec.object_format == ObjectFormat::Coff {
                check_x86_pe(t, &b);
                if let Some(wine) = &wine {
                    let run = Command::new(wine)
                        .arg(&exe)
                        .env("WINEDEBUG", "-all")
                        .output()
                        .unwrap();
                    assert_exit(&run, 0);
                    assert_eq!(text(&run.stdout).replace('\r', ""), "ok\n", "{t}");
                }
            } else {
                assert_eq!(&b[..4], &0xfeed_facf_u32.to_le_bytes(), "{t}");
                let cpu = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                assert_eq!(cpu, 0x0100_0007, "{t}");
                if rosetta {
                    let run = Command::new("arch")
                        .arg("-x86_64")
                        .arg(&exe)
                        .output()
                        .unwrap();
                    assert_exit(&run, 0);
                    assert_eq!(text(&run.stdout), "ok\n");
                }
            }
        }
    }
}

/// `h.rok` for a wasm32 target at -O0 / -O2, run under its host if there.
fn wasm_target_runs(dir: &Path, targets: &Path, spec: &TargetSpec) {
    let t = spec.name.as_str();
    for o in ["-O0", "-O2"] {
        let exe = dir.join(format!("h-{t}{o}.wasm"));
        let exe_s = exe.display().to_string();
        let out = rokc(
            None,
            dir,
            &["-q", "build", "h.rok", "--target", t, o, "-o", &exe_s],
        );
        assert_exit(&out, 0);
        let b = fs::read(&exe).unwrap();
        let has = |s: &[u8]| b.windows(s.len()).any(|w| w == s);
        assert_eq!(&b[..4], b"\0asm", "{t}");
        // The browser's is a reactor.
        let entry: &[u8] = if spec.reactor() {
            b"_initialize"
        } else {
            b"_start"
        };
        assert!(has(entry) && has(b"wasi_snapshot_preview1"), "{t}");
        let threads: &[&str] = if spec.threads {
            &["-W", "threads=y,shared-memory=y", "-S", "threads=y"]
        } else {
            &[]
        };
        let run = if spec.reactor() {
            Command::new("node")
                .arg(targets.join(t).join("rok.mjs"))
                .arg(&exe)
                .output()
                .ok()
        } else {
            std::env::var_os("ROK_TEST_WASM_RUNNER").map(|r| {
                Command::new(r)
                    .arg("run")
                    .args(threads)
                    .arg(&exe)
                    .output()
                    .unwrap()
            })
        };
        if let Some(run) = run {
            assert_exit(&run, 0);
            assert_eq!(text(&run.stdout), "ok\n", "{t}");
        }
    }
}

/// A browser module is a reactor; its exports stay callable from JS.
// spec: §9.5.9
#[test]
fn browser_module_exports_to_js() {
    let dir = scratch("browser-exports");
    if setup().is_none() {
        return;
    }
    let t = Path::new(ROKC)
        .parent()
        .unwrap()
        .join("../lib/rok/targets/wasm32-unknown-unknown");
    if !t.is_dir() {
        eprintln!("skipped: no {} (targets/build.sh)", t.display());
        return;
    }
    put(
        dir.join("app.rok"),
        "extern \"C\" { fn rok_gc_collect(); }\n\
         \n\
         #[export_name(\"add\")]\n\
         extern \"C\" fn add(a: i32, b: i32) -> i32 { a + b }\n\
         \n\
         #[export_name(\"strings\")]\n\
         extern \"C\" fn strings(n: i32) -> i64 {\n\
         \x20   let xs: List<str> = List::new();\n\
         \x20   let i = 0;\n\
         \x20   while i < n {\n\
         \x20       xs.push(f\"s{i}\");\n\
         \x20       if i % 1000 == 0 { unsafe { rok_gc_collect() }; }\n\
         \x20       i += 1;\n\
         \x20   }\n\
         \x20   println(f\"{xs.len()} strings, last {xs[xs.len() - 1]}\");\n\
         \x20   let total: i64 = 0;\n\
         \x20   for s in xs { total += s.len() as i64; }\n\
         \x20   total\n\
         }\n\
         \n\
         fn main() -> i32 {\n\
         \x20   println(\"main\");\n\
         \x20   3\n\
         }\n",
    )
    .unwrap();
    let script = format!(
        "import {{ load }} from {:?}; import {{ readFileSync }} from 'node:fs'; \
         const app = await load(readFileSync(process.argv[1])); \
         console.log('code', app.main()); console.log('add', app.exports.add(2, 3)); \
         console.log('total', app.exports.strings(20000) === 108890n);",
        t.join("rok.mjs").display().to_string()
    );
    for o in ["-O0", "-O2"] {
        let out = rokc(
            None,
            &dir,
            &[
                "-q",
                "build",
                "app.rok",
                "--target",
                "wasm32-unknown-unknown",
                o,
                "-o",
                "app.wasm",
            ],
        );
        assert_exit(&out, 0);
        let Ok(run) = Command::new("node")
            .args(["--input-type=module", "-e", &script, "app.wasm"])
            .current_dir(&dir)
            .output()
        else {
            eprintln!("skipped: no node");
            return;
        };
        assert_exit(&run, 0);
        assert_eq!(
            text(&run.stdout),
            "main\ncode 3\nadd 5\n20000 strings, last s19999\ntotal true\n",
            "{o}"
        );
    }
}

/// A Linux executable for `t`: machine, class, static (musl) or a PIE
/// with glibc's loader and versions up to 2.34.
fn check_linux_elf(spec: &TargetSpec, b: &[u8]) {
    let t = spec.name.as_str();
    let machine: u16 = match spec.arch.as_str() {
        "x86_64" => 62,
        "aarch64" => 183,
        "x86" => 3,
        "riscv64" => 243,
        _ => 40,
    };
    let class = if spec.pointer_width == 64 { 2 } else { 1 };
    let loader = spec.dynamic_loader.as_bytes();
    let has = |s: &[u8]| b.windows(s.len()).any(|w| w == s);
    assert_eq!(&b[..4], b"\x7fELF", "{t}");
    assert_eq!(b[4], class, "{t}: ELF class");
    assert_eq!(u16::from_le_bytes([b[18], b[19]]), machine, "{t}");
    if spec.arch == "riscv64" {
        // RVC, the double-float ABI (LP64D).
        assert_eq!(b[48], 0x5, "{t}: e_flags");
    }
    let ty = u16::from_le_bytes([b[16], b[17]]);
    if spec.dynamic_linking {
        assert_eq!(ty, 3, "{t}: a PIE");
        assert!(
            has(loader) && has(b"libc.so.6") && has(b"GLIBC_2.34"),
            "{t}"
        );
        assert!(!has(b"GLIBC_2.35"), "{t}: newer than glibc 2.34");
    } else {
        assert_eq!(ty, 2, "{t}: a static ET_EXEC");
        assert!(!has(b"ld-linux"), "{t}");
    }
}

/// A glibc runtime archive: on 32-bit targets it calls the 64-bit time
/// and file-offset interfaces (glibc 2.34+), none of the 32-bit `time_t`
/// / `off_t` ones (stdlib spec 3.3: no 2038 limit).
fn check_glibc_runtime(spec: &TargetSpec, a: &[u8]) {
    if spec.pointer_width == 64 {
        return;
    }
    let t = spec.name.as_str();
    // A symbol name in an object's string table.
    let has = |s: &str| {
        let n = s.len();
        a.windows(n + 2)
            .any(|w| w[0] == 0 && w[n + 1] == 0 && &w[1..=n] == s.as_bytes())
    };
    for old in [
        "clock_gettime",
        "clock_getres",
        "clock_nanosleep",
        "nanosleep",
        "gettimeofday",
        "fstat",
        "fstatat",
        "fstat64",
        "fstatat64",
        "lstat",
        "lstat64",
        "pthread_cond_timedwait",
        "setsockopt",
        "getsockopt",
        "sendmsg",
        "recvmsg",
        "fcntl",
        "futimens",
        "utimensat",
        "ppoll",
        "pselect",
        "timerfd_settime",
        "timerfd_gettime",
        "lseek",
        "readdir",
        "mmap",
        "openat",
        "ftruncate",
        "sendfile",
    ] {
        assert!(!has(old), "{t}: the runtime calls the 32-bit `{old}`");
    }
    for new in [
        "__clock_gettime64",
        "__nanosleep64",
        "__fstat64_time64",
        "lseek64",
    ] {
        assert!(has(new), "{t}: the runtime does not call `{new}`");
    }
}

/// An x86 PE image, large-address-aware, with an 8 MiB main stack.
fn check_x86_pe(t: &str, b: &[u8]) {
    assert_eq!(&b[..2], b"MZ", "{t}");
    let pe = u32::from_le_bytes(b[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&b[pe..pe + 4], b"PE\0\0", "{t}");
    assert_eq!(u16::from_le_bytes([b[pe + 4], b[pe + 5]]), 0x14c, "{t}");
    // IMAGE_FILE_LARGE_ADDRESS_AWARE
    assert_ne!(b[pe + 22] & 0x20, 0, "{t}");
    // SizeOfStackReserve (PE32)
    let at = pe + 24 + 72;
    assert_eq!(
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap()),
        8 << 20,
        "{t}"
    );
}

// spec: §5.7.14 §9.1.2
#[test]
fn extern_dependencies_are_top_level_names() {
    let Some(env) = setup() else { return };
    let dir = scratch("extern");
    // `greet` (a library with a submodule and a `pkg::` import) and
    // `app`, which uses it from its root and from a submodule.
    fs::create_dir_all(dir.join("greet")).unwrap();
    put(
        dir.join("greet/src/lib.rok"),
        "pub mod words;\nuse pkg::words::hello;\n\
         pub fn greet(n: i32) -> i32 { return hello() + n; }\n",
    )
    .unwrap();
    put(
        dir.join("greet/src/words.rok"),
        "pub fn hello() -> i32 { return 40; }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "mod sub;\nuse greet::greet;\nprintln(f\"{greet(1)} {sub::two()}\");\n",
    )
    .unwrap();
    put(
        dir.join("app/src/sub.rok"),
        "use greet::words::hello;\npub fn two() -> i32 { return hello() + 2; }\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["run", "app", "--extern", "greet=greet"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "41 42\n");
    // Prebuilt into deps/ with its fingerprint (9.2.3).
    for f in ["libgreet.a", "greet.rokhir", "greet.fp"] {
        assert!(dir.join("app/target/debug/deps").join(f).is_file(), "{f}");
    }
    // `--extern=name=path/src/lib.rok` works too; a missing prebuilt one
    // fails.
    let out = rokc(
        Some(&env),
        &dir,
        &[
            "build",
            "--release",
            "app",
            "--extern=greet=greet/src/lib.rok",
        ],
    );
    assert_exit(&out, 0);
    assert!(
        dir.join("app/target/release")
            .join(exe_name("app"))
            .is_file()
    );
    assert_exit(&rokc(None, &dir, &["build", "app", "--extern", "g=x.a"]), 1);
    // Without it, `greet` is unknown.
    assert_exit(&rokc(Some(&env), &dir, &["check", "app"]), 1);
    // The standard library's name is taken, as a dependency and as a
    // library's own.
    let reserved = "`rok` is reserved for the standard library; name the package something \
                    else, e.g. `rok_util`";
    let out = rokc(None, &dir, &["check", "app", "--extern", "rok=greet"]);
    assert_exit(&out, 2);
    assert!(
        text(&out.stderr).contains(reserved),
        "{}",
        text(&out.stderr)
    );
    let out = rokc(None, &dir, &["build", "greet", "--name", "rok"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains(reserved),
        "{}",
        text(&out.stderr)
    );
    // Nor can a package take a name code can't spell as its path.
    let out = rokc(None, &dir, &["check", "app", "--extern", "pkg=greet"]);
    assert_exit(&out, 2);
    assert!(
        text(&out.stderr).contains("`pkg` already names a package's own root (`pkg::`)"),
        "{}",
        text(&out.stderr)
    );
    let out = rokc(None, &dir, &["build", "greet", "--name", "match"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("`match` is a keyword"),
        "{}",
        text(&out.stderr)
    );
}

// spec: §5.7.14
#[test]
fn extern_dependency_is_a_package_boundary() {
    let Some(env) = setup() else { return };
    let dir = scratch("extern-boundary");
    // `pkg::` inside `dep` is `dep`; `dep::..` expression paths work in a
    // submodule; `pub(pkg)` / private items of `dep` stay inside it.
    fs::create_dir_all(dir.join("dep")).unwrap();
    put(
        dir.join("dep/src/lib.rok"),
        "pub mod words;\nfn secret() -> i32 { return 1; }\n\
         pub(pkg) fn internal() -> i32 { return 2; }\n\
         pub fn via_pkg() -> i32 { return pkg::words::deep() + secret(); }\n",
    )
    .unwrap();
    put(
        dir.join("dep/src/words.rok"),
        "pub fn hello() -> i32 { return 40; }\n\
         pub fn deep() -> i32 { return pkg::internal() * 10; }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "mod sub;\nfn internal() -> i32 { return 7; }\n\
         println(f\"{sub::two()} {dep::via_pkg()} {pkg::internal()}\");\n",
    )
    .unwrap();
    put(
        dir.join("app/src/sub.rok"),
        "pub fn two() -> i32 { return dep::words::hello() + 2; }\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["run", "app", "--extern", "dep=dep"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "42 21 7\n");

    fs::create_dir_all(dir.join("bad")).unwrap();
    put(
        dir.join("bad/src/main.rok"),
        "println(f\"{dep::secret()} {dep::internal()}\");\n",
    )
    .unwrap();
    let out = rokc(None, &dir, &["check", "bad", "--extern", "dep=dep"]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(err.contains("`dep::secret` is private"), "{err}");
    assert!(err.contains("`dep::internal` is `pub(pkg)`"), "{err}");

    // A user module may not take a dependency's name.
    fs::create_dir_all(dir.join("clash")).unwrap();
    put(
        dir.join("clash/src/main.rok"),
        "mod dep;\nprintln(f\"{dep::own()}\");\n",
    )
    .unwrap();
    put(
        dir.join("clash/src/dep.rok"),
        "pub fn own() -> i32 { return 3; }\n",
    )
    .unwrap();
    let out = rokc(None, &dir, &["check", "clash", "--extern", "dep=dep"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("module `dep` has the name of the dependency `dep`"),
        "{}",
        text(&out.stderr)
    );
}

/// A prebuilt library: code from the archive, generics instantiated by the
/// user, `any` across the boundary, one lazy static, defaults resolved in
/// the declaring package.
#[test]
fn defaults_resolve_in_the_declaring_package() {
    let Some(env) = setup() else { return };
    let dir = scratch("extern-defaults");
    fs::create_dir_all(dir.join("dep")).unwrap();
    put(
        dir.join("dep/src/lib.rok"),
        "fn one() -> i32 { 1 }\n\
         const LIMIT: i32 = 9;\n\
         pub struct S {\n    pub n: i32 = 5;\n}\n\
         pub struct Cfg {\n    pub f: fn() -> i32 = one;\n    pub k: i32 = LIMIT;\n}\n\
         pub fn use_s(s: S = S {}) -> i32 { s.n }\n\
         pub fn lim(x: i32 = LIMIT) -> i32 { x }\n\
         pub fn via_one<T>(t: T, x: i32 = one()) -> i32 { x }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "fn one() -> i32 { 100 }\nconst LIMIT: i32 = 200;\nstruct S {}\n\
         let c = dep::Cfg {};\n\
         println(f\"{(c.f)()} {c.k} {dep::use_s()} {dep::lim()} {dep::via_one(0)}\");\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["run", "app", "--extern", "dep=dep"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "1 9 5 9 1\n");
}

/// A sum in a prebuilt dependency's signature has the same tags in the
/// app: member order does not follow the build's type numbering (the app
/// instantiates `Dict<str, i32>` before `List<i32>`, the library after).
#[test]
fn prebuilt_dependency_sums_keep_their_tags() {
    let Some(env) = setup() else { return };
    let dir = scratch("prebuilt-sums");
    fs::create_dir_all(dir.join("dep")).unwrap();
    put(
        dir.join("dep/src/lib.rok"),
        "pub struct Zebra { pub n: i32; }\n\
         pub fn pick(b: bool) -> List<i32> | Dict<str, i32> {\n\
             if b { let xs: List<i32> = [1, 2, 3]; return xs; }\n\
             let d: Dict<str, i32> = Dict::new();\n\
             d[\"k\"] = 7;\n\
             return d;\n\
         }\n\
         pub fn size(x: List<i32> | Dict<str, i32> | Zebra) -> str {\n\
             match x {\n\
                 xs: List<i32> => f\"list {xs.len()}\",\n\
                 d: Dict<str, i32> => f\"dict {d.len()}\",\n\
                 z: Zebra => f\"zebra {z.n}\",\n\
             }\n\
         }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "struct Mine { a: i32; }\n\
         let early: Dict<str, i32> = Dict::new();\n\
         let m = Mine { a: early.len() as i32 };\n\
         println(dep::size(dep::pick(true)));\n\
         println(dep::size(dep::pick(false)));\n\
         println(dep::size(dep::Zebra { n: 4 + m.a }));\n\
         let own: Dict<str, i32> = Dict::new();\n\
         println(dep::size(own));\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["build", "dep"]);
    assert_exit(&out, 0);
    let rokhir = dir.join("dep/target/debug/dep.rokhir");
    let ext = format!("dep={}", rokhir.display());
    let out = rokc(Some(&env), &dir, &["run", "-q", "app", "--extern", &ext]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "list 3\ndict 1\nzebra 4\ndict 0\n");
}

/// Hand-off summaries survive in a prebuilt dependency's ROKHIR.
#[test]
fn prebuilt_dependency_keeps_handoff_summaries() {
    let Some(env) = setup() else { return };
    let dir = scratch("prebuilt-handoff");
    fs::create_dir_all(dir.join("dep")).unwrap();
    put(
        dir.join("dep/src/lib.rok"),
        "pub struct Bag { pub items: List<i32>; }\n\
         fn lock(xs: List<i32>) -> Mutex<List<i32>> {\n\
             return Mutex::new(xs);\n\
         }\n\
         pub fn store(xs: List<i32>) -> Mutex<List<i32>> { return lock(xs); }\n\
         pub fn count(xs: List<i32>) -> usize { return xs.len(); }\n\
         pub fn bag(xs: List<i32>) -> Bag { return Bag { items: xs }; }\n\
         impl Bag {\n\
             pub fn share(self) -> Mutex<List<i32>> { return Mutex::new(self.items); }\n\
         }\n\
         pub fn store_if(xs: List<i32>, ok: bool) -> Mutex<List<i32>> | none {\n\
             if !ok { return none; }\n\
             return lock(xs);\n\
         }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    let app = |body: &str| {
        put(
            dir.join("app/src/main.rok"),
            format!("let xs: List<i32> = [1];\n{body}\nprintln(f\"{{xs.len()}}\");\n"),
        )
        .unwrap();
    };
    let out = rokc(Some(&env), &dir, &["build", "dep"]);
    assert_exit(&out, 0);
    let rokhir = dir.join("dep/target/debug/dep.rokhir");
    let ext = format!("dep={}", rokhir.display());
    let dump = text(
        &rokc(
            None,
            &dir,
            &["inspect", "--dump", "dep/target/debug/dep.rokhir"],
        )
        .stdout,
    );
    assert!(dump.contains("--- summaries\ndep::lock\n"), "{dump}");
    // By exit: the `Mutex` (variant 0) took `xs`, `none` (1) nothing.
    assert!(dump.contains("dep::store_if\n"), "{dump}");
    assert!(dump.contains("    x 0 0\n    x 1\n"), "{dump}");
    assert!(
        !dump.contains("pub fn store(xs: List<i32>) -> Mutex<List<i32>> { return"),
        "{dump}"
    );

    app("let n = dep::count(xs);\nlet _m = dep::store(xs.clone());\nxs.push(n as i32);");
    let out = rokc(Some(&env), &dir, &["run", "-q", "app", "--extern", &ext]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "2\n");

    app("let _m = dep::store(xs);\nxs.push(2);");
    let out = rokc(Some(&env), &dir, &["build", "app", "--extern", &ext]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains(
            "error[E202]: `xs` was moved into a `Mutex` by `dep::store(xs)` at line 2 (inside \
             `lock`, lib.rok:3)"
        ),
        "{err}"
    );

    app("if dep::store_if(xs, false) is none { xs.push(2); } else { xs = [7]; }");
    let out = rokc(Some(&env), &dir, &["run", "-q", "app", "--extern", &ext]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "2\n");

    app("if let _m: Mutex<List<i32>> = dep::store_if(xs, true) { xs.push(2); }");
    let out = rokc(Some(&env), &dir, &["build", "app", "--extern", &ext]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains("error[E202]: `xs` was moved into a `Mutex` by `dep::store_if(xs, true)`"),
        "{err}"
    );

    app("let b = dep::bag(xs);\nlet _m = b.share();\nxs.push(2);");
    let out = rokc(Some(&env), &dir, &["build", "app", "--extern", &ext]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains(
            "error[E202]: `xs` shares what `b.share()` moved into a `Mutex` at line 3 (inside \
             `Bag::share`, lib.rok:9)"
        ),
        "{err}"
    );
}

/// Aggregates over 64 bytes cross a prebuilt dependency by address
/// (spec 8.6.6), both ways and through function values, whichever
/// backend built each side (`-O0`: native, `-O2`: LLVM).
// spec: §9.2.10
#[test]
fn prebuilt_dependency_passes_large_aggregates_by_address() {
    let Some(env) = setup() else { return };
    let dir = scratch("prebuilt-big");
    fs::create_dir_all(dir.join("dep")).unwrap();
    put(
        dir.join("dep/src/lib.rok"),
        "pub struct Big { pub a: [i64; 12]; pub tag: u8; }\n\
         pub fn make(k: i64) -> Big {\n\
             let b = Big { a: [0; 12], tag: 7 };\n\
             for i in 0..12 { b.a[i] = k * i as i64; }\n\
             return b;\n\
         }\n\
         pub fn total(b: Big) -> i64 { let t = 0i64; for x in b.a { t += x; } return t + b.tag as i64; }\n\
         pub fn twice(f: fn(Big) -> Big, b: Big) -> Big { return f(f(b)); }\n\
         pub fn summer() -> fn(Big) -> i64 { return total; }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "fn inc(b: dep::Big) -> dep::Big { let c = b; c.a[11] += 1; c.tag += 1; return c; }\n\
         let b = dep::make(2);\n\
         let c = dep::twice(inc, b);\n\
         let s = dep::summer();\n\
         println(f\"{dep::total(b)} {c.a[11]} {c.tag} {s(c)} {b.a[11]}\");\n",
    )
    .unwrap();
    for (dep_opt, app_opt) in [("-O2", "-O0"), ("-O0", "-O2")] {
        let out = rokc(Some(&env), &dir, &["build", dep_opt, "dep"]);
        assert_exit(&out, 0);
        let profile = if dep_opt == "-O2" { "release" } else { "debug" };
        let rokhir = dir.join(format!("dep/target/{profile}/dep.rokhir"));
        let ext = format!("dep={}", rokhir.display());
        let out = rokc(
            Some(&env),
            &dir,
            &["run", "-q", app_opt, "app", "--extern", &ext],
        );
        assert_exit(&out, 0);
        assert_eq!(text(&out.stdout), "139 24 9 143 22\n", "dep {dep_opt}");
    }
}

/// An `extern "C"` declaration of a prebuilt dependency's
/// `#[export_name]` function is that function, on both back ends.
// spec: §9.5.8
#[test]
fn extern_declaration_names_a_dependency_export() {
    let Some(env) = setup() else { return };
    let dir = scratch("prebuilt-export");
    fs::create_dir_all(dir.join("dep")).unwrap();
    put(
        dir.join("dep/src/lib.rok"),
        "#[export_name(\"dep_sum\")]\npub fn sum(x: i64, y: i64) -> i64 { x + y }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "extern \"C\" { fn dep_sum(x: i64, y: i64) -> i64; }\n\
         println(f\"{dep::sum(1, 2)} {unsafe { dep_sum(3, 4) }}\");\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["build", "dep"]);
    assert_exit(&out, 0);
    let ext = format!("dep={}", dir.join("dep/target/debug/dep.rokhir").display());
    for opts in [&["-O0"][..], &["-O0", "-C", "backend=llvm"], &["-O2"]] {
        let mut args = vec!["run", "-q"];
        args.extend_from_slice(opts);
        args.extend_from_slice(&["app", "--extern", &ext]);
        let out = rokc(Some(&env), &dir, &args);
        assert_exit(&out, 0);
        assert_eq!(text(&out.stdout), "3 7\n", "{opts:?}");
    }
}

// spec: §9.1.3 §9.4.7 §9.4.8 §9.4.9 §9.4.10 §9.9.2 §9.9.14
#[test]
fn prebuilt_dependency_is_linked_and_instantiated() {
    let Some(env) = setup() else { return };
    let dir = scratch("prebuilt");
    fs::create_dir_all(dir.join("geo")).unwrap();
    put(
        dir.join("geo/src/lib.rok"),
        "pub mod shapes;\n\
         static COUNT: Atomic<i32> = Atomic::new(0);\n\
         pub fn bump() -> i32 { return COUNT.fetch_add(1) + 1; }\n\
         pub struct Bad { pub at: i32; }\n\
         impl Display for Bad { fn fmt(self, ref f: Formatter) throws (FmtError) { try f.write_str(f\"bad {self.at}\"); } }\n\
         impl Debug for Bad { fn fmt(self, ref f: Formatter) throws (FmtError) { try Display::fmt(self, ref f); } }\n\
         impl Error for Bad {}\n\
         pub fn check(x: i32) throws -> i32 { if x < 0 { throw Bad { at: x }; } return x * 2; }\n\
         pub struct Stack<T> { items: List<T>; }\n\
         impl<T> Stack<T> {\n\
             pub fn new() -> Stack<T> { return Stack { items: List::new() }; }\n\
             pub fn push(self, v: T) { self.items.push(v); bump(); }\n\
             pub fn len(self) -> usize { return self.items.len(); }\n\
         }\n\
         pub fn largest(xs: List<any shapes::Shape>) -> i32 {\n\
             let best = 0;\n\
             for s in xs { if s.area() > best { best = s.area(); } }\n\
             return best;\n\
         }\n",
    )
    .unwrap();
    put(
        dir.join("geo/src/shapes.rok"),
        "pub trait Shape { fn area(self) -> i32; }\n\
         pub struct Sq { pub s: i32; }\n\
         impl Shape for Sq { fn area(self) -> i32 { return self.s * self.s; } }\n\
         pub fn sq(s: i32) -> any Shape { return Sq { s: s }; }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "use geo::shapes::{Shape, sq};\n\
         struct Rect { w: i32; h: i32; }\n\
         impl Shape for Rect { fn area(self) -> i32 { return self.w * self.h; } }\n\
         let st: geo::Stack<str> = geo::Stack::new();\n\
         st.push(\"a\");\n\
         println(f\"{st.len()} {geo::bump()}\");\n\
         try geo::check(-3) catch e { println(f\"caught {e}\"); };\n\
         let xs: List<any Shape> = List::new();\n\
         xs.push(Rect { w: 2, h: 9 });\n\
         xs.push(sq(4));\n\
         println(f\"{geo::largest(xs)}\");\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["build", "geo"]);
    assert_exit(&out, 0);
    let rokhir = dir.join("geo/target/debug/geo.rokhir");
    assert!(rokhir.is_file() && dir.join("geo/target/debug/libgeo.a").is_file());
    let out = rokc(
        Some(&env),
        &dir,
        &[
            "run",
            "-q",
            "app",
            "--extern",
            &format!("geo={}", rokhir.display()),
        ],
    );
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "1 2\ncaught bad -3\n18\n");
    // The `.a` names it too; the build used the archive, not the source.
    put(dir.join("geo/src/lib.rok"), "this is not rok\n").unwrap();
    let out = rokc(
        Some(&env),
        &dir,
        &[
            "run",
            "-q",
            "app",
            "--extern",
            "geo=geo/target/debug/libgeo.a",
        ],
    );
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "1 2\ncaught bad -3\n18\n");

    // The interface: concrete bodies cut, generic ones kept.
    let out = rokc(
        None,
        &dir,
        &["inspect", "--dump", "geo/target/debug/geo.rokhir"],
    );
    assert_exit(&out, 0);
    let dump = text(&out.stdout);
    // The old name prints the same, and warns.
    let old = rokc(None, &dir, &["rokhir-text", "geo/target/debug/geo.rokhir"]);
    assert_exit(&old, 0);
    assert_eq!(text(&old.stdout), dump);
    assert!(
        text(&old.stderr).contains(
            "rokc: warning: `rokc rokhir-text` is now `rokc inspect --dump` (write `rokc inspect \
             --dump geo/target/debug/geo.rokhir`); `rokhir-text` goes in 0.2.0"
        ),
        "{}",
        text(&old.stderr)
    );
    assert!(dump.contains("package: geo (hash: "), "{dump}");
    assert!(dump.contains("pub fn bump() -> i32 ;"), "{dump}");
    assert!(
        dump.contains("pub fn new() -> Stack<T> { return Stack"),
        "{dump}"
    );
    let out = rokc(None, &dir, &["inspect", "geo/target/debug/libgeo.a"]);
    assert_exit(&out, 0);
    let info = text(&out.stdout);
    for want in [
        "Package: geo\n",
        "ROKHIR version: 8\n",
        "Exports:\n",
        "  pub fn check(x: i32) throws -> i32\n",
        "Generic items (ROKHIR):\n  pub struct Stack<T>\n",
        "Concrete .o files:\n  part0.o (",
    ] {
        assert!(info.contains(want), "{want:?} in\n{info}");
    }
}

/// A prebuilt library's interface carries its `include_str!` /
/// `include_bytes!` data (spec 9.4.7): moved away from its sources and
/// data files, its generic and `const` users still get the bytes.
// spec: §9.4.7
#[test]
fn moved_prebuilt_library_keeps_its_included_files() {
    let Some(env) = setup() else { return };
    let dir = scratch("prebuilt-include");
    fs::create_dir_all(dir.join("dep/src/data")).unwrap();
    put(dir.join("dep/src/data/blob.bin"), "\x01\x02z\n").unwrap();
    put(dir.join("dep/src/data/note.txt"), "hi \"there\"\n").unwrap();
    put(
        dir.join("dep/src/lib.rok"),
        "pub const NOTE: str = include_str!(\"data/note.txt\");\n\
         pub fn blob_len<T>(t: T) -> usize {\n    let b = include_bytes!(\"data/blob.bin\");\n    b.len()\n}\n\
         #[inline]\npub fn first() -> u8 { let b = include_bytes!(\"data/blob.bin\"); b[0] }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "print(dep::NOTE);\nprintln(f\"{dep::blob_len(0)} {dep::first()}\");\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["build", "dep"]);
    assert_exit(&out, 0);
    fs::rename(dir.join("dep"), dir.join("moved")).unwrap();
    let ext = format!(
        "dep={}",
        dir.join("moved/target/debug/dep.rokhir").display()
    );
    let out = rokc(Some(&env), &dir, &["run", "-q", "app", "--extern", &ext]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "hi \"there\"\n4 1\n");
}

/// A ROKHIR from another format version or compiler, or built under
/// another name, is refused with a "rebuild" message.
// spec: §0.3.3 §9.3.4 §9.3.5 §9.4.3 §9.4.4 §9.4.5 §9.4.6 §9.4.7 §9.4.9
#[test]
fn stale_rokhir_is_rejected() {
    let Some(env) = setup() else { return };
    let dir = scratch("stale-rokhir");
    fs::create_dir_all(dir.join("dep")).unwrap();
    put(
        dir.join("dep/src/lib.rok"),
        "pub fn one() -> i32 { return 1; }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "println(f\"{dep::one()}\");\n",
    )
    .unwrap();
    assert_exit(&rokc(Some(&env), &dir, &["build", "dep"]), 0);
    let rokhir = dir.join("dep/target/debug/dep.rokhir");
    let ext = format!("dep={}", rokhir.display());
    let out = rokc(Some(&env), &dir, &["run", "-q", "app", "--extern", &ext]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "1\n");

    let other = format!("other={}", rokhir.display());
    let out = rokc(Some(&env), &dir, &["check", "app", "--extern", &other]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("built as `dep`"),
        "{}",
        text(&out.stderr)
    );

    let good = fs::read(&rokhir).unwrap();
    // The layout (9.4.3 - 9.4.7): header, sized strings (producer, meta),
    // then at the body offset the cut files and hand-off summaries.
    let word = |i: usize| u32::from_le_bytes(good[i..i + 4].try_into().unwrap());
    assert_eq!(&good[..4], b"ROKH");
    assert_eq!((word(4), word(8)), (8, 0));
    let varint = |at: &mut usize| -> usize {
        let extra = good[*at].leading_ones() as usize;
        let mut v = usize::from(good[*at] & 0xffu8.checked_shr(extra as u32 + 1).unwrap_or(0));
        for b in &good[*at + 1..=*at + extra] {
            v = (v << 8) | usize::from(*b);
        }
        *at += 1 + extra;
        v
    };
    let string = |at: &mut usize| -> String {
        let n = varint(at);
        *at += n;
        String::from_utf8(good[*at - n..*at].to_vec()).unwrap()
    };
    let mut at = 16;
    assert!(string(&mut at).starts_with("rokc "));
    assert_eq!(string(&mut at), "dep");
    let src = "pub fn one() -> i32 { return 1; }\n";
    let digest = rokc_core::hash::sha256(format!("lib.rok\0{src}\0").as_bytes());
    assert_eq!(string(&mut at), rokc_core::hash::hex(&digest)[..8]);
    let mut at = word(12) as usize;
    assert_eq!(varint(&mut at), 1);
    assert_eq!(string(&mut at), "lib.rok");
    assert_eq!(string(&mut at), "pub fn one() -> i32 ;\n");
    assert_eq!(varint(&mut at), 0);
    assert_eq!(at, good.len());
    // 9.3.5: the archive's functions are global `_Y` symbols.
    if !cfg!(windows) && Command::new("nm").arg("--version").output().is_ok() {
        let nm = Command::new("nm")
            .arg(dir.join("dep/target/debug/libdep.a"))
            .output()
            .unwrap();
        let want = format!("_Y{}_dep_one_", &rokc_core::hash::hex(&digest)[..8]);
        assert!(
            text(&nm.stdout)
                .lines()
                .any(|l| l.contains(" T ") && l.ends_with(&want)),
            "no {want}: {}",
            text(&nm.stdout)
        );
    }
    let mut bad = good.clone();
    bad[4] = 99; // format version
    put(&rokhir, &bad).unwrap();
    let out = rokc(Some(&env), &dir, &["check", "app", "--extern", &ext]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains("ROKHIR format version 99") && err.contains("rebuild"),
        "{err}"
    );

    // Another compiler's: the producer string (after the 16-byte
    // header and its length byte) differs.
    let mut bad = good;
    bad[17 + 5] ^= 1; // inside "rokc 0.."
    put(&rokhir, &bad).unwrap();
    let out = rokc(Some(&env), &dir, &["check", "app", "--extern", &ext]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains("was built by") && err.contains("rebuild"),
        "{err}"
    );
}

/// Library bodies nothing reaches are skipped by default;
/// `--check-library` lowers (so type-checks) every one of them.
// spec: §9.1.2
#[test]
fn check_library_checks_every_body() {
    let dir = scratch("check-lib");
    put(dir.join("main.rok"), "fn main() {}\n").unwrap();
    assert_exit(
        &rokc(None, &dir, &["check", "--check-library", "main.rok"]),
        0,
    );
}

/// `-C target-feature` / `-C target-cpu` reach LLVM; the native backend
/// builds for the baseline.
// spec: §9.1.2 §0.6.4
#[test]
fn target_cpu_and_features() {
    let Some(env) = setup() else { return };
    let dir = scratch("target-cpu");
    let src = "fn add(a: List<f32>, b: List<f32>, c: List<f32>) {\n    \
               for i in 0..a.len() {\n        c[i] = a[i] + b[i];\n    }\n}\n\n\
               let a: List<f32> = [];\nlet b: List<f32> = [];\nlet c: List<f32> = [];\n\
               for i in 0..64 {\n    a.push(1.0f32);\n    b.push(2.0f32);\n    \
               c.push(0.0f32);\n}\nadd(a, b, c);\nprintln(f\"{c[3]}\");\n";
    put(dir.join("v.rok"), src).unwrap();
    let asm = dir.join("target/x86_64-apple-darwin/release/build/v.s");
    let ymm = |extra: &[&str]| {
        let mut args = vec![
            "build",
            "v.rok",
            "-O2",
            "--target=x86_64-apple-darwin",
            "--emit=asm",
        ];
        args.extend_from_slice(extra);
        assert_exit(&rokc(None, &dir, &args), 0);
        fs::read_to_string(&asm).unwrap().contains("ymm")
    };
    assert!(!ymm(&[]), "the baseline has no AVX");
    assert!(ymm(&["-C", "target-feature=+avx2"]));
    assert!(ymm(&["-C", "target-cpu=haswell"]));

    let out = rokc(
        Some(&env),
        &dir,
        &["run", "v.rok", "-O2", "-C", "target-cpu=native"],
    );
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "3\n");
    let out = rokc(
        Some(&env),
        &dir,
        &["run", "v.rok", "-C", "target-cpu=native"],
    );
    assert_exit(&out, 0);
    assert!(
        text(&out.stderr).contains("the native backend (-O0) builds for the target's baseline CPU")
    );

    let out = rokc(None, &dir, &["build", "v.rok", "-C", "target-feature=avx2"]);
    assert_exit(&out, 2);
    assert!(text(&out.stderr).contains("`avx2` is not `+feature` or `-feature`"));
    let foreign = if cfg!(target_arch = "x86_64") {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    let out = rokc(
        None,
        &dir,
        &[
            "build",
            "v.rok",
            "-O2",
            &format!("--target={foreign}"),
            "-C",
            "target-cpu=native",
        ],
    );
    assert_exit(&out, 2);
    assert!(text(&out.stderr).contains("is not the build machine's platform"));
}

/// Stdlib 0.1.2: `rok::bare` names neither `rok::core` nor `rok::std`, and
/// `rok::core` does not name `rok::std` (copies of the tiers, with one
/// module each that does).
// spec: std §0.1.2
#[test]
fn library_tiers_depend_only_downward() {
    fn copy(from: &Path, to: &Path) {
        for e in fs::read_dir(from).unwrap() {
            let p = e.unwrap().path();
            let dest = to.join(p.file_name().unwrap());
            if p.is_dir() {
                copy(&p, &dest);
            } else {
                put(&dest, fs::read(&p).unwrap()).unwrap();
            }
        }
    }
    let dir = scratch("tiers");
    let lib = workspace().join("library");
    for tier in ["bare", "core"] {
        copy(&lib.join(tier), &dir.join(tier));
        let root = dir.join(tier).join("src/lib.rok");
        let text = fs::read_to_string(&root).unwrap();
        put(&root, format!("{text}\npub mod leak;\n")).unwrap();
    }
    put(
        dir.join("bare/src/leak.rok"),
        "use rok::core::sync::Mutex;\n\npub fn n() -> usize { rok::std::env::args().len() }\n",
    )
    .unwrap();
    put(
        dir.join("core/src/leak.rok"),
        "pub fn n() -> usize { rok::std::env::args().len() }\n",
    )
    .unwrap();
    put(dir.join("main.rok"), "fn main() {}\n").unwrap();
    let (core, bare) = (dir.join("core/src/lib.rok"), dir.join("bare/src/lib.rok"));
    let std = lib.join("std/src/lib.rok");
    let out = rokc(
        None,
        &dir,
        &[
            "check",
            "main.rok",
            "--bootstrap",
            core.to_str().unwrap(),
            "--bootstrap",
            std.to_str().unwrap(),
            "--bootstrap",
            bare.to_str().unwrap(),
        ],
    );
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    for want in [
        "`rok::bare` must not depend on `rok::core`",
        "`rok::bare` must not depend on `rok::std`",
        "`rok::core` must not depend on `rok::std`",
    ] {
        assert!(err.contains(want), "{want}\n{err}");
    }
}

// spec: §9.9.1 §9.9.10
#[test]
fn emit_expanded_prints_meta_expansions() {
    // `--emit=expansions` lists the nested expansion of `Tag`'s derive
    // (used at 11:5) after the attribute's.
    let dir = scratch("expanded");
    let src = "trait Named { fn type_name() -> str; }\n\
               #[impl_derive(Named)]\n\
               pub @derive {\n    impl Named for @Self {\n        \
               fn type_name() -> str { return @Self.as_str(); }\n    }\n}\n\
               #[impl_attribute(with_tag)]\n\
               pub @attribute {\n    @item\n    #[derive(Named)]\n    struct Tag {}\n}\n\
               #[with_tag]\nfn f() -> i32 { 1 }\n\
               println(Tag::type_name());\n";
    put(dir.join("m.rok"), src).unwrap();
    let out = rokc(
        None,
        &dir,
        &[
            "build",
            "m.rok",
            "--emit=expansions",
            "-o",
            "m.expanded.rok",
        ],
    );
    assert_exit(&out, 0);
    let got = fs::read_to_string(dir.join("m.expanded.rok")).unwrap();
    let want = "// expanded from this `#[with_tag]` (m.rok:14:1)\n\
                fn f() -> i32 {\n    1\n}\n\
                #[derive(Named)]\nstruct Tag {\n}\n\n\
                // expanded from this `#[derive(Named)]` (m.rok:11:5)\n\
                impl Named for Tag {\n    fn type_name() -> str {\n        \
                return \"Tag\";\n    }\n}\n";
    assert_eq!(got, want);
}

// spec: §9.1.2
#[test]
fn overflow_checks_option_overrides_the_opt_level() {
    let Some(env) = setup() else { return };
    let dir = scratch("ovf");
    put(
        dir.join("ovf.rok"),
        "fn add(a: i32, b: i32) -> i32 { return a + b; }\nprintln(f\"{add(2147483647, 1)}\");\n",
    )
    .unwrap();
    let run = |args: &[&str]| rokc(Some(&env), &dir, &[&["run", "ovf.rok"], args].concat());
    assert_exit(&run(&["-O2", "-C", "overflow-checks"]), 101);
    assert_exit(&run(&["-O0", "-C", "overflow-checks=no"]), 0);
    assert_exit(&run(&["-C", "bogus"]), 2);
}

#[test]
fn dependency_trait_is_not_the_current_package() {
    let Some(env) = setup() else { return };
    let dir = scratch("clash");
    // Spec 4.9.6: a dependency's trait ranks with core's unless `use`d.
    fs::create_dir_all(dir.join("dz")).unwrap();
    put(
        dir.join("dz/src/lib.rok"),
        "pub trait DepZero { fn zero() -> Self; }\n\
         impl DepZero for i32 { fn zero() -> i32 { return 7; } }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "println(f\"{i32::zero()}\");\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["run", "app", "--extern", "dz=dz"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("is ambiguous"),
        "{}",
        text(&out.stderr)
    );
    put(
        dir.join("app/src/main.rok"),
        "use dz::DepZero;\nprintln(f\"{i32::zero()}\");\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["run", "app", "--extern", "dz=dz"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "7\n");
}

/// Spec 6.9.8: `pkg::` in a meta block names the block's own package
/// wherever it expands, not the use site's.
#[test]
fn meta_block_pkg_names_its_package() {
    let Some(env) = setup() else { return };
    let dir = scratch("meta_pkg");
    fs::create_dir_all(dir.join("dl")).unwrap();
    put(
        dir.join("dl/src/lib.rok"),
        "pub trait Named { fn nm(self) -> str; }\n\
         pub fn tag(s: str) -> str { f\"<{s}>\" }\n\
         #[impl_derive(Named)]\n\
         pub @derive {\n    impl pkg::Named for @Self {\n        \
         fn nm(self) -> str { pkg::tag(@Self.as_str()) }\n    }\n}\n\
         #[derive(Named)]\npub struct Inner {}\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    put(
        dir.join("app/src/main.rok"),
        "use dl::Named;\n\
         fn tag(s: str) -> str { \"wrong\" }\n\
         #[derive(Named)]\nstruct Outer {}\n\
         println(Outer {}.nm());\nprintln(dl::Inner {}.nm());\n",
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["run", "app", "--extern", "dl=dl"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "<Outer>\n<Inner>\n");
}

/// Spec 2.9.5: a user impl overlapping a dependency's bounded blanket
/// impl is an error at its declaration; without the bound's impl for
/// the user type, the two are disjoint.
#[test]
fn dependency_blanket_impl_overlaps_user_impl() {
    let Some(env) = setup() else { return };
    let dir = scratch("overlap_dep");
    fs::create_dir_all(dir.join("dd")).unwrap();
    put(
        dir.join("dd/src/lib.rok"),
        "pub trait Shape {}\n\
         pub trait Describe { fn d(self) -> i32; }\n\
         impl<T: Shape> Describe for T { fn d(self) -> i32 { return 1; } }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    let main = "use dd::{Shape, Describe};\n\
                struct Mine<T> { v: T; }\n\
                impl<T> Describe for Mine<T> { fn d(self) -> i32 { return 2; } }\n\
                println(f\"{Mine { v: 1 }.d()}\");\n";
    put(dir.join("app/src/main.rok"), main).unwrap();
    let out = rokc(Some(&env), &dir, &["run", "app", "--extern", "dd=dd"]);
    assert_exit(&out, 0);
    assert_eq!(text(&out.stdout), "2\n");
    put(
        dir.join("app/src/main.rok"),
        format!("{main}impl<T> Shape for Mine<T> {{}}\n"),
    )
    .unwrap();
    let out = rokc(Some(&env), &dir, &["check", "app", "--extern", "dd=dd"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("overlap on `Mine<T>` - bounds don't make impls disjoint"),
        "{}",
        text(&out.stderr)
    );
}

// spec: std §1.10.3 §1.10.7
#[test]
fn missing_mlir_plugins_are_errors() {
    // Stdlib bare 1.10.3: a missing plugin is a clean error up front.
    let dir = scratch("mlir-plugin");
    put(dir.join("p.rok"), "println(\"hi\");\n").unwrap();
    let out = rokc(
        None,
        &dir,
        &["build", "p.rok", "--load-mlir-plugin", "/no/such/libd.so"],
    );
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("no such file"),
        "{}",
        text(&out.stderr)
    );
    let out = rokc(
        None,
        &dir,
        &["build", "p.rok", "--load-mlir-plugin=nosuchdialect"],
    );
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("ROK_MLIR_PLUGIN_PATH"),
        "{}",
        text(&out.stderr)
    );
    // A file that is no plugin is refused when loaded, which fails the
    // build.
    let Some(env) = setup() else { return };
    for flag in ["--load-mlir-plugin", "--load-mlir-pass-plugin"] {
        let out = rokc(Some(&env), &dir, &["build", "p.rok", flag, "p.rok"]);
        assert_exit(&out, 1);
        let err = text(&out.stderr);
        assert!(
            err.contains("LLVM back end failed") && err.contains("Failed to load"),
            "{err}"
        );
    }
}

/// An MLIR plugin built as docs/mlir-plugins.md says runs on rokc's MLIR.
// spec: std §1.10.2 §1.10.3 §1.10.4
#[test]
fn mlir_plugin_binds_to_rokcs_mlir() {
    let Some(env) = setup() else { return };
    // A static rokc (musl) loads no plugins.
    if cfg!(target_env = "musl") {
        return;
    }
    let dev = rokc_llvm::PLUGIN_DEV.expect("rokc links LLVM but has no plugin tree");
    let dir = scratch("mlir-plugin-run");
    let src = workspace().join("compiler/rokc/tests/plugin/answer.cpp");
    let lib = dir.join(format!(
        "{}rokt{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let mut cxx;
    if cfg!(windows) {
        cxx = Command::new(std::env::var_os("CXX").unwrap_or_else(|| "cl".into()));
        cxx.args([
            "/nologo",
            "/std:c++17",
            "/O2",
            "/DNDEBUG",
            "/GR-",
            "/MD",
            "/EHs-c-",
            "/D_HAS_EXCEPTIONS=0",
            "/Zc:__cplusplus",
            "/bigobj",
            "/w",
            "/LD",
        ])
        .arg(format!("/I{dev}/include"))
        .arg(&src)
        .arg(format!("/Fo{}\\", dir.display()))
        .arg(format!("/Fe{}", lib.display()))
        .arg("/link")
        .arg(format!("{dev}/lib/rokc.lib"));
    } else {
        // Xcode's, not a Homebrew one first in PATH; elsewhere the
        // PATH's (NixOS has no /usr/bin/c++).
        let host = if cfg!(target_vendor = "apple") {
            "/usr/bin/c++"
        } else {
            "c++"
        };
        cxx = Command::new(std::env::var_os("CXX").unwrap_or_else(|| host.into()));
        cxx.args([
            "-std=c++17",
            "-O1",
            "-DNDEBUG",
            "-fno-rtti",
            "-fno-exceptions",
            "-fPIC",
            "-fvisibility-inlines-hidden",
            "-w",
            "-shared",
        ])
        .arg(format!("-I{dev}/include"))
        .arg(&src)
        .arg("-o")
        .arg(&lib);
        if cfg!(target_vendor = "apple") {
            cxx.args(["-mmacosx-version-min=11.0", "-undefined", "dynamic_lookup"]);
        }
    }
    let out = cxx.output().unwrap();
    assert_exit(&out, 0);

    put(
        dir.join("p.rok"),
        "use rok::bare::mlir::__mlir_op;\n\nfn main() -> i32 {\n    \
         let x: i32 = unsafe { __mlir_op::<\"rokt.answer\", i32>() };\n    \
         println(f\"{x}\");\n    0\n}\n",
    )
    .unwrap();
    let lib = lib.to_str().unwrap();
    let exe = format!("p{}", std::env::consts::EXE_SUFFIX);
    let out = rokc(
        Some(&env),
        &dir,
        &[
            "build",
            "p.rok",
            "--load-mlir-plugin",
            lib,
            "--load-mlir-pass-plugin",
            lib,
            "--mlir-pass",
            "rokt-lower",
            "-o",
            &exe,
        ],
    );
    assert_exit(&out, 0);
    let run = Command::new(dir.join(&exe)).output().unwrap();
    assert_eq!(text(&run.stdout), "42\n");
}

/// `rokc --print cfg` (spec 6.3.11): the target's keys and `--cfg`.
// spec: §6.3.11 §9.1.4
#[test]
fn print_cfg_lists_the_active_cfg() {
    let dir = scratch("print_cfg");
    let out = rokc(
        None,
        &dir,
        &[
            "--print",
            "cfg",
            "--target=x86_64-unknown-linux-gnu",
            "--cfg",
            "feature=gpu",
            "-O2",
        ],
    );
    assert_exit(&out, 0);
    assert_eq!(
        text(&out.stdout),
        "feature=\"gpu\"\ntarget_arch=\"x86_64\"\ntarget_endian=\"little\"\n\
         target_env=\"gnu\"\ntarget_family=\"unix\"\ntarget_os=\"linux\"\n\
         target_pointer_width=\"64\"\n"
    );
    let out = rokc(None, &dir, &["--print", "cfg"]);
    assert!(text(&out.stdout).starts_with("debug_assertions\n"));
    assert_exit(&rokc(None, &dir, &["--print", "sysroot"]), 2);
}

/// `--print target-list` names every built-in target; `--print
/// target-spec` prints one as ROKON, which reads back as the same spec.
// spec: §0.6.2 §0.6.5 §9.1.4
#[test]
fn print_target_list_and_spec() {
    let dir = scratch("print_target");
    let out = rokc(None, &dir, &["--print", "target-list"]);
    assert_exit(&out, 0);
    let list = text(&out.stdout);
    let names: Vec<&str> = list.lines().filter(|l| !l.contains(' ')).collect();
    let mut want: Vec<&str> = rokc_target::BUILTIN.to_vec();
    for (old, new) in rokc_target::RENAMED {
        let at = want.iter().position(|t| t == old).unwrap();
        want[at] = new;
    }
    assert_eq!(names, want);
    for t in rokc_target::BUILTIN {
        let out = rokc(None, &dir, &["--print", "target-spec", "--target", t]);
        assert_exit(&out, 0);
        let spec = TargetSpec::from_rokon(&text(&out.stdout), "out", t).unwrap();
        assert_eq!(spec, TargetSpec::resolve(Some(t)).unwrap(), "{t}");
    }
    let out = rokc(None, &dir, &["--print", "target-spec"]);
    assert_exit(&out, 0);
    assert_eq!(
        text(&out.stdout),
        format!("{}\n", TargetSpec::host().to_rokon())
    );
    let out = rokc(None, &dir, &["--print", "target-spec", "--target", "nope"]);
    assert_exit(&out, 1);
    assert!(text(&out.stderr).contains("unknown target `nope`"));
}

/// `--print lldbinit`: the lldb command that loads the formatters the
/// toolchain ships (a repository build: `library/lldb/`).
// spec: §9.9.6
#[test]
fn print_lldbinit() {
    let dir = scratch("print_lldbinit");
    let out = rokc(None, &dir, &["--print", "lldbinit"]);
    assert_exit(&out, 0);
    let line = text(&out.stdout);
    let path = line
        .strip_prefix("command script import '")
        .and_then(|p| p.strip_suffix("'\n"))
        .unwrap_or_else(|| panic!("{line:?}"));
    let path = Path::new(path);
    assert!(
        path.ends_with("lldb/rok_lldb.py") && path.is_file(),
        "{line:?}"
    );
}

/// A custom target (`--target <name>.rokon`): a copy of the host's spec
/// builds the same code under its own name; its runtime is looked up in
/// `lib/rok/targets/<name>/`; a bad field is an error at its line.
// spec: §0.6.5 §9.1.2
#[test]
fn custom_target_file() {
    let Some(env) = setup() else { return };
    let dir = scratch("custom_target");
    put(
        dir.join("h.rok"),
        "fn main() -> i32 {\n    println(\"ok\");\n    0\n}\n",
    )
    .unwrap();
    put(dir.join("mine.rokon"), TargetSpec::host().to_rokon()).unwrap();
    for kind in ["llvm-ir", "obj"] {
        let emit = |extra: &[&str], out: &str| {
            let mut args = vec!["-q", "emit", "h.rok", "--emit", kind, "-o", out];
            args.extend(extra);
            assert_exit(&rokc(Some(&env), &dir, &args), 0);
            fs::read(dir.join(out)).unwrap()
        };
        let host = emit(&[], "host.out");
        let mine = emit(&["--target", "mine.rokon"], "mine.out");
        assert!(host == mine, "{kind} differs");
    }
    // A shipped C library (Linux) is looked up in `targets/mine/` too:
    // link with a sysroot whose `mine` is the host's directory.
    let host = TargetSpec::host();
    let sys = Path::new(ROKC)
        .parent()
        .unwrap()
        .join(format!("custom-target-{}", std::process::id()));
    let exe = if host.libc.shipped() {
        let _ = fs::remove_dir_all(&sys);
        fs::create_dir_all(sys.join("bin")).unwrap();
        fs::create_dir_all(sys.join("lib/rok/targets")).unwrap();
        let exe = sys.join("bin/rokc");
        // A copy: `current_exe` must name it, and a hard link would change
        // `ROKC`'s ctime, so its identity, under the tests running beside.
        fs::copy(ROKC, &exe).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            rokc_core::real_path(Path::new(ROKC).parent().unwrap().join("../lib/rok/targets"))
                .unwrap()
                .join(&host.name),
            sys.join("lib/rok/targets/mine"),
        )
        .unwrap();
        exe
    } else {
        PathBuf::from(ROKC)
    };
    let out = Command::new(&exe)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .args([
            "-q",
            "build",
            "h.rok",
            "--target",
            "mine.rokon",
            "-o",
            "mine",
        ])
        .arg("--rokr")
        .arg(&env.rokr)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&sys);
    assert_exit(&out, 0);
    let run = Command::new(dir.join("mine")).output().unwrap();
    assert_exit(&run, 0);
    assert_eq!(text(&run.stdout), "ok\n");
    // Without --rokr: the runtime of target `mine`.
    let out = rokc(None, &dir, &["build", "h.rok", "--target", "mine.rokon"]);
    assert_exit(&out, 1);
    let err = text(&out.stderr).replace('\\', "/");
    assert!(
        err.contains("no runtime for mine:") && err.contains("targets/mine"),
        "{err}"
    );
    let bad =
        TargetSpec::host()
            .to_rokon()
            .replacen("TargetSpec {", "TargetSpec {\n    tier: 1,", 1);
    put(dir.join("bad.rokon"), bad).unwrap();
    let out = rokc(
        None,
        &dir,
        &["--print", "target-spec", "--target", "bad.rokon"],
    );
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.contains("unknown field `tier`") && err.contains("bad.rokon:2:5"),
        "{err}"
    );
}

/// `isize` / `usize` limits follow the target's pointer width (stdlib
/// 2.1.4): a 32-bit target folds them to 32-bit values.
#[test]
fn pointer_sized_limits_follow_the_target() {
    let Some(env) = setup() else { return };
    let dir = scratch("pointer_width_limits");
    put(
        dir.join("w.rok"),
        "#[export_name(\"t_max\")]\npub fn t_max() -> i64 { usize::MAX as i64 + \
         usize::BITS as i64 + isize::MIN as i64 }\nfn main() {}\n",
    )
    .unwrap();
    for (target, want) in [
        ("--target=i686-unknown-linux-gnu", "ret i64 2147483679"),
        (
            "--target=x86_64-unknown-linux-gnu",
            "ret i64 -9223372036854775745",
        ),
    ] {
        let out = rokc(
            Some(&env),
            &dir,
            &[
                "build",
                "-O2",
                "--emit=llvmir",
                target,
                "w.rok",
                "-o",
                "w.ll",
            ],
        );
        assert_exit(&out, 0);
        let ir = fs::read_to_string(dir.join("w.ll")).unwrap();
        assert!(ir.contains(want), "{target}: {ir}");
    }
}

/// `#[cfg]` on an impl member (stdlib 3.7.2 `Child::pidfd`, Linux only):
/// the method exists for a Linux target and not for macOS.
#[test]
fn cfg_on_impl_members_follows_the_target() {
    let dir = scratch("cfg_impl_member");
    put(
        dir.join("p.rok"),
        "use rok::std::process::Command;\nfn main() {\n    \
         let c = try Command::new(\"true\").spawn() catch _ { return; };\n    \
         let fd = try c.pidfd() catch _ { -1 };\n    println(f\"{fd}\");\n}\n",
    )
    .unwrap();
    let check = |target: &str| rokc(None, &dir, &["check", target, "p.rok"]);
    assert_exit(&check("--target=x86_64-unknown-linux-gnu"), 0);
    let out = check("--target=aarch64-apple-darwin");
    assert_exit(&out, 1);
    assert!(text(&out.stderr).contains("no method `pidfd`"));
}

/// `#[non_exhaustive]` (spec 6.2.10): a match on the type from another
/// package needs a `_` arm even when it names every variant; inside its
/// own package it does not.
// spec: §6.2.10
#[test]
fn non_exhaustive_needs_a_catch_all_across_packages() {
    let dir = scratch("non-exhaustive");
    fs::create_dir_all(dir.join("dep")).unwrap();
    put(
        dir.join("dep/src/lib.rok"),
        "#[non_exhaustive]\npub enum E { A, B }\n\
         pub fn n(e: E) -> i32 { return match e { E::A => 1, E::B => 2 }; }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    let main = |arms: &str| {
        put(
            dir.join("app/src/main.rok"),
            format!("use dep::E;\nlet v = match E::A {{ {arms} }};\nprintln(f\"{{v}}\");\n"),
        )
        .unwrap();
    };
    main("E::A => 1, E::B => 2");
    let out = rokc(None, &dir, &["check", "app", "--extern", "dep=dep"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("needs a `_` arm"),
        "{}",
        text(&out.stderr)
    );
    main("E::A => 1, E::B => 2, _ => 3");
    assert_exit(
        &rokc(None, &dir, &["check", "app", "--extern", "dep=dep"]),
        0,
    );
}

/// A native build links the prebuilt standard library, except at `-O3`,
/// where it is compiled with the program (9.2.16).
// spec: §9.3.6 / std §0.1.3
#[test]
fn o3_compiles_std_from_source() {
    let Some(env) = setup() else { return };
    let dir = scratch("o3-std");
    put(dir.join("main.rok"), "println(\"hi\");\n").unwrap();
    for (opt, std) in [("-O2", "Std prebuilt ("), ("-O3", "Std from source")] {
        let out = rokc(Some(&env), &dir, &["run", opt, "-v", "main.rok"]);
        assert_exit(&out, 0);
        assert_eq!(text(&out.stdout), "hi\n");
        assert!(
            text(&out.stderr).contains(std),
            "{opt}: {}",
            text(&out.stderr)
        );
    }
}

/// PGO: instrument, run, merge, use; a stale profile warns. The REPL, the
/// native backend and wasm32-unknown-unknown refuse it.
// spec: §9.1.2 §9.1.7 §9.2.10 §9.9.7
#[test]
fn profile_guided_optimization_round_trip() {
    let dir = scratch("pgo");
    let main = |extra: &str| {
        let src = format!(
            "fn classify(n: i64) -> i64 {{\n    if n % 7 == 0 {{ 3 }} else if n % 3 == 0 {{ 2 }} else {{ 1 }}\n}}\n\n\
             fn main() {{\n    let s: i64 = 0;\n    for i in 0i64..100000i64 {{\n        s += classify(i);{extra}\n    }}\n    \
             println(f\"{{s}}\");\n}}\n"
        );
        put(dir.join("src/main.rok"), src).unwrap();
    };
    main("");
    let refused = |args: &[&str], code: i32, msg: &str| {
        let out = rokc(None, &dir, args);
        assert_exit(&out, code);
        assert!(text(&out.stderr).contains(msg), "{}", text(&out.stderr));
    };
    refused(
        &["build", "-C", "profile-use=missing.profdata"],
        1,
        "-C profile-use: no profile",
    );
    refused(
        &["build", "-C", "backend=native", "-C", "profile-generate"],
        2,
        "need the LLVM backend",
    );
    refused(
        &["repl", "-C", "profile-generate"],
        1,
        "the REPL does not support",
    );
    refused(&["profdata", "merge"], 2, "usage: rokc profdata merge");

    let Some(env) = setup() else { return };
    let targets = Path::new(ROKC).parent().unwrap().join("../lib/rok/targets");
    let rt = targets.join(env!("ROK_HOST")).join(if cfg!(windows) {
        "clang_rt.profile.lib"
    } else {
        "libclang_rt.profile.a"
    });
    if !rt.is_file() {
        eprintln!("skipped: no {} (targets/build.sh)", rt.display());
        return;
    }
    if targets.join("wasm32-unknown-unknown").is_dir() {
        refused(
            &[
                "build",
                "--target",
                "wasm32-unknown-unknown",
                "-C",
                "profile-generate",
            ],
            1,
            "the target has no profile runtime",
        );
    }
    let build = |args: &[&str]| {
        let out = rokc(Some(&env), &dir, &[&["-q", "build"], args].concat());
        assert_exit(&out, 0);
        text(&out.stderr)
    };
    let run = |exe: &str, profile: Option<&Path>| {
        let mut cmd = Command::new(dir.join(exe));
        cmd.current_dir(&dir).env_remove("LLVM_PROFILE_FILE");
        if let Some(p) = profile {
            cmd.env("LLVM_PROFILE_FILE", p);
        }
        let out = cmd.output().unwrap();
        assert_exit(&out, 0);
        assert_eq!(text(&out.stdout), "157144\n");
    };
    let raw = dir.join("raw");
    let raw_s = raw.display().to_string();
    let count = |d: &Path| fs::read_dir(d).map_or(0, Iterator::count);
    build(&["-C", &format!("profile-generate={raw_s}"), "-o", "instr0"]);
    run("instr0", None);
    assert_eq!(count(&raw), 1);
    fs::remove_dir_all(&raw).unwrap();
    build(&[
        "-O2",
        "-C",
        &format!("profile-generate={raw_s}"),
        "-o",
        "instr",
    ]);
    run("instr", None);
    run("instr", Some(&dir.join("env.profraw")));
    assert_eq!(count(&raw), 1);
    let merge = [
        "-q",
        "profdata",
        "merge",
        "-o",
        "p.profdata",
        &raw_s,
        "env.profraw",
    ];
    assert_exit(&rokc(None, &dir, &merge), 0);

    let err = build(&["-O2", "-C", "profile-use=p.profdata", "-o", "pgo"]);
    assert!(!err.contains("out of date"), "{err}");
    run("pgo", None);
    // The profile reaches the code: it lays out the branches otherwise.
    build(&["-O2", "--emit=asm", "-o", "plain.s"]);
    build(&[
        "-O2",
        "-C",
        "profile-use=p.profdata",
        "--emit=asm",
        "-o",
        "pgo.s",
    ]);
    assert_ne!(
        fs::read(dir.join("plain.s")).unwrap(),
        fs::read(dir.join("pgo.s")).unwrap()
    );
    // Code changed since the profile: a warning, still a build.
    main("\n        if s % 1000 == 999 {\n            println(\"x\");\n        }");
    let err = build(&["-O2", "-C", "profile-use=p.profdata", "-o", "stale"]);
    assert!(err.contains("the profile is out of date for"), "{err}");
    // A Fresh build says so again.
    let err = build(&["-O2", "-C", "profile-use=p.profdata", "-o", "stale"]);
    assert!(err.contains("the profile is out of date for"), "{err}");
}

const PGO_MAIN: &str = "fn classify(n: i64) -> i64 {\n    if n % 7 == 0 { 3 } else if n % 3 == 0 { \
                        2 } else { 1 }\n}\n\nfn main() {\n    let s: i64 = 0;\n    for i in \
                        0i64..100000i64 {\n        s += classify(i);\n    }\n    \
                        println(f\"{s}\");\n}\n";

/// `rokc profdata show`, an overflowing merge, and context-sensitive PGO.
// spec: §9.1.2 §9.9.7
#[test]
fn profdata_show_merge_warnings_and_context_sensitive_pgo() {
    let dir = scratch("pgo-cs");
    let max = 1u64 << 63;
    put(
        dir.join("big.proftext"),
        format!("# IR level Instrumentation Flag\n:ir\nhot\n1\n2\n{max}\n5\n\ncold\n2\n1\n7\n"),
    )
    .unwrap();
    let Some(env) = setup() else { return };
    let ok = |args: &[&str]| {
        let out = rokc(None, &dir, args);
        assert_exit(&out, 0);
        (text(&out.stdout), text(&out.stderr))
    };
    let (_, err) = ok(&[
        "profdata",
        "merge",
        "-o",
        "big.profdata",
        "big.proftext",
        "big.proftext",
    ]);
    assert!(
        err.contains("warning: profdata merge: big.proftext: hot: counter overflow"),
        "{err}"
    );
    let (show, _) = ok(&[
        "profdata",
        "show",
        "--all-functions",
        "--topn",
        "1",
        "big.profdata",
    ]);
    for want in [
        "Counters:\n  cold:\n    Hash: 0x0000000000000002\n    Counters: 1\n",
        "Instrumentation level: IR  entry_first = 0\nFunctions shown: 2\nTotal functions: 2\n",
        "Top 1 functions with the largest internal block counts:\n  hot, max count = ",
    ] {
        assert!(show.contains(want), "{want:?} in\n{show}");
    }

    let rt = Path::new(ROKC)
        .parent()
        .unwrap()
        .join("../lib/rok/targets")
        .join(env!("ROK_HOST"))
        .join(if cfg!(windows) {
            "clang_rt.profile.lib"
        } else {
            "libclang_rt.profile.a"
        });
    if !rt.is_file() {
        eprintln!("skipped: no {} (targets/build.sh)", rt.display());
        return;
    }
    put(dir.join("src/main.rok"), PGO_MAIN).unwrap();
    let build = |args: &[&str]| {
        let out = rokc(Some(&env), &dir, &[&["-q", "build", "-O2"], args].concat());
        assert_exit(&out, 0);
    };
    let run = |exe: &str| {
        let out = Command::new(dir.join(exe))
            .current_dir(&dir)
            .env_remove("LLVM_PROFILE_FILE")
            .output()
            .unwrap();
        assert_exit(&out, 0);
        assert_eq!(text(&out.stdout), "157144\n");
    };
    build(&["-C", "profile-generate=one", "-o", "i1"]);
    run("i1");
    ok(&["-q", "profdata", "merge", "-o", "p1.profdata", "one"]);
    build(&[
        "-C",
        "profile-use=p1.profdata",
        "-C",
        "cs-profile-generate=two",
        "-o",
        "i2",
    ]);
    run("i2");
    ok(&[
        "-q",
        "profdata",
        "merge",
        "-o",
        "p2.profdata",
        "two",
        "p1.profdata",
    ]);
    let (show, _) = ok(&["profdata", "show", "p2.profdata"]);
    assert!(show.contains("Context-sensitive records: "), "{show}");
    build(&["-C", "profile-use=p2.profdata", "-o", "pgo"]);
    run("pgo");
    let out = rokc(None, &dir, &["build", "-C", "cs-profile-generate"]);
    assert_exit(&out, 2);
    assert!(text(&out.stderr).contains("needs -C profile-use"));
}

/// PGO for wasm32-wasi: the profile runtime (with wasi-libc's emulated
/// mmap) writes through WASI; the component run by
/// `ROK_TEST_WASM_RUNNER` (wasmtime) with the directory preopened.
// spec: §9.9.7
#[test]
fn profile_guided_optimization_on_wasm32_wasi() {
    let dir = scratch("pgo-wasm");
    if setup().is_none() {
        return;
    }
    let t = Path::new(ROKC)
        .parent()
        .unwrap()
        .join("../lib/rok/targets/wasm32-wasi");
    if !t.join("libclang_rt.profile.a").is_file() {
        eprintln!("skipped: no wasm32-wasi profile runtime (targets/build.sh)");
        return;
    }
    put(dir.join("src/main.rok"), PGO_MAIN).unwrap();
    let build = |args: &[&str]| {
        let base = ["-q", "build", "--target", "wasm32-wasi", "-O2"];
        let out = rokc(None, &dir, &[&base[..], args].concat());
        assert_exit(&out, 0);
    };
    build(&["-C", "profile-generate=raw", "-o", "instr.wasm"]);
    let Some(runner) = std::env::var_os("ROK_TEST_WASM_RUNNER") else {
        eprintln!("skipped: no ROK_TEST_WASM_RUNNER to run wasm32-wasi");
        return;
    };
    let run = Command::new(runner)
        .args(["run", "--dir", ".", "instr.wasm"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_exit(&run, 0);
    assert_eq!(text(&run.stdout), "157144\n");
    let merge = ["-q", "profdata", "merge", "-o", "p.profdata", "raw"];
    assert_exit(&rokc(None, &dir, &merge), 0);
    build(&["-C", "profile-use=p.profdata", "-o", "pgo.wasm"]);
}

/// `ROK_CC` gets what a `wasm32-wasi` component link needs. A stand-in
/// driver records its arguments.
// spec: §9.1.7 §9.2.14
#[cfg(unix)]
#[test]
fn rok_cc_links_wit_components() {
    use std::os::unix::fs::PermissionsExt;
    let Some(_env) = setup() else { return };
    let rt = workspace().join("target/lib/rok/targets/wasm32-wasi/librokr.a");
    if !rt.is_file() {
        eprintln!("skipped: no wasm32-wasi runtime (targets/build.sh wasm32-wasi)");
        return;
    }
    let dir = scratch("rok-cc-wit");
    put(
        dir.join("calc.wit"),
        "package test:calc;\n\nworld calc {\n    export add: func(a: s32, b: s32) -> s32;\n}\n",
    )
    .unwrap();
    put(
        dir.join("main.rok"),
        "ffi::wit::export { \"calc.wit\" }\n\n#[wit::export]\nfn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let cc = dir.join("cc.sh");
    let args = dir.join("args");
    put(
        &cc,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 1\n",
            args.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&cc, fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(ROKC)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .env("ROK_CC", &cc)
        .args([
            "build",
            "main.rok",
            "--target=wasm32-wasi",
            "-o",
            "out.wasm",
        ])
        .output()
        .unwrap();
    assert_exit(&out, 1);
    let args = fs::read_to_string(&args).unwrap();
    for want in [
        "--target=wasm32-wasip2",
        "-Wl,--export=add",
        "-Wl,--export=cabi_realloc",
        "-mexec-model=reactor",
    ] {
        assert!(args.lines().any(|l| l == want), "{want}:\n{args}");
    }
    assert!(
        args.lines().any(|l| l.starts_with("-Wl,--component-type,")),
        "{args}"
    );
}

/// `rokc run` exit codes, argument passing, progress output and the
/// diagnostic knobs.
// spec: §1.1.1 §9.1.2 §9.1.4 §9.1.6 §9.1.7 §9.1.8 §9.9.1 §9.9.5 §9.9.8
#[test]
fn run_passes_arguments_codes_and_progress() {
    let Some(env) = setup() else { return };
    let dir = scratch("run-codes");
    let app = dir.join("app");
    let src = "use rok::std::env;\n\
               extern \"C\" { fn raise(sig: i32) -> i32; }\n\
               fn main() -> i32 {\n\
               \x20   let a = env::args();\n\
               \x20   let last = a[a.len() - 1];\n\
               \x20   println(f\"{a.len()} {last}\");\n\
               \x20   if last == \"kill\" { unsafe { raise(9) }; }\n\
               \x20   let keep: List<List<i32>> = [];\n\
               \x20   for i in 0..200000 { keep.push([i]); if keep.len() > 64 { keep = []; } }\n\
               \x20   3\n\
               }\n";
    put(app.join("src/main.rok"), src).unwrap();
    let rokr = env.rokr.display().to_string();
    let out = rokc(None, &app, &["run", "--rokr", &rokr, "--", "one", "two"]);
    assert_exit(&out, 3);
    assert_eq!(text(&out.stdout), "3 two\n");
    let err = text(&out.stderr);
    for verb in ["   Compiling app", "    Finished in ", "     Running `"] {
        assert!(err.contains(verb), "{verb:?}: {err}");
    }
    if cfg!(unix) {
        let out = rokc(None, &app, &["run", "-q", "--rokr", &rokr, "--", "kill"]);
        assert_exit(&out, 137);
    }
    let exe = app.join("target/debug").join(exe_name("app"));
    let out = Command::new(&exe)
        .env("ROKR_GC_STATS", "1")
        .output()
        .unwrap();
    let err = text(&out.stderr);
    assert!(
        err.starts_with("rok gc: ") && err.contains(" collections ("),
        "{err}"
    );

    let out = rokc(None, &app, &["clean"]);
    assert!(text(&out.stderr).contains("     Removed "));
    let out = Command::new(ROKC)
        .current_dir(&app)
        .env("ROK_HOME", workspace())
        .env("ROK_VERBOSE", "1")
        .env("ROK_TIME_PASSES", "1")
        .args(["build", "--rokr"])
        .arg(&env.rokr)
        .output()
        .unwrap();
    assert_exit(&out, 0);
    let err = text(&out.stderr);
    for want in [
        "    Building ",
        "   Stage: front end (",
        "time: ",
        " parse\n",
    ] {
        assert!(err.contains(want), "{want:?}: {err}");
    }

    let out = rokc(None, &app, &["emit"]);
    assert_exit(&out, 0);
    assert!(text(&out.stderr).contains("       Wrote "));
    assert!(app.join("target/debug/build/app.mlir").is_file());
    assert_exit(
        &rokc(None, &app, &["emit", "--emit=mlir,llvmir", "-o", "x"]),
        2,
    );
    let out = rokc(None, &app, &["help", "build"]);
    assert_exit(&out, 0);
    assert!(text(&out.stdout).contains("build [path]"));

    put(dir.join("x.no"), "println(\"no\");\n").unwrap();
    let out = rokc(None, &dir, &["run", "x.no"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("is not a Rok source file"),
        "{}",
        text(&out.stderr)
    );

    put(dir.join("ice.rok"), "let x = 1; // ice-here\n").unwrap();
    let out = Command::new(ROKC)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .env("ROKC_TEST_ICE", "ice-here")
        .args(["check", "ice.rok"])
        .output()
        .unwrap();
    assert_exit(&out, 101);
}

/// A failed build ends with what failed and the counts, as `check` does;
/// `-q` and JSON output leave the line out.
// spec: §9.1.8
#[test]
fn build_error_ends_with_summary() {
    let dir = scratch("build-summary");
    let app = new_package(&dir, "app", false);
    put(
        app.join("src/main.rok"),
        "fn main() {\n    println(y);\n}\n",
    )
    .unwrap();
    for cmd in ["build", "run"] {
        let out = rokc(None, &app, &[cmd]);
        assert_exit(&out, 1);
        let err = text(&out.stderr);
        assert!(
            err.ends_with("\nerror: could not build app (1 error, 0 warnings)\n"),
            "{err}"
        );
    }
    for quiet in [&["-q"][..], &["--message-format=json"][..]] {
        let out = rokc(None, &app, &[&["build"][..], quiet].concat());
        assert_exit(&out, 1);
        assert!(!text(&out.stderr).contains("could not build"), "{quiet:?}");
    }
    // Denied lints are the errors; the others the warnings.
    put(
        app.join("src/main.rok"),
        "fn main() {\n    let a = 1;\n    let b = 2;\n}\n",
    )
    .unwrap();
    let out = rokc(None, &app, &["build", "-D", "unused_variables"]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(
        err.ends_with("\nerror: could not build app (2 errors, 0 warnings)\n"),
        "{err}"
    );
}

/// An up-to-date build says `Fresh`, then repeats its warnings.
// spec: §9.1.2 §9.1.8
#[test]
fn fresh_build_prints_warnings_after_status() {
    let Some(env) = setup() else { return };
    let dir = scratch("fresh-warnings");
    let app = new_package(&dir, "app", false);
    put(app.join("src/main.rok"), "fn main() {\n    let a = 1;\n}\n").unwrap();
    assert_exit(&rokc(Some(&env), &app, &["build"]), 0);
    let out = rokc(Some(&env), &app, &["build"]);
    assert_exit(&out, 0);
    let err = text(&out.stderr);
    let fresh = err.find("       Fresh app").expect(&err);
    let warning = err.find("warning[").expect(&err);
    assert!(fresh < warning, "{err}");
}

/// The first build at a level builds the standard library and says so,
/// once; `Finished` counts it.
// spec: §9.1.8
#[test]
fn first_std_build_is_announced() {
    let dir = scratch("std-announce");
    let core = dir.join("core/src/lib.rok");
    // A library of its own, new on every run: nothing cached.
    let fresh_core = || {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        put(&core, format!("// {t}\n")).unwrap();
    };
    put(dir.join("x.rok"), "fn main() {}\n").unwrap();
    let core_s = core.display().to_string();
    let build = |extra: &[&str]| {
        let args = ["build", "-O0", "x.rok", "-o", "x", "--bootstrap", &core_s];
        text(&rokc(None, &dir, &[&args[..], extra].concat()).stderr)
    };
    fresh_core();
    let err = build(&[]);
    assert!(
        err.starts_with("   Compiling std (-O0, cached for next builds)\n   Compiling x (.)\n"),
        "{err}"
    );
    assert!(!build(&[]).contains("Compiling std"));
    fresh_core();
    assert!(!build(&["-q"]).contains("Compiling std"));
}

/// A lone file runs as itself; it builds beside itself, over its own
/// earlier build but never over another file.
// spec: §9.1.2 §9.1.8
#[test]
fn lone_file_run_and_build_output() {
    let Some(env) = setup() else { return };
    let dir = scratch("lone-output");
    put(dir.join("hi.rok"), "println(\"hi\");\n").unwrap();
    let out = rokc(Some(&env), &dir, &["run", "hi.rok"]);
    assert_exit(&out, 0);
    assert!(
        text(&out.stderr).contains("     Running hi.rok\n"),
        "{}",
        text(&out.stderr)
    );
    // `-v` names the cached executable.
    let out = rokc(Some(&env), &dir, &["run", "-v", "hi.rok"]);
    assert!(
        text(&out.stderr).contains("     Running `"),
        "{}",
        text(&out.stderr)
    );

    let hi = exe_name("hi");
    for _ in 0..2 {
        let out = rokc(Some(&env), &dir, &["build", "hi.rok"]);
        assert_exit(&out, 0);
        let err = text(&out.stderr);
        assert!(
            err.ends_with(&format!("       Built {hi} (use -o to put it elsewhere)\n")),
            "{err}"
        );
    }
    let out = Command::new(dir.join(&hi)).output().unwrap();
    assert_eq!(text(&out.stdout), "hi\n");

    put(dir.join("ho.rok"), "println(\"ho\");\n").unwrap();
    let ho = dir.join(exe_name("ho"));
    put(&ho, "mine").unwrap();
    let out = rokc(Some(&env), &dir, &["build", "ho.rok"]);
    assert_exit(&out, 1);
    let err = text(&out.stderr);
    assert!(err.contains("build ho.rok -o <file>`"), "{err}");
    assert_eq!(fs::read_to_string(&ho).unwrap(), "mine");
    let other = dir.join(exe_name("other"));
    let out = rokc(
        Some(&env),
        &dir,
        &["build", "ho.rok", "-o", other.to_str().unwrap()],
    );
    assert_exit(&out, 0);
    assert!(other.is_file());
}

/// `fmt --diff` shows the old line before the new one.
// spec: §9.1.2
#[test]
fn fmt_diff_old_before_new() {
    let dir = scratch("fmt-diff-order");
    put(dir.join("a.rok"), "fn f() {\nlet x = 1;\n}\n").unwrap();
    let out = rokc(None, &dir, &["fmt", "--diff", "a.rok"]);
    let s = text(&out.stdout);
    let old = s.find("-    2 let x = 1;\n").expect(&s);
    let new = s.find("+    2     let x = 1;\n").expect(&s);
    assert!(old < new, "{s}");
}

/// `doc --open` opens the index in `$BROWSER`; one that cannot start
/// is a warning naming the file.
// spec: §9.1.2
#[test]
fn doc_open_accepted() {
    let dir = scratch("doc-open");
    let app = new_package(&dir, "app", true);
    let doc = |browser: &Path| {
        Command::new(ROKC)
            .current_dir(&app)
            .env("ROK_HOME", workspace())
            .env("BROWSER", browser)
            .args(["doc", "--open"])
            .output()
            .unwrap()
    };
    // Any program that starts will do: rokc itself.
    let out = doc(Path::new(ROKC));
    assert_exit(&out, 0);
    assert!(
        !text(&out.stderr).contains("cannot open"),
        "{}",
        text(&out.stderr)
    );
    let out = doc(&dir.join("no-such-browser"));
    assert_exit(&out, 0);
    let err = text(&out.stderr);
    assert!(
        err.contains("warning: cannot open a browser") && err.contains("index.html in one"),
        "{err}"
    );
}

/// `doc --std` without a path, in a package: the standard library alone,
/// and the status line says so instead of a temporary package's path.
// spec: §9.1.2
#[test]
fn doc_std_alone_names_the_library() {
    let dir = scratch("doc-std-alone");
    let app = new_package(&dir, "app", true);
    let out = Command::new(ROKC)
        .current_dir(&app)
        .env("ROK_HOME", workspace())
        .args(["doc", "--std"])
        .output()
        .unwrap();
    assert_exit(&out, 0);
    let err = text(&out.stderr);
    assert!(
        err.contains(" the standard library\n") && !err.contains("rokc-"),
        "{err}"
    );
    assert!(app.join("target/doc/rok-std").is_dir());
    assert!(!app.join("target/doc/app").exists());
}

/// `rokc completions <shell>`: rokc's commands and their flags, from the
/// same text as `rokc help`.
// spec: §9.1.4
#[test]
fn completions_follow_help() {
    let dir = scratch("rokc-completions");
    for shell in ["bash", "zsh", "fish", "powershell"] {
        let out = rokc(None, &dir, &["completions", shell]);
        assert_exit(&out, 0);
        let script = text(&out.stdout);
        for cmd in ["build", "profdata", "bindgen"] {
            assert!(script.contains(cmd), "{shell} lacks {cmd}");
        }
        for flag in ["emit", "target-dir", "check-library"] {
            // fish spells `--x` as `-l x`.
            let want = if shell == "fish" {
                format!("-l {flag}")
            } else {
                format!("--{flag}")
            };
            assert!(script.contains(&want), "{shell} lacks {want}");
        }
    }
    // Windows runners may resolve `bash` to WSL, which cannot read the path.
    #[cfg(unix)]
    {
        let out = rokc(None, &dir, &["completions", "bash"]);
        let file = dir.join("rokc.bash");
        fs::write(&file, &out.stdout).unwrap();
        let check = Command::new("bash").arg("-n").arg(&file).output().unwrap();
        assert!(check.status.success(), "{}", text(&check.stderr));
    }
    assert_exit(&rokc(None, &dir, &["completions", "tcsh"]), 2);
    assert_exit(&rokc(None, &dir, &["completions"]), 2);
}

/// The runtime ABI in emitted code: upstream dialects only, the TLS block
/// passed as a hidden last argument and read only by the C `main`, fixed
/// slot offsets (`p` is the pointer size), plain C runtime declarations,
/// no unwinding.
// spec: §0.2.3 §8.3.1 §8.3.2 §8.3.4 §8.3.5 §8.3.8 §8.4.12 §8.5.6 §8.5.10 §8.5.11 §8.6.4 §8.6.7
// spec: §8.6.8 §8.7.3 §8.7.4 §8.7.6 §8.7.7 §8.9.11 §9.2.12 / std §1.5.5
#[test]
fn emitted_code_follows_the_runtime_abi() {
    let Some(env) = setup() else { return };
    let dir = scratch("abi");
    let pkg = dir.join("abi");
    let src = "use rok::bare::volatile;\nuse rok::bare::ptr::Ptr;\nuse rok::std::env;\n\
               struct P { a: i64; b: str; }\n\
               #[inline(never)]\nfn keep(x: i64) -> i64 { x * 3 + 1 }\n\
               fn mk(n: i64) -> P { P { a: n, b: \"s\" } }\n\
               fn len_of(xs: List<i64>) -> usize { xs.len() }\n\
               fn fails(n: i64) throws -> i64 {\n\
               \x20   if n > 5 { throw ValueError { msg: \"big\" }; }\n\
               \x20   n\n\
               }\n\
               fn down(n: i64, acc: i64) -> i64 { if n == 0 { acc } else { down(n - 1, acc + 1) } }\n\
               fn main() {\n\
               \x20   let t = 0i64;\n\
               \x20   for i in 0..10i64 { t += keep(i) + mk(i).a; }\n\
               \x20   let p: Ptr<i64> = Ptr::dangling();\n\
               \x20   let r = try? fails(t);\n\
               \x20   let deep = down(env::args().len() as i64 * 20000000, 0);\n\
               \x20   println(f\"{t} {len_of([1, 2])} {r ?? -1} {deep}\");\n\
               \x20   if t < 0 {\n\
               \x20       unsafe { volatile::write(p, 1i64); println(f\"{volatile::read(p.as_const())}\"); }\n\
               \x20   }\n\
               }\n";
    put(pkg.join("src/main.rok"), src).unwrap();
    let emit = |args: &[&str]| {
        let mut all = vec!["-q", "emit"];
        all.extend_from_slice(args);
        assert_exit(&rokc(None, &pkg, &all), 0);
    };
    let read = |p: &str| fs::read_to_string(pkg.join("target").join(p)).unwrap();

    emit(&["--emit=mlir,llvmir"]);
    let mlir = read("debug/build/abi.mlir");
    let dialects: std::collections::BTreeSet<&str> = mlir
        .lines()
        .filter_map(|l| {
            let op = l.trim_start();
            let op = op
                .split_once(" = ")
                .map_or(op, |(_, r)| r)
                .trim_start_matches('"');
            let (d, rest) = op.split_once('.')?;
            (!d.is_empty()
                && d.bytes().all(|b| b.is_ascii_lowercase())
                && rest.starts_with(|c: char| c.is_ascii_lowercase()))
            .then_some(d)
        })
        .collect();
    let upstream = ["arith", "cf", "func", "index", "llvm", "math", "scf"];
    assert!(
        dialects.iter().all(|d| upstream.contains(d)),
        "{dialects:?}"
    );
    for want in [
        "llvm.mlir.global external thread_local @rok_tls()",
        "[88] : (!llvm.ptr) -> !llvm.ptr, i8",
        "llvm.call @rok_runtime_yield() ",
        "llvm.func @rok_gc_alloc_slow(i64, i64, i32) -> !llvm.ptr",
        "llvm.func @rok_runtime_yield()",
        "llvm.call @rok_gc_alloc_slow(",
    ] {
        assert!(mlir.contains(want), "no {want:?} in the MLIR");
    }
    for op in [
        "func.func",
        "func.call",
        "func.return",
        "arith.addi",
        "arith.muli",
        "arith.cmpi",
        "cf.cond_br",
        "cf.br",
        "llvm.alloca",
        "llvm.load",
        "llvm.store",
    ] {
        assert!(mlir.contains(&format!("{op} ")), "no `{op}` in the MLIR");
    }
    assert!(!mlir.contains("llvm.coro"), "`llvm.coro.*` in the MLIR");
    let ll = read("debug/build/abi.ll");
    for want in [
        "@rok_tls = external thread_local",
        "@llvm.threadlocal.address.p0(ptr @rok_tls)",
        "i32 8\n",
        "i32 16\n",
        "call ptr @rok_gc_alloc_slow(",
        "declare ptr @rok_gc_alloc_slow(i64, i64, i32)",
        "declare void @rok_runtime_yield()",
        "load volatile",
        "store volatile",
    ] {
        assert!(ll.contains(want), "no {want:?} in the LLVM IR");
    }
    for unwanted in ["invoke ", "landingpad", "personality", "addrspace("] {
        assert!(!ll.contains(unwanted), "{unwanted:?} in the LLVM IR");
    }
    // A rok body takes its caller's `rok_tls` address last (8.3.8): only
    // what is entered from outside rok (the C `main`; a lazy static's
    // initializer, off the hot path) reads TLS.
    let readers: Vec<&str> = ll
        .split("\ndefine ")
        .skip(1)
        .filter(|d| d[..d.find("\n}").unwrap_or(d.len())].contains("@llvm.threadlocal.address"))
        .map(|d| d.lines().next().unwrap_or_default())
        .filter(|d| !d.contains("_.init("))
        .collect();
    assert!(
        readers.len() == 1 && readers[0].contains("@main("),
        "TLS read outside the C `main`: {readers:#?}"
    );
    assert!(
        ll.lines()
            .any(|l| l.starts_with("define ") && l.contains("_keep_(i64 %0, ptr %1)")),
        "`keep` takes no `rok_tls` address"
    );
    assert!(
        ll.lines()
            .any(|l| l.starts_with("define ") && l.contains("_len_0of_(") && l.contains("ptr")),
        "a List argument is a pointer"
    );

    emit(&["--emit=llvmir", "--target=i686-unknown-linux-gnu"]);
    let ll = read("i686-unknown-linux-gnu/debug/build/abi.ll");
    for want in [", i32 4\n", ", i32 8\n", ", i32 48\n"] {
        assert!(ll.contains(want), "no {want:?} in the i686 LLVM IR");
    }
    for (target, mechanism) in [
        ("x86_64-unknown-linux-gnu", "%fs:"),
        ("aarch64-unknown-linux-gnu", "TPIDR_EL0"),
        ("aarch64-apple-darwin", "_rok_tls@TLVPPAGE"),
        ("x86_64-pc-windows-msvc", "_tls_index"),
        ("wasm32-wasi-threads", "__tls_base"),
    ] {
        emit(&["--emit=asm", "--target", target]);
        let asm = read(&format!("{target}/debug/build/abi.s"));
        assert!(asm.contains(mechanism), "{target}: no {mechanism:?}");
    }

    emit(&["--emit=asm", "-O2"]);
    let asm = read("release/build/abi.s");
    assert!(
        asm.lines()
            .any(|l| l.contains("_keep_") && (l.contains("bl\t") || l.contains("call"))),
        "`#[inline(never)]` was inlined"
    );
    let out = rokc(Some(&env), &pkg, &["run", "-q", "-O2"]);
    assert_exit(&out, 0);
    assert!(
        text(&out.stdout).ends_with(" 20000000\n"),
        "{}",
        text(&out.stdout)
    );
}

/// `#[cold]` (6.2.6): at -O2 the function keeps its call, out of the
/// caller's hot path, and LLVM sees it as cold.
// spec: §6.2.6
#[test]
fn cold_functions_stay_out_of_line() {
    let Some(_env) = setup() else { return };
    let dir = scratch("cold");
    let pkg = dir.join("cold");
    let src = "#[cold]\nfn slow(x: i64) -> i64 { x * 3 + 1 }\n\
               fn main() {\n\
               \x20   let t = 0i64;\n\
               \x20   for i in 0..10i64 { t += slow(i); }\n\
               \x20   println(f\"{t}\");\n\
               }\n";
    put(pkg.join("src/main.rok"), src).unwrap();
    assert_exit(&rokc(None, &pkg, &["-q", "emit", "--emit=mlir", "-O2"]), 0);
    let mlir = fs::read_to_string(pkg.join("target/release/build/cold.mlir")).unwrap();
    assert!(
        mlir.lines()
            .any(|l| l.contains("_slow_") && l.contains(r#""noinline", "cold""#)),
        "`#[cold]` is not noinline + cold"
    );
    assert_exit(&rokc(None, &pkg, &["-q", "emit", "--emit=asm", "-O2"]), 0);
    let asm = fs::read_to_string(pkg.join("target/release/build/cold.s")).unwrap();
    assert!(
        asm.lines()
            .any(|l| l.contains("_slow_") && (l.contains("bl\t") || l.contains("call"))),
        "`#[cold]` was inlined"
    );
}

/// `walk`'s two safepoint-free returns neither link nor unlink its frame.
// spec: §8.4.7
#[test]
fn frames_link_only_on_paths_that_reach_a_safepoint() {
    let Some(_env) = setup() else { return };
    let dir = scratch("lazylink");
    let pkg = dir.join("lazylink");
    let src = "fn walk(xs: List<str>, i: usize, acc: str) -> str {\n\
               \x20   if i >= xs.len() { return acc; }\n\
               \x20   if i % 3 == 0 { let s = f\"{acc}{xs[i]}\"; return walk(xs, i + 1, s); }\n\
               \x20   acc\n\
               }\n\
               fn main() { println(walk([\"a\", \"b\"], 0, \"\")); }\n";
    put(pkg.join("src/main.rok"), src).unwrap();
    assert_exit(&rokc(None, &pkg, &["-q", "emit", "--emit=mlir"]), 0);
    let mlir = fs::read_to_string(pkg.join("target/debug/build/lazylink.mlir")).unwrap();
    let start = mlir.find("__walk_(").expect("no walk");
    let start = mlir[..start].rfind("func.func").unwrap();
    let walk = &mlir[start..start + mlir[start..].find("\n  }").unwrap()];
    assert_eq!(walk.matches("func.return").count(), 3, "{walk}");
    let top = walk
        .lines()
        .find_map(|l| {
            let (v, rest) = l.trim().split_once(" = ")?;
            rest.starts_with("llvm.getelementptr inbounds")
                .then_some(())
                .filter(|()| rest.contains("[0] : (!llvm.ptr) -> !llvm.ptr, i8"))
                .map(|()| v)
        })
        .expect("no shadow-stack top");
    let stores = walk
        .lines()
        .filter(|l| l.trim().starts_with("llvm.store") && l.contains(&format!(", {top} :")))
        .count();
    assert_eq!(stores, 2, "{walk}");
}

/// A loop whose only safepoints are its polls roots its lists and links
/// a frame on the polls' yield paths alone.
// spec: §8.4.8
#[test]
fn poll_roots_link_only_on_the_yield_path() {
    let Some(_env) = setup() else { return };
    let dir = scratch("pollroots");
    let pkg = dir.join("pollroots");
    let src = "fn scan(xs: List<i64>, n: i64) -> i64 {\n\
               \x20   let t: i64 = 0;\n\
               \x20   let k: i64 = 0;\n\
               \x20   while k != n {\n\
               \x20       let i: usize = 0;\n\
               \x20       while i < xs.len() { t += xs[i]; i += 1; }\n\
               \x20       k += 1;\n\
               \x20   }\n\
               \x20   t\n\
               }\n\
               fn main() { println(f\"{scan([1, 2], 3)}\"); }\n";
    put(pkg.join("src/main.rok"), src).unwrap();
    assert_exit(&rokc(None, &pkg, &["-q", "emit", "--emit=mlir"]), 0);
    let mlir = fs::read_to_string(pkg.join("target/debug/build/pollroots.mlir")).unwrap();
    let start = mlir.find("__scan_(").expect("no scan");
    let start = mlir[..start].rfind("func.func").unwrap();
    let scan = &mlir[start..start + mlir[start..].find("\n  }").unwrap()];
    let top = scan
        .lines()
        .find_map(|l| {
            let (v, rest) = l.trim().split_once(" = ")?;
            rest.starts_with("llvm.getelementptr inbounds")
                .then_some(())
                .filter(|()| rest.contains("[0] : (!llvm.ptr) -> !llvm.ptr, i8"))
                .map(|()| v)
        })
        .expect("no shadow-stack top");
    let (mut block, mut on_yield) = ("", 0);
    for l in scan.lines().map(str::trim) {
        if l.starts_with('^') {
            block = l;
            continue;
        }
        let rooting = l.starts_with("llvm.store")
            && (l.contains(&format!(", {top} :")) || l.contains(", %rc"));
        if rooting {
            assert!(block.starts_with("^rok_yield"), "{l} in {block}:\n{scan}");
            on_yield += 1;
        }
    }
    assert!(on_yield > 0, "{scan}");
}

/// Every C symbol the runtime defines is `rok_`-prefixed.
// spec: §8.5.8
#[test]
fn runtime_symbols_carry_the_rok_prefix() {
    if cfg!(windows) || Command::new("nm").arg("--version").output().is_err() {
        eprintln!("skipped: no nm");
        return;
    }
    let env = common::setup_runtime();
    let out = Command::new("nm")
        .arg("-g")
        .arg(&env.rokr)
        .output()
        .unwrap();
    assert_exit(&out, 0);
    let listing = text(&out.stdout);
    let defined: std::collections::BTreeSet<&str> = listing
        .lines()
        .filter_map(|l| match l.split_whitespace().collect::<Vec<_>>()[..] {
            [_, kind, name] if "TDBRSC".contains(kind) => Some(name),
            _ => None,
        })
        .map(|s| {
            s.strip_prefix('_')
                .filter(|_| cfg!(target_os = "macos"))
                .unwrap_or(s)
        })
        .collect();
    let toolchain = |s: &str| {
        // Rust-mangled and reserved `_` names, compiler builtins.
        ["_", "rust_", "anon.", "ltmp", "atomic_", "."]
            .iter()
            .any(|p| s.starts_with(p))
    };
    let foreign: Vec<&&str> = defined
        .iter()
        .filter(|s| !toolchain(s) && !s.starts_with("rok_"))
        .collect();
    assert!(
        foreign.is_empty(),
        "runtime symbols without `rok_`: {foreign:?}"
    );
    for want in [
        "rok_gc_alloc_slow",
        "rok_runtime_init",
        "rok_thread_spawn",
        "rok_coro_park",
        "rok_task_key",
        "rok_io_poll",
        "rok_timer_register",
        "rok_panic",
    ] {
        assert!(defined.contains(want), "the runtime lacks `{want}`");
    }
}

/// Stdlib 1.8: a prebuilt dependency's ROKHIR carries the `.wit` files
/// its `ffi::wit` items read (one outside its source dir, with its
/// `deps/`): a dependent builds, links and runs with them gone.
#[test]
fn wit_prebuilt_dependency_carries_its_wit() {
    let Some(_env) = setup() else { return };
    let rt = workspace().join("target/lib/rok/targets/wasm32-wasi/librokr.a");
    if !rt.is_file() {
        eprintln!("skipped: no wasm32-wasi runtime (targets/build.sh wasm32-wasi)");
        return;
    }
    let dir = scratch("wit-prebuilt");
    put(
        dir.join("calc/wit/calc.wit"),
        "package test:calc;\n\nworld calc {\n    use test:num/types.{n};\n    \
         export add: func(a: n, b: n) -> n;\n}\n",
    )
    .unwrap();
    put(
        dir.join("calc/wit/deps/num.wit"),
        "package test:num;\n\ninterface types {\n    type n = s32;\n}\n",
    )
    .unwrap();
    put(
        dir.join("calc/src/lib.rok"),
        "ffi::wit::export { \"../wit/calc.wit\" }\n\n\
         #[wit::export]\npub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    put(
        dir.join("app/main.rok"),
        "fn main() { println(f\"{calc::add(2, 3)}\"); }\n",
    )
    .unwrap();
    let out = rokc(
        None,
        &dir,
        &["-q", "build", "calc", "--target", "wasm32-wasi"],
    );
    assert_exit(&out, 0);
    fs::remove_dir_all(dir.join("calc/src")).unwrap();
    fs::remove_dir_all(dir.join("calc/wit")).unwrap();
    let out = rokc(
        None,
        &dir,
        &[
            "-q",
            "build",
            "app/main.rok",
            "--target",
            "wasm32-wasi",
            "--extern",
            "calc=calc/target/wasm32-wasi/debug/calc.rokhir",
            "-o",
            "app.wasm",
        ],
    );
    assert_exit(&out, 0);
    let Some(runner) = std::env::var_os("ROK_TEST_WASM_RUNNER") else {
        return;
    };
    for (args, want) in [
        (&["run", "app.wasm"][..], "5\n"),
        (&["run", "--invoke", "add(4, 5)", "app.wasm"][..], "9\n"),
    ] {
        let run = Command::new(&runner)
            .current_dir(&dir)
            .args(args)
            .output()
            .unwrap();
        assert_exit(&run, 0);
        assert_eq!(text(&run.stdout), want);
    }
}

/// `rokc bindgen <args>` in `dir`, or `None` (said on stderr) when this
/// rokc finds no libclang; `ROK_REQUIRE_LIBCLANG=1` makes that a failure.
fn bindgen(dir: &Path, args: &[&str]) -> Option<Output> {
    let mut args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    if !args.iter().any(|a| a == "--target") {
        let dirs = host_c_includes();
        if !dirs.is_empty() && !args.iter().any(|a| a == "--") {
            args.push("--".into());
        }
        for d in dirs {
            args.extend(["-isystem".into(), d.to_string_lossy().into_owned()]);
        }
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = rokc(None, dir, &[&["bindgen"], &args[..]].concat());
    if text(&out.stderr).contains("needs libclang, which is missing") {
        let msg = "no libclang (llvm/build-libclang.sh)";
        assert!(
            std::env::var("ROK_REQUIRE_LIBCLANG").as_deref() != Ok("1"),
            "ROK_REQUIRE_LIBCLANG=1 but {msg}: {}",
            text(&out.stderr)
        );
        // Straight to the handle: libtest captures `eprintln!`.
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("\n!!!!!!!! SKIPPED: {msg}: rokc bindgen NOT tested !!!!!!!!\n").as_bytes(),
        );
        return None;
    }
    Some(out)
}

/// Linux: the host C compiler's include directories but those of its
/// built-in headers (libclang has its own), for the C library headers
/// libclang's FHS defaults miss (`NixOS` has no `/usr/include`).
fn host_c_includes() -> Vec<PathBuf> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let Ok(out) = Command::new("cc")
        .args(["-xc", "-E", "-v", "-"])
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    text(&out.stderr)
        .lines()
        .skip_while(|l| !l.starts_with("#include <...>"))
        .skip(1)
        .take_while(|l| l.starts_with(' '))
        .map(|l| PathBuf::from(l.trim()))
        .filter(|d| !d.join("stddef.h").is_file())
        .collect()
}

const BINDGEN_H: &str = r#"#include <stddef.h>
#include <stdint.h>
#include <stdio.h>

#define TINY_VERSION 3
#define TINY_NAME "tiny"
#define TINY_MASK (1u << 4)
#define TINY_BIG 0xffffffffffffffffULL
#define TINY_NEG (-2)
#define TINY_PI 3.25
#define TINY_SUM (TINY_VERSION + 4)
#define TINY_FN(x) ((x) + 1)
#define TINY_API

typedef int32_t tiny_id;
typedef struct tiny_point { int x; int y; } tiny_point;
typedef struct { double w, h; } tiny_size;
struct tiny_opaque;
typedef struct tiny_opaque tiny_handle;
enum tiny_color { TINY_RED, TINY_GREEN = 5, TINY_BLUE };
typedef enum { TINY_A = -1, TINY_B = 7 } tiny_mode;
enum { TINY_ANON_X = 100 };
union tiny_value { int i; float f; uint8_t bytes[8]; };
struct tiny_flags { unsigned a : 3; unsigned b : 5; int c; unsigned d : 1; };
struct tiny_nested { struct { int u, v; } inner; union { long long l; char c; }; int tail[]; };
#pragma pack(push, 1)
struct tiny_packed { char c; int i; };
#pragma pack(pop)
struct tiny_aligned { _Alignas(16) char c; };
typedef int (*tiny_cb)(int, void *);
typedef void tiny_fn_t(int);

extern int tiny_counter;
extern const char *const tiny_names[];

tiny_point tiny_add(tiny_point a, tiny_point b);
int tiny_apply(tiny_cb cb, void *data, int n);
size_t tiny_len(const char *s);
int tiny_sum(int n, ...);
tiny_handle *tiny_open(const char *path, FILE *log);
int tiny_close(tiny_handle *h);
void tiny_on(tiny_fn_t *f);
static inline int tiny_twice(int x) { return 2 * x; }
int match(int type, int fn);
enum tiny_color tiny_next(enum tiny_color c, tiny_mode m);
size_t tiny_size_of(int which);
"#;

const BINDGEN_C: &str = r#"#include "tiny.h"
#include <stdarg.h>
#include <string.h>

int tiny_counter = 41;
const char *const tiny_names[] = {"zero", "one"};

struct tiny_opaque { int fd; };
static struct tiny_opaque the_handle = {9};

tiny_point tiny_add(tiny_point a, tiny_point b) {
    tiny_point r = {a.x + b.x, a.y + b.y};
    return r;
}

int tiny_apply(tiny_cb cb, void *data, int n) { return cb ? cb(n, data) : -1; }

size_t tiny_len(const char *s) { return strlen(s); }

int tiny_sum(int n, ...) {
    va_list ap;
    int s = 0;
    va_start(ap, n);
    for (int i = 0; i < n; i++) s += va_arg(ap, int);
    va_end(ap);
    return s;
}

tiny_handle *tiny_open(const char *path, FILE *log) {
    (void)log;
    return path ? &the_handle : 0;
}

int tiny_close(tiny_handle *h) { return h->fd; }

void tiny_on(tiny_fn_t *f) { f(7); }

int match(int type, int fn) { return type * 10 + fn; }

enum tiny_color tiny_next(enum tiny_color c, tiny_mode m) { return m == TINY_B ? TINY_BLUE : c; }

size_t tiny_size_of(int which) {
    size_t sizes[] = {sizeof(struct tiny_flags), sizeof(struct tiny_nested),
                      sizeof(struct tiny_packed), sizeof(struct tiny_aligned),
                      sizeof(union tiny_value), sizeof(tiny_size)};
    return sizes[which];
}
"#;

const BINDGEN_ROK: &str = r#"mod tiny_sys;
use tiny_sys::*;
use rok::bare::ffi::CString;
use rok::bare::mem::size_of;
use rok::bare::ptr::CPtr;

extern "C" fn twice(n: i32, data: CPtr<u8>) -> i32 {
    n * 2 + (data.addr() as i32)
}

extern "C" fn seen(x: i32) {
    println(f"seen {x}");
}

fn main() {
    println(f"{TINY_VERSION} {TINY_NAME} {TINY_MASK} {TINY_BIG} {TINY_SUM} {TINY_NEG} {TINY_PI}");
    let p = unsafe { tiny_add(tiny_point { x: 1, y: 2 }, tiny_point { x: 10, y: 20 }) };
    println(f"add {p.x} {p.y}");
    println(f"apply {unsafe { tiny_apply(twice, CPtr::null(), 21) }}");
    let s = try! CString::from_str("hello");
    let path = CPtr::from_const(s.as_ptr());
    println(f"len {unsafe { tiny_len(path) }} sum {unsafe { tiny_sum(3, 1, 2, 3) }}");
    let h = unsafe { tiny_open(path, CPtr::null()) };
    println(f"open {h.is_null()} {unsafe { tiny_close(h) }}");
    unsafe { tiny_on(seen) };
    println(f"match {unsafe { match_(4, 2) }}");
    println(f"next {unsafe { tiny_next(TINY_RED, TINY_B) }} {TINY_GREEN} {TINY_A} {TINY_ANON_X}");
    println(f"counter {unsafe { tiny_counter }}");
    let rok = [size_of::<tiny_flags>(), size_of::<tiny_nested>(), size_of::<tiny_packed>(), size_of::<tiny_aligned>(), size_of::<tiny_value>(), size_of::<tiny_size>()];
    let same = 0;
    for i in 0..6 {
        if rok[i] == unsafe { tiny_size_of(i as i32) } {
            same += 1;
        }
    }
    println(f"sizes {same} of 6");
}
"#;

/// Bindings to a small C library: generated, compiled and run against
/// it, their layouts checked against clang's `sizeof`.
// spec: std §1.7.6
#[test]
fn bindgen_binds_a_c_library() {
    let Some(env) = setup() else { return };
    if common::test_target().is_some() {
        return;
    }
    let dir = scratch("bindgen");
    put(dir.join("tiny.h"), BINDGEN_H).unwrap();
    put(dir.join("tiny.c"), BINDGEN_C).unwrap();
    put(dir.join("app/src/main.rok"), BINDGEN_ROK).unwrap();
    let Some(out) = bindgen(&dir, &["tiny.h", "-o", "app/src/tiny_sys.rok"]) else {
        return;
    };
    assert_exit(&out, 0);
    let rok = fs::read_to_string(dir.join("app/src/tiny_sys.rok")).unwrap();
    for want in [
        "use rok::bare::ptr::CPtr;",
        "pub const TINY_NAME: str = \"tiny\";",
        "pub const TINY_SUM: i32 = 7;",
        "#[repr(C)]\npub struct tiny_point {\n    pub x: i32;\n    pub y: i32;\n}",
        "pub struct tiny_opaque;",
        "pub type tiny_handle = tiny_opaque;",
        "pub union tiny_value {",
        "pub _bitfield0: [u8; 4];",
        "#[repr(packed)]\npub struct tiny_packed {",
        "#[repr(C, align(16))]\npub struct tiny_aligned {",
        "pub _anon0: tiny_nested_anon0;",
        "pub tail: [i32; 0];",
        "pub type tiny_cb = unsafe extern \"C\" fn(i32, CPtr<u8>) -> i32 | none;",
        "// skipped fn `tiny_twice`: static or inline, no symbol to link",
        "pub static tiny_names: [CPtr<c_char>; 0];",
        "pub static mut tiny_counter: i32;",
        "pub fn tiny_sum(n: i32, ...) -> i32;",
        "pub fn tiny_on(f: unsafe extern \"C\" fn(i32) | none);",
        "#[link_name = \"match\"]\n    pub fn match_(type_: i32, fn_: i32) -> i32;",
        "pub fn tiny_len(s: CPtr<c_char>) -> usize;",
    ] {
        assert!(rok.contains(want), "missing {want:?} in:\n{rok}");
    }
    // `FILE` comes along from <stdio.h>, the rest of it does not.
    assert!(
        rok.contains("pub type FILE = ") || rok.contains("pub struct FILE"),
        "{rok}"
    );
    assert!(!rok.contains("fn printf"), "{rok}");

    // The library, as the platform's archive.
    let run_ok = |c: &mut Command| assert_exit(&c.current_dir(&dir).output().unwrap(), 0);
    run_ok(Command::new("clang").args(["-c", "tiny.c", "-o", "tiny.o"]));
    if cfg!(windows) {
        run_ok(Command::new("lib").args(["/nologo", "/OUT:tiny.lib", "tiny.o"]));
    } else {
        run_ok(Command::new("ar").args(["rcs", "libtiny.a", "tiny.o"]));
    }
    let lib_dir = dir.to_str().unwrap();
    assert_exit(
        &rokc(
            Some(&env),
            &dir,
            &["-q", "build", "app", "-L", lib_dir, "-l", "static=tiny"],
        ),
        0,
    );
    let run = Command::new(dir.join("app/target/debug").join(exe_name("app")))
        .output()
        .unwrap();
    assert_exit(&run, 0);
    assert_eq!(
        text(&run.stdout),
        "3 tiny 16 18446744073709551615 7 -2 3.25\nadd 11 22\napply 42\nlen 5 sum 6\n\
         open false 9\nseen 7\nmatch 42\nnext 6 5 -1 100\ncounter 41\n\
         sizes 6 of 6\n"
    );

    // A header clang rejects is its error, exit 1.
    put(dir.join("bad.h"), "int f(;\n").unwrap();
    let out = bindgen(&dir, &["bad.h"]).unwrap();
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("bad.h:1:7: error"),
        "{}",
        text(&out.stderr)
    );
}

const BINDGEN_UNSUPPORTED_H: &str = r"#include <stddef.h>
typedef int (*vcb)(const char *fmt, ...);
__int128 add128(__int128 a);
unsigned __int128 uadd128(unsigned __int128 a);
_Float16 half(_Float16 x);
long double ld(long double x);
struct wide { long double x; int y; };
struct inner { __int128 v; };
struct outer { struct inner i; int k; };
struct plain { int a; double b; };
void take_wide(struct wide w);
void take_wide_ptr(struct wide *w);
struct outer give_outer(void);
void take_plain(struct plain p, size_t n);
typedef void (*wide_cb)(struct wide);
struct has_vcb { vcb f; int n; };
int use_vcb(vcb f);
#define LD_ONE 1.0L
#define HALF_ONE 1.0f16
";

/// Types rok cannot express are skipped; a struct holding one is opaque
/// bytes. A cross target gets only clang's own headers.
// spec: std §1.7.6
#[test]
fn bindgen_skips_what_rok_cannot_express() {
    if common::test_target().is_some() {
        return;
    }
    let dir = scratch("bindgen_unsupported");
    put(dir.join("u.h"), BINDGEN_UNSUPPORTED_H).unwrap();
    put(dir.join("app/src/main.rok"), "mod u;\n\nfn main() {}\n").unwrap();
    let t = "x86_64-unknown-linux-gnu";
    let Some(out) = bindgen(&dir, &["u.h", "--target", t, "-o", "app/src/u.rok"]) else {
        return;
    };
    assert_exit(&out, 0);
    let b = fs::read_to_string(dir.join("app/src/u.rok")).unwrap();
    for want in [
        "// skipped typedef `vcb`: a variadic function pointer\n",
        "// skipped fn `add128`: type `__int128` has no rok form\n",
        "// skipped fn `uadd128`: type `unsigned __int128` has no rok form\n",
        "// skipped fn `half`: type `_Float16` has no rok form\n",
        "// skipped fn `ld`: type `long double` has no rok form\n",
        "// `wide` as bytes (field `x`: type `long double` has no rok form)\n\
         #[repr(C, align(16))]\npub struct wide {\n    pub _opaque: [u8; 32];\n}\n",
        "#[repr(C)]\npub struct outer {\n    pub i: inner;\n    pub k: i32;\n}\n",
        "// skipped fn `take_wide`: `wide` is passed by value but written as bytes (field `x`: \
         type `long double` has no rok form)\n",
        "// skipped fn `give_outer`: `inner` is passed by value but written as bytes (field `v`: \
         type `__int128` has no rok form)\n",
        "// skipped typedef `wide_cb`: `wide` is passed by value but written as bytes",
        "// `has_vcb` as bytes (field `f`: `vcb` is a variadic function pointer)\n",
        "// skipped fn `use_vcb`: `vcb` is a variadic function pointer\n",
        "    pub fn take_wide_ptr(w: CPtr<wide>);\n",
        "    pub fn take_plain(p: plain, n: usize);\n",
    ] {
        assert!(b.contains(want), "lacks {want:?}:\n{b}");
    }
    assert!(!b.contains("LD_ONE") && !b.contains("HALF_ONE"), "{b}");
    assert_exit(
        &rokc(None, &dir.join("app"), &["-q", "check", "--target", t]),
        0,
    );

    // wasm32-wasi is never this machine: its headers are given.
    put(dir.join("s.h"), "#include <stdio.h>\nint f(FILE *f);\n").unwrap();
    let out = bindgen(&dir, &["s.h", "--target", "wasm32-wasi"]).unwrap();
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains(
            "'stdio.h' file not found\nnote: wasm32-wasi is not this machine's platform, so its \
             system headers are not used; give the target's after `--` (`-isystem <dir>` or \
             `--sysroot <dir>`)"
        ),
        "{}",
        text(&out.stderr)
    );
    put(dir.join("inc/stdio.h"), "typedef struct _IO_FILE FILE;\n").unwrap();
    let inc = dir.join("inc");
    let args = [
        "s.h",
        "--target",
        "wasm32-wasi",
        "--",
        "-isystem",
        inc.to_str().unwrap(),
    ];
    let out = bindgen(&dir, &args).unwrap();
    assert_exit(&out, 0);
    assert!(
        text(&out.stdout).contains("    pub fn f(f: CPtr<FILE>) -> i32;\n"),
        "{}",
        text(&out.stdout)
    );
}

/// System headers bind (with what they include from their own tree)
/// into declarations rokc accepts.
// spec: std §1.7.6
#[test]
fn bindgen_binds_system_headers() {
    if common::test_target().is_some() {
        return;
    }
    // Where the host's C headers are.
    let mut dirs: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        dirs.extend(std::env::split_paths(
            &std::env::var_os("INCLUDE").unwrap_or_default(),
        ));
    } else if cfg!(target_os = "macos") {
        let sdk = Command::new("xcrun")
            .arg("--show-sdk-path")
            .output()
            .unwrap();
        dirs.push(PathBuf::from(text(&sdk.stdout).trim()).join("usr/include"));
    } else {
        dirs = host_c_includes();
    }
    let dir = scratch("bindgen_sys");
    let mut bound = 0;
    for h in [
        "stdio.h", "stdlib.h", "string.h", "math.h", "time.h", "zlib.h",
    ] {
        let Some(path) = dirs.iter().map(|d| d.join(h)).find(|p| p.is_file()) else {
            continue;
        };
        let pkg = dir.join(h.replace('.', "_"));
        put(pkg.join("src/main.rok"), "mod b;\n\nfn main() {}\n").unwrap();
        let out = pkg.join("src/b.rok");
        let args = [path.to_str().unwrap(), "-o", out.to_str().unwrap()];
        let Some(made) = bindgen(&dir, &args) else {
            return;
        };
        assert_exit(&made, 0);
        let b = fs::read_to_string(&out).unwrap();
        assert!(b.contains("extern \"C\" {"), "{h}:\n{b}");
        assert_exit(&rokc(None, &pkg, &["-q", "check"]), 0);
        bound += 1;
    }
    assert!(bound >= 3, "found only {bound} of the headers in {dirs:?}");
}

/// OpenSSL 3 for the TLS interop test (`ROK_OPENSSL`, `openssl` on
/// `PATH`, Homebrew's) and its minor version.
fn openssl3() -> Option<(PathBuf, u32)> {
    let mut cands: Vec<PathBuf> = std::env::var_os("ROK_OPENSSL")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    cands.push("openssl".into());
    cands.push("/opt/homebrew/opt/openssl@3/bin/openssl".into());
    cands.push("/usr/local/opt/openssl@3/bin/openssl".into());
    cands.into_iter().find_map(|p| {
        let out = Command::new(&p).arg("version").output().ok()?;
        let v = text(&out.stdout);
        let minor = v
            .strip_prefix("OpenSSL 3.")?
            .split('.')
            .next()?
            .parse()
            .ok()?;
        Some((p, minor))
    })
}

#[track_caller]
fn ossl(o: &Path, dir: &Path, args: &[&str]) {
    let out = Command::new(o)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "openssl {args:?}: {}",
        text(&out.stderr)
    );
}

/// A CA (P-256) and leaves under it: `name.pem` / `name.key`.
fn interop_certs(o: &Path, dir: &Path) {
    let ec = ["-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-256"];
    ossl(
        o,
        dir,
        &[&["genpkey"][..], &ec, &["-out", "ca.key"]].concat(),
    );
    ossl(
        o,
        dir,
        &[
            "req",
            "-x509",
            "-new",
            "-key",
            "ca.key",
            "-subj",
            "/CN=interop CA",
            "-days",
            "30",
            "-out",
            "ca.pem",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign",
        ],
    );
    let leaves: [(&str, &[&str], &str, &[&str]); 4] = [
        ("p256", &ec, "serverAuth", &["-days", "30"]),
        (
            "ed25519",
            &["-algorithm", "ED25519"],
            "serverAuth",
            &["-days", "30"],
        ),
        ("client", &ec, "clientAuth", &["-days", "30"]),
        (
            "expired",
            &ec,
            "serverAuth",
            &[
                "-not_before",
                "20200101000000Z",
                "-not_after",
                "20210101000000Z",
            ],
        ),
    ];
    for (name, alg, eku, validity) in leaves {
        let (key, csr, cert) = (
            format!("{name}.key"),
            format!("{name}.csr"),
            format!("{name}.pem"),
        );
        ossl(o, dir, &[&["genpkey"][..], alg, &["-out", &key]].concat());
        ossl(
            o,
            dir,
            &[
                "req",
                "-new",
                "-key",
                &key,
                "-subj",
                &format!("/CN={name}"),
                "-out",
                &csr,
            ],
        );
        let ext = format!(
            "basicConstraints=CA:FALSE\nsubjectAltName=DNS:localhost\nextendedKeyUsage={eku}\nkeyUsage=critical,digitalSignature\n"
        );
        put(dir.join("ext"), ext).unwrap();
        let x509 = [
            "x509", "-req", "-in", &csr, "-CA", "ca.pem", "-CAkey", "ca.key", "-extfile", "ext",
            "-out", &cert,
        ];
        ossl(o, dir, &[&x509[..], validity].concat());
    }
}

/// Our client against `openssl s_server -rev` with `server` arguments:
/// the client's report.
fn vs_s_server(o: &Path, probe: &Path, dir: &Path, server: &str, client: &str) -> String {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut srv = Command::new(o)
        .current_dir(dir)
        .args([
            "s_server",
            "-accept",
            &port.to_string(),
            "-tls1_3",
            "-rev",
            "-cert",
            "p256.pem",
            "-key",
            "p256.key",
        ])
        .args(server.split_whitespace())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(srv.stdout.take().unwrap()).lines();
    while let Some(Ok(l)) = lines.next() {
        if l.starts_with("ACCEPT") {
            break;
        }
    }
    std::thread::spawn(move || lines.for_each(drop));
    let out = Command::new(probe)
        .current_dir(dir)
        .args(["client", &port.to_string(), "localhost", "ca.pem"])
        .args(client.split_whitespace())
        .output()
        .unwrap();
    let _ = srv.kill();
    let _ = srv.wait();
    text(&out.stdout)
}

/// `openssl s_client` (`args`, once per `conns`, resuming after the
/// first) against our server: per connection what `s_client` saw, then
/// the server's report.
fn vs_s_client(
    o: &Path,
    probe: &Path,
    dir: &Path,
    server: &str,
    args: &str,
    conns: usize,
) -> (Vec<String>, String) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::Stdio;
    let mut srv = Command::new(probe)
        .current_dir(dir)
        .args(["server", "p256.pem", "p256.key", &format!("conns={conns}")])
        .args(server.split_whitespace())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(srv.stderr.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let port = first.trim().strip_prefix("listening ").unwrap().to_string();
    let mut seen = Vec::new();
    for i in 0..conns {
        let mut c = Command::new(o)
            .current_dir(dir)
            .args([
                "s_client",
                "-connect",
                &format!("127.0.0.1:{port}"),
                "-tls1_3",
                "-servername",
                "localhost",
            ])
            .args([
                "-CAfile",
                "ca.pem",
                "-verify_return_error",
                "-ign_eof",
                "-sess_out",
                "sess.pem",
            ])
            .args(if i > 0 {
                &["-sess_in", "sess.pem"][..]
            } else {
                &[]
            })
            .args(args.split_whitespace())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(b"ping\nbye\n").unwrap();
        let out = c.wait_with_output().unwrap();
        let all = text(&out.stdout) + &text(&out.stderr);
        let facts: Vec<&str> = [
            "echo:ping",
            "Reused,",
            "Negotiated TLS1.3 group: X25519MLKEM768",
        ]
        .into_iter()
        .filter(|k| all.contains(k))
        .collect();
        seen.push(format!(
            "exit {} {}",
            out.status.code().unwrap_or(-1),
            facts.join(" | ")
        ));
    }
    let mut report = String::new();
    srv.stdout
        .take()
        .unwrap()
        .read_to_string(&mut report)
        .unwrap();
    let _ = srv.wait();
    (seen, report)
}

/// `net::tls` against OpenSSL 3 on loopback, both directions. Skipped
/// without OpenSSL 3.
// spec: §3.2.5
#[test]
fn tls_interop_openssl() {
    let Some(env) = setup() else { return };
    let Some((o, minor)) = openssl3().filter(|_| common::test_target().is_none()) else {
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "\n!!!!!!!! SKIPPED tls_interop_openssl: no OpenSSL 3 (set ROK_OPENSSL) !!!!!!!!\n"
        );
        return;
    };
    if minor < 2 {
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "\n!!!!!!!! SKIPPED tls_interop_openssl: OpenSSL 3.2+ needed (x509 -not_before / -not_after); found 3.{minor} (set ROK_OPENSSL) !!!!!!!!\n"
        );
        return;
    }
    let hybrid = minor >= 5;
    let dir = scratch("tls_interop");
    interop_certs(&o, &dir);
    let probe = dir.join(exe_name("probe"));
    let src = workspace().join("compiler/rokc/tests/tls_interop.rok");
    assert_exit(&common::rokc_build(&env, &src, &probe).unwrap(), 0);

    let ok = "conn 0: resumed false early false reply gnip\n";
    let group = if hybrid { "X25519MLKEM768" } else { "X25519" };
    let cells: [(&str, String, &str); 6] = [
        (
            "aes128 x25519",
            "-ciphersuites TLS_AES_128_GCM_SHA256 -groups X25519".into(),
            "suite=aes128 pq=0",
        ),
        (
            "aes256",
            format!("-ciphersuites TLS_AES_256_GCM_SHA384 -groups {group}"),
            "suite=aes256",
        ),
        (
            "chacha20",
            "-ciphersuites TLS_CHACHA20_POLY1305_SHA256".into(),
            "suite=chacha",
        ),
        (
            "ed25519 server",
            "-cert ed25519.pem -key ed25519.key".into(),
            "",
        ),
        ("stateless HRR", "-stateless".into(), ""),
        (
            "client certificate",
            "-Verify 1 -CAfile ca.pem -verify_return_error".into(),
            "cert=client.pem key=client.key",
        ),
    ];
    for (what, server, client) in &cells {
        assert_eq!(vs_s_server(&o, &probe, &dir, server, client), ok, "{what}");
    }
    assert_eq!(
        vs_s_server(&o, &probe, &dir, "-naccept 2", "conns=2"),
        format!("{ok}conn 1: resumed true early false reply gnip\n"),
        "resumption"
    );
    assert_eq!(
        vs_s_server(&o, &probe, &dir, "-cert expired.pem -key expired.key", ""),
        "conn 0: error Certificate alert 45\n",
        "expired"
    );

    let echo = "exit 0 echo:ping";
    for suite in [
        "TLS_AES_128_GCM_SHA256",
        "TLS_AES_256_GCM_SHA384",
        "TLS_CHACHA20_POLY1305_SHA256",
    ] {
        let (seen, report) = vs_s_client(
            &o,
            &probe,
            &dir,
            "",
            &format!("-ciphersuites {suite} -groups X25519"),
            1,
        );
        assert_eq!(seen, [echo], "{suite}");
        assert_eq!(
            report, "srv 0: resumed false early  peer certs 0\n",
            "{suite}"
        );
    }
    if hybrid {
        // s_client's first share is X25519's; the server asks for the hybrid.
        let (seen, _) = vs_s_client(&o, &probe, &dir, "", "-groups X25519:X25519MLKEM768", 1);
        assert_eq!(
            seen,
            [format!("{echo} | Negotiated TLS1.3 group: X25519MLKEM768")],
            "HelloRetryRequest"
        );
    }
    let (seen, report) = vs_s_client(&o, &probe, &dir, "", "", 2);
    assert!(
        seen[1].starts_with(&format!("{echo} | Reused,")),
        "resumption {seen:?}"
    );
    assert!(
        report.ends_with("srv 1: resumed true early  peer certs 0\n"),
        "{report}"
    );
    let (seen, report) = vs_s_client(
        &o,
        &probe,
        &dir,
        "croots=ca.pem",
        "-cert client.pem -key client.key",
        1,
    );
    assert!(seen[0].starts_with(echo), "client certificate {seen:?}");
    // s_client may send the CA after the leaf.
    assert!(
        report.starts_with("srv 0: resumed false early  peer certs ")
            && !report.contains("certs 0"),
        "{report}"
    );
}

/// A compiler knob (9.1.7, 9.9.5) set to a value outside its accepted
/// ones warns once on stderr and keeps its default.
// spec: §9.1.7 §9.9.5
#[test]
fn env_knob_bad_value_warns() {
    let Some(env) = setup() else { return };
    let dir = scratch("env-knob");
    let lib = new_package(&dir, "knobs", true);
    let out = Command::new(ROKC)
        .current_dir(&lib)
        .env("ROK_HOME", workspace())
        .env("ROK_BACKEND", "gcc")
        .env("ROK_VERBOSE", "yes")
        .env("ROK_TIME_PASSES", "on")
        .env("ROK_INCREMENTAL_VERIFY", "true")
        .env("ROK_TEST_THREADS", "0")
        .args(["test", "-q", "--rokr"])
        .arg(&env.rokr)
        .output()
        .unwrap();
    assert_exit(&out, 0);
    let err = text(&out.stderr);
    let warnings: Vec<&str> = err
        .lines()
        .filter(|l| l.starts_with("rokc: warning: "))
        .collect();
    assert_eq!(
        warnings,
        [
            "rokc: warning: ignoring ROK_BACKEND=gcc (expected native|llvm)",
            "rokc: warning: ignoring ROK_VERBOSE=yes (expected 0|1)",
            "rokc: warning: ignoring ROK_TIME_PASSES=on (expected 0|1)",
            "rokc: warning: ignoring ROK_INCREMENTAL_VERIFY=true (expected 0|1)",
            "rokc: warning: ignoring ROK_TEST_THREADS=0 (expected a positive number)",
        ]
    );
}

/// The old `ROKC_*` names still work and warn once, naming the new one;
/// the new name, when set too, wins without a warning. `test` (as `run`
/// and `bench`) warns about the runtime's old names.
// spec: §9.1.7 §9.9.8
#[test]
fn old_env_names_warn() {
    let Some(env) = setup() else { return };
    let dir = scratch("env-old");
    let lib = new_package(&dir, "knobs", true);
    let run = |vars: &[(&str, &Path)]| {
        let mut c = Command::new(ROKC);
        c.current_dir(&lib).env("ROK_HOME", workspace());
        for (new, old) in rokc_core::RENAMED_VARS
            .iter()
            .chain(rokc_core::RUNTIME_VARS)
        {
            c.env_remove(new).env_remove(old);
        }
        c.envs(vars.iter().copied())
            .args(["test", "-q", "--rokr"])
            .arg(&env.rokr)
            .output()
            .unwrap()
    };
    let warnings = |out: &Output| {
        let mut w: Vec<String> = text(&out.stderr)
            .lines()
            .filter(|l| l.starts_with("rokc: warning: "))
            .map(str::to_owned)
            .collect();
        w.sort();
        w
    };
    let (old, new) = (dir.join("old"), dir.join("new"));
    let out = run(&[
        ("ROKC_TARGET_DIR", &old),
        ("ROKC_NUM_JOBS", Path::new("2")),
        ("ROK_WORKERS", Path::new("2")),
    ]);
    assert_exit(&out, 0);
    assert_eq!(
        warnings(&out),
        [
            "rokc: warning: ROKC_NUM_JOBS is now ROK_TEST_THREADS: set ROK_TEST_THREADS instead \
             (ROKC_NUM_JOBS goes in 0.2.0)",
            "rokc: warning: ROKC_TARGET_DIR is now ROK_TARGET_DIR: set ROK_TARGET_DIR instead \
             (ROKC_TARGET_DIR goes in 0.2.0)",
            "rokc: warning: ROK_WORKERS is now ROKR_WORKERS: set ROKR_WORKERS instead (programs \
             read ROK_WORKERS until 0.2.0)",
        ]
    );
    assert!(old.join("debug").is_dir());
    let out = run(&[
        ("ROK_TARGET_DIR", &new),
        ("ROKC_TARGET_DIR", &old),
        ("ROKR_WORKERS", Path::new("2")),
        ("ROK_WORKERS", Path::new("2")),
    ]);
    assert_exit(&out, 0);
    assert!(warnings(&out).is_empty(), "{}", text(&out.stderr));
    assert!(new.join("debug").is_dir());
}

/// `--out` still names the output, with a warning that says `-o`; `-o`
/// and `--output` do not warn.
// spec: §9.1.2
#[test]
fn out_flag_warns() {
    let dir = scratch("out-flag");
    put(dir.join("src/main.rok"), "fn main() {}\n").unwrap();
    let out = rokc(None, &dir, &["emit", "--emit=hir", "--out", "a.hir"]);
    assert_exit(&out, 0);
    assert!(
        text(&out.stderr).contains(
            "rokc: warning: `--out` is now `-o` or `--output` (write `-o a.hir`); `--out` goes \
             in 0.2.0"
        ),
        "{}",
        text(&out.stderr)
    );
    assert!(dir.join("a.hir").is_file());
    for f in ["-o", "--output"] {
        let out = rokc(None, &dir, &["emit", "--emit=hir", f, "b.hir"]);
        assert_exit(&out, 0);
        assert!(
            !text(&out.stderr).contains("warning"),
            "{}",
            text(&out.stderr)
        );
    }
}

/// `--emit=source` and `--emit=expansions` are the new names of `ast`
/// and `expanded`, which still work and warn.
// spec: §9.1.2 §9.9.10
#[test]
fn emit_new_names() {
    let dir = scratch("emit-names");
    put(
        dir.join("src/main.rok"),
        "#[derive(Clone)]\nstruct P { x: i32; }\nfn main() {}\n",
    )
    .unwrap();
    for (new, old) in [("source", "ast"), ("expansions", "expanded")] {
        let out = rokc(
            None,
            &dir,
            &["emit", &format!("--emit={new}"), "-o", "n.rok"],
        );
        assert_exit(&out, 0);
        assert!(
            !text(&out.stderr).contains("warning"),
            "{}",
            text(&out.stderr)
        );
        let out = rokc(
            None,
            &dir,
            &["emit", &format!("--emit={old}"), "-o", "o.rok"],
        );
        assert_exit(&out, 0);
        assert!(
            text(&out.stderr).contains(&format!(
                "rokc: warning: `--emit={old}` is now `--emit={new}`; `{old}` goes in 0.2.0"
            )),
            "{}",
            text(&out.stderr)
        );
        let read = |f: &str| fs::read_to_string(dir.join(f)).unwrap();
        assert_eq!(read("n.rok"), read("o.rok"), "{new}");
    }
}

/// The ecosystem's target names are aliases of the old ones, which warn.
// spec: §0.6.2 §9.1.4
#[test]
fn new_target_aliases() {
    let dir = scratch("target-alias");
    for (old, new) in rokc_target::RENAMED {
        let spec = |t: &str| rokc(None, &dir, &["--print", "target-spec", "--target", t]);
        let (a, b) = (spec(new), spec(old));
        assert_exit(&a, 0);
        assert_exit(&b, 0);
        assert_eq!(text(&a.stdout), text(&b.stdout), "{new}");
        assert!(!text(&a.stderr).contains("warning"), "{}", text(&a.stderr));
        assert_eq!(
            text(&b.stderr),
            format!(
                "rokc: warning: target `{old}` is now `{new}` (write `--target {new}`); `{old}` \
                 goes in 0.2.0\n"
            )
        );
    }
}

/// `--print target-list` lists each new name, and the old one after it
/// marked deprecated.
// spec: §9.1.4
#[test]
fn target_list_marks_deprecated() {
    let dir = scratch("target-list-old");
    let out = rokc(None, &dir, &["--print", "target-list"]);
    assert_exit(&out, 0);
    let list = text(&out.stdout);
    for (old, new) in rokc_target::RENAMED {
        assert!(
            list.contains(&format!("\n{new}\n{old} (deprecated: use {new})\n")),
            "{list}"
        );
    }
}

/// A macOS program links against the default SDK (no SDKROOT) and against
/// every installed one: the macOS 27 SDK's .tbd files list arm64e.x1.
// spec: §9.2.15
#[test]
fn links_against_every_installed_macos_sdk() {
    if !cfg!(target_os = "macos") || common::test_target().is_some() {
        return;
    }
    let Some(env) = setup() else { return };
    let dir = scratch("macos_sdks");
    put(dir.join("h.rok"), "fn main() { println(\"hi\"); }\n").unwrap();
    let mut sdks: Vec<Option<PathBuf>> = vec![None];
    for d in [
        "/Library/Developer/CommandLineTools/SDKs",
        "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs",
    ] {
        let Ok(rd) = fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("MacOSX")
                && !p.is_symlink()
                && p.join("usr/lib/libSystem.tbd").is_file()
            {
                sdks.push(Some(p));
            }
        }
    }
    for sdk in &sdks {
        let exe = dir.join("h");
        let mut cmd = Command::new(ROKC);
        cmd.current_dir(&dir).env("ROK_HOME", workspace());
        match sdk {
            Some(p) => cmd.env("SDKROOT", p),
            None => cmd.env_remove("SDKROOT"),
        };
        let out = cmd
            .args(["-q", "build", "h.rok", "-o"])
            .arg(&exe)
            .arg("--rokr")
            .arg(&env.rokr)
            .output()
            .unwrap();
        assert!(out.status.success(), "{sdk:?}: {}", text(&out.stderr));
        let run = Command::new(&exe).output().unwrap();
        assert_eq!(text(&run.stdout), "hi\n", "{sdk:?}");
    }
}
