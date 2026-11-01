//! Function lowering entry points and generic instantiation
//! (monomorphization, generic struct / enum instantiation).

#[allow(clippy::wildcard_imports)]
use super::*;

/// Match an AST parameter type against an argument's `HirTy` to bind
/// generic parameters (`[T; N]` against `[i32; 4]` binds `T → i32`).
pub(super) fn unify_param_ty_with(
    param_ty: &rokc_ast::Ty,
    arg_ty: &HirTy,
    generic_names: &std::collections::HashSet<String>,
    subst: &mut HashMap<String, HirTy>,
    ctx: &LowerCtx<'_>,
) {
    let mut rec = |p: &rokc_ast::Ty, a: &HirTy| {
        unify_param_ty_with(p, a, generic_names, subst, ctx);
    };
    match (&param_ty.kind, arg_ty) {
        (rokc_ast::TyKind::Path(p), _)
            if p.segments.len() == 1 && generic_names.contains(&p.segments[0].name) =>
        {
            // First binding wins; a conflict surfaces as an argument
            // mismatch. `!` fits any `T`, so a later argument rebinds it.
            let b = subst
                .entry(p.segments[0].name.clone())
                .or_insert_with(|| arg_ty.clone());
            if *b == HirTy::Never {
                *b = arg_ty.clone();
            }
        }
        // Generic struct / enum param `Pair<T>` against arg `Pair__i32`:
        // recover T by reading the instantiation's recorded generic args.
        (rokc_ast::TyKind::Path(p), HirTy::Struct(_) | HirTy::Enum(_)) => {
            let Some(last) = p.segments.last() else {
                return;
            };
            let generic_args = match arg_ty {
                HirTy::Struct(sid) => ctx.struct_def(*sid).generic_args,
                HirTy::Enum(eid) => ctx.enum_def(*eid).generic_args,
                _ => Vec::new(),
            };
            for (param_arg, concrete) in last.generics.iter().zip(generic_args.iter()) {
                rec(param_arg, concrete);
            }
        }
        (rokc_ast::TyKind::Array(elem, len), HirTy::Array(arg_elem, n)) => {
            // `[T; N]` against `[i32; 3]` binds the const parameter `N`.
            let len_name = match len {
                rokc_ast::ConstExpr::Path(p) if p.segments.len() == 1 => {
                    Some(p.segments[0].name.clone())
                }
                rokc_ast::ConstExpr::Expr(e) => match &e.kind {
                    rokc_ast::ExprKind::Path(p) if p.segments.len() == 1 => {
                        Some(p.segments[0].name.clone())
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(name) = len_name.filter(|n| generic_names.contains(n)) {
                subst
                    .entry(name)
                    .or_insert_with(|| HirTy::Const(i128::from(*n), None));
            }
            unify_param_ty_with(elem, arg_elem, generic_names, subst, ctx);
        }
        // `GcBuf<T>` against `GcBuf<i32>`.
        (rokc_ast::TyKind::Path(p), HirTy::GcBuf(e)) => {
            if let Some(g) = p.segments.last().and_then(|s| s.generics.first()) {
                rec(g, e);
            }
        }
        (rokc_ast::TyKind::Tuple(parts), HirTy::Tuple(arg_parts)) => {
            for (p, a) in parts.iter().zip(arg_parts.iter()) {
                rec(p, a);
            }
        }
        // Sums are sets (spec 2.6.4): members named outright cancel and a
        // lone parameter takes the rest (`T | none` against
        // `i32 | str | none` binds `T = i32 | str`).
        (rokc_ast::TyKind::Sum(parts), HirTy::Sum(arg_parts)) => {
            let is_param = |p: &rokc_ast::Ty| {
                matches!(&p.kind, rokc_ast::TyKind::Path(path)
                    if path.segments.len() == 1 && generic_names.contains(&path.segments[0].name))
            };
            // `T?` is `Nest(T) | none`.
            let parts: Vec<&rokc_ast::Ty> =
                parts.iter().map(|p| p.nest_inner().unwrap_or(p)).collect();
            let (params, fixed): (Vec<&rokc_ast::Ty>, Vec<&rokc_ast::Ty>) =
                parts.iter().partition(|p| is_param(p));
            let fixed: Vec<HirTy> = fixed
                .iter()
                .map(|p| match &p.kind {
                    rokc_ast::TyKind::None => HirTy::None,
                    _ => ctx.resolve_ast_ty(p),
                })
                .collect();
            if let [param] = params.as_slice() {
                let rest = arg_parts.iter().filter(|a| !fixed.contains(a)).cloned();
                rec(param, &HirTy::sum(rest));
            } else {
                for (p, a) in parts.iter().zip(arg_parts.iter()) {
                    rec(p, a);
                }
            }
        }
        (
            rokc_ast::TyKind::Fn(args, ret, _),
            HirTy::Fn(arg_args, arg_ret, _)
            | HirTy::Closure {
                args: arg_args,
                ret: arg_ret,
                ..
            },
        ) => {
            for (p, a) in args.iter().zip(arg_args.iter()) {
                rec(p, a);
            }
            rec(ret, arg_ret);
        }
        (rokc_ast::TyKind::Throws { ok, err }, HirTy::Throws { ok: a, err: e }) => {
            rec(ok, a);
            rec(err, e);
        }
        // A non-throwing argument for a `rethrows` parameter: its error
        // parameter binds to "no error".
        (rokc_ast::TyKind::Throws { ok, err }, a) => {
            rec(ok, a);
            rec(err, &HirTy::NoThrow);
        }
        // Nothing to bind: the argument type check reports mismatches.
        _ => {}
    }
}

/// Bind `fn_def`'s type params from the type of argument `i`. A generic
/// method's `self` is typed by its impl target `self_ty`.
pub(super) fn bind_from_arg(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    self_ty: Option<&Ty>,
    i: usize,
    arg_ty: &HirTy,
    subst: &mut HashMap<String, HirTy>,
) {
    let Some(param) = fn_def.params.get(i) else {
        return;
    };
    let self_param = self_ty.filter(|_| param.name == "self");
    let Some(t) = param.ty.as_ref().or(self_param) else {
        return;
    };
    let generic_names: std::collections::HashSet<String> = fn_def
        .generics
        .params
        .iter()
        .map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. }
            | rokc_ast::GenericParam::Const { name, .. } => name.clone(),
        })
        .collect();
    unify_param_ty_with(t, arg_ty, &generic_names, subst, ctx);
    bind_assoc_bindings(ctx, fn_def, self_param.map(|_| arg_ty), subst);
}

/// The type parameter names of `fn_def`.
fn type_param_names(fn_def: &FnDef) -> std::collections::HashSet<String> {
    fn_def
        .generics
        .params
        .iter()
        .filter_map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. } => Some(name.clone()),
            rokc_ast::GenericParam::Const { .. } => None,
        })
        .collect()
}

/// Each `Assoc = T` binding in `fn_def`'s bounds and `where` clause on a
/// bare parameter or `Self`: `(subject, assoc, T, bound span)`.
fn assoc_bindings(fn_def: &FnDef) -> Vec<(String, &str, &Ty, rokc_diagnostic::Span)> {
    let bare = |t: &Ty| match &t.kind {
        rokc_ast::TyKind::Path(p) => match p.segments.as_slice() {
            [s] if s.generics.is_empty() => Some(s.name.clone()),
            _ => None,
        },
        rokc_ast::TyKind::SelfUpper => Some("Self".to_owned()),
        _ => None,
    };
    let preds = fn_def
        .generics
        .params
        .iter()
        .filter_map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, bounds, .. } => {
                Some((name.clone(), bounds.as_slice()))
            }
            rokc_ast::GenericParam::Const { .. } => None,
        })
        .chain(
            fn_def
                .where_clause
                .iter()
                .filter_map(|w| Some((bare(&w.ty)?, w.bounds.as_slice()))),
        );
    let mut out = Vec::new();
    for (subject, bounds) in preds {
        for b in bounds {
            let rokc_ast::TyKind::Path(bp) = &b.kind else {
                continue;
            };
            for g in bp.segments.iter().flat_map(|s| &s.generics) {
                if let Some((assoc, target)) = rokc_ast::as_assoc_binding(g) {
                    out.push((subject.clone(), assoc, target, b.span));
                }
            }
        }
    }
    out
}

/// `subject::assoc` resolved, if `subject` is known and the result is
/// fully concrete. `Self` is the receiver's type `self_hir`.
fn project_assoc(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    subst: &HashMap<String, HirTy>,
    self_hir: Option<&HirTy>,
    subject: &str,
    assoc: &str,
    span: rokc_diagnostic::Span,
) -> Option<HirTy> {
    let self_hir = self_hir.filter(|_| subject == "Self");
    if self_hir.is_none() && !subst.contains_key(subject) {
        return None;
    }
    let seg = |n: &str| rokc_ast::PathSegment {
        name: n.to_owned(),
        generics: Vec::new(),
    };
    let proj = Ty {
        kind: rokc_ast::TyKind::Path(rokc_ast::Path {
            segments: vec![seg(subject), seg(assoc)],
            span,
        }),
        span,
    };
    let t = resolve_in_mono(ctx, fn_def, subst, self_hir.cloned(), &proj);
    let mut unknown = Vec::new();
    collect_unknown_paths(&t, &mut unknown);
    (t != HirTy::Unknown && unknown.is_empty()).then_some(t)
}

