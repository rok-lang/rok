//! Differential testing: seeded random cases, one rok program per suite,
//! each printing a line checked against Python 3 (ROKON: the compiler's
//! own writer).
//!
//! Skips without LLVM or `python3`. `ROK_DIFF_SEED` picks another case
//! set, `ROK_DIFF_CASES` the cases per suite (default 120). Programs land
//! in `target/differential/<suite>/`. Mismatches in `KNOWN` ops are
//! printed, not failed.

// Case builders juggle many short names; the Python templates use
// `{w}`-style placeholders filled by `replace`.
#![allow(
    clippy::many_single_char_names,
    clippy::literal_string_with_formatting_args
)]

mod common;

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;

use common::{
    Env, Rng, Step, case_timeout, exe_name, output_within, par_map, rokc_build, run_command, setup,
    shard,
};
use rokc_rokon::{Field, Kind, Value};

/// `(suite, op, why)`: mismatches that do not fail the test.
const KNOWN: &[(&str, &str, &str)] = &[];

fn seed() -> u64 {
    std::env::var("ROK_DIFF_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x00d1_ff00)
}

fn cases() -> usize {
    std::env::var("ROK_DIFF_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120)
}

/// How a line is compared.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Exact,
    /// Same `f64` (NaN = NaN) and the same significant digits.
    Float,
}

struct Case {
    op: &'static str,
    /// The rok expression or statements, for the report.
    repro: String,
    cmp: Cmp,
}

#[derive(Default)]
struct Suite {
    name: &'static str,
    /// rok items before `main`.
    head: String,
    /// `main`'s body.
    body: String,
    /// Python statements; one `print` per case. Empty: `want` holds the
    /// expected lines.
    py: String,
    want: Vec<String>,
    cases: Vec<Case>,
}

impl Suite {
    /// A case printing `f"{expr}"`.
    fn hole(&mut self, op: &'static str, expr: &str, py: &str, cmp: Cmp) {
        let _ = writeln!(self.body, "    println(f\"{{{expr}}}\");");
        let _ = writeln!(self.py, "print({py})");
        self.cases.push(Case {
            op,
            repro: expr.to_owned(),
            cmp,
        });
    }

    /// A case printing `expr` (a `str`), after `stmts`.
    fn line(&mut self, op: &'static str, stmts: &str, expr: &str, py: &str) {
        let _ = writeln!(self.body, "    {{\n{stmts}        println({expr});\n    }}");
        let _ = writeln!(self.py, "print({py})");
        self.cases.push(Case {
            op,
            repro: format!("{stmts}println({expr});"),
            cmp: Cmp::Exact,
        });
    }
}

const PY_HEAD: &str = r#"import decimal, math, struct
def W(v, bits, signed):
    v &= (1 << bits) - 1
    if signed and v >> (bits - 1):
        v -= 1 << bits
    return v
def LIM(bits, signed):
    return (-(1 << (bits - 1)), (1 << (bits - 1)) - 1) if signed else (0, (1 << bits) - 1)
def CH(v, bits, signed):
    lo, hi = LIM(bits, signed)
    return str(v) if lo <= v <= hi else 'none'
def SAT(v, bits, signed):
    lo, hi = LIM(bits, signed)
    return min(max(v, lo), hi)
def TD(a, b):
    q = abs(a) // abs(b)
    return q if (a < 0) == (b < 0) else -q
def TR(a, b):
    return a - b * TD(a, b)
def B(x):
    return 'true' if x else 'false'
def FB(n):
    return struct.unpack('<d', struct.pack('<Q', n))[0]
def BF(x):
    return 'nan' if x != x else str(struct.unpack('<Q', struct.pack('<d', x))[0])
def ROUNDED(f, x):
    r = float(f(x))
    return math.copysign(r, x) if r == 0 else r
def FMOD(a, b):
    return math.fmod(a, b)
def ROUND(x):
    if abs(x) >= 2.0**52:
        return x
    return ROUNDED(lambda y: decimal.Decimal(y).quantize(1, decimal.ROUND_HALF_UP), x)
def FMIN(a, b):
    return b if a != a else a if b != b else min(a, b)
def FMAX(a, b):
    return b if a != a else a if b != b else max(a, b)
def E(s):
    return s.replace('\\', '\\\\').replace('\n', '\\n').replace('\r', '\\r')
def BAR(ps):
    return ''.join('[' + E(p) + ']' for p in ps)
def LINES(s):
    ps = s.split('\n')
    if ps and ps[-1] == '':
        ps.pop()
    return [p[:-1] if p.endswith('\r') else p for p in ps]
def BYTE(s, i):
    return 'none' if i < 0 else str(len(s[:i].encode()))
def LIST(xs):
    return '[' + ', '.join(str(x) for x in xs) + ']'
def SLIST(xs):
    return '[' + ', '.join('"' + x + '"' for x in xs) + ']'
def OPT(x):
    return 'none' if x is None else str(x)
"#;

const ROK_HEAD: &str = r#"fn e(s: str) -> str { s.replace("\\", "\\\\").replace("\n", "\\n").replace("\r", "\\r") }
fn fb(x: f64) -> str { if x.is_nan() { "nan" } else { f"{x.to_bits()}" } }
fn bar(s: str, sep: str) -> str {
    let b = StrBuf::new();
    for p in s.split(sep) { b.push_str("["); b.push_str(e(p)); b.push_str("]"); }
    b.to_str()
}
fn barw(s: str) -> str {
    let b = StrBuf::new();
    for p in s.split_whitespace() { b.push_str("["); b.push_str(e(p)); b.push_str("]"); }
    b.to_str()
}
fn barl(s: str) -> str {
    let b = StrBuf::new();
    for p in s.lines() { b.push_str("["); b.push_str(e(p)); b.push_str("]"); }
    b.to_str()
}
fn sorted_keys(d: Dict<i64, i64>) -> List<i64> {
    let ks: List<i64> = List::new();
    for k in d.keys() { ks.push(k); }
    ks.sort();
    ks
}
fn sorted_set(s: Set<i64>) -> List<i64> {
    let ks = s.to_list();
    ks.sort();
    ks
}
fn odkeys(d: Dict<i64, i64>) -> List<i64> {
    let ks: List<i64> = List::new();
    for k in d.keys() { ks.push(k); }
    ks
}
"#;

/// A rok string literal.
fn rq(s: &str) -> String {
    rokc_rokon::quote(s)
}

/// A Python string literal.
fn pq(s: &str) -> String {
    let mut o = String::from("'");
    for c in s.chars() {
        if c.is_ascii_graphic() && c != '\'' && c != '\\' || c == ' ' {
            o.push(c);
        } else {
            let _ = write!(o, "\\U{:08x}", c as u32);
        }
    }
    o.push('\'');
    o
}

struct IntTy {
    name: &'static str,
    bits: u32,
    signed: bool,
}

const INT_TYS: &[IntTy] = &[
    IntTy {
        name: "i8",
        bits: 8,
        signed: true,
    },
    IntTy {
        name: "i16",
        bits: 16,
        signed: true,
    },
    IntTy {
        name: "i32",
        bits: 32,
        signed: true,
    },
    IntTy {
        name: "i64",
        bits: 64,
        signed: true,
    },
    IntTy {
        name: "u8",
        bits: 8,
        signed: false,
    },
    IntTy {
        name: "u16",
        bits: 16,
        signed: false,
    },
    IntTy {
        name: "u32",
        bits: 32,
        signed: false,
    },
    IntTy {
        name: "u64",
        bits: 64,
        signed: false,
    },
];

impl IntTy {
    const fn min(&self) -> i128 {
        if self.signed {
            -(1i128 << (self.bits - 1))
        } else {
            0
        }
    }

    const fn max(&self) -> i128 {
        if self.signed {
            (1i128 << (self.bits - 1)) - 1
        } else {
            (1i128 << self.bits) - 1
        }
    }

    const fn wrap(&self, v: i128) -> i128 {
        let m = (1i128 << self.bits) - 1;
        let v = v & m;
        if self.signed && v >> (self.bits - 1) != 0 {
            v - (1i128 << self.bits)
        } else {
            v
        }
    }

    /// A value biased to the edges.
    fn value(&self, rng: &mut Rng) -> i128 {
        let raw = i128::from(rng.next()) | (i128::from(rng.next()) << 64);
        match rng.below(8) {
            0 => self.min(),
            1 => self.max(),
            2 => 0,
            3 => self.wrap(-1),
            4 => self.wrap(i128::from(rng.range(-9, 9))),
            5 => self.max() - i128::from(rng.range(0, 3)),
            _ => self.wrap(raw),
        }
    }

    fn lit(&self, v: i128) -> String {
        if v == self.min() && self.signed {
            format!("{}::MIN", self.name)
        } else if v < 0 {
            format!("(-{}{})", -v, self.name)
        } else {
            format!("{v}{}", self.name)
        }
    }

    fn py(&self) -> String {
        format!(
            "{}, {}",
            self.bits,
            if self.signed { "True" } else { "False" }
        )
    }
}

fn int_suite(rng: &mut Rng) -> Suite {
    let mut s = Suite {
        name: "int",
        ..Suite::default()
    };
    for _ in 0..cases() * 2 {
        let t = rng.pick(INT_TYS);
        let (a, b) = (t.value(rng), t.value(rng));
        let (la, lb) = (t.lit(a), t.lit(b));
        let ty = t.py();
        let n = rng.below(t.bits as usize * 2);
        match rng.below(15) {
            0 => {
                let (op, py) = *rng.pick(&[("add", "+"), ("sub", "-"), ("mul", "*")]);
                s.hole(
                    "wrapping",
                    &format!("{la}.wrapping_{op}({lb})"),
                    &format!("W({a} {py} {b}, {ty})"),
                    Cmp::Exact,
                );
            }
            1 => {
                let (op, py) = *rng.pick(&[("add", "+"), ("sub", "-"), ("mul", "*")]);
                s.hole(
                    "checked",
                    &format!("{la}.checked_{op}({lb}):?"),
                    &format!("CH({a} {py} {b}, {ty})"),
                    Cmp::Exact,
                );
            }
            2 => {
                let (op, py) = *rng.pick(&[("add", "+"), ("sub", "-"), ("mul", "*")]);
                s.hole(
                    "saturating",
                    &format!("{la}.saturating_{op}({lb})"),
                    &format!("SAT({a} {py} {b}, {ty})"),
                    Cmp::Exact,
                );
            }
            3..=5 if b != 0 && !(t.signed && a == t.min() && b == -1) => {
                let (op, py) = rng
                    .pick(&[
                        ("/", format!("TD({a}, {b})")),
                        ("%", format!("TR({a}, {b})")),
                        ("%%", format!("{a} % {b}")),
                    ])
                    .clone();
                let tag = match op {
                    "/" => "div",
                    "%" => "rem",
                    _ => "modulo",
                };
                s.hole(tag, &format!("({la}) {op} ({lb})"), &py, Cmp::Exact);
            }
            6 => s.hole(
                "wrapping_shl",
                &format!("{la}.wrapping_shl({n}u32)"),
                &format!("W({a} << ({n} % {}), {ty})", t.bits),
                Cmp::Exact,
            ),
            7 => s.hole(
                "wrapping_shr",
                &format!("{la}.wrapping_shr({n}u32)"),
                &format!("W({a} >> ({n} % {}), {ty})", t.bits),
                Cmp::Exact,
            ),
            8 => {
                let k = n % t.bits as usize;
                let (op, py) = *rng.pick(&[("<<", "<<"), (">>", ">>")]);
                s.hole(
                    "shift",
                    &format!("{la} {op} {k}"),
                    &format!("W({a} {py} {k}, {ty})"),
                    Cmp::Exact,
                );
            }
            9 => {
                let (op, py) = *rng.pick(&[("&", "&"), ("|", "|"), ("^", "^")]);
                s.hole(
                    "bitwise",
                    &format!("({la}) {op} ({lb})"),
                    &format!("W({a} {py} {b}, {ty})"),
                    Cmp::Exact,
                );
            }
            10 => {
                let u = (1i128 << t.bits) - 1;
                let (op, py) = rng.pick(&[
                    ("count_ones", format!("bin({a} & {u}).count('1')")),
                    ("leading_zeros", format!("{} - ({a} & {u}).bit_length()", t.bits)),
                    (
                        "trailing_zeros",
                        format!(
                            "{} if {a} & {u} == 0 else (({a} & {u}) & -({a} & {u})).bit_length() - 1",
                            t.bits
                        ),
                    ),
                ])
                .clone();
                s.hole("bitcount", &format!("{la}.{op}()"), &py, Cmp::Exact);
            }
            11 => {
                let k = n % t.bits as usize;
                let bits = t.bits;
                s.hole(
                    "rotate",
                    &format!("{la}.rotate_left({k}u32)"),
                    &format!(
                        "W((({a} & {u}) << {k} | ({a} & {u}) >> ({bits} - {k})) & {u}, {ty})",
                        u = (1i128 << bits) - 1
                    ),
                    Cmp::Exact,
                );
            }
            12 => {
                let to = rng.pick(INT_TYS);
                s.hole(
                    "as",
                    &format!("{la} as {}", to.name),
                    &format!("W({a}, {})", to.py()),
                    Cmp::Exact,
                );
            }
            13 => {
                let (op, py) = *rng.pick(&[("<", "<"), ("<=", "<="), ("==", "=="), (">", ">")]);
                s.hole(
                    "compare",
                    &format!("({la}) {op} ({lb})"),
                    &format!("B({a} {py} {b})"),
                    Cmp::Exact,
                );
            }
            _ => s.hole(
                "wrapping_neg",
                &format!("{la}.wrapping_neg()"),
                &format!("W(-{a}, {ty})"),
                Cmp::Exact,
            ),
        }
    }
    s
}

fn big(rng: &mut Rng) -> String {
    let digits = 1 + rng.below(45);
    let mut d: String = (0..digits)
        .map(|_| char::from(b'0' + rng.below(10) as u8))
        .collect();
    d = d.trim_start_matches('0').to_owned();
    if d.is_empty() {
        d.push('0');
    }
    if rng.chance(40) && d != "0" {
        d.insert(0, '-');
    }
    d
}

fn bigint_suite(rng: &mut Rng) -> Suite {
    let mut s = Suite {
        name: "bigint",
        ..Suite::default()
    };
    for _ in 0..cases() {
        let (a, b) = (big(rng), big(rng));
        let stmts = format!("        let a: int = {a};\n        let b: int = {b};\n");
        match rng.below(6) {
            0 => s.line(
                "add-sub-mul",
                &stmts,
                "f\"{a + b} {a - b} {a * b}\"",
                &format!("f'{{{a} + {b}}} {{{a} - {b}}} {{{a} * {b}}}'"),
            ),
            1 if b != "0" => s.line(
                "div-rem",
                &stmts,
                "f\"{a / b} {a % b}\"",
                &format!("f'{{TD({a}, {b})}} {{TR({a}, {b})}}'"),
            ),
            2 if b != "0" => s.line("modulo", &stmts, "f\"{a %% b}\"", &format!("{a} % {b}")),
            3 => s.line(
                "compare",
                &stmts,
                "f\"{a < b} {a == b} {a >= b}\"",
                &format!("B({a} < {b}) + ' ' + B({a} == {b}) + ' ' + B({a} >= {b})"),
            ),
            4 => s.line(
                "parse",
                "",
                &format!("f\"{{try {}.parse::<int>() catch _ {{ int::zero() }}}}\"", rq(&a)),
                &a,
            ),
            _ => s.line(
                "to_i64",
                &stmts,
                "f\"{a.to_i64():?} {(a * b).to_i64():?}\"",
                &format!(
                    "OPT({a} if -2**63 <= {a} < 2**63 else None) + ' ' + OPT({a} * {b} if -2**63 <= {a} * {b} < 2**63 else None)"
                ),
            ),
        }
    }
    s
}

fn float_value(rng: &mut Rng) -> f64 {
    match rng.below(6) {
        0 => f64::from_bits(rng.next()),
        1 => rng.range(-1_000_000, 1_000_000) as f64 / 1000.0,
        2 => 10f64.powi(rng.range(-30, 30) as i32),
        3 => f64::from_bits(rng.next() % 4_000_000),
        4 => rng.range(-100, 100) as f64,
        _ => {
            f64::from_bits(rng.next()).abs().clamp(1e-300, 1e300)
                * if rng.chance(50) { -1.0 } else { 1.0 }
        }
    }
}

fn float_suite(rng: &mut Rng) -> Suite {
    let mut s = Suite {
        name: "float",
        ..Suite::default()
    };
    for _ in 0..cases() * 2 {
        let v = float_value(rng);
        let bits = v.to_bits();
        let w = float_value(rng);
        let wb = w.to_bits();
        let fv = format!("f64::from_bits({bits}u64)");
        let fw = format!("f64::from_bits({wb}u64)");
        match rng.below(9) {
            0 | 1 => s.hole("display", &fv, &format!("repr(FB({bits}))"), Cmp::Float),
            2 if v.is_finite() => s.hole(
                "literal",
                &format!("{v:e}"),
                &format!("repr(FB({bits}))"),
                Cmp::Float,
            ),
            3 => {
                // Decimal text, maybe with an exponent.
                let mut t = String::new();
                if rng.chance(30) {
                    t.push('-');
                }
                for _ in 0..=rng.below(19) {
                    t.push(char::from(b'0' + rng.below(10) as u8));
                }
                if rng.chance(60) {
                    t.push('.');
                    for _ in 0..=rng.below(19) {
                        t.push(char::from(b'0' + rng.below(10) as u8));
                    }
                }
                if rng.chance(50) {
                    let _ = write!(t, "e{}", rng.range(-330, 310));
                }
                s.hole(
                    "parse",
                    &format!("fb(try {}.parse::<f64>() catch _ {{ f64::NAN }})", rq(&t)),
                    &format!("BF(float('{t}'))"),
                    Cmp::Exact,
                );
            }
            4 if v.is_finite() && v.abs() < 1e17 => {
                let p = rng.below(8);
                s.hole(
                    "precision",
                    &format!("{fv}:.{p}"),
                    &format!("'%.{p}f' % FB({bits})"),
                    Cmp::Exact,
                );
            }
            5 => {
                let (op, py) = *rng.pick(&[("+", "+"), ("-", "-"), ("*", "*"), ("/", "/")]);
                if op == "/" && w == 0.0 {
                    continue;
                }
                s.hole(
                    "arith",
                    &format!("fb({fv} {op} {fw})"),
                    &format!("BF(FB({bits}) {py} FB({wb}))"),
                    Cmp::Exact,
                );
            }
            6 if w != 0.0 && v.is_finite() && w.is_finite() => {
                let (op, tag, py) = *rng.pick(&[
                    ("%", "fmod", "FMOD(FB({a}), FB({b}))"),
                    ("%%", "modulo", "FB({a}) % FB({b})"),
                ]);
                s.hole(
                    tag,
                    &format!("fb({fv} {op} {fw})"),
                    &format!(
                        "BF({})",
                        py.replace("{a}", &bits.to_string())
                            .replace("{b}", &wb.to_string())
                    ),
                    Cmp::Exact,
                );
            }
            7 if v.is_finite() => {
                let (op, py) = *rng.pick(&[
                    ("floor", "ROUNDED(math.floor, FB({a}))"),
                    ("ceil", "ROUNDED(math.ceil, FB({a}))"),
                    ("trunc", "ROUNDED(math.trunc, FB({a}))"),
                    ("round", "ROUND(FB({a}))"),
                    ("abs", "abs(FB({a}))"),
                    (
                        "sqrt",
                        "(math.sqrt(FB({a})) if FB({a}) >= 0 else float('nan'))",
                    ),
                ]);
                s.hole(
                    "math",
                    &format!("fb({fv}.{op}())"),
                    &format!("BF({})", py.replace("{a}", &bits.to_string())),
                    Cmp::Exact,
                );
            }
            // Equal operands (`0.0` / `-0.0`) may give either.
            8 if v.partial_cmp(&w) != Some(std::cmp::Ordering::Equal) => {
                let (op, py) = *rng.pick(&[
                    ("min", "FMIN(FB({a}), FB({b}))"),
                    ("max", "FMAX(FB({a}), FB({b}))"),
                    ("copysign", "math.copysign(FB({a}), FB({b}))"),
                ]);
                s.hole(
                    "math2",
                    &format!("fb({fv}.{op}({fw}))"),
                    &format!(
                        "BF({})",
                        py.replace("{a}", &bits.to_string())
                            .replace("{b}", &wb.to_string())
                    ),
                    Cmp::Exact,
                );
            }
            _ => {}
        }
    }
    s
}

const ALPHA: &[&str] = &[
    "a", "b", "c", "ab", " ", ",", ", ", "é", "한", "😀", "Σ", "ß", "İ", "\t", "\n", "\r\n", "ǅ",
    "ﬃ", "ς", "\u{3000}", "x",
];

fn word(rng: &mut Rng, max: usize) -> String {
    (0..rng.below(max + 1)).map(|_| *rng.pick(ALPHA)).collect()
}

fn str_suite(rng: &mut Rng) -> Suite {
    let mut s = Suite {
        name: "str",
        ..Suite::default()
    };
    for _ in 0..cases() * 2 {
        let w = word(rng, 8);
        let p = word(rng, 2);
        let (rw, pw, rp, pp) = (rq(&w), pq(&w), rq(&p), pq(&p));
        match rng.below(14) {
            0 if !p.is_empty() => s.line(
                "split",
                "",
                &format!("bar({rw}, {rp})"),
                &format!("BAR({pw}.split({pp}))"),
            ),
            1 => {
                let (op, py) = *rng.pick(&[
                    ("trim", "strip"),
                    ("trim_start", "lstrip"),
                    ("trim_end", "rstrip"),
                ]);
                s.line(
                    op,
                    "",
                    &format!("e({rw}.{op}())"),
                    &format!("E({pw}.{py}())"),
                );
            }
            2 => s.hole(
                "find",
                &format!("{rw}.find({rp}):?"),
                &format!("BYTE({pw}, {pw}.find({pp}))"),
                Cmp::Exact,
            ),
            3 => s.hole(
                "rfind",
                &format!("{rw}.rfind({rp}):?"),
                &format!("BYTE({pw}, {pw}.rfind({pp}))"),
                Cmp::Exact,
            ),
            4 => s.line(
                "to_uppercase",
                "",
                &format!("e({rw}.to_uppercase())"),
                &format!("E({pw}.upper())"),
            ),
            5 => s.line(
                "to_lowercase",
                "",
                &format!("e({rw}.to_lowercase())"),
                &format!("E({pw}.lower())"),
            ),
            6 => {
                let to = word(rng, 2);
                let op = if p.is_empty() {
                    "replace-empty"
                } else {
                    "replace"
                };
                s.line(
                    op,
                    "",
                    &format!("e({rw}.replace({rp}, {}))", rq(&to)),
                    &format!("E({pw}.replace({pp}, {}))", pq(&to)),
                );
            }
            7 => {
                let (op, py) = *rng.pick(&[
                    ("starts_with", "startswith"),
                    ("ends_with", "endswith"),
                    ("contains", "__contains__"),
                ]);
                s.hole(
                    op,
                    &format!("{rw}.{op}({rp})"),
                    &format!("B({pw}.{py}({pp}))"),
                    Cmp::Exact,
                );
            }
            8 => s.hole(
                "len",
                &format!("{rw}.len()"),
                &format!("len({pw}.encode())"),
                Cmp::Exact,
            ),
            9 => s.hole(
                "char_count",
                &format!("{rw}.char_count()"),
                &format!("len({pw})"),
                Cmp::Exact,
            ),
            10 => s.line(
                "split_whitespace",
                "",
                &format!("barw({rw})"),
                &format!("BAR({pw}.split())"),
            ),
            11 => s.line(
                "lines",
                "",
                &format!("barl({rw})"),
                &format!("BAR(LINES({pw}))"),
            ),
            12 => {
                let (op, py) = *rng.pick(&[
                    (
                        "strip_prefix",
                        "({w}[len({p}):] if {w}.startswith({p}) else '<none>')",
                    ),
                    (
                        "strip_suffix",
                        "({w}[:len({w}) - len({p})] if {w}.endswith({p}) else '<none>')",
                    ),
                ]);
                s.line(
                    op,
                    "",
                    &format!("e({rw}.{op}({rp}) ?? \"<none>\")"),
                    &format!("E({})", py.replace("{w}", &pw).replace("{p}", &pp)),
                );
            }
            _ => {
                let n = rng.below(4);
                s.line(
                    "repeat",
                    "",
                    &format!("e({rw}.repeat({n}))"),
                    &format!("E({pw} * {n})"),
                );
            }
        }
    }
    s
}

fn sort_suite(rng: &mut Rng) -> Suite {
    let mut s = Suite {
        name: "sort",
        ..Suite::default()
    };
    for _ in 0..cases() {
        let n = rng.below(40);
        let xs: Vec<i64> = (0..n).map(|_| rng.range(-50, 50)).collect();
        let lit = format!("{xs:?}");
        match rng.below(3) {
            0 => s.line(
                "sort",
                &format!("        let xs: List<i64> = {lit};\n        xs.sort();\n"),
                "f\"{xs:?}\"",
                &format!("LIST(sorted({lit}))"),
            ),
            1 => {
                let k = 1 + rng.below(9);
                s.line(
                    "sort_by-stable",
                    &format!(
                        "        let xs: List<i64> = {lit};\n        xs.sort_by(|p: i64, q: i64| (p %% {k}).cmp(q %% {k}));\n"
                    ),
                    "f\"{xs:?}\"",
                    &format!("LIST(sorted({lit}, key=lambda x: x % {k}))"),
                );
            }
            _ => {
                let ws: Vec<String> = (0..n.min(12))
                    .map(|_| {
                        (0..rng.below(4))
                            .map(|_| *rng.pick(&["a", "b", "B", "z", "é", "한"]))
                            .collect()
                    })
                    .collect();
                let rl = format!(
                    "[{}]",
                    ws.iter().map(|w| rq(w)).collect::<Vec<_>>().join(", ")
                );
                let pl = format!(
                    "[{}]",
                    ws.iter().map(|w| pq(w)).collect::<Vec<_>>().join(", ")
                );
                s.line(
                    "sort-str",
                    &format!("        let xs: List<str> = {rl};\n        xs.sort();\n"),
                    "f\"{xs:?}\"",
                    &format!("SLIST(sorted({pl}))"),
                );
            }
        }
    }
    s
}

fn dict_suite(rng: &mut Rng) -> Suite {
    let mut s = Suite {
        name: "dict",
        ..Suite::default()
    };
    s.body.push_str(
        "    let d: Dict<i64, i64> = Dict::new();\n    let od: Dict<i64, i64> = Dict::new();\n    let st: Set<i64> = Set::new();\n",
    );
    s.py.push_str("d = {}\nod = {}\nst = set()\n");
    for _ in 0..cases() * 2 {
        let k = rng.range(0, 12);
        let v = rng.range(-5, 5);
        match rng.below(12) {
            0 | 1 => s.hole(
                "insert",
                &format!("d.insert({k}, {v}):?"),
                &format!("[OPT(d.get({k})), d.__setitem__({k}, {v})][0]"),
                Cmp::Exact,
            ),
            2 => s.hole(
                "remove",
                &format!("d.remove({k}):?"),
                &format!("OPT(d.pop({k}, None))"),
                Cmp::Exact,
            ),
            3 => s.hole(
                "get",
                &format!("d.get({k}):?}} {{d.contains_key({k})}} {{d.len()"),
                &format!("OPT(d.get({k})) + ' ' + B({k} in d) + ' ' + str(len(d))"),
                Cmp::Exact,
            ),
            4 => s.hole("keys", "sorted_keys(d):?", "LIST(sorted(d))", Cmp::Exact),
            5 | 6 => s.hole(
                "ordered-insert",
                &format!("od.insert({k}, {v}):?"),
                &format!("[OPT(od.get({k})), od.__setitem__({k}, {v})][0]"),
                Cmp::Exact,
            ),
            7 => s.hole(
                "ordered-remove",
                &format!("od.remove({k}):?"),
                &format!("OPT(od.pop({k}, None))"),
                Cmp::Exact,
            ),
            8 => s.hole("ordered-keys", "odkeys(od):?", "LIST(od)", Cmp::Exact),
            9 => s.hole(
                "set-insert",
                &format!("st.insert({k})"),
                &format!("[B({k} not in st), st.add({k})][0]"),
                Cmp::Exact,
            ),
            10 => s.hole(
                "set-remove",
                &format!("st.remove({k})"),
                &format!("[B({k} in st), st.discard({k})][0]"),
                Cmp::Exact,
            ),
            _ => {
                let other: Vec<i64> = (0..rng.below(6)).map(|_| rng.range(0, 12)).collect();
                let (op, py) = *rng.pick(&[
                    ("union", "|"),
                    ("intersection", "&"),
                    ("difference", "-"),
                    ("symmetric_difference", "^"),
                ]);
                let lit = format!("{other:?}");
                s.line(
                    "set-algebra",
                    &format!(
                        "        let ol: List<i64> = {lit};\n        let o: Set<i64> = Set::new();\n        for x in ol {{ o.insert(x); }}\n"
                    ),
                    &format!("f\"{{sorted_set(st.{op}(o)):?}} {{st.is_subset(o)}} {{st.is_disjoint(o)}}\""),
                    &format!(
                        "LIST(sorted(st {py} set({lit}))) + ' ' + B(st <= set({lit})) + ' ' + B(st.isdisjoint(set({lit})))"
                    ),
                );
            }
        }
    }
    s
}

fn slice_suite(rng: &mut Rng) -> Suite {
    let mut s = Suite {
        name: "slice",
        ..Suite::default()
    };
    let opt = |rng: &mut Rng| {
        if rng.chance(30) {
            None
        } else {
            Some(rng.range(-10, 10))
        }
    };
    for _ in 0..cases() * 2 {
        let n = rng.below(9);
        let xs: Vec<i64> = (0..n as i64).map(|i| i * 10 + 1).collect();
        let lit = format!("{xs:?}");
        let (a, b) = (opt(rng), opt(rng));
        let k = if rng.chance(40) {
            None
        } else {
            Some([1, 2, 3, -1, -2, -3][rng.below(6)])
        };
        let r = |x: Option<i64>| x.map_or_else(String::new, |v| v.to_string());
        let rok_slice = match k {
            Some(k) => format!("{} : {} : {}", r(a), r(b), k),
            None => format!("{} : {}", r(a), r(b)),
        };
        let py_slice = match k {
            Some(k) => format!("{}:{}:{}", r(a), r(b), k),
            None => format!("{}:{}", r(a), r(b)),
        };
        match rng.below(3) {
            0 | 1 => s.line(
                "list",
                &format!("        let xs: List<i64> = {lit};\n"),
                &format!("f\"{{xs[{rok_slice}]:?}}\""),
                &format!("LIST({lit}[{py_slice}])"),
            ),
            _ => {
                let t: String = (0..n).map(|i| char::from(b'a' + i as u8)).collect();
                let plain = format!("{} : {}", r(a), r(b));
                s.line(
                    "str",
                    &format!("        let t = {};\n", rq(&t)),
                    &format!("f\"[{{t[{plain}]}}]\""),
                    &format!("'[' + {}[{}:{}] + ']'", pq(&t), r(a), r(b)),
                );
            }
        }
    }
    s
}

/// `[[T; C]; R]`, bare or in a struct, by value into a call and back:
/// sub-word elements past the argument registers.
fn aggregate_suite(rng: &mut Rng) -> Suite {
    let mut s = Suite {
        name: "aggregate",
        ..Suite::default()
    };
    for i in 0..cases() / 2 {
        let ty = *rng.pick(&["bool", "u8", "i8", "i16", "u16", "i32", "f32", "i64"]);
        let (r, c) = (1 + rng.below(4), 1 + rng.below(24));
        let val = |rng: &mut Rng| match ty {
            "bool" => rng.chance(50).to_string(),
            "f32" => format!("{}.5", rng.range(-99, 99)),
            "u8" | "u16" => rng.range(0, 250).to_string(),
            _ => rng.range(-120, 120).to_string(),
        };
        let a: Vec<Vec<String>> = (0..r).map(|_| (0..c).map(|_| val(rng)).collect()).collect();
        let lit = |v: &String| match ty {
            "bool" => v.clone(),
            _ => format!("{v}{ty}"),
        };
        let rows: Vec<String> = a
            .iter()
            .map(|row| format!("[{}]", row.iter().map(lit).collect::<Vec<_>>().join(", ")))
            .collect();
        let arr = format!("[[{ty}; {c}]; {r}]");
        let (y, x) = (rng.below(r), rng.below(c));
        let last = &a[r - 1][c - 1];
        let first = if (y, x) == (0, 0) { last } else { &a[0][0] };
        let want = format!("{last} {first} {last}");
        let get = |v: &str| {
            format!(
                "{{{v}[{y}][{x}]}} {{{v}[0][0]}} {{{v}[{}][{}]}}",
                r - 1,
                c - 1
            )
        };
        if rng.chance(25) {
            // An over-aligned variant beside a longer array (and, half
            // the time, a GC reference): the payload takes both.
            let align = *rng.pick(&[16, 32, 64]);
            let gc = if rng.chance(50) { " S(str)," } else { "" };
            let _ = writeln!(
                s.head,
                "#[repr(align({align}))] struct Al{i} {{ a: u64; b: {ty}; }}\n\
                 enum Sm{i} {{ P(Al{i}), Q(u8), R({arr}),{gc} }}\n\
                 fn sm{i}(e: Sm{i}, f: Sm{i}) -> (Sm{i}, Sm{i}) {{ return (f, e); }}\n\
                 fn show{i}(e: Sm{i}) -> str {{ match e {{ Sm{i}::P(p) => f\"{{p.a}} {{p.b}}\", Sm{i}::R(a) => f\"{}\", _ => \"-\" }} }}",
                get("a")
            );
            let b = val(rng);
            s.line(
                "sum",
                &format!(
                    "        let (x, y) = sm{i}(Sm{i}::R([{}]), Sm{i}::P(Al{i} {{ a: 77u64, b: {} }}));\n",
                    rows.join(", "),
                    lit(&b)
                ),
                &format!("f\"{{show{i}(x)}} {{show{i}(y)}}\""),
                &pq(&format!("77 {b} {} {} {last}", a[y][x], a[0][0])),
            );
        } else if rng.chance(50) {
            let _ = writeln!(
                s.head,
                "fn agg{i}(a: {arr}, y: usize, x: usize) -> {arr} {{ let b = a; b[y][x] = a[{}][{}]; return b; }}",
                r - 1,
                c - 1
            );
            // Half the time through a function value (spec 8.6.6 covers
            // both).
            let call = if rng.chance(50) {
                format!(
                    "let f: fn({arr}, usize, usize) -> {arr} = agg{i};\n        let b = f(a, {y}, {x});"
                )
            } else {
                format!("let b = agg{i}(a, {y}, {x});")
            };
            s.line(
                "array",
                &format!(
                    "        let a: {arr} = [{}];\n        {call}\n",
                    rows.join(", ")
                ),
                &format!("f\"{}\"", get("b")),
                &pq(&want),
            );
        } else {
            let _ = writeln!(
                s.head,
                "struct Agg{i} {{ pre: bool; a: {arr}; post: u8; }}\n\
                 fn agg{i}(g: Agg{i}, y: usize, x: usize) -> Agg{i} {{ let h = g; h.a[y][x] = g.a[{}][{}]; h.post = g.post + 1u8; return h; }}",
                r - 1,
                c - 1
            );
            s.line(
                "struct",
                &format!(
                    "        let g = Agg{i} {{ pre: true, a: [{}], post: 7u8 }};\n        let h = agg{i}(g, {y}, {x});\n",
                    rows.join(", ")
                ),
                &format!("f\"{} {{h.pre}} {{h.post}}\"", get("h.a")),
                &pq(&format!("{want} true 8")),
            );
        }
    }
    s
}

fn rokon_value(rng: &mut Rng, depth: u32) -> Value {
    const STRS: &[&str] = &[
        "",
        "plain",
        "q \" b \\",
        "nl \n t \t",
        "bell \u{7} c1 \u{85}",
        "한 😀",
        "{x}",
        "bidi \u{202e} \u{2066}",
    ];
    let kind = if depth > 2 || rng.chance(35) {
        match rng.below(7) {
            0 => Kind::None,
            1 => Kind::Bool(rng.chance(50)),
            2 => Kind::Int(i128::from(rng.next() as i64 >> rng.below(64))),
            3 | 4 => {
                let f = float_value(rng);
                Kind::Float(if f.is_finite() { f } else { 2.5 })
            }
            5 => Kind::Char(*rng.pick(&['a', '\'', '\\', '\n', '한', '😀'])),
            _ => Kind::Str((*rng.pick(STRS)).to_owned()),
        }
    } else {
        let n = rng.below(5);
        let vs = |rng: &mut Rng| {
            (0..n)
                .map(|_| rokon_value(rng, depth + 1))
                .collect::<Vec<_>>()
        };
        let fields = |rng: &mut Rng, n: usize| {
            (0..n)
                .map(|i| Field::new(&format!("f{i}"), rokon_value(rng, depth + 1)))
                .collect::<Vec<_>>()
        };
        match rng.below(6) {
            0 => Kind::List(vs(rng)),
            1 => Kind::Tuple(vs(rng)),
            2 => Kind::Dict(
                (0..n)
                    .map(|i| {
                        let k = if rng.chance(50) {
                            Value::str(format!("k{i}"))
                        } else {
                            Value::new(Kind::Int(i as i128))
                        };
                        (k, rokon_value(rng, depth + 1))
                    })
                    .collect(),
            ),
            3 => Kind::Struct {
                name: rng.chance(50).then(|| "Config".to_owned()),
                fields: fields(rng, n.max(1)),
            },
            4 => {
                let k = rng.below(2);
                Kind::Variant {
                    path: "Mode::Fast".into(),
                    args: vs(rng),
                    named: fields(rng, k),
                    braces: false,
                }
            }
            _ => Kind::Variant {
                path: "Shape::Rect".into(),
                args: Vec::new(),
                named: fields(rng, n.max(1)),
                braces: true,
            },
        }
    };
    Value::new(kind)
}

fn rokon_suite(rng: &mut Rng) -> Suite {
    let mut s = Suite {
        name: "rokon",
        head: "use rok::std::rokon;\n".into(),
        ..Suite::default()
    };
    for _ in 0..cases() {
        let v = rokon_value(rng, 0);
        let text = rokc_rokon::to_string(&v);
        let _ = writeln!(
            s.body,
            "    {{\n        let v = try rokon::parse({}) catch e {{ rokon::Value::Str(f\"ERROR {{e}}\") }};\n        println(try! v.to_str());\n        println(\"----\");\n    }}",
            rq(&text)
        );
        s.want.push(text.clone());
        s.cases.push(Case {
            op: "round-trip",
            repro: text,
            cmp: Cmp::Exact,
        });
    }
    s
}

/// Significant digits of a float's text (`-1.50e3` -> `15`).
fn digits(t: &str) -> String {
    let m = t.split(['e', 'E']).next().unwrap_or(t);
    let d: String = m.chars().filter(char::is_ascii_digit).collect();
    d.trim_start_matches('0').trim_end_matches('0').to_owned()
}

fn same(cmp: Cmp, got: &str, want: &str) -> bool {
    match cmp {
        Cmp::Exact => got == want,
        Cmp::Float => {
            let norm = |t: &str| t.trim().to_ascii_lowercase();
            let (g, w) = (norm(got), norm(want));
            match (g.parse::<f64>(), w.parse::<f64>()) {
                (Ok(a), Ok(b)) if a.is_nan() && b.is_nan() => true,
                (Ok(a), Ok(b)) if a.to_bits() == b.to_bits() => {
                    a.is_infinite() || digits(&g) == digits(&w)
                }
                _ => false,
            }
        }
    }
}

/// `python3`; Windows installs name it `python`.
fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|py| {
        Command::new(py)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success() && common::text(&o.stdout).starts_with("Python 3"))
    })
}

