//! Seeded fuzzing of the front end with mutated corpus cases and grammar
//! generated programs.
//!
//! - `front_end_in_process`: lexer, parser and formatter in process: no
//!   panic; clean input formats to text that parses and formats to itself.
//! - `rokc_check`: `rokc check -` gives a diagnostic or success, never an
//!   internal error, a signal or a hang.
//!
//! `ROK_FUZZ_SECS` (default 5) per test; `ROK_FUZZ_SEED` picks another
//! input sequence. A crash is minimized into
//! `tests/fuzz_crashes/<target>_<hash>.rok`; one whose signature is already
//! there is known, a new one fails. `known_crashes` lists fixed ones.

mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::{ROKC, Rng, par_map, workspace};

const DEFAULT_SEED: u64 = 0x005e_ed0f_f022;
/// CPU seconds after which a `rokc check` is a hang (a debug `rokc`
/// checks a case in about a second): `RLIMIT_CPU`, so a loaded machine
/// does not stretch it. Unix only.
#[cfg(unix)]
const CHECK_CPU_SECS: libc::rlim_t = 30;
/// Wall-clock backstop for a hang that does not spin; the only limit
/// where there is no `RLIMIT_CPU` (Windows).
const CHECK_WALL: Duration = Duration::from_secs(300);
/// An in-process input slower than this is a hang.
const IN_PROCESS_TIMEOUT: Duration = Duration::from_secs(10);

fn budget() -> Duration {
    let secs = std::env::var("ROK_FUZZ_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5.0_f64);
    Duration::from_secs_f64(secs)
}

fn seed() -> u64 {
    std::env::var("ROK_FUZZ_SEED")
        .ok()
        .and_then(|s| {
            s.strip_prefix("0x")
                .map_or_else(|| s.parse().ok(), |h| u64::from_str_radix(h, 16).ok())
        })
        .unwrap_or(DEFAULT_SEED)
}

fn input_rng(i: usize) -> Rng {
    Rng(seed() ^ (i as u64).wrapping_mul(0xa076_1d64_78bd_642f))
}

fn crash_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fuzz_crashes")
}

fn corpus() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.is_dir() {
                Some(p.join("src/main.rok")).filter(|m| m.exists())
            } else {
                Some(p).filter(|p| p.extension().is_some_and(|x| x == "rok"))
            }
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .collect()
}

/// Rough lexemes: whitespace runs, words, string literals, `//` comments,
/// single other characters. Concatenated they give `src` back.
fn lexemes(src: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = src[i..].chars().next().unwrap();
        if c.is_whitespace() {
            while let Some(c) = src[i..].chars().next().filter(|c| c.is_whitespace()) {
                i += c.len_utf8();
            }
        } else if c.is_alphanumeric() || c == '_' {
            while let Some(c) = src[i..]
                .chars()
                .next()
                .filter(|c| c.is_alphanumeric() || *c == '_')
            {
                i += c.len_utf8();
            }
        } else if c == '"' {
            i += 1;
            while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            while !src.is_char_boundary(i) {
                i += 1;
            }
        } else if src[i..].starts_with("//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else {
            i += c.len_utf8();
        }
        out.push(&src[start..i]);
    }
    out
}

const SPECIAL: &[&str] = &[
    "0",
    "-1",
    "255u8",
    "-128i8",
    "2147483648",
    "9223372036854775807",
    "9223372036854775808",
    "18446744073709551616",
    "0x",
    "1e999",
    "1.5e-320",
    "\"\\u{110000}\"",
    "\"\\u{D800}\"",
    "'\\u{1F600}'",
    "''",
    "f\"{",
    "f\"{}}\"",
    "f\"{x:>{w}}\"",
    "{",
    "}",
    "(",
    ")",
    "[",
    "]",
    "<",
    ">",
    "::",
    "..",
    "..=",
    "?",
    "??",
    "?.",
    "!",
    "|",
    "&",
    "=>",
    "->",
    ";",
    ",",
    ":",
    "@",
    "#",
    "#[",
    "$",
    "\\",
    "`",
    "self",
    "Self",
    "sup",
    "pkg",
    "any",
    "none",
    "never",
    "ref",
    "move",
    "async",
    "await",
    "yield",
    "throw",
    "throws",
    "try",
    "catch",
    "finally",
    "using",
    "unsafe",
    "extern",
    "meta",
    "where",
    "impl",
    "trait",
    "union",
    "static",
    "const",
    "_",
];