/// `I: IntoIterator<Item = T>` with `I` bound: `T = I::Item`, so a later
/// closure argument `fn(T) -> U` learns its parameter type. The binding
/// may be structured (`Item = (A, B)`), from a bound or a `where`;
/// `where Self: ..` binds once the receiver's type `self_hir` is known.
fn bind_assoc_bindings(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    self_hir: Option<&HirTy>,
    subst: &mut HashMap<String, HirTy>,
) {
    let names = type_param_names(fn_def);
    for (subject, assoc, target, span) in assoc_bindings(fn_def) {
        let open = names
            .iter()
            .any(|n| !subst.contains_key(n) && super::expr::ast_ty_mentions(target, n));
        if !open {
            continue;
        }
        if let Some(t) = project_assoc(ctx, fn_def, subst, self_hir, &subject, assoc, span) {
            unify_param_ty_with(target, &t, &names, subst, ctx);
        }
    }
}

/// Does `ty` have another shape than `want` (a tuple of another arity, or
/// no tuple / fn at all)? Parameters and open types fit anything.
fn shape_differs(want: &Ty, ty: &HirTy) -> bool {
    match (&want.kind, ty) {
        (rokc_ast::TyKind::Tuple(ws), HirTy::Tuple(ts)) => {
            ws.len() != ts.len() || ws.iter().zip(ts).any(|(w, t)| shape_differs(w, t))
        }
        (rokc_ast::TyKind::Fn(wa, _, _), HirTy::Fn(ta, _, _) | HirTy::Closure { args: ta, .. }) => {
            wa.len() != ta.len()
        }
        (rokc_ast::TyKind::Tuple(_) | rokc_ast::TyKind::Fn(..), t) => matches!(
            t,
            HirTy::Unit
                | HirTy::Prim(_)
                | HirTy::None
                | HirTy::Sum(_)
                | HirTy::Tuple(_)
                | HirTy::Struct(_)
                | HirTy::Enum(_)
                | HirTy::Array(..)
                | HirTy::Fn(..)
                | HirTy::Closure { .. }
                | HirTy::GcBuf(_)
        ),
        _ => false,
    }
}

/// A call's `Assoc = (A, B)` binding that the argument's associated type
/// cannot take (`unzip` on items that are not pairs), as an error naming
/// the form to give.
fn assoc_shape_error(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    self_hir: Option<&HirTy>,
    subst: &HashMap<String, HirTy>,
) -> Option<String> {
    let names = type_param_names(fn_def);
    assoc_bindings(fn_def)
        .into_iter()
        .find_map(|(subject, assoc, target, span)| {
            // Only a tuple or fn form can mismatch in shape; a bare `T`
            // is left to the bound check.
            let shaped = matches!(
                target.kind,
                rokc_ast::TyKind::Tuple(_) | rokc_ast::TyKind::Fn(..)
            );
            if !shaped
                || !names
                    .iter()
                    .any(|n| super::expr::ast_ty_mentions(target, n))
            {
                return None;
            }
            let owner = match subject.as_str() {
                "Self" => self_hir,
                s => subst.get(s),
            };
            // A generic body's call: the owner is still a parameter.
            if owner.is_none_or(|o| super::bounds::has_param(ctx, o)) {
                return None;
            }
            let t = project_assoc(ctx, fn_def, subst, self_hir, &subject, assoc, span)?;
            if !shape_differs(target, &t) {
                return None;
            }
            let owner = owner.map_or_else(|| subject.clone(), |o| ctx.ty_name(o));
            let leaf = leaf_name(&fn_def.name);
            // An impl's copy of a trait method is `__impl<tag>_<name>`.
            let shown = leaf
                .strip_prefix("__impl")
                .and_then(|r| r.split_once('_'))
                .map_or(leaf, |(_, m)| m);
            let want = rokc_ast::print::print_ty(target);
            let fix = if assoc == "Item" {
                format!(" - turn each item into `{want}` first, e.g. with `.map(|x| ..)`")
            } else {
                format!(" - pass a type whose `{assoc}` is `{want}`")
            };
            Some(format!(
                "`{shown}` needs `{assoc}` of the form `{want}`, but `{owner}` has \
                 `{assoc} = {}`{fix}",
                ctx.ty_name(&t)
            ))
        })
}

/// Resolve `ty` under the substitution of a monomorphization in
/// progress, from `fn_def`'s module. `self_ty` (resolved) fills `Self`.
pub(super) fn resolve_in_mono(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    subst: &HashMap<String, HirTy>,
    self_ty: Option<HirTy>,
    ty: &Ty,
) -> HirTy {
    let mut rcx = LowerCtx::new(ctx.env, subst, self_ty, ctx.here());
    rcx.current_module = module_of(&fn_def.name);
    rcx.set_bounds(&fn_def.generics, &fn_def.where_clause);
    rcx.resolve_ast_ty(ty)
}

/// Instantiation limits: deeper nesting means the generic recursion
/// never reaches a fixed point (`fn r<T>(x: T) { r((x, x)) }`).
pub(super) const MONO_DEPTH_LIMIT: u32 = 64;
pub(super) const MONO_TYPE_SIZE_LIMIT: usize = 256;

pub(super) fn ty_size(ty: &HirTy) -> usize {
    1 + match ty {
        HirTy::Sum(p) | HirTy::Tuple(p) => p.iter().map(ty_size).sum(),
        HirTy::Array(e, _) => ty_size(e),
        HirTy::Fn(a, r, _) => a.iter().map(ty_size).sum::<usize>() + ty_size(r),
        HirTy::Closure { args, ret, env, .. } => {
            args.iter().map(ty_size).sum::<usize>() + ty_size(ret) + ty_size(env)
        }
        HirTy::Throws { ok, err } => ty_size(ok) + ty_size(err),
        _ => 0,
    }
}

/// Find or make the instance of a generic function for the call's lowered
/// argument types; `(fn id, return type)`, or `None` after reporting.
pub(super) fn monomorphize(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    self_ty: Option<&Ty>,
    args: &[HirExpr],
    span: rokc_diagnostic::Span,
) -> Option<(FunctionId, HirTy)> {
    monomorphize_on(ctx, fn_def, self_ty, None, args, span)
}

/// Bind the impl parameters of `self_ty` (`Box<T>`) from the known
/// `Self` type `hint` (`Box<i32>` → `T = i32`).
pub(super) fn bind_self(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    self_ty: &Ty,
    hint: &HirTy,
    subst: &mut HashMap<String, HirTy>,
) {
    let names: std::collections::HashSet<String> = fn_def
        .generics
        .params
        .iter()
        .filter_map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. } => Some(name.clone()),
            rokc_ast::GenericParam::Const { .. } => None,
        })
        .collect();
    unify_param_ty_with(self_ty, hint, &names, subst, ctx);
    if let HirTy::Param(p) = hint {
        super::bounds::bind_trait_args(ctx, fn_def, *p, subst);
    }
}

