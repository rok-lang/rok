//! Help for an unknown method: the names a type answers to, and the
//! names other languages use for them.

#[allow(clippy::wildcard_imports)]
use super::*;
use rokc_diagnostic::Code;

/// `(rok method, names other languages use for it)`.
pub(super) const METHOD_SYNONYMS: &[(&str, &[&str])] = &[
    ("len", &["length", "size", "count", "lenght"]),
    ("push", &["append", "add", "push_back"]),
    ("to_uppercase", &["upper", "toUpperCase", "toupper"]),
    ("to_lowercase", &["lower", "toLowerCase", "tolower"]),
    ("trim", &["strip"]),
    ("starts_with", &["startswith", "startsWith"]),
    ("ends_with", &["endswith", "endsWith"]),
    ("contains", &["includes", "has"]),
    ("iter", &["items", "entries"]),
    ("remove", &["delete", "del"]),
    ("join", &["implode"]),
];

/// Iterator methods a collection reaches through `.iter()`.
const ITER_METHODS: &[&str] = &[
    "map",
    "filter",
    "filter_map",
    "flat_map",
    "fold",
    "any",
    "all",
    "sum",
    "product",
    "for_each",
    "enumerate",
    "zip",
    "find",
    "position",
    "skip",
    "take",
    "rev",
    "chain",
    "collect",
];

/// Names of the methods (taking `self`) a user calls on `ty`, sorted:
/// visible, and neither they nor their trait's declaration marked
/// `#[doc(hidden)]` or `#[internal]` (left out of completion and docs).
pub(super) fn method_names(ctx: &LowerCtx<'_>, ty: &HirTy) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let user = |name: &str, info: Option<&MethodInfo>, def: Option<&FnDef>| {
        let hidden_decl = info
            .and_then(|i| i.trait_name.as_deref())
            .and_then(|t| ctx.env.trait_of(t)?.decls.get(name))
            .is_some_and(|d| rokc_ast::doc_hidden(&d.attrs));
        info.is_none_or(|i| ctx.is_visible(&i.module, i.vis))
            && !hidden_decl
            && def.is_none_or(|d| {
                !rokc_ast::doc_hidden(&d.attrs)
                    && d.params.first().is_some_and(|p| p.name == "self")
            })
    };
    if let Some(target) = resolve::method_target_for(ty) {
        for ((t, name), ids) in ctx.env.all_methods() {
            if *t == target
                && ids.iter().any(|id| {
                    let def = match ctx.env.decl(*id) {
                        Some(MethodOrFn::Fn(d)) => Some(*d),
                        Some(MethodOrFn::Method { fn_def, .. }) => Some(&**fn_def),
                        None => None,
                    };
                    user(name, ctx.env.method_info(*id), def)
                        && ctx.sig_by_id(*id).param_names.first().map(String::as_str)
                            == Some("self")
                })
            {
                out.push(name.clone());
            }
        }
    }
    if let Some(base) = generic_base(ctx, ty) {
        for ((b, name), gms) in ctx.env.all_generic_methods() {
            if *b == base
                && gms
                    .iter()
                    .any(|gm| user(name, Some(&gm.info), Some(&gm.def)))
            {
                out.push(name.clone());
            }
        }
    }
    let all = |_: &Impl| true;
    for (gi, _) in ctx.generic_impls(ty, Pick::Where(&all), 0) {
        for (name, gm) in &gi.methods {
            if user(name, Some(&gm.info), Some(&gm.def)) {
                out.push(name.clone());
            }
        }
    }
    out.retain(|n| !n.is_empty() && !n.starts_with("__") && !n.contains(['#', '/']));
    out.sort();
    out.dedup();
    out
}