struct Pool {
    corpus: Vec<String>,
    /// Distinct lexemes of the corpus plus `SPECIAL`.
    tokens: Vec<String>,
}

impl Pool {
    fn new() -> Self {
        let corpus = corpus();
        let mut set = std::collections::BTreeSet::new();
        for src in &corpus {
            for l in lexemes(src) {
                if !l.trim().is_empty() && l.len() < 40 {
                    set.insert(l.to_owned());
                }
            }
        }
        set.extend(SPECIAL.iter().map(|s| (*s).to_owned()));
        Self {
            corpus,
            tokens: set.into_iter().collect(),
        }
    }
}

fn mutate(rng: &mut Rng, pool: &Pool) -> String {
    let src = rng.pick(&pool.corpus);
    let mut out = src.clone();
    for _ in 0..=rng.below(4) {
        out = if rng.chance(70) {
            mutate_lexemes(rng, pool, &out)
        } else {
            mutate_lines(rng, pool, &out)
        };
    }
    out
}

fn mutate_lexemes(rng: &mut Rng, pool: &Pool, src: &str) -> String {
    let mut ls: Vec<String> = lexemes(src).into_iter().map(str::to_owned).collect();
    let solid: Vec<usize> = (0..ls.len())
        .filter(|&i| !ls[i].trim().is_empty())
        .collect();
    if solid.is_empty() {
        return src.to_owned();
    }
    let i = *rng.pick(&solid);
    let n = 1 + rng.below(3);
    let end = (i + n * 2).min(ls.len());
    match rng.below(6) {
        0 => {
            ls.drain(i..end);
        }
        1 => {
            let dup: Vec<String> = ls[i..end].to_vec();
            let at = *rng.pick(&solid);
            ls.splice(at..at, dup);
        }
        2 => {
            let j = *rng.pick(&solid);
            ls.swap(i, j);
        }
        3 | 4 => ls[i].clone_from(rng.pick(&pool.tokens)),
        _ => {
            let t = rng.pick(&pool.tokens).clone();
            ls.insert(i, t);
        }
    }
    ls.concat()
}

fn mutate_lines(rng: &mut Rng, pool: &Pool, src: &str) -> String {
    let mut lines: Vec<String> = src.lines().map(str::to_owned).collect();
    if lines.is_empty() {
        return src.to_owned();
    }
    let i = rng.below(lines.len());
    match rng.below(4) {
        0 => {
            lines.remove(i);
        }
        1 => {
            let l = lines[i].clone();
            let at = rng.below(lines.len());
            lines.insert(at, l);
        }
        2 => {
            let j = rng.below(lines.len());
            lines.swap(i, j);
        }
        _ => {
            let other: Vec<&str> = rng.pick(&pool.corpus).lines().collect();
            if !other.is_empty() {
                let a = rng.below(other.len());
                let b = (a + 1 + rng.below(8)).min(other.len());
                let at = rng.below(lines.len());
                lines.splice(at..at, other[a..b].iter().map(|s| (*s).to_owned()));
            }
        }
    }
    lines.join("\n") + "\n"
}

/// Random programs from a small grammar: mostly ill-typed, which is the
/// point - every path from parse to codegen must end in a diagnostic.
struct Gen<'r> {
    rng: &'r mut Rng,
    depth: u32,
    vars: Vec<String>,
}

const PRIMS: &[&str] = &[
    "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "usize", "isize", "f32", "f64", "int",
    "float", "bool", "char", "str", "none", "any", "never", "S", "E", "StrBuf",
];

