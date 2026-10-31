//! Native code against the platform: the C ABI against clang-built C, the
//! stack-overflow report, `-g` DWARF. Unix only; skips without LLVM.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use common::{Env, ROKC, scratch, setup, text};

/// `rokc build <src> <extra..>` with the harness library and runtime.
fn build(env: &Env, src: &Path, extra: &[&str]) -> Output {
    Command::new(ROKC)
        .arg("-q")
        .arg("build")
        .arg(src)
        .arg("--bootstrap")
        .arg(&env.bootstrap)
        .arg("--rokr")
        .arg(&env.rokr)
        .args(std::env::var("ROK_TEST_OPT").ok().filter(|o| !o.is_empty()))
        .args(extra)
        .output()
        .unwrap()
}

#[track_caller]
fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "status {:?}\nstdout: {}\nstderr: {}",
        out.status,
        text(&out.stdout),
        text(&out.stderr)
    );
}

const C_SIDE: &str = r"
#include <stdarg.h>
typedef struct { float x, y, z; } V3;
typedef struct { double a, b, c, d; } D4;
typedef struct { long a, b, c; } Big;
typedef struct { signed char a; short b; } Small;
typedef struct { long a; double b; } Mix;
typedef struct { int a, b, c; } I3;
V3 v3_scale(V3 v, float k) { V3 r = { v.x * k, v.y * k, v.z * k }; return r; }
double d4_sum(D4 d) { return d.a + d.b + d.c + d.d; }
Big big_make(long a) { Big b = { a, a * 2, a * 3 }; return b; }
long big_sum(Big b) { return b.a + b.b + b.c; }
Small small_inc(Small s) { s.a += 1; s.b += 1; return s; }
Mix mix_twice(Mix m) { m.a *= 2; m.b *= 2; return m; }
int i3_sum(I3 s) { return s.a + s.b + s.c; }
long many(long a, long b, long c, long d, long e, I3 s, long f) {
    return a + b + c + d + e + s.a + s.b + s.c + f;
}
int sx(signed char c, unsigned short u) { return c * 100000 + u; }
long vsum(int n, ...) {
    va_list ap; va_start(ap, n); long t = 0;
    for (int i = 0; i < n; i++) t += va_arg(ap, long);
    va_end(ap); return t;
}
typedef struct { int q, r; } DT;
DT dt_div(int n, int d) { DT x = { n / d, n % d }; return x; }
int via(DT (*f)(int, int)) { DT d = f(9, 4); return d.q * 10 + d.r; }
double vmix(int n, ...) {
    va_list ap; va_start(ap, n); double t = 0;
    for (int i = 0; i < n; i++) { t += va_arg(ap, int); t += va_arg(ap, double); }
    va_end(ap); return t;
}
int cb_v3(V3 (*f)(V3, float)) { V3 v = { 1, 2, 3 }; V3 r = f(v, 3.0f); return (int)(r.x + r.y + r.z); }
long cb_big(Big (*f)(Big)) { Big b = { 1, 2, 3 }; Big r = f(b); return r.a * 100 + r.b * 10 + r.c; }
double cb_d4(double (*f)(D4)) { D4 d = { 1, 2, 3, 4 }; return f(d); }
Small cb_small(Small (*f)(Small)) { Small s = { 4, 5 }; return f(s); }
Mix rok_mix(Mix m);
double call_rok_mix(void) { Mix m = { 3, 0.5 }; Mix r = rok_mix(m); return r.a + r.b; }
Big rev_c(Big b) { Big r = { b.c, b.b, b.a }; return r; }
V3 (*get_v3_scale(void))(V3, float) { return v3_scale; }
Big (*get_rev(void))(Big) { return rev_c; }
long cb_big2(Big (*f)(Big)) { return cb_big(f); }
signed char neg8(signed char c) { return -c; }
signed char rok_neg8(signed char c);
unsigned short rok_dbl16(unsigned short u);
int call_narrow(void) { return rok_neg8(-7) * 100000 + rok_dbl16(40000); }
char cid(char c) { return c; }
typedef struct { float x; double y; } FD;
typedef struct { float f; int i; } FI;
FD fd_mul(FD v, int k) { FD r = { v.x * k, v.y * k }; return r; }
FI fi_inc(FI v) { v.f += 1; v.i += 1; return v; }
int ult(unsigned a, unsigned b) { return a < b; }
";

const ROK_SIDE: &str = r#"
struct V3 { x: f32; y: f32; z: f32; }
struct D4 { a: f64; b: f64; c: f64; d: f64; }
struct Big { a: i64; b: i64; c: i64; }
struct Small { a: i8; b: i16; }
struct Mix { a: i64; b: f64; }
struct I3 { a: i32; b: i32; c: i32; }
struct DT { q: i32; r: i32; }
struct FD { x: f32; y: f64; }
struct FI { f: f32; i: i32; }

extern "C" {
    fn v3_scale(v: V3, k: f32) -> V3;
    fn d4_sum(d: D4) -> f64;
    fn big_make(a: i64) -> Big;
    fn big_sum(b: Big) -> i64;
    fn small_inc(s: Small) -> Small;
    fn mix_twice(m: Mix) -> Mix;
    fn i3_sum(s: I3) -> i32;
    fn many(a: i64, b: i64, c: i64, d: i64, e: i64, s: I3, f: i64) -> i64;
    fn sx(c: i8, u: u16) -> i32;
    fn vsum(n: i32, ...) -> i64;
    fn vmix(n: i32, ...) -> f64;
    fn dt_div(n: i32, d: i32) -> DT;
    fn via(f: unsafe fn(i32, i32) -> DT) -> i32;
    fn printf(fmt: str, ...) -> i32;
    fn cb_v3(f: fn(V3, f32) -> V3) -> i32;
    fn cb_big(f: fn(Big) -> Big) -> i64;
    fn cb_d4(f: fn(D4) -> f64) -> f64;
    fn cb_small(f: fn(Small) -> Small) -> Small;
    fn call_rok_mix() -> f64;
    // C function pointers as values (spec 5.9.4).
    fn get_v3_scale() -> extern "C" fn(V3, f32) -> V3;
    fn get_rev() -> extern "C" fn(Big) -> Big;
    fn cb_big2(f: extern "C" fn(Big) -> Big) -> i64;
    fn neg8(c: i8) -> i8;
    fn call_narrow() -> i32;
    fn cid(c: c_char) -> c_char;
    fn fd_mul(v: FD, k: i32) -> FD;
    fn fi_inc(v: FI) -> FI;
    fn ult(a: u32, b: u32) -> i32;
}

// C calls these (spec 5.9.10): aggregates the C way both ways.
extern "C" fn scale_v3(v: V3, k: f32) -> V3 { V3 { x: v.x * k, y: v.y * k, z: v.z * k } }
extern "C" fn rev_big(b: Big) -> Big { Big { a: b.c, b: b.b, c: b.a } }
extern "C" fn sum_d4(d: D4) -> f64 { d.a + d.b + d.c + d.d }
extern "C" fn neg_small(s: Small) -> Small { Small { a: -s.a, b: -s.b } }
#[export_name("rok_mix")]
extern "C" fn mix_rok(m: Mix) -> Mix { Mix { a: m.a + 1, b: m.b * 2.0 } }
// Narrow results come back extended where the platform ABI says so.
#[export_name("rok_neg8")]
extern "C" fn neg8_rok(c: i8) -> i8 { -c }
#[export_name("rok_dbl16")]
extern "C" fn dbl16_rok(u: u16) -> u16 { u.wrapping_mul(2) }

fn main() {
    unsafe {
        let v = v3_scale(V3 { x: 1.0, y: 2.0, z: 3.0 }, 2.0);
        println(f"{v.x} {v.y} {v.z}");
        println(f"{d4_sum(D4 { a: 1.0, b: 2.0, c: 3.0, d: 4.5 })}");
        let b = big_make(5);
        println(f"{b.a} {b.b} {b.c} {big_sum(b)}");
        let s = small_inc(Small { a: -3, b: 300 });
        println(f"{s.a} {s.b}");
        let m = mix_twice(Mix { a: 21, b: 1.25 });
        println(f"{m.a} {m.b}");
        println(f"{i3_sum(I3 { a: 1, b: 2, c: 3 })}");
        println(f"{many(1, 2, 3, 4, 5, I3 { a: 6, b: 7, c: 8 }, 9)}");
        let c: i8 = -5;
        let u: u16 = 65535;
        println(f"{sx(c, u)}");
        let x: i64 = 40;
        println(f"{vsum(3, x, x, 2 as i64)}");
        let small: i8 = -2;
        let half: f32 = 0.5;
        println(f"{vmix(2, small, half, true, 1.25)}");
        // A C function handed back to C as a pointer keeps its C type.
        println(f"{via(dt_div)}");
        let n = cb_small(neg_small);
        println(f"{cb_v3(scale_v3)} {cb_big(rev_big)} {cb_d4(sum_d4) as i64} {call_rok_mix() as i64} {n.a} {n.b}");
    }
    // From rok they are rok functions.
    let r = rev_big(Big { a: 7, b: 8, c: 9 });
    println(f"{r.a}{r.b}{r.c}");
    // `extern "C" fn` values: received from C, made from a rok `extern
    // "C" fn` (its C entry), called with the C ABI, handed back to C.
    let f = unsafe { get_v3_scale() };
    let w = f(V3 { x: 1.0, y: 2.0, z: 3.0 }, 2.0);
    let g: extern "C" fn(Big) -> Big = unsafe { get_rev() };
    let rb = g(Big { a: 1, b: 2, c: 3 });
    let h: extern "C" fn(Big) -> Big = rev_big;
    let rh = h(Big { a: 4, b: 5, c: 6 });
    let k: extern "C" fn(Small) -> Small | none = neg_small;
    let ks = match k { none => Small { a: 0, b: 0 }, p: extern "C" fn(Small) -> Small => p(Small { a: 1, b: 2 }) };
    println(f"{w.x} {w.y} {w.z} {rb.a}{rb.b}{rb.c} {rh.a}{rh.b}{rh.c} {ks.a} {ks.b} {unsafe { cb_big2(h) }}");
    println(f"{unsafe { neg8(5) }} {unsafe { call_narrow() }}");
    println(f"{unsafe { cid(c'A') } as i32}");
    let fd = unsafe { fd_mul(FD { x: 1.5, y: 2.25 }, -2) };
    let fi = unsafe { fi_inc(FI { f: 0.5, i: -8 }) };
    let big: u32 = 4000000000;
    println(f"{fd.x} {fd.y} {fi.f} {fi.i} {unsafe { ult(5, big) }}{unsafe { ult(big, 5) }}");
}
"#;

const OVERFLOW: &str = r#"
use rok::std::thread;

// Not a tail call, and no accumulator LLVM could turn into a loop.
fn down(n: i64) -> i64 {
    if n == 0 { return 0; }
    return down(n - 1) % 7 + 1;
}

async fn deep(n: i64) -> i64 {
    if n == 0 { return 0; }
    return (await deep(n - 1)) % 7 + 1;
}

fn main() {
    print("before\n");
    let args = rok::std::env::args();
    let mode = args[1];
    if mode == "main" {
        println(f"{down(1000000000)}");
    } else if mode == "thread" {
        let h = thread::spawn(|| down(1000000000));
        println(f"{try! h.join()}");
    } else {
        println(f"{task::block_on(deep(1000000000))}");
    }
}
"#;

/// Runaway recursion on the main thread, a spawned thread and through
/// nested `await`s: the runtime reports it (after flushing stdout) and
/// aborts, instead of a bare SIGSEGV.
// spec: §3.14.8
#[test]
fn stack_overflow_is_reported() {
    let Some(env) = setup() else { return };
    let dir = scratch("overflow");
    fs::write(dir.join("main.rok"), OVERFLOW).unwrap();
    let exe = dir.join("out");
    let out = Command::new(ROKC)
        .args(["-q", "build"])
        .arg(dir.join("main.rok"))
        .arg("--bootstrap")
        .arg(&env.bootstrap)
        .arg("--bootstrap")
        .arg(env.workspace.join("library/std/src/lib.rok"))
        .arg("--rokr")
        .arg(&env.rokr)
        .args(std::env::var("ROK_TEST_OPT").ok().filter(|o| !o.is_empty()))
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    ok(&out);
    for (mode, name) in [("main", "main"), ("thread", "<unnamed>"), ("async", "main")] {
        let run = Command::new(&exe).arg(mode).output().unwrap();
        let stderr = text(&run.stderr);
        assert!(!run.status.success(), "{mode}: {:?}", run.status);
        assert!(
            stderr.contains(&format!("thread '{name}' has overflowed its stack")),
            "{mode}: {stderr}"
        );
        assert_eq!(text(&run.stdout), "before\n", "{mode}");
    }
}

