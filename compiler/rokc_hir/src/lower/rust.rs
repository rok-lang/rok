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
    let a = args.unwrap_or_else(|| if name.ends_with("Map") { "K, V" } else { "T" });
    let w = rok_type(&format!("{name}<{a}>"));
    Some(match name {
        "Vec" | "VecDeque" | "HashMap" | "BTreeMap" | "HashSet" | "BTreeSet" => {
            format!("Rok has no `{name}`: write `{w}`")
        }
        "String" => "Rok has no `String`: write `str`, its one string type".to_owned(),
        "Box" => {
            format!("Rok has no `Box`: write `{w}`; the GC frees a value once nothing refers to it")
        }
        "Rc" | "Arc" if a.starts_with("RefCell<") => {
            if w.starts_with("Cell<") {
                format!(
                    "Rok has no `{name}` or `RefCell`: write `{w}`; copies of a `Cell` share \
                     one value"
                )
            } else {
                format!(
                    "Rok has no `{name}` or `RefCell`: write `{w}`; it is one object however \
                     many hold it"
                )
            }
        }
        "Rc" | "Arc" => match a.split_once('<') {
            Some((lock @ ("Mutex" | "RwLock"), _)) => {
                format!("Rok has no `{name}`: write `{w}`; copies of a `{lock}` share one lock")
            }
            _ => format!(
                "Rok has no `{name}`: write `{w}`; the GC frees shared values, and a `List`, \
                 `Dict` or `Mutex` is one object however many hold it"
            ),
        },
        "RefCell" => format!(
            "Rok has no `RefCell`: write `{w}`; every variable can change, and `ref p: {w}` \
             lets a function change the caller's"
        ),
        _ => return None,
    })
}

/// Rust type text `t` in Rok's spelling, nested types too:
/// `Rc<RefCell<Vec<String>>>` is `List<str>`, `Rc<RefCell<i32>>` is
/// `Cell<i32>`.
pub(super) fn rok_type(t: &str) -> String {
    let t = t.trim();
    let Some((name, rest)) = t.split_once('<') else {
        return if t == "String" { "str" } else { t }.to_owned();
    };
    let Some(inner) = rest.strip_suffix('>') else {
        return t.to_owned();
    };
    let raw = split_top(inner);
    let args: Vec<String> = raw.iter().map(|a| rok_type(a)).collect();
    let one = || args.first().cloned().unwrap_or_default();
    match name.trim() {
        "Vec" => format!("List<{}>", args.join(", ")),
        "VecDeque" => format!("Deque<{}>", args.join(", ")),
        "HashMap" | "BTreeMap" => format!("Dict<{}>", args.join(", ")),
        "HashSet" | "BTreeSet" => format!("Set<{}>", args.join(", ")),
        "Option" => format!("{} | none", one()),
        "Rc" | "Arc"
            if raw
                .first()
                .is_some_and(|a| a.trim().starts_with("RefCell<")) =>
        {
            let u = one();
            if shared(&u) { u } else { format!("Cell<{u}>") }
        }
        "Rc" | "Arc" | "Box" | "RefCell" => one(),
        n => format!("{n}<{}>", args.join(", ")),
    }
}

/// `t` at its top-level commas.
fn split_top(t: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut from) = (0i32, 0);
    for (i, c) in t.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&t[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(&t[from..]);
    out
}