const LITS: &[&str] = &[
    "0",
    "1",
    "-1",
    "42",
    "7i32",
    "255u8",
    "-128i8",
    "3u64",
    "2147483647",
    "9223372036854775807",
    "0x7f",
    "0b101",
    "1_000",
    "0.5",
    "-0.0",
    "1e10",
    "1.5e-300",
    "true",
    "false",
    "none",
    "'a'",
    "'한'",
    "'\\n'",
    "\"\"",
    "\"héllo\"",
    "\"a\\tb\"",
    "\"\\u{1F600}\"",
    "f64::NAN",
    "i32::MAX",
    "S { a: 1, b: \"x\" }",
    "E::A",
    "E::B(2)",
    "E::C { x: 3 }",
];

const BINOPS: &[&str] = &[
    "+", "-", "*", "/", "%", "%%", "<<", ">>", "&", "|", "^", "==", "!=", "<", "<=", ">", ">=",
    "&&", "||", "??", "..", "..=",
];

const METHODS: &[&str] = &[
    ".len()",
    ".to_string()",
    ".is_empty()",
    ".abs()",
    ".iter().map(|x| x).collect()",
    ".iter().sum()",
    ".push(1)",
    ".get(0)",
    ".clone()",
    ".to_uppercase()",
    ".split(\",\")",
    ".chars().count()",
    ".keys()",
    ".unwrap()",
    "?.len()",
    "!",
];

impl Gen<'_> {
    fn ty(&mut self) -> String {
        self.depth += 1;
        let t = if self.depth > 3 || self.rng.chance(55) {
            (*self.rng.pick(PRIMS)).to_owned()
        } else {
            match self.rng.below(9) {
                0 => format!("List<{}>", self.ty()),
                1 => format!("Dict<{}, {}>", self.ty(), self.ty()),
                2 => format!("Set<{}>", self.ty()),
                3 => format!("({}, {})", self.ty(), self.ty()),
                4 => format!("{} | {}", self.ty(), self.ty()),
                5 => format!("fn({}) -> {}", self.ty(), self.ty()),
                6 => format!("[{}; {}]", self.ty(), self.rng.below(4)),
                7 => format!("Slice<{}>", self.ty()),
                _ => format!("{} | none", self.ty()),
            }
        };
        self.depth -= 1;
        t
    }

    fn atom(&mut self) -> String {
        if !self.vars.is_empty() && self.rng.chance(40) {
            self.rng.pick(&self.vars).clone()
        } else {
            (*self.rng.pick(LITS)).to_owned()
        }
    }

    fn expr(&mut self) -> String {
        if self.depth > 4 || self.rng.chance(25) {
            return self.atom();
        }
        self.depth += 1;
        let e = match self.rng.below(24) {
            0..=4 => {
                let op = self.rng.pick(BINOPS);
                format!("{} {op} {}", self.expr(), self.expr())
            }
            5 => format!("-{}", self.expr()),
            6 => format!("!({})", self.expr()),
            7 => format!("({})", self.expr()),
            8 => format!(
                "if {} {{ {} }} else {{ {} }}",
                self.expr(),
                self.expr(),
                self.expr()
            ),
            9 => {
                let s = self.expr();
                let p = self.pat();
                let a = self.expr();
                let b = self.expr();
                format!("match {s} {{ {p} => {a}, _ => {b} }}")
            }
            10 => format!("{{ let t = {}; {} }}", self.expr(), self.expr()),
            11 => format!("|x: {}| {}", self.ty(), self.expr()),
            12 => format!("f({})", self.expr()),
            13 => {
                let m = self.rng.pick(METHODS);
                format!("{}{m}", self.expr())
            }
            14 => format!("{}[{}]", self.expr(), self.expr()),
            15 => {
                let a = if self.rng.chance(50) {
                    self.expr()
                } else {
                    String::new()
                };
                let b = if self.rng.chance(50) {
                    self.expr()
                } else {
                    String::new()
                };
                let k = if self.rng.chance(50) {
                    format!(":{}", self.expr())
                } else {
                    String::new()
                };
                format!("{}[{a}:{b}{k}]", self.expr())
            }
            16 => format!("[{}, {}]", self.expr(), self.expr()),
            17 => format!("{{{}: {}}}", self.expr(), self.expr()),
            18 => format!("({}, {})", self.expr(), self.expr()),
            19 => format!("f\"{{{}}} {{{}:?}}\"", self.expr(), self.expr()),
            20 => format!("{} as {}", self.expr(), self.ty()),
            21 => format!("try {} catch _ {{ {} }}", self.expr(), self.expr()),
            22 => format!("{} is {}", self.expr(), self.ty()),
            _ => format!("S {{ a: {}, b: {} }}", self.expr(), self.expr()),
        };
        self.depth -= 1;
        e
    }

    fn pat(&mut self) -> String {
        if self.depth > 4 || self.rng.chance(35) {
            return match self.rng.below(5) {
                0 => "_".into(),
                1 => "y".into(),
                2 => "none".into(),
                _ => (*self.rng.pick(&LITS[..22])).to_owned(),
            };
        }
        self.depth += 1;
        let p = match self.rng.below(8) {
            0 => format!("({}, {})", self.pat(), self.pat()),
            1 => format!("{} | {}", self.pat(), self.pat()),
            2 => "1..=5".into(),
            3 => format!("y: {}", self.ty()),
            4 => format!("E::B({})", self.pat()),
            5 => format!("S {{ a: {}, .. }}", self.pat()),
            6 => format!("[{}, ..]", self.pat()),
            _ => format!("E::C {{ x: {} }}", self.pat()),
        };
        self.depth -= 1;
        p
    }

    #[allow(clippy::many_single_char_names)]
    fn stmt(&mut self, out: &mut String) {
        let v = format!("v{}", self.vars.len());
        let line = match self.rng.below(10) {
            0 | 1 => {
                let t = self.ty();
                let e = self.expr();
                self.vars.push(v.clone());
                format!("let {v}: {t} = {e};")
            }
            2 | 3 => {
                let e = self.expr();
                self.vars.push(v.clone());
                format!("let {v} = {e};")
            }
            4 => {
                let s = self.expr();
                let p = self.pat();
                let g = self.expr();
                let a = self.expr();
                let b = self.expr();
                format!("match {s} {{ {p} if {g} => {a}, _ => {b} }};")
            }
            5 => format!(
                "if let {} = {} {{ println(\"x\"); }}",
                self.pat(),
                self.expr()
            ),
            6 => format!("for y in {} {{ println(f\"{{y}}\"); }}", self.expr()),
            7 => format!("println(f\"{{{}}}\");", self.expr()),
            8 if !self.vars.is_empty() => {
                let t = self.rng.pick(&self.vars).clone();
                format!("{t} = {};", self.expr())
            }
            _ => format!("let _ = {};", self.expr()),
        };
        let _ = writeln!(out, "    {line}");
    }
}