/// The name generic impls of `ty` are filed under (`List__i32` gives
/// `List`).
fn generic_base(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<String> {
    let (name, generic_args) = match ty {
        HirTy::Struct(id) => {
            let d = ctx.struct_def(*id);
            (d.name, d.generic_args)
        }
        HirTy::Enum(id) => {
            let d = ctx.enum_def(*id);
            (d.name, d.generic_args)
        }
        HirTy::Prim(p) => (p.name().to_owned(), Vec::new()),
        HirTy::GcBuf(_) => (resolve::GC_BUF.to_owned(), Vec::new()),
        _ => return None,
    };
    Some(if generic_args.is_empty() {
        name
    } else {
        expr::enum_base_name(&name).to_owned()
    })
}

/// A type of the standard library or a primitive: never suggest an impl
/// on it.
fn is_std(ctx: &LowerCtx<'_>, ty: &HirTy) -> bool {
    match ty {
        HirTy::Struct(id) => ctx.struct_def(*id).name.starts_with("rok::"),
        HirTy::Enum(id) => ctx.enum_def(*id).name.starts_with("rok::"),
        HirTy::Prim(_) | HirTy::GcBuf(_) | HirTy::Array(..) => true,
        _ => false,
    }
}

/// The span of the first `name` after `recv` within the call `span`.
fn method_span(ctx: &LowerCtx<'_>, recv: Span, span: Span, name: &str) -> Option<Span> {
    let text = ctx.src(span)?;
    let from = recv.end.checked_sub(span.start)? as usize;
    let at = from + text.get(from..)?.find(name)?;
    ctx.sub_span(span, at, at + name.len())
}

/// Help for `recv.method(..)`, which `recv`'s type lacks; a fix where
/// the help is a concrete replacement. Call right after the error.
pub(super) fn no_method_help(
    ctx: &LowerCtx<'_>,
    recv: &HirExpr,
    method: &str,
    span: Span,
) -> String {
    let ty = &recv.ty;
    let shown = ctx.ty_name(ty);
    let names = method_names(ctx, ty);
    let has = |m: &str| names.iter().any(|n| n == m);
    let recv_src = ctx
        .src(recv.span)
        .filter(|_| recv.span.file == span.file && recv.span.end <= span.end);
    if let Some(h) = read_only_view_help(ctx, ty, method, recv_src) {
        return h;
    }
    // `xs.map(..)` on a List: an iterator method.
    if ITER_METHODS.contains(&method) && has("iter") {
        let r = recv_src.unwrap_or("xs");
        if let Some(src) = recv_src {
            ctx.fix_last(recv.span, format!("{src}.iter()"), "iterate first");
        }
        return format!("`{shown}` is not an iterator: write `{r}.iter().{method}(..)`");
    }
    let r = recv_src.unwrap_or("x");
    // `xs.set(i, v)`: the write is the index operator.
    if method == "set" && ["List<", "Deque<"].iter().any(|b| shown.starts_with(b)) {
        return format!(
            "write `{r}[i] = v`, which panics past the end; check `i < {r}.len()` first to \
             skip it"
        );
    }
    if let Some((plain, help)) = twin_help(ctx, ty, method, &format!("{r}.")) {
        if let Some(at) = method_span(ctx, recv.span, span, method) {
            ctx.fix_last(at, plain, format!("use `{plain}`"));
        }
        return help;
    }
    let pick = rokc_diagnostic::suggest_or_synonym(
        method,
        names.iter().map(String::as_str),
        METHOD_SYNONYMS,
    );
    if let Some(s) = pick {
        if let Some(at) = method_span(ctx, recv.span, span, method) {
            ctx.fix_last(at, s, format!("use `{s}`"));
        }
        return format!("did you mean `{s}`?");
    }
    if is_std(ctx, ty) {
        let mut close: Vec<&str> = names.iter().map(String::as_str).collect();
        close.sort_by_key(|n| rokc_diagnostic::edit_distance(method, n));
        close.truncate(6);
        if close.is_empty() {
            return format!("`{shown}` has no methods; pass it to a function instead");
        }
        let list: Vec<String> = close.iter().map(|n| format!("`{n}`")).collect();
        return format!(
            "`{shown}` is a standard type; call one of its methods, such as {}",
            list.join(", ")
        );
    }
    format!(
        "add `fn {method}(self, ..)` in an `impl {shown} {{ .. }}` block, or import a trait \
         that provides it"
    )
}

/// Help for `try_x` / `x_or_none`, which `ty` lacks while it has `x`:
/// a function has one failure channel and `try?` turns a throw into
/// `none` (stdlib 0.5.9), so the plain name is the one to call. `owner`
/// is what goes before the name (`s.`, `Path::`). `None` when `x` is
/// missing, or for `try_x` when `x` does not throw (another meaning).
pub(super) fn twin_help<'n>(
    ctx: &LowerCtx<'_>,
    ty: &HirTy,
    name: &'n str,
    owner: &str,
) -> Option<(&'n str, String)> {
    let (plain, try_form) = match name.strip_prefix("try_") {
        Some(p) => (p, true),
        None => (name.strip_suffix("_or_none")?, false),
    };
    if plain.is_empty() {
        return None;
    }
    let throws = |t: &rokc_ast::ThrowsKind| !matches!(t, rokc_ast::ThrowsKind::None);
    let throws = match super::expr::pick_method(ctx, ty, plain, None).ok()?? {
        super::expr::MethodPick::Concrete(id) => match ctx.env.decl(id)? {
            MethodOrFn::Fn(d) => throws(&d.throws),
            MethodOrFn::Method { fn_def, .. } => throws(&fn_def.throws),
        },
        super::expr::MethodPick::Generic(gm) => throws(&gm.def.throws),
    };
    let call = format!("{owner}{plain}(..)");
    let help = match (try_form, throws) {
        (true, true) => format!(
            "`{plain}` throws its error itself; there is no `try_` form: write `try {call}`"
        ),
        (true, false) => return None,
        (false, true) => format!("`try?` gives `none` for `{plain}`'s error: write `try? {call}`"),
        (false, false) => format!("`{plain}` gives `none` itself: write `{call}`"),
    };
    Some((plain, help))
}