/// A Rok type or constructor whose copies are one object.
fn shared(t: &str) -> bool {
    t.starts_with('[')
        || [
            "List", "Dict", "Set", "Deque", "Mutex", "RwLock", "Cell", "Atomic",
        ]
        .iter()
        .any(|n| t.strip_prefix(n).is_some_and(|r| r.starts_with(['<', ':'])))
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

/// An argument `e` that is a Rust constructor too, or a Rok lock or
/// `Cell` around one: its type and Rok form.
fn arg_form(ctx: &LowerCtx<'_>, e: &Expr, nested: &mut Vec<u32>) -> Option<(String, String)> {
    let ExprKind::Call { callee, args, .. } = &e.kind else {
        return None;
    };
    let ExprKind::Path(p) = &callee.kind else {
        return None;
    };
    let s: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
    if let (["Mutex" | "RwLock" | "Cell", "new"], [CallArg::Positional(x)]) = (&s[..], &args[..]) {
        let (_, form) = arg_form(ctx, x, nested)?;
        return Some((s[0].to_owned(), format!("{}::new({form})", s[0])));
    }
    let mut own = Vec::new();
    let (form, _) = ctor_form(ctx, &s, args, &mut own)?;
    nested.append(&mut own);
    nested.push(e.span.start);
    Some((s[0].to_owned(), form))
}

/// A call of a Rust standard constructor: the Rok form of the whole
/// call, nested constructors included.
pub(super) fn call_help(ctx: &LowerCtx<'_>, segs: &[&str], args: &[CallArg]) -> Option<String> {
    if let Some(h) = path_help(segs) {
        return Some(h);
    }
    let (form, why) = ctor_form(ctx, segs, args, &mut Vec::new())?;
    Some(format!("write `{form}`{why}"))
}

/// The starts of the Rust constructor calls nested in the call
/// `segs(args)`, whose errors its one error replaces.
pub(super) fn nested_ctors(ctx: &LowerCtx<'_>, segs: &[&str], args: &[CallArg]) -> Vec<u32> {
    let mut nested = Vec::new();
    match ctor_form(ctx, segs, args, &mut nested) {
        Some(_) => nested,
        None => Vec::new(),
    }
}

/// `segs(args)` as (Rok form, why) when it is a Rust standard
/// constructor; `nested` gets the start of each one inside it.
fn ctor_form(
    ctx: &LowerCtx<'_>,
    segs: &[&str],
    args: &[CallArg],
    nested: &mut Vec<u32>,
) -> Option<(String, String)> {
    let [ty, f] = segs else {
        return None;
    };
    let arg = match args {
        [] => None,
        [CallArg::Positional(e)] => Some(e),
        _ => return None,
    };
    let mut inner_nested = Vec::new();
    let inner = arg.and_then(|e| arg_form(ctx, e, &mut inner_nested));
    let x = match (&inner, arg) {
        (Some((_, form)), _) => Some(form.clone()),
        (None, Some(e)) => Some(ctx.src(e.span)?.to_owned()),
        (None, None) => None,
    };
    let inner_ty = inner.as_ref().map(|i| i.0.as_str());
    let out = match (*ty, *f, x) {
        ("Vec", "new", None) => ("[]".to_owned(), ", or `List::new()`".to_owned()),
        ("Vec", "with_capacity", Some(n)) => (format!("List::with_capacity({n})"), String::new()),
        ("VecDeque", "new", None) => ("Deque::new()".to_owned(), String::new()),
        ("HashMap" | "BTreeMap", "new", None) => ("Dict::new()".to_owned(), String::new()),
        ("HashSet" | "BTreeSet", "new", None) => ("Set::new()".to_owned(), String::new()),
        ("String", "new", None) => (
            "\"\"".to_owned(),
            ": a `str` is Rok's one string type".to_owned(),
        ),
        ("String", "from", Some(s)) => (s, ": a string literal is a `str` already".to_owned()),
        ("Box" | "RefCell", "new", Some(x)) => (x, format!(": Rok has no `{ty}`")),
        ("Rc" | "Arc", "new", Some(x)) if inner_ty == Some("RefCell") => {
            if shared(&x) {
                (
                    x,
                    format!(
                        ": Rok has no `{ty}` or `RefCell`; it is one object however many hold it"
                    ),
                )
            } else {
                (
                    format!("Cell::new({x})"),
                    format!(": Rok has no `{ty}` or `RefCell`; copies of a `Cell` share one value"),
                )
            }
        }
        ("Rc" | "Arc", "new", Some(x)) => {
            let why = match x.split_once("::") {
                Some((lock @ ("Mutex" | "RwLock"), _)) => {
                    format!(": copies of a `{lock}` share one lock")
                }
                _ => ": the GC frees shared values".to_owned(),
            };
            (x, why)
        }
        _ => return None,
    };
    nested.append(&mut inner_nested);
    Some(out)
}

/// Rust's `match f() { Ok(v) => .., Err(e) => .. }` (or `if let Ok(v) =
/// f()`) on a call `f()` that throws: one error in place of the call's
/// missing `try` and each arm's unknown variant. `since` marks where the
/// match's diagnostics start; true when it reported.
pub(super) fn result_match(
    ctx: &LowerCtx<'_>,
    scrutinee: &Expr,
    arms: &[AstMatchArm],
    ok_ty: &HirTy,
    since: usize,
    span: Span,
) -> bool {
    if !matches!(
        scrutinee.kind,
        ExprKind::Call { .. } | ExprKind::MethodCall { .. }
    ) {
        return false;
    }
    let Some(call) = ctx.src(scrutinee.span) else {
        return false;
    };
    // (variant, binder, arm body, pattern start) of each `Ok(x)` / `Err(e)`.
    let mut found: Vec<(&str, &str, Option<String>, u32)> = Vec::new();
    for a in arms {
        if let AstPatternKind::EnumVariant { path, args } = &a.pat.kind {
            if let ([seg], [arg]) = (path.segments.as_slice(), args.as_slice()) {
                if matches!(seg.name.as_str(), "Ok" | "Err") {
                    let x = ctx.src(arg.span).unwrap_or("v");
                    found.push((&seg.name, x, arm_text(ctx, &a.body), a.pat.span.start));
                }
            }
        }
    }
    let throw_at = |d: &rokc_diagnostic::RokDiagnostic| {
        d.message.starts_with("this call can throw")
            && d.primary
                .as_ref()
                .is_some_and(|l| l.span.start == scrutinee.span.start)
    };
    let variant_at = |d: &rokc_diagnostic::RokDiagnostic| {
        d.message.starts_with("cannot find enum variant `")
            && d.primary
                .as_ref()
                .is_some_and(|l| found.iter().any(|f| f.3 == l.span.start))
    };
    // `if let` desugars to a match whose `_` arm starts at the `if`.
    let if_let = arms
        .iter()
        .any(|a| matches!(a.pat.kind, AstPatternKind::Wildcard) && a.pat.span.start == span.start);
    // `let Ok(x) = f() else { .. };` desugars to a match at `f()`, its
    // else block the `_` arm.
    let let_else = (span.start == scrutinee.span.start)
        .then(|| {
            arms.iter()
                .find(|a| matches!(a.pat.kind, AstPatternKind::Wildcard))
                .map(|a| arm_text(ctx, &a.body).unwrap_or_else(|| "{ .. }".to_owned()))
        })
        .flatten();
    let ok = found.iter().find(|f| f.0 == "Ok");
    let err = found.iter().find(|f| f.0 == "Err");
    let help = match (ok, err) {
        (Some((_, x, ..)), None) if let_else.is_some() => {
            let b = let_else.as_deref().unwrap_or("{ .. }");
            format!("write `let {x} = try {call} catch _ {b};`")
        }
        (Some((_, x, ..)), None) if if_let => {
            format!("write `if let {x}: {} = try? {call}`", ctx.ty_name(ok_ty))
        }
        (Some((_, x, Some(body), _)), Some((_, e, caught, _)))
            if !if_let && *body == format!("{{ {x} }}") =>
        {
            let caught = caught.as_deref().unwrap_or("{ .. }");
            format!("write `try {call} catch {e} {caught}`")
        }
        (Some((_, x, ..)), _) => {
            let e = err.map_or("e", |f| f.1);
            format!(
                "write `let {x} = try {call} catch {e} {{ .. }};`, or keep the arms: \
                 `match Result::catching(|| try {call}) {{ .. }}`"
            )
        }
        (None, Some((_, e, ..))) => format!("write `try {call} catch {e} {{ .. }}`"),
        (None, None) => return false,
    };
    {
        let mut diags = ctx.env.sink.diagnostics.borrow_mut();
        let tail = &diags[since.min(diags.len())..];
        if found.is_empty() || !tail.iter().any(throw_at) || !tail.iter().any(variant_at) {
            return false;
        }
        let mut i = since;
        while i < diags.len() {
            if throw_at(&diags[i]) || variant_at(&diags[i]) {
                diags.remove(i);
            } else {
                i += 1;
            }
        }
    }
    let at = Span {
        end: scrutinee.span.end,
        ..span
    };
    ctx.emit_diag(
        rokc_diagnostic::RokDiagnostic::error(
            Code::UNHANDLED_ERROR,
            at,
            format!(
                "`{call}` throws instead of returning a `Result`: it has no `Ok` or `Err` to match"
            ),
        )
        .with_help(help),
    );
    true
}

/// An arm body as the text of a `catch` block: `{ e }` for a one-line
/// expression or a block of one, `None` for a longer block (whose span
/// covers only its `{`).
fn arm_text(ctx: &LowerCtx<'_>, body: &Expr) -> Option<String> {
    let e = match &body.kind {
        ExprKind::Block(b) if b.stmts.is_empty() && b.label.is_none() => b.trailing.as_deref()?,
        ExprKind::Block(_) => return None,
        _ => body,
    };
    if matches!(e.kind, ExprKind::Block(_)) {
        return None;
    }
    let t = ctx.src(e.span)?;
    (!t.contains('\n')).then(|| format!("{{ {t} }}"))
}