fn generate(rng: &mut Rng) -> String {
    let mut out = String::from(
        "struct S { a: i32; b: str; }\n\
         enum E { A, B(i32), C { x: i64 } }\n\
         fn f(x: i32) -> i32 { x + 1 }\n\
         fn main() {\n",
    );
    let mut g = Gen {
        rng,
        depth: 0,
        vars: Vec::new(),
    };
    for _ in 0..=g.rng.below(5) {
        g.stmt(&mut out);
    }
    out.push_str("}\n");
    out
}

/// A long flat chain (`1 + 1 + ..`, `x.f().f()..`): no nesting for the
/// parser's depth limit, deep trees for every later pass.
fn chain(rng: &mut Rng) -> String {
    let n = 50 + rng.below(2000);
    let (head, link) = *rng.pick(&[
        ("1", " + 1"),
        ("true", " && true"),
        ("\"a\"", ".to_string()"),
        ("1", " * 2 - 1"),
        ("[1]", ".len().to_string().len()"),
    ]);
    format!("fn main() {{\n    let x = {head}{};\n}}\n", link.repeat(n))
}

fn input(i: usize, pool: &Pool) -> String {
    let mut rng = input_rng(i);
    if rng.chance(3) {
        chain(&mut rng)
    } else if rng.chance(30) {
        generate(&mut rng)
    } else {
        mutate(&mut rng, pool)
    }
}