// A 2.4 MB frame: without stack probes its first store lands far below
// the guard page.
const WIDE: &str = r#"
fn wide(n: i64) -> i64 {
    let a: [i64; 300000] = [n; 300000];
    if n == 0 { return a[7]; }
    return wide(n - 1) % 7 + a[(n % 300000) as usize];
}

fn main() {
    print("before\n");
    println(f"{wide(1000)}");
}
"#;

/// Recursion through huge frames is reported like any overflow (stack
/// probes).
#[test]
fn big_frame_overflow_is_reported() {
    let Some(env) = setup() else { return };
    let dir = scratch("wide");
    fs::write(dir.join("main.rok"), WIDE).unwrap();
    let exe = dir.join("out");
    let out = Command::new(ROKC)
        .args(["-q", "build"])
        .arg(dir.join("main.rok"))
        .arg("--bootstrap")
        .arg(&env.bootstrap)
        .arg("--rokr")
        .arg(&env.rokr)
        .args(std::env::var("ROK_TEST_OPT").ok().filter(|o| !o.is_empty()))
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    ok(&out);
    let run = Command::new(&exe).output().unwrap();
    let stderr = text(&run.stderr);
    assert!(!run.status.success(), "{:?}", run.status);
    assert!(
        stderr.contains("thread 'main' has overflowed its stack"),
        "{stderr}"
    );
    assert_eq!(text(&run.stdout), "before\n");
}

const CHATTY: &str = r#"
use rok::std::thread;

async fn main() {
    let busy = Atomic::new(0);
    for i in 0..4 {
        let t = thread::spawn(|| {
            while true {
                println("tick");
                busy.store(1);
            }
        });
        t.detach();
    }
    let s = task::spawn(async {
        while true {
            println("task");
            await task::yield_now();
        }
    });
    while busy.load() == 0 {}
    print("end\n");
}
"#;

/// Once `main` returns, other threads stop at their next safepoint outside
/// runtime code, so the exit flush never waits on a lock they hold.
#[test]
fn shutdown_stops_other_threads() {
    let Some(env) = setup() else { return };
    let dir = scratch("chatty");
    fs::write(dir.join("main.rok"), CHATTY).unwrap();
    let exe = dir.join("out");
    let out = Command::new(ROKC)
        .args(["-q", "build"])
        .arg(dir.join("main.rok"))
        .arg("--bootstrap")
        .arg(&env.bootstrap)
        .arg("--bootstrap")
        .arg(env.workspace.join("library/std/src/lib.rok"))
        .arg("--rokr")
        .arg(&env.rokr)
        .args(std::env::var("ROK_TEST_OPT").ok().filter(|o| !o.is_empty()))
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    ok(&out);
    for _ in 0..20 {
        let run = Command::new(&exe).output().unwrap();
        ok(&run);
        let stdout = text(&run.stdout);
        assert!(stdout.contains("end\n"), "{stdout:?}");
    }
}

const DEBUGGEE: &str = "fn add(a: i32, b: i32) -> i32 {
    let s = a + b;
    let t = s * 2;
    return t;
}

fn main() {
    let x = add(3, 4);
    let p = P { x: x, y: 2.5 };
    let name = \"rok\";
    let pair = (p, [1u8, 2u8]);
    if x > 0 {
        let x = pair.1[0];
        println(f\"{pair.0.x + x as i32 - 1}\");
    }
}

struct P { x: i32; y: f64; }
";

/// `-g` (spec 9.9.6), both back ends: a subprogram named by the rok
/// path with its parameters and locals, and a line table pointing into
/// the `.rok`; `llvm-dwarfdump --verify` finds no error.
// spec: §0.2.4 §9.2.11 §9.5.10
#[test]
fn debug_info_names_functions_lines_and_locals() {
    let Some(env) = setup() else { return };
    let Some(dump) = ["llvm-dwarfdump", "dwarfdump"]
        .into_iter()
        .find(|t| Command::new(t).arg("--version").output().is_ok())
    else {
        return;
    };
    for backend in ["llvm", "native"] {
        let dir = scratch(&format!("debug_{backend}"));
        fs::write(dir.join("main.rok"), DEBUGGEE).unwrap();
        let exe = dir.join("out");
        let be = format!("backend={backend}");
        ok(&build(
            &env,
            &dir.join("main.rok"),
            &["-g", "-C", &be, "-o", exe.to_str().unwrap()],
        ));
        let run = Command::new(&exe).output().unwrap();
        ok(&run);
        assert_eq!(text(&run.stdout), "14\n");
        // macOS keeps DWARF in the `.dSYM` clang's dsymutil step writes.
        let dsym = dir.join("out.dSYM");
        let target = if dsym.exists() { dsym } else { exe };
        check_debuggee(dump, &target, backend);
    }
}

/// `-g`, both back ends: an item declared in a function body (spec
/// 3.9.1) and its method and `static` read as written, not by their
/// lifted names.
// spec: §3.9.1
#[test]
fn debug_info_names_block_local_items() {
    let Some(env) = setup() else { return };
    let Some(dump) = ["llvm-dwarfdump", "dwarfdump"]
        .into_iter()
        .find(|t| Command::new(t).arg("--version").output().is_ok())
    else {
        return;
    };
    let src = "fn f() -> i32 {\n    static HITS: i32 = 2;\n    struct L { v: i32; }\n    \
               impl L { fn get(self) -> i32 { self.v + HITS } }\n    let l = L { v: 1 };\n    \
               l.get()\n}\nfn main() { println(f\"{f()}\"); }\n";
    for backend in ["llvm", "native"] {
        let dir = scratch(&format!("debug_local_{backend}"));
        fs::write(dir.join("main.rok"), src).unwrap();
        let exe = dir.join("out");
        let be = format!("backend={backend}");
        ok(&build(
            &env,
            &dir.join("main.rok"),
            &["-g", "-C", &be, "-o", exe.to_str().unwrap()],
        ));
        let dsym = dir.join("out.dSYM");
        let target = if dsym.exists() { dsym } else { exe };
        let info = Command::new(dump)
            .arg("--debug-info")
            .arg(&target)
            .output()
            .unwrap();
        let info = text(&info.stdout);
        for name in ["(\"L\")", "(\"L__get\")"] {
            assert!(info.contains(name), "{backend}: no {name}:\n{info}");
        }
        assert!(!info.contains("(\"f/"), "{backend}: a lifted name:\n{info}");
    }
}

fn check_debuggee(dump: &str, target: &Path, backend: &str) {
    let info = Command::new(dump)
        .arg("--debug-info")
        .arg(target)
        .output()
        .unwrap();
    let info = text(&info.stdout);
    let sub = info
        .split("DW_TAG_subprogram")
        .find(|s| s.contains("(\"add\")") && s.contains("main.rok"))
        .unwrap_or_else(|| panic!("{backend}: no `add` subprogram:\n{info}"));
    assert!(sub.contains("DW_AT_decl_line\t(1)"), "{backend}: {sub}");
    for var in ["(\"a\")", "(\"b\")", "(\"s\")", "(\"t\")", "\"i32\")"] {
        assert!(sub.contains(var), "{backend}: `add` lacks {var}:\n{sub}");
    }
    // The shadowing `x` lives in the `if` body's lexical block.
    let main = info
        .split("DW_TAG_subprogram")
        .find(|s| s.contains("(\"main\")") && s.contains("main.rok"))
        .unwrap_or_else(|| panic!("{backend}: no rok `main`:\n{info}"));
    let block = main
        .split("DW_TAG_lexical_block")
        .nth(1)
        .unwrap_or_else(|| panic!("{backend}: no lexical block:\n{main}"));
    assert!(
        block.contains("(\"x\")") && block.contains("\"u8\")"),
        "{backend}: {block}"
    );
    // Aggregates: a struct with its fields, `str`, a tuple, an array.
    let p = info
        .split("DW_TAG_structure_type")
        .find(|s| s.contains("DW_AT_name\t(\"P\")"))
        .unwrap_or_else(|| panic!("{backend}: no `P` type:\n{info}"));
    for want in ["(\"x\")", "(\"y\")", "DW_AT_data_member_location\t(0x08)"] {
        assert!(p.contains(want), "{backend}: `P` lacks {want}:\n{p}");
    }
    for want in [
        "(\"p\")",
        "(\"name\")",
        "(\"pair\")",
        "(\"str\")",
        "(\"tuple\")",
        "DW_TAG_array_type",
    ] {
        assert!(info.contains(want), "{backend}: no {want}:\n{info}");
    }
    let lines = Command::new(dump)
        .arg("--debug-line")
        .arg(target)
        .output()
        .unwrap();
    let lines = text(&lines.stdout);
    assert!(lines.contains("main.rok"), "{backend}: {lines}");
    // `let t = s * 2;` (line 3) has a row.
    assert!(
        lines
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some("3")),
        "{backend}: no line-3 row:\n{lines}"
    );
    verify_dwarf(dump, target, backend);
}

/// `(depth, tag, name)` of each entry of the unit named `unit` in a
/// `--debug-info` dump, sorted. LLVM's `__ARRAY_SIZE_TYPE__` (the type
/// of its subrange bounds) is left out.
fn unit_dies(info: &str, unit: &str) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    let mut inside = false;
    for die in info.split("\n0x") {
        let Some((_, rest)) = die.split_once(':') else {
            continue;
        };
        let head = rest.lines().next().unwrap_or("");
        let tag = head.trim();
        if !tag.starts_with("DW_TAG_") && tag != "NULL" {
            continue;
        }
        if tag == "DW_TAG_compile_unit" {
            inside = rest.contains(&format!("DW_AT_name\t(\"{unit}\")"));
            continue;
        }
        let name = rest
            .lines()
            .find_map(|l| l.trim().strip_prefix("DW_AT_name\t(\""))
            .map_or("", |n| n.trim_end_matches("\")"));
        if inside && name != "__ARRAY_SIZE_TYPE__" {
            let depth = head.len() - head.trim_start().len();
            out.push((depth, tag.to_owned(), name.to_owned()));
        }
    }
    out.sort();
    out
}

/// `llvm-dwarfdump --verify` reports no error (`dwarfdump` has no
/// verifier).
fn verify_dwarf(dump: &str, target: &Path, what: &str) {
    if dump != "llvm-dwarfdump" {
        return;
    }
    let verify = Command::new(dump)
        .arg("--verify")
        .arg(target)
        .output()
        .unwrap();
    let verify = text(&verify.stdout);
    assert!(!verify.contains("error:"), "{what}: {verify}");
}

const DEBUGGEE_TYPES: &str = "enum Shape { Circle(f64), Rect(i32, i32), Empty }

fn pick(b: bool) -> i64 | f64 {
    if b { return 1.5f64; }
    return 7i64;
}

fn show(s: Shape, v: i64 | f64, xs: List<i32>) -> i32 {
    let n = xs.len() as i32;
    return n;
}