fn run_suite(env: &Env, py: &str, suite: &Suite, dir: &Path) -> Result<Vec<String>, String> {
    let _ = fs::create_dir_all(dir);
    let src = format!("{ROK_HEAD}{}\nfn main() {{\n{}}}\n", suite.head, suite.body);
    let main = dir.join("main.rok");
    fs::write(&main, &src).unwrap();
    let want: Vec<String> = if suite.want.is_empty() {
        let script = dir.join("oracle.py");
        fs::write(&script, format!("{PY_HEAD}{}", suite.py)).unwrap();
        // UTF-8 stdout whatever the console code page (Windows).
        let out = Command::new(py)
            .args(["-I", "-X", "utf8"])
            .arg(&script)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "oracle failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    } else {
        suite.want.clone()
    };
    let exe = dir.join(exe_name("out"));
    let build = rokc_build(env, &main, &exe)?;
    if !build.status.success() {
        let err = String::from_utf8_lossy(&build.stderr);
        return Err(format!(
            "rok build failed ({}, {}):\n{}",
            main.display(),
            build.status,
            err.lines().take(12).collect::<Vec<_>>().join("\n")
        ));
    }
    let run = output_within(&mut run_command(&exe), case_timeout("", Step::Run))?;
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    if !run.status.success() {
        return Err(format!(
            "rok run exited {:?}: {}",
            run.status.code(),
            String::from_utf8_lossy(&run.stderr)
        ));
    }
    let got: Vec<String> = if suite.want.is_empty() {
        stdout.lines().map(str::to_owned).collect()
    } else {
        stdout
            .split("\n----\n")
            .map(str::to_owned)
            .filter(|b| !b.is_empty())
            .collect()
    };
    if got.len() != suite.cases.len() || want.len() != suite.cases.len() {
        return Err(format!(
            "{} cases, rok printed {}, the oracle {} ({})",
            suite.cases.len(),
            got.len(),
            want.len(),
            dir.display()
        ));
    }
    let mut out = Vec::new();
    for ((c, g), w) in suite.cases.iter().zip(&got).zip(&want) {
        if !same(c.cmp, g, w) {
            out.push(format!(
                "{} [{}] {}\n      rok:    {g}\n      oracle: {w}",
                suite.name,
                c.op,
                c.repro.replace('\n', " ")
            ));
        }
    }
    Ok(out)
}