/// Digits masked, first line, at most 160 bytes.
fn mask(msg: &str) -> String {
    let line = msg.lines().next().unwrap_or("").trim();
    let mut out = String::new();
    for c in line.chars() {
        if c.is_ascii_digit() {
            if !out.ends_with('#') {
                out.push('#');
            }
        } else {
            out.push(c);
        }
        if out.len() >= 160 {
            break;
        }
    }
    out
}

struct Outcome {
    code: Option<i32>,
    stderr: String,
    timed_out: bool,
}

fn run_check(src: &str) -> Outcome {
    let mut cmd = Command::new(ROKC);
    cmd.args(["-q", "--color=never", "check", "-"])
        .env("ROK_HOME", workspace())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    // SAFETY: getrlimit / setrlimit are async-signal-safe.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
            let mut r: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_CPU, &mut r) == 0 {
                r.rlim_cur = CHECK_CPU_SECS.min(r.rlim_max);
                if libc::setrlimit(libc::RLIMIT_CPU, &r) == 0 {
                    return Ok(());
                }
            }
            Err(std::io::Error::last_os_error())
        });
    }
    let mut child = cmd.spawn().expect("spawn rokc");
    let mut stdin = child.stdin.take().unwrap();
    let text = src.to_owned();
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(text.as_bytes());
    });
    let mut err = child.stderr.take().unwrap();
    let reader = thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if start.elapsed() > CHECK_WALL {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        thread::sleep(Duration::from_millis(5));
    };
    // The kernel ends a child past its CPU limit with SIGXCPU.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        timed_out |= status.and_then(|s| s.signal()) == Some(libc::SIGXCPU);
    }
    let _ = writer.join();
    let stderr = String::from_utf8_lossy(&reader.join().unwrap()).into_owned();
    Outcome {
        code: status.and_then(|s| s.code()),
        stderr,
        timed_out,
    }
}

/// The crash signature of a `rokc` run, `None` when it ended normally.
fn signature(o: &Outcome) -> Option<String> {
    if o.timed_out {
        return Some("timeout".into());
    }
    if o.stderr.contains("has overflowed its stack") {
        return Some("stack overflow".into());
    }
    // A panic report starts its line (a diagnostic quoting source that
    // says "panicked at" does not).
    let report = o.stderr.match_indices("panicked at ").find(|&(at, _)| {
        let line = o.stderr[..at].rsplit('\n').next().unwrap_or("");
        line.starts_with("thread '") && line.ends_with("' ")
    });
    if let Some((at, _)) = report {
        let rest = &o.stderr[at + "panicked at ".len()..];
        let loc = rest.lines().next().unwrap_or("");
        let file = loc.split(':').next().unwrap_or(loc);
        let msg = rest.lines().nth(1).unwrap_or("");
        return Some(format!("panic {file}: {}", mask(msg)));
    }
    match o.code {
        None => Some("signal".into()),
        Some(101) => Some("internal error (exit 101)".into()),
        _ => None,
    }
}

fn check_signature(src: &str) -> Option<String> {
    signature(&run_check(src))
}

/// Shrink `src` while `same(candidate)` holds: chunks of lines, then
/// chunks of lexemes, halving the chunk size; stops at `deadline`.
fn minimize(src: &str, deadline: Instant, same: &dyn Fn(&str) -> bool) -> String {
    let mut cur = src.to_owned();
    for by_lines in [true, false] {
        let split = |s: &str| -> Vec<String> {
            if by_lines {
                s.split_inclusive('\n').map(str::to_owned).collect()
            } else {
                lexemes(s).into_iter().map(str::to_owned).collect()
            }
        };
        let mut parts = split(&cur);
        let mut chunk = parts.len().div_ceil(2).max(1);
        loop {
            let mut i = 0;
            while i < parts.len() {
                if Instant::now() > deadline {
                    return cur;
                }
                let end = (i + chunk).min(parts.len());
                let cand: String = parts[..i].concat() + &parts[end..].concat();
                if !cand.trim().is_empty() && same(&cand) {
                    cur = cand;
                    parts = split(&cur);
                } else {
                    i += chunk;
                }
            }
            if chunk == 1 {
                break;
            }
            chunk = chunk.div_ceil(2);
        }
    }
    cur
}

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// `(target, signature) -> file` of the recorded crashes.
fn known() -> BTreeMap<(String, String), PathBuf> {
    let mut out = BTreeMap::new();
    let Ok(rd) = fs::read_dir(crash_dir()) else {
        return out;
    };
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        let Ok(src) = fs::read_to_string(&p) else {
            continue;
        };
        let field = |k: &str| {
            src.lines()
                .find_map(|l| l.strip_prefix(k))
                .map(|s| s.trim().to_owned())
        };
        if let (Some(t), Some(s)) = (field("// fuzz-target:"), field("// fuzz-signature:")) {
            out.insert((t, s), p);
        }
    }
    out
}

