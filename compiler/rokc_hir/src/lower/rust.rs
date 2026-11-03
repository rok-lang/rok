//! Rust habits found in lowering: Rust's standard types and paths,
//! each with the Rok form to write instead.

#[allow(clippy::wildcard_imports)]
use super::*;
use rokc_diagnostic::Code;

/// `f().unwrap()` and kin on a call `f()` that throws: one error in place
/// of the call's missing `try` and the missing method.
pub(super) fn throwing_receiver(
    ctx: &LowerCtx<'_>,
    recv: &HirExpr,
    method: &str,
    args: &[CallArg],
    span: Span,
) -> Option<HirExpr> {
    if !matches!(
        method,
        "unwrap" | "expect" | "unwrap_or" | "unwrap_or_else" | "ok"
    ) {
        return None;
    }
    let call = ctx.src(recv.span)?;
    let missing_try = |d: &rokc_diagnostic::RokDiagnostic| {
        d.message.starts_with("this call can throw")
            && d.primary
                .as_ref()
                .is_some_and(|l| l.span.start == recv.span.start)
    };
    {
        let mut diags = ctx.env.sink.diagnostics.borrow_mut();
        let at = diags.iter().rposition(missing_try)?;
        diags.remove(at);
    }
    let arg = match args {
        [CallArg::Positional(e)] => ctx.src(e.span),
        _ => None,
    };
    let help = match (method, arg) {
        ("expect", Some(m)) => match m.strip_prefix('"').and_then(|m| m.strip_suffix('"')) {
            Some(m) if !m.contains(['{', '}']) => {
                #[allow(clippy::literal_string_with_formatting_args)] // an f-string, shown
                let catch = format!("catch e {{ panic(f\"{m}: {{e}}\") }}");
                format!("write `try! {call}`, or `try {call} {catch}`")
            }
            _ => format!("write `try! {call}`: it panics on the error"),
        },
        ("unwrap_or", Some(d)) => format!("write `try? {call} ?? {d}`"),
        ("ok", None) => format!("write `try? {call}`: the value, or `none` on an error"),
        ("unwrap_or_else", _) => format!("write `try {call} catch e {{ .. }}`"),
        _ => format!("write `try! {call}`: it panics on the error"),
    };
    let e = ctx.error_expr(
        Code::UNHANDLED_ERROR,
        span,
        format!("`{call}` throws instead of returning a `Result`: Rok has no `{method}` on it"),
    );
    ctx.help_last(Some(help));
    Some(e)
}

/// `crate::` / `super::` / `self::` at the start of a path.
pub(super) fn path_help(segs: &[&str]) -> Option<String> {
    let (first, rest) = segs.split_first()?;
    let rest = rest.join("::");
    match *first {
        "crate" => Some(format!("this package's root is `pkg`: write `pkg::{rest}`")),
        "super" => Some(format!("the parent module is `sup`: write `sup::{rest}`")),
        "self" if !rest.is_empty() => Some(format!(
            "an item of this module needs no prefix: write `{rest}`"
        )),
        _ => None,
    }
}

/// A Rust standard type `name<args>` that Rok spells another way.
pub(super) fn type_help(name: &str, args: Option<&str>) -> Option<String> {
    if let Some(h) = path_help(&name.split("::").collect::<Vec<_>>()) {
        return Some(h);
    }
    let a = args.unwrap_or("T");
    Some(match name {
        "Vec" => format!("Rok has no `Vec`: write `List<{a}>`"),
        "VecDeque" => format!("Rok has no `VecDeque`: write `Deque<{a}>`"),
        "HashMap" | "BTreeMap" => {
            format!(
                "Rok has no `{name}`: write `Dict<{}>`",
                args.unwrap_or("K, V")
            )
        }
        "HashSet" | "BTreeSet" => format!("Rok has no `{name}`: write `Set<{a}>`"),
        "String" => "Rok has no `String`: write `str`, its one string type".to_owned(),
        "Box" => {
            format!("Rok has no `Box`: write `{a}`; the GC frees a value once nothing refers to it")
        }
        "Rc" | "Arc" => match args.and_then(|a| a.strip_prefix("Mutex<")) {
            Some(inner) => format!(
                "Rok has no `{name}`: write `Mutex<{inner}`; copies of a `Mutex` share one lock"
            ),
            None => format!(
                "Rok has no `{name}`: write `{a}`; the GC frees shared values, and a `List`, \
                 `Dict` or `Mutex` is one object however many hold it"
            ),
        },
        "RefCell" => format!(
            "Rok has no `RefCell`: write `{a}`; every variable can change, and `ref p: {a}` \
             lets a function change the caller's"
        ),
        _ => return None,
    })
}

/// `Ok(x)` / `Err(e)` with no `Result` expected: in a `throws` function
/// the value or a `throw`, else `help` (the `Result::Ok` path) and the
/// `throws` way.
pub(super) fn result_help(name: &str, arg: Option<&str>, throws: bool, help: String) -> String {
    let Some(x) = arg else {
        return help;
    };
    match (name, throws) {
        ("Ok", true) if x == "()" => {
            "a `throws` function needs no `Ok(())`: delete it (`return;` leaves early)".to_owned()
        }
        ("Ok", true) => format!("a `throws` function returns the value itself: write `{x}`"),
        ("Err", true) => format!("write `throw {x}`"),
        ("Ok", false) => {
            format!("{help}; or declare the function `throws` and return `{x}` itself")
        }
        ("Err", false) => format!("{help}; or declare the function `throws` and `throw {x}`"),
        _ => help,
    }
}

/// A call of a Rust standard constructor, `texts` its arguments.
pub(super) fn call_help(segs: &[&str], texts: &[&str]) -> Option<String> {
    if let Some(h) = path_help(segs) {
        return Some(h);
    }
    let [ty, f] = segs else {
        return None;
    };
    let arg = texts.first().copied();
    Some(match (*ty, *f, arg) {
        ("Vec", "new", None) => "write `[]`, or `List::new()`".to_owned(),
        ("Vec", "with_capacity", Some(n)) => format!("write `List::with_capacity({n})`"),
        ("VecDeque", "new", None) => "write `Deque::new()`".to_owned(),
        ("HashMap" | "BTreeMap", "new", None) => "write `Dict::new()`".to_owned(),
        ("HashSet" | "BTreeSet", "new", None) => "write `Set::new()`".to_owned(),
        ("String", "new", None) => "write `\"\"`: a `str` is Rok's one string type".to_owned(),
        ("String", "from", Some(s)) => {
            format!("write `{s}`: a string literal is a `str` already")
        }
        ("Box" | "RefCell", "new", Some(x)) => format!("write `{x}`: Rok has no `{ty}`"),
        ("Rc" | "Arc", "new", Some(x)) => match x.strip_prefix("RefCell::new(") {
            Some(inner) => format!(
                "write `{}`: Rok has no `{ty}` or `RefCell`",
                inner.strip_suffix(')').unwrap_or(inner)
            ),
            None if x.starts_with("Mutex::new(") => {
                format!("write `{x}`: copies of a `Mutex` share one lock")
            }
            None => format!("write `{x}`: the GC frees shared values"),
        },
        _ => return None,
    })
}
