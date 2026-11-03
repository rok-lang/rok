//! Build-independent function names. Lowering names a closure by its id
//! and a method of a non-generic type by the type's id, both handed out
//! in lowering order; an edit anywhere would rename (and so re-symbol)
//! functions it did not touch. Here a closure becomes
//! `<creator>::{closure#k}` (the `k`th closure its creator takes the
//! address of), a coroutine body (its resume function, `__coro_<id>`)
//! `<creator>::{coroutine#k}`, and `Struct<id>__m` becomes
//! `<type path>__m`.

use rokc_core::HashMap;

use rokc_core::hash::{hex, sha256};
use rokc_ir_common::FunctionId;

use crate::{MirFunction, RValue, Statement};

/// Lowering-name prefixes of the functions named after their creator,
/// and the kind their stable name says.
const KINDS: [(&str, &str); 2] = [("__closure_", "closure"), ("__coro_", "coroutine")];

/// Whether `s` holds a lowering name `KINDS` renames.
fn has_token(s: &str) -> bool {
    KINDS.iter().any(|(p, _)| s.contains(p))
}

/// Renames `functions` as above; `type_name(enum_, id)` is a type's
/// build-independent name.
pub fn stable_names(
    functions: &mut [MirFunction],
    type_name: &dyn Fn(bool, u32) -> Option<String>,
) {
    for f in functions.iter_mut() {
        if let Some(n) = type_prefixed(&f.name, type_name) {
            f.name = n;
        }
    }
    let index: HashMap<FunctionId, usize> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id, i))
        .collect();
    let closures: HashMap<&str, usize> = functions
        .iter()
        .enumerate()
        .filter_map(|(i, f)| Some((closure_id(&f.name)?, i)))
        .collect();
    // Each closure's creators (what refers to it or to a function made
    // from it, `__fnval`, other than those), with its rank among the
    // closures each one refers to.
    let mut made_by: HashMap<usize, Vec<(usize, usize)>> = HashMap::default();
    for (g, f) in functions.iter().enumerate() {
        let own = closure_token(&f.name);
        let mut seen: Vec<usize> = Vec::new();
        for s in f.blocks.iter().flat_map(|b| &b.stmts) {
            let Statement::Assign {
                rvalue: RValue::FnAddr { callee, .. } | RValue::Call { callee, .. },
                ..
            } = s
            else {
                continue;
            };
            let Some(&j) = index.get(callee) else {
                continue;
            };
            let Some(token) = closure_token(&functions[j].name).filter(|t| Some(*t) != own) else {
                continue;
            };
            let Some(&c) = closures.get(token) else {
                continue;
            };
            if !seen.contains(&c) {
                made_by.entry(c).or_default().push((g, seen.len()));
                seen.push(c);
            }
        }
    }
    let mut names: HashMap<usize, Option<String>> = HashMap::default();
    let mut map: HashMap<String, String> = HashMap::default();
    for (&id, &i) in &closures {
        if let Some(n) = closure_name(i, functions, &closures, &made_by, &mut names) {
            map.insert(id.to_owned(), n);
        }
    }
    if map.is_empty() {
        return;
    }
    let mut lookup = |t: &str| map.get(t).cloned();
    for f in functions.iter_mut() {
        if has_token(&f.name) {
            f.name = replace_closures(&f.name, &mut lookup);
        }
        if let Some(a) = f.facts.mono_args.as_mut().filter(|a| has_token(a)) {
            *a = replace_closures(a, &mut lookup);
        }
    }
}

/// `Struct<id>__m` / `Enum<id>__m` with the type's name for its id.
fn type_prefixed(name: &str, type_name: &dyn Fn(bool, u32) -> Option<String>) -> Option<String> {
    let (enum_, rest) = if let Some(r) = name.strip_prefix("Struct") {
        (false, r)
    } else {
        (true, name.strip_prefix("Enum")?)
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let tail = rest[digits..].strip_prefix("__")?;
    let id = rest[..digits].parse().ok()?;
    Some(format!("{}__{tail}", type_name(enum_, id)?))
}

/// `__closure_<id>` / `__coro_<id>`: the lowering name of a closure or
/// coroutine body.
fn closure_id(name: &str) -> Option<&str> {
    closure_token(name).filter(|t| t.len() == name.len())
}

/// The `__closure_<id>` / `__coro_<id>` `name` starts with (a closure's,
/// a coroutine body's, or a function made from one).
fn closure_token(name: &str) -> Option<&str> {
    KINDS.iter().find_map(|(p, _)| {
        let digits = name.strip_prefix(p)?;
        let n = digits.bytes().take_while(u8::is_ascii_digit).count();
        (n > 0).then(|| &name[..p.len() + n])
    })
}

/// What a lowering-name token names (`closure`, `coroutine`).
fn kind(token: &str) -> &'static str {
    KINDS
        .iter()
        .find(|(p, _)| token.starts_with(p))
        .map_or("closure", |(_, k)| k)
}

/// The stable name of closure `i`: under its creator with the least
/// stable name. `None`: no creator (or a cycle of closures only).
fn closure_name(
    i: usize,
    functions: &[MirFunction],
    closures: &HashMap<&str, usize>,
    made_by: &HashMap<usize, Vec<(usize, usize)>>,
    names: &mut HashMap<usize, Option<String>>,
) -> Option<String> {
    if let Some(n) = names.get(&i) {
        return n.clone();
    }
    names.insert(i, None);
    let mut best: Option<String> = None;
    for &(g, k) in made_by.get(&i).into_iter().flatten() {
        let creator = &functions[g];
        // A closure (or a specialized copy of one) made it: under that
        // closure's own stable name.
        let path = creator.path();
        let base = match closure_token(path).and_then(|t| Some((t, *closures.get(t)?))) {
            Some((t, c)) => match closure_name(c, functions, closures, made_by, names) {
                Some(n) => n + &path[t.len()..],
                None => continue,
            },
            None => path.to_owned(),
        };
        // An instance's closures differ by the instance's arguments.
        // An instance's closures differ by the instance's arguments (a
        // specialized copy's name the closures it was made for).
        let args = match &creator.facts.mono_args {
            Some(a) => {
                let mut whole = true;
                let a = replace_closures(a, &mut |t| {
                    let n = closures
                        .get(t)
                        .and_then(|&c| closure_name(c, functions, closures, made_by, names));
                    whole &= n.is_some();
                    n
                });
                if !whole {
                    continue;
                }
                format!("@{}", &hex(&sha256(a.as_bytes()))[..8])
            }
            None => String::new(),
        };
        let n = format!("{base}::{{{}#{k}{args}}}", kind(&functions[i].name));
        if best.as_ref().is_none_or(|b| n < *b) {
            best = Some(n);
        }
    }
    names.insert(i, best.clone());
    best
}

/// `s` with every whole `__closure_<id>` / `__coro_<id>` replaced by what `name` says
/// (kept where it says `None`).
fn replace_closures(s: &str, name: &mut dyn FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some((at, p)) = KINDS
        .iter()
        .filter_map(|(p, _)| Some((rest.find(p)?, p)))
        .min()
    {
        out.push_str(&rest[..at]);
        let tail = &rest[at + p.len()..];
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        let token = &rest[at..at + p.len() + digits];
        out.push_str(&name(token).unwrap_or_else(|| token.to_owned()));
        rest = &rest[at + token.len()..];
    }
    out.push_str(rest);
    out
}
