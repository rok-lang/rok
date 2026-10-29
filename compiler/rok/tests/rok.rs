//! `rok` end to end: scratch `ROK_HOME`s, local git repositories made
//! with the `git` CLI, and the `rokc` built next to `rok`. Tests that
//! compile and run native code skip when rokc is built without LLVM
//! (unless `ROK_REQUIRE_LLVM=1`).

// `HEAD^{tree}` is git revision syntax, not a format argument.
#![allow(clippy::literal_string_with_formatting_args)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

const ROK: &str = env!("CARGO_BIN_EXE_rok");

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `rok` runs the `rokc` beside it (and tests that link want the
/// runtime): build both once, unless `ROK_TEST_PREBUILT` says they are
/// (rokc's tests/common/mod.rs).
fn build_rokc() {
    static BUILT: OnceLock<bool> = OnceLock::new();
    let ok = *BUILT.get_or_init(|| {
        if std::env::var_os("ROK_TEST_PREBUILT").is_some() {
            return true;
        }
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        Command::new(cargo)
            .args(["build", "-p", "rokc", "-p", "rokr"])
            .current_dir(workspace())
            .status()
            .unwrap()
            .success()
    });
    assert!(ok, "cargo build -p rokc -p rokr failed");
}

fn llvm() -> bool {
    build_rokc();
    // `rokc version -v` names the LLVM it links.
    let found = Command::new(Path::new(ROK).with_file_name("rokc"))
        .args(["version", "-v"])
        .output()
        .is_ok_and(|o| !String::from_utf8_lossy(&o.stdout).contains("llvm: none"));
    if !found {
        assert!(
            std::env::var("ROK_REQUIRE_LLVM").as_deref() != Ok("1"),
            "ROK_REQUIRE_LLVM=1 but rokc is built without LLVM"
        );
    }
    found
}

/// The executable file `name` builds to.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// `s` as a ROKON string literal (Windows paths have backslashes).
fn q(s: impl std::fmt::Display) -> String {
    rokc_rokon::quote(&s.to_string())
}

fn scratch(name: &str) -> PathBuf {
    rokc_core::tmp::test_dir(name)
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn rok_cmd(home: &Path, cwd: &Path, args: &[&str]) -> Command {
    build_rokc();
    let mut c = Command::new(ROK);
    c.current_dir(cwd)
        .env("ROK_HOME", home)
        .env_remove("ROK_ENV")
        .env_remove("ROK_REGISTRY")
        .env_remove("ROK_REGISTRY_TOKEN")
        .env_remove("CLICOLOR_FORCE")
        .args(args);
    if cfg!(unix) {
        user_env(&mut c, &home.join("user"));
    }
    // Stones' https `origin` (see `origin`) fetches from the scratch tree.
    let root = rokc_core::tmp::test_root().display().to_string();
    c.env("GIT_CONFIG_COUNT", "1")
        .env(
            "GIT_CONFIG_KEY_0",
            format!("url.{}/.insteadOf", root.replace('\\', "/")),
        )
        .env("GIT_CONFIG_VALUE_0", ORIGIN);
    c
}

/// A user whose home is `user`, with no XDG directories set: the
/// completion files `rok toolchain` refreshes stay in the test tree.
fn user_env(c: &mut Command, user: &Path) {
    c.env("HOME", user)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME");
}

/// Where `origin` points stones; `rok_cmd` maps it to the test root.
const ORIGIN: &str = "https://example.invalid/";

/// The https URL `rok publish` takes for the repository at `dir`.
fn origin(dir: &Path) -> String {
    let rel = dir.strip_prefix(rokc_core::tmp::test_root()).unwrap();
    format!("{ORIGIN}{}", rel.display().to_string().replace('\\', "/"))
}

fn rok(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    rok_cmd(home, cwd, args).output().unwrap()
}

#[track_caller]
fn ok(out: &Output) -> String {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        text(&out.stdout),
        text(&out.stderr)
    );
    text(&out.stdout)
}

#[track_caller]
fn fails(out: &Output, want: &str) {
    let err = text(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "unexpected success: {err}");
    assert!(err.contains(want), "stderr lacks {want:?}:\n{err}");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
    text(&out.stdout).trim().to_owned()
}

/// A git repository at `dir` holding `files`; the new commit.
fn commit(dir: &Path, files: &[(&str, &str)]) -> String {
    if !dir.join(".git").exists() {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
    }
    for (name, body) in files {
        write(&dir.join(name), body);
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "c"]);
    git(dir, &["rev-parse", "HEAD"])
}

/// `name=path` lines of `rok deps`.
fn deps(home: &Path, cwd: &Path, extra: &[&str]) -> Vec<(String, String)> {
    let mut args = extra.to_vec();
    args.push("deps");
    ok(&rok(home, cwd, &args))
        .lines()
        .map(|l| {
            let (n, p) = l.split_once('=').unwrap();
            (n.to_owned(), p.to_owned())
        })
        .collect()
}

const LIB_V1: &str = "pub fn greet() -> i32 { return 1; }\n";
const LIB_V2: &str = "pub fn greet() -> i32 { return 2; }\n";

/// A manifest with comments, which `add` / `remove` keep.
const APP0: &str = "// my app\nPackage {\n    name: \"app\", // keep\n    deps: {\n        // deps go here\n    },\n}\n";