/// Help for `Type::name(..)` (`display`), which type `full` lacks. Call
/// right after the error.
pub(super) fn no_assoc_help(ctx: &LowerCtx<'_>, full: &str, name: &str, display: &str, span: Span) {
    if !path_twin_help(ctx, full, name, display, span) {
        ctx.help_last(super::reflect::inspect_help_key(ctx, full, name));
    }
}

/// `twin_help` for `Type::name(..)`: the help and a fix naming the plain
/// function; false when there is none.
fn path_twin_help(ctx: &LowerCtx<'_>, full: &str, name: &str, display: &str, span: Span) -> bool {
    let Some(ty) = ctx
        .lookup_struct(full)
        .map(HirTy::Struct)
        .or_else(|| ctx.lookup_enum(full).map(HirTy::Enum))
    else {
        return false;
    };
    let owner = display
        .rsplit_once("::")
        .map_or(String::new(), |(o, _)| format!("{o}::"));
    let Some((plain, help)) = twin_help(ctx, &ty, name, &owner) else {
        return false;
    };
    let at = ctx
        .src(span)
        .and_then(|t| t.find(name))
        .and_then(|i| ctx.sub_span(span, i, i + name.len()));
    if let Some(at) = at {
        ctx.fix_last(at, plain, format!("use `{plain}`"));
    }
    ctx.help_last(Some(help));
    true
}

/// Methods a `SliceMut` has and the read-only `Slice` lacks.
const VIEW_WRITES: &[&str] = &[
    "index_set",
    "fill",
    "swap",
    "reverse",
    "sort",
    "sort_by",
    "sort_by_key",
    "copy_from_slice",
];

/// `xs[a:b].sort()` / `s[i] = x` on a `Slice`: name the writable view.
fn read_only_view_help(
    ctx: &LowerCtx<'_>,
    ty: &HirTy,
    method: &str,
    recv_src: Option<&str>,
) -> Option<String> {
    let HirTy::Struct(id) = ty else {
        return None;
    };
    let def = ctx.struct_def(*id);
    if expr::enum_base_name(&def.name) != "rok::core::collections::slice::Slice"
        || !VIEW_WRITES.contains(&method)
    {
        return None;
    }
    let elem = def
        .generic_args
        .first()
        .map_or_else(|| "T".to_owned(), |t| ctx.ty_name(t));
    let src = recv_src
        .filter(|s| s.contains('[') && s.ends_with(']'))
        .unwrap_or("xs[a:b]");
    Some(format!(
        "`{}` is a read-only view; a view that writes the list is a `SliceMut<{elem}>`: \
         `let s: SliceMut<{elem}> = {src};`",
        ctx.ty_name(ty)
    ))
}