fn record(target: &str, sig: &str, origin: &str, src: &str) -> PathBuf {
    let dir = crash_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{target}_{:08x}.rok", fnv(sig)));
    let mut text =
        format!("// fuzz-target: {target}\n// fuzz-signature: {sig}\n// {origin}; minimized.\n");
    text.push_str(src);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    fs::write(&path, text).unwrap();
    path
}

/// Crashes found by a run, by signature: `(input index, source)`.
type Found = Mutex<BTreeMap<String, (usize, String)>>;

/// Shrink and record each crash; the new ones (not in `known`) fail.
fn report(target: &str, found: Found, same: &dyn Fn(&str, &str) -> bool) {
    let known = known();
    let mut new = Vec::new();
    let mut seen = Vec::new();
    for (sig, (i, src)) in found.into_inner().unwrap() {
        if let Some(p) = known.get(&(target.to_owned(), sig.clone())) {
            seen.push(format!("{sig} ({})", p.display()));
            continue;
        }
        let deadline = Instant::now() + budget().max(Duration::from_secs(60));
        let small = minimize(&src, deadline, &|c| same(c, &sig));
        let origin = format!("input {i} of seed {:#x}", seed());
        let path = record(target, &sig, &origin, &small);
        new.push(format!("{sig}\n    repro: {}", path.display()));
    }
    if !seen.is_empty() {
        println!(
            "{target}: {} known crash(es) hit:\n  {}",
            seen.len(),
            seen.join("\n  ")
        );
    }
    assert!(
        new.is_empty(),
        "{target}: {} new crash(es) (recorded; a rerun treats them as known):\n  {}",
        new.len(),
        new.join("\n  ")
    );
}

/// Remember the file of each panic for the signature (the parser panics
/// on its own thread, so every thread records and the caller looks at
/// the shared slot).
static PANIC_AT: Mutex<Option<String>> = Mutex::new(None);

fn install_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(l) = info.location() {
                *PANIC_AT
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(l.file().to_owned());
            }
            default(info);
        }));
    });
}

fn payload_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}

fn has_error(diags: &[rokc_diagnostic::RokDiagnostic]) -> bool {
    diags.iter().any(rokc_diagnostic::RokDiagnostic::is_error)
}

/// Lex, parse and format `src`; `Err(signature)` on a failure.
fn front_end(src: &str) -> Result<(), String> {
    let run = || -> Result<(), String> {
        let (tokens, lex_diags) = rokc_lexer::Lexer::new(src, "fuzz.rok").lex_all();
        let (_, diags) = rokc_parser::parse_program(&tokens, "fuzz.rok");
        let clean = !has_error(&lex_diags) && !has_error(&diags);
        let path = Path::new("fuzz.rok");
        if let Ok(once) = rokc_fmt::format_file(path, src) {
            if clean {
                let (t2, l2) = rokc_lexer::Lexer::new(&once, "fuzz.rok").lex_all();
                let (_, d2) = rokc_parser::parse_program(&t2, "fuzz.rok");
                if has_error(&l2) || has_error(&d2) {
                    return Err("fmt: output of a clean parse has errors".into());
                }
                match rokc_fmt::format_file(path, &once) {
                    Ok(twice) if twice == once => {}
                    Ok(_) => return Err("fmt: not idempotent".into()),
                    Err(_) => return Err("fmt: rejects its own output".into()),
                }
            }
        }
        Ok(())
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(r) => r,
        Err(p) => {
            let file = PANIC_AT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .unwrap_or_default();
            Err(format!("panic {file}: {}", mask(&payload_text(&*p))))
        }
    }
}

