//! `rokc lint`, `rokc doc`, `rokc bench` on scratch packages.
//! Lint and doc need no LLVM; bench builds native code and skips without
//! the toolchain (see `common::setup`).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{ROKC, scratch, setup, text, workspace};

/// A fresh scratch package directory holding `files`.
fn package(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = scratch(name);
    for (path, text) in files {
        let p = dir.join("src").join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    dir
}

fn rokc(cwd: &Path, args: &[&str]) -> Output {
    Command::new(ROKC)
        .current_dir(cwd)
        .env("ROK_HOME", workspace())
        .args(args)
        .output()
        .unwrap()
}

#[track_caller]
fn exit(out: &Output, code: i32) -> (String, String) {
    let (o, e) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(code), "stdout: {o}\nstderr: {e}");
    (o, e)
}

/// Build the `kind` (`test` | `bench`) harness in `dir`; each executable
/// (absolute) and its cases, read from the emitted manifest. `rokc` builds
/// the harness and `rokc_harness` runs it, so a test drives them apart.
#[track_caller]
fn build_harness(dir: &Path, rokr: &str, kind: &str) -> Vec<(PathBuf, Vec<String>)> {
    let manifest = dir.join("harness.manifest");
    let mp = manifest.to_str().unwrap();
    exit(&rokc(dir, &[kind, "--rokr", rokr, "--emit-harness", mp]), 0);
    let text = fs::read_to_string(&manifest).unwrap();
    let mut hs: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "exe" => {
                let p = PathBuf::from(rest);
                hs.push((if p.is_absolute() { p } else { dir.join(p) }, Vec::new()));
            }
            "case" => hs.last_mut().unwrap().1.push(rest.to_owned()),
            _ => {}
        }
    }
    hs
}

const LINTED: &str = "\
use rok::core::str::StrBuf;

fn helper() -> i32 { 1 }
fn unusedFn() -> i32 { let y = 2; 3 }
#[must_use]
fn important() -> i32 { 3 }
#[deprecated = \"use helper\"]
fn old() -> i32 { 4 }
struct bad_name { v: i32; }
const lower: i32 = 1;
fn thrower() throws -> i32 { 1 }

#[allow(unused_variables)]
fn quiet() { let a = 1; }