/// `monomorphize` with `Self` known up front (see `bind_self`).
pub(super) fn monomorphize_on(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    self_ty: Option<&Ty>,
    self_hint: Option<&HirTy>,
    args: &[HirExpr],
    span: rokc_diagnostic::Span,
) -> Option<(FunctionId, HirTy)> {
    let fn_module = module_of(&fn_def.name);
    // Unify each parameter's AST type with its argument's type; the
    // argument type check reports conflicts.
    let mut subst: HashMap<String, HirTy> = ctx.explicit_generics.clone();
    if let (Some(t), Some(hint)) = (self_ty, self_hint) {
        bind_self(ctx, fn_def, t, hint, &mut subst);
    }
    for (i, arg) in args.iter().enumerate() {
        bind_from_arg(ctx, fn_def, self_ty, i, &arg.ty, &mut subst);
    }
    // A parameter the arguments leave open (only in the return type, or
    // only met `!`) binds from the expected type.
    if let Some(expected) = ctx.current_expected().filter(|t| **t != HirTy::Unknown) {
        let names: std::collections::HashSet<String> = fn_def
            .generics
            .params
            .iter()
            .filter_map(|gp| match gp {
                rokc_ast::GenericParam::Type { name, .. }
                | rokc_ast::GenericParam::Const { name, .. }
                    if subst.get(name).is_none_or(|t| *t == HirTy::Never) =>
                {
                    Some(name.clone())
                }
                _ => None,
            })
            .collect();
        if !names.is_empty() {
            let ret = sig_ret_ast(fn_def);
            let ok = match &ret.kind {
                rokc_ast::TyKind::Throws { ok, .. } => ok.as_ref(),
                _ => &ret,
            };
            unify_param_ty_with(ok, expected, &names, &mut subst, ctx);
        }
    }

    if let Some(HirTy::Param(p)) = self_hint {
        if let Err(msg) = super::bounds::pick_bound(ctx, fn_def, *p, &mut subst) {
            ctx.emit_error(rokc_diagnostic::ErrorKind::Type, msg);
            return None;
        }
    }

    // Spec 2.10.5: unbound params take their defaults. One pass: a default
    // can't depend on another default.
    for gp in &fn_def.generics.params {
        if let rokc_ast::GenericParam::Type {
            name,
            default: Some(d),
            ..
        } = gp
        {
            if !subst.contains_key(name) {
                // Earlier parameters stay in scope: `C = List<T>`.
                let resolved = resolve_in_mono(ctx, fn_def, &subst, None, d);
                subst.insert(name.clone(), resolved);
            }
        }
    }

    if let Some(msg) = assoc_shape_error(ctx, fn_def, self_hint, &subst) {
        ctx.emit_error(rokc_diagnostic::ErrorKind::Type, msg);
        return None;
    }

    // Every generic param must be inferable.
    let mut subst_tys: Vec<HirTy> = Vec::new();
    for gp in &fn_def.generics.params {
        match gp {
            rokc_ast::GenericParam::Type { name, .. } => {
                if let Some(t) = subst.get(name) {
                    subst_tys.push(t.clone());
                } else if ctx.infer_vars {
                    let v = super::infer::fresh(ctx);
                    subst.insert(name.clone(), v.clone());
                    subst_tys.push(v);
                } else {
                    let msg = if fn_def.name == "rok::core::collections::list::list_literal" {
                        "cannot infer the element type of the list literal from its \
                         elements, the expected type or a later use - annotate the \
                         binding: `let xs: List<i32> = [];`"
                            .to_owned()
                    } else {
                        format!(
                            "cannot infer type parameter `{name}` of `{}` from the call \
                             arguments, the expected type or a later use - annotate the \
                             binding or write `::<..>`",
                            ctx.display_path(&fn_def.name)
                        )
                    };
                    ctx.emit_uninferred(msg);
                    ctx.tag_last(rokc_diagnostic::Tag::TypeParamInfer);
                    return None;
                }
            }
            rokc_ast::GenericParam::Const { name, ty } => {
                let Some(v) = subst.get(name).and_then(HirTy::as_const) else {
                    ctx.emit_uninferred(format!(
                        "cannot infer const parameter `{name}` of `{}` from the call \
                         arguments or the expected type - pass it explicitly (`::<..>`) \
                         (spec 2.11.6)",
                        ctx.display_path(&fn_def.name)
                    ));
                    return None;
                };
                let c = typed_const(ctx, name, ty, v)?;
                subst.insert(name.clone(), c.clone());
                subst_tys.push(c);
            }
        }
    }
    if subst_tys.iter().any(|t| matches!(t, HirTy::Unknown)) {
        // An argument already failed to lower (reported).
        return None;
    }
    if subst_tys.iter().any(|t| super::infer::has_var(ctx, t)) {
        return Some(var_instance(ctx, fn_def, self_ty, self_hint, &subst));
    }
    if subst_tys.iter().any(|t| ty_size(t) > MONO_TYPE_SIZE_LIMIT)
        || ctx.env.sink.mono_depth.get() >= MONO_DEPTH_LIMIT
    {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "instantiating generic function `{}` recursed past the limit \
                 (depth {MONO_DEPTH_LIMIT}) - the instantiation never reaches a \
                 fixed point",
                fn_def.name
            ),
        );
        return None;
    }

    // A known `Self` (the receiver's type) is used as is: the template's
    // own module may not see the impl's type (a trait default method).
    let self_hir = self_hint
        .cloned()
        .or_else(|| self_ty.map(|t| resolve_in_mono(ctx, fn_def, &subst, None, t)));
    check_call_bounds(ctx, fn_def, &subst, self_hir.as_ref(), &fn_module);

    // A generic body being checked (`bounds`): the callee's signature
    // only, unless its return type comes from its body.
    if !infers_ret(fn_def, |s| ctx.env.diverges(s))
        && subst_tys
            .iter()
            .chain(&self_hir)
            .any(|t| super::bounds::has_param(ctx, t))
    {
        return Some(var_instance(ctx, fn_def, self_ty, self_hint, &subst));
    }
    let key = (
        fn_def.name.clone(),
        subst_tys.iter().map(ty_mangle).collect::<Vec<_>>(),
    );
    let existing = ctx.env.sink.mono_cache.borrow().get(&key).copied();
    super::incr::log(
        || format!("m|{}|{}", key.0, key.1.join(",")),
        || super::incr::Lookup::Mono(key.0.clone(), subst_tys.clone(), existing),
    );
    if let Some(existing_id) = existing {
        if being_inferred(ctx.env, existing_id) {
            return None;
        }
        let ret_ty = ctx
            .env
            .sink
            .mono_sigs
            .borrow()
            .get(&existing_id)
            .map(|s| s.ret_ty.clone())
            .expect("mono signature registered with its cache entry");
        return Some((existing_id, ret_ty));
    }

    let new_id = FunctionId(ctx.env.sink.next_id.get());
    ctx.env.sink.next_id.set(new_id.0 + 1);
    // Pre-cache so a recursive call inside the body finds the in-progress mono.
    super::incr::touch(new_id);
    super::incr::touch_key(|| super::incr::Key::Mono(key.0.clone(), subst_tys.clone()));
    ctx.env.sink.mono_cache.borrow_mut().insert(key, new_id);
    // Resolve the return type up front for recursive calls in the body.
    let mut preview_ret_ty =
        resolve_in_mono(ctx, fn_def, &subst, self_hir.clone(), &sig_ret_ast(fn_def));
    if infers_ret(fn_def, |s| ctx.env.diverges(s)) {
        preview_ret_ty = with_ok(preview_ret_ty, HirTy::Unknown);
    }
    let deferred = defers_refs(fn_def);
    let refs: Vec<(usize, HirTy)> = fn_def
        .params
        .iter()
        .enumerate()
        .filter(|(_, p)| p.by_ref)
        .map(|(i, p)| {
            let ty = match (&p.ty, &self_hir) {
                (_, Some(st)) if p.name == "self" => st.clone(),
                (Some(t), _) => resolve_in_mono(ctx, fn_def, &subst, self_hir.clone(), t),
                (None, _) => HirTy::Any,
            };
            (i, ref_slot_ty(deferred, ty))
        })
        .collect();
    if !refs.is_empty() {
        ctx.env.sink.ref_out.borrow_mut().insert(
            new_id,
            RefOut {
                ret: preview_ret_ty.clone(),
                refs,
                deferred,
            },
        );
    }
    ctx.env.sink.mono_sigs.borrow_mut().insert(
        new_id,
        FnSig {
            id: new_id,
            ret_ty: preview_ret_ty,
            param_defaults: Vec::new(),
            param_tys: Vec::new(),
            param_names: Vec::new(),
            param_refs: Vec::new(),
            variadic: false,
        },
    );

    // Stable keys (types by name) so the symbol is the same in every build
    // that makes it (prebuilt users, REPL cells).
    let mono_args = subst_tys
        .iter()
        .map(HirTy::stable_key)
        .collect::<Vec<_>>()
        .join("_");
    let mangled_name = format!("{}{}{mono_args}", fn_def.name, rokc_ir_common::MONO_MARK);

    ctx.env
        .sink
        .mono_depth
        .set(ctx.env.sink.mono_depth.get() + 1);
    let mono_fn = lower_instance(
        ctx,
        fn_def,
        new_id,
        mangled_name,
        self_hir,
        fn_module,
        &subst,
        span,
    );
    ctx.env
        .sink
        .mono_depth
        .set(ctx.env.sink.mono_depth.get() - 1);
    let mut mono_fn = mono_fn;
    mono_fn.facts.mono_args = Some(mono_args);

    let ret_ty = mono_fn.ret_ty.clone();
    let mono_param_tys: Vec<HirTy> = mono_fn.params.iter().map(|p| p.ty.clone()).collect();
    let mono_param_names: Vec<String> = mono_fn.params.iter().map(|p| p.name.clone()).collect();
    ctx.env.sink.mono_sigs.borrow_mut().insert(
        new_id,
        FnSig {
            id: new_id,
            ret_ty: ret_ty.clone(),
            param_defaults: Vec::new(),
            param_tys: mono_param_tys,
            param_names: mono_param_names,
            param_refs: mono_fn.params.iter().map(|p| p.by_ref).collect(),
            variadic: false,
        },
    );
    ctx.env.sink.extras.borrow_mut().push(mono_fn);
    Some((new_id, ret_ty))
}

/// Each `T: Bound` and `where` predicate of a call of `fn_def` under
/// `subst` needs an impl.
pub(super) fn check_call_bounds(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    subst: &HashMap<String, HirTy>,
    self_hir: Option<&HirTy>,
    fn_module: &str,
) {
    let mut goals: Vec<(HirTy, &Vec<rokc_ast::Ty>)> = Vec::new();
    for gp in &fn_def.generics.params {
        if let rokc_ast::GenericParam::Type { name, bounds, .. } = gp {
            if let Some(concrete) = subst.get(name) {
                goals.push((concrete.clone(), bounds));
            }
        }
    }
    for w in &fn_def.where_clause {
        goals.push((
            resolve_in_mono(ctx, fn_def, subst, self_hir.cloned(), &w.ty),
            &w.bounds,
        ));
    }
    for (concrete, bounds) in goals {
        let concrete = &concrete;
        if method_target_for(concrete).is_none() {
            continue;
        }
        for bound in bounds {
            // `T: Iterator<Item = i32>` also checks the impl's arguments
            // and associated types.
            let Some(want) = ctx.bound_ref(bound, fn_module, |t| {
                resolve_in_mono(ctx, fn_def, subst, self_hir.cloned(), t)
            }) else {
                continue;
            };
            // A bound naming no trait was reported where it is written.
            if !ctx.env.names.is_trait(&want.path) {
                continue;
            }
            if !ctx.implements_trait(concrete, &want, 0) {
                // An auto trait says which part lacks it, and what to do.
                let why = if ctx.env.names.auto_traits.contains(want.path.as_str()) {
                    ctx.auto_trait_cause(concrete, &want.path, 0)
                } else {
                    String::new()
                };
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!(
                        "type `{}` does not satisfy trait bound `{}` required by `{}`{why}",
                        ctx.ty_name(concrete),
                        ctx.trait_name(&want),
                        // `task::spawn` lowered to its plain twin is `spawn`.
                        ctx.display_path(if fn_def.name == "rok::core::task::spawn_plain" {
                            "rok::core::task::spawn"
                        } else {
                            fn_def.name.as_str()
                        })
                    ),
                );
                if want.path == super::reflect::INSPECT {
                    ctx.help_last(super::reflect::derive_inspect_help(ctx, concrete));
                }
                if super::expr::is_order_trait(ctx, &want.path)
                    && super::expr::sum_value_terms(concrete) > 1
                {
                    ctx.help_last(Some(format!(
                        "a sum of different types has no order; {}",
                        super::expr::sum_order_help()
                    )));
                }
                // `int` / `float` into a float rounds, so it is no `From`.
                let to = ctx.ty_name(concrete);
                let from = ctx.trait_name(&want);
                if matches!(to.as_str(), "f32" | "f64")
                    && matches!(from.as_str(), "From<int>" | "From<float>")
                {
                    ctx.help_last(Some(format!(
                        "`From` is lossless; use `x.to_f64()` (rounds), or `try {to}::try_from(x)` \
                         to throw past the range"
                    )));
                }
                // `ThreadLocal::get` copies; other values are borrowed.
                if fn_def.name.starts_with("rok::std::thread::ThreadLocal")
                    && fn_def.name.ends_with("get")
                {
                    ctx.help_last(Some(
                        "use the value inside `with`: `tl.with(|v| ..)`, or take it out with \
                         `tl.replace(v)` / `tl.take()` (stdlib 3.4.4)"
                            .to_owned(),
                    ));
                }
            }
        }
    }
}

