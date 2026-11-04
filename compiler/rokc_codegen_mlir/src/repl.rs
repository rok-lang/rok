//! REPL cells compile only what is new: a function whose text equals
//! an earlier cell's definition of its symbol, with all callees likewise,
//! becomes a declaration of it, so library code compiles once per session.
//!
//! Equal text is equal behaviour: symbols are stable across cells, and a
//! function referring to anything this cell redefines is redefined too.
//! The host renames redefined symbols so later cells get the newest
//! definition. Works on MLIR text: a top-level op is a two-space-indented
//! line up to the next.

use rokc_core::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// A function this cell defines anew: symbol, text hash, and referenced
/// symbols (its `known` entry goes when one is redefined).
#[derive(Clone, Debug)]
pub struct ReplFn {
    pub name: String,
    pub hash: u64,
    pub refs: Vec<String>,
}

#[derive(PartialEq)]
enum Kind {
    /// An exported function with a body: may be an earlier cell's.
    Candidate,
    /// An exported definition always made anew (statics, slots,
    /// `rok_repl_cell`, `rok.register_types`).
    Fresh,
    /// Internal, or a declaration.
    Other,
}

struct Op<'a> {
    text: &'a str,
    name: &'a str,
    kind: Kind,
    refs: Vec<&'a str>,
}

/// `text` with functions `known` already has made declarations (and how
/// many), plus the exported symbols and functions it defines anew.
pub fn reuse(
    text: &str,
    known: &HashMap<String, u64>,
) -> (String, usize, Vec<String>, Vec<ReplFn>) {
    let mut ops: Vec<Op<'_>> = Vec::new();
    let mut head = 0;
    let mut start = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let top = line.starts_with("  ") && !line[2..].starts_with([' ', '}', '^']);
        if top || line == "}\n" || line == "}" {
            if let Some(s) = start.take() {
                ops.push(op(&text[s..at]));
            } else {
                head = at;
            }
            if top {
                start = Some(at);
            }
        }
        at += line.len();
    }
    let tail = ops.last().map_or(head, |o| {
        o.text.as_ptr() as usize - text.as_ptr() as usize + o.text.len()
    });
    let hash = |s: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.hash(&mut h);
        h.finish()
    };
    let fresh: HashSet<&str> = ops
        .iter()
        .filter(|o| o.kind == Kind::Fresh)
        .map(|o| o.name)
        .collect();
    let mut reused: HashSet<&str> = ops
        .iter()
        .filter(|o| o.kind == Kind::Candidate && known.get(o.name) == Some(&hash(o.text)))
        .map(|o| o.name)
        .collect();
    let candidates: HashSet<&str> = ops
        .iter()
        .filter(|o| o.kind == Kind::Candidate)
        .map(|o| o.name)
        .collect();
    // A fixpoint over all ops: cells have a few hundred.
    loop {
        let drop: Vec<&str> = ops
            .iter()
            .filter(|o| reused.contains(o.name))
            .filter(|o| {
                o.refs
                    .iter()
                    .any(|r| fresh.contains(r) || (candidates.contains(r) && !reused.contains(r)))
            })
            .map(|o| o.name)
            .collect();
        if drop.is_empty() {
            break;
        }
        for d in drop {
            reused.remove(d);
        }
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(
        &text[..ops
            .first()
            .map_or(tail, |o| o.text.as_ptr() as usize - text.as_ptr() as usize)],
    );
    let mut fns = Vec::new();
    for o in &ops {
        if reused.contains(o.name) {
            out.push_str(&declaration(o.text));
            continue;
        }
        out.push_str(o.text);
        if o.kind == Kind::Candidate {
            let mut refs: Vec<String> = o.refs.iter().map(|r| (*r).to_owned()).collect();
            refs.sort();
            refs.dedup();
            fns.push(ReplFn {
                name: o.name.to_owned(),
                hash: hash(o.text),
                refs,
            });
        }
    }
    out.push_str(&text[tail..]);
    let mut fresh: Vec<String> = fresh.into_iter().map(str::to_owned).collect();
    fresh.extend(fns.iter().map(|f| f.name.clone()));
    (out, reused.len(), fresh, fns)
}