fn main() {
    let xs = [10, 20, 30];
    println(f\"{show(Shape::Rect(3, 4), pick(true), xs)}\");
    println(f\"{held(7i64)}\");
}

fn held(a: any) -> i32 {
    let total = 5;
    let f = [ref total] |k: i32| { total += k; return total; };
    let r = f(3);
    return r + total + call(dbl, 1);
}

fn dbl(k: i32) -> i32 { return k * 2; }

fn call(g: fn(i32) -> i32, k: i32) -> i32 {
    let r = g(k);
    return r;
}
";

/// `-g` DWARF for sums, `any`, `GcBuf`, captured variables and `fn`
/// values, as lldb (with rok's formatters) and gdb show them.
#[test]
fn debug_info_shows_sums_and_gc_objects() {
    let Some(env) = setup() else { return };
    let Some(dump) = ["llvm-dwarfdump", "dwarfdump"]
        .into_iter()
        .find(|t| Command::new(t).arg("--version").output().is_ok())
    else {
        return;
    };
    let mut units = Vec::new();
    for backend in ["llvm", "native"] {
        let dir = scratch(&format!("debug_types_{backend}"));
        fs::write(dir.join("main.rok"), DEBUGGEE_TYPES).unwrap();
        let exe = dir.join("out");
        let be = format!("backend={backend}");
        ok(&build(
            &env,
            &dir.join("main.rok"),
            &["-g", "-C", &be, "-o", exe.to_str().unwrap()],
        ));
        let dsym = dir.join("out.dSYM");
        let target = if dsym.exists() { &dsym } else { &exe };
        let info = Command::new(dump)
            .arg("--debug-info")
            .arg(target)
            .output()
            .unwrap();
        let info = text(&info.stdout);
        verify_dwarf(dump, target, backend);
        units.push(unit_dies(&info, "main.rok"));
        let part = info
            .split("DW_TAG_variant_part")
            .find(|s| s.contains("(\"Rect\")"))
            .unwrap_or_else(|| panic!("{backend}: no `Shape` variant part:\n{info}"));
        assert!(
            part.contains("(\"Circle\")")
                && part.contains("DW_AT_discr_value")
                && part.contains("\"Shape::Rect\""),
            "{backend}: {part}"
        );
        assert!(
            info.split("DW_TAG_variant_part")
                .any(|s| s.contains("(\"f64\")") && s.contains("(\"i64\")")),
            "{backend}: no `f64 | i64` variant part:\n{info}"
        );
        assert!(
            info.split("DW_TAG_variant_part")
                .any(|s| s.contains("(\"any::i64\")")),
            "{backend}: no `any` variant part:\n{info}"
        );
        let buf = info
            .split("DW_TAG_structure_type")
            .find(|s| s.contains("(\"GcBuf<i32>\")"))
            .unwrap_or_else(|| panic!("{backend}: no `GcBuf<i32>` object type:\n{info}"));
        assert!(
            buf.contains("(\"items\")") && buf.contains("(\"type_id\")"),
            "{backend}: {buf}"
        );
        // Parameters have variables, used or not.
        let show = info
            .split("DW_TAG_subprogram")
            .find(|s| s.contains("(\"show\")"))
            .unwrap_or_else(|| panic!("{backend}: no `show` subprogram:\n{info}"));
        for p in ["s", "v", "xs"] {
            assert!(
                show.split("DW_TAG_formal_parameter")
                    .any(|d| d.contains(&format!("(\"{p}\")")) && d.contains("DW_AT_location")),
                "{backend}: no parameter `{p}`:\n{show}"
            );
        }
        let fv = info
            .split("DW_TAG_structure_type")
            .find(|s| s.contains("DW_AT_name\t(\"fn(i32) -> i32\")"))
            .unwrap_or_else(|| panic!("{backend}: no `fn(i32) -> i32` type:\n{info}"));
        assert!(
            fv.contains("(\"code\")") && fv.contains("(\"env\")"),
            "{backend}: {fv}"
        );
        assert!(
            info.contains("DW_TAG_subroutine_type"),
            "{backend}: no subroutine type:\n{info}"
        );
        assert!(
            info.split("DW_TAG_subprogram")
                .any(|s| s.contains("(\"rok.register_types\")")),
            "{backend}: no `rok.register_types` subprogram:\n{info}"
        );
        assert!(
            info.split("DW_TAG_subprogram")
                .filter(|s| s.contains("(\"main\")"))
                .count()
                >= 2,
            "{backend}: no subprogram for the C `main`:\n{info}"
        );
        // `call` stops before its call: past its last use the slot of
        // `g` is cleared (a dead GC root).
        if let Some(out) = gdb(
            &exe,
            &[
                "break main.rok:10",
                "break main.rok:23",
                "break main.rok:29",
                "run",
                "info args",
                "c",
                "info args",
                "info locals",
                "c",
                "info args",
            ],
        ) {
            for want in [
                "s = Shape::Rect(3, 4)",
                "v = f64 | i64::f64(1.5)",
                "a = any::i64(7)",
                "total = 8",
                "g = fn(i32) -> i32 {code: 0x",
                "<dbl__fnval>, env: 0x0}",
            ] {
                assert!(out.contains(want), "{backend}: no `{want}`:\n{out}");
            }
        }
        let Some(out) = lldb(
            &exe,
            &[
                "b main.rok:10",
                "run",
                "frame variable s v xs.h->items[0].buf->items[2]",
                "b main.rok:23",
                "c",
                "frame variable a total",
                "b main.rok:29",
                "c",
                "frame variable g",
            ],
        ) else {
            continue;
        };
        // The active variant only: its name, then its fields.
        let vars = out.split("frame variable").nth(1).unwrap_or_default();
        assert!(
            vars.contains("(Shape) s = Rect {\n  0 = 3\n  1 = 4\n}"),
            "{backend}: {out}"
        );
        assert!(vars.contains("v = f64 {\n  0 = 1.5\n}"), "{backend}: {out}");
        assert!(
            !vars.contains("Circle") && !vars.contains("$discr$"),
            "{backend}: {out}"
        );
        assert!(vars.contains("= 30"), "{backend}: {out}");
        let held = out.split("frame variable").nth(2).unwrap_or_default();
        assert!(
            held.contains("(any) a = i64 {\n  0 = 7\n}"),
            "{backend}: {out}"
        );
        assert!(held.contains("total = 8"), "{backend}: {out}");
        let g = out.split("frame variable").nth(3).unwrap_or_default();
        assert!(
            g.contains("(fn(i32) -> i32) g = {\n  code = 0x")
                && g.contains("`dbl__fnval at main.rok:26"),
            "{backend}: {out}"
        );
    }
    // The same entries, tag and name, in the program's unit.
    assert!(units[0].len() > 20, "{:?}", units[0]);
    assert_eq!(units[0], units[1], "llvm vs native DIEs");
}

/// `lldb --batch` output for `cmds` on `exe`; `None` without a usable lldb
/// (a wedged debug service hangs `run`; Rosetta processes take none).
fn lldb(exe: &Path, cmds: &[&str]) -> Option<String> {
    if cfg!(target_os = "macos") && std::env::consts::ARCH != "aarch64" {
        return None;
    }
    let init = Command::new(ROKC)
        .args(["--print", "lldbinit"])
        .output()
        .unwrap();
    ok(&init);
    let init = text(&init.stdout);
    let mut c = Command::new("lldb");
    c.args(["--batch", "-O", init.trim()]);
    debugger(c, exe, cmds, "-o", "kill").filter(|o| o.contains("stop reason"))
}

/// The output of `gdb -batch` running `cmds` on `exe`; `None` without a
/// gdb that runs it.
fn gdb(exe: &Path, cmds: &[&str]) -> Option<String> {
    let mut c = Command::new("gdb");
    c.args(["-batch", "-nx"]);
    // A stop: `Breakpoint 2, f (..) at ..` (`Thread 1 "out" hit Breakpoint 2.1, ..`).
    let stop = |l: &str| {
        l.split_once("Breakpoint ")
            .and_then(|(_, r)| r.split_once(", "))
            .is_some_and(|(n, _)| n.chars().all(|c| c.is_ascii_digit() || c == '.'))
    };
    debugger(c, exe, cmds, "-ex", "kill").filter(|o| o.lines().any(stop))
}

/// The stdout of debugger `c` running `cmds` (each after `flag`), then
/// `last`, on `exe`; `None` if it cannot start or runs past a minute.
fn debugger(mut c: Command, exe: &Path, cmds: &[&str], flag: &str, last: &str) -> Option<String> {
    for cmd in cmds.iter().chain(&[last]) {
        c.args([flag, cmd]);
    }
    let mut child = c
        .arg(exe)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipped: {c:?} did not finish");
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Some(text(&child.wait_with_output().unwrap().stdout))
}

const DEBUGGEE_OPT: &str = "struct Wrap { inner: any; n: i32; }

fn fib(n: i64) -> i64 {
    if n < 2 { return n; }
    let a = fib(n - 1);
    let b = fib(n - 2);
    return a + b;
}

yield fn evens(limit: i32) -> i32 {
    let k = 0;
    while k < limit {
        let twice = k * 2;
        yield twice;
        k += 1;
    }
}

fn main() {
    let base = 10;
    let f = |k: i32| { return k + base; };
    let w: any = Wrap { inner: 5i64, n: 2 };
    let sum = 0;
    for v in evens(3) {
        sum += v;
    }
    println(f\"{fib(10)} {f(sum)} {held(w)}\");
}

fn held(w: any) -> i32 {
    return 1;
}
";

/// `-g` at `-O0` and `-O2`: coroutine locals, closures, nested `any`, and
/// `-O2` breakpoints; both backends write the same entries.
#[test]
fn debug_info_optimized_coroutines_closures() {
    let Some(env) = setup() else { return };
    let Some(dump) = ["llvm-dwarfdump", "dwarfdump"]
        .into_iter()
        .find(|t| Command::new(t).arg("--version").output().is_ok())
    else {
        return;
    };
    let mut units = Vec::new();
    for (name, args) in [
        ("llvm", &["-C", "backend=llvm"][..]),
        ("native", &["-C", "backend=native"][..]),
        ("o2", &["-O2"][..]),
    ] {
        let dir = scratch(&format!("debug_opt_{name}"));
        fs::write(dir.join("main.rok"), DEBUGGEE_OPT).unwrap();
        let exe = dir.join("out");
        let mut extra = vec!["-g", "-o", exe.to_str().unwrap()];
        extra.extend_from_slice(args);
        ok(&build(&env, &dir.join("main.rok"), &extra));
        let run = Command::new(&exe).output().unwrap();
        ok(&run);
        assert_eq!(text(&run.stdout), "55 16 1\n");
        let dsym = dir.join("out.dSYM");
        let target = if dsym.exists() { &dsym } else { &exe };
        verify_dwarf(dump, target, name);
        let info = Command::new(dump)
            .arg("--debug-info")
            .arg(target)
            .output()
            .unwrap();
        let info = text(&info.stdout);
        // `isOptimized` (Mach-O: `DW_AT_APPLE_optimized`).
        if dsym.exists() {
            let unit = info
                .split("DW_TAG_compile_unit")
                .find(|u| u.contains("DW_AT_name\t(\"main.rok\")"))
                .and_then(|u| u.split("DW_TAG_").next())
                .unwrap_or_else(|| panic!("{name}: no main.rok unit:\n{info}"));
            let opt = unit.contains("DW_AT_APPLE_optimized\t(true)");
            assert_eq!(opt, name == "o2", "{name}: {unit}");
        }
        if name == "o2" {
            // Elsewhere the optimizer keeps `fib` recursive: `finish`
            // stops at the breakpoint again.
            if std::env::consts::ARCH != "aarch64" {
                continue;
            }
            let Some(out) = lldb(
                &exe,
                &[
                    "b main.rok:14",
                    "b main.rok:7",
                    "run",
                    "frame variable k twice",
                    "breakpoint delete 1",
                    "c",
                    "frame variable a n",
                    "finish",
                ],
            ) else {
                continue;
            };
            let co = out.split("frame variable").nth(1).unwrap_or_default();
            assert!(co.contains("k = 0") && co.contains("twice = 0"), "{out}");
            let fib = out.split("frame variable").nth(2).unwrap_or_default();
            // `n` is dead here unless the recursion became a loop.
            assert!(fib.contains("a = 1"), "{out}");
            assert!(fib.contains("step out"), "{out}");
            // To `main` if the optimizer made the recursion a loop, else
            // to the calling `fib`.
            assert!(fib.contains("Return value: (long) $0 = "), "{out}");
            continue;
        }
        units.push(unit_dies(&info, "main.rok"));
        let Some(out) = lldb(
            &exe,
            &[
                "b main.rok:14",
                "b main.rok:27",
                "b main.rok:31",
                "run",
                "frame variable",
                "breakpoint delete 1",
                "c",
                "frame variable -P 3 f",
                "c",
                "frame variable -P 6 -D 12 w",
                "finish",
            ],
        ) else {
            continue;
        };
        // The resume function by its creator; no compiler temporaries.
        let co = out.split("frame variable").nth(1).unwrap_or_default();
        assert!(
            co.contains("k = 0") && co.contains("twice = 0"),
            "{name}: {out}"
        );
        assert!(!co.contains("__"), "{name}: {out}");
        assert!(out.contains("out`evens::{coroutine#0}"), "{name}: {out}");
        let f = out.split("frame variable").nth(2).unwrap_or_default();
        assert!(
            f.contains("env = 0x") && f.contains("0 = 10"),
            "{name}: {out}"
        );
        let w = out.split("frame variable").nth(3).unwrap_or_default();
        assert!(
            w.contains("(any) w = Wrap {") && w.contains("inner = i64 {") && w.contains("0 = 5"),
            "{name}: {out}"
        );
        assert!(w.contains("Return value: (int) $0 = 1"), "{name}: {out}");
    }
    assert!(units[0].len() > 20, "{:?}", units[0]);
    assert_eq!(units[0], units[1], "llvm vs native DIEs");
}

/// `-g` ELF executables linked from several units by the embedded lld:
/// per-unit tables, exact lexical blocks, no ranges left by `--gc-sections`.
#[test]
fn elf_debug_info_of_several_objects() {
    if setup().is_none() {
        return;
    }
    let targets = Path::new(ROKC).parent().unwrap().join("../lib/rok/targets");
    let dump = "llvm-dwarfdump";
    if !targets.is_dir() || Command::new(dump).arg("--version").output().is_err() {
        eprintln!("skipped: no {} or no {dump}", targets.display());
        return;
    }
    let dir = scratch("elf-debug");
    fs::create_dir_all(dir.join("greet/src")).unwrap();
    fs::write(
        dir.join("greet/src/lib.rok"),
        "pub fn greet(n: i32) -> i32 {\n    let s = 0;\n    for i in 0..n {\n        \
         if i % 2 == 0 {\n            let d = i * 3;\n            s += d;\n        } else {\n            \
         let e = i + 7;\n            s -= e;\n        }\n    }\n    return s;\n}\n\
         pub fn unused(n: i32) -> i32 { return n * 9; }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app/src")).unwrap();
    fs::write(
        dir.join("app/src/main.rok"),
        "use greet::greet;\nstruct Node { id: i32; kids: List<Node>; }\n\
         enum Shape { Circle(f64), Rect(i32, i32), Empty }\n\
         fn kids(k: i32) -> i64 {\n    let n = Node { id: k, kids: [] };\n    \
         let s = Shape::Rect(k, 2);\n    return n.kids.len() as i64;\n}\n\
         let k = greet(5);\nif k > 0 {\n    let w = k * 2;\n    \
         println(f\"{w}\");\n}\nprintln(f\"{k} {kids(k)}\");\n",
    )
    .unwrap();
    let builds = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-musl"]
        .into_iter()
        .flat_map(|t| [(t, "-O0"), (t, "-O1")]);
    for (t, opt) in builds {
        if !targets.join(t).is_dir() {
            continue;
        }
        let exe = dir.join(format!("app-{t}{opt}"));
        let out = Command::new(ROKC)
            .current_dir(&dir)
            .env("ROK_HOME", common::workspace())
            .args(["-q", "build", "app", "--extern", "greet=greet", "-g"])
            .args([opt, "--target", t, "-o"])
            .arg(&exe)
            .output()
            .unwrap();
        ok(&out);
        let info = Command::new(dump)
            .arg("--debug-info")
            .arg(&exe)
            .output()
            .unwrap();
        let info = text(&info.stdout);
        let sub = info
            .split("DW_TAG_subprogram")
            .find(|s| s.contains("(\"greet::greet\")"))
            .unwrap_or_else(|| panic!("{t} {opt}: no `greet` subprogram:\n{info}"));
        assert!(sub.contains("greet/src/lib.rok\")"), "{t} {opt}: {sub}");
        // Optimized code keeps no dead locals (nor their types).
        let o0 = opt == "-O0";
        assert!(
            !o0 || sub.contains("(\"d\")") && sub.contains("(\"e\")"),
            "{t} {opt}: {sub}"
        );
        // Sums are variant parts; a struct reached again through a
        // `GcBuf` field is a declaration.
        assert!(
            !o0 || info
                .split("DW_TAG_variant_part")
                .any(|s| s.contains("\"Shape::Rect\")") && s.contains("DW_AT_discr_value")),
            "{t} {opt}: no `Shape` variant part:\n{info}"
        );
        assert!(
            !o0 || info
                .split("DW_TAG_structure_type")
                .any(|s| s.contains("(\"Node\")") && s.contains("DW_AT_declaration")),
            "{t} {opt}: no `Node` declaration:\n{info}"
        );
        let lines = Command::new(dump)
            .arg("--debug-line")
            .arg(&exe)
            .output()
            .unwrap();
        let lines = text(&lines.stdout);
        // Directories and names are separate entries; LLVM's DWARF 4
        // leaves a unit's own directory implicit: its comp_dir.
        for f in ["\"lib.rok\"", "\"main.rok\""] {
            assert!(lines.contains(f), "{t} {opt}: no line table names {f}");
        }
        for d in ["/greet/src\"", "/app/src\""] {
            assert!(
                lines.contains(d) || !o0 && info.contains(&format!("{d})")),
                "{t} {opt}: no line table or unit names {d}"
            );
        }
        // Dropped code's DWARF addresses are the tombstone, so no unit
        // (rok's, the runtime's, the C library's) has an error.
        verify_dwarf(dump, &exe, &format!("{t} {opt}"));
    }
}

/// `-O0` puts each function in its own section, so unused prebuilt code
/// is dropped at link time.
// spec: §9.2.14
#[test]
fn unused_dependency_code_is_dropped() {
    if setup().is_none() {
        return;
    }
    let targets = Path::new(ROKC).parent().unwrap().join("../lib/rok/targets");
    let dir = scratch("gc-sections");
    fs::create_dir_all(dir.join("dep/src")).unwrap();
    fs::write(
        dir.join("dep/src/lib.rok"),
        "pub fn used() -> str {\n    return \"marker-of-used-code\";\n}\n\n\
         pub fn unused() -> str {\n    return \"marker-of-unused-code\";\n}\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("app/src")).unwrap();
    fs::write(
        dir.join("app/src/main.rok"),
        "use dep::used;\nprintln(used());\n",
    )
    .unwrap();
    let has = |b: &[u8], s: &str| b.windows(s.len()).any(|w| w == s.as_bytes());
    for t in [
        None,
        Some("x86_64-unknown-linux-gnu"),
        Some("aarch64-unknown-linux-gnu"),
        Some("x86_64-unknown-linux-musl"),
        Some("aarch64-unknown-linux-musl"),
        Some("x86_64-pc-windows-msvc"),
        Some("aarch64-pc-windows-msvc"),
    ] {
        if t.is_some_and(|t| !targets.join(t).is_dir()) {
            continue;
        }
        let exe = dir.join(format!("app-{}", t.unwrap_or("host")));
        let mut rokc = Command::new(ROKC);
        rokc.current_dir(&dir)
            .env("ROK_HOME", common::workspace())
            .args(["-q", "build", "app", "--extern", "dep=dep", "-O0", "-o"])
            .arg(&exe);
        if let Some(t) = t {
            rokc.args(["--target", t]);
        }
        ok(&rokc.output().unwrap());
        let b = fs::read(&exe).unwrap();
        assert!(has(&b, "marker-of-used-code"), "{t:?}");
        assert!(!has(&b, "marker-of-unused-code"), "{t:?}");
    }
}

/// Aggregates by value both ways, HFAs, `sret`, register exhaustion,
/// caller-extended narrow ints and variadic promotions, against clang.
const NICHE_C: &str = r"
#include <stddef.h>
typedef long (*op)(long);
typedef struct { op f; long x; } Cb;
long twice(long x) { return 2 * x; }
op get_op(int some) { return some ? twice : NULL; }
long call_or(op f, long x, long d) { return f ? f(x) : d; }
long run_cb(Cb c) { return c.f ? c.f(c.x) : -1; }
Cb make_cb(int some) { Cb c = { some ? twice : NULL, 21 }; return c; }
op slot;
long call_slot(long x) { return slot ? slot(x) : -1; }
int nz_or(int v, int d) { return v ? v : d; }
int nz_make(int some) { return some ? 7 : 0; }
long g = 77;
long *get_g(int some) { return some ? &g : NULL; }
long ptr_read_or(long *p, long d) { return p ? *p : d; }
";

const NICHE_ROK: &str = r#"
use rok::bare::ptr::Ptr;

struct Cb { f: extern "C" fn(i64) -> i64 | none; x: i64; }

extern "C" {
    fn twice(x: i64) -> i64;
    fn get_op(some: i32) -> extern "C" fn(i64) -> i64 | none;
    fn call_or(f: extern "C" fn(i64) -> i64 | none, x: i64, d: i64) -> i64;
    fn run_cb(c: Cb) -> i64;
    fn make_cb(some: i32) -> Cb;
    fn call_slot(x: i64) -> i64;
    static mut slot: extern "C" fn(i64) -> i64 | none;
    fn nz_or(v: NonZero<i32> | none, d: i32) -> i32;
    fn nz_make(some: i32) -> NonZero<i32> | none;
    fn get_g(some: i32) -> Ptr<i64> | none;
    fn ptr_read_or(p: Ptr<i64> | none, d: i64) -> i64;
}

extern "C" fn neg(x: i64) -> i64 { -x }

fn show(f: extern "C" fn(i64) -> i64 | none) -> str {
    match f { none => "none", g: extern "C" fn(i64) -> i64 => f"{g(5)}" }
}

fn widen(f: extern "C" fn(i64) -> i64 | none) -> extern "C" fn(i64) -> i64 | none | i32 { f }

fn back(v: extern "C" fn(i64) -> i64 | none | i32) -> str {
    match v { g: extern "C" fn(i64) -> i64 => f"fn {g(1)}", none => "none", i: i32 => f"i32 {i}" }
}

fn main() {
    let a = unsafe { get_op(1) };
    let z = unsafe { get_op(0) };
    println(f"{a is none} {z is none} {show(a)} {show(z)}");
    if let g: extern "C" fn(i64) -> i64 = a { println(f"if-let {g(21)}"); }
    if let g: extern "C" fn(i64) -> i64 = z { println(f"bad {g(1)}"); } else { println("if-let none"); }
    let n: extern "C" fn(i64) -> i64 | none = none;
    let p: extern "C" fn(i64) -> i64 | none = neg;
    println(f"{unsafe { call_or(n, 4, 99) }} {unsafe { call_or(p, 4, 99) }} {unsafe { call_or(a, 4, 99) }}");
    let xs: List<extern "C" fn(i64) -> i64 | none> = List::new();
    xs.push(a);
    xs.push(z);
    xs.push(p);
    for f in xs { print(f"{show(f)},"); }
    println("");
    let c = Cb { f: p, x: 7 };
    let d = Cb { f: none, x: 7 };
    let m = unsafe { make_cb(1) };
    let k = unsafe { make_cb(0) };
    println(f"{unsafe { run_cb(c) }} {unsafe { run_cb(d) }} {show(m.f)} {m.x} {show(k.f)} {unsafe { run_cb(m) }}");
    let wi: extern "C" fn(i64) -> i64 | none | i32 = 3;
    println(f"{back(widen(a))} {back(widen(z))} {back(wi)}");
    let narrow: extern "C" fn(i64) -> i64 | none = match widen(a) { i: i32 => none, g: extern "C" fn(i64) -> i64 | none => g };
    println(f"{show(narrow)}");
    let dflt: extern "C" fn(i64) -> i64 = neg;
    let h = z ?? dflt;
    let h2 = a ?? dflt;
    println(f"{h(3)} {h2(3)}");
    unsafe {
        println(f"{show(slot)} {call_slot(50)}");
        slot = neg;
        println(f"{show(slot)} {call_slot(50)}");
        slot = none;
        println(f"{slot is none} {call_slot(50)}");
        let nz: NonZero<i32> | none = NonZero::new(5);
        let nn: NonZero<i32> | none = none;
        let mk = nz_make(1) ?? NonZero::new(1) ?? panic("zero");
        println(f"{nz_or(nz, 1)} {nz_or(nn, 1)} {mk} {nz_make(0) is none}");
        let p = get_g(1);
        let rd: i64 = match p { q: Ptr<i64> => q.read(), none => -1 };
        println(f"{ptr_read_or(p, -1)} {ptr_read_or(get_g(0), -1)} {rd} {get_g(0) is none}");
    }
}
"#;

/// Null niches across the C ABI: nullable fn pointers, `NonZero<i32> |
/// none` as `int`, `Ptr<T> | none` as `T*`.
#[test]
fn c_fn_ptr_null_niche() {
    let Some(env) = setup() else { return };
    let dir = scratch("cniche");
    fs::write(dir.join("side.c"), NICHE_C).unwrap();
    fs::write(dir.join("main.rok"), NICHE_ROK).unwrap();
    let obj = dir.join("main.o");
    let bare = env.workspace.join("library/bare/src/lib.rok");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &[
            "--bootstrap",
            bare.to_str().unwrap(),
            "--emit=obj",
            "-o",
            obj.to_str().unwrap(),
        ],
    ));
    let exe = dir.join("out");
    ok(&Command::new("clang")
        .arg(&obj)
        .arg(dir.join("side.c"))
        .arg(&env.rokr)
        .args(["-lm", "-o"])
        .arg(&exe)
        .output()
        .unwrap());
    let run = Command::new(&exe).output().unwrap();
    ok(&run);
    assert_eq!(
        text(&run.stdout),
        "false true 10 none\nif-let 42\nif-let none\n99 -4 8\n10,none,-5,\n\
         -7 -1 10 21 none 42\nfn 2 none i32 3\n10\n-3 6\nnone -1\n-5 -50\ntrue -1\n\
         5 1 7 true\n77 -1 77 true\n"
    );
}

/// Ten arguments, `str`s past the last argument register. Runtime ABI
/// (spec 8.5): a `str` is a by-value `(ptr, len)` struct, never split
/// between the last register and the stack. C ABI: a `char*`.
const PAIRS_C: &str = r"
#include <string.h>
typedef struct { const char *p; unsigned long n; } S;
static long h(S s) { return s.n ? (long)s.n * 1000 + s.p[0] : 0; }
long rt10(S a, long b, long c, long d, S e, S f, long g, S h1, long i, S j) {
    return h(a) + b + c * 10 + d * 100 + h(e) * 2 + h(f) * 3 + g * 7 + h(h1) * 5 + i * 11 + h(j) * 13;
}
S rt_pick(long a, long b, long c, long d, long e, long f, long g, S s, long i) {
    S r = { s.p + (a + b + c + d + e + f + g + i - 35), s.n - 1 };
    return r;
}
static long hc(const char *s) { return (long)strlen(s) * 1000 + s[0]; }
long c10(const char *a, long b, long c, long d, const char *e, const char *f, long g,
         const char *h1, long i, const char *j) {
    return hc(a) + b + c * 10 + d * 100 + hc(e) * 2 + hc(f) * 3 + g * 7 + hc(h1) * 5 + i * 11 + hc(j) * 13;
}
";

const PAIRS_ROK: &str = r#"
extern "rok" {
    fn rt10(a: str, b: i64, c: i64, d: i64, e: str, f: str, g: i64, h: str, i: i64, j: str) -> i64;
    fn rt_pick(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, s: str, i: i64) -> str;
}

extern "C" {
    fn c10(a: str, b: i64, c: i64, d: i64, e: str, f: str, g: i64, h: str, i: i64, j: str) -> i64;
}

fn main() {
    let x = unsafe { rt10("a", 1, 2, 3, "bc", "def", 4, "ghij", 5, "klmno") };
    let y = unsafe { c10("a", 1, 2, 3, "bc", "def", 4, "ghij", 5, "klmno") };
    let s = unsafe { rt_pick(1, 2, 3, 4, 5, 6, 7, "xyzw", 8) };
    println(f"{x} {y} {s}");
}
"#;

#[test]
fn ten_argument_calls_with_strs() {
    let Some(env) = setup() else { return };
    let dir = scratch("pairs");
    fs::write(dir.join("side.c"), PAIRS_C).unwrap();
    fs::write(dir.join("main.rok"), PAIRS_ROK).unwrap();
    let obj = dir.join("main.o");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &["--emit=obj", "-o", obj.to_str().unwrap()],
    ));
    let exe = dir.join("out");
    ok(&Command::new("clang")
        .arg(&obj)
        .arg(dir.join("side.c"))
        .arg(&env.rokr)
        .args(["-lm", "-o"])
        .arg(&exe)
        .output()
        .unwrap());
    let run = Command::new(&exe).output().unwrap();
    ok(&run);
    // h(s) = len * 1000 + first byte.
    let h = |s: &str| s.len() as i64 * 1000 + i64::from(s.as_bytes()[0]);
    let want = h("a")
        + 1
        + 20
        + 300
        + h("bc") * 2
        + h("def") * 3
        + 28
        + h("ghij") * 5
        + 55
        + h("klmno") * 13;
    assert_eq!(text(&run.stdout), format!("{want} {want} yzw\n"));
    // x86_64 has six argument registers: `e` (words 5, 6) would
    // straddle, so it and every later pair go whole on the stack.
    let ll = dir.join("x86.ll");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &[
            "--emit=llvmir",
            "--target=x86_64-unknown-linux-gnu",
            "-o",
            ll.to_str().unwrap(),
        ],
    ));
    let x86 = fs::read_to_string(ll).unwrap();
    assert!(
        x86.contains(
            "declare i64 @rt10({ i64, i64 }, i64, i64, i64, ptr byval({ ptr, i64 }) align 8, \
             ptr byval({ ptr, i64 }) align 8, i64, ptr byval({ ptr, i64 }) align 8, i64, \
             ptr byval({ ptr, i64 }) align 8)"
        ),
        "{x86}"
    );
}