/// Is `id`'s return type being inferred (a call of it from its own
/// body)? Recorded for `infer_ret_of`.
fn being_inferred(env: &Env<'_>, id: FunctionId) -> bool {
    let open = matches!(inferred_ret(env, id), Some(None));
    if open {
        env.sink.recursive_ret.borrow_mut().push(id);
    }
    open
}

/// `lower_fn_inner` for an instance; one of an implicit generic
/// (spec 4.2.2) records its call site for the body's errors.
#[allow(clippy::too_many_arguments)]
fn lower_instance(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    id: FunctionId,
    name: String,
    self_hir: Option<HirTy>,
    module: String,
    subst: &HashMap<String, HirTy>,
    span: rokc_diagnostic::Span,
) -> HirFunction {
    let implicit = fn_def.generics.params.iter().any(
        |gp| matches!(gp, rokc_ast::GenericParam::Type { name, .. } if name.starts_with("T#")),
    );
    if implicit {
        let site = (fn_def.name.clone(), span);
        ctx.env.sink.call_sites.borrow_mut().push(site);
    }
    let f = lower_fn_inner(ctx.env, fn_def, id, name, self_hir, module, subst);
    if implicit {
        ctx.env.sink.call_sites.borrow_mut().pop();
    }
    f
}

/// An instance over `let` inference variables (`infer`): its signature
/// only - the probe that asked for it is rolled back.
fn var_instance(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    self_ty: Option<&Ty>,
    self_hint: Option<&HirTy>,
    subst: &HashMap<String, HirTy>,
) -> (FunctionId, HirTy) {
    let id = FunctionId(ctx.env.sink.next_id.get());
    ctx.env.sink.next_id.set(id.0 + 1);
    super::incr::touch(id);
    let self_hir = self_hint
        .cloned()
        .or_else(|| self_ty.map(|t| resolve_in_mono(ctx, fn_def, subst, None, t)));
    let ret_ty = resolve_in_mono(ctx, fn_def, subst, self_hir.clone(), &sig_ret_ast(fn_def));
    let param_tys: Vec<HirTy> = fn_def
        .params
        .iter()
        .map(|p| match (&p.ty, &self_hir) {
            (_, Some(st)) if p.name == "self" => st.clone(),
            (Some(t), _) => resolve_in_mono(ctx, fn_def, subst, self_hir.clone(), t),
            (None, _) => HirTy::Any,
        })
        .collect();
    let refs: Vec<(usize, HirTy)> = fn_def
        .params
        .iter()
        .zip(&param_tys)
        .enumerate()
        .filter(|(_, (p, _))| p.by_ref)
        .map(|(i, (_, t))| (i, ref_slot_ty(defers_refs(fn_def), t.clone())))
        .collect();
    if !refs.is_empty() {
        ctx.env.sink.ref_out.borrow_mut().insert(
            id,
            RefOut {
                ret: ret_ty.clone(),
                refs,
                deferred: defers_refs(fn_def),
            },
        );
    }
    ctx.env.sink.mono_sigs.borrow_mut().insert(
        id,
        FnSig {
            id,
            ret_ty: ret_ty.clone(),
            param_defaults: Vec::new(),
            param_names: fn_def.params.iter().map(|p| p.name.clone()).collect(),
            param_refs: fn_def.params.iter().map(|p| p.by_ref).collect(),
            param_tys,
            variadic: false,
        },
    );
    (id, ret_ty)
}

/// Lower one body; `subst` is the instance's substitution (empty if not
/// generic), `module` where its names resolve.
pub(super) fn lower_fn_inner(
    env: &Env<'_>,
    fn_def: &FnDef,
    id: FunctionId,
    name: String,
    self_ty: Option<HirTy>,
    module: String,
    subst: &HashMap<String, HirTy>,
) -> HirFunction {
    let ret = if env.infers_ret(id) {
        let r = decl_ret_ok(env, id);
        if fn_def.body.is_some() {
            super::incr::touch(id);
            if let Some((_, f)) = env.sink.inferred_fns.borrow_mut().remove(&id) {
                return f;
            }
        }
        Some(r)
    } else if infers_ret(fn_def, |s| env.diverges(s)) {
        let (r, done) = infer_ret_of(
            env,
            fn_def,
            id,
            &name,
            self_ty.clone(),
            &module,
            subst,
            true,
        );
        if let Some(f) = done {
            return f;
        }
        Some(r)
    } else {
        None
    };
    lower_fn_with(env, fn_def, id, name, self_ty, module, subst, ret, None)
}

/// Spec 4.1.3: without `-> T`, the return type comes from the body when it
/// has a value or may diverge everywhere (4.3.7). `main` keeps `()`.
pub(super) fn infers_ret(f: &FnDef, may_diverge: impl Fn(rokc_diagnostic::Span) -> bool) -> bool {
    struct V {
        depth: u32,
        found: bool,
    }
    impl crate::walk::AstVisitor for V {
        fn stmt(&mut self, s: &Stmt) {
            if self.depth == 0 && matches!(s.kind, StmtKind::Return(Some(_))) {
                self.found = true;
            }
        }
        fn enter_body(&mut self) {
            self.depth += 1;
        }
        fn leave_body(&mut self) {
            self.depth -= 1;
        }
    }
    let Some(body) = &f.body else {
        return false;
    };
    if f.ret_ty.is_some() || f.is_async || f.is_generator || f.name == "main" {
        return false;
    }
    if body.trailing.is_some() || may_diverge(f.span) {
        return true;
    }
    let mut v = V {
        depth: 0,
        found: false,
    };
    crate::walk::walk_block(&mut v, body);
    v.found
}

/// The ok side of the return type of declaration `id` (in
/// `Env::infer_ret`), inferred on first use.
fn decl_ret_ok(env: &Env<'_>, id: FunctionId) -> HirTy {
    let cached = inferred_ret(env, id);
    match cached {
        Some(Some(t)) => t,
        Some(None) => {
            env.sink.recursive_ret.borrow_mut().push(id);
            HirTy::Unknown
        }
        None => match env.decl(id).expect("a declaration") {
            MethodOrFn::Fn(f) => {
                let module = fn_module(env, f);
                let start = env.sink.next_id.get();
                let keep = !env.sink.bodies_done.get();
                let (t, done) =
                    infer_ret_of(env, f, id, &f.name, None, &module, &HashMap::new(), keep);
                keep_inferred(env, id, start, done);
                t
            }
            MethodOrFn::Method {
                fn_def,
                self_ty,
                module,
                key,
                subst,
            } => {
                let self_ty = Some(self_ty.clone());
                let start = env.sink.next_id.get();
                let keep = !env.sink.bodies_done.get();
                let (t, done) = infer_ret_of(env, fn_def, id, key, self_ty, module, subst, keep);
                keep_inferred(env, id, start, done);
                t
            }
        },
    }
}

/// A declaration lowered for good while inferring its return type: the
/// decl pass takes it (`lower_fn_inner`).
fn keep_inferred(env: &Env<'_>, id: FunctionId, start: u32, done: Option<HirFunction>) {
    if let Some(f) = done {
        super::incr::touch(id);
        env.sink.inferred_fns.borrow_mut().insert(id, (start, f));
    }
}

/// `decl_ret_ok` in the declared return type (`()`, or a `Throws`
/// carrier around it).
pub(super) fn decl_ret(ctx: &LowerCtx<'_>, id: FunctionId, declared: HirTy) -> HirTy {
    with_ok(declared, decl_ret_ok(ctx.env, id))
}

fn has_unknown(t: &HirTy) -> bool {
    match t {
        HirTy::Unknown => true,
        HirTy::Sum(ts) | HirTy::Tuple(ts) => ts.iter().any(has_unknown),
        HirTy::Array(t, _) | HirTy::GcBuf(t) => has_unknown(t),
        HirTy::Fn(ps, r, _) => ps.iter().any(has_unknown) || has_unknown(r),
        HirTy::Closure { args, ret, env, .. } => {
            args.iter().any(has_unknown) || has_unknown(ret) || has_unknown(env)
        }
        HirTy::Throws { ok, err } => has_unknown(ok) || has_unknown(err),
        _ => false,
    }
}

fn with_ok(declared: HirTy, ok: HirTy) -> HirTy {
    match declared {
        HirTy::Throws { err, .. } => HirTy::Throws {
            ok: Box::new(ok),
            err,
        },
        _ => ok,
    }
}