fn main() {
    let v: i32 | none = none;
    if v == none { println(\"n\"); }
    let f = 1.5;
    let z = f as f64;
    let q = 3i32;
    let w = q.clone();
    println(f\"{w} {f:.2} {z}\");
    for i in 0..3 { let i = 5; println(f\"{i}\"); }
    let t = true;
    if t == true { println(\"t\"); }
    let k = 1;
    k = k;
    if f == f64::NAN { println(\"nan\"); }
    important();
    println(f\"{old()} {helper()}\");
    try? thrower();
    while true { break; }
    quiet();
    return;
    println(\"x\");
}
";

/// An item's `#[allow]` covers its variants even after a `#[derive]`
/// (whose impls carry the attribute's span); an `_` between digits is
/// still camel case (rustc's rule).
#[test]
fn lint_camel_case_allow_and_digit_underscores() {
    let src = "#[derive(Clone, PartialEq)]\n#[allow(non_camel_case_types)]\n\
               pub enum A { A_x }\n\
               #[allow(non_camel_case_types)]\n#[derive(Clone, PartialEq)]\n\
               pub enum B { B_x }\n\
               pub enum Ver { V1_0, V1_1, Bad_c, X__1 }\n\
               fn main() { let _ = A::A_x; let _ = B::B_x; let _ = Ver::V1_0; \
               let _ = Ver::V1_1; let _ = Ver::Bad_c; let _ = Ver::X__1; }\n";
    let dir = package("lint_camel", &[("main.rok", src)]);
    let (_, err) = exit(&rokc(&dir, &["lint"]), 0);
    let flagged: Vec<&str> = err
        .lines()
        .filter_map(|l| l.split("variant `").nth(1)?.split('`').next())
        .collect();
    assert_eq!(flagged, ["Bad_c", "X__1"], "{err}");
}

/// The types of an `extern` block's functions and statics are uses of
/// their imports.
#[test]
fn lint_counts_extern_types_as_uses() {
    let sys = "use rok::bare::ptr::{CPtr, ConstPtr, Ptr};\n\
               extern \"C\" {\n    pub fn f(p: CPtr<u8>) -> ConstPtr<u8>;\n    \
               pub static g: CPtr<u8>;\n}\n";
    let dir = package(
        "lint_extern",
        &[("main.rok", "mod sys;\nfn main() {}\n"), ("sys.rok", sys)],
    );
    let (_, err) = exit(&rokc(&dir, &["lint"]), 0);
    assert!(err.contains("unused import `rok::bare::ptr::Ptr`"), "{err}");
    assert_eq!(err.matches("unused import").count(), 1, "{err}");
}

#[test]
fn lint_finds_each_kind_and_honors_levels() {
    let dir = package("lint", &[("main.rok", LINTED)]);
    // `self_assignment` and `nan_comparison` deny by default.
    let (_, err) = exit(&rokc(&dir, &["lint"]), 1);
    for want in [
        "unused import `rok::core::str::StrBuf`",
        "function `unusedFn` is never used",
        "function `unusedFn` should have a snake case name",
        "unused variable `y`",
        "type `bad_name` should have an upper camel case name",
        "constant `lower` should have an upper case name",
        "comparing with `none` using `==`",
        "casting to the same type `f64`",
        "redundant `.clone()`",
        "`i` shadows the loop variable `i`",
        "comparison with `true`",
        "`k` is assigned to itself",
        "unused result of `important`",
        "use of deprecated function `old`",
        "`try?` result discarded",
        "`while true` is `loop`",
        "unreachable statement",
        "`#[warn(unused_variables)]` on by default",
        "error[E0132]: `k` is assigned to itself",
        "`#[deny(self_assignment)]` on by default",
        "error[E0132]: comparison with NaN is always false",
        "`#[deny(nan_comparison)]` on by default",
    ] {
        assert!(err.contains(want), "missing {want:?}:\n{err}");
    }
    // `#[allow]` on `quiet`; the f-string's own `as f64` is not ours.
    assert!(!err.contains("unused variable `a`"), "{err}");
    assert_eq!(err.matches("casting to the same type").count(), 1, "{err}");

    // `-D` makes it an error (exit 1); `-A` on a group silences it.
    let (_, err) = exit(&rokc(&dir, &["lint", "-D", "unused_variables"]), 1);
    assert!(
        err.contains("error[E0132]") && err.contains("`-D unused_variables` on the command line")
    );
    let (_, err) = exit(&rokc(&dir, &["lint", "-A", "all", "-W", "deprecated"]), 0);
    assert!(
        err.contains("deprecated function") && !err.contains("unused"),
        "{err}"
    );
    let (_, err) = exit(&rokc(&dir, &["lint", "-A", "nope"]), 1);
    assert!(err.contains("unknown lint `nope`"), "{err}");

    // Attributes beat the command line, innermost first.
    fs::write(
        dir.join("src/main.rok"),
        "#![deny(unused)]\nmod m;\n#[warn(unused_variables)]\nfn f() { let a = 1; }\n\
         fn main() { f(); let b = 1; }\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/m.rok"),
        "#![allow(dead_code)]\nfn hidden() {}\n",
    )
    .unwrap();
    let (_, err) = exit(&rokc(&dir, &["lint", "-A", "unused_variables"]), 1);
    assert!(
        err.contains("warning[E0132]: unused variable `a`\n  --> src/main.rok:4:14"),
        "{err}"
    );
    assert!(
        err.contains("error[E0132]: unused variable `b`\n  --> src/main.rok:5:22"),
        "{err}"
    );
    assert!(
        err.contains("`#[deny(unused_variables)]` set here"),
        "{err}"
    );
    assert!(!err.contains("hidden"), "{err}");

    // JSON lines on stdout.
    let (out, _) = exit(&rokc(&dir, &["-q", "--message-format=json", "lint"]), 1);
    let line = out.lines().find(|l| l.contains("`b`")).expect(&out);
    assert!(
        line.starts_with("{\"type\":\"error\",\"code\":\"E0132\",\"kind\":\"lint\""),
        "{line}"
    );

    let (out, _) = exit(&rokc(&dir, &["lint", "--list"]), 0);
    assert!(out.contains("unused_variables") && out.contains("nonstandard_style"));

    // No false hits: a method reached only through `?.`, an import used
    // only as the head of a later `use`.
    fs::write(
        dir.join("src/main.rok"),
        "mod a { pub mod inner { pub fn f() -> i32 { 1 } } }\n\
         use a::inner;\nuse inner::f;\n\
         struct V { x: i32; }\nimpl V { fn get(self) -> i32 { self.x } }\n\
         fn mk() -> V | none { V { x: f() } }\n\
         fn main() { println(f\"{mk()?.get() ?? 0}\"); }\n",
    )
    .unwrap();
    let (_, err) = exit(&rokc(&dir, &["lint"]), 0);
    assert!(!err.contains("[E0132]"), "{err}");
}

#[test]
fn doc_writes_html_with_links_and_search_index() {
    let dep = package(
        "doc-dep",
        &[(
            "lib.rok",
            "//! Utilities.\n\n/// Says hi.\npub fn hello() -> i32 { 1 }\n",
        )],
    );
    let dir = package(
        "doc",
        &[
            (
                "lib.rok",
                "//! Shapes and **things**.\n//!\n//! - one\n//! - two\n\n\
                 pub mod geo;\nmod hidden;\n\n\
                 /// A point. See `Shape` and [rok](https://example.com).\n\
                 #[derive(Clone)]\npub struct Point {\n    /// Across.\n    pub x: f64;\n    \
                 secret: f64;\n}\n\n\
                 impl Point {\n    /// The origin.\n    pub fn origin() -> Point { Point { x: 0.0, secret: 0.0 } }\n}\n\n\
                 /// Kinds.\npub enum Shape {\n    /// Round.\n    Circle(f64),\n    Square,\n}\n\n\
                 /// Adds.\n///\n/// ```\n/// add(1, 2) < 4\n/// ```\npub fn add(a: i32, b: i32) -> i32 { a + b }\n\n\
                 fn private_fn() {}\n\n\
                 pub mod inl {\n    //! Inline docs.\n    #![doc = \"More inline.\"]\n\n    \
                 pub fn g() {}\n}\n",
            ),
            (
                "geo.rok",
                "//! Geometry.\n\n/// Area.\npub fn area(p: pkg::Point) -> f64 { p.x }\n",
            ),
            ("hidden.rok", "pub fn invisible() {}\n"),
        ],
    );
    let ext = format!("util={}", dep.display());
    let (_, err) = exit(&rokc(&dir, &["doc", "--extern", &ext]), 0);
    let err = err.replace('\\', "/");
    assert!(err.contains("target/doc/index.html"), "{err}");
    let read = |p: &str| fs::read_to_string(dir.join("target/doc").join(p)).unwrap();

    let index = read("index.html");
    let at = |s: &str| {
        index
            .find(s)
            .unwrap_or_else(|| panic!("{s} not in {index}"))
    };
    assert!(
        at("doc/index.html") < at("util/index.html"),
        "user package first"
    );

    let root = read(&format!(
        "{}/index.html",
        dir.file_name().unwrap().to_str().unwrap()
    ));
    for want in [
        "<strong>things</strong>",
        "<li>one</li>",
        "id=\"struct.Point\"",
        "<pre class=\"sig fn\"><code>pub fn add(a: i32, b: i32) -&gt; i32</code></pre>",
        "add(1, 2) &lt; 4",
        "<a href=\"https://example.com\">rok</a>",
        "#enum.Shape\"><code>Shape</code></a>",
        "x: f64</code><div class=\"doc\"><p>Across.</p>",
        "impl Clone for Point",
        "The origin.",
        "Circle(f64)",
        "href=\"../style.css\"",
        "geo/index.html\">geo</a>",
    ] {
        assert!(root.contains(want), "missing {want:?}:\n{root}");
    }
    for absent in ["secret", "private_fn", "hidden", "invisible"] {
        assert!(!root.contains(absent), "{absent} leaked:\n{root}");
    }
    let geo = read(&format!(
        "{}/geo/index.html",
        dir.file_name().unwrap().to_str().unwrap()
    ));
    assert!(
        geo.contains("Geometry.") && geo.contains("href=\"../../style.css\""),
        "{geo}"
    );
    assert!(geo.contains("index.html#struct.Point\">Point</a>"), "{geo}");
    assert!(read("util/index.html").contains("Says hi."));
    let inl = read(&format!(
        "{}/inl/index.html",
        dir.file_name().unwrap().to_str().unwrap()
    ));
    assert!(
        inl.contains("Inline docs.") && inl.contains("More inline."),
        "{inl}"
    );

    let search = read("search-index.js");
    assert!(
        search.contains("\"p\":\"util::hello\",\"k\":\"fn\""),
        "{search}"
    );
    assert!(search.contains("geo::area"), "{search}");
    assert!(!search.contains("invisible"), "{search}");
    assert!(read("search.js").contains("SEARCH_INDEX"));
}

/// `rokc doc --std` (no package): the standard library tiers alone, into
/// `./target/doc/`, one directory per tier.
#[test]
fn doc_std_documents_the_library() {
    let dir = scratch("doc-std");
    let (_, err) = exit(&rokc(&dir, &["doc", "--std"]), 0);
    let err = err.replace('\\', "/");
    assert!(err.contains("target/doc/index.html"), "{err}");
    let read = |p: &str| fs::read_to_string(dir.join("target/doc").join(p)).unwrap();
    let index = read("index.html");
    for want in [
        "rok-core/index.html\">rok::core",
        "rok-std/index.html\">rok::std",
    ] {
        assert!(index.contains(want), "missing {want:?}:\n{index}");
    }
    let list = read("rok-core/collections/list/index.html");
    assert!(list.contains("id=\"struct.List\""), "{list}");
    assert!(list.contains("href=\"../../../style.css\""), "{list}");
    assert!(read("search-index.js").contains("rok::core::collections::list::List"));
}

#[test]
fn bench_times_each_bench() {
    let Some(env) = setup() else { return };
    let dir = package(
        "bench",
        &[(
            "lib.rok",
            "use rok::core::bench::Bench;\n\
             use rok::core::hint::black_box;\n\
             pub fn fib(n: i64) -> i64 { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }\n\
             #[bench]\nfn fib_10(b: Bench) { b.iter(|| black_box(fib(10i64))); }\n\
             #[bench]\nfn other(b: Bench) { b.iter(|| black_box(fib(2i64))); }\n\
             #[bench]\nfn boom(b: Bench) { b.iter(|| { panic(\"bad bench\"); }); }\n",
        )],
    );
    let rokr = env.rokr.display().to_string();
    let hs = build_harness(&dir, &rokr, "bench");
    assert_eq!(hs.len(), 1, "one harness: {hs:?}");
    let (exe, cases) = &hs[0];
    let find = |name: &str| {
        cases
            .iter()
            .find(|c| c.as_str() == name || c.ends_with(&format!("::{name}")))
            .unwrap_or_else(|| panic!("no {name} in {cases:?}"))
            .clone()
    };
    let fib = find("fib_10");
    let boom = find("boom");

    // The library prints the stable machine line (field 4 = 0: not a tty).
    let one = Command::new(exe)
        .args([fib.as_str(), "3", "2", "0", "6"])
        .output()
        .unwrap();
    assert!(
        text(&one.stdout).contains("#rokbench name="),
        "{}",
        text(&one.stdout)
    );
    // A panicking bench exits non-zero and reports on stderr.
    let bad = Command::new(exe)
        .args([boom.as_str(), "3", "2", "0", "6"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(
        text(&bad.stderr).contains("bad bench"),
        "{}",
        text(&bad.stderr)
    );

    // The runner selects, times and reports each; `fib` selects one of three.
    let mut buf = Vec::new();
    let opts = rokc_harness::bench::Options {
        filter: Some("fib"),
        iters: Some(3),
        samples: 2,
        rokbench: false,
    };
    let ok = rokc_harness::bench::run(exe, cases, &opts, &mut buf);
    let s = text(&buf);
    assert!(ok, "{s}");
    assert!(s.contains("benchmarking 1 bench\n"), "{s}");
    assert!(
        s.contains("bench result: ok. 1 run; 0 failed; 2 filtered out"),
        "{s}"
    );

    // With no filter the panicking bench fails the run.
    let mut buf = Vec::new();
    let opts = rokc_harness::bench::Options {
        filter: None,
        iters: Some(3),
        samples: 2,
        rokbench: false,
    };
    let ok = rokc_harness::bench::run(exe, cases, &opts, &mut buf);
    let s = text(&buf);
    assert!(!ok, "{s}");
    assert!(s.contains("boom") && s.contains("FAILED"), "{s}");
    assert!(s.contains("2 run; 1 failed"), "{s}");

    // `#[bench]` functions are not part of a normal build.
    let (_, err) = exit(&rokc(&dir, &["check"]), 0);
    assert!(!err.contains("fib_10"), "{err}");
}

#[test]
fn tests_and_benches_directories() {
    let Some(env) = setup() else { return };
    let dir = scratch("dirs");
    let write = |rel: &str, text: &str| {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    };
    write(
        "src/lib.rok",
        "pub fn add(a: i64, b: i64) -> i64 { a + b }\n",
    );
    // tests/ and benches/ sit beside src/, reach the package through
    // pkg::, and are collected only by `test` / `bench`.
    write(
        "tests/basic.rok",
        "use pkg::add;\n#[test]\nfn adds() { if add(40i64, 2i64) != 42i64 { panic(\"broke\"); } }\n",
    );
    write(
        "benches/perf.rok",
        "use pkg::add;\nuse rok::core::bench::Bench;\nuse rok::core::hint::black_box;\n\
         #[bench]\nfn add_bench(b: Bench) { b.iter(|| black_box(add(40i64, 2i64))); }\n",
    );
    let rokr = env.rokr.display().to_string();

    // `test` collects tests/; the runner names each case by its path.
    let mut buf = Vec::new();
    for (exe, cases) in &build_harness(&dir, &rokr, "test") {
        let opts = rokc_harness::test::Options {
            filter: None,
            threads: 1,
            show_output: false,
        };
        rokc_harness::test::run(exe, cases, &opts, &mut buf);
    }
    let s = text(&buf);
    assert!(s.contains("test tests::basic::adds ... ok"), "{s}");

    // `bench` collects benches/; run the one bench directly for its line.
    let bh = build_harness(&dir, &rokr, "bench");
    let (exe, name) = bh
        .iter()
        .find_map(|(e, cs)| cs.iter().find(|c| c.contains("add_bench")).map(|c| (e, c)))
        .expect("add_bench harness");
    let o = Command::new(exe)
        .args([name.as_str(), "3", "2", "0", "6"])
        .output()
        .unwrap();
    assert!(
        text(&o.stdout).contains("#rokbench name=benches::perf::add_bench "),
        "{}",
        text(&o.stdout)
    );

    // Neither directory is part of a normal build.
    let (_, err) = exit(&rokc(&dir, &["check"]), 0);
    assert!(!err.contains("adds") && !err.contains("add_bench"), "{err}");
}