fn op(text: &str) -> Op<'_> {
    let first = text.lines().next().unwrap_or("");
    let mut refs = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find('@') {
        let r = &rest[i + 1..];
        let n = r
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$'))
            .unwrap_or(r.len());
        // A function value's thunk (`<f>.v`) stands for `f`.
        let name = &r[..n];
        refs.push(name.strip_suffix(".v").unwrap_or(name));
        rest = &r[n..];
    }
    let name = refs.first().copied().unwrap_or("");
    refs.retain(|r| *r != name && !r.starts_with("rok.str.") && !r.starts_with("rok.scan"));
    let body = first.trim_end().ends_with('{');
    let internal = first.contains(" internal ")
        || (first.contains(" private @") && first.starts_with("  llvm."))
        || first.contains("#llvm.linkage<internal>");
    let kind = if first.starts_with("  func.func @") && body && name != "rok_repl_cell" {
        Kind::Candidate
    } else if internal {
        Kind::Other
    } else if first.starts_with("  func.func") || first.starts_with("  llvm.func") {
        if body { Kind::Fresh } else { Kind::Other }
    } else if first.starts_with("  llvm.mlir.global") {
        // `@x()` with no value and no region declares.
        let decl = !body && first.contains(&format!("@{name}()"));
        if decl { Kind::Other } else { Kind::Fresh }
    } else {
        Kind::Other
    };
    Op {
        text,
        name,
        kind,
        refs,
    }
}

/// `  func.func @f(%arg0: T, ..) -> R [attributes {..}] {` and its body
/// -> `  func.func private @f(T, ..) -> R`.
fn declaration(def: &str) -> String {
    let first = def.lines().next().unwrap_or("");
    let first = first.trim_end().trim_end_matches('{').trim_end();
    let first = first.split(" attributes {").next().unwrap_or(first);
    let mut out = first.replacen("func.func @", "func.func private @", 1);
    while let Some(i) = out.find("%arg") {
        let n = out[i..].find(": ").map_or(out.len(), |n| i + n + 2);
        out.replace_range(i..n, "");
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: &str = "module {
  func.func @f(%arg0: !llvm.struct<(ptr, i64)>, %arg1: i64) -> i64 {
    %0 = func.call @g() : () -> i64
    func.return %0 : i64
  }
  func.func @g() -> i64 {
    %0 = llvm.mlir.addressof @rok.str.00 : !llvm.ptr
    %1 = arith.constant 1 : i64
    func.return %1 : i64
  }
  func.func @h() {
    %0 = llvm.mlir.addressof @S : !llvm.ptr
    func.return
  }
  func.func private @rok_panic(i64)
  llvm.mlir.global external @S(0 : i64) {addr_space = 0 : i32} : i64
  llvm.mlir.global external @T() {addr_space = 0 : i32} : i64
}
";

    #[test]
    fn known_functions_become_declarations() {
        let (_, _, fresh, fns) = reuse(CELL, &HashMap::default());
        assert_eq!(fns.len(), 3);
        assert!(fresh.contains(&"S".to_owned()) && !fresh.contains(&"T".to_owned()));
        assert_eq!(fns[0].refs, ["g"]);
        let known: HashMap<String, u64> = fns.iter().map(|f| (f.name.clone(), f.hash)).collect();
        let (text, n, _, fns) = reuse(CELL, &known);
        assert_eq!(n, 2);
        // `h` uses a static this cell defines: defined again.
        assert_eq!(
            fns.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["h"]
        );
        assert!(text.contains("  func.func private @f(!llvm.struct<(ptr, i64)>, i64) -> i64\n"));
        assert!(text.contains("  func.func private @g() -> i64\n"));
        assert!(
            text.ends_with("{addr_space = 0 : i32} : i64\n}\n"),
            "{text}"
        );
        // A changed callee makes its caller new too.
        let changed = CELL.replace("constant 1", "constant 2");
        let (_, _, _, fns) = reuse(&changed, &known);
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["f", "g", "h"]);
    }
}