/// `name` is another language's word for `target` (`length` for `len`).
pub(super) fn is_synonym(name: &str, target: &str) -> bool {
    METHOD_SYNONYMS
        .iter()
        .any(|(t, names)| *t == target && names.contains(&name))
}

/// `T` of `T | none` (a sum of the rest when there are more).
pub(super) fn some_part(ty: &HirTy) -> Option<HirTy> {
    let HirTy::Sum(cases) = ty else {
        return None;
    };
    if !cases.contains(&HirTy::None) {
        return None;
    }
    let rest: Vec<HirTy> = cases
        .iter()
        .filter(|c| **c != HirTy::None)
        .cloned()
        .collect();
    match rest.as_slice() {
        [] => None,
        [t] => Some(t.clone()),
        _ => Some(HirTy::sum(rest)),
    }
}

/// A literal to write after `??` for a value of type `ty`.
pub(super) const fn default_lit(ty: &HirTy) -> Option<&'static str> {
    let HirTy::Prim(p) = ty else {
        return None;
    };
    Some(match p {
        PrimTy::F32 | PrimTy::F64 => "0.0",
        PrimTy::Bool => "false",
        PrimTy::Char => "'\\0'",
        PrimTy::Str => "\"\"",
        PrimTy::CChar => return None,
        _ => "0",
    })
}

/// Source text of `span` short enough to quote in a help.
pub(super) fn short_src<'c>(ctx: &'c LowerCtx<'_>, span: Span) -> Option<&'c str> {
    ctx.src(span)
        .filter(|t| !t.is_empty() && t.len() <= 40 && !t.contains('\n'))
}

/// The ways to get the value out of `v: T | none`.
pub(super) fn optional_forms(ctx: &LowerCtx<'_>, v: &str, some: &HirTy) -> String {
    let t = ctx.ty_name(some);
    let d = default_lit(some).unwrap_or("default");
    let x = if v == "x" { "y" } else { "x" };
    format!(
        "write `{v} ?? {d}`, `if let {x}: {t} = {v} {{ .. }}`, or \
         `match {v} {{ {x}: {t} => .., none => .. }}`"
    )
}

/// `v.unwrap()` and kin on `v: T | none`: rok has no such methods, so
/// show the forms that get the value out.
pub(super) fn optional_method(
    ctx: &LowerCtx<'_>,
    recv: &HirExpr,
    method: &str,
    args: &[CallArg],
    span: Span,
) -> Option<HirExpr> {
    const NAMES: &[&str] = &[
        "unwrap",
        "expect",
        "unwrap_or",
        "unwrap_or_default",
        "unwrap_or_else",
        "is_some",
        "is_none",
        "ok_or",
        "get",
        "value",
    ];
    if !NAMES.contains(&method) {
        return None;
    }
    let some = some_part(&recv.ty)?;
    let v = short_src(ctx, recv.span).filter(|_| recv.span.file == span.file);
    let what = v.map_or("this value".to_owned(), |v| format!("`{v}`"));
    let e = ctx.error_expr(
        Code::NO_METHOD,
        span,
        format!(
            "{what} is `{}` and may be none; rok has no `{method}`",
            ctx.ty_name(&recv.ty)
        ),
    );
    let v = v.unwrap_or("v");
    let arg = match args {
        [CallArg::Positional(a)] => short_src(ctx, a.span),
        _ => None,
    };
    let help = match (method, arg) {
        ("unwrap_or", Some(d)) => {
            let fix = format!("{v} ?? {d}");
            ctx.fix_last(span, fix.clone(), format!("use `{fix}`"));
            format!("write `{fix}`")
        }
        ("is_none" | "is_some", _) => {
            let fix = if method == "is_none" {
                format!("{v} is none")
            } else {
                format!("{v} != none")
            };
            ctx.fix_last(span, fix.clone(), format!("use `{fix}`"));
            format!("write `{fix}`")
        }
        _ => optional_forms(ctx, v, &some),
    };
    ctx.help_last(Some(help));
    Some(e)
}