/// Run `front_end` on a worker thread with a watchdog: `Err` for a hang.
fn front_end_watched(src: &str) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let text = src.to_owned();
    thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let _ = tx.send(front_end(&text));
        })
        .unwrap();
    rx.recv_timeout(IN_PROCESS_TIMEOUT)
        .unwrap_or_else(|_| Err("timeout".into()))
}

/// Inputs `0..` on all cores until the budget is spent; crashes by
/// signature (first input wins).
fn drive(target: &str, test: &(dyn Fn(&str) -> Result<(), String> + Sync)) -> Found {
    let pool = Pool::new();
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let found: Found = Mutex::new(BTreeMap::new());
    let deadline = Instant::now() + budget();
    let workers = thread::available_parallelism().map_or(4, std::num::NonZero::get);
    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    if Instant::now() > deadline {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let src = input(i, &pool);
                    if let Err(sig) = test(&src) {
                        found
                            .lock()
                            .unwrap()
                            .entry(sig)
                            .and_modify(|e| {
                                if i < e.0 {
                                    *e = (i, src.clone());
                                }
                            })
                            .or_insert_with(|| (i, src));
                    }
                }
            });
        }
    });
    let n = next.load(Ordering::Relaxed);
    let crashes = found.lock().unwrap().len();
    println!(
        "{target}: {n} inputs in {:.1}s (seed {:#x}), {crashes} crash signature(s)",
        budget().as_secs_f64(),
        seed()
    );
    found
}

#[test]
fn front_end_in_process() {
    install_hook();
    let found = drive("parse", &front_end_watched);
    report("parse", found, &|src, sig| {
        front_end_watched(src).err().as_deref() == Some(sig)
    });
}

#[test]
fn rokc_check() {
    let found = drive("check", &|src| check_signature(src).map_or(Ok(()), Err));
    report("check", found, &|src, sig| {
        check_signature(src).as_deref() == Some(sig)
    });
}

/// Replay the recorded crashes; print the ones that are fixed (delete
/// those files). Does not fail: the files document open bugs.
#[test]
fn known_crashes() {
    install_hook();
    let known: Vec<_> = known().into_iter().collect();
    let results = par_map(&known, |((target, sig), path)| {
        let src = fs::read_to_string(path).unwrap();
        let now = match target.as_str() {
            "check" => check_signature(&src),
            _ => front_end_watched(&src).err(),
        };
        (now.as_deref() == Some(sig.as_str()), path.clone(), now)
    });
    for (still, path, now) in results {
        if !still {
            println!(
                "fixed? {} no longer crashes the same way (now: {now:?}); delete it",
                path.display()
            );
        }
    }
}

#[test]
fn generator_and_mutator_are_deterministic() {
    let pool = Pool::new();
    assert_eq!(input(7, &pool), input(7, &pool));
    assert_eq!(input(8, &pool), input(8, &pool));
    let src = "fn main() { let s = \"a b\"; // c\n}\n";
    assert_eq!(lexemes(src).concat(), src);
    assert_eq!(mask("index 12 out of 3"), "index # out of #");
    let run = |code, stderr: &str| Outcome {
        code,
        stderr: stderr.into(),
        timed_out: false,
    };
    let ice = "\nthread 'main' panicked at compiler/rokc_hir/src/x.rs:10:5:\nindex 3 out of 2\n";
    assert_eq!(
        signature(&run(Some(101), ice)).as_deref(),
        Some("panic compiler/rokc_hir/src/x.rs: index # out of #")
    );
    assert_eq!(signature(&run(Some(1), "error: [parse] x")), None);
    let quoted = "error[E002]: x\n 1 | // thread 'main' panicked at a.rok:1:1:\n   |    ^\n";
    assert_eq!(signature(&run(Some(1), quoted)), None);
    assert_eq!(signature(&run(None, "")).as_deref(), Some("signal"));
}