/// Lower the body with an open return type and join its `return` and tail
/// types (spec 4.3.4: one path, its type; several, their sum). With
/// `keep`, a body whose paths agree (no `throws`, no `ref` parameter) is
/// final as lowered; otherwise it is rolled back.
#[allow(clippy::too_many_arguments)]
fn infer_ret_of(
    env: &Env<'_>,
    fn_def: &FnDef,
    id: FunctionId,
    name: &str,
    self_ty: Option<HirTy>,
    module: &str,
    subst: &HashMap<String, HirTy>,
    keep: bool,
) -> (HirTy, Option<HirFunction>) {
    if let Some(Some(t)) = inferred_ret(env, id) {
        return (t, None);
    }
    super::incr::touch(id);
    env.sink.inferred_ret.borrow_mut().insert(id, None);
    let hits = env.sink.recursive_ret.borrow().len();
    let tentatives = env.sink.tentative_ret.borrow().len();
    let mut probe = LowerCtx::new(env, subst, None, fn_def.span);
    let mark = probe.probe_mark();
    let mut tys = Vec::new();
    let mut f = lower_fn_with(
        env,
        fn_def,
        id,
        name.to_owned(),
        self_ty,
        module.to_owned(),
        subst,
        Some(HirTy::Unknown),
        Some(&mut tys),
    );
    // A call of itself leaves its type open (`Unknown`); one of a fn
    // still being inferred further out makes this result tentative.
    let (self_hit, tentative) = {
        let mut hit = env.sink.recursive_ret.borrow_mut();
        let own = hit[hits..].contains(&id);
        let mut i = hits;
        while i < hit.len() {
            if hit[i] == id {
                hit.remove(i);
            } else {
                i += 1;
            }
        }
        (own, hit.len() > hits)
    };
    tys.retain(|t| *t != HirTy::Never);
    let plain = match &fn_def.throws {
        rokc_ast::ThrowsKind::None => true,
        rokc_ast::ThrowsKind::Typed(e) => e.is_empty(),
        rokc_ast::ThrowsKind::Untyped => false,
    } && !fn_def.params.iter().any(|p| p.by_ref);
    let agreed = tys.windows(2).all(|w| w[0] == w[1]);
    let final_as_is = keep && plain && agreed && !self_hit && !tentative;
    if !final_as_is {
        probe.rollback(&mark);
    }
    let unit = tys.contains(&HirTy::Unit);
    tys.retain(|t| *t != HirTy::Unit);
    let fail = |why: &str| {
        probe.emit_error_at(
            rokc_diagnostic::ErrorKind::Type,
            fn_def.span,
            format!(
                "cannot infer the return type of `{}`: {why} - write `-> T` (spec 4.1.3)",
                leaf_name(&fn_def.name)
            ),
        );
        HirTy::Unknown
    };
    // No path reaches the end or a `return`: `!` (spec 4.3.7).
    let ret = if tys.is_empty() && !unit {
        HirTy::Never
    } else if tys.is_empty() {
        HirTy::Unit
    } else if unit {
        if tentative {
            HirTy::Unknown
        } else {
            fail("one path returns a value and another none")
        }
    } else {
        let mut parts = Vec::new();
        for t in tys {
            t.spread(&mut parts);
        }
        HirTy::sum(parts)
    };
    if tentative {
        env.sink
            .inferred_ret
            .borrow_mut()
            .insert(id, Some(ret.clone()));
        env.sink.tentative_ret.borrow_mut().push(id);
        return (ret, None);
    }
    let stale: Vec<FunctionId> = env.sink.tentative_ret.borrow_mut().split_off(tentatives);
    for t in stale {
        env.sink.inferred_ret.borrow_mut().remove(&t);
    }
    let ret = if self_hit && has_unknown(&ret) {
        fail("its body calls it again")
    } else {
        ret
    };
    env.sink
        .inferred_ret
        .borrow_mut()
        .insert(id, Some(ret.clone()));
    if let Some(r) = env.sink.ref_out.borrow_mut().get_mut(&id) {
        r.ret = with_ok(r.ret.clone(), ret.clone());
    }
    // An instance's preview signature: a call made while its body is
    // lowered again (a recursive cycle through other instances) reads it.
    if let Some(sig) = env.sink.mono_sigs.borrow_mut().get_mut(&id) {
        sig.ret_ty = with_ok(sig.ret_ty.clone(), ret.clone());
    }
    if final_as_is {
        f.ret_ty = ret.clone();
        return (ret, Some(f));
    }
    (ret, None)
}

/// `lower_fn_inner` with the ok side of the return type given (`ret`;
/// `Unknown` with `ret_tys` to collect the body's value types).
#[allow(clippy::too_many_arguments)]
fn lower_fn_with(
    env: &Env<'_>,
    fn_def: &FnDef,
    id: FunctionId,
    name: String,
    self_ty: Option<HirTy>,
    module: String,
    subst: &HashMap<String, HirTy>,
    ret: Option<HirTy>,
    ret_tys: Option<&mut Vec<HirTy>>,
) -> HirFunction {
    // A `rethrows` instance with a non-throwing argument doesn't vote on
    // warnings (its `try`s cover nothing by construction).
    let nothrow = HirTy::NoThrow;
    let instance =
        (!subst.is_empty() && !subst.values().any(|t| *t == nothrow)).then_some((fn_def.span, id));
    if let Some((t, id)) = instance {
        env.sink.instance_warns.borrow_mut().instance(t, id);
    }
    env.sink.instances.borrow_mut().push(instance);
    env.sink.bodies.borrow_mut().push(fn_def.span);
    let is_const = fn_def
        .attrs
        .iter()
        .any(|a| a.path == [rokc_ast::CONST_FN_ATTR]);
    let is_std = module == "rok" || module.starts_with("rok::");
    super::incr::touch(id);
    env.sink
        .fn_origin
        .borrow_mut()
        .insert(id, (is_const, is_std));
    let mut ctx = LowerCtx::new(env, subst, self_ty, fn_def.span);
    if let Some(body) = &fn_def.body {
        ctx.sole_names = sole_names(body);
    }
    if fn_def.attrs.iter().any(|a| a.path == ["track_caller"]) {
        let fixed_abi = fn_def
            .attrs
            .iter()
            .any(|a| a.path == [rokc_ast::EXTERN_FN_ATTR] || a.name() == "export_name");
        // The location is a hidden parameter: no fixed signature.
        if fixed_abi || name == "main" {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "`#[track_caller]` on `{}`: not on `main` or a fixed-ABI \
                     (`extern \"C\"`, `#[export_name]`) function",
                    fn_def.name
                ),
            );
        } else {
            env.sink.track_caller.borrow_mut().insert(id);
        }
    }
    ctx.current_module = module;
    ctx.set_bounds(&fn_def.generics, &fn_def.where_clause);
    ctx.assoc_names = fn_def
        .attrs
        .iter()
        .filter(|a| a.path.first().is_some_and(|p| p == crate::ASSOC_TYPES_ATTR))
        .flat_map(|a| a.path[1..].iter().cloned())
        .collect();
    ctx.unsafe_depth = u32::from(fn_def.is_unsafe);
    // The synthesized entry (spec 4.16.2).
    ctx.implicit_main = env.implicit_main && name == "main" && ctx.self_ty.is_none();
    ctx.harness_main = env.harness_main
        && name == "main"
        && ctx.current_module.is_empty()
        && ctx.self_ty.is_none();

    if let Some(b) = &fn_def.body {
        ctx.ref_captured = ref_captured_bindings(&fn_def.params, b);
    }
    let mut params = Vec::with_capacity(fn_def.params.len());
    for p in &fn_def.params {
        params.push(lower_param(&mut ctx, p));
    }
    // A `[ref]`-captured parameter moves into a cell (`let x = x;`).
    let mut prelude = Vec::new();
    // An `async` method's `ref self` is shared with its future through a
    // cell, which the call hands back (`defers_refs`).
    ctx.ref_cells = defers_refs(fn_def);
    for (p, ap) in params.iter().zip(&fn_def.params) {
        if ctx.ref_captured.contains(&(p.name.clone(), ap.span)) || (ctx.ref_cells && p.by_ref) {
            // A by-value `self` is not a slot to write.
            if p.name == "self" && !p.by_ref {
                ctx.emit_error_at(
                    rokc_diagnostic::ErrorKind::Type,
                    ap.span,
                    "`[ref self]` needs a `ref self` method (spec 4.7.4)".to_owned(),
                );
                ctx.help_last(Some(
                    "write `ref self` in the method's parameters, or capture `self` by value \
                     (drop it from the list)"
                        .to_owned(),
                ));
            }
            let init = hir_local(p.local, &p.ty, fn_def.span);
            prelude.push(ctx.cell_let(&p.name, p.ty.clone(), init));
        }
    }
    ctx.owe_params = params
        .iter()
        .zip(&fn_def.params)
        .filter(|(p, ap)| {
            // An `async` / generator body runs in its coroutine, which
            // owns the captured parameters instead.
            !fn_def.is_async
                && !fn_def.is_generator
                && !p.by_ref
                && !ctx.ref_captured.contains(&(p.name.clone(), ap.span))
        })
        .map(|(p, _)| (p.local, p.ty.clone()))
        .collect();
    // A captured `ref` parameter hands back its cell, where the writes go.
    ctx.ref_params = params
        .iter()
        .filter(|p| p.by_ref)
        .map(|p| {
            let l = ctx.lookup(&p.name).unwrap_or(p.local);
            (l, ref_slot_ty(ctx.ref_cells, p.ty.clone()))
        })
        .collect();

    let mut ret_ty = ctx.resolve_ast_ty(&sig_ret_ast(fn_def));
    if let Some(r) = ret {
        ret_ty = with_ok(ret_ty, r);
    }
    if ret_tys.is_some() {
        ctx.ret_tys = Some(Vec::new());
    }
    if let HirTy::Throws { ok, err } = &ret_ty {
        ctx.fn_ret_ty = (**ok).clone();
        // Spec 4.16.3: an error escaping a throwing `main` is reported by
        // the entry itself (exit 1); `main` keeps a plain C return type.
        if name == "main" && ctx.self_ty.is_none() {
            ctx.uncaught_in_main = true;
            ret_ty = ctx.fn_ret_ty.clone();
        } else {
            ctx.fn_throws = Some((**err).clone());
        }
    } else {
        ctx.fn_ret_ty = ret_ty.clone();
    }

    let body = fn_def.body.as_ref().map(|b| {
        let mut block = lower_fn_body(&mut ctx, b, &fn_def.name);
        block.stmts.splice(0..0, prelude);
        block
    });
    if let Some(out) = ret_tys {
        *out = ctx.ret_tys.take().unwrap_or_default();
    }
    env.sink.instances.borrow_mut().pop();
    env.sink.bodies.borrow_mut().pop();
    // `ref` parameters: the function hands their values back too.
    let ref_tys: Vec<HirTy> = ctx.ref_params.iter().map(|(_, t)| t.clone()).collect();
    ret_ty = ref_packed(&ret_ty, &ref_tys);

    HirFunction {
        id,
        def_id: None,
        name,
        params,
        locals: ctx.into_locals(),
        ret_ty,
        body,
        is_async: false,
        is_generator: false,
        span: fn_def.span,
        facts: rokc_ir_common::FnFacts {
            inline: inline_hint(&fn_def.attrs),
            ..rokc_ir_common::FnFacts::default()
        },
    }
}