/// `#[repr(..)]` layouts against clang's (spec 6.2.2).
const REPR_C: &str = r"
#include <stddef.h>
typedef struct { char a; int b; char c; } C1;
typedef struct __attribute__((packed)) { char a; int b; short c; } P;
typedef struct { char x; P p; char y; } HasP;
typedef struct __attribute__((aligned(16))) { char a; } A;
typedef struct { char x; A a; char y; } Outer;
long layout(int i) {
    static const long t[] = {
        sizeof(C1), _Alignof(C1), offsetof(C1, b), offsetof(C1, c),
        sizeof(P), _Alignof(P), offsetof(P, b), offsetof(P, c),
        sizeof(HasP), offsetof(HasP, p), offsetof(HasP, y),
        sizeof(A), _Alignof(A), sizeof(Outer), offsetof(Outer, a), offsetof(Outer, y),
    };
    return t[i];
}
long c1_sum(C1 s) { return s.a * 100 + s.b * 10 + s.c; }
long p_sum(P p) { return p.a * 100 + p.b * 10 + p.c; }
long has_p(HasP h) { return h.x * 1000 + h.p.b * 10 + h.y; }
long outer(Outer o) { return o.x * 100 + o.a.a * 10 + o.y; }
P p_make(int b) { P p = { 1, b, 3 }; return p; }
double twice(double d) { return d * 2; }
";