// spec: §9.10.1 §9.10.3 §9.10.4
#[test]
fn add_lock_update_offline_remove() {
    let root = scratch("lock");
    let home = root.join("home");
    let repo = root.join("greet");
    let c1 = commit(&repo, &[("src/lib.rok", LIB_V1)]);
    let url = repo.display().to_string();
    let qurl = q(&url);
    let app = root.join("app");
    write(
        &app.join("src/main.rok"),
        "use greet::greet;\nprintln(f\"{greet()}\");\n",
    );
    write(&app.join("rok.rokon"), APP0);

    ok(&rok(&home, &app, &["add", "greet", "--git", &url]));
    let manifest = read(&app.join("rok.rokon"));
    assert_eq!(
        manifest,
        APP0.replace(
            "    },",
            &format!("        \"greet\": Dep::git({qurl}),\n    }},")
        )
    );
    let lock = read(&app.join("rok.lock"));
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    assert!(
        lock.starts_with(
            "// rok.lock: generated by `rok`; do not edit. Commit it with your sources.\n\
             Lock {\n    version: 1,\n    packages: [\n        Locked {\n            \
             name: \"greet\",\n"
        ),
        "{lock}"
    );
    for want in [
        qurl.clone(),
        format!("commit: \"{c1}\",\n            tree: \"{tree}\",\n        }},\n    ],\n}}\n"),
    ] {
        assert!(lock.contains(&want), "{lock}");
    }
    let checkout = home.join("cache/git/checkouts").join(&c1);
    let slashes = |s: String| s.replace('\\', "/");
    assert_eq!(
        slashes(deps(&home, &app, &[])[0].1.clone()),
        slashes(checkout.join("src/lib.rok").display().to_string())
    );

    // A new commit upstream: builds keep the locked one; `update` moves.
    let c2 = commit(&repo, &[("src/lib.rok", LIB_V2)]);
    assert!(deps(&home, &app, &["--locked"])[0].1.contains(&c1));
    ok(&rok(&home, &app, &["update", "greet"]));
    assert!(read(&app.join("rok.lock")).contains(&c2));
    assert!(deps(&home, &app, &[])[0].1.contains(&c2));
    fails(&rok(&home, &app, &["update", "nope"]), "not a dependency");

    // Offline: a modified or missing checkout comes back from the cached
    // clone; nothing is fetched.
    let c2_dir = home.join("cache/git/checkouts").join(&c2);
    write(&c2_dir.join("src/lib.rok"), "tampered");
    let out = rok(&home, &app, &["--offline", "deps"]);
    ok(&out);
    assert!(
        text(&out.stderr).contains("was modified"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(read(&c2_dir.join("src/lib.rok")), LIB_V2);
    fs::remove_dir_all(&c2_dir).unwrap();
    ok(&rok(&home, &app, &["--offline", "deps"]));
    // `update --offline` re-resolves against the cached clone.
    ok(&rok(&home, &app, &["--offline", "update"]));
    // A fresh ROK_HOME has no cache: offline fails, online fetches the
    // locked commit.
    let home2 = root.join("home2");
    fails(&rok(&home2, &app, &["--offline", "deps"]), "--offline");
    assert!(deps(&home2, &app, &[])[0].1.contains(&c2));

    // --locked refuses to change the lock.
    let other = "\n        \"other\": Dep::git(\"/nope\"),";
    write(
        &app.join("rok.rokon"),
        &read(&app.join("rok.rokon")).replace("deps: {", &format!("deps: {{{other}")),
    );
    fails(&rok(&home, &app, &["--locked", "deps"]), "--locked");
    write(
        &app.join("rok.rokon"),
        &read(&app.join("rok.rokon")).replace(other, ""),
    );

    // A tag; `remove` edits the file and prunes the lock.
    git(&repo, &["tag", "v1", &c1]);
    ok(&rok(
        &home,
        &app,
        &["add", "greet", "--git", &url, "--tag", "v1"],
    ));
    assert!(
        read(&app.join("rok.rokon"))
            .contains(&format!("\"greet\": Dep::git({qurl}, tag: \"v1\"),"))
    );
    assert!(read(&app.join("rok.lock")).contains("tag: \"v1\""));
    assert!(read(&app.join("rok.lock")).contains(&format!("commit: \"{c1}\"")));
    ok(&rok(&home, &app, &["remove", "greet"]));
    assert_eq!(read(&app.join("rok.rokon")), APP0);
    assert!(!read(&app.join("rok.lock")).contains("Locked"));
    fails(&rok(&home, &app, &["remove", "greet"]), "not a dependency");

    // A bad source leaves rok.rokon as it was.
    let before = read(&app.join("rok.rokon"));
    fails(
        &rok(
            &home,
            &app,
            &[
                "add",
                "x",
                "--git",
                &root.join("missing").display().to_string(),
            ],
        ),
        "cannot fetch",
    );
    assert_eq!(read(&app.join("rok.rokon")), before);
    fails(
        &rok(&home, &app, &["add", "My.X", "--path", "."]),
        "try `my-x`",
    );
}

// spec: §9.10.3 §9.10.4
#[test]
fn transitive_dependencies_and_conflicts() {
    let root = scratch("transitive");
    let home = root.join("home");
    let b = root.join("b");
    commit(&b, &[("src/lib.rok", "pub fn b() -> i32 { return 2; }\n")]);
    let a = root.join("a");
    commit(
        &a,
        &[
            ("src/lib.rok", "pub fn a() -> i32 { return 1; }\n"),
            (
                "rok.rokon",
                &format!(
                    "Package {{ name: \"a\", deps: {{ \"b\": Dep::git({}) }} }}\n",
                    q(b.display())
                ),
            ),
        ],
    );
    let app = root.join("app");
    write(&app.join("src/main.rok"), "println(\"x\");\n");
    ok(&rok(
        &home,
        &app,
        &["add", "a", "--git", &a.display().to_string()],
    ));
    let names: Vec<String> = deps(&home, &app, &[]).into_iter().map(|(n, _)| n).collect();
    // Each package after its own dependencies.
    assert_eq!(names, ["b", "a"]);
    let lock = read(&app.join("rok.lock"));
    assert!(
        lock.contains("name: \"a\"") && lock.contains("name: \"b\""),
        "{lock}"
    );

    // `b` from a path as well: two packages named `b`.
    let other_b = root.join("other_b");
    write(
        &other_b.join("src/lib.rok"),
        "pub fn b() -> i32 { return 3; }\n",
    );
    let before = read(&app.join("rok.rokon"));
    fails(
        &rok(&home, &app, &["add", "b", "--path", "../other_b"]),
        "conflicting requirements for package `b`",
    );
    assert_eq!(read(&app.join("rok.rokon")), before);

    // Path dependencies are written relative to the project; a
    // non-library is refused.
    write(&root.join("tool/src/main.rok"), "println(\"t\");\n");
    fails(
        &rok(&home, &app, &["add", "tool", "--path", "../tool"]),
        "is an executable package (src/main.rok, no src/lib.rok): a dependency must be a library",
    );
    ok(&rok(
        &home,
        &app,
        &["add", "c", "--path", "../other_b", "--dev"],
    ));
    // On one line or one entry per line, by the other entries' widths
    // (the scratch path's).
    let manifest = read(&app.join("rok.rokon")).replace(['\n', ' ', ','], "");
    assert!(
        manifest.contains("dev_deps:{\"c\":Dep::path(\"../other_b\")}"),
        "{manifest}"
    );
    // Dev dependencies are for tests only.
    assert_eq!(deps(&home, &app, &[]).len(), 2);
    assert_eq!(ok(&rok(&home, &app, &["deps", "--dev"])).lines().count(), 3);
}

// spec: §0.3.4 §9.10.3
#[test]
fn manifest_errors_and_reserved_forms() {
    let root = scratch("manifest");
    let home = root.join("home");
    let app = root.join("app");
    write(&app.join("src/main.rok"), "println(\"x\");\n");
    write(
        &app.join("rok.rokon"),
        "Package {\n    deps: { \"foo\": \"1.0.x.y\" },\n}\n",
    );
    let out = rok(&home, &app, &["deps"]);
    fails(&out, "is not a version requirement");
    assert!(
        text(&out.stderr).contains("rok.rokon:2:20"),
        "{}",
        text(&out.stderr)
    );
    write(
        &app.join("rok.rokon"),
        "Package { deps: { \"foo\": Dep::native(\"z\") } }\n",
    );
    fails(
        &rok(&home, &app, &["deps"]),
        "a native package goes in the `native` field",
    );
    write(
        &app.join("rok.rokon"),
        "Package {\n    name: env(\"X\"),\n}\n",
    );
    let out = rok(&home, &app, &["deps"]);
    fails(&out, "error[E002]: calls are not allowed in ROKON");
    fails(&out, "rok.rokon:2:11\n");
    write(&app.join("rok.rokon"), "Package { deps: {\n");
    fails(&rok(&home, &app, &["deps"]), "rok.rokon:2:1\n");
    write(&app.join("rok.rokon"), "Package {}\n");
    ok(&rok(&home, &app, &["deps"]));
    fs::remove_file(app.join("rok.rokon")).unwrap();

    // A wrongly cased manifest is ignored with a warning.
    write(&app.join("Rok.rokon"), "Package { deps: { \"x\": 1 } }\n");
    let out = rok(&home, &app, &["deps"]);
    ok(&out);
    assert!(
        text(&out.stderr).contains("the manifest is named `rok.rokon` (lowercase)"),
        "{}",
        text(&out.stderr)
    );
}

// spec: §9.10.3
#[test]
fn manifest_errors_say_what_to_write() {
    let root = scratch("manifest-help");
    let home = root.join("home");
    let app = root.join("app");
    write(&app.join("src/main.rok"), "println(\"x\");\n");
    let example = "   = help: a `rok.rokon` holds one `Package { .. }`: `Package { name: \"app\", \
                   version: \"0.1.0\", deps: {} }`";
    // TOML, JSON.
    write(&app.join("rok.rokon"), "name = \"app\"\n");
    let out = rok(&home, &app, &["deps"]);
    fails(
        &out,
        "error[E002]: expected the end of the file: a ROKON file holds one value",
    );
    fails(&out, example);
    write(&app.join("rok.rokon"), "{\"name\": \"app\"}\n");
    let out = rok(&home, &app, &["deps"]);
    fails(&out, "error[E200]: expected `Package { .. }`, found a dict");
    fails(&out, example);
    // A path where a version requirement goes.
    write(
        &app.join("rok.rokon"),
        "Package { deps: { \"geom\": \"../geom\" } }\n",
    );
    let out = rok(&home, &app, &["deps"]);
    fails(
        &out,
        "error[E200]: `../geom` is a path, not a version requirement",
    );
    fails(
        &out,
        "   = help: a path dependency is `Dep::path(\"../geom\")`",
    );
    // A field without its `,`.
    write(
        &app.join("rok.rokon"),
        "Package { name: \"app\" version: \"0.1.0\" }\n",
    );
    let out = rok(&home, &app, &["deps"]);
    fails(
        &out,
        "error[E002]: expected `,` or `}`, found field `version`",
    );
    fails(
        &out,
        "   = help: separate the fields with `,`: `name: \"app\", version: ..`",
    );
    // A path dependency that is not there.
    write(
        &app.join("rok.rokon"),
        "Package { deps: { \"geom\": Dep::path(\"../nope\") } }\n",
    );
    fails(
        &rok(&home, &app, &["deps"]),
        "dependency `geom`: no directory `../nope` (",
    );
    fails(
        &rok(&home, &app, &["deps"]),
        "); a `Dep::path` is relative to the directory of the `rok.rokon` that names it",
    );
}

// spec: §9.10.2
#[test]
fn new_and_init_write_a_manifest() {
    let root = scratch("new");
    let home = root.join("home");
    ok(&rok(&home, &root, &["new", "--lib", "mylib"]));
    assert!(read(&root.join("mylib/src/lib.rok")).contains("#[test]"));
    assert!(!root.join("mylib/lib.rok").exists());
    ok(&rok(&home, &root, &["new", "mybin"]));
    assert_eq!(
        read(&root.join("mybin/src/main.rok")),
        "println(\"Hello, world!\");\n"
    );
    let mut files: Vec<String> = fs::read_dir(root.join("mybin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files, [".git", ".gitignore", "rok.rokon", "src"]);
    assert_eq!(
        read(&root.join("mylib/rok.rokon")),
        "Package { name: \"mylib\", version: \"0.1.0\", deps: {} }\n"
    );
    // Its own git repository, ignoring target/.
    assert_eq!(read(&root.join("mylib/.gitignore")), "target/\n");
    assert!(root.join("mylib/.git").is_dir());
    // Package names: `[a-z][a-z0-9_-]*`.
    fails(&rok(&home, &root, &["new", "My App"]), "try `my-app`");
    assert!(!root.join("My App").exists());
    // The standard library's name is taken.
    fails(
        &rok(&home, &root, &["new", "--lib", "rok"]),
        "`rok` is reserved for the standard library; name the package something else, e.g. `rok_util`",
    );
    assert!(!root.join("rok").exists());
    let dir = root.join("Existing.Dir");
    fs::create_dir_all(&dir).unwrap();
    // Inside a repository: no new one.
    git(&dir, &["init", "-q"]);
    let out = rok(&home, &dir, &["init"]);
    ok(&out);
    assert!(text(&out.stderr).contains("the package name is `existing-dir`"));
    assert!(dir.join("src/main.rok").is_file());
    assert!(read(&dir.join("rok.rokon")).contains("name: \"existing-dir\""));
    fs::create_dir_all(dir.join("sub")).unwrap();
    ok(&rok(&home, &dir.join("sub"), &["new", "inner"]));
    assert!(!dir.join("sub/inner/.git").exists());
    fails(&rok(&home, &dir, &["init"]), "already exists");
    let version = ok(&rok(&home, &dir, &["--version"]));
    assert!(
        version.starts_with(&format!("rok {}", env!("CARGO_PKG_VERSION"))),
        "{version}"
    );
    // Usage errors exit 2 (spec 9.1.6); the rest 1.
    let usage = |args: &[&str]| rok(&home, &root, args).status.code();
    assert_eq!(usage(&["new"]), Some(2));
    assert_eq!(usage(&["add", "x", "--frob"]), Some(2));
    assert_eq!(usage(&["frob"]), Some(2));
    assert_eq!(usage(&["toolchain", "frob"]), Some(2));
    // A command on a package, outside one: rok says so.
    let out = rok(&home, &root, &["check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).starts_with("rok: error: not inside a package"),
        "{}",
        text(&out.stderr)
    );
    // Per-command help.
    let help = ok(&rok(&home, &root, &["add", "--help"]));
    assert!(help.contains("add <name> --path <dir>") && !help.contains("publish"));
    assert_eq!(ok(&rok(&home, &root, &["help", "add"])), help);
    let help = ok(&rok(&home, &root, &["test", "--help"]));
    assert!(
        help.contains("--threads") && !help.contains("--iters"),
        "{help}"
    );
}

/// `rok args` in `cwd` with `TERM=xterm` (16 colours) and `vars`; its
/// stderr.
fn stderr_with(home: &Path, cwd: &Path, vars: &[(&str, &str)], args: &[&str]) -> String {
    let out = rok_cmd(home, cwd, args)
        .env_remove("NO_COLOR")
        .env_remove("COLORTERM")
        .env("TERM", "xterm")
        .envs(vars.iter().copied())
        .output()
        .unwrap();
    text(&out.stderr)
}

// spec: §9.1.8 §9.10.1
#[test]
fn status_lines_coloured_when_forced() {
    let root = scratch("colour-forced");
    let home = root.join("home");
    let err = stderr_with(&home, &root, &[("CLICOLOR_FORCE", "1")], &["new", "a"]);
    assert!(err.starts_with("\x1b[1;96m     Created\x1b[0m "), "{err:?}");
    let err = stderr_with(&home, &root, &[], &["--color=always", "new", "b"]);
    assert!(err.starts_with("\x1b[1;96m     Created\x1b[0m "), "{err:?}");
    let vars = [("CLICOLOR_FORCE", "1"), ("TERM", "xterm-256color")];
    let err = stderr_with(&home, &root, &vars, &["new", "c"]);
    assert!(
        err.starts_with("\x1b[1;38;5;81m     Created\x1b[0m "),
        "{err:?}"
    );
    let vars = [("CLICOLOR_FORCE", "1"), ("COLORTERM", "24bit")];
    let err = stderr_with(&home, &root, &vars, &["new", "d"]);
    assert!(
        err.starts_with("\x1b[1;38;2;56;189;248m     Created\x1b[0m "),
        "{err:?}"
    );
    // Labels are coloured, and there without colour.
    let err = stderr_with(&home, &root, &[("CLICOLOR_FORCE", "1")], &["check"]);
    assert!(
        err.starts_with("rok: \x1b[1;91merror:\x1b[0m not inside a package"),
        "{err:?}"
    );
}

// spec: §9.1.8 §9.10.1
#[test]
fn no_colour_when_piped_or_no_color() {
    let root = scratch("colour-off");
    let home = root.join("home");
    let force = ("CLICOLOR_FORCE", "1");
    for (i, (vars, args)) in [
        (&[][..], &[][..]),
        (&[("NO_COLOR", "1"), force][..], &[][..]),
        (&[("TERM", "dumb")][..], &[][..]),
        (&[force][..], &["--color=never"][..]),
        (&[force][..], &["--no-color"][..]),
        (&[force][..], &["--color", "never"][..]),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("p{i}");
        let args = [args, &["new", &name]].concat();
        let err = stderr_with(&home, &root, vars, &args);
        assert!(err.starts_with("     Created "), "{args:?}: {err:?}");
        assert!(!err.contains('\x1b'), "{vars:?} {args:?}: {err:?}");
    }
    ok(&rok(&home, &root, &["env", "create", "e1"]));
    ok(&rok(&home, &root, &["env", "use", "e1"]));
    let list = rok_cmd(&home, &root, &["env", "list"])
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm")
        .env_remove("NO_COLOR")
        .env_remove("COLORTERM")
        .output()
        .unwrap();
    assert_eq!(text(&list.stdout), "\x1b[1;96m*\x1b[0m e1 (active)\n");
    let out = rok(&home, &root, &["--color=sometimes", "new", "x"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("`--color` takes auto|always|never, not `sometimes`"),
        "{}",
        text(&out.stderr)
    );
}

// spec: §9.1.5 §9.10.1
#[test]
fn rok_passes_color_to_rokc() {
    let root = scratch("colour-rokc");
    let home = root.join("home");
    ok(&rok(&home, &root, &["new", "app"]));
    let app = root.join("app");
    write(&app.join("src/main.rok"), "let x: i32 = nope;\n");
    for args in [
        &["--color=always", "check"][..],
        &["check", "--color", "always"],
    ] {
        let err = stderr_with(&home, &app, &[], args);
        assert!(
            err.contains("\x1b[1;91merror[E100]\x1b[0m"),
            "{args:?}: {err:?}"
        );
    }
    let err = stderr_with(
        &home,
        &app,
        &[("CLICOLOR_FORCE", "1")],
        &["check", "--no-color"],
    );
    assert!(
        err.contains("error[E100]") && !err.contains('\x1b'),
        "{err:?}"
    );
}

// spec: §9.10.4 / std §3.9.4
#[test]
fn rok_add_writes_canonical_rokon() {
    let root = scratch("canon");
    let home = root.join("home");
    write(&root.join("u/src/lib.rok"), LIB_V1);
    ok(&rok(&home, &root, &["new", "app"]));
    let app = root.join("app");
    ok(&rok(&home, &app, &["add", "u", "--path", "../u"]));
    // Fits in 100 columns: one line.
    assert_eq!(
        read(&app.join("rok.rokon")),
        "Package { name: \"app\", version: \"0.1.0\", deps: { \"u\": Dep::path(\"../u\") } }\n"
    );
    ok(&rok(
        &home,
        &app,
        &["add", "a_rather_long_dependency_name", "--path", "../u"],
    ));
    assert_eq!(
        read(&app.join("rok.rokon")),
        "Package {\n    name: \"app\",\n    version: \"0.1.0\",\n    deps: { \"u\": \
         Dep::path(\"../u\"), \"a_rather_long_dependency_name\": Dep::path(\"../u\") },\n}\n"
    );
    // A file with comments keeps its layout.
    let mine = "// mine\nPackage { name: \"app\", deps: { \"u\": Dep::path(\"../u\") } }\n";
    write(&app.join("rok.rokon"), mine);
    ok(&rok(&home, &app, &["remove", "u"]));
    assert!(read(&app.join("rok.rokon")).starts_with("// mine\nPackage { name: \"app\""));
    // The manifest's name is the package's.
    write(
        &app.join("rok.rokon"),
        "Package { name: \"hello\", deps: {} }\n",
    );
    let out = rok(&home, &app, &["build"]);
    if llvm() {
        ok(&out);
        assert!(app.join("target/debug").join(exe("hello")).is_file());
    }
    write(&app.join("rok.rokon"), "Package { name: \"My_App\" }\n");
    fails(&rok(&home, &app, &["check"]), "try `my_app`");

    // `my-lib` is a package name; code says `my_lib`. `-` and `_` are one
    // name, so both spellings cannot be dependencies.
    ok(&rok(&home, &root, &["new", "--lib", "my-lib"]));
    assert!(read(&root.join("my-lib/rok.rokon")).contains("name: \"my-lib\""));
    write(&app.join("rok.rokon"), "Package { name: \"my-app\" }\n");
    write(
        &app.join("src/main.rok"),
        "use my_lib::add;\nprintln(f\"{add(1, 2)}\");\n",
    );
    ok(&rok(&home, &app, &["add", "my-lib", "--path", "../my-lib"]));
    ok(&rok(&home, &app, &["-q", "check"]));
    assert!(ok(&rok(&home, &app, &["deps"])).starts_with("my_lib="));
    fails(
        &rok(&home, &app, &["add", "my_lib", "--path", "../my-lib"]),
        "are the same package",
    );
    if llvm() {
        assert_eq!(ok(&rok(&home, &app, &["-q", "run"])), "3\n");
        assert!(app.join("target/debug").join(exe("my-app")).is_file());
    }
}

// spec: §9.10.1
#[test]
fn lint_and_doc_with_dependencies() {
    let root = scratch("lintdoc");
    let home = root.join("home");
    write(
        &root.join("util/src/lib.rok"),
        "/// Says hi.\npub fn hello() -> i32 { let unused = 1; 1 }\n",
    );
    let app = root.join("app");
    write(
        &app.join("rok.rokon"),
        "Package {\n    name: \"app\",\n    deps: { \"util\": Dep::path(\"../util\") },\n    \
         lints: { \"unused_variables\": \"deny\" },\n}\n",
    );
    write(
        &app.join("src/main.rok"),
        "use util::hello;\nlet x = hello();\nprintln(\"hi\");\n",
    );
    // `lints` sets the level, the command line overrides it, and the
    // dependency is not linted.
    let out = rok(&home, &app, &["lint"]);
    fails(&out, "unused variable `x`");
    assert!(
        !text(&out.stderr).contains("`unused`"),
        "{}",
        text(&out.stderr)
    );
    let out = rok(&home, &app, &["lint", "-W", "unused_variables"]);
    ok(&out);
    assert!(text(&out.stderr).contains("warning[E503]"));
    fails(
        &rok(&home, &app, &["lint", "-D", "all"]),
        "unused variable `x`",
    );

    ok(&rok(&home, &app, &["-q", "doc"]));
    assert!(read(&app.join("target/doc/util/index.html")).contains("Says hi."));
    assert!(read(&app.join("target/doc/index.html")).contains("app/index.html"));
}

/// `rok build` lints against the prebuilt std, whose `using` cleanup is
/// `File.drop__by_Drop`: that is a close, not a use that leaks.
// spec: §6.2.9
#[test]
fn build_lints_using_through_prebuilt_std() {
    if !llvm() {
        return;
    }
    let root = scratch("usinglint");
    let home = root.join("home");
    let app = root.join("app");
    write(&app.join("rok.rokon"), "Package {\n    name: \"app\",\n}\n");
    write(
        &app.join("src/main.rok"),
        "use rok::std::fs::File;\n\n\
         fn leaks(p: str) -> str {\n    let f = try! File::open(p);\n    try! f.read_to_str()\n}\n\n\
         fn scoped(p: str) -> str {\n    using h = try! File::open(p) { try! h.read_to_str() }\n}\n\n\
         fn main() {\n    println(leaks(\"rok.rokon\"));\n    println(scoped(\"rok.rokon\"));\n}\n",
    );
    let out = rok(&home, &app, &["build"]);
    ok(&out);
    let err = text(&out.stderr);
    assert!(
        err.contains("`f` holds a `File` that is never closed"),
        "{err}"
    );
    assert!(!err.contains("`h` holds"), "{err}");
}

// spec: §6.2.9 §9.10.1 §9.10.3
#[test]
fn lints_from_and_files_in_a_package() {
    let root = scratch("lintsfrom");
    let home = root.join("home");
    let base = "Package {\n    lints: { \"unused_variables\": \"deny\", \"non_snake_case\": \"deny\" },\n}\n";
    write(&root.join("base/rok.rokon"), base);
    write(
        &root.join("mid/rok.rokon"),
        "Package {\n    lints_from: \"../base\",\n    lints: { \"non_snake_case\": \"allow\" },\n}\n",
    );
    let app = root.join("app");
    write(
        &app.join("rok.rokon"),
        "Package { lints_from: \"../mid\" }\n",
    );
    write(
        &app.join("src/main.rok"),
        "fn BadName() -> i32 { 1 }\nlet x = BadName();\nprintln(\"hi\");\n",
    );
    // base's deny reaches app through mid; mid's allow overrides base's.
    let out = rok(&home, &app, &["lint"]);
    fails(&out, "unused variable `x`");
    assert!(
        !text(&out.stderr).contains("snake"),
        "{}",
        text(&out.stderr)
    );
    // A file names the package it is in (rokc compiles that package), and
    // a lone file in a directory with only rok.rokon is linted alone: both
    // use the enclosing manifest's lints, from inside or outside.
    write(&root.join("mid/t.rok"), "let y = 1;\nprintln(\"t\");\n");
    for (cwd, path, want) in [
        (&app, "src/main.rok", "unused variable `x`"),
        (&root, "app/src/main.rok", "unused variable `x`"),
        (&root.join("mid"), "t.rok", "unused variable `y`"),
        (&root, "mid/t.rok", "unused variable `y`"),
    ] {
        for cmd in ["lint", "check"] {
            fails(&rok(&home, cwd, &[cmd, path]), want);
        }
    }
    // This manifest's own entry wins over the inherited one.
    write(
        &app.join("rok.rokon"),
        "Package {\n    lints_from: \"../mid\",\n    lints: { \"unused_variables\": \"warn\" },\n}\n",
    );
    let out = rok(&home, &app, &["lint"]);
    ok(&out);
    assert!(text(&out.stderr).contains("unused variable `x`"));
    // An imported environment keeps pointing at the same directory.
    ok(&rok(&home, &root, &["env", "import", "shared", "app"]));
    let env = read(&home.join("envs/shared/rok.rokon"));
    assert!(
        env.contains(&format!(
            "lints_from: {:?}",
            root.join("mid").display().to_string()
        )),
        "{env}"
    );

    // Errors: a cycle, a missing directory, a directory without a manifest.
    write(
        &root.join("base/rok.rokon"),
        "Package { lints_from: \"../app\" }\n",
    );
    fails(&rok(&home, &app, &["lint"]), "cycle:");
    write(&root.join("base/rok.rokon"), base);
    write(
        &app.join("rok.rokon"),
        "Package { lints_from: \"../nope\" }\n",
    );
    fails(&rok(&home, &app, &["lint"]), "is not a directory");
    fs::create_dir_all(root.join("empty")).unwrap();
    write(
        &app.join("rok.rokon"),
        "Package { lints_from: \"../empty\" }\n",
    );
    fails(&rok(&home, &app, &["lint"]), "no rok.rokon in");
}

// spec: §9.8.3 §9.10.5
#[test]
fn named_environments() {
    let root = scratch("envs");
    let home = root.join("home");
    let lib = root.join("lib1");
    write(&lib.join("src/lib.rok"), LIB_V1);
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();

    ok(&rok(&home, &root, &["env", "create", "work"]));
    // `rok --env work jupyter install`: a kernelspec that runs the
    // kernel in that environment.
    let jupyter = root.join("jupyter");
    ok(
        &rok_cmd(&home, &root, &["--env", "work", "jupyter", "install"])
            .env("JUPYTER_DATA_DIR", &jupyter)
            .output()
            .unwrap(),
    );
    let spec = read(&jupyter.join("kernels/rok-work/kernel.json"));
    assert!(
        spec.contains(r#""--env","work","jupyter","--connection-file","{connection_file}"],"display_name":"rok (work)""#),
        "{spec}"
    );
    // A kernel started from a GUI still finds the tools.
    assert!(spec.contains(r#""env":{"ROK_HOME":"#) && spec.contains(r#""PATH":"#));
    for logo in ["logo-32x32.png", "logo-64x64.png"] {
        let png = fs::read(jupyter.join("kernels/rok-work").join(logo)).unwrap();
        assert!(png.starts_with(b"\x89PNG"), "{logo}");
    }
    fails(
        &rok(&home, &root, &["env", "create", "work"]),
        "already exists",
    );
    ok(&rok(
        &home,
        &root,
        &["install", "--env", "work", "greet", "--path", "lib1"],
    ));
    // `rok install` is the one way into an environment.
    fails(
        &rok(
            &home,
            &root,
            &["add", "--env", "work", "x", "--path", "lib1"],
        ),
        "`rok add` edits a package's rok.rokon or a script's block; for the libraries the REPL \
         / Jupyter see in environment `work`, run `rok install --env work <name>`",
    );
    fails(
        &rok(&home, &elsewhere, &["add", "x", "--path", "../lib1"]),
        "for a script, name it: `rok add <name> <file.rok>`",
    );
    // In an environment, path dependencies are absolute.
    let manifest = read(&home.join("envs/work/rok.rokon"));
    let want = q(lib.display().to_string().replace('\\', "/"));
    assert!(
        manifest.contains(&format!("\"greet\": Dep::path({want}),")),
        "{manifest}"
    );
    assert_eq!(deps(&home, &elsewhere, &["--env", "work"])[0].0, "greet");
    // Outside a project: `default` (empty), then what `env use` chose,
    // and $ROK_ENV over both.
    assert!(deps(&home, &elsewhere, &[]).is_empty());
    ok(&rok(&home, &root, &["env", "use", "work"]));
    assert_eq!(deps(&home, &elsewhere, &[])[0].0, "greet");
    ok(&rok(
        &home,
        &root,
        &["install", "--env", "default", "d", "--path", "lib1"],
    ));
    let out = rok_cmd(&home, &elsewhere, &["deps"])
        .env("ROK_ENV", "default")
        .output()
        .unwrap();
    assert_eq!(ok(&out).split('=').next(), Some("d"));
    let list = ok(&rok(&home, &root, &["env", "list"]));
    assert_eq!(list, "  default\n* work (active)\n");

    // Export, import elsewhere.
    let export = root.join("export");
    ok(&rok(&home, &root, &["env", "export", "work", "export"]));
    assert!(export.join("rok.rokon").is_file());
    ok(&rok(&home, &root, &["env", "import", "copy", "export"]));
    assert_eq!(deps(&home, &elsewhere, &["--env", "copy"])[0].0, "greet");
    // Import from a project: relative paths become absolute.
    let proj = root.join("proj");
    write(&proj.join("src/main.rok"), "println(\"p\");\n");
    write(
        &proj.join("rok.rokon"),
        "Package {\n    deps: {\n        \"g\": Dep::path(\"../lib1\"), // here\n    },\n}\n",
    );
    ok(&rok(&home, &root, &["env", "import", "fromproj", "proj"]));
    assert!(
        read(&home.join("envs/fromproj/rok.rokon"))
            .contains(&format!("\"g\": Dep::path({}), // here", q(lib.display())))
    );

    // `env shell`: $SHELL with ROK_ENV and rok on PATH.
    let mut child = rok_cmd(&home, &root, &["env", "shell", "work"])
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Windows: %COMSPEC%, whatever SHELL says.
    let script: &[u8] = if cfg!(windows) {
        b"echo env=%ROK_ENV%\r\nwhere rok\r\n"
    } else {
        b"echo \"env=$ROK_ENV\"; command -v rok\n"
    };
    child.stdin.take().unwrap().write_all(script).unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = ok(&out);
    assert!(stdout.contains("env=work"), "{stdout}");
    assert!(stdout.contains(ROK), "{stdout}");

    ok(&rok(&home, &root, &["env", "remove", "work"]));
    fails(
        &rok(&home, &root, &["env", "remove", "work"]),
        "no environment",
    );
    fails(
        &rok(&home, &elsewhere, &["--env", "work", "deps"]),
        "no environment `work`",
    );
    // The choice went with it: back to `default`.
    assert_eq!(deps(&home, &elsewhere, &[])[0].0, "d");
}

/// A fake toolchain `version` in `dir`: the real `rok`, and a `rokc`
/// that says which toolchain it is and how it was called.
#[cfg(unix)]
fn fake_toolchain(dir: &Path, version: &str) {
    use std::os::unix::fs::PermissionsExt;
    let host = env!("ROK_HOST");
    write(
        &dir.join("bin/rokc"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'rokc {version} (fake)'; echo \
             'host: {host}'; exit 0; fi\necho \"rokc {version}: $*\"\n"
        ),
    );
    fs::set_permissions(dir.join("bin/rokc"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::copy(ROK, dir.join("bin/rok")).unwrap();
    write(&dir.join("bin/rokls"), "");
    write(&dir.join("lib/librokr.a"), "");
    write(&dir.join("lib/rok/core/src/lib.rok"), "");
}

// spec: §9.2.19 §9.10.6
#[cfg(unix)]
#[test]
fn pinned_toolchains() {
    let root = scratch("toolchain");
    let home = root.join("home");
    let app = root.join("app");
    let host = env!("ROK_HOST");
    write(&app.join("src/main.rok"), "println(\"x\");\n");
    write(&app.join("rok.rokon"), "Package { toolchain: \"9.9\" }\n");
    fails(&rok(&home, &app, &["build"]), "rok toolchain install 9.9");

    fake_toolchain(&root.join("fake1"), "9.9.1");
    fake_toolchain(&root.join("fake2/rok-9.10.0"), "9.10.0");
    let tc = |args: &[&str]| rok(&home, &root, &[&["toolchain"][..], args].concat());
    assert_eq!(tc(&["install", "9.9.1"]).status.code(), Some(2));
    fails(&tc(&["install", "9.9.1", "--from", "app"]), "no bin/rokc");
    // The version is the toolchain's own (`bin/rokc --version`).
    fails(
        &tc(&["install", "9.11", "--from", "fake1"]),
        "is rokc 9.9.1, not 9.11",
    );
    ok(&tc(&["install", "9.9.1", "--from", "fake1"]));
    // A tarball with one top directory.
    let tar = Command::new("tar")
        .current_dir(root.join("fake2"))
        .args(["-czf", "../tc.tar.gz", "rok-9.10.0"])
        .status()
        .unwrap();
    assert!(tar.success());
    ok(&tc(&["install", "9.10", "--from", "tc.tar.gz"]));
    // An archive is checked against the `.sha256` beside it, a download
    // against the one at its URL (which must exist).
    let sum = root.join("tc.tar.gz.sha256");
    write(&sum, &format!("{}  tc.tar.gz\n", "0".repeat(64)));
    fails(
        &tc(&["install", "9.10", "--from", "tc.tar.gz"]),
        "its .sha256 says 0000",
    );
    let good = format!("{}  tc.tar.gz\n", sha256_of(&root.join("tc.tar.gz")));
    write(&sum, &good);
    ok(&tc(&["install", "9.10", "--from", "tc.tar.gz"]));
    let url = format!("http://127.0.0.1:{}/tc.tar.gz", serve_dir(root.clone()));
    ok(&tc(&["install", "9.10", "--from", &url]));
    write(&sum, &format!("{}\n", "f".repeat(64)));
    fails(
        &tc(&["install", "9.10", "--from", &url]),
        "its .sha256 says ffff",
    );
    fs::remove_file(&sum).unwrap();
    fails(
        &tc(&["install", "9.10", "--from", &url]),
        "tc.tar.gz.sha256",
    );
    write(&sum, &good);
    assert!(
        home.join(format!("toolchains/9.10.0-{host}/bin/rokc"))
            .is_file()
    );
    // The first one became the default, with the proxies.
    assert_eq!(
        ok(&tc(&["list"])),
        format!("* 9.9.1-{host} (default)\n  9.10.0-{host}\n")
    );
    assert!(home.join("bin/rok").is_file() && home.join("bin/rokc").is_file());

    // The proxies (no ROK_HOME: theirs is where they are). Precedence:
    // `+<version>` > ROK_TOOLCHAIN > rok.rokon > the default.
    let proxy = |tool: &str, cwd: &Path, env: Option<&str>, args: &[&str]| {
        let mut c = Command::new(home.join("bin").join(tool));
        c.current_dir(cwd).env_remove("ROK_HOME").args(args);
        match env {
            Some(t) => c.env("ROK_TOOLCHAIN", t),
            None => c.env_remove("ROK_TOOLCHAIN"),
        };
        ok(&c.output().unwrap())
    };
    assert_eq!(proxy("rokc", &root, None, &["x"]), "rokc 9.9.1: x\n");
    let out = rok(&home, &root, &["toolchain", "default", "9.10"]);
    ok(&out);
    assert_eq!(proxy("rokc", &root, None, &["x"]), "rokc 9.10.0: x\n");
    assert_eq!(proxy("rokc", &app, None, &["x"]), "rokc 9.9.1: x\n");
    assert_eq!(
        proxy("rokc", &app, Some("9.10"), &["x"]),
        "rokc 9.10.0: x\n"
    );
    assert_eq!(
        proxy("rokc", &app, Some("9.10"), &["+9.9", "x"]),
        "rokc 9.9.1: x\n"
    );
    // The proxy `rok` runs the toolchain's `rok`, which runs its pin's
    // `rokc` with the arguments for it.
    assert_eq!(
        proxy("rok", &app, None, &["-q", "build", "--release"]),
        "rokc 9.9.1: -q build --release\n"
    );
    assert_eq!(
        proxy("rok", &root, None, &["+9.9", "-q", "fmt"]),
        "rokc 9.9.1: -q fmt\n"
    );
    // `rok toolchain` through the proxy is the proxy's own.
    assert_eq!(
        proxy("rok", &root, None, &["toolchain", "default"]),
        "9.10\n"
    );
    // An unknown pin names where it came from.
    write(&app.join("rok.rokon"), "Package { toolchain: \"9.8\" }\n");
    let out = Command::new(home.join("bin/rokc"))
        .current_dir(&app)
        .env_remove("ROK_TOOLCHAIN")
        .output()
        .unwrap();
    fails(&out, "toolchain 9.8 (pinned by ");
    // A field the proxy does not know (a newer rok's) keeps the pin.
    write(
        &app.join("rok.rokon"),
        "Package { toolchain: \"9.9\", from_a_newer_rok: true }\n",
    );
    assert_eq!(proxy("rokc", &app, None, &["x"]), "rokc 9.9.1: x\n");

    ok(&tc(&["remove", "9.9.1"]));
    fails(&tc(&["remove", "9.9.1"]), "is not installed");
    assert_eq!(ok(&tc(&["list"])), format!("* 9.10.0-{host} (default)\n"));
    // Pinning this rok's own version runs the rokc beside it.
    let mine: String = env!("CARGO_PKG_VERSION")
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".");
    write(
        &app.join("rok.rokon"),
        &format!("Package {{ toolchain: \"{mine}\" }}\n"),
    );
    ok(&rok(&home, &app, &["-q", "check"]));
}

/// `rok toolchain default` rewrites the completion scripts the
/// installers wrote from the new default, never makes one, and says
/// which under `-v`; a toolchain without `completions` leaves them.
// spec: §9.10.6
#[cfg(unix)]
#[test]
fn toolchain_default_refreshes_completions() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("toolchain-completions");
    let home = root.join("home");
    let user = home.join("user");
    fake_toolchain(&root.join("fake1"), "9.9.1");
    fake_toolchain(&root.join("fake2"), "9.10.0");
    fake_toolchain(&root.join("fake3"), "9.11.0");
    // Too old for `completions`: both tools refuse it.
    for t in ["rok", "rokc"] {
        let exe = root.join("fake3/bin").join(t);
        let old = fs::read(&exe).unwrap();
        let real = root.join("fake3/bin").join(format!("{t}.real"));
        fs::write(&real, old).unwrap();
        write(
            &exe,
            &format!(
                "#!/bin/sh\n[ \"$1\" = completions ] && exit 2\nexec \"{}\" \"$@\"\n",
                real.display()
            ),
        );
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let tc = |args: &[&str]| rok(&home, &root, &[&["toolchain"][..], args].concat());
    for (v, dir) in [("9.9.1", "fake1"), ("9.10.0", "fake2"), ("9.11.0", "fake3")] {
        ok(&tc(&["install", v, "--from", dir]));
    }
    let bash = user.join(".local/share/bash-completion/completions/rok");
    let zsh = home.join("zsh/_rokc");
    let ps1 = home.join("completions/rok.ps1");
    for f in [&bash, &zsh, &ps1] {
        write(f, "old\n");
    }
    let absent = [
        user.join(".local/share/bash-completion/completions/rokc"),
        home.join("zsh/_rok"),
        user.join(".config/fish/completions/rok.fish"),
        user.join(".config/fish/completions/rokc.fish"),
    ];

    let out = tc(&["default", "9.10"]);
    ok(&out);
    assert!(
        !text(&out.stderr).contains("note:"),
        "{}",
        text(&out.stderr)
    );
    assert!(read(&bash).contains("complete -o bashdefault -o default -F _rok rok"));
    assert_eq!(read(&zsh), "rokc 9.10.0: completions zsh\n");
    let rok_ps1 = ok(&rok(&home, &root, &["completions", "powershell"]));
    assert_eq!(
        read(&ps1),
        format!("{rok_ps1}rokc 9.10.0: completions powershell\n")
    );
    for f in &absent {
        assert!(!f.exists(), "{}", f.display());
    }

    let out = tc(&["default", "9.9", "-v"]);
    ok(&out);
    let err = text(&out.stderr);
    for want in [
        "        note: refreshed bash completions for rok in ~/.local/share/bash-completion/completions/rok\n".to_owned(),
        format!("        note: refreshed zsh completions for rokc in {}\n", zsh.display()),
        format!(
            "        note: refreshed powershell completions for rok and rokc in {}\n",
            ps1.display()
        ),
    ] {
        assert!(err.contains(&want), "lacks {want:?}:\n{err}");
    }
    assert_eq!(err.matches("note:").count(), 3, "{err}");
    assert_eq!(read(&zsh), "rokc 9.9.1: completions zsh\n");

    // `-q` wins over `-v`.
    let out = rok(&home, &root, &["-q", "-v", "toolchain", "default", "9.10"]);
    assert_eq!(text(&out.stderr), "");
    assert_eq!(read(&zsh), "rokc 9.10.0: completions zsh\n");

    let before: Vec<String> = [&bash, &zsh, &ps1].iter().map(|f| read(f)).collect();
    let out = rok(&home, &root, &["-v", "toolchain", "default", "9.11"]);
    ok(&out);
    assert!(
        !text(&out.stderr).contains("note:"),
        "{}",
        text(&out.stderr)
    );
    let after: Vec<String> = [&bash, &zsh, &ps1].iter().map(|f| read(f)).collect();
    assert_eq!(before, after);
    for f in &absent {
        assert!(!f.exists(), "{}", f.display());
    }
}

// spec: §9.10.7
#[test]
fn script_dependencies() {
    let root = scratch("script");
    let home = root.join("home");
    write(&root.join("lib1/src/lib.rok"), LIB_V1);
    let script = "//! ```rokon\n//! Package {\n//!     deps: { \"greet\": Dep::path(\"lib1\") },\n//! }\n//! ```\n\
                  use greet::greet;\nprintln(f\"{greet()}\");\n";
    write(&root.join("s.rok"), script);
    ok(&rok(&home, &root, &["-q", "check", "s.rok"]));
    // Its lock lives in the cache, not next to the script.
    assert!(!root.join("rok.lock").exists());
    assert!(home.join("cache/scripts").is_dir());
    write(
        &root.join("plain.rok"),
        "use greet::greet;\nprintln(f\"{greet()}\");\n",
    );
    assert_eq!(
        rok(&home, &root, &["-q", "check", "plain.rok"])
            .status
            .code(),
        Some(1)
    );
    write(
        &root.join("bad.rok"),
        "//! ```rokon\n//! Package {\n//!     deps: { \"g\": 1 },\n//! }\n//! ```\n",
    );
    fails(
        &rok(&home, &root, &["check", "bad.rok"]),
        "bad.rok (//! rokon block):2:18",
    );

    if llvm() {
        let out = rok(&home, &root, &["-q", "run", "s.rok"]);
        assert_eq!(ok(&out), "1\n");
        // A dependency's change is a new build of the script.
        write(&root.join("lib1/src/lib.rok"), LIB_V2);
        let out = rok(&home, &root, &["-q", "run", "s.rok"]);
        assert_eq!(ok(&out), "2\n");
    }
}

const ECHO: &str = "#!/usr/bin/env rok\nuse rok::std::env;\nuse rok::std::io;\n\
                    let line = try! io::stdin().read_line();\n\
                    println(f\"{env::args()[1..]:?} {line.trim()}\");\n\
                    eprintln(\"err\");\nenv::exit(3);\n";

/// The cached executable `rok run -v` ran, and whether it compiled.
fn ran(out: &Output) -> (String, bool) {
    let err = text(&out.stderr);
    let exe = err
        .lines()
        .find_map(|l| l.trim().strip_prefix("Running `"))
        .unwrap_or_else(|| panic!("no Running line: {err}"));
    (
        exe.trim_end_matches('`').to_owned(),
        err.contains("Compiling"),
    )
}

// spec: §1.2.3 §9.10.1 §9.10.7
#[test]
fn scripts_run_directly() {
    if !llvm() {
        return;
    }
    let root = scratch("direct");
    let home = root.join("home");
    write(&root.join("echo.rok"), ECHO);
    // `rok <file> args..`: every argument after the file is the
    // program's, stdin / stdout / stderr are its own, and so is the
    // exit code.
    for (file, args) in [("echo.rok", ["-q", "--x"]), ("./echo.rok", ["--", "a b"])] {
        let mut child = rok_cmd(&home, &root, &[file, args[0], args[1]])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"in\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
        assert_eq!(
            text(&out.stdout),
            format!("[{:?}, {:?}] in\n", args[0], args[1])
        );
        assert_eq!(text(&out.stderr), "err\n");
    }
    // A file named like a command is a command unless it is a path.
    write(&root.join("version"), "println(\"no\");\n");
    assert!(ok(&rok(&home, &root, &["version"])).starts_with("rok "));
    // Compile errors are reported as by `rok run`.
    write(&root.join("bad.rok"), "let x: i32 = \"s\";\n");
    fails(&rok(&home, &root, &["bad.rok"]), "error[E");
    // Through the shebang line.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file = root.join("echo.rok");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        let bin = Path::new(ROK).parent().unwrap();
        let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let out = Command::new(&file)
            .arg("z")
            .current_dir(&root)
            .env("PATH", path)
            .env("ROK_HOME", &home)
            .env_remove("ROK_ENV")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
        assert_eq!(text(&out.stdout), "[\"z\"] \n");
    }
}

// spec: §9.10.7
#[test]
fn script_executables_are_cached_by_content() {
    if !llvm() {
        return;
    }
    let root = scratch("script-cache");
    let home = root.join("home");
    let file = root.join("t.rok");
    write(&file, "println(\"one\");\n");
    let run = |args: &[&str]| {
        let mut all = vec!["run", "-v", "t.rok"];
        all.splice(1..1, args.iter().copied());
        let out = rok(&home, &root, &all);
        ok(&out);
        (text(&out.stdout), ran(&out))
    };
    let (out, (one, compiled)) = run(&[]);
    assert_eq!((out.as_str(), compiled), ("one\n", true));
    let entry = Path::new(&one).parent().unwrap().to_path_buf();
    // A script's own directory, in the cache (`target/<profile>/scripts/`).
    let scripts = entry.parent().unwrap();
    assert!(scripts.starts_with(home.join("cache/scripts")), "{one}");
    assert!(scripts.ends_with("target/debug/scripts"), "{one}");
    // The second run compiles nothing.
    assert_eq!(run(&[]), ("one\n".to_owned(), (one.clone(), false)));
    // An edit is a new key; the old entry stays and serves the old text.
    write(&file, "println(\"two\");\n");
    let (out, (two, compiled)) = run(&[]);
    assert_eq!((out.as_str(), compiled), ("two\n", true));
    assert_ne!(one, two);
    write(&file, "println(\"one\");\n");
    assert_eq!(run(&[]), ("one\n".to_owned(), (one, false)));
    // Options before the file, and their own key.
    let (out, (rel, compiled)) = run(&["--release"]);
    assert_eq!((out.as_str(), compiled), ("one\n", true));
    assert!(rel.contains("release"), "{rel}");
    // Unused for a day, an entry goes when another is built.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 3600);
    fs::File::options()
        .write(true)
        .open(entry.join("used"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    write(&file, "println(\"three\");\n");
    assert_eq!(run(&[]).0, "three\n");
    assert!(!entry.exists());
}

// spec: §9.10.7
#[test]
fn concurrent_script_runs_share_one_entry() {
    if !llvm() {
        return;
    }
    let root = scratch("script-race");
    let home = root.join("home");
    write(&root.join("t.rok"), "println(\"ok\");\n");
    // The script's cache directory first: the runs race on the script.
    ok(&rok(&home, &root, &["-q", "check", "t.rok"]));
    let runs: Vec<_> = (0..6)
        .map(|_| {
            rok_cmd(&home, &root, &["t.rok"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for r in runs {
        let out = r.wait_with_output().unwrap();
        assert_eq!(ok(&out), "ok\n", "{}", text(&out.stderr));
    }
    let cache: Vec<_> = fs::read_dir(home.join("cache/scripts"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(cache.len(), 1);
    let scripts = cache[0].path().join("target/debug/scripts");
    let entries: Vec<_> = fs::read_dir(&scripts).unwrap().flatten().collect();
    assert_eq!(entries.len(), 1);
    let left: Vec<String> = fs::read_dir(entries[0].path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.ends_with(".dSYM"))
        .collect();
    let mut left = left;
    left.sort();
    // Windows: the PDB of the scratch name the link wrote, which the
    // executable names.
    let mut want = vec![format!(".{}.fp", exe("t")), exe("t"), "used".to_owned()];
    if cfg!(windows) {
        want.push(format!(".{}.pdb", exe("t")));
    }
    want.sort();
    assert_eq!(left, want);
}

// spec: §9.10.1
#[test]
fn build_run_and_repl_with_dependencies() {
    if !llvm() {
        return;
    }
    let root = scratch("e2e");
    let home = root.join("home");
    let repo = root.join("greet");
    commit(&repo, &[("src/lib.rok", LIB_V2)]);
    let app = root.join("app");
    ok(&rok(&home, &root, &["new", "app"]));
    write(
        &app.join("src/main.rok"),
        "use greet::greet;\nprintln(f\"{greet()}\");\n",
    );
    ok(&rok(
        &home,
        &app,
        &["add", "greet", "--git", &repo.display().to_string()],
    ));
    assert_eq!(ok(&rok(&home, &app, &["-q", "run"])), "2\n");
    assert!(app.join("target/debug").join(exe("app")).is_file());
    // `repl <path>`: the session has the program's imports and dependencies.
    let mut child = rok_cmd(&home, &root, &["-q", "repl", "app"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b":type greet()\ngreet() + 1\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(ok(&out), "2\ni32\n3\n", "{}", text(&out.stderr));
    ok(&rok(&home, &app, &["-q", "build", "--release"]));
    assert!(app.join("target/release").join(exe("app")).is_file());
    ok(&rok(&home, &app, &["-q", "clean"]));
    assert!(!app.join("target").exists());

    // `:add` in the REPL goes to the active environment; bare `rok` is `rok repl`.
    write(&root.join("lib1/src/lib.rok"), LIB_V1);
    ok(&rok(&home, &root, &["env", "create", "r"]));
    let mut child = rok_cmd(&home, &root, &["--env", "r"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = format!(
        ":add g --path {}\nuse g::greet;\ngreet() + 40\n",
        root.join("lib1").display()
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = ok(&out);
    assert!(stdout.contains("41\n"), "{stdout}\n{}", text(&out.stderr));
    assert!(read(&home.join("envs/r/rok.rokon")).contains("\"g\": Dep::path("));
}

/// `rok build` builds each dependency once into `target/<profile>/deps`
/// (`lib<name>.a` + `<name>.rokhir`), reuses it while nothing it depends
/// on changes, and rebuilds it (and the ones after it) when its source
/// does. A dependency of a dependency is built first.
// spec: §9.2.15
#[test]
fn prebuilt_dependencies_are_reused_and_rebuilt() {
    if !llvm() {
        return;
    }
    let root = scratch("prebuilt");
    let home = root.join("home");
    write(
        &root.join("base/src/lib.rok"),
        "pub struct Cell { pub v: i32; }\npub fn cell(v: i32) -> Cell { return Cell { v: v }; }\n",
    );
    write(&root.join("base/rok.rokon"), "Package { name: \"base\" }\n");
    write(
        &root.join("mid/src/lib.rok"),
        "pub fn twice<T>(x: T) -> (T, T) { return (x, x); }\n\
         pub fn get() -> i32 { return base::cell(20).v + 1; }\n",
    );
    write(
        &root.join("mid/rok.rokon"),
        "Package {\n    name: \"mid\",\n    deps: { \"base\": Dep::path(\"../base\") },\n}\n",
    );
    let app = root.join("app");
    ok(&rok(&home, &root, &["new", "app"]));
    write(
        &app.join("src/main.rok"),
        "let (a, b) = mid::twice(\"x\");\nprintln(f\"{mid::get()} {a}{b}\");\n",
    );
    write(
        &app.join("rok.rokon"),
        "Package {\n    name: \"app\",\n    deps: { \"mid\": Dep::path(\"../mid\") },\n}\n",
    );
    let built = |out: &Output, dep: &str| text(&out.stderr).contains(&format!("Compiling {dep} "));
    let out = rok(&home, &app, &["run"]);
    assert_eq!(ok(&out), "21 xx\n");
    assert!(
        built(&out, "base") && built(&out, "mid"),
        "{}",
        text(&out.stderr)
    );
    let deps = app.join("target/debug/deps");
    for f in ["libbase.a", "base.rokhir", "libmid.a", "mid.rokhir"] {
        assert!(deps.join(f).is_file(), "{f}");
    }
    // Nothing changed: both reused.
    let out = rok(&home, &app, &["run"]);
    assert_eq!(ok(&out), "21 xx\n");
    assert!(
        !built(&out, "base") && !built(&out, "mid"),
        "{}",
        text(&out.stderr)
    );
    // `base` changed: it and `mid` (built after it) are rebuilt.
    write(
        &root.join("base/src/lib.rok"),
        "pub struct Cell { pub v: i32; }\npub fn cell(v: i32) -> Cell { return Cell { v: v * 2 }; }\n",
    );
    let out = rok(&home, &app, &["run"]);
    assert_eq!(ok(&out), "41 xx\n");
    assert!(
        built(&out, "base") && built(&out, "mid"),
        "{}",
        text(&out.stderr)
    );
    // Another profile is another build.
    let out = rok(&home, &app, &["build", "--release"]);
    ok(&out);
    assert!(built(&out, "mid"), "{}", text(&out.stderr));
    assert!(app.join("target/release/deps/mid.rokhir").is_file());
}

// spec: §9.9.13 §9.10.1
#[test]
fn rokls_is_rok_lsp() {
    let root = scratch("lsp");
    let frame = |b: &str| format!("Content-Length: {}\r\n\r\n{b}", b.len());
    let input = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#,
        r#"{"jsonrpc":"2.0","method":"exit"}"#,
    ]
    .map(frame)
    .concat();
    build_rokc();
    for (exe, args) in [(env!("CARGO_BIN_EXE_rokls"), &[][..]), (ROK, &["lsp"][..])] {
        let mut child = Command::new(exe)
            .args(args)
            .current_dir(&root)
            .env("ROK_HOME", root.join("home"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let stdout = ok(&out);
        assert!(stdout.contains(r#""hoverProvider":true"#), "{stdout}");
    }
}

const ONE: &str = "pub fn one() -> i32 { return 1; }\n";

/// A stone's repository at `dir`: `files` plus a `rok.rokon` for `name`
/// `vers` with `deps`, committed, with remote `origin` `origin(dir)`.
fn stone(dir: &Path, name: &str, vers: &str, deps: &str, files: &[(&str, &str)]) -> String {
    stone_meta(dir, name, vers, deps, "", files)
}

/// `stone` with manifest fields `meta` (`license: "MIT", `).
fn stone_meta(
    dir: &Path,
    name: &str,
    vers: &str,
    deps: &str,
    meta: &str,
    files: &[(&str, &str)],
) -> String {
    let manifest =
        format!("Package {{ name: \"{name}\", version: \"{vers}\", {meta}deps: {{ {deps} }} }}\n");
    let mut all = files.to_vec();
    all.push(("rok.rokon", &manifest));
    let c = commit(dir, &all);
    if git(dir, &["remote"]).is_empty() {
        git(dir, &["remote", "add", "origin", &origin(dir)]);
    }
    c
}

/// `name` / `vers` pairs of the stones in `rok.lock`.
fn locked_stones(lock: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut name = "";
    for l in lock.lines().map(str::trim) {
        if let Some(n) = l.strip_prefix("name: ") {
            name = n.trim_end_matches(',');
        } else if let Some(v) = l.strip_prefix("vers: ") {
            out.push(format!(
                "{}@{}",
                name.trim_matches('"'),
                v.trim_end_matches(',').trim_matches('"')
            ));
        }
    }
    out
}

/// Publish `num` 1.0.0, 1.1.0, 2.0.0 and `geom` 0.3.0 (needing `num`
/// ^1.0) to the index checkout `index`, checking `publish` on the way;
/// `num` 1.0.0's tree.
fn publish_stones(root: &Path, home: &Path, index: &Path) -> String {
    let reg = index.display().to_string();
    let index_log = || git(index, &["log", "--format=%s"]);
    let one = ONE;
    // `num` 1.0.0: a dry run prints the line and writes nothing.
    let num = root.join("num");
    let c1 = stone(&num, "num", "1.0.0", "", &[("src/lib.rok", one)]);
    let tree1 = git(&num, &["rev-parse", "HEAD^{tree}"]);
    let line = format!(
        "Stone {{ name: \"num\", vers: \"1.0.0\", deps: [], source: Dep::git({}, rev: \
         \"{c1}\"), cksum: \"{tree1}\", yanked: false }}\n",
        q(origin(&num))
    );
    let out = rok(home, &num, &["publish"]);
    assert_eq!(ok(&out), line);
    let note = |o: &Output| text(&o.stderr).trim().to_owned();
    assert_eq!(
        note(&out),
        format!(
            "note: dry run; --commit sends it to the registry API of {reg} (needs `rok login`), \
             which adds it as 3/n/num"
        )
    );
    let out = rok(home, &num, &["publish", "--index", &reg]);
    assert_eq!(ok(&out), line);
    assert_eq!(
        note(&out),
        format!("note: dry run; --commit commits it to {reg} as 3/n/num (not pushed)")
    );
    assert!(!index.join("3/n/num").exists());
    // Without --index, --commit goes through the registry API: a token first.
    fails(
        &rok(home, &num, &["publish", "--commit"]),
        "run `rok login` first",
    );
    ok(&rok(home, &num, &["publish", "--index", &reg, "--commit"]));
    assert_eq!(read(&index.join("3/n/num")), line);
    assert!(
        index_log().starts_with("Publish num 1.0.0\n"),
        "{}",
        index_log()
    );
    fails(
        &rok(home, &num, &["publish", "--index", &reg, "--commit"]),
        "num 1.0.0 is already in the index",
    );
    write(&num.join("src/lib.rok"), "dirty");
    fails(&rok(home, &num, &["publish"]), "uncommitted changes");

    // More versions, and `geom` 0.3.0 needing `num` ^1.0.
    for v in ["1.1.0", "2.0.0"] {
        stone(&num, "num", v, "", &[("src/lib.rok", one)]);
        ok(&rok(home, &num, &["publish", "--index", &reg, "--commit"]));
    }
    let geom = root.join("geom");
    let two = "use num::one;\npub fn two() -> i32 { return one() + one(); }\n";
    // Manifest metadata rides along in the entry.
    stone_meta(
        &geom,
        "geom",
        "0.3.0",
        "\"num\": \"^1.0\"",
        "description: \"Shapes\", license: \"MIT\", repository: \"https://example.com/geom\", ",
        &[("src/lib.rok", two)],
    );
    ok(&rok(home, &geom, &["publish", "--index", &reg, "--commit"]));
    let entry = read(&index.join("ge/om/geom"));
    assert!(
        entry.contains("deps: [{ name: \"num\", req: \"^1.0\", kind: \"normal\" }]"),
        "{entry}"
    );
    assert!(
        entry.contains(
            "yanked: false, description: \"Shapes\", license: \"MIT\", repository: \
             \"https://example.com/geom\" }"
        ),
        "{entry}"
    );
    // A path dependency cannot be published.
    let bad = root.join("bad");
    stone(
        &bad,
        "bad",
        "0.1.0",
        "\"x\": Dep::path(\"..\")",
        &[("src/lib.rok", one)],
    );
    fails(
        &rok(home, &bad, &["publish"]),
        "dependency `x` is not a stone",
    );
    // The source is `origin` as configured, before the publisher's own
    // `insteadOf` rewrites, and must be https.
    let ssh = root.join("ssh");
    stone(&ssh, "ssh", "0.1.0", "", &[("src/lib.rok", one)]);
    git(
        &ssh,
        &[
            "config",
            "url.git@github.com:.insteadOf",
            "https://github.com/",
        ],
    );
    git(
        &ssh,
        &["remote", "set-url", "origin", "https://github.com/o/ssh"],
    );
    let line = ok(&rok(home, &ssh, &["publish"]));
    assert!(
        line.contains("source: Dep::git(\"https://github.com/o/ssh\""),
        "{line}"
    );
    git(
        &ssh,
        &["remote", "set-url", "origin", "git@github.com:o/ssh.git"],
    );
    fails(
        &rok(home, &ssh, &["publish"]),
        "a stone's source must be an https:// URL; run `git remote set-url origin \
         https://github.com/o/ssh.git`",
    );
    git(&ssh, &["remote", "remove", "origin"]);
    fails(
        &rok(home, &ssh, &["publish"]),
        "add one with `git remote add origin https://<host>/<owner>/<repo>`",
    );

    tree1
}

/// Package metadata (`authors`, `homepage`, `documentation`, `readme`,
/// `keywords`, `publish`) is checked at publish; index entries from a
/// newer rok, with fields this one does not know, still resolve.
// spec: §9.10.3 §9.10.4a
#[test]
fn package_metadata_and_publish() {
    let root = scratch("pkg_meta");
    let home = root.join("home");
    let index = root.join("index");
    commit(
        &index,
        &[("config.rokon", "Config { api: \"https://rokstones.io\" }\n")],
    );
    let reg = index.display().to_string();
    write(
        &home.join("config.rokon"),
        &format!("Config {{ registry: {} }}\n", q(&reg)),
    );
    let meta = root.join("meta");
    let fields = "authors: [\"Ann <a@x.org>\"], homepage: \"https://x.org\", documentation: \
                  \"https://x.org/doc\", keywords: [\"geo\", \"math\"], license: \"MIT\", ";
    let files = [("src/lib.rok", ONE), ("README.md", "# meta\n")];
    // `fields` with `extra`; `extra` keywords replace the default ones.
    let publish = |extra: &str| {
        let base = if extra.starts_with("keywords") {
            fields.replace("keywords: [\"geo\", \"math\"], ", "")
        } else {
            fields.to_owned()
        };
        stone_meta(
            &meta,
            "meta",
            "0.1.0",
            "",
            &format!("{base}{extra}"),
            &files,
        );
        rok(&home, &meta, &["publish"])
    };
    // Checked, not in the entry: rok 0.1.1 and older reject unknown fields.
    let line = ok(&publish(""));
    assert!(
        line.ends_with("yanked: false, license: \"MIT\" }\n"),
        "{line}"
    );
    fails(&publish("readme: \"doc/R.md\", "), "`readme: \"doc/R.md\"`");
    fails(
        &publish("readme: \"../meta/README.md\", "),
        "is outside the package; write a path inside it",
    );
    ok(&publish("readme: \"README.md\", "));
    fails(
        &publish("keywords: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\"], "),
        "`keywords` has 6 entries; keep at most 5",
    );
    fails(
        &publish("publish: false, "),
        "`meta` has `publish: false` in rok.rokon, so it is not published; remove that line \
         (or write `publish: true`)",
    );
    // An entry as a newer rok may write it.
    let newer = line.replace(
        "license: \"MIT\" }",
        "license: \"MIT\", authors: [\"Ann <a@x.org>\"], keywords: [\"geo\"] }",
    );
    commit(&index, &[("me/ta/meta", &newer)]);
    let app = root.join("app");
    write(&app.join("src/main.rok"), "println(\"x\");\n");
    write(&app.join("rok.rokon"), APP0);
    ok(&rok(&home, &app, &["add", "meta"]));
    assert_eq!(locked_stones(&read(&app.join("rok.lock"))), ["meta@0.1.0"]);
}

/// A dependency on the package itself (here a test-only one) is an
/// error: `pkg::` is how a package names its own items.
// spec: §5.7.14
#[test]
fn self_dependency_is_rejected() {
    let root = scratch("self_dep");
    let home = root.join("home");
    let lib = root.join("me");
    write(&lib.join("src/lib.rok"), "pub fn f() -> i32 { 1 }\n");
    write(
        &lib.join("rok.rokon"),
        "Package {\n    name: \"me\",\n    dev_deps: { \"me\": Dep::path(\".\") },\n}\n",
    );
    fails(
        &rok(&home, &lib, &["check"]),
        "dependency `me` (required by the root package) is the package being built",
    );
}

/// A stand-in for the registry API on `http://127.0.0.1:<port>` (the
/// port): token `rks_ok` is account `alice`, who may publish;
/// `rks_reader` is `bob`, who may not; others get 401. Each name and
/// version publishes once. Every request goes to `log` as
/// `<method> <path> [<Authorization>] <body>`.
fn fake_api(log: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut published = Vec::new();
        for conn in listener.incoming() {
            let mut conn = conn.unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut start = String::new();
            reader.read_line(&mut start).unwrap();
            let (mut auth, mut len) = (String::new(), 0);
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let Some((key, value)) = header.trim_end().split_once(": ") else {
                    break;
                };
                match key.to_ascii_lowercase().as_str() {
                    "authorization" => value.clone_into(&mut auth),
                    "content-length" => len = value.parse().unwrap(),
                    _ => {}
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            let mut words = start.split(' ');
            let (method, path) = (words.next().unwrap(), words.next().unwrap());
            log.lock()
                .unwrap()
                .push(format!("{method} {path} [{auth}] {body}"));
            let field = |f: &str| {
                let rest = body.split(&format!("{f}: \"")).nth(1).unwrap_or("");
                rest.split('"').next().unwrap().to_owned()
            };
            let (code, reply) = match (auth.as_str(), path) {
                ("Bearer rks_ok", "/api/v1/me") => {
                    (200, "Me { login: \"alice\", publish: true }".to_owned())
                }
                ("Bearer rks_reader", "/api/v1/me") => {
                    (200, "Me { login: \"bob\", publish: false }".to_owned())
                }
                ("Bearer rks_ok", "/api/v1/publish") => {
                    let (n, v) = (field("name"), field("vers"));
                    if published.contains(&(n.clone(), v.clone())) {
                        let m = format!("{n} {v} is already in the index");
                        (409, format!("Error {{ message: \"{m}\" }}"))
                    } else {
                        let reply = format!(
                            "Published {{ name: \"{n}\", vers: \"{v}\", file: \"3/n/{n}\" }}"
                        );
                        published.push((n, v));
                        (200, reply)
                    }
                }
                ("Bearer rks_ok", "/api/v1/yank") => (
                    200,
                    format!(
                        "Yanked {{ name: \"{}\", vers: \"{}\", yanked: true }}",
                        field("name"),
                        field("vers")
                    ),
                ),
                _ => (401, "Error { message: \"bad token\" }".to_owned()),
            };
            let _ = write!(
                conn,
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    port
}

/// `login` / `logout`, and `publish` / `yank` without --index: through
/// the registry API named by the index's config.rokon (`fake_api`).
#[test]
fn login_publish_yank_api() {
    let root = scratch("api");
    let home = root.join("home");
    let log = Arc::new(Mutex::new(Vec::new()));
    let api = format!("http://127.0.0.1:{}", fake_api(log.clone()));
    let last = || log.lock().unwrap().last().cloned().unwrap_or_default();
    let index = root.join("index");
    commit(
        &index,
        &[("config.rokon", &format!("Config {{ api: \"{api}\" }}\n"))],
    );
    let reg = index.display().to_string();
    let at = |args: &[&str]| {
        let mut c = rok_cmd(&home, &root.join("num"), args);
        c.env("ROK_REGISTRY", &reg);
        c
    };
    // Every output, checked never to hold a token.
    let done = |c: &mut Command| {
        let out = c.output().unwrap();
        for t in ["rks_ok", "rks_reader", "rks_nope"] {
            assert!(!text(&out.stdout).contains(t) && !text(&out.stderr).contains(t));
        }
        out
    };
    let run = |args: &[&str]| done(&mut at(args));
    stone(
        &root.join("num"),
        "num",
        "1.0.0",
        "",
        &[("src/lib.rok", ONE)],
    );

    let out = run(&["logout"]);
    ok(&out);
    assert!(text(&out.stderr).contains("not logged in"));
    fails(&run(&["publish", "--commit"]), "run `rok login` first");
    fails(&run(&["yank", "--vers", "1.0.0"]), "run `rok login` first");
    // A dry run needs no token and sends nothing.
    ok(&run(&["publish"]));
    assert!(log.lock().unwrap().is_empty());

    // A bad token, and an account that may not publish: nothing saved.
    fails(
        &run(&["login", "rks_nope"]),
        &format!("{api} refused the token: create one at {api}/me"),
    );
    assert_eq!(last(), "GET /api/v1/me [Bearer rks_nope] ");
    fails(
        &run(&["login", "rks_reader"]),
        "this account cannot publish yet: publishing is open to rokstones.io maintainers only",
    );
    let cred = home.join("credentials.rokon");
    assert!(!cred.exists());

    // The token from stdin.
    let mut child = at(&["login"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "rks_ok").unwrap();
    let out = child.wait_with_output().unwrap();
    ok(&out);
    assert!(text(&out.stderr).contains("Logged in as alice"));
    assert!(!text(&out.stderr).contains("rks_ok") && !text(&out.stdout).contains("rks_ok"));
    assert_eq!(read(&cred), "Credentials { token: \"rks_ok\" }\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&cred).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Publish: the entry line is the body; a second time, the API's
    // error is relayed.
    let out = run(&["publish", "--commit"]);
    let line = ok(&out);
    assert!(line.starts_with("Stone { name: \"num\", vers: \"1.0.0\""));
    assert_eq!(
        last(),
        format!("POST /api/v1/publish [Bearer rks_ok] {}", line.trim_end())
    );
    assert!(text(&out.stderr).contains(&format!("Published num 1.0.0 to {api}")));
    fails(
        &run(&["publish", "--commit"]),
        &format!("{api}: num 1.0.0 is already in the index"),
    );

    // Yank with the environment's token, no file; a bad one is refused.
    ok(&run(&["logout"]));
    assert!(!cred.exists());
    ok(&done(
        at(&["yank", "--vers", "1.0.0"]).env("ROK_REGISTRY_TOKEN", "rks_ok"),
    ));
    assert_eq!(
        last(),
        "POST /api/v1/yank [Bearer rks_ok] Yank { name: \"num\", vers: \"1.0.0\", undo: false, \
         native: false }"
    );
    ok(&done(
        at(&["yank", "--native", "--undo", "--vers", "2.0.0", "z"])
            .env("ROK_REGISTRY_TOKEN", "rks_ok"),
    ));
    assert_eq!(
        last(),
        "POST /api/v1/yank [Bearer rks_ok] Yank { name: \"z\", vers: \"2.0.0\", undo: true, \
         native: true }"
    );
    fails(
        &done(at(&["yank", "--vers", "1.0.0"]).env("ROK_REGISTRY_TOKEN", "rks_nope")),
        "refused the token",
    );

    // The API must be HTTPS off the loopback host.
    commit(
        &index,
        &[("config.rokon", "Config { api: \"http://example.com\" }\n")],
    );
    fails(
        &run(&["login", "rks_ok"]),
        "api `http://example.com` must be an https:// URL",
    );
}

/// Stones end to end on a local index: publish (dry run and commit),
/// `add` / lock / `update`, conflicts, yank, cksum checks, search.
#[test]
fn stones_publish_add_update_yank() {
    let root = scratch("stones");
    let home = root.join("home");
    let index = root.join("index");
    commit(
        &index,
        &[("config.rokon", "Config { api: \"https://rokstones.io\" }\n")],
    );
    for kv in [
        ["user.name", "t"],
        ["user.email", "t@t"],
        ["commit.gpgsign", "false"],
    ] {
        git(&index, &["config", kv[0], kv[1]]);
    }
    let reg = index.display().to_string();
    let qreg = q(&reg);
    write(
        &home.join("config.rokon"),
        &format!("Config {{ registry: {qreg} }}\n"),
    );
    let num = root.join("num");
    let tree1 = publish_stones(&root, &home, &index);
    let index_log = || git(&index, &["log", "--format=%s"]);

    // `add`: `^` the newest version; the lock pins both stones.
    let app = root.join("app");
    write(
        &app.join("src/main.rok"),
        "use geom::two;\nprintln(f\"{two()}\");\n",
    );
    write(&app.join("rok.rokon"), APP0);
    ok(&rok(&home, &app, &["add", "geom"]));
    assert!(read(&app.join("rok.rokon")).contains("\"geom\": Dep::version(\"^0.3.0\"),"));
    let lock = read(&app.join("rok.lock"));
    assert_eq!(locked_stones(&lock), ["geom@0.3.0", "num@1.1.0"]);
    assert!(
        lock.contains("source: Dep::registry(") && lock.contains(&qreg),
        "{lock}"
    );
    let names: Vec<String> = deps(&home, &app, &[]).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["num", "geom"]);
    if llvm() {
        assert_eq!(ok(&rok(&home, &app, &["-q", "run"])), "2\n");
    }

    // A root requirement the chosen version does not meet: a conflict
    // naming both, and rok.rokon as it was.
    let before = read(&app.join("rok.rokon"));
    let out = rok(&home, &app, &["add", "num@2"]);
    fails(&out, "conflicting requirements for stone `num`");
    for want in [
        "`2` (required by the root package)",
        "`^1.0` (required by `geom`)",
    ] {
        assert!(text(&out.stderr).contains(want), "{}", text(&out.stderr));
    }
    assert_eq!(read(&app.join("rok.rokon")), before);
    ok(&rok(&home, &app, &["add", "num@~1.1"]));
    assert!(read(&app.join("rok.rokon")).contains("\"num\": Dep::version(\"~1.1\"),"));
    fails(
        &rok(&home, &app, &["add", "num@^9"]),
        "no version of stone `num` matches `^9`",
    );
    fails(&rok(&home, &app, &["add", "nope"]), "no stone named `nope`");
    fails(
        &rok(&home, &app, &["add", "num@x.y"]),
        "is not a version requirement",
    );

    // A new version: builds keep the locked one until `update`.
    stone(&num, "num", "1.1.5", "", &[("src/lib.rok", ONE)]);
    ok(&rok(&home, &num, &["publish", "--index", &reg, "--commit"]));
    ok(&rok(&home, &app, &["--locked", "deps"]));
    assert!(locked_stones(&read(&app.join("rok.lock"))).contains(&"num@1.1.0".to_owned()));
    ok(&rok(&home, &app, &["update", "num"]));
    assert!(locked_stones(&read(&app.join("rok.lock"))).contains(&"num@1.1.5".to_owned()));
    ok(&rok(&home, &app, &["--offline", "deps"]));

    // Yanked: kept while locked, skipped by `update`; `--undo` restores.
    ok(&rok(
        &home,
        &num,
        &["yank", "--vers", "1.1.5", "--index", &reg],
    ));
    assert!(read(&index.join("3/n/num")).contains("vers: \"1.1.5\""));
    assert!(index_log().starts_with("Yank num 1.1.5\n"));
    ok(&rok(&home, &app, &["deps"]));
    assert!(locked_stones(&read(&app.join("rok.lock"))).contains(&"num@1.1.5".to_owned()));
    ok(&rok(&home, &app, &["update"]));
    assert!(locked_stones(&read(&app.join("rok.lock"))).contains(&"num@1.1.0".to_owned()));
    let out = rok(&home, &app, &["add", "num@=1.1.5"]);
    fails(&out, "1.1.5 (yanked)");
    ok(&rok(
        &home,
        &num,
        &["yank", "--undo", "--vers", "1.1.5", "--index", &reg, "num"],
    ));
    ok(&rok(&home, &app, &["add", "num@=1.1.5"]));
    fails(
        &rok(&home, &num, &["yank", "--vers", "9.9.9", "--index", &reg]),
        "not in the index",
    );

    // An entry whose cksum is not its commit's tree is refused.
    let entry = read(&index.join("3/n/num"))
        .lines()
        .next()
        .unwrap()
        .replace("1.0.0", "1.0.1");
    let forged = format!("{}\n", entry.replace(&tree1, &"0".repeat(40)));
    write(
        &index.join("3/n/num"),
        &(read(&index.join("3/n/num")) + &forged),
    );
    git(&index, &["commit", "-qam", "forged"]);
    fails(
        &rok(&home, &app, &["add", "num@=1.0.1"]),
        "but the index's cksum is",
    );

    // `search`, with the index from ROK_REGISTRY in a fresh ROK_HOME.
    let home2 = root.join("home2");
    let out = rok_cmd(&home2, &root, &["search", "o"])
        .env("ROK_REGISTRY", &reg)
        .output()
        .unwrap();
    assert_eq!(ok(&out), "geom 0.3.0 - Shapes\n");
    assert_eq!(ok(&rok(&home, &root, &["search", "NU"])), "num 2.0.0\n");
    // Either spelling of `-` / `_` finds a stone.
    let big = root.join("big-int");
    stone(&big, "big-int", "0.1.0", "", &[("src/lib.rok", ONE)]);
    ok(&rok(&home, &big, &["publish", "--index", &reg, "--commit"]));
    for q in ["big_int", "G-I", "big-int"] {
        assert_eq!(ok(&rok(&home, &root, &["search", q])), "big-int 0.1.0\n");
    }

    // A per-dependency index.
    ok(&rok(&home, &app, &["add", "geom@0.3", "--registry", &reg]));
    assert!(read(&app.join("rok.rokon")).contains(&format!(
        "\"geom\": Dep::version(\"0.3\", registry: {qreg}),"
    )));
}

/// `cmd` in `dir`, which must succeed.
#[cfg(unix)]
fn sh(dir: &Path, cmd: &str, args: &[&str]) {
    let out = Command::new(cmd)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{cmd} {args:?}: {}",
        text(&out.stderr)
    );
}

/// Whether `shell` runs here (zsh, fish and pwsh may be missing).
fn have(shell: &str) -> bool {
    Command::new(shell)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `rok completions <shell>`: a script with rok's commands, its own and
/// those it passes to rokc, that the shell parses; toolchain and
/// environment names come from `rok __complete`.
// spec: §9.10.1
#[test]
fn completions_for_each_shell() {
    let root = scratch("completions");
    let home = root.join("home");
    for shell in ["bash", "zsh", "fish", "powershell"] {
        let script = ok(&rok(&home, &root, &["completions", shell]));
        for want in [
            "toolchain",
            "completions",
            "build",
            "env",
            "--offline",
            "--release",
            "rok __complete toolchains",
        ] {
            // fish spells `--x` as `-l x`.
            let want = match want.strip_prefix("--") {
                Some(f) if shell == "fish" => format!("-l {f}"),
                _ => want.to_owned(),
            };
            assert!(script.contains(&want), "{shell} lacks {want}");
        }
        let ext = if shell == "powershell" { "ps1" } else { shell };
        let file = root.join(format!("rok-{shell}.{ext}"));
        write(&file, &script);
        let check: &[&str] = match shell {
            "bash" => &["bash", "-n"],
            "zsh" => &["zsh", "-n"],
            "fish" => &["fish", "--no-config", "-n"],
            _ => &[],
        };
        if let Some((sh, args)) = check.split_first().filter(|(sh, _)| have(sh)) {
            let out = Command::new(sh).args(args).arg(&file).output().unwrap();
            assert!(out.status.success(), "{shell}: {}", text(&out.stderr));
        }
        if shell == "powershell" && have("pwsh") {
            let parse = format!(
                "$e = $null; [void][Management.Automation.Language.Parser]::ParseFile('{}', \
                 [ref]$null, [ref]$e); if ($e) {{ $e; exit 1 }}",
                file.display()
            );
            let out = Command::new("pwsh")
                .args(["-NoProfile", "-c", &parse])
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", text(&out.stdout));
        }
    }
    let out = rok(&home, &root, &["completions", "tcsh"]);
    assert_eq!(out.status.code(), Some(2));
    fails(&out, "rok completions bash|zsh|fish|powershell");

    let host = env!("ROK_HOST");
    fs::create_dir_all(home.join(format!("toolchains/9.9.1-{host}"))).unwrap();
    fs::create_dir_all(home.join("envs/dev")).unwrap();
    assert_eq!(
        ok(&rok(&home, &root, &["__complete", "toolchains"])),
        "9.9.1\n"
    );
    assert_eq!(ok(&rok(&home, &root, &["__complete", "envs"])), "dev\n");

    // What bash offers, with this rok first on PATH.
    #[cfg(unix)]
    {
        let bin = Path::new(ROK).parent().unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let offer = |words: &[&str]| {
            let out = Command::new("bash")
                .arg("-c")
                .arg(
                    "source \"$1\"; shift; COMP_WORDS=(\"$@\"); \
                     COMP_CWORD=$((${#COMP_WORDS[@]} - 1)); _rok; echo \"${COMPREPLY[*]}\"",
                )
                .arg("bash")
                .arg(root.join("rok-bash.bash"))
                .args(words)
                .env("PATH", &path)
                .env("ROK_HOME", &home)
                .output()
                .unwrap();
            ok(&out).trim().to_owned()
        };
        assert_eq!(offer(&["rok", "toolc"]), "toolchain");
        assert_eq!(offer(&["rok", "toolchain", "def"]), "default");
        assert_eq!(offer(&["rok", "toolchain", "default", ""]), "9.9.1");
        assert_eq!(offer(&["rok", "+"]), "+9.9.1");
        assert_eq!(offer(&["rok", "--env", ""]), "dev");
        assert_eq!(offer(&["rok", "--env", "dev", "env", "use", ""]), "dev");
        assert_eq!(offer(&["rok", "build", "--rel"]), "--release");
        assert_eq!(offer(&["rok", "add", "x", "--regi"]), "--registry");
        assert_eq!(offer(&["rok", "help", "compl"]), "completions");
    }
}

/// `sh install.sh` in `root` for a user whose home is `user` and login
/// shell `shell`, with no XDG or zsh directories set.
#[cfg(unix)]
fn install_sh(root: &Path, user: &Path, shell: &str) -> Command {
    fs::create_dir_all(user).unwrap();
    let mut c = Command::new("sh");
    c.arg(workspace().join("install.sh"))
        .current_dir(root)
        .env("HOME", user)
        .env("SHELL", shell)
        .env_remove("ROK_HOME")
        .env_remove("ROK_NO_MODIFY_PATH")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("ZDOTDIR")
        .env_remove("CLICOLOR_FORCE");
    c
}

/// `root/rok.tar.gz`: a fake toolchain `version`.
#[cfg(unix)]
fn fake_archive(root: &Path, version: &str) {
    let top = format!("rok-{version}");
    fake_toolchain(&root.join("stage").join(&top), version);
    let tar = Command::new("tar")
        .current_dir(root.join("stage"))
        .args(["-czf", "../rok.tar.gz", &top])
        .status()
        .unwrap();
    assert!(tar.success());
}

/// The line install.sh adds to a shell startup file.
#[cfg(unix)]
const PROFILE_LINE: &str = "[ -f \"$HOME/.rok/env\" ] && . \"$HOME/.rok/env\"";

/// install.sh writes `$ROK_HOME/env`, sources it (guarded) from the
/// startup files that exist and the login shell's, once, and installs
/// the completions; a second run changes nothing.
// spec: §9.10.6
#[cfg(unix)]
#[test]
fn install_sh_sets_up_path() {
    let root = scratch("install-sh-path");
    let user = root.join("user");
    fake_archive(&root, "9.9.3");
    write(&user.join(".bashrc"), "alias ll='ls -l'");
    let install = || {
        install_sh(&root, &user, "/bin/zsh")
            .args(["--archive", "rok.tar.gz"])
            .output()
            .unwrap()
    };
    let out = install();
    ok(&out);
    let err = text(&out.stderr);
    assert!(
        err.starts_with("   Installed rok 9.9.3 to ~/.rok\n"),
        "{err}"
    );
    assert!(
        err.contains("    Modified ~/.bashrc, ~/.zshenv (PATH)\n"),
        "{err}"
    );
    assert!(err.contains("A new shell finds rok on PATH"), "{err}");
    assert!(err.contains("  . \"$HOME/.rok/env\""), "{err}");
    let bashrc = format!("alias ll='ls -l'\n{PROFILE_LINE}\n");
    assert_eq!(read(&user.join(".bashrc")), bashrc);
    assert_eq!(read(&user.join(".zshenv")), format!("{PROFILE_LINE}\n"));
    assert!(!user.join(".profile").exists());
    assert!(!user.join(".bash_profile").exists());
    assert!(!user.join(".config/fish").exists());
    let bash = read(&user.join(".local/share/bash-completion/completions/rok"));
    assert!(bash.contains("complete -o bashdefault -o default -F _rok rok"));
    assert!(read(&user.join(".rok/zsh/_rok")).starts_with("#compdef rok\n"));

    // Sourced twice, env puts the proxies on PATH once.
    let path = |shell: &str| {
        let out = Command::new(shell)
            .args([
                "-c",
                ". \"$HOME/.zshenv\"; . \"$HOME/.rok/env\"; echo \"$PATH\"",
            ])
            .env("HOME", &user)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        ok(&out).trim().to_owned()
    };
    let want = format!("{}:/usr/bin:/bin", user.join(".rok/bin").display());
    assert_eq!(path("sh"), want);
    if Command::new("zsh").arg("-fc").arg("true").status().is_ok() {
        assert_eq!(path("zsh"), want);
    }

    // Again: nothing to change, no line twice.
    let out = install();
    ok(&out);
    let err = text(&out.stderr);
    assert!(!err.contains("Modified"), "{err}");
    assert_eq!(read(&user.join(".bashrc")), bashrc);
    assert_eq!(read(&user.join(".zshenv")), format!("{PROFILE_LINE}\n"));

    // Without ~/.rok the startup files still run.
    fs::remove_dir_all(user.join(".rok")).unwrap();
    let out = Command::new("sh")
        .args([
            "-c",
            ". \"$HOME/.zshenv\"; . \"$HOME/.bashrc\"; echo started",
        ])
        .env("HOME", &user)
        .output()
        .unwrap();
    assert_eq!(ok(&out), "started\n");
}

/// A bash user with no startup files gets `~/.bashrc` and `~/.profile`
/// (never a new `~/.bash_profile`, which would hide `~/.profile`).
// spec: §9.10.6
#[cfg(unix)]
#[test]
fn install_sh_bash_files() {
    let root = scratch("install-sh-bash");
    let user = root.join("user");
    fake_archive(&root, "9.9.4");
    let out = install_sh(&root, &user, "/usr/local/bin/bash")
        .args(["--archive", "rok.tar.gz"])
        .output()
        .unwrap();
    ok(&out);
    let err = text(&out.stderr);
    assert!(
        err.contains("    Modified ~/.bashrc, ~/.profile (PATH)\n"),
        "{err}"
    );
    assert_eq!(read(&user.join(".profile")), format!("{PROFILE_LINE}\n"));
    assert!(!user.join(".bash_profile").exists());
    assert!(!user.join(".zshenv").exists());
}

/// fish: `conf.d/rok.fish` (under `$XDG_CONFIG_HOME`) sources `env.fish`,
/// and the fish completions go beside it.
// spec: §9.10.6
#[cfg(unix)]
#[test]
fn install_sh_fish_conf_d() {
    let root = scratch("install-sh-fish");
    let user = root.join("user");
    let xdg = root.join("xdg");
    fake_archive(&root, "9.9.5");
    let install = || {
        install_sh(&root, &user, "/opt/bin/fish")
            .args(["--archive", "rok.tar.gz"])
            .env("XDG_CONFIG_HOME", &xdg)
            .output()
            .unwrap()
    };
    let out = install();
    ok(&out);
    let err = text(&out.stderr);
    let conf = xdg.join("fish/conf.d/rok.fish");
    assert!(
        err.contains(&format!("    Modified {} (PATH)\n", conf.display())),
        "{err}"
    );
    assert_eq!(
        read(&conf),
        "test -f \"$HOME/.rok/env.fish\"; and source \"$HOME/.rok/env.fish\"\n"
    );
    let comp = read(&xdg.join("fish/completions/rok.fish"));
    assert!(comp.contains("complete -c rok "), "{comp}");
    // No sh startup file exists and fish is the login shell: none made.
    assert!(!user.join(".profile").exists());
    let out = install();
    assert!(!text(&out.stderr).contains("Modified"));
    if Command::new("fish").arg("--version").output().is_ok() {
        let out = Command::new("fish")
            .args(["--no-config", "-c"])
            .arg(format!(
                "set -gx PATH /usr/bin /bin; source {0}; source {0}; string join : $PATH",
                conf.display()
            ))
            .env("HOME", &user)
            .output()
            .unwrap();
        let want = format!("{}:/usr/bin:/bin\n", user.join(".rok/bin").display());
        assert_eq!(ok(&out), want);
        let out = Command::new("fish")
            .args(["--no-config", "-n"])
            .arg(xdg.join("fish/completions/rok.fish"))
            .output()
            .unwrap();
        ok(&out);
    }
}

/// `--no-modify-path` and `ROK_NO_MODIFY_PATH=1` write no shell files and
/// print what to add to PATH instead.
// spec: §9.10.6
#[cfg(unix)]
#[test]
fn install_sh_no_modify_path() {
    let root = scratch("install-sh-no-path");
    fake_archive(&root, "9.9.6");
    for (i, how) in ["flag", "env"].into_iter().enumerate() {
        let user = root.join(format!("user{i}"));
        write(&user.join(".profile"), "# mine\n");
        let mut c = install_sh(&root, &user, "/bin/zsh");
        c.args(["--archive", "rok.tar.gz"]);
        if how == "flag" {
            c.arg("--no-modify-path");
        } else {
            c.env("ROK_NO_MODIFY_PATH", "1");
        }
        let out = c.output().unwrap();
        ok(&out);
        let err = text(&out.stderr);
        assert!(err.contains("Add ~/.rok/bin to PATH"), "{how}: {err}");
        assert!(!err.contains("Modified"), "{how}: {err}");
        assert_eq!(read(&user.join(".profile")), "# mine\n");
        assert!(!user.join(".zshenv").exists());
        assert!(!user.join(".rok/env").exists());
        assert!(!user.join(".rok/zsh").exists());
        assert!(!user.join(".local").exists());
    }
}

/// Without `$SHELL` (containers, some CI) install.sh does not fail.
/// Some `sh`s refill it from the user database, so only the run and
/// `$ROK_HOME/env` are checked.
// spec: §9.10.6
#[cfg(unix)]
#[test]
fn install_sh_without_shell() {
    let root = scratch("install-sh-no-shell");
    fake_archive(&root, "9.9.7");
    let user = root.join("user");
    let mut c = install_sh(&root, &user, "");
    c.env_remove("SHELL").args(["--archive", "rok.tar.gz"]);
    let out = c.output().unwrap();
    ok(&out);
    assert!(user.join(".rok/env").exists(), "{}", text(&out.stderr));
}

/// `install.sh --archive` checks the archive against the `.sha256`
/// beside it before unpacking.
// spec: §9.10.6
#[cfg(unix)]
#[test]
fn install_sh_checks_sums() {
    let root = scratch("install-sh");
    let home = root.join("home");
    fake_toolchain(&root.join("stage/rok-9.9.1"), "9.9.1");
    let tar = Command::new("tar")
        .current_dir(root.join("stage"))
        .args(["-czf", "../rok.tar.gz", "rok-9.9.1"])
        .status()
        .unwrap();
    assert!(tar.success());
    let install = || {
        install_sh(&root, &root.join("user"), "/bin/sh")
            .args(["--archive", "rok.tar.gz"])
            .env("ROK_HOME", &home)
            .output()
            .unwrap()
    };
    let sum = root.join("rok.tar.gz.sha256");
    write(&sum, &format!("{}  rok.tar.gz\n", "a".repeat(64)));
    fails(&install(), "its .sha256 says aaaa");
    assert!(!home.join("toolchains").exists());
    write(
        &sum,
        &format!("{}  rok.tar.gz\n", sha256_of(&root.join("rok.tar.gz"))),
    );
    let out = install();
    assert_eq!(ok(&out), "");
    let err = text(&out.stderr);
    assert!(err.starts_with("   Installed rok 9.9.1 to "), "{err}");
    assert!(err.contains("Next steps:\n  rok --version\n"), "{err}");
    let host = env!("ROK_HOST");
    assert!(
        home.join(format!("toolchains/9.9.1-{host}/bin/rokc"))
            .is_file()
    );
}

/// `c`'s stderr on a pseudo-terminal; it must succeed.
#[cfg(unix)]
fn stderr_on_tty(mut c: Command) -> String {
    use std::io::Read;
    // SAFETY: plain libc calls on a pseudo-terminal this function owns.
    let (master, slave) = unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0 && libc::grantpt(m) == 0 && libc::unlockpt(m) == 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(m)).to_owned();
        let master = <fs::File as std::os::fd::FromRawFd>::from_raw_fd(m);
        let slave = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name.to_str().unwrap())
            .unwrap();
        (master, slave)
    };
    let mut child = c.stderr(slave).spawn().unwrap();
    // The slave closes with the child: the reader then sees EOF or EIO.
    drop(c);
    let reader = std::thread::spawn(move || {
        let mut master = master;
        let mut out = Vec::new();
        let mut b = [0u8; 4096];
        while let Ok(n @ 1..) = master.read(&mut b) {
            out.extend_from_slice(&b[..n]);
        }
        out
    });
    assert!(child.wait().unwrap().success());
    text(&reader.join().unwrap())
}

/// install.sh colours its status lines as rok does: on a terminal (its
/// stderr, so `curl .. | sh` too) or with `CLICOLOR_FORCE`, never with
/// `NO_COLOR` or `TERM=dumb` or when piped.
// spec: §9.1.8 §9.10.6
#[cfg(unix)]
#[test]
fn install_sh_colours() {
    let root = scratch("install-sh-colour");
    fake_archive(&root, "9.9.4");
    let install = |vars: &[(&str, &str)]| {
        let mut c = install_sh(&root, &root.join("user"), "/bin/sh");
        c.args(["--no-modify-path", "--archive", "rok.tar.gz"])
            .env("ROK_HOME", root.join("home"))
            .env("TERM", "xterm")
            .env_remove("NO_COLOR")
            .env_remove("COLORTERM")
            .envs(vars.iter().copied())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        c
    };
    let sky = "\x1b[1;96m   Installed\x1b[0m rok 9.9.4 to ";
    let err = stderr_on_tty(install(&[]));
    assert!(err.starts_with(sky), "{err:?}");
    let err = stderr_on_tty(install(&[("TERM", "xterm-256color")]));
    assert!(
        err.starts_with("\x1b[1;38;5;81m   Installed\x1b[0m "),
        "{err:?}"
    );
    for vars in [&[("NO_COLOR", "1")][..], &[("TERM", "dumb")]] {
        let err = stderr_on_tty(install(vars));
        assert!(err.starts_with("   Installed rok 9.9.4 to "), "{err:?}");
        assert!(!err.contains('\x1b'), "{vars:?}: {err:?}");
    }
    let piped = |vars: &[(&str, &str)]| {
        let out = install(vars)
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        assert!(out.status.success());
        text(&out.stderr)
    };
    let err = piped(&[]);
    assert!(
        err.starts_with("   Installed rok 9.9.4 to ") && !err.contains('\x1b'),
        "{err:?}"
    );
    let err = piped(&[("CLICOLOR_FORCE", "1"), ("COLORTERM", "truecolor")]);
    assert!(
        err.starts_with("\x1b[1;38;2;56;189;248m   Installed\x1b[0m "),
        "{err:?}"
    );
    let err = piped(&[("CLICOLOR_FORCE", "1"), ("NO_COLOR", "1")]);
    assert!(!err.contains('\x1b'), "{err:?}");
    // Errors: the label is there either way.
    let bad = |vars: &[(&str, &str)]| {
        let out = install_sh(&root, &root.join("user"), "/bin/sh")
            .args(["--archive", "missing.tar.gz"])
            .env("TERM", "xterm")
            .env_remove("NO_COLOR")
            .env_remove("COLORTERM")
            .envs(vars.iter().copied())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        text(&out.stderr)
    };
    assert!(bad(&[]).starts_with("install.sh: error: "), "{}", bad(&[]));
    let err = bad(&[("CLICOLOR_FORCE", "1")]);
    assert!(
        err.starts_with("install.sh: \x1b[1;91merror:\x1b[0m "),
        "{err:?}"
    );
}

/// `install.sh --archive` takes a release's `.tar.xz` as well; `-q`
/// prints nothing.
// spec: §9.10.6
#[cfg(unix)]
#[test]
fn install_sh_reads_tar_xz() {
    let root = scratch("install-sh-xz");
    let home = root.join("home");
    fake_toolchain(&root.join("stage/rok-9.9.2"), "9.9.2");
    let tar = Command::new("tar")
        .current_dir(root.join("stage"))
        .args(["-cJf", "../rok.tar.xz", "rok-9.9.2"])
        .status()
        .unwrap();
    assert!(tar.success(), "tar -cJf (tar needs xz)");
    let out = install_sh(&root, &root.join("user"), "/bin/sh")
        .args(["-q", "--archive", "rok.tar.xz"])
        .env("ROK_HOME", &home)
        .output()
        .unwrap();
    assert_eq!(ok(&out), "");
    assert_eq!(text(&out.stderr), "");
    let host = env!("ROK_HOST");
    assert!(
        home.join(format!("toolchains/9.9.2-{host}/bin/rokc"))
            .is_file()
    );
}

/// `rok toolchain update` against a local release server: the newest
/// release (`latest/download/rok-version`) or a given one, checked
/// against its `.sha256`, becomes the default with new proxies; a
/// toolchain whose proxies do not run is rolled back.
// spec: §9.10.6
#[cfg(unix)]
#[test]
fn update_toolchain() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("update-toolchain");
    let home = root.join("home");
    let host = env!("ROK_HOST");
    let dist = root.join("dist");
    let release = |v: &str, sum: Option<&str>, broken_rok: bool| {
        let stage = root.join("stage").join(v);
        let top = stage.join(format!("rok-{v}"));
        fake_toolchain(&top, v);
        if broken_rok {
            let rok = top.join("bin/rok");
            write(&rok, "#!/bin/sh\nexit 1\n");
            fs::set_permissions(&rok, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let name = format!("rok-{v}-{host}.tar.gz");
        let to = dist.join(format!("download/v{v}"));
        fs::create_dir_all(&to).unwrap();
        let tar = Command::new("tar")
            .current_dir(&stage)
            .arg("-czf")
            .arg(to.join(&name))
            .arg(format!("rok-{v}"))
            .status()
            .unwrap();
        assert!(tar.success());
        let sum = sum.map_or_else(|| sha256_of(&to.join(&name)), str::to_owned);
        write(
            &to.join(format!("{name}.sha256")),
            &format!("{sum}  {name}\n"),
        );
        write(&dist.join("latest/download/rok-version"), &format!("{v}\n"));
    };
    let url = format!("http://127.0.0.1:{}", serve_dir(dist.clone()));
    let update = |args: &[&str]| {
        let mut c = rok_cmd(&home, &root, &[&["toolchain", "update"][..], args].concat());
        c.env("ROK_DIST", &url);
        c.output().unwrap()
    };
    let proxy = |args: &[&str]| {
        let mut c = Command::new(home.join("bin/rokc"));
        c.current_dir(&root)
            .env_remove("ROK_HOME")
            .env_remove("ROK_TOOLCHAIN")
            .args(args);
        ok(&c.output().unwrap())
    };
    let list = || ok(&rok(&home, &root, &["toolchain", "list"]));

    fails(&update(&[]), "latest/download/rok-version");
    release("9.9.1", None, false);
    let out = update(&[]);
    ok(&out);
    let err = text(&out.stderr);
    assert!(
        err.starts_with(&format!(
            " Downloading rok 9.9.1 ({host})\n   Installed rok 9.9.1 to "
        )),
        "{err}"
    );
    assert_eq!(list(), format!("* 9.9.1-{host} (default)\n"));
    assert_eq!(proxy(&["x"]), "rokc 9.9.1: x\n");
    let out = update(&[]);
    assert_eq!(text(&out.stderr), "rok 9.9.1 is up to date\n");

    // Through the proxy, which updates itself and, quietly, the
    // completion scripts there are.
    release("9.10.0", None, false);
    let zsh = home.join("zsh/_rokc");
    write(&zsh, "old\n");
    let mut c = Command::new(home.join("bin/rok"));
    user_env(&mut c, &home.join("user"));
    c.current_dir(&root)
        .env_remove("ROK_HOME")
        .env_remove("ROK_TOOLCHAIN")
        .env("ROK_DIST", &url)
        .args(["toolchain", "update"]);
    let out = c.output().unwrap();
    ok(&out);
    assert_eq!(
        text(&out.stderr),
        format!(
            " Downloading rok 9.10.0 ({host})\n     Updated rok 9.9.1 -> 9.10.0\n        note: \
             9.9.1-{host} stays installed (`rok toolchain remove 9.9.1-{host}`)\n"
        )
    );
    assert_eq!(
        list(),
        format!("  9.9.1-{host}\n* 9.10.0-{host} (default)\n")
    );
    assert_eq!(proxy(&["x"]), "rokc 9.10.0: x\n");
    assert_eq!(read(&zsh), "rokc 9.10.0: completions zsh\n");

    // A damaged download is refused before anything changes.
    release("9.11.0", Some(&"0".repeat(64)), false);
    fails(&update(&[]), "its .sha256 says 0000");
    // Proxies that do not run the new toolchain are rolled back.
    release("9.12.0", None, true);
    fails(
        &update(&[]),
        "rolled back: the default toolchain and the proxies are as they were",
    );
    assert_eq!(
        list(),
        format!("  9.9.1-{host}\n* 9.10.0-{host} (default)\n")
    );
    assert_eq!(proxy(&["x"]), "rokc 9.10.0: x\n");
    let leftovers: Vec<_> = fs::read_dir(home.join("bin"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // A given version: the default already, or installed (no download).
    let out = update(&["9.10.0"]);
    assert_eq!(text(&out.stderr), "rok 9.10.0 is up to date\n");
    fails(
        &update(&["greet"]),
        "rok toolchain update: `greet` is not a version",
    );
    fs::remove_dir_all(dist.join("download/v9.9.1")).unwrap();
    let out = update(&["9.9.1", "-v"]);
    ok(&out);
    let err = text(&out.stderr);
    assert!(
        err.starts_with("     Updated rok 9.10.0 -> 9.9.1\n"),
        "{err}"
    );
    assert!(
        err.ends_with(&format!(
            "        note: refreshed zsh completions for rokc in {}\n",
            zsh.display()
        )),
        "{err}"
    );
    assert_eq!(read(&zsh), "rokc 9.9.1: completions zsh\n");
    assert_eq!(
        list(),
        format!("* 9.9.1-{host} (default)\n  9.10.0-{host}\n")
    );
    assert_eq!(proxy(&["x"]), "rokc 9.9.1: x\n");
    assert_eq!(update(&["9.9.1", "x"]).status.code(), Some(2));
    assert_eq!(update(&["--from", "x"]).status.code(), Some(2));
    // `clean` removes every installed toolchain but the default.
    assert_eq!(
        list(),
        format!("* 9.9.1-{host} (default)\n  9.10.0-{host}\n")
    );
    ok(&rok(&home, &root, &["toolchain", "clean"]));
    assert_eq!(list(), format!("* 9.9.1-{host} (default)\n"));
    assert_eq!(proxy(&["x"]), "rokc 9.9.1: x\n");
    // Idempotent: nothing left to remove.
    let out = rok_cmd(&home, &root, &["toolchain", "clean"])
        .output()
        .unwrap();
    ok(&out);
    assert!(
        text(&out.stderr).contains("nothing to clean"),
        "{}",
        text(&out.stderr)
    );
    for gone in ["self", "upgrade"] {
        fails(
            &rok(&home, &root, &[gone]),
            &format!("unknown command `{gone}`"),
        );
    }
}

/// `GET /<name>` of the files in `dir` on `http://127.0.0.1:<port>`
/// (the port); 404 for others.
#[cfg(unix)]
fn serve_dir(dir: PathBuf) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let mut conn = conn.unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut start = String::new();
            reader.read_line(&mut start).unwrap();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim_end().is_empty() {
                    break;
                }
            }
            let name = start
                .split(' ')
                .nth(1)
                .unwrap_or("/")
                .trim_start_matches('/');
            let (code, body) = fs::read(dir.join(name)).map_or((404, Vec::new()), |b| (200, b));
            let _ = write!(
                conn,
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = conn.write_all(&body);
        }
    });
    port
}

#[cfg(unix)]
fn sha256_of(path: &Path) -> String {
    rokc_core::hash::hex(&rokc_core::hash::sha256(&fs::read(path).unwrap()))
}

/// Native packages end to end on a local index: `yy` (a shared library)
/// and `zz` (a static one calling into `yy`) made from C with the
/// system `cc`, published, added, locked, linked and run; a missing
/// target, a changed archive, the offline cache and a `Dep::native(url:)`.
#[cfg(unix)]
#[test]
fn native_packages() {
    if !llvm() || Command::new("cc").arg("--version").output().is_err() {
        return;
    }
    let host = env!("ROK_HOST");
    let apple = rokc_target::TargetSpec::host().object_format == rokc_target::ObjectFormat::MachO;
    let root = scratch("native");
    let home = root.join("home");
    let index = root.join("index");
    commit(
        &index,
        &[("config.rokon", "Config { api: \"https://rokstones.io\" }\n")],
    );
    for kv in [
        ["user.name", "t"],
        ["user.email", "t@t"],
        ["commit.gpgsign", "false"],
    ] {
        git(&index, &["config", kv[0], kv[1]]);
    }
    let reg = index.display().to_string();
    let qreg = q(&reg);
    let config = format!("Config {{ registry: {qreg} }}\n");
    write(&home.join("config.rokon"), &config);

    // The archives: `lib/` and a license, packed by `tar`.
    let dist = root.join("dist");
    fs::create_dir_all(&dist).unwrap();
    let pack = |name: &str, c: &str, shared: bool| {
        let src = root.join(name);
        write(&src.join(format!("{name}.c")), c);
        write(&src.join("pkg/LICENSE"), "MIT\n");
        fs::create_dir_all(src.join("pkg/lib")).unwrap();
        let c_file = format!("{name}.c");
        if !shared {
            sh(&src, "cc", &["-c", "-fPIC", &c_file, "-o", "x.o"]);
            sh(&src, "ar", &["rcs", &format!("pkg/lib/lib{name}.a"), "x.o"]);
        } else if apple {
            let lib = format!("pkg/lib/lib{name}.dylib");
            let id = format!("@rpath/lib{name}.dylib");
            sh(
                &src,
                "cc",
                &["-dynamiclib", "-install_name", &id, "-o", &lib, &c_file],
            );
        } else {
            let lib = format!("pkg/lib/lib{name}.so");
            let so = format!("-Wl,-soname,lib{name}.so");
            sh(&src, "cc", &["-shared", "-fPIC", &so, "-o", &lib, &c_file]);
        }
        let tgz = dist.join(format!("{name}-1.0.0.tar.gz"));
        let tgz_arg = tgz.display().to_string();
        sh(&src, "tar", &["-czf", &tgz_arg, "-C", "pkg", "."]);
        sha256_of(&tgz)
    };
    let yy_sum = pack("yy", "int yy_mul(int a, int b) { return a * b; }\n", true);
    let zz_sum = pack(
        "zz",
        "int yy_mul(int, int);\nint zz_muladd(int a, int b, int c) { return yy_mul(a, b) + c; }\n",
        false,
    );
    let ext = if apple { "dylib" } else { "so" };
    let yy_meta = format!(
        "Native {{ name: \"yy\", vers: \"1.0.0\", targets: {{ \"{host}\": Artifact {{ url: \
         \"file://{}/yy-1.0.0.tar.gz\", sha256: \"{yy_sum}\", links: {{ \"yy\": \"dylib\" }}, \
         runtime: [\"lib/libyy.{ext}\"] }} }}, license: \"MIT\" }}\n",
        dist.display()
    );
    let zz_meta = format!(
        "Native {{ name: \"zz\", vers: \"1.0.0\", deps: {{ \"yy\": \"^1.0\" }}, targets: {{ \
         \"{host}\": Artifact {{ url: \"file://{}/zz-1.0.0.tar.gz\", sha256: \"{zz_sum}\", links: \
         {{ \"zz\": \"static\" }} }} }} }}\n",
        dist.display()
    );
    write(&root.join("yy.rokon"), &yy_meta);
    write(&root.join("zz.rokon"), &zz_meta);

    // `publish --native`: a dry run, then committed; one namespace per kind.
    let line = ok(&rok(&home, &root, &["publish", "--native", "yy.rokon"]));
    assert!(
        line.starts_with("Native { name: \"yy\", vers: \"1.0.0\", deps: {}, "),
        "{line}"
    );
    assert!(!index.join("native/2/yy").exists());
    for f in ["yy.rokon", "zz.rokon"] {
        let args = ["publish", "--native", f, "--index", &reg, "--commit"];
        ok(&rok(&home, &root, &args));
    }
    assert_eq!(read(&index.join("native/2/yy")), line);
    assert!(git(&index, &["log", "--format=%s"]).starts_with("Publish native zz 1.0.0\n"));
    fails(
        &rok(
            &home,
            &root,
            &["publish", "--native", "yy.rokon", "--index", &reg],
        ),
        "native package yy 1.0.0 is already in the index",
    );
    assert_eq!(
        ok(&rok(&home, &root, &["search", "--native", "y"])),
        "yy 1.0.0\n"
    );
    assert_eq!(ok(&rok(&home, &root, &["search", "y"])), "");
    native_yank(&home, &root, &index, &line);
    // No license, no publish.
    fs::remove_file(root.join("yy/pkg/LICENSE")).unwrap();
    let bad = dist.join("yy-2.0.0.tar.gz");
    let bad_arg = bad.display().to_string();
    sh(
        &root.join("yy"),
        "tar",
        &["-czf", &bad_arg, "-C", "pkg", "."],
    );
    let bad_meta = yy_meta
        .replace("1.0.0", "2.0.0")
        .replace(&yy_sum, &sha256_of(&bad));
    write(&root.join("bad.rokon"), &bad_meta);
    fails(
        &rok(&home, &root, &["publish", "--native", "bad.rokon"]),
        "the archive has no license file",
    );

    // An app calling both through `extern "C"` (no `#[link]`: the
    // packages name their libraries).
    let app = root.join("app");
    ok(&rok(&home, &root, &["new", "app"]));
    write(
        &app.join("src/main.rok"),
        "extern \"C\" {\n    fn zz_muladd(a: i32, b: i32, c: i32) -> i32;\n    fn yy_mul(a: i32, b: \
         i32) -> i32;\n}\nlet a = unsafe { zz_muladd(6, 7, 1) };\nlet b = unsafe { yy_mul(2, 3) \
         };\nprintln(f\"{a} {b}\");\n",
    );
    ok(&rok(&home, &app, &["add", "--native", "zz"]));
    let manifest = read(&app.join("rok.rokon"));
    assert!(
        manifest.contains("\"zz\": Dep::native(\"^1.0.0\")"),
        "{manifest}"
    );
    let lock = read(&app.join("rok.lock"));
    for n in ["yy", "zz"] {
        assert!(
            lock.contains(&format!(
                "LockedNative {{\n            name: \"{n}\",\n            vers: \"1.0.0\",\n"
            )),
            "{lock}"
        );
    }
    assert_eq!(ok(&rok(&home, &app, &["-q", "run"])), "43 6\n");
    // The shared library sits beside the executable, which finds it there
    // from anywhere.
    assert!(app.join(format!("target/debug/libyy.{ext}")).is_file());
    let run = Command::new(app.join("target/debug/app"))
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(ok(&run), "43 6\n");
    native_in_tools(&home, &root, &app, ext);

    // A target the packages have no build for.
    let other = if host == "x86_64-unknown-linux-gnu" {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    fails(
        &rok(&home, &app, &["build", "--target", other]),
        &format!("native package `zz` 1.0.0 has no build for {other} (it has: {host})"),
    );
    // A file named by url and hash; the artifact url relative to it.
    let vend = root.join("vend");
    fs::create_dir_all(&vend).unwrap();
    fs::copy(dist.join("yy-1.0.0.tar.gz"), vend.join("yy.tgz")).unwrap();
    let from = format!("file://{}/yy-1.0.0.tar.gz", dist.display());
    write(&vend.join("yy.rokon"), &yy_meta.replace(&from, "yy.tgz"));
    let direct = root.join("direct");
    write(
        &direct.join("src/main.rok"),
        "extern \"C\" { fn yy_mul(a: i32, b: i32) -> i32; }\nlet x = unsafe { yy_mul(4, 5) \
         };\nprintln(f\"{x}\");\n",
    );
    let manifest = |sum: &str| {
        format!(
            "Package {{ native: {{ \"yy\": Dep::native(url: \"../vend/yy.rokon\", sha256: \
             \"{sum}\") }} }}\n"
        )
    };
    write(&direct.join("rok.rokon"), &manifest(&"0".repeat(64)));
    fails(&rok(&home, &direct, &["-q", "run"]), "but 0000");
    let sum = sha256_of(&vend.join("yy.rokon"));
    write(&direct.join("rok.rokon"), &manifest(&sum));
    assert_eq!(ok(&rok(&home, &direct, &["-q", "run"])), "20\n");

    // Offline: what is cached builds without its sources.
    for f in ["yy-1.0.0.tar.gz", "zz-1.0.0.tar.gz"] {
        fs::remove_file(dist.join(f)).unwrap();
    }
    ok(&rok(&home, &app, &["-q", "clean"]));
    assert_eq!(ok(&rok(&home, &app, &["--offline", "-q", "run"])), "43 6\n");

    // A changed archive is refused (a fresh cache fetches it again).
    write(&dist.join("yy-1.0.0.tar.gz"), "not the archive");
    write(&dist.join("zz-1.0.0.tar.gz"), "not the archive");
    let home2 = root.join("home2");
    write(&home2.join("config.rokon"), &config);
    let out = rok(&home2, &app, &["-q", "build"]);
    fails(
        &out,
        &format!("native package `zz` 1.0.0 ({host}): file://"),
    );
    fails(&out, &format!("but {zz_sum} is expected"));

    // `remove --native` takes it out of rok.rokon and rok.lock.
    ok(&rok(&home, &app, &["remove", "--native", "zz"]));
    assert!(!read(&app.join("rok.lock")).contains("LockedNative"));
}

/// `native_packages`' app beyond `run`: a deleted runtime copy, `deps`,
/// checks and the REPL.
#[cfg(unix)]
fn native_in_tools(home: &Path, root: &Path, app: &Path, ext: &str) {
    // A deleted copy comes back on the next build.
    let copy = app.join(format!("target/debug/libyy.{ext}"));
    fs::remove_file(&copy).unwrap();
    ok(&rok(home, app, &["-q", "build"]));
    assert!(copy.is_file());

    // `deps` lists them: rokc's arguments under a `# native` line each.
    let deps = ok(&rok(home, app, &["deps"]));
    for want in [
        "# native zz 1.0.0\n-L",
        "-lstatic=zz\n# native yy 1.0.0\n-L",
        "-ldylib=yy\n--native-runtime=",
    ] {
        assert!(deps.contains(want), "{want:?} in {deps}");
    }
    // Checks link nothing.
    for c in ["check", "lint", "doc"] {
        ok(&rok(home, app, &["-q", c]));
    }
    // The REPL loads both: the shared library, then the static one. In
    // an environment, `:add --native` adds them to a running session.
    let repl = |args: &[&str], input: &str| {
        let mut child = rok_cmd(home, app, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).unwrap();
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        let stdout = ok(&out);
        assert!(
            stdout.contains("43\n6\n"),
            "{stdout}\n{}",
            text(&out.stderr)
        );
    };
    let cells = "extern \"C\" {\n    fn zz_muladd(a: i32, b: i32, c: i32) -> i32;\n    fn \
                 yy_mul(a: i32, b: i32) -> i32;\n}\nunsafe { zz_muladd(6, 7, 1) }\nunsafe { \
                 yy_mul(2, 3) }\n";
    repl(&["-q", "repl"], cells);
    ok(&rok(home, root, &["env", "create", "n"]));
    repl(&["--env", "n"], &format!(":add --native zz\n{cells}"));
}

/// `yank --native` hides a version from resolution; `--undo` brings it
/// back (to `line`).
#[cfg(unix)]
fn native_yank(home: &Path, root: &Path, index: &Path, line: &str) {
    let reg = index.display().to_string();
    let yank = ["yank", "--native", "--vers", "1.0.0", "--index", &reg, "yy"];
    ok(&rok(home, root, &yank));
    assert!(read(&index.join("native/2/yy")).contains("yanked: true"));
    assert!(git(index, &["log", "--format=%s"]).starts_with("Yank native yy 1.0.0\n"));
    assert_eq!(ok(&rok(home, root, &["search", "--native", "y"])), "");
    ok(&rok(home, root, &[&yank[..], &["--undo"]].concat()));
    assert_eq!(read(&index.join("native/2/yy")), line);
    fails(&rok(home, root, &yank[..6]), "usage: rok yank");
}

/// `rok deps` as `name[features]` per package, in build order.
fn enabled(home: &Path, cwd: &Path, args: &[&str]) -> String {
    let mut all = vec!["deps"];
    all.extend(args);
    let out = ok(&rok(home, cwd, &all));
    let mut parts: Vec<(String, Vec<String>)> = Vec::new();
    for l in out.lines() {
        if let Some(v) = l.strip_prefix("--extern-feature=") {
            let (n, f) = v.split_once('=').unwrap();
            let last = parts.last_mut().unwrap();
            assert_eq!(last.0, n, "{out}");
            last.1.push(f.to_owned());
        } else if let Some((n, _)) = l.split_once('=') {
            parts.push((n.to_owned(), Vec::new()));
        }
    }
    let shown: Vec<String> = parts
        .iter()
        .map(|(n, f)| format!("{n}[{}]", f.join(",")))
        .collect();
    shown.join(" ")
}

const TLS: &str = "#[cfg(feature = \"fast\")]\npub fn speed() -> i32 { return 2; }\n\
                   #[cfg(not(feature = \"fast\"))]\npub fn speed() -> i32 { return 1; }\n";
const HTTP: &str = "#[cfg(feature = \"tls\")]\npub fn port() -> i32 { return 443; }\n\
                    #[cfg(not(feature = \"tls\"))]\npub fn port() -> i32 { return 80; }\n";
const APP_FEATURES: &str = "Package {
    name: \"app\",
    deps: {
        \"http\": Dep::path(\"../http\"),
        \"cli\": Dep::path(\"../cli\"),
        \"extra\": Dep::path(\"../extra\", optional: true),
    },
    dev_deps: { \"tls\": Dep::path(\"../tls\", features: [\"fast\"]) },
    features: {
        \"default\": [\"web\"],
        \"web\": [\"http/tls\"],
        \"full\": [\"web\", \"http/fast\"],
        \"opt\": [\"dep:extra\"],
    },
}
";

/// Features: the root's default and `--features` / `--all-features` /
/// `--no-default-features`, optional dependencies enabled by `dep:` and
/// `<dep>/<f>`, `<dep>?/<f>`, one set per package for the whole graph,
/// dev dependencies' only in a test build, errors, `cfg(feature)` in the
/// code, and rebuilds when a package's features change.
#[test]
fn features_pick_packages_and_cfg() {
    let root = scratch("features");
    let home = root.join("home");
    let pkg = |name: &str, manifest: &str, lib: &str| {
        write(&root.join(name).join("rok.rokon"), manifest);
        write(&root.join(name).join("src/lib.rok"), lib);
    };
    pkg(
        "tls",
        "Package { name: \"tls\", features: { \"default\": [\"ring\"], \"ring\": [], \"fast\": [] } }\n",
        TLS,
    );
    pkg(
        "json",
        "Package { name: \"json\", features: { \"pretty\": [], \"compact\": [] } }\n",
        ONE,
    );
    pkg(
        "http",
        "Package {\n    name: \"http\",\n    deps: {\n        \"tls\": Dep::path(\"../tls\", \
         optional: true),\n        \"json\": Dep::path(\"../json\"),\n    },\n    features: { \
         \"tls\": [\"dep:tls\"], \"fast\": [\"tls?/fast\"], \"pretty\": [\"json/pretty\"] },\n}\n",
        HTTP,
    );
    let cli = "Package {\n    name: \"cli\",\n    deps: {\n        \"http\": \
               Dep::path(\"../http\", features: [\"pretty\"]),\n        \"json\": \
               Dep::path(\"../json\", features: [\"compact\"]),\n    },\n}\n";
    pkg("cli", cli, ONE);
    pkg("extra", "Package { name: \"extra\" }\n", ONE);
    let app = root.join("app");
    write(&app.join("rok.rokon"), APP_FEATURES);
    write(
        &app.join("src/main.rok"),
        "let web = cfg!(feature = \"web\");\nprintln(f\"{http::port()} {web}\");\n",
    );
    let e = |args: &[&str]| enabled(&home, &app, args);
    // `default` is `web`: `http/tls` enables the optional `tls` (with its
    // own default). `json` has what `http` and `cli` ask, once.
    assert_eq!(
        e(&[]),
        "tls[default,ring] json[compact,pretty] http[pretty,tls] cli[]"
    );
    assert_eq!(
        e(&["--no-default-features"]),
        "json[compact,pretty] http[pretty] cli[]"
    );
    // `tls?/fast` waits for something else to enable `tls`.
    assert_eq!(
        e(&["--no-default-features", "--features", "http/fast"]),
        "json[compact,pretty] http[fast,pretty] cli[]"
    );
    assert_eq!(
        e(&["--features=full"]),
        "tls[default,fast,ring] json[compact,pretty] http[fast,pretty,tls] cli[]"
    );
    // `dep:extra`.
    assert_eq!(
        e(&["--no-default-features", "--features", "opt,http/tls"]),
        "tls[default,ring] json[compact,pretty] http[pretty,tls] cli[] extra[]"
    );
    assert_eq!(
        e(&["--all-features"]),
        "tls[default,fast,ring] json[compact,pretty] http[fast,pretty,tls] cli[] extra[]"
    );
    // A dev dependency's features count in a test build only.
    assert_eq!(
        e(&["--dev", "--no-default-features"]),
        "tls[default,fast,ring] json[compact,pretty] http[pretty] cli[]"
    );
    fails(
        &rok(&home, &app, &["deps", "--features", "nope"]),
        "the root package has no feature `nope` (required by --features)",
    );
    fails(
        &rok(&home, &app, &["deps", "--features", "http/nope"]),
        "package `http` has no feature `nope` (required by --features)",
    );
    fails(
        &rok(&home, &app, &["deps", "--features", "zip/x"]),
        "the root package has no dependency `zip`",
    );
    write(
        &root.join("cli/rok.rokon"),
        &cli.replace("[\"compact\"]", "[\"bogus\"]"),
    );
    fails(
        &rok(&home, &app, &["deps"]),
        "package `json` has no feature `bogus` (required by package `cli`)",
    );
    write(&root.join("cli/rok.rokon"), cli);
    write(
        &app.join("rok.rokon"),
        &APP_FEATURES
            .replace("\"web\", \"http/fast\"", "\"web\", \"loop\"")
            .replace("\"opt\": [", "\"loop\": [\"full\"],\n        \"opt\": ["),
    );
    let out = rok(&home, &app, &["deps"]);
    fails(&out, "feature cycle: full -> loop -> full");
    assert!(
        text(&out.stderr).contains("rok.rokon:13:"),
        "{}",
        text(&out.stderr)
    );
    write(&app.join("rok.rokon"), APP_FEATURES);

    // `rok add --optional`: the dependency and a feature enabling it.
    ok(&rok(
        &home,
        &app,
        &[
            "add",
            "extra2",
            "--path",
            "../extra",
            "--optional",
            "--features",
            "a,b",
            "--no-default-features",
        ],
    ));
    let m = read(&app.join("rok.rokon"));
    assert!(
        m.contains(
            "\"extra2\": Dep::path(\n            \"../extra\",\n            optional: true,\n            \
             features: [\"a\", \"b\"],\n            default_features: false,\n        ),"
        ) && m.contains("\"extra2\": [\"dep:extra2\"],"),
        "{m}"
    );
    write(&app.join("rok.rokon"), APP_FEATURES);

    if !llvm() {
        return;
    }
    // The code sees its package's features; a change rebuilds, the same
    // features again do not.
    let run = |args: &[&str]| {
        let out = rok(&home, &app, &[&["run"], args].concat());
        (ok(&out), text(&out.stderr))
    };
    let (out, err) = run(&[]);
    assert_eq!(out, "443 true\n", "{err}");
    let (out, err) = run(&["--no-default-features"]);
    assert_eq!(out, "80 false\n");
    assert!(err.contains("Compiling http "), "{err}");
    let (out, err) = run(&["--no-default-features"]);
    assert_eq!(out, "80 false\n");
    assert!(!err.contains("Compiling"), "{err}");
}

/// Features in a registry index: `publish` writes them, resolution reads
/// them; `rok add --features --no-default-features` writes the
/// dependency so; the lock holds optional stones whatever is enabled.
#[test]
fn stone_features_round_trip() {
    let root = scratch("stone-features");
    let home = root.join("home");
    let index = root.join("index");
    commit(
        &index,
        &[("config.rokon", "Config { api: \"https://rokstones.io\" }\n")],
    );
    for kv in [
        ["user.name", "t"],
        ["user.email", "t@t"],
        ["commit.gpgsign", "false"],
    ] {
        git(&index, &["config", kv[0], kv[1]]);
    }
    let reg = index.display().to_string();
    let qreg = q(&reg);
    write(
        &home.join("config.rokon"),
        &format!("Config {{ registry: {qreg} }}\n"),
    );
    let num = root.join("num");
    stone_meta(
        &num,
        "num",
        "1.0.0",
        "",
        "features: { \"big\": [] }, ",
        &[("src/lib.rok", ONE)],
    );
    ok(&rok(&home, &num, &["publish", "--index", &reg, "--commit"]));
    let codec = root.join("codec");
    stone_meta(
        &codec,
        "codec",
        "1.0.0",
        "\"num\": Dep::version(\"^1.0\", optional: true, features: [\"big\"], default_features: \
         false)",
        "features: { \"default\": [\"std\"], \"std\": [], \"num\": [\"dep:num\"] }, ",
        &[("src/lib.rok", ONE)],
    );
    let line = ok(&rok(
        &home,
        &codec,
        &["publish", "--index", &reg, "--commit"],
    ));
    assert!(
        line.contains(
            "deps: [{ name: \"num\", req: \"^1.0\", kind: \"normal\", optional: true, features: \
             [\"big\"], default_features: false }], features: { \"default\": [\"std\"], \"std\": \
             [], \"num\": [\"dep:num\"] }, source: Dep::git("
        ),
        "{line}"
    );
    assert_eq!(read(&index.join("co/de/codec")), line);

    let app = root.join("app");
    write(&app.join("src/main.rok"), "println(1);\n");
    write(&app.join("rok.rokon"), APP0);
    ok(&rok(
        &home,
        &app,
        &["add", "codec", "--features", "num", "--no-default-features"],
    ));
    let m = read(&app.join("rok.rokon"));
    assert!(
        m.contains(
            "\"codec\": Dep::version(\"^1.0.0\", features: [\"num\"], default_features: false),"
        ),
        "{m}"
    );
    assert_eq!(enabled(&home, &app, &[]), "num[big] codec[num]");
    let lock = read(&app.join("rok.lock"));
    assert_eq!(locked_stones(&lock), ["codec@1.0.0", "num@1.0.0"]);
    write(
        &app.join("rok.rokon"),
        &APP0.replace("// deps go here", "\"codec\": \"^1.0\","),
    );
    assert_eq!(enabled(&home, &app, &[]), "codec[default,std]");
    // `num` stays pinned: the lock covers every optional stone.
    assert_eq!(read(&app.join("rok.lock")), lock);

    // Index features are checked against the stone's rok.rokon at its
    // commit, in any order.
    let entry = read(&index.join("3/n/num"));
    let forged = entry.replace(
        "features: { \"big\": [] }",
        "features: { \"big\": [], \"fast\": [] }",
    );
    assert_ne!(forged, entry);
    commit(&index, &[("3/n/num", &forged)]);
    let home2 = root.join("home2");
    write(
        &home2.join("config.rokon"),
        &format!("Config {{ registry: {qreg} }}\n"),
    );
    let app2 = root.join("app2");
    write(&app2.join("src/main.rok"), "println(1);\n");
    write(
        &app2.join("rok.rokon"),
        &APP0.replace("// deps go here", "\"num\": \"^1.0\","),
    );
    fails(
        &rok(&home2, &app2, &["deps"]),
        "stone `num` 1.0.0: the index's features differ from its rok.rokon's at commit",
    );
}

/// A closed stdout ends `rok` quietly (`rok help | head`): killed by
/// SIGPIPE (Windows: its own status, std dropping the writes), not an
/// internal error (101).
#[test]
// spec: §9.10.1
fn closed_output_ends_rok_quietly() {
    // A pipe whose reader has exited: `rok version` never reads stdin.
    let mut reader = Command::new(ROK)
        .arg("version")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pipe = reader.stdin.take().unwrap();
    assert!(reader.wait().unwrap().success());
    let out = Command::new(ROK)
        .arg("help")
        .stdout(pipe)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&out.status),
        Some(13),
        "{stderr}"
    );
    // Windows' std drops writes to a pipe whose reader left: `help` ends
    // as it would.
    #[cfg(not(unix))]
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(stderr, "");
}

/// `rok -q <args>`'s stdout.
#[track_caller]
fn script_out(home: &Path, cwd: &Path, args: &[&str]) -> String {
    let mut all = vec!["-q"];
    all.extend_from_slice(args);
    ok(&rok(home, cwd, &all))
}

/// `rok install`: a library goes into the active environment, for the
/// REPL; neither scripts nor packages see it. An executable package or a
/// script file is built into `$ROK_HOME/bin`; `uninstall` undoes either.
// spec: §9.10.5 §9.10.7
#[test]
fn install_libraries_and_executables() {
    if !llvm() {
        return;
    }
    let root = scratch("install");
    let home = root.join("home");
    let greet = root.join("greet");
    write(&greet.join("rok.rokon"), "Package { name: \"greet\" }\n");
    write(&greet.join("src/lib.rok"), LIB_V1);
    let script = "use greet::greet;\nprintln(f\"{greet()}\");\n";
    write(&root.join("s.rok"), script);

    // A library: the package's own name, absolute in the environment,
    // prebuilt into its target/.
    ok(&rok(&home, &root, &["install", "--path", "greet"]));
    let manifest = read(&home.join("envs/default/rok.rokon"));
    let src = q(greet.display().to_string().replace('\\', "/"));
    assert!(
        manifest.contains(&format!("\"greet\": Dep::path({src})")),
        "{manifest}"
    );
    assert!(
        home.join("envs/default/target/debug/deps/libgreet.a")
            .is_file()
    );
    // The REPL's dependencies (`rok deps` outside a package).
    assert_eq!(deps(&home, &root, &[])[0].0, "greet");
    // A script does not see it: the help is the command that adds it,
    // and a note says why the installed one does not count.
    let out = rok(&home, &root, &["-q", "s.rok"]);
    fails(&out, "= help: run `rok add greet s.rok`");
    fails(
        &out,
        "= note: `greet` is installed in environment `default`, but a script uses only the \
         libraries of its own `//! rokon` block (installed libraries are for `rok repl` / `rok \
         jupyter`)",
    );
    // `import greet`: the library's `rok add`.
    write(&root.join("i.rok"), "import greet\n");
    fails(
        &rok(&home, &root, &["check", "i.rok"]),
        "= help: to use library `greet`, run `rok add greet i.rok`, then write `use greet::f;`",
    );

    // A library that does not build is not installed.
    write(
        &root.join("broken/src/lib.rok"),
        "pub fn f() -> i32 { return \"s\"; }\n",
    );
    fails(
        &rok(&home, &root, &["install", "--path", "broken"]),
        "`broken` did not build in environment `default`; nothing was installed",
    );
    assert_eq!(read(&home.join("envs/default/rok.rokon")), manifest);

    // A package does not see it, with a manifest or without.
    for (dir, files) in [
        ("app", &[("rok.rokon", "Package { name: \"app\" }\n")][..]),
        ("bare", &[][..]),
    ] {
        let app = root.join(dir);
        write(&app.join("src/main.rok"), script);
        for (f, t) in files {
            write(&app.join(f), t);
        }
        fails(
            &rok(&home, &app, &["-q", "build"]),
            "= help: `greet` is not a dependency of this package: add it to rok.rokon (`rok add \
             greet`)",
        );
    }
    fails(
        &rok(&home, &root.join("app"), &["--env", "default", "build"]),
        "`--env default` is for scripts and the REPL: a package builds with the dependencies in \
         its rok.rokon only (add one: `rok add <name>`)",
    );

    // Executables: a package with src/main.rok, from a path and from git
    // (built from a copy: the cached checkout stays as it was), and a
    // script file.
    let hello = root.join("hello");
    write(&hello.join("rok.rokon"), "Package { name: \"hello\" }\n");
    write(&hello.join("src/main.rok"), "println(\"hello\");\n");
    ok(&rok(&home, &root, &["install", "--path", "hello"]));
    let tool = root.join("tool");
    let commit = commit(&tool, &[("src/main.rok", "println(\"tool\");\n")]);
    ok(&rok(
        &home,
        &root,
        &["install", "--git", &tool.display().to_string()],
    ));
    write(&root.join("hi.rok"), "println(\"hi\");\n");
    ok(&rok(&home, &root, &["install", "--path", "hi.rok"]));
    for (name, want) in [("hello", "hello\n"), ("tool", "tool\n"), ("hi", "hi\n")] {
        let out = Command::new(home.join("bin").join(exe(name)))
            .output()
            .unwrap();
        assert_eq!(ok(&out), want);
    }
    assert!(hello.join("target/release").is_dir());
    assert!(
        !home
            .join("cache/git/checkouts")
            .join(&commit)
            .join("target")
            .exists()
    );
    let list = ok(&rok(&home, &root, &["install", "--list"]));
    let want = format!(
        "libraries (environment `default`):\n  greet Dep::path({src})\nexecutables ({}):\n  hello \
         Dep::path({})\n  tool Dep::git({}, rev: \"{commit}\")\n  hi Dep::path({})\n",
        home.join("bin").display(),
        q(hello.display().to_string().replace('\\', "/")),
        q(tool.display()),
        q(root.join("hi.rok").display().to_string().replace('\\', "/")),
    );
    assert_eq!(list, want);
    // The same commit again is kept; --force or a new commit rebuilds; a
    // path always does.
    let url = tool.display().to_string();
    let at = |c: &str| format!("{url} at {}", &c[..12]);
    let again = rok(&home, &root, &["install", "--git", &url]);
    assert!(
        text(&again.stderr).contains(&format!(
            "`tool` {} is installed already ({}); to rebuild it, run `rok install --git {url} \
             --force`",
            at(&commit),
            home.join("bin").join(exe("tool")).display()
        )),
        "{}",
        text(&again.stderr)
    );
    let forced = rok(&home, &root, &["install", "--git", &url, "--force"]);
    assert!(
        text(&forced.stderr).contains(&format!(" Reinstalled tool {}", at(&commit))),
        "{}",
        text(&forced.stderr)
    );
    let commit2 = crate::commit(&tool, &[("src/main.rok", "println(\"tool 2\");\n")]);
    let replaced = rok(&home, &root, &["install", "--git", &url]);
    assert!(
        text(&replaced.stderr).contains(&format!(
            "    Replaced tool {} with {}",
            at(&commit),
            at(&commit2)
        )),
        "{}",
        text(&replaced.stderr)
    );
    let out = Command::new(home.join("bin").join(exe("tool")))
        .output()
        .unwrap();
    assert_eq!(ok(&out), "tool 2\n");
    let rebuilt = rok(&home, &root, &["install", "--path", "hello"]);
    assert!(
        text(&rebuilt.stderr).contains(" Reinstalled hello "),
        "{}",
        text(&rebuilt.stderr)
    );
    fails(
        &rok(&home, &root, &["install", "rokc", "--path", "hello"]),
        "`rokc` is the toolchain's own executable",
    );
    write(&home.join("bin").join(exe("mine")), "");
    fails(
        &rok(&home, &root, &["install", "mine", "--path", "hello"]),
        "and `rok install` did not put it there; remove it first",
    );

    // `uninstall`: executables, then the library.
    ok(&rok(&home, &root, &["uninstall", "hello", "hi"]));
    assert!(!home.join("bin").join(exe("hello")).exists());
    assert!(home.join("bin").join(exe("tool")).is_file());
    let list = ok(&rok(&home, &root, &["install", "--list"]));
    assert!(
        !list.contains("hello") && list.contains("  tool "),
        "{list}"
    );
    ok(&rok(&home, &root, &["uninstall", "greet"]));
    let out = rok(&home, &root, &["-q", "s.rok"]);
    fails(&out, "= help: run `rok add greet s.rok`");
    assert!(
        !text(&out.stderr).contains("note:"),
        "{}",
        text(&out.stderr)
    );
    fails(
        &rok(&home, &root, &["uninstall", "greet"]),
        "`greet` is not installed: environment `default` has no library `greet`",
    );
    fails(
        &rok(&home, &root, &["uninstall", "mine"]),
        "`mine` is not installed",
    );
}

/// `//! rokon` block lines for `Package { deps: { <deps> } }`, then `body`.
fn block(deps: &str, body: &str) -> String {
    format!("//! ```rokon\n//! Package {{ deps: {{ {deps} }} }}\n//! ```\n{body}")
}

/// A script uses only the `//! rokon` blocks of the files it reaches (its
/// own and those its `mod`s load, each path relative to its file),
/// merged; never the environment's libraries. `rok init --script` makes
/// it a package.
// spec: §4.16.8 §9.10.2 §9.10.5 §9.10.7
#[test]
fn scripts_use_their_blocks() {
    if !llvm() {
        return;
    }
    let root = scratch("script-blocks");
    let home = root.join("home");
    for (dir, lib) in [("greet", LIB_V1), ("greet2", LIB_V2)] {
        write(
            &root.join(dir).join("rok.rokon"),
            "Package { name: \"greet\" }\n",
        );
        write(&root.join(dir).join("src/lib.rok"), lib);
    }
    write(
        &root.join("util/src/lib.rok"),
        "pub fn ten() -> i32 { return 10; }\n",
    );
    // The environment's `greet` is not the script's.
    ok(&rok(&home, &root, &["install", "--path", "greet"]));
    let uses = "use greet::greet;\nprintln(f\"{greet()}\");\n";
    write(
        &root.join("s.rok"),
        &block("\"greet\": Dep::path(\"greet2\")", uses),
    );
    assert_eq!(script_out(&home, &root, &["s.rok"]), "2\n");
    assert_eq!(
        script_out(&home, &root, &["--env", "default", "s.rok"]),
        "2\n"
    );

    // `mod helper;` loads app/helper.rok; its `mod deep;` app/helper/deep.rok,
    // whose block's path is relative to it.
    let app = root.join("app");
    write(
        &app.join("hi.rok"),
        &block(
            "\"greet\": Dep::path(\"../greet\")",
            "mod helper;\nuse greet::greet;\nprintln(f\"{greet() + helper::ten()}\");\n",
        ),
    );
    let helper = "mod deep;\npub fn ten() -> i32 { return deep::ten(); }\n";
    write(&app.join("helper.rok"), helper);
    let deep = |n: i32| {
        block(
            "\"util\": Dep::path(\"../../util\")",
            &format!("use util::ten as t;\npub fn ten() -> i32 {{ return t() * {n}; }}\n"),
        )
    };
    write(&app.join("helper/deep.rok"), &deep(1));
    assert_eq!(script_out(&home, &root, &["app/hi.rok"]), "11\n");
    let names = |file: &str| -> Vec<String> {
        ok(&rok(&home, &root, &["deps", file]))
            .lines()
            .filter_map(|l| Some(l.split_once('=')?.0.to_owned()))
            .collect()
    };
    let mut both = names("app/hi.rok");
    both.sort();
    assert_eq!(both, ["greet", "util"]);
    // Run on its own, a file has only the blocks it reaches.
    assert_eq!(names("app/helper/deep.rok"), ["util"]);
    // A helper's change is a new build.
    write(&app.join("helper/deep.rok"), &deep(2));
    assert_eq!(script_out(&home, &root, &["app/hi.rok"]), "21\n");
    // Every block is part of the cache key.
    let count = || fs::read_dir(home.join("cache/scripts")).unwrap().count();
    let before = count();
    write(
        &app.join("helper.rok"),
        &block("\"greet\": Dep::path(\"../greet\")", helper),
    );
    assert_eq!(script_out(&home, &root, &["app/hi.rok"]), "21\n");
    assert_eq!(count(), before + 1);
    // Two blocks, two requirements of one library.
    write(
        &app.join("helper.rok"),
        &block("\"greet\": Dep::path(\"../greet2\")", helper),
    );
    let sep = std::path::MAIN_SEPARATOR;
    fails(
        &rok(&home, &root, &["app/hi.rok"]),
        &format!(
            "`greet` is required as Dep::path(\"../greet\") in app{sep}hi.rok and as \
             Dep::path(\"../greet2\") in app{sep}helper.rok: use one version, the same entry in \
             both blocks (or keep it in one of them)"
        ),
    );
    write(&app.join("helper.rok"), helper);

    // To a package: rok.rokon from every block, the files under src/
    // without theirs.
    ok(&rok(&home, &root, &["init", "--script", "app/hi.rok"]));
    assert_eq!(
        read(&app.join("rok.rokon")),
        "Package {\n    name: \"hi\",\n    version: \"0.1.0\",\n    deps: { \"greet\": \
         Dep::path(\"../greet\"), \"util\": Dep::path(\"../util\") },\n}\n"
    );
    assert!(!app.join("hi.rok").exists() && !app.join("helper").exists());
    assert_eq!(read(&app.join("src/helper.rok")), helper);
    assert!(read(&app.join("src/main.rok")).starts_with("mod helper;\n"));
    assert!(read(&app.join("src/helper/deep.rok")).starts_with("use util::ten as t;\n"));
    assert_eq!(script_out(&home, &app, &["run"]), "21\n");
    fails(
        &rok(&home, &root, &["init", "--script", "s.rok", "x"]),
        "usage: rok init --script <file.rok>",
    );
}

/// `rok add` / `remove` with a script edit its `//! rokon` block: made
/// after the shebang and doc lines when missing, removed when empty, a
/// path relative to the script. `import x` says what to write.
// spec: §9.10.4 §9.10.7
#[test]
fn script_add_and_remove() {
    let root = scratch("script-add");
    let home = root.join("home");
    write(&root.join("lib1/src/lib.rok"), LIB_V1);
    write(&root.join("util/src/lib.rok"), LIB_V2);
    fs::create_dir_all(root.join("elsewhere")).unwrap();
    let plain = "#!/usr/bin/env rok\n//! Greets.\nuse greet::greet;\nprintln(f\"{greet()}\");\n";
    let file = root.join("s.rok");
    write(&file, plain);
    ok(&rok(
        &home,
        &root,
        &["add", "greet", "--path", "lib1", "s.rok"],
    ));
    let one = "#!/usr/bin/env rok\n//! Greets.\n//! ```rokon\n//! Package {\n//!     deps: {\n\
               //!         \"greet\": Dep::path(\"lib1\"),\n//!     },\n//! }\n//! ```\n\
               use greet::greet;\nprintln(f\"{greet()}\");\n";
    assert_eq!(read(&file), one);
    if llvm() {
        assert_eq!(script_out(&home, &root, &["s.rok"]), "1\n");
    }
    // Again: the same text.
    ok(&rok(
        &home,
        &root,
        &["add", "greet", "--path", "lib1", "s.rok"],
    ));
    assert_eq!(read(&file), one);
    // From elsewhere: the path is relative to the script.
    ok(&rok(
        &home,
        &root.join("elsewhere"),
        &["add", "util", "--path", "../util", "../s.rok"],
    ));
    assert!(read(&file).contains("//!         \"util\": Dep::path(\"util\"),\n"));
    // Removing both gives the file back.
    ok(&rok(&home, &root, &["remove", "util", "s.rok"]));
    assert_eq!(read(&file), one);
    ok(&rok(&home, &root, &["remove", "greet", "s.rok"]));
    assert_eq!(read(&file), plain);
    fails(
        &rok(&home, &root, &["remove", "greet", "s.rok"]),
        "`greet` is not a dependency in the block of s.rok",
    );
    // A block of the user's keeps its layout and comments.
    let mine = "//! ```rokon\n//! Package { deps: { \"greet\": Dep::path(\"lib1\") } } // mine\n\
                //! ```\nprintln(1);\n";
    write(&file, mine);
    ok(&rok(
        &home,
        &root,
        &["add", "util", "--path", "util", "s.rok"],
    ));
    let text = read(&file);
    assert!(
        text.contains("// mine") && text.contains("\"util\": Dep::path(\"util\")"),
        "{text}"
    );
    // What does not resolve is not written; a block holds only `deps`.
    write(&file, plain);
    fails(
        &rok(&home, &root, &["add", "x", "--path", "elsewhere", "s.rok"]),
        "is not a library package: no src/lib.rok",
    );
    assert_eq!(read(&file), plain);
    fails(
        &rok(
            &home,
            &root,
            &["add", "greet", "--dev", "--path", "lib1", "s.rok"],
        ),
        "a script's block holds only `deps`: no --dev",
    );
    fails(
        &rok(
            &home,
            &root,
            &["add", "greet", "--path", "lib1", "nope.rok"],
        ),
        "`nope.rok` does not exist",
    );

    // `import x` (or `include` / `require`): `mod` for x.rok beside the
    // file; the `use` hint for a name the file knows; else `rok add`.
    write(&root.join("helper.rok"), "pub fn f() {}\n");
    write(&root.join("i.rok"), "import helper\n");
    fails(
        &rok(&home, &root, &["check", "i.rok"]),
        "= help: to load helper.rok, write `mod helper;` then call `helper::f()` or `use helper::f;`",
    );
    for known in ["import rok\n", "mod geo { pub fn f() {} }\ninclude geo\n"] {
        write(&root.join("i.rok"), known);
        fails(
            &rok(&home, &root, &["check", "i.rok"]),
            "= help: did you mean `use`?",
        );
    }
    write(&root.join("i.rok"), "require json\n");
    fails(
        &rok(&home, &root, &["check", "i.rok"]),
        "= help: to use library `json`, run `rok add json i.rok`, then write `use json::f;`",
    );
}

/// `rok install <stone>` from a local index: into the environment, pinned
/// in its lock (a stone is a library).
// spec: §9.10.4a §9.10.5
#[test]
fn install_stones() {
    if !llvm() {
        return;
    }
    let root = scratch("install-stones");
    let home = root.join("home");
    let index = root.join("index");
    commit(
        &index,
        &[("config.rokon", "Config { api: \"https://rokstones.io\" }\n")],
    );
    for kv in [
        ["user.name", "t"],
        ["user.email", "t@t"],
        ["commit.gpgsign", "false"],
    ] {
        git(&index, &["config", kv[0], kv[1]]);
    }
    let reg = index.display().to_string();
    write(
        &home.join("config.rokon"),
        &format!("Config {{ registry: {} }}\n", q(&reg)),
    );
    let num = root.join("num");
    stone(&num, "num", "1.0.0", "", &[("src/lib.rok", ONE)]);
    ok(&rok(&home, &num, &["publish", "--index", &reg, "--commit"]));

    ok(&rok(&home, &root, &["install", "num"]));
    let env = home.join("envs/default");
    assert!(read(&env.join("rok.rokon")).contains("\"num\": Dep::version(\"^1.0.0\")"));
    assert_eq!(locked_stones(&read(&env.join("rok.lock"))), ["num@1.0.0"]);
    ok(&rok(&home, &root, &["uninstall", "num"]));
    assert!(locked_stones(&read(&env.join("rok.lock"))).is_empty());
    // A script gets a stone into its block, pinned as a package's is.
    let file = root.join("s.rok");
    write(&file, "use num::one;\nprintln(f\"{one()}\");\n");
    ok(&rok(&home, &root, &["add", "num", "s.rok"]));
    assert!(read(&file).contains("//!         \"num\": Dep::version(\"^1.0.0\"),\n"));
    assert_eq!(script_out(&home, &root, &["s.rok"]), "1\n");
    // `import num`: a library name.
    write(&root.join("i.rok"), "import num\n");
    fails(
        &rok(&home, &root, &["check", "i.rok"]),
        "= help: to use library `num`, run `rok add num i.rok`, then write `use num::f;`",
    );
}

/// Names code can't spell as a path are refused: `use pkg::add;` in an
/// executable is its own root, never a library called `pkg`.
// spec: §9.10.3 §5.5.7
#[test]
fn new_rejects_names_code_cannot_spell() {
    let root = scratch("new-names");
    let home = root.join("home");
    for (name, why) in [
        ("pkg", "`pkg` already names a package's own root (`pkg::`)"),
        ("sup", "`sup` already names the parent module (`sup::`)"),
        ("match", "`match` is a keyword"),
        ("self", "`self` is a keyword"),
    ] {
        let out = rok(&home, &root, &["new", "--lib", "--bin", name]);
        fails(&out, why);
        assert!(text(&out.stderr).contains(&format!("e.g. `{name}_util`")));
        assert!(!root.join(name).exists());
    }
    write(
        &root.join("named-pkg/rok.rokon"),
        "Package { name: \"pkg\", version: \"0.1.0\", deps: {} }\n",
    );
    write(&root.join("named-pkg/src/main.rok"), "println(\"hi\");\n");
    fails(
        &rok(&home, &root.join("named-pkg"), &["build"]),
        "`pkg` already names a package's own root",
    );
}

/// Every `rok new` template builds, runs and tests as written.
// spec: §0.3.1 §9.10.2
#[test]
fn new_templates_build_run_and_test() {
    if !llvm() {
        return;
    }
    let root = scratch("new-templates");
    let home = root.join("home");
    for (args, name, runs) in [
        (&["new"][..], "plain", "Hello, world!\n"),
        (&["new", "--lib"][..], "onlylib", ""),
        (&["new", "--lib", "--bin"][..], "my-calc", "2 + 2 = 4\n"),
    ] {
        let mut new = args.to_vec();
        new.push(name);
        ok(&rok(&home, &root, &new));
        let dir = root.join(name);
        ok(&rok(&home, &dir, &["build"]));
        ok(&rok(&home, &dir, &["build", "--release"]));
        if !runs.is_empty() {
            assert_eq!(script_out(&home, &dir, &["run"]), runs, "{name}");
        }
        let tests = script_out(&home, &dir, &["test"]);
        assert!(tests.contains("test result: ok"), "{name}: {tests}");
    }
    // The executable reaches its library's items and modules by the
    // library's code name (`my-calc` → `my_calc`).
    let calc = root.join("my-calc");
    write(
        &calc.join("src/lib.rok"),
        "pub mod ops;\n\npub fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n\n\
         pub struct Pair {\n    pub a: i32;\n    pub b: i32;\n}\n",
    );
    write(
        &calc.join("src/ops.rok"),
        "pub fn mul(a: i32, b: i32) -> i32 {\n    return a * b;\n}\n",
    );
    write(
        &calc.join("src/main.rok"),
        "use my_calc::{Pair, add};\nuse my_calc::ops::mul;\n\n\
         let p = Pair { a: 3, b: 4 };\nprintln(f\"{add(p.a, p.b)} {mul(p.a, p.b)} {my_calc::ops::mul(2, 5)}\");\n",
    );
    assert_eq!(script_out(&home, &calc, &["run"]), "7 12 10\n");
}

/// A package with `src/lib.rok` and `src/main.rok` (spec 0.3.1): `rok new
/// --lib --bin` makes one, a dependent gets its library, `rok install`
/// installs its executable (`--lib`: its library), and `rok publish`
/// takes it, as it takes an executable-only package; a stone without a
/// library is not a dependency.
// spec: §0.3.1 §9.10.2 §9.10.4a §9.10.5
#[test]
fn lib_and_main_packages() {
    if !llvm() {
        return;
    }
    let root = scratch("lib-and-main");
    let home = root.join("home");
    let out = rok(&home, &root, &["new", "--lib", "--bin", "both"]);
    ok(&out);
    assert!(
        text(&out.stderr).contains("Created library and executable package"),
        "{}",
        text(&out.stderr)
    );
    let both = root.join("both");
    assert!(read(&both.join("src/lib.rok")).contains("pub fn add"));
    assert_eq!(
        read(&both.join("src/main.rok")),
        "use both::add;\n\nprintln(f\"2 + 2 = {add(2, 2)}\");\n"
    );
    assert_eq!(script_out(&home, &both, &["run"]), "2 + 2 = 4\n");
    let tests = script_out(&home, &both, &["test"]);
    assert!(tests.contains("test adds ... ok"), "{tests}");
    assert!(both.join("target/debug/libboth.a").is_file());

    // A dependent sees the library.
    let user = root.join("user");
    write(
        &user.join("rok.rokon"),
        "Package { name: \"user\", deps: { \"both\": Dep::path(\"../both\") } }\n",
    );
    write(
        &user.join("src/main.rok"),
        "use both::add;\nprintln(f\"{add(1, 2)}\");\n",
    );
    assert_eq!(script_out(&home, &user, &["run"]), "3\n");

    // `rok install`: the executable, with a note on the library; `--lib`
    // installs the library instead.
    let out = rok(&home, &root, &["install", "--path", "both"]);
    ok(&out);
    assert!(
        text(&out.stderr).contains(
            "note: `both` also has a library (src/lib.rok); `rok install --lib ..` puts that \
             into environment `default` (for `rok repl` / `rok jupyter`) instead"
        ),
        "{}",
        text(&out.stderr)
    );
    let installed = Command::new(home.join("bin").join(exe("both")))
        .output()
        .unwrap();
    assert_eq!(ok(&installed), "2 + 2 = 4\n");
    ok(&rok(&home, &root, &["install", "--lib", "--path", "both"]));
    assert!(read(&home.join("envs/default/rok.rokon")).contains("\"both\": Dep::path("));
    // For the REPL (`rok deps` outside a package).
    assert!(deps(&home, &root, &[]).iter().any(|(n, _)| n == "both"));
    write(&root.join("s.rok"), "println(7);\n");
    let list = ok(&rok(&home, &root, &["install", "--list"]));
    assert_eq!(list.matches("  both ").count(), 2, "{list}");
    let hello = root.join("hello");
    write(&hello.join("src/main.rok"), "println(\"hello\");\n");
    fails(
        &rok(&home, &root, &["install", "--lib", "--path", "hello"]),
        "`hello` has no library (src/lib.rok) for --lib; without --lib, its executable \
         (src/main.rok) goes into",
    );
    fails(
        &rok(&home, &root, &["install", "--lib", "--path", "s.rok"]),
        "`s.rok` is a script: --lib installs a package's library (src/lib.rok)",
    );

    // Stones: one with both, and one with only an executable.
    let index = root.join("index");
    commit(
        &index,
        &[("config.rokon", "Config { api: \"https://rokstones.io\" }\n")],
    );
    for kv in [
        ["user.name", "t"],
        ["user.email", "t@t"],
        ["commit.gpgsign", "false"],
    ] {
        git(&index, &["config", kv[0], kv[1]]);
    }
    let reg = index.display().to_string();
    write(
        &home.join("config.rokon"),
        &format!("Config {{ registry: {} }}\n", q(&reg)),
    );
    let calc = root.join("calc");
    let main = "use calc::one;\nprintln(f\"calc {one()}\");\n";
    stone(
        &calc,
        "calc",
        "1.0.0",
        "",
        &[("src/lib.rok", ONE), ("src/main.rok", main)],
    );
    let tool = root.join("tool");
    stone(
        &tool,
        "tool",
        "0.1.0",
        "",
        &[("src/main.rok", "println(\"tool\");\n")],
    );
    for dir in [&calc, &tool] {
        let line = ok(&rok(&home, dir, &["publish", "--index", &reg]));
        assert!(line.starts_with("Stone { name: "), "{line}");
        ok(&rok(&home, dir, &["publish", "--index", &reg, "--commit"]));
    }
    let empty = root.join("empty");
    stone(&empty, "empty", "0.1.0", "", &[("README", "")]);
    fails(
        &rok(&home, &empty, &["publish"]),
        "`empty` is not a package: no src/lib.rok (a library) or src/main.rok (an executable)",
    );
    let installed = rok(&home, &root, &["install", "tool"]);
    assert!(
        text(&installed.stderr).contains("   Installed tool 0.1.0 ("),
        "{}",
        text(&installed.stderr)
    );
    ok(&rok(&home, &root, &["install", "calc"]));
    // Reinstalling: the same version is kept unless --force; another
    // version, newer or older, replaces it.
    let says = |args: &[&str], want: &str| {
        let out = rok(&home, &root, args);
        assert!(text(&out.stderr).contains(want), "{}", text(&out.stderr));
    };
    says(&["install", "tool"], "`tool` 0.1.0 is installed already (");
    says(
        &["install", "tool"],
        "to rebuild it, run `rok install tool --force`",
    );
    says(&["install", "tool", "--force"], " Reinstalled tool 0.1.0 (");
    stone(
        &tool,
        "tool",
        "0.2.0",
        "",
        &[("src/main.rok", "println(\"tool\");\n")],
    );
    ok(&rok(
        &home,
        &tool,
        &["publish", "--index", &reg, "--commit"],
    ));
    says(
        &["install", "tool"],
        "    Upgraded tool from 0.1.0 to 0.2.0 (",
    );
    says(
        &["install", "tool@0.1"],
        "  Downgraded tool from 0.2.0 to 0.1.0 (",
    );
    for (name, want) in [("tool", "tool\n"), ("calc", "calc 1\n")] {
        let out = Command::new(home.join("bin").join(exe(name)))
            .output()
            .unwrap();
        assert_eq!(ok(&out), want);
    }
    ok(&rok(&home, &root, &["install", "--lib", "calc"]));
    assert!(
        read(&home.join("envs/default/rok.rokon")).contains("\"calc\": Dep::version(\"^1.0.0\")")
    );
    write(
        &root.join("c.rok"),
        "use calc::one;\nprintln(f\"{one()}\");\n",
    );
    ok(&rok(&home, &root, &["add", "calc", "c.rok"]));
    assert_eq!(script_out(&home, &root, &["c.rok"]), "1\n");
    fails(
        &rok(&home, &root, &["install", "--lib", "tool"]),
        "`tool` has no library (src/lib.rok) for --lib",
    );
    // An executable-only stone is no dependency.
    let app = root.join("app");
    write(
        &app.join("rok.rokon"),
        "Package { name: \"app\", deps: { \"tool\": Dep::version(\"^0.1\") } }\n",
    );
    write(&app.join("src/main.rok"), "println(\"app\");\n");
    fails(
        &rok(&home, &app, &["build"]),
        "is an executable package (src/main.rok, no src/lib.rok): a dependency must be a \
         library; a program is installed with `rok install`",
    );
}

/// rok leaves nothing in the temp directory: a script run, a failing
/// build, an environment library's prebuild, an executable install.
#[test]
fn rok_leaves_no_temporaries() {
    if !llvm() {
        return;
    }
    let root = scratch("no-temporaries");
    let home = root.join("home");
    let tmp = root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let run = |args: &[&str]| {
        let mut c = rok_cmd(&home, &root, args);
        for v in ["TMPDIR", "TMP", "TEMP"] {
            c.env(v, &tmp);
        }
        c.output().unwrap()
    };
    write(
        &root.join("greet/rok.rokon"),
        "Package { name: \"greet\" }\n",
    );
    write(&root.join("greet/src/lib.rok"), LIB_V1);
    write(&root.join("tool/rok.rokon"), "Package { name: \"tool\" }\n");
    write(&root.join("tool/src/main.rok"), "println(\"tool\");\n");
    write(&root.join("hi.rok"), "println(\"hi\");\n");
    write(&root.join("bad.rok"), "let x: i32 = \"no\";\n");
    assert_eq!(ok(&run(&["-q", "hi.rok"])), "hi\n");
    assert_ne!(run(&["-q", "bad.rok"]).status.code(), Some(0));
    ok(&run(&["install", "--path", "greet"]));
    ok(&run(&["install", "--path", "tool"]));
    let left: Vec<String> = fs::read_dir(&tmp)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "rokc-target")
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}