/// The `#[inline]` hint of a function (spec 6.2.4); `#[cold]` (6.2.6)
/// unless it is `#[inline(always)]`.
fn inline_hint(attrs: &[rokc_ast::Attribute]) -> rokc_ir_common::Inline {
    use rokc_ast::{AttrArg, AttrBody};
    use rokc_ir_common::Inline;
    let hint = match attrs.iter().find(|a| a.name() == "inline").map(|a| &a.body) {
        None => Inline::Auto,
        Some(AttrBody::Args(xs)) => match xs.as_slice() {
            [AttrArg::Word(w)] if w == "always" => Inline::Always,
            [AttrArg::Word(w)] if w == "never" => Inline::Never,
            _ => Inline::Hint,
        },
        Some(_) => Inline::Hint,
    };
    if hint != Inline::Always && attrs.iter().any(|a| a.name() == "cold") {
        return Inline::Cold;
    }
    hint
}

/// `{T → arg}` for a generic item's type parameters.
pub(super) fn generic_subst(
    generics: &rokc_ast::Generics,
    args: &[HirTy],
) -> HashMap<String, HirTy> {
    let mut subst = HashMap::new();
    for (i, gp) in generics.params.iter().enumerate() {
        let (rokc_ast::GenericParam::Type { name, .. }
        | rokc_ast::GenericParam::Const { name, .. }) = gp;
        if let Some(arg) = args.get(i) {
            subst.insert(name.clone(), arg.clone());
        }
    }
    subst
}

/// Every parameter has a default (spec 2.10.5).
pub(super) fn all_defaulted(generics: &rokc_ast::Generics) -> bool {
    generics.params.iter().all(|p| {
        matches!(
            p,
            rokc_ast::GenericParam::Type {
                default: Some(_),
                ..
            }
        )
    })
}

/// Spec 2.10.5: omitted trailing type arguments take their defaults. Then
/// `args` must fit `generics`: too long is cut, too short padded with
/// `Unknown` (so uses add no errors), and the error comes back.
pub(super) fn fit_args(
    name: &str,
    generics: &rokc_ast::Generics,
    args: &mut Vec<HirTy>,
    res: impl Fn(&HashMap<String, HirTy>, &Ty) -> HirTy,
) -> Option<(String, String)> {
    let given = args.len();
    while let Some(rokc_ast::GenericParam::Type {
        default: Some(d), ..
    }) = generics.params.get(args.len())
    {
        let t = res(&generic_subst(generics, args), d);
        args.push(t);
    }
    if args.len() == generics.params.len() {
        return None;
    }
    let e = misfit(name, generics, given);
    args.resize(generics.params.len(), HirTy::Unknown);
    Some(e)
}

/// The error (message, help) for `given` arguments to `name<generics>`,
/// a count that does not fit.
pub(super) fn misfit(name: &str, generics: &rokc_ast::Generics, given: usize) -> (String, String) {
    let max = generics.params.len();
    let min = generics
        .params
        .iter()
        .take_while(|p| {
            !matches!(
                p,
                rokc_ast::GenericParam::Type {
                    default: Some(_),
                    ..
                }
            )
        })
        .count();
    let fix = match given.checked_sub(max) {
        Some(1) => "remove the last argument".to_owned(),
        Some(n) => format!("remove the last {n} arguments"),
        None => {
            let missing: Vec<String> = generics.params[given..min]
                .iter()
                .map(|p| format!("`{}`", param_name(p)))
                .collect();
            format!("add one for {}", missing.join(", "))
        }
    };
    (
        arity_msg(name, min, max, given),
        format!("`{name}` is declared `{}`; {fix}", declared(name, generics)),
    )
}

/// The error (message, help) for `name<generics>` written with no
/// arguments where some have no default.
pub(super) fn bare_generic(name: &str, generics: &rokc_ast::Generics) -> (String, String) {
    let example: Vec<&str> = generics
        .params
        .iter()
        .map_while(|p| match p {
            rokc_ast::GenericParam::Type { default: None, .. } => Some("i32"),
            rokc_ast::GenericParam::Type { .. } => None,
            rokc_ast::GenericParam::Const { .. } => Some("1"),
        })
        .collect();
    (
        format!("`{name}` needs generic arguments"),
        format!(
            "`{name}` is declared `{}`; write it with them, e.g. `{name}<{}>`",
            declared(name, generics),
            example.join(", ")
        ),
    )
}

/// "`S` takes 1 generic argument but 2 were given" (`min..=max` taken).
pub(super) fn arity_msg(name: &str, min: usize, max: usize, given: usize) -> String {
    let takes = match (min, max) {
        (0, 0) => "no generic arguments".to_owned(),
        (1, 1) => "1 generic argument".to_owned(),
        (a, b) if a == b => format!("{b} generic arguments"),
        (0, 1) => "at most 1 generic argument".to_owned(),
        (0, b) => format!("at most {b} generic arguments"),
        (a, b) => format!("{a} to {b} generic arguments"),
    };
    let was = if given == 1 { "was" } else { "were" };
    format!("`{name}` takes {takes} but {given} {was} given")
}

/// `S<T, U = i32>` as declared.
pub(super) fn declared(name: &str, generics: &rokc_ast::Generics) -> String {
    let params: Vec<String> = generics
        .params
        .iter()
        .map(|p| match p {
            rokc_ast::GenericParam::Type {
                name,
                default: Some(d),
                ..
            } => format!("{name} = {}", rokc_ast::print::print_ty(d)),
            rokc_ast::GenericParam::Type { name, .. } => name.clone(),
            rokc_ast::GenericParam::Const { name, ty } => {
                format!("const {name}: {}", rokc_ast::print::print_ty(ty))
            }
        })
        .collect();
    format!("{name}<{}>", params.join(", "))
}

fn param_name(p: &rokc_ast::GenericParam) -> &str {
    let (rokc_ast::GenericParam::Type { name, .. } | rokc_ast::GenericParam::Const { name, .. }) =
        p;
    name
}

/// Spec 2.11.3: const argument `v` for `name: ty`, range-checked and
/// tagged with the parameter's type.
pub(super) fn typed_const(
    ctx: &LowerCtx<'_>,
    name: &str,
    ty: &Ty,
    (v, tag): (i128, Option<PrimTy>),
) -> Option<HirTy> {
    let p = match ctx.resolve_ast_ty(ty) {
        HirTy::Prim(p) if p.is_int() || matches!(p, PrimTy::Bool | PrimTy::CChar) => p,
        other => {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "const parameter `{name}` has type `{}`; only integers, `bool`, `char` \
                     and `c_char` may be const parameters (spec 2.11.3)",
                    ctx.ty_name(&other)
                ),
            );
            return None;
        }
    };
    let is_char = tag == Some(PrimTy::Char);
    if is_char != (p == PrimTy::Char) {
        let found = if is_char { "a `char`" } else { "an integer" };
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "const argument for `{name}: {}` must be a `{}`, found {found}",
                p.name(),
                p.name()
            ),
        );
        return None;
    }
    let fits = match p {
        PrimTy::Bool => v == 0 || v == 1,
        PrimTy::Char => u32::try_from(v).ok().and_then(char::from_u32).is_some(),
        _ => super::pattern::int_fits(v, p),
    };
    if !fits {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "const argument `{v}` out of range for `{name}: {}`",
                p.name()
            ),
        );
        return None;
    }
    Some(HirTy::Const(v, Some(p)))
}

/// Field list of a generic struct instantiated with `args`.
/// `res` resolves a field type under the instantiation's substitution.
pub(super) fn generic_struct_fields(
    def: &AstStructDef,
    res: &dyn Fn(&Ty) -> HirTy,
) -> Vec<HirField> {
    let field = |name: String, ty: &Ty, default: Option<Expr>| HirField {
        name,
        ty: res(ty),
        default,
    };
    match &def.kind {
        AstStructKind::Named(fs) => fs
            .iter()
            .map(|f| {
                let d = f
                    .default
                    .clone()
                    .map(|d| in_module(&module_of(&def.name), d));
                field(f.name.clone(), &f.ty, d)
            })
            .collect(),
        AstStructKind::Tuple(fs) => fs
            .iter()
            .enumerate()
            .map(|(i, f)| field(i.to_string(), &f.ty, None))
            .collect(),
        AstStructKind::Unit => Vec::new(),
    }
}