const REPR_ROK: &str = r#"
use rok::bare::ptr::{align_of, size_of};

#[repr(C)]
struct C1 { a: i8; b: i32; c: i8; }
#[repr(packed)]
struct P { a: i8; b: i32; c: i16; }
struct HasP { x: i8; p: P; y: i8; }
#[repr(align(16))]
struct A { a: i8; }
struct Outer { x: i8; a: A; y: i8; }
#[repr(transparent)]
struct T { v: f64; }

extern "C" {
    fn layout(i: i32) -> i64;
    fn c1_sum(s: C1) -> i64;
    fn p_sum(p: P) -> i64;
    fn has_p(h: HasP) -> i64;
    fn outer(o: Outer) -> i64;
    fn p_make(b: i32) -> P;
    fn twice(t: T) -> T;
}

fn main() {
    let c = unsafe { [layout(0), layout(1), layout(4), layout(5), layout(8), layout(11), layout(12), layout(13)] };
    println(f"{c[0]} {c[1]} {c[2]} {c[3]} {c[4]} {c[5]} {c[6]} {c[7]}");
    println(f"{size_of::<C1>()} {align_of::<C1>()} {size_of::<P>()} {align_of::<P>()} {size_of::<HasP>()} {size_of::<A>()} {align_of::<A>()} {size_of::<Outer>()}");
    let p = unsafe { p_make(7) };
    unsafe {
        println(f"{c1_sum(C1 { a: 1, b: 2, c: 3 })} {p_sum(P { a: 4, b: 5, c: 6 })} {has_p(HasP { x: 1, p: P { a: 0, b: 2, c: 0 }, y: 3 })} {outer(Outer { x: 7, a: A { a: 8 }, y: 9 })}");
        println(f"{p.a} {p.b} {p.c} {twice(T { v: 1.25 }).v}");
    }
}
"#;

#[test]
fn repr_layouts_match_clang() {
    let Some(env) = setup() else { return };
    let dir = scratch("repr");
    fs::write(dir.join("side.c"), REPR_C).unwrap();
    fs::write(dir.join("main.rok"), REPR_ROK).unwrap();
    let obj = dir.join("main.o");
    let bare = env.workspace.join("library/bare/src/lib.rok");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &[
            "--bootstrap",
            bare.to_str().unwrap(),
            "--emit=obj",
            "-o",
            obj.to_str().unwrap(),
        ],
    ));
    let exe = dir.join("out");
    ok(&Command::new("clang")
        .arg(&obj)
        .arg(dir.join("side.c"))
        .arg(&env.rokr)
        .args(["-lm", "-o"])
        .arg(&exe)
        .output()
        .unwrap());
    let run = Command::new(&exe).output().unwrap();
    ok(&run);
    let out = text(&run.stdout);
    let lines: Vec<&str> = out.lines().collect();
    // clang's sizes / alignments, then rok's: the same.
    assert_eq!(lines[0], "12 4 7 1 9 16 16 48", "{out}");
    assert_eq!(lines[1], lines[0], "{out}");
    assert_eq!(lines[2], "123 456 1023 789", "{out}");
    assert_eq!(lines[3], "1 7 3 2.5", "{out}");
    // System V: a packed struct's unaligned fields make it MEMORY both
    // ways, like clang; the over-aligned one goes byval at 16.
    let ll = dir.join("x86.ll");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &[
            "--bootstrap",
            bare.to_str().unwrap(),
            "--emit=llvmir",
            "--target=x86_64-unknown-linux-gnu",
            "-o",
            ll.to_str().unwrap(),
        ],
    ));
    let x86 = fs::read_to_string(ll).unwrap();
    for want in [
        "declare i64 @c1_sum({ i64, i8 })",
        "declare i64 @p_sum(ptr byval(<{ i8, i32, i16 }>) align 8)",
        "declare void @p_make(ptr sret(<{ i8, i32, i16 }>) align 1, i32)",
        "@outer(ptr byval(<{ i8, [15 x i8], <{ i8, [15 x i8] }>, i8, [15 x i8] }>) align 16)",
        "declare double @twice(double)",
    ] {
        assert!(x86.contains(want), "x86_64 lacks `{want}`:\n{x86}");
    }
}