#[test]
fn differential() {
    let Some(env) = setup() else { return };
    let Some(py) = python() else {
        println!("differential: no python3; skipped");
        return;
    };
    let mut rng = Rng(seed());
    // Every suite is generated, then sharded: a shard's cases do not
    // depend on the shard spec.
    let suites = shard(vec![
        int_suite(&mut rng),
        bigint_suite(&mut rng),
        float_suite(&mut rng),
        str_suite(&mut rng),
        sort_suite(&mut rng),
        dict_suite(&mut rng),
        slice_suite(&mut rng),
        rokon_suite(&mut rng),
        aggregate_suite(&mut rng),
    ]);
    let root = env
        .workspace
        .join(format!("target/differential{}", common::opt_suffix()));
    let results = par_map(&suites, |s| run_suite(&env, py, s, &root.join(s.name)));
    let mut fails = Vec::new();
    let mut known = Vec::new();
    for (s, r) in suites.iter().zip(results) {
        match r {
            Err(e) => fails.push(format!("{}: {e}", s.name)),
            Ok(ms) => {
                println!(
                    "{}: {} cases, {} mismatches",
                    s.name,
                    s.cases.len(),
                    ms.len()
                );
                for m in ms {
                    let op = m
                        .split('[')
                        .nth(1)
                        .and_then(|r| r.split(']').next())
                        .unwrap_or("");
                    if KNOWN.iter().any(|(n, o, _)| *n == s.name && *o == op) {
                        known.push(m);
                    } else {
                        fails.push(m);
                    }
                }
            }
        }
    }
    if !known.is_empty() {
        println!("{} known mismatches, e.g.:\n  {}", known.len(), known[0]);
    }
    let shown: Vec<&String> = fails.iter().take(25).collect();
    assert!(
        fails.is_empty(),
        "{} differential mismatches (seed {}, programs in {}):\n  {}",
        fails.len(),
        seed(),
        root.display(),
        shown
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

#[test]
fn float_compare() {
    assert!(same(Cmp::Float, "1000000000000000000000", "1e+21"));
    assert!(same(Cmp::Float, "0.1", "0.1"));
    assert!(!same(Cmp::Float, "0.10000000000000001", "0.1"));
    assert!(same(Cmp::Float, "NaN", "nan"));
    assert!(same(Cmp::Float, "-inf", "-inf"));
    assert!(!same(Cmp::Float, "-0", "0.0"));
    assert_eq!(digits("-1.50e3"), "15");
}

#[test]
fn shards_partition() {
    let all: Vec<u32> = (0..10).collect();
    let mut got: Vec<u32> = (1..=4)
        .flat_map(|i| common::shard_of(&format!("{i}/4"), all.clone()))
        .collect();
    assert_eq!(common::shard_of("2/4", all.clone()), [1, 5, 9]);
    got.sort_unstable();
    assert_eq!(got, all);
    assert_eq!(common::shard_of("1/1", all.clone()), all);
}