/// Variants of a generic enum instantiation; `res` resolves a payload
/// type under the instantiation's substitution.
pub(super) fn generic_enum_variants(
    def: &AstEnumDef,
    res: &dyn Fn(&Ty) -> HirTy,
) -> Vec<HirEnumVariant> {
    def.variants
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let tag = u32::try_from(i).expect("variant index overflow");
            let (payload_ty, named_fields, arity) = match &v.kind {
                rokc_ast::EnumVariantKind::Unit | rokc_ast::EnumVariantKind::Value(_) => {
                    (None, Vec::new(), 0)
                }
                rokc_ast::EnumVariantKind::Tuple(tys) if tys.len() == 1 => {
                    (Some(res(&tys[0])), Vec::new(), 1)
                }
                rokc_ast::EnumVariantKind::Tuple(tys) => (
                    Some(HirTy::Tuple(tys.iter().map(res).collect())),
                    Vec::new(),
                    tys.len(),
                ),
                rokc_ast::EnumVariantKind::Named(fields) => (
                    Some(HirTy::Tuple(fields.iter().map(|f| res(&f.ty)).collect())),
                    fields.iter().map(|f| f.name.clone()).collect(),
                    fields.len(),
                ),
            };
            HirEnumVariant {
                name: v.name.clone(),
                tag: tag.into(),
                payload_ty,
                named_fields,
                arity,
            }
        })
        .collect()
}

pub(super) fn lower_fn(
    env: &Env<'_>,
    fn_def: &FnDef,
    id: FunctionId,
    method: Option<(HirTy, String)>,
    module: String,
) -> HirFunction {
    // Methods are named by `fn_table` key (`Ty__m`) so same-named methods
    // don't collide as MLIR symbols.
    let (method_self_ty, name) = match method {
        Some((ty, key)) => (Some(ty), key),
        None => (None, fn_def.name.clone()),
    };
    lower_fn_inner(
        env,
        fn_def,
        id,
        name,
        method_self_ty,
        module,
        &HashMap::new(),
    )
}

/// Parameter and return types of declaration `i` resolved as its body
/// sees them: in its module, `Self` and trait parameters bound.
pub(super) fn decl_sig(env: &Env<'_>, i: usize, base: &FnSig) -> FnSig {
    let no_subst = HashMap::new();
    let id = FunctionId(u32::try_from(i).expect("FunctionId overflow"));
    let (f, self_ty, module, subst) = match env.decl(id).expect("a declaration") {
        MethodOrFn::Fn(f) => (*f, None, fn_module(env, f), &no_subst),
        MethodOrFn::Method {
            fn_def,
            self_ty,
            module,
            subst,
            ..
        } => (&**fn_def, Some(self_ty.clone()), module.clone(), subst),
    };
    let (param_tys, ret_ty) = sig_tys(env, f, self_ty, module, subst);
    FnSig {
        ret_ty,
        param_tys,
        ..base.clone()
    }
}

/// `f`'s parameter and result types, resolved in `module`.
pub(super) fn sig_tys(
    env: &Env<'_>,
    f: &FnDef,
    self_ty: Option<HirTy>,
    module: String,
    subst: &HashMap<String, HirTy>,
) -> (Vec<HirTy>, HirTy) {
    let mut ctx = LowerCtx::new(env, subst, self_ty, f.span);
    ctx.current_module = module;
    ctx.set_bounds(&f.generics, &f.where_clause);
    ctx.assoc_names = f
        .attrs
        .iter()
        .filter(|a| a.path.first().is_some_and(|p| p == crate::ASSOC_TYPES_ATTR))
        .flat_map(|a| a.path[1..].iter().cloned())
        .collect();
    let param_tys = f
        .params
        .iter()
        .map(|p| match (&ctx.self_ty, p.name.as_str(), &p.ty) {
            (Some(st), "self", _) => st.clone(),
            (_, _, Some(t)) => ctx.resolve_ast_ty(t),
            (_, _, None) => HirTy::Any,
        })
        .collect();
    let ret_ty = ctx.resolve_ast_ty(&super::throws::sig_ret_ast(f));
    (param_tys, ret_ty)
}

pub(super) fn lower_param(ctx: &mut LowerCtx<'_>, p: &Param) -> HirParam {
    let ty = if p.name == "self" {
        // Implicit `self` receiver inherits the impl target's type.
        ctx.self_ty.clone().unwrap_or(HirTy::Any)
    } else {
        p.ty.as_ref().map_or(HirTy::Any, |t| ctx.resolve_ast_ty(t))
    };
    let local = ctx.declare(Some(p.name.clone()), ty.clone());
    HirParam {
        name: p.name.clone(),
        local,
        ty,
        by_ref: p.by_ref,
    }
}

/// Lower a body against its return type: the tail must match, and a
/// non-unit function must not fall off the end.
pub(super) fn lower_fn_body(ctx: &mut LowerCtx<'_>, b: &Block, fn_name: &str) -> HirBlock {
    ctx.push_expected(ctx.fn_ret_ty.clone());
    if ctx.uncaught_in_main {
        main_frame(ctx);
    }
    if let Some(t) = &b.trailing {
        super::expr::guard_sink(ctx, t);
    }
    // Parameters the body owns are released on its exits.
    ctx.push_scope();
    let owed = ctx.err_frames.len();
    let mut flags = Vec::new();
    for (l, ty) in std::mem::take(&mut ctx.owe_params) {
        flags.extend(super::affine::owe(ctx, l, &ty));
    }
    let mut block = lower_block(ctx, b);
    if !flags.is_empty() {
        flags.append(&mut block.stmts);
        block.stmts = flags;
        super::affine::settle(ctx, owed, &mut block.stmts, &mut block.trailing, b.span);
    }
    ctx.pop_scope();
    ctx.pop_expected();
    let mut block = check_fn_body(ctx, block, fn_name);
    if ctx.uncaught_in_main {
        block = uncaught_in_main(ctx, block, b.span);
    }
    finish_throwing_body(ctx, &mut block);
    if !ctx.ref_params.is_empty() && !hir_block_diverges(&block) {
        let span = b.span;
        let v = block
            .trailing
            .take()
            .unwrap_or_else(|| super::throws::unit_value(span));
        block.trailing = Some(ref_pack(ctx, v, span));
    }
    block
}

/// The value checks of `lower_fn_body`.
fn check_fn_body(ctx: &mut LowerCtx<'_>, mut block: HirBlock, fn_name: &str) -> HirBlock {
    let ret = ctx.fn_ret_ty.clone();
    let diverges = hir_block_diverges(&block);
    if let Some(tys) = ctx.ret_tys.as_mut() {
        match &block.trailing {
            Some(t) if !hir_expr_diverges(t) => tys.push(t.ty.clone()),
            None if !diverges => tys.push(HirTy::Unit),
            _ => {}
        }
    }
    let falls_off = || {
        let name = leaf_name(fn_name);
        if ret == HirTy::Never {
            format!(
                "function `{name}` returns `!` but its body can finish; it must end in a \
                 call of a `-> !` function or a `loop` without `break`"
            )
        } else {
            format!(
                "function `{name}` returns `{}` but its body can finish without returning a \
                 value (missing `return`)",
                ctx.ty_name(&ret)
            )
        }
    };
    match block.trailing.take() {
        // A unit function discards a trailing value (`fn f() { g() }`).
        Some(t) if ret == HirTy::Unit && t.ty != HirTy::Unit => block.stmts.push(HirStmt::Expr(t)),
        Some(t) if hir_expr_diverges(&t) => block.trailing = Some(t),
        // `fn f() -> i32 { if c { return 1; } }`: a unit-valued tail
        // means some path falls off the end.
        Some(t) if t.ty == HirTy::Unit && !matches!(ret, HirTy::Unknown | HirTy::Unit) => {
            ctx.emit_error(rokc_diagnostic::ErrorKind::Type, falls_off());
            block.trailing = Some(t);
        }
        Some(t) => {
            let name = leaf_name(fn_name).to_owned();
            block.trailing = Some(coerce(ctx, &ret, t, &|| format!("the body of `{name}`")));
        }
        None => {
            if !matches!(ret, HirTy::Unit | HirTy::Unknown) && !hir_block_diverges(&block) {
                // `fn f() -> i32 { a + b; }`: the `;` drops the value.
                let last = match block.stmts.last() {
                    Some(HirStmt::Expr(e)) if e.ty == ret => ctx.src(e.span).map(|t| (e.span, t)),
                    _ => None,
                };
                if let Some((at, text)) = last {
                    ctx.emit_error_at(
                        rokc_diagnostic::ErrorKind::Type,
                        at,
                        format!(
                            "function `{}` returns `{}`, but the `;` after its last expression \
                             drops the value",
                            leaf_name(fn_name),
                            ctx.ty_name(&ret)
                        ),
                    );
                    ctx.help_last(Some(format!("remove the `;` to return it: `{text}`")));
                } else {
                    ctx.emit_error(rokc_diagnostic::ErrorKind::Type, falls_off());
                }
            }
        }
    }
    block
}

/// An `async` method with `ref self`: its future runs the body after the
/// call returns, so the call hands back the parameter's cell, read once
/// the future completes (`ref_unpack`).
pub(super) fn defers_refs(f: &FnDef) -> bool {
    f.params.iter().any(|p| p.by_ref)
        && f.attrs
            .iter()
            .any(|a| a.path.first().map(String::as_str) == Some(rokc_ast::CORO_FN_ATTR))
}