// spec: §9.3.7
#[test]
fn c_abi_matches_clang() {
    let Some(env) = setup() else { return };
    let dir = scratch("cabi");
    fs::write(dir.join("side.c"), C_SIDE).unwrap();
    fs::write(dir.join("main.rok"), ROK_SIDE).unwrap();
    let obj = dir.join("main.o");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &["--emit=obj", "-o", obj.to_str().unwrap()],
    ));
    let exe = dir.join("out");
    ok(&Command::new("clang")
        .arg(&obj)
        .arg(dir.join("side.c"))
        .arg(&env.rokr)
        .args(["-lm", "-o"])
        .arg(&exe)
        .output()
        .unwrap());
    let run = Command::new(&exe).output().unwrap();
    ok(&run);
    assert_eq!(
        text(&run.stdout),
        "2 4 6\n10.5\n5 10 15 30\n-2 301\n42 2.5\n6\n45\n-434465\n82\n0.75\n21\n18 321 10 5 -4 -5\n987\n2 4 6 321 654 -1 -2 321\n-5 714464\n65\n-3 -4.5 1.5 -7 10\n"
    );

    // The other targets' lowering, as LLVM IR declarations (what clang
    // emits for `side.c`, modulo clang splitting a SysV two-eightbyte
    // struct into two parameters, which LLVM does the same way).
    let decls = |triple: &str| {
        let ll = dir.join(format!("{triple}.ll"));
        ok(&build(
            &env,
            &dir.join("main.rok"),
            &[
                "--emit=llvmir",
                &format!("--target={triple}"),
                "-o",
                ll.to_str().unwrap(),
            ],
        ));
        fs::read_to_string(ll).unwrap()
    };
    let x86 = decls("x86_64-unknown-linux-gnu");
    for want in [
        "declare { <2 x float>, float } @v3_scale({ <2 x float>, float }, float)",
        "declare double @d4_sum(ptr byval({ double, double, double, double }) align 8)",
        "declare void @big_make(ptr sret({ i64, i64, i64 }) align 8, i64)",
        "declare i32 @small_inc(i32)",
        "declare { i64, double } @mix_twice({ i64, double })",
        "declare i32 @i3_sum({ i64, i32 })",
        // Only one GPR left for a two-eightbyte struct: all in memory.
        "declare i64 @many(i64, i64, i64, i64, i64, ptr byval({ i32, i32, i32 }) align 8, i64)",
        "declare i32 @sx(i8 signext, i16 zeroext)",
        // Narrow results too, and C-callable definitions both ways.
        "declare signext i8 @neg8(i8 signext)",
        "declare signext i8 @cid(i8 signext)",
        "signext i8 @rok_neg8(i8 signext",
        "zeroext i16 @rok_dbl16(i16 zeroext",
        "declare i64 @vsum(i32, ...)",
        "call double (i32, ...) @vmix(i32 2, i32",
    ] {
        assert!(x86.contains(want), "x86_64 lacks `{want}`:\n{x86}");
    }
    // A call through an `extern "C" fn(Big) -> Big` pointer: `sret` and
    // `byval` on the call site, as on a declaration.
    assert!(
        x86.lines().any(|l| l.contains("call void %")
            && l.contains("ptr sret({ i64, i64, i64 })")
            && l.contains("ptr byval({ i64, i64, i64 })")),
        "x86_64 lacks an indirect C call with sret / byval:\n{x86}"
    );
    // Direct calls too: alias analysis reads `byval` off the call only
    // (a `tail` call's unmarked caller copy is dead to it).
    assert!(
        x86.contains("call double @d4_sum(ptr byval({ double, double, double, double }) align 8"),
        "x86_64 lacks a direct C call with byval:\n{x86}"
    );
    let a64 = decls("aarch64-unknown-linux-gnu");
    for want in [
        "declare [3 x float] @v3_scale([3 x float], float)",
        "declare double @d4_sum([4 x double])",
        "declare void @big_make(ptr sret({ i64, i64, i64 }) align 8, i64)",
        "declare i64 @big_sum(ptr)",
        "declare i32 @small_inc(i64)",
        "declare [2 x i64] @mix_twice([2 x i64])",
        "declare i64 @many(i64, i64, i64, i64, i64, [2 x i64], i64)",
        // AAPCS64 leaves the upper bits unspecified: no extension.
        "declare i32 @sx(i8, i16)",
        "declare i8 @neg8(i8)",
    ] {
        assert!(a64.contains(want), "aarch64 lacks `{want}`:\n{a64}");
    }
    let apple = decls("aarch64-apple-darwin");
    for want in [
        "declare i32 @sx(i8 signext, i16 zeroext)",
        "declare signext i8 @neg8(i8 signext)",
        "zeroext i16 @rok_dbl16(i16 zeroext",
    ] {
        assert!(apple.contains(want), "Apple arm64 lacks `{want}`:\n{apple}");
    }
    // C's `char` is unsigned on 32-bit ARM Linux: a `c_char` crosses
    // zero-extended (the bits are rok's signed `c_char`).
    let arm = decls("armv7-unknown-linux-gnueabihf");
    for want in [
        "declare zeroext i8 @cid(i8 zeroext)",
        "declare signext i8 @neg8(i8 signext)",
    ] {
        assert!(arm.contains(want), "armv7 lacks `{want}`:\n{arm}");
    }
    // RISC-V LP64D: float structs as their fields (LLVM splits a
    // first-class struct as clang expands it), 32-bit integers
    // sign-extended, `u32` too; C `char` is unsigned.
    let rv = decls("riscv64gc-unknown-linux-gnu");
    for want in [
        "declare { float, double } @fd_mul({ float, double }, i32 signext)",
        "declare { float, i32 } @fi_inc({ float, i32 })",
        "declare { i64, double } @mix_twice({ i64, double })",
        "declare [2 x i64] @v3_scale([2 x i64], float)",
        "declare double @d4_sum(ptr)",
        "declare void @big_make(ptr sret({ i64, i64, i64 }) align 8, i64)",
        "declare i64 @small_inc(i64)",
        "declare signext i32 @i3_sum([2 x i64])",
        "declare signext i32 @sx(i8 signext, i16 zeroext)",
        "declare signext i32 @ult(i32 signext, i32 signext)",
        "declare zeroext i8 @cid(i8 zeroext)",
        "declare i64 @vsum(i32 signext, ...)",
    ] {
        assert!(rv.contains(want), "riscv64 lacks `{want}`:\n{rv}");
    }
}

/// A by-address aggregate (spec 8.6.6) is copied with its alignment:
/// copies of unknown alignment become byte loads and stores where
/// unaligned accesses are slow (RISC-V without fast misaligned access).
#[test]
fn by_address_copies_keep_their_alignment() {
    let Some(env) = setup() else { return };
    let dir = scratch("aligned_copies");
    let src = dir.join("main.rok");
    fs::write(
        &src,
        "fn twice(a: [i64; 10]) -> [i64; 10] {\n    let b = a;\n    for i in 0usize..10 { b[i] = a[i] * 2; }\n    b\n}\n\
         fn main() -> i32 { twice([1i64; 10])[3] as i32 - 2 }\n",
    )
    .unwrap();
    let ll = dir.join("main.ll");
    ok(&build(
        &env,
        &src,
        &[
            "--emit=llvmir",
            "--target=riscv64gc-unknown-linux-gnu",
            "-o",
            ll.to_str().unwrap(),
        ],
    ));
    let ir = fs::read_to_string(ll).unwrap();
    let copies: Vec<&str> = ir
        .lines()
        .filter(|l| l.contains("@llvm.memcpy") && l.contains("i64 80,"))
        .collect();
    assert!(!copies.is_empty(), "no 80-byte copy:\n{ir}");
    for c in copies {
        assert!(
            c.contains("(ptr align 8 %") && c.contains(", ptr align 8 %"),
            "unaligned copy `{c}`"
        );
    }
}

/// `#[link(..)]` on an `extern` block puts the library on the link
/// line (spec 6.2.3); `LIBRARY_PATH` is the C driver's search path.
// spec: §9.2.14
#[test]
fn link_attribute_links_the_library() {
    let Some(env) = setup() else { return };
    let dir = scratch("link_attr");
    fs::write(
        dir.join("probe.c"),
        "int rok_link_probe(int x) { return x * 3; }\n",
    )
    .unwrap();
    let obj = dir.join("probe.o");
    ok(&Command::new("clang")
        .args(["-c", "-o"])
        .arg(&obj)
        .arg(dir.join("probe.c"))
        .output()
        .unwrap());
    for lib in ["roklinka", "roklinkb"] {
        ok(&Command::new("ar")
            .arg("rcs")
            .arg(dir.join(format!("lib{lib}.a")))
            .arg(&obj)
            .output()
            .unwrap());
    }
    let src = r#"
#[link("roklinka")]
extern "C" {
    fn rok_link_probe(x: i32) -> i32;
}

#[link(name = "roklinkb", kind = "static")]
extern "C" {}

fn main() -> i32 {
    unsafe { rok_link_probe(14) }
}
"#;
    fs::write(dir.join("main.rok"), src).unwrap();
    let exe = dir.join("out");
    let out = Command::new(ROKC)
        .arg("-q")
        .arg("build")
        .arg(dir.join("main.rok"))
        .arg("--bootstrap")
        .arg(&env.bootstrap)
        .arg("--rokr")
        .arg(&env.rokr)
        .arg("-o")
        .arg(&exe)
        .env("LIBRARY_PATH", &dir)
        .output()
        .unwrap();
    ok(&out);
    let run = Command::new(&exe).output().unwrap();
    assert_eq!(run.status.code(), Some(42));
}

/// An `extern "C" fn` called on threads rok did not start: the first
/// call attaches the thread, enough allocation to collect runs, and the
/// threads leave the registry when they end (the second round collects
/// with them gone).
// spec: §8.3.8
#[test]
fn foreign_thread_callback_allocates() {
    let Some(env) = setup() else { return };
    let dir = scratch("foreign_thread");
    fs::write(
        dir.join("side.c"),
        r"
#include <pthread.h>
typedef long (*cb_t)(long);
static cb_t g_cb;
static void *run(void *a) {
    long n = (long)a;
    long t = g_cb(n);
    return (void *)(t + g_cb(n));
}
long run_threads(cb_t f, long n) {
    pthread_t th[4];
    long total = 0;
    g_cb = f;
    for (int i = 0; i < 4; i++) pthread_create(&th[i], 0, run, (void *)n);
    for (int i = 0; i < 4; i++) {
        void *r;
        pthread_join(th[i], &r);
        total += (long)r;
    }
    return total;
}
",
    )
    .unwrap();
    fs::write(
        dir.join("main.rok"),
        r#"
extern "C" {
    fn run_threads(f: fn(i64) -> i64, n: i64) -> i64;
}

extern "C" fn fill(n: i64) -> i64 {
    let xs: List<i64> = List::new();
    for i in 0..n { xs.push(i); }
    xs.len() as i64
}

fn main() -> i32 {
    let a = unsafe { run_threads(fill, 2000000) };
    let b = unsafe { run_threads(fill, 2000000) };
    let c = fill(3000000);
    if a == 16000000 && b == 16000000 && c == 3000000 { 0 } else { 1 }
}
"#,
    )
    .unwrap();
    let obj = dir.join("main.o");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &["--emit=obj", "-o", obj.to_str().unwrap()],
    ));
    let exe = dir.join("out");
    ok(&Command::new("clang")
        .arg(&obj)
        .arg(dir.join("side.c"))
        .arg(&env.rokr)
        .args(["-pthread", "-lm", "-o"])
        .arg(&exe)
        .output()
        .unwrap());
    for _ in 0..5 {
        ok(&Command::new(&exe).output().unwrap());
    }
}

/// `kind = "static"` takes `lib<name>.a` from the `-L` directories even
/// beside a shared library of the same name (ld64 would take the
/// `.dylib`); a missing archive is an error naming it.
#[test]
fn static_link_kind_takes_the_archive() {
    let Some(env) = setup() else { return };
    let dir = scratch("link_static");
    let libs = dir.join("libs");
    fs::create_dir_all(&libs).unwrap();
    let c = |file: &str, body: &str, args: &[&str]| {
        let src = dir.join(format!("{file}.c"));
        fs::write(&src, body).unwrap();
        ok(&Command::new("clang")
            .args(args)
            .arg("-o")
            .arg(dir.join(file))
            .arg(&src)
            .output()
            .unwrap());
    };
    c(
        "a.o",
        "int rok_static_probe(int x) { return x * 3; }\n",
        &["-c"],
    );
    ok(&Command::new("ar")
        .arg("rcs")
        .arg(libs.join("librokstatic.a"))
        .arg(dir.join("a.o"))
        .output()
        .unwrap());
    let shared = if cfg!(target_os = "macos") {
        "librokstatic.dylib"
    } else {
        "librokstatic.so"
    };
    c(
        &format!("libs/{shared}"),
        "int rok_static_probe(int x) { return x * 5; }\n",
        &["-shared", "-fPIC"],
    );
    let src = r#"
#[link(name = "rokstatic", kind = "static")]
extern "C" {
    fn rok_static_probe(x: i32) -> i32;
}

#[link(name = "rokmissing", kind = "static")]
#[cfg(missing)]
extern "C" {}

fn main() -> i32 {
    unsafe { rok_static_probe(14) }
}
"#;
    fs::write(dir.join("main.rok"), src).unwrap();
    let exe = dir.join("out");
    let exe_arg = exe.display().to_string();
    let lib_arg = format!("-L{}", libs.display());
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &[&lib_arg, "-o", &exe_arg],
    ));
    let run = Command::new(&exe).output().unwrap();
    assert_eq!(run.status.code(), Some(42));
    let out = build(
        &env,
        &dir.join("main.rok"),
        &[
            "-L",
            &libs.display().to_string(),
            "--cfg",
            "missing",
            "-o",
            &exe_arg,
        ],
    );
    assert!(!out.status.success());
    // ld64 has no -Bstatic: rokc looks for the archive itself there.
    let want = if cfg!(target_os = "macos") {
        "no librokmissing.a in the -L directories"
    } else {
        "unable to find library -lrokmissing"
    };
    assert!(text(&out.stderr).contains(want), "{}", text(&out.stderr));
}

