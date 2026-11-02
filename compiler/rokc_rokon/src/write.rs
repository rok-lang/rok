//! The canonical text of a value: 4-space indent; a value that fits in
//! `WIDTH` columns (where it starts) is written on one line, otherwise
//! one entry per line, each with a trailing comma.

use std::fmt::Write as _;

use crate::{Field, Kind, Value};

const WIDTH: usize = 100;
const INDENT: usize = 4;

/// The canonical text of `v` (no trailing newline).
pub fn to_string(v: &Value) -> String {
    let mut out = String::new();
    block(v, 0, 0, 0, &mut out);
    out
}

/// `v` on one line, whatever its width (for splicing into a line).
pub fn to_line(v: &Value) -> String {
    flat(v)
}

/// A rok string literal holding `s`.
pub fn quote(s: &str) -> String {
    quoted(s, '"')
}

fn quoted(s: &str, q: char) -> String {
    let mut out = String::from(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            // The bidi controls the lexer rejects too.
            c if c.is_control()
                || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') =>
            {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// One entry of a bracketed list: `value`, `name: value` or `key: value`.
enum Entry<'a> {
    Value(&'a Value),
    Named(&'a Field),
    Pair(&'a Value, &'a Value),
}

/// `(head, open, entries, close)` of a bracketed value; `None` for a
/// scalar or a unit variant.
fn parts(v: &Value) -> Option<(String, &'static str, Vec<Entry<'_>>, &'static str)> {
    Some(match &v.kind {
        Kind::List(vs) => (String::new(), "[", values(vs), "]"),
        Kind::Tuple(vs) => (String::new(), "(", values(vs), ")"),
        Kind::Dict(ps) => (
            String::new(),
            "{",
            ps.iter().map(|(k, v)| Entry::Pair(k, v)).collect(),
            "}",
        ),
        Kind::Struct { name, fields } => (
            name.as_ref().map_or_else(String::new, |n| format!("{n} ")),
            "{",
            named(fields),
            "}",
        ),
        // Named values only: `E::V { a: 1 }` whether read as that or as
        // `E::V(a: 1)` - std rokon's `Value` keeps no form.
        Kind::Variant {
            path,
            args,
            named: fs,
            braces,
        } if *braces || (args.is_empty() && !fs.is_empty()) => {
            (format!("{path} "), "{", named(fs), "}")
        }
        Kind::Variant {
            path,
            args,
            named: fs,
            ..
        } if !(args.is_empty() && fs.is_empty()) => {
            let mut es = values(args);
            es.extend(named(fs));
            (path.clone(), "(", es, ")")
        }
        _ => return None,
    })
}

fn values(vs: &[Value]) -> Vec<Entry<'_>> {
    vs.iter().map(Entry::Value).collect()
}

fn named(fs: &[Field]) -> Vec<Entry<'_>> {
    fs.iter().map(Entry::Named).collect()
}

fn scalar(v: &Value) -> String {
    match &v.kind {
        Kind::None => "none".into(),
        Kind::Bool(b) => b.to_string(),
        Kind::Int(i) => i.to_string(),
        Kind::Float(f) => float(*f),
        Kind::Char(c) => quoted(&c.to_string(), '\''),
        Kind::Str(s) => quote(s),
        Kind::Variant { path, .. } => path.clone(),
        _ => unreachable!("bracketed values have parts"),
    }
}

/// Shortest round-trip digits, always with a `.`: plain for 1e-5 up to
/// 1e17, else `1.0e17`. NaN / infinity have no ROKON form.
fn float(x: f64) -> String {
    assert!(x.is_finite(), "NaN/infinity has no ROKON form");
    let mag = x.abs();
    let text = if mag != 0.0 && !(1e-5..1e17).contains(&mag) {
        format!("{x:e}")
    } else {
        format!("{x}")
    };
    if text.contains('.') {
        return text;
    }
    match text.split_once('e') {
        Some((digits, exp)) => format!("{digits}.0e{exp}"),
        None => format!("{text}.0"),
    }
}

/// `v` on one line.
fn flat(v: &Value) -> String {
    let Some((head, open, entries, close)) = parts(v) else {
        return scalar(v);
    };
    let items: Vec<String> = entries.iter().map(entry_flat).collect();
    let one_tuple = matches!(v.kind, Kind::Tuple(ref t) if t.len() == 1);
    match (open, items.is_empty()) {
        (_, true) => format!("{head}{open}{close}"),
        ("{", false) => format!("{head}{{ {} }}", items.join(", ")),
        _ if one_tuple => format!("({},)", items[0]),
        _ => format!("{head}{open}{}{close}", items.join(", ")),
    }
}

fn entry_flat(e: &Entry<'_>) -> String {
    match e {
        Entry::Value(v) => flat(v),
        Entry::Named(f) => format!("{}: {}", f.name, flat(&f.value)),
        Entry::Pair(k, v) => format!("{}: {}", flat(k), flat(v)),
    }
}

/// Write `v` starting at column `col` of a line indented `indent`;
/// `tail` columns follow it on the line (a comma).
fn block(v: &Value, indent: usize, col: usize, tail: usize, out: &mut String) {
    let one = flat(v);
    let parts = parts(v);
    let Some((head, open, entries, close)) =
        parts.filter(|(_, _, es, _)| !es.is_empty() && col + one.chars().count() + tail > WIDTH)
    else {
        out.push_str(&one);
        return;
    };
    let inner = indent + INDENT;
    let _ = write!(out, "{head}{open}");
    for e in &entries {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', inner));
        let prefix = match e {
            Entry::Value(_) => String::new(),
            Entry::Named(f) => format!("{}: ", f.name),
            Entry::Pair(k, _) => format!("{}: ", flat(k)),
        };
        out.push_str(&prefix);
        let value = match e {
            Entry::Value(v) | Entry::Pair(_, v) => v,
            Entry::Named(f) => &f.value,
        };
        block(value, inner, inner + prefix.chars().count(), 1, out);
        out.push(',');
    }
    out.push('\n');
    out.extend(std::iter::repeat_n(' ', indent));
    out.push_str(close);
}