/// A `ref` parameter's slot in the packed return: its cell when deferred.
pub(super) fn ref_slot_ty(deferred: bool, t: HirTy) -> HirTy {
    if deferred {
        HirTy::GcBuf(Box::new(t))
    } else {
        t
    }
}

/// A function with `ref` parameters returns `(ret, r1, ..)` (no unit `ret`;
/// one field unwrapped). A throwing function's whole carrier is `ret`, so
/// `ref` values come back on the error path too (spec 4.7) while the
/// payload overlays ok and error (8.6.5).
pub(super) fn ref_packed(ret: &HirTy, refs: &[HirTy]) -> HirTy {
    if refs.is_empty() {
        return ret.clone();
    }
    let mut fields: Vec<HirTy> = Vec::new();
    if *ret != HirTy::Unit {
        fields.push(ret.clone());
    }
    fields.extend(refs.iter().cloned());
    if fields.len() == 1 {
        fields.pop().expect("one field")
    } else {
        HirTy::Tuple(fields)
    }
}

/// `v` packed with the current values of the `ref` parameters.
pub(super) fn ref_pack(ctx: &LowerCtx<'_>, v: HirExpr, span: rokc_diagnostic::Span) -> HirExpr {
    if ctx.ref_params.is_empty() {
        return v;
    }
    let refs: Vec<HirExpr> = ctx
        .ref_params
        .iter()
        .map(|(l, t)| {
            if ctx.ref_cells {
                hir_local(*l, t, span)
            } else {
                ctx.read_local(*l, span)
            }
        })
        .collect();
    let tys: Vec<HirTy> = refs.iter().map(|r| r.ty.clone()).collect();
    let ty = ref_packed(&v.ty, &tys);
    let mut fields = Vec::new();
    let mut stmts = Vec::new();
    if v.ty == HirTy::Unit {
        stmts.push(HirStmt::Expr(v));
    } else {
        fields.push(v);
    }
    fields.extend(refs);
    let value = if fields.len() == 1 {
        fields.pop().expect("one field")
    } else {
        HirExpr {
            kind: HirExprKind::Tuple { fields },
            ty: ty.clone(),
            span,
        }
    };
    if stmts.is_empty() {
        return value;
    }
    HirExpr {
        ty,
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: Some(value),
            label: None,
        })),
        span,
    }
}

/// Call a function with `ref` parameters: write each final `ref` value back
/// into its argument place (`places` by parameter index: a local, a field
/// chain, a `[ref]` cell) and yield the value. A throwing callee's values
/// are written back before its error moves on, as with real references.
pub(super) fn ref_unpack(
    ctx: &mut LowerCtx<'_>,
    call: HirExpr,
    out: &RefOut,
    places: &[Option<HirExpr>],
) -> HirExpr {
    let span = call.span;
    let call_ty = call.ty.clone();
    let tmp = ctx.declare(None, call_ty.clone());
    let mut stmts = vec![HirStmt::Let {
        local: tmp,
        init: call,
    }];
    let whole = hir_local(tmp, &call_ty, span);
    let lead = usize::from(out.ret != HirTy::Unit);
    let n = lead + out.refs.len();
    let field = |i: usize| -> HirExpr {
        if n == 1 {
            return whole.clone();
        }
        let ty = match &call_ty {
            HirTy::Tuple(ts) => ts[i].clone(),
            t => t.clone(),
        };
        HirExpr {
            kind: HirExprKind::TupleField {
                target: Box::new(whole.clone()),
                index: u32::try_from(i).expect("field index overflow"),
                field_ty: ty.clone(),
            },
            ty,
            span,
        }
    };
    let mut later = Vec::new();
    for (k, (pi, ty)) in out.refs.iter().enumerate() {
        let place = places.get(*pi).and_then(Option::as_ref);
        let value = match ty {
            // The cell the future writes: read when it completes.
            HirTy::GcBuf(elem) if out.deferred => HirExpr {
                kind: HirExprKind::BufGet {
                    buf: Box::new(field(lead + k)),
                    index: Box::new(hir_int(0, PrimTy::Usize, span)),
                    elem: (**elem).clone(),
                },
                ty: (**elem).clone(),
                span,
            },
            _ => field(lead + k),
        };
        if let Some(st) = place.and_then(|p| assign_place(ctx, p, value)) {
            if out.deferred {
                later.push((place.and_then(place_root), st));
            } else {
                stmts.push(st);
            }
        }
    }
    if !later.is_empty() {
        if ctx.awaited == Some(span) {
            ctx.awaited = None;
            ctx.await_write_back = later.into_iter().map(|(_, st)| st).collect();
        } else if let Some(l) = later.iter().filter_map(|(l, _)| *l).find(|l| {
            ctx.locals[l.0 as usize]
                .name
                .as_deref()
                .is_some_and(|n| !n.starts_with('#'))
        }) {
            // The future would hold the place until it completes, with
            // nothing to write it back then.
            let name = ctx.locals[l.0 as usize].name.clone().unwrap_or_default();
            let text = ctx.src(span).filter(|t| t.len() <= 60 && !t.contains('\n'));
            ctx.emit_error_at(
                rokc_diagnostic::ErrorKind::Handoff,
                span,
                format!(
                    "this `async` call takes `{name}` as `ref self`: its future holds `{name}` \
                     until it completes, so it must be awaited where it is made"
                ),
            );
            ctx.help_last(Some(match text {
                Some(t) => format!("write `await {t}`; `{name}` gets the new value then"),
                None => {
                    format!("`await` the call where it is made; `{name}` gets the new value then")
                }
            }));
        }
    }
    let value = (lead == 1).then(|| field(0));
    // A carrier: now route a thrown error on.
    let (trailing, ty) = match value {
        Some(c) if matches!(out.ret, HirTy::Throws { .. }) => {
            let v = unpack_call(ctx, c);
            let ty = v.ty.clone();
            (Some(v), ty)
        }
        v => (v, out.ret.clone()),
    };
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing,
            label: None,
        })),
        ty,
        span,
    }
}

/// The local at the root of place `e` (a local, a field chain on one,
/// or a `[ref]` cell); `None`: not a place a `ref` argument can name.
pub(super) fn place_root(e: &HirExpr) -> Option<LocalId> {
    match &e.kind {
        HirExprKind::Local(l) => Some(*l),
        HirExprKind::FieldRead { target, .. } => place_root(target),
        HirExprKind::BufGet { buf, .. } => match buf.kind {
            HirExprKind::Local(l) => Some(l),
            _ => None,
        },
        _ => None,
    }
}

/// Whether [`assign_place`] can write `place` back.
pub(super) fn writable_place(ctx: &LowerCtx<'_>, place: &HirExpr) -> bool {
    match &place.kind {
        HirExprKind::Local(_) => true,
        HirExprKind::BufGet { buf, .. } => {
            matches!(buf.kind, HirExprKind::Local(l) if ctx.is_boxed(l))
        }
        HirExprKind::FieldRead { target, .. } => writable_place(ctx, target),
        _ => false,
    }
}

/// `place = value` for a local, a field chain on a local, or a cell. A
/// narrowed local writes through to the slot it narrows, as `=` does.
pub(super) fn assign_place(
    ctx: &mut LowerCtx<'_>,
    place: &HirExpr,
    value: HirExpr,
) -> Option<HirStmt> {
    match &place.kind {
        HirExprKind::Local(l) => {
            let span = value.span;
            let st = HirStmt::Assign { local: *l, value };
            super::expr::write_through(ctx, Some(st), span)
        }
        HirExprKind::BufGet { buf, .. } if matches!(buf.kind, HirExprKind::Local(l) if ctx.is_boxed(l)) => {
            Some(HirStmt::Expr(buf_store((**buf).clone(), value)))
        }
        HirExprKind::FieldRead {
            target,
            struct_id,
            field_index,
            ..
        } => {
            let span = value.span;
            let rebuilt = HirExpr {
                kind: HirExprKind::WithField {
                    target: target.clone(),
                    struct_id: *struct_id,
                    field_index: *field_index,
                    value: Box::new(value),
                },
                ty: target.ty.clone(),
                span,
            };
            assign_place(ctx, target, rebuilt)
        }
        _ => None,
    }
}

/// The inferred return type of `id` so far (`Some(None)`: being
/// inferred).
#[allow(clippy::option_option)] // the table's entry, if any
fn inferred_ret(env: &Env<'_>, id: FunctionId) -> Option<Option<HirTy>> {
    let r = env.sink.inferred_ret.borrow().get(&id).cloned();
    super::incr::log(
        || format!("r|{}", id.0),
        || super::incr::Lookup::InferredRet(id, r.clone()),
    );
    r
}

/// Names `body` binds exactly once and writes exactly once as a path
/// after: a value in one has no other handle in this function (nested
/// closures and blocks count too).
fn sole_names(body: &rokc_ast::Block) -> std::collections::HashSet<String> {
    #[derive(Default)]
    struct Count {
        binds: HashMap<String, u32>,
        paths: HashMap<String, u32>,
    }
    impl crate::walk::AstVisitor for Count {
        fn path(&mut self, p: &rokc_ast::Path) {
            for seg in &p.segments {
                *self.paths.entry(seg.name.clone()).or_default() += 1;
            }
        }
        fn bind(&mut self, name: &str) {
            *self.binds.entry(name.to_owned()).or_default() += 1;
        }
    }
    let mut c = Count::default();
    crate::walk::walk_block(&mut c, body);
    c.binds
        .into_iter()
        .filter(|(n, b)| *b == 1 && c.paths.get(n) == Some(&1))
        .map(|(n, _)| n)
        .collect()
}