/// `-l` links a library the source does not name; `--native-runtime`
/// copies a shared library beside the executable, which then loads it
/// from its own directory (rpath `@loader_path` / `$ORIGIN`).
// spec: §9.1.2 §9.2.14
#[cfg(unix)]
#[test]
fn link_flags_and_runtime_files() {
    let Some(env) = setup() else { return };
    let dir = scratch("link_runtime");
    let libs = dir.join("libs");
    fs::create_dir_all(&libs).unwrap();
    let (shared, id) = if cfg!(target_os = "macos") {
        ("librokrt.dylib", "-Wl,-install_name,@rpath/librokrt.dylib")
    } else {
        ("librokrt.so", "-Wl,-soname,librokrt.so")
    };
    fs::write(
        dir.join("rt.c"),
        "int rok_rt_probe(int x) { return x * 3; }\n",
    )
    .unwrap();
    ok(&Command::new("clang")
        .args(["-shared", "-fPIC", id, "-o"])
        .arg(libs.join(shared))
        .arg(dir.join("rt.c"))
        .output()
        .unwrap());
    let src = "extern \"C\" {\n    fn rok_rt_probe(x: i32) -> i32;\n}\n\nfn main() -> i32 {\n    \
               unsafe { rok_rt_probe(14) }\n}\n";
    fs::write(dir.join("main.rok"), src).unwrap();
    let out_dir = dir.join("out");
    fs::create_dir_all(&out_dir).unwrap();
    let exe = out_dir.join("app");
    let lib_dir = format!("-L{}", libs.display());
    let runtime = format!("--native-runtime={}", libs.join(shared).display());
    let exe_arg = exe.display().to_string();
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &[&lib_dir, "-l", "dylib=rokrt", &runtime, "-o", &exe_arg],
    ));
    assert!(out_dir.join(shared).is_file());
    // The library's own directory is gone: only the copy can be loaded.
    fs::remove_dir_all(&libs).unwrap();
    let run = Command::new(&exe).current_dir(&dir).output().unwrap();
    assert_eq!(run.status.code(), Some(42), "{}", text(&run.stderr));
    let out = build(&env, &dir.join("main.rok"), &["-l", "shared=x"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("expected [static=|dylib=]<name>"),
        "{}",
        text(&out.stderr)
    );
}

/// A stand-in Windows runtime (what a hello-sized program calls) and
/// the C side of the ABI checks, `long` widened to 64 bits.
const WIN_C_SIDE: &str = r#"
typedef __builtin_va_list va_list;
#define va_start(a, b) __builtin_va_start(a, b)
#define va_arg(a, t) __builtin_va_arg(a, t)
#define va_end(a) __builtin_va_end(a)
typedef __SIZE_TYPE__ usize;
#define long long long
typedef struct { const char *p; usize n; } RokStr;
__declspec(thread) __attribute__((aligned(8))) unsigned char rok_tls[13 * sizeof(void *) + 16];
int _write(int, const void *, unsigned);
void *calloc(usize, usize);
void exit(int);
void rok_runtime_init(void) {}
void rok_runtime_shutdown(void) {}
void rok_runtime_yield(void) {}
void rok_enter_native(void) {}
void rok_leave_native(void) {}
int rok_native_reenter(void) { return 0; }
void rok_native_resume(int x) { (void)x; }
void rok_nosp_enter(void) {}
void rok_nosp_leave(void) {}
unsigned long rok_hash_seed(void) { return 1; }
void rok_gc_add_root(const void *w) { (void)w; }
void *rok_gc_alloc_slow(usize size, usize align, unsigned t) {
    char *p = calloc(1, size + align + 16); *(unsigned *)p = t; return p;
}
void rok_gc_register_type(unsigned id, const void *t) { (void)id; (void)t; }
void rok_gc_wb_slow(void *o) { (void)o; }
void rok_panic_at(RokStr m, RokStr at) { _write(2, m.p, (unsigned)m.n); (void)at; exit(101); }
int rok_static_init_begin(unsigned char *s, const char *at, usize n) { (void)at; (void)n; if (*s == 0) { *s = 1; return 1; } return 0; }
void rok_static_init_end(unsigned char *s) { *s = 2; }
usize rok_str_len(RokStr s) { return s.n; }
int rok_str_cmp(RokStr a, RokStr b) {
    if (a.n != b.n) return 1;
    for (usize i = 0; i < a.n; i++) if (a.p[i] != b.p[i]) return 1;
    return 0;
}
int rok_str_eq(RokStr a, RokStr b) { return !rok_str_cmp(a, b); }
int rok_stdio_write(int fd, RokStr s, int nl) {
    _write(fd, s.p, (unsigned)s.n); if (nl) _write(fd, "\n", 1); return 0;
}
int side_count = 42;
void show(long v) {
    char b[32]; int n = 31; int neg = v < 0;
    unsigned long u = neg ? -(unsigned long)v : v;
    b[n] = '\n';
    do { b[--n] = '0' + u % 10; u /= 10; } while (u);
    if (neg) b[--n] = '-';
    _write(1, b + n, 32 - n);
}
typedef struct { float x, y, z; } V3;
typedef struct { long a, b, c; } Big;
typedef struct { signed char a; short b; } Small;
typedef struct { long a; double b; } Mix;
typedef struct { int a, b, c; } I3;
V3 v3_scale(V3 v, float k) { V3 r = { v.x * k, v.y * k, v.z * k }; return r; }
Big big_make(long a) { Big b = { a, a * 2, a * 3 }; return b; }
long big_sum(Big b) { return b.a + b.b + b.c; }
Small small_inc(Small s) { s.a += 1; s.b += 1; return s; }
Mix mix_twice(Mix m) { m.a *= 2; m.b *= 2; return m; }
long many(long a, long b, long c, long d, long e, I3 s, long f) {
    return a + b + c + d + e + s.a + s.b + s.c + f;
}
double fmany(double a, long b, double c, int d, float e, double f, long g) {
    return a + b * 10 + c * 100 + d * 1000 + e * 10000 + f * 100000 + g * 1000000;
}
double vmix(int n, ...) {
    va_list ap; va_start(ap, n); double t = 0;
    for (int i = 0; i < n; i++) { t += va_arg(ap, int); t += va_arg(ap, double); }
    va_end(ap); return t;
}
long cb_big(Big (*f)(Big)) { Big b = { 1, 2, 3 }; Big r = f(b); return r.a * 100 + r.b * 10 + r.c; }
Small cb_small(Small (*f)(Small)) { Small s = { 4, 5 }; return f(s); }
"#;

const WIN_ROK_SIDE: &str = r#"
struct V3 { x: f32; y: f32; z: f32; }
struct Big { a: i64; b: i64; c: i64; }
struct Small { a: i8; b: i16; }
struct Mix { a: i64; b: f64; }
struct I3 { a: i32; b: i32; c: i32; }

extern "C" {
    fn show(v: i64);
    fn v3_scale(v: V3, k: f32) -> V3;
    fn big_make(a: i64) -> Big;
    fn big_sum(b: Big) -> i64;
    fn small_inc(s: Small) -> Small;
    fn mix_twice(m: Mix) -> Mix;
    fn many(a: i64, b: i64, c: i64, d: i64, e: i64, s: I3, f: i64) -> i64;
    fn fmany(a: f64, b: i64, c: f64, d: i32, e: f32, f: f64, g: i64) -> f64;
    fn vmix(n: i32, ...) -> f64;
    fn cb_big(f: fn(Big) -> Big) -> i64;
    fn cb_small(f: fn(Small) -> Small) -> Small;
}

extern "C" fn rev_big(b: Big) -> Big { Big { a: b.c, b: b.b, c: b.a } }
extern "C" fn neg_small(s: Small) -> Small { Small { a: -s.a, b: -s.b } }

// rok to rok: arguments past the four register positions.
fn mixed(a: i64, s: str, b: f64, t: Big, c: i32, d: f32, u: str, e: i64) -> i64 {
    a + s.len() as i64 * 10 + (b * 100.0) as i64 + t.a * 1000 + t.c * 10000
        + c as i64 * 100000 + (d * 1000000.0) as i64 + u.len() as i64 * 10000000
        + e * 100000000
}

fn main() {
    println("hello from rok");
    unsafe {
        let v = v3_scale(V3 { x: 1.0, y: 2.0, z: 3.0 }, 2.0);
        show((v.x * 100.0 + v.y * 10.0 + v.z) as i64);
        let b = big_make(5);
        show(b.a * 100 + b.b * 10 + b.c + big_sum(b) * 1000);
        let s = small_inc(Small { a: -3, b: 300 });
        show(s.a as i64 * 1000 + s.b as i64);
        let m = mix_twice(Mix { a: 21, b: 1.25 });
        show(m.a * 100 + (m.b * 10.0) as i64);
        show(many(1, 2, 3, 4, 5, I3 { a: 6, b: 7, c: 8 }, 9));
        show(fmany(1.0, 2, 3.0, 4, 5.0, 6.0, 7) as i64);
        let small: i8 = -2;
        let half: f32 = 0.5;
        show((vmix(2, small, half, true, 1.25) * 100.0) as i64);
        show(cb_big(rev_big));
        let n = cb_small(neg_small);
        show(n.a as i64 * 1000 + n.b as i64);
        show(mixed(1, "ab", 0.5, Big { a: 3, b: 0, c: 4 }, 5, 0.25, "xyz", 6));
        let k = "xyz";
        show(if k == "xyz" { 1 } else { 0 });
        // Data of a DLL, and of an object (lld-link makes its import
        // address).
        show(if NlsAnsiCodePage > 0u16 { 1 } else { 0 });
        side_count += 1;
        show(side_count as i64);
    }
}

#[link("ntdll")]
extern "system" {
    static NlsAnsiCodePage: u16;
}

extern "C" {
    static mut side_count: i32;
}
"#;

const WIN_EXPECTED: &str =
    "hello from rok\n246\n30615\n-1699\n4225\n45\n7654321\n75\n321\n-4005\n630793071\n1\n1\n43\n";

/// `(machine, subsystem, unwind table present)` of a PE image.
fn pe_header(exe: &[u8]) -> (u16, u16, bool) {
    assert_eq!(&exe[..2], b"MZ");
    let pe = u32::from_le_bytes(exe[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&exe[pe..pe + 4], b"PE\0\0");
    let opt = pe + 24;
    // The exception directory (`.pdata`): data directory 3, after a
    // PE32+ / PE32 optional header's 112 / 96 bytes.
    let dirs = if exe[opt] == 0x0b && exe[opt + 1] == 0x02 {
        112
    } else {
        96
    };
    let pdata = opt + dirs + 3 * 8 + 4;
    (
        u16::from_le_bytes([exe[pe + 4], exe[pe + 5]]),
        u16::from_le_bytes([exe[opt + 68], exe[opt + 69]]),
        u32::from_le_bytes(exe[pdata..pdata + 4].try_into().unwrap()) > 0,
    )
}

/// Windows executables from this host without an SDK; run under
/// `ROK_TEST_WINE` where given.
// spec: §9.2.14 §9.5.9
#[test]
fn windows_executables() {
    let Some(env) = setup() else { return };
    let dir = scratch("windows");
    fs::write(dir.join("side.c"), WIN_C_SIDE).unwrap();
    let pkg = dir.join("winpkg");
    fs::create_dir_all(pkg.join("src")).unwrap();
    fs::write(pkg.join("src/main.rok"), WIN_ROK_SIDE).unwrap();
    let wine = std::env::var_os("ROK_TEST_WINE").filter(|w| !w.is_empty());
    for (triple, machine, builds) in [
        ("x86_64-pc-windows-msvc", 0x8664, &["-O0", "-O2"][..]),
        ("aarch64-pc-windows-msvc", 0xaa64, &["-O0"][..]),
        ("i686-pc-windows-msvc", 0x14c, &["-O0", "-O2"][..]),
    ] {
        let side = dir.join(format!("{triple}.obj"));
        ok(&Command::new("clang")
            .args(["-target", triple, "-O1", "-c", "-o"])
            .arg(&side)
            .arg(dir.join("side.c"))
            .output()
            .unwrap());
        for (i, opt) in builds.iter().enumerate() {
            let profile = if *opt == "-O0" { "debug" } else { "release" };
            let exe = pkg.join(format!("target/{triple}/{profile}/winpkg.exe"));
            let gui = i == builds.len() - 1 && machine == 0xaa64;
            let mut rokc = Command::new(ROKC);
            rokc.args(["-q", "build"])
                .arg(&pkg)
                .arg("--bootstrap")
                .arg(&env.bootstrap)
                .arg("--rokr")
                .arg(&side)
                .args(["--target", triple, opt]);
            if gui {
                rokc.args(["-C", "subsystem=windows"]);
            }
            ok(&rokc.output().unwrap());
            let sub = if gui { 2 } else { 3 };
            assert_eq!(
                pe_header(&fs::read(&exe).unwrap()),
                (machine, sub, machine != 0x14c),
                "{triple} {opt}"
            );
            if let (Some(wine), false) = (&wine, machine == 0xaa64) {
                let run = Command::new(wine)
                    .arg(&exe)
                    .env("WINEDEBUG", "-all")
                    .output()
                    .unwrap();
                assert_eq!(
                    text(&run.stdout).replace('\r', ""),
                    WIN_EXPECTED,
                    "{triple} {opt}"
                );
            }
        }
    }
}

/// Calls that touch only a `List` held by a local that only ever takes
/// lists fresh from a call carry that local's alias scope and the other
/// such locals' as `noalias`; a list from any other call gets none.
#[test]
fn list_alias_scopes() {
    let Some(env) = setup() else { return };
    let dir = scratch("listscope");
    fs::write(
        dir.join("main.rok"),
        r#"
fn id(xs: List<f64>) -> List<f64> { xs }

fn main() -> i32 {
    let a: List<f64> = [1.0, 2.0];
    let b: List<f64> = [3.0, 4.0];
    let c = id(a);
    b[1] = a[0] + c[1];
    println(f"{b[1]}");
    return 0;
}
"#,
    )
    .unwrap();
    let ll = dir.join("main.ll");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &["-O2", "--emit=llvmir", "-o", ll.to_str().unwrap()],
    ));
    let ir = fs::read_to_string(ll).unwrap();
    let main = ir.split("__main_(").nth(1).unwrap();
    let main = &main[..main.find("\n}\n").unwrap()];
    let scoped = |set: bool| {
        main.lines()
            .filter(|l| {
                l.contains(" call ") && l.contains("0index_") && l.contains("0index_0set_") == set
            })
            .map(|l| l.contains("!alias.scope") && l.contains("!noalias"))
            .collect::<Vec<_>>()
    };
    // `a[0]`, `c[1]`, `b[1]`; `b[1] = ..`.
    assert_eq!(scoped(false), [true, false, true], "{main}");
    assert_eq!(scoped(true), [true], "{main}");
}

/// A counted loop indexing lists with its counter and an invariant index
/// is copied: the copy, behind a bounds test, calls accessors without
/// the check (no panic in them); the original keeps the checked ones.
#[test]
fn bounds_versioned_loops() {
    let Some(env) = setup() else { return };
    let dir = scratch("boundsversion");
    fs::write(
        dir.join("main.rok"),
        r#"
fn f(x: List<f64>, y: List<f64>, i: usize, n: usize) -> f64 {
    let s = 0.0;
    for j in i + 1..n {
        s += x[i] - x[j];
        y[j] = s;
    }
    s
}

fn main() -> i32 {
    let x: List<f64> = [1.0, 2.0, 3.0];
    println(f"{f(x, x, 0, 3)}");
    return 0;
}
"#,
    )
    .unwrap();
    let ll = dir.join("main.ll");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &["-O2", "--emit=llvmir", "-o", ll.to_str().unwrap()],
    ));
    let ir = fs::read_to_string(ll).unwrap();
    // The definitions whose name has `part`.
    let defs = |part: &str| {
        ir.split("\ndefine ")
            .skip(1)
            .filter(|d| d.lines().next().unwrap().contains(part))
            .map(|d| d[..d.find("\n}\n").unwrap()].to_owned())
            .collect::<Vec<_>>()
    };
    let f = defs("__f_(").remove(0);
    let calls = |unchecked: bool, set: bool| {
        f.lines()
            .filter(|l| {
                l.contains(" call ")
                    && l.contains("0index_")
                    && l.contains("_123_unchecked_") == unchecked
                    && l.contains("0index_0set_") == set
            })
            .count()
    };
    // `x[i]`, `x[j]`; `y[j] = s`: once unchecked, once checked.
    assert_eq!((calls(true, false), calls(true, true)), (2, 1), "{f}");
    assert_eq!((calls(false, false), calls(false, true)), (2, 1), "{f}");
    let unchecked = defs("_123_unchecked_");
    assert_eq!(unchecked.len(), 2);
    for d in unchecked {
        assert!(!d.contains("panic"), "{d}");
    }
}

/// A nest of counted loops gets a copy entered when its bounds are at
/// most 8, polling nowhere; the nest as written polls.
#[test]
fn short_nest_polls_nowhere() {
    let Some(env) = setup() else { return };
    let dir = scratch("shortnest");
    fs::write(
        dir.join("main.rok"),
        r#"
fn square(n: usize) -> usize {
    let c: usize = 0;
    for i in 0..n {
        for j in 0..n {
            c += i * 10 + j;
        }
    }
    c
}

fn main() -> i32 {
    println(f"{square(3)}");
    return 0;
}
"#,
    )
    .unwrap();
    let out = |emit: &str, file: &str| {
        let path = dir.join(file);
        ok(&build(
            &env,
            &dir.join("main.rok"),
            &["-O2", emit, "-o", path.to_str().unwrap()],
        ));
        fs::read_to_string(path).unwrap()
    };
    let mir = out("--emit=mir", "main.mir");
    let square = &mir[mir.find("fn square").unwrap()..];
    let square = &square[..square.find("\nfn ").unwrap_or(square.len())];
    assert_eq!(square.matches(", 8usize)").count(), 2, "{square}");
    let ir = out("--emit=llvmir", "main.ll");
    let d = ir
        .split("\ndefine ")
        .find(|d| d.lines().next().unwrap().contains("__square_("))
        .unwrap();
    let polls = d[..d.find("\n}\n").unwrap()]
        .lines()
        .filter(|l| l.contains(" call ") && l.contains("rok_runtime_yield"))
        .count();
    assert_eq!(polls, 2, "the nest as written polls in both loops");
}

/// A versioned loop keeps invariant `a[i]` in a local: one load in front,
/// one store on exit. An overflow-checked sum keeps it in memory.
#[test]
fn versioned_loop_keeps_an_element() {
    let Some(env) = setup() else { return };
    let dir = scratch("keptelement");
    fs::write(
        dir.join("main.rok"),
        r#"
fn pull(a: List<f64>, b: List<f64>, i: usize, n: usize) {
    for j in i + 1..n {
        a[i] += b[j];
        b[j] += 1.0;
    }
}

fn count(a: List<i64>, b: List<i64>, i: usize, n: usize) {
    for j in i + 1..n {
        a[i] += b[j];
    }
}

fn main() -> i32 {
    let a: List<f64> = [0.0, 1.0, 2.0];
    pull(a, [0.0, 1.0, 2.0], 0, 3);
    count([0, 1], [0, 1], 0, 2);
    println(f"{a:?}");
    return 0;
}
"#,
    )
    .unwrap();
    let ll = dir.join("main.ll");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &["-O2", "--emit=llvmir", "-o", ll.to_str().unwrap()],
    ));
    let ir = fs::read_to_string(ll).unwrap();
    let sets = |name: &str| {
        let d = ir
            .split("\ndefine ")
            .find(|d| d.lines().next().unwrap().contains(name))
            .unwrap();
        d[..d.find("\n}\n").unwrap()]
            .lines()
            .filter(|l| l.contains(" call ") && l.contains("0index_0set_123_unchecked_"))
            .count()
    };
    // `a[i]`, `b[j]` in the unchecked copy; `b[j]` and the store on the
    // way out in the one keeping `a[i]`.
    assert_eq!(sets("__pull_("), 4);
    assert_eq!(sets("__count_("), 1);
}

/// A two-pointer loop (`lo < hi`, stepped toward each other) and an index
/// `ctr + k` are versioned too: each access has an unchecked copy.
#[test]
fn bounds_versioned_two_pointers_and_offsets() {
    let Some(env) = setup() else { return };
    let dir = scratch("boundsversion2");
    fs::write(
        dir.join("main.rok"),
        r#"
fn rev(xs: List<i64>, lo: i64, hi: i64) {
    while lo < hi {
        let t = xs[lo as usize];
        xs[lo as usize] = xs[hi as usize];
        xs[hi as usize] = t;
        lo += 1;
        hi -= 1;
    }
}

fn shift(xs: List<i64>, n: usize) {
    for i in 0..n {
        xs[i] = xs[i + 1];
    }
}

fn main() -> i32 {
    let x: List<i64> = [1, 2, 3];
    rev(x, 0, 2);
    shift(x, 2);
    println(f"{x:?}");
    return 0;
}
"#,
    )
    .unwrap();
    let ll = dir.join("main.ll");
    ok(&build(
        &env,
        &dir.join("main.rok"),
        &["-O2", "--emit=llvmir", "-o", ll.to_str().unwrap()],
    ));
    let ir = fs::read_to_string(ll).unwrap();
    let calls = |name: &str, unchecked: bool| {
        let d = ir
            .split("\ndefine ")
            .find(|d| d.lines().next().unwrap().contains(name))
            .unwrap();
        d[..d.find("\n}\n").unwrap()]
            .lines()
            .filter(|l| {
                l.contains(" call ")
                    && l.contains("0index_")
                    && l.contains("_123_unchecked_") == unchecked
            })
            .count()
    };
    // `rev`: two loads, two stores, in the unchecked copy twice (one for
    // `hi - lo <= 64`, polling nowhere); `shift`: one of each.
    assert_eq!((calls("__rev_(", true), calls("__rev_(", false)), (8, 4));
    let rev = ir
        .split("\ndefine ")
        .find(|d| d.lines().next().unwrap().contains("__rev_("))
        .unwrap();
    let polls = rev[..rev.find("\n}\n").unwrap()]
        .lines()
        .filter(|l| l.contains(" call ") && l.contains("rok_runtime_yield"))
        .count();
    assert_eq!(polls, 2, "the checked loop and the long unchecked one poll");
    assert_eq!(
        (calls("__shift_(", true), calls("__shift_(", false)),
        (2, 2)
    );
}

/// A loop that calls a closure or a `fn` value, directly or through a
/// function, keeps its checks: the callee may shrink the list. The same
/// loop calling nothing is versioned.
#[test]
fn bounds_versioning_skips_indirect_calls() {
    let Some(env) = setup() else { return };
    let dir = scratch("boundsindirect");
    let versioned = |call: &str| {
        fs::write(
            dir.join("main.rok"),
            format!(
                r#"
fn run(f: fn()) {{
    f();
}}

fn sum(x: List<i64>, n: usize, f: fn()) -> i64 {{
    let s: i64 = 0;
    for j in 0..n {{
        {call}
        s += x[j];
    }}
    s
}}

fn main() -> i32 {{
    let x: List<i64> = [1, 2, 3];
    println(f"{{sum(x, 3, || {{}})}}");
    return 0;
}}
"#
            ),
        )
        .unwrap();
        let ll = dir.join("main.ll");
        ok(&build(
            &env,
            &dir.join("main.rok"),
            &["-O2", "--emit=llvmir", "-o", ll.to_str().unwrap()],
        ));
        fs::read_to_string(ll).unwrap().contains("_123_unchecked_")
    };
    assert!(versioned(""));
    assert!(!versioned("f();"));
    assert!(!versioned("run(f);"));
}

/// `print` / `println` take any `Display` value and default the argument to
/// the empty string, so `println()` writes a blank line (0.1.1, #1 / #2).
const PRINTING: &str = "fn main() {
    println(\"line\");
    println(42);
    println(3.5f64);
    println(true);
    print(\"a\");
    print(\"b\");
    println();
    println(f\"x={1 + 2}\");
    println();
}
";

#[test]
fn printing_display_and_blank_line() {
    let Some(env) = setup() else { return };
    let dir = scratch("printing");
    fs::write(dir.join("main.rok"), PRINTING).unwrap();
    let exe = dir.join("out");
    let out = Command::new(ROKC)
        .args(["-q", "build"])
        .arg(dir.join("main.rok"))
        .arg("--bootstrap")
        .arg(&env.bootstrap)
        .arg("--bootstrap")
        .arg(env.workspace.join("library/std/src/lib.rok"))
        .arg("--rokr")
        .arg(&env.rokr)
        .args(std::env::var("ROK_TEST_OPT").ok().filter(|o| !o.is_empty()))
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    ok(&out);
    let run = Command::new(&exe).output().unwrap();
    ok(&run);
    assert_eq!(text(&run.stdout), "line\n42\n3.5\ntrue\nab\nx=3\n\n");
}
