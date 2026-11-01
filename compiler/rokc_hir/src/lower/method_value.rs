//! Method path values (spec 3.5.3): `T::m`, `Tr::m`, `<T as Tr>::m`,
//! `i32::add` without a call are `fn` values (`fn(args) -> R` static,
//! `fn(Self, args) -> R` instance). The value is the capture-free
//! eta-expansion `|a, b| T::m(a, b)`, whose body is the ordinary path call
//! (intrinsics, trait dispatch, instantiation, `throws`).

use super::expr::{MethodPick, bind_explicit, pick_method, self_path_segs};
#[allow(clippy::wildcard_imports)]
use super::*;

/// What the value needs to know of the method before it is expanded.
struct Shape {
    /// Parameters, `self` included.
    arity: usize,
    /// A `ref` parameter or `ref self`.
    has_ref: bool,
    throws: bool,
    /// Parameter and return types when the path fixes them; else the
    /// expected `fn` type supplies them.
    ty: Option<(Vec<HirTy>, HirTy)>,
}

impl Shape {
    fn of_def(def: &FnDef) -> Self {
        Self {
            arity: def.params.len(),
            has_ref: def.params.iter().any(|p| p.by_ref),
            throws: !matches!(def.throws, rokc_ast::ThrowsKind::None),
            ty: None,
        }
    }
}

/// `path` as a method value, or `None` when it names no method (the
/// caller goes on to enum variants / unit structs / its error).
pub(super) fn method_path_value(
    ctx: &mut LowerCtx<'_>,
    callee: &ExprKind,
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    let found = match callee {
        ExprKind::Path(path) => shape(ctx, path, span),
        ExprKind::QPath(q) => qpath_shape(ctx, q, span),
        _ => None,
    };
    let shape = match found? {
        Ok(s) => s,
        Err(e) => return Some(e),
    };
    let display = match callee {
        ExprKind::Path(path) => self_path_segs(ctx, path).join("::"),
        ExprKind::QPath(q) => format!("<..>::{}", q.item),
        _ => String::new(),
    };
    if shape.has_ref {
        return Some(ctx.error_expr(
            span,
            format!(
                "`{display}` has a `ref` parameter (or `ref self`) and cannot be used as a `fn` \
                 value - a `fn` type has no `ref` parameters (spec 4.7.6)"
            ),
        ));
    }
    let expected = match ctx.current_expected() {
        Some(t @ (HirTy::Fn(args, ..) | HirTy::Closure { args, .. }))
            if args.len() == shape.arity =>
        {
            Some(t.clone())
        }
        _ => None,
    };
    let fn_ty = match (shape.ty, expected) {
        (Some((params, ret)), _) => HirTy::Fn(params, Box::new(ret), crate::FnBounds::default()),
        (None, Some(t)) => t,
        // An un-annotated `let`: the later calls fix the types (`infer`).
        (None, None) if ctx.infer_vars => HirTy::Fn(
            (0..shape.arity).map(|_| super::infer::fresh(ctx)).collect(),
            Box::new(HirTy::Unknown),
            crate::FnBounds::default(),
        ),
        (None, None) => {
            return Some(ctx.error_expr(
                span,
                format!(
                    "type annotations needed: the `fn` type of `{display}` as a value - annotate \
                     the binding (`let f: fn(..) -> R = {display};`), pass it to a `fn` \
                     parameter, or name the type arguments (spec 3.5.3)"
                ),
            ));
        }
    };
    // `|#m0, #m1| path(#m0, #m1)`, under `try` when the method throws.
    let names: Vec<String> = (0..shape.arity).map(|i| format!("#m{i}")).collect();
    let mut body = Expr {
        kind: ExprKind::Call {
            callee: Box::new(Expr {
                kind: callee.clone(),
                span,
            }),
            args: names
                .iter()
                .map(|n| CallArg::Positional(super::throws::path_expr(n, span)))
                .collect(),
            trailing_closure: None,
        },
        span,
    };
    if shape.throws {
        body = Expr {
            kind: ExprKind::Try {
                kind: rokc_ast::TryKind::Plain,
                operand: Box::new(body),
            },
            span,
        };
    }
    let closure = Expr {
        kind: ExprKind::Closure {
            captures: Vec::new(),
            params: names
                .into_iter()
                .map(|name| Param {
                    name,
                    ty: None,
                    by_ref: false,
                    default: None,
                    variadic: false,
                    span,
                })
                .collect(),
            ret_ty: None,
            body: Box::new(body),
            is_async: false,
            is_generator: false,
        },
        span,
    };
    Some(lower_expected(ctx, &closure, fn_ty))
}

/// The method `path` names: `None` if none, `Err` after an error.
fn shape(
    ctx: &mut LowerCtx<'_>,
    path: &AstPath,
    span: rokc_diagnostic::Span,
) -> Option<Result<Shape, HirExpr>> {
    let [ty_part @ .., last] = path.segments.as_slice() else {
        return None;
    };
    if ty_part.is_empty() {
        return None;
    }
    let method = last.name.as_str();
    let self_segs = self_path_segs(ctx, path);
    let ty_segs: Vec<&str> = self_segs[..self_segs.len() - 1]
        .iter()
        .map(String::as_str)
        .collect();
    let full = ctx.canon_type(&ty_segs);
    // `Tr::m`: `Self` comes from the expected `fn` type at the call.
    if let Some(key) = full.as_ref().filter(|f| ctx.env.names.is_trait(f)) {
        let decl = ctx.env.trait_of(key)?.decls.get(method)?;
        return Some(Ok(Shape::of_def(decl)));
    }
    let ty_ast = |segments: &[rokc_ast::PathSegment]| Ty {
        kind: AstTyKind::Path(AstPath {
            segments: segments.to_vec(),
            span,
            origin: None,
        }),
        span,
    };
    // `T::m` / `i32::m` / `Self::m`.
    if let [one] = ty_part {
        let ty = ty_ast(ty_part);
        let simple = one.name == "Self"
            || ctx.ty_subst.contains_key(&one.name)
            || matches!(resolve_ty(&ty), HirTy::Prim(_));
        if simple && full.as_ref().is_none_or(|f| ctx.lookup_struct(f).is_none()) {
            let t = ctx.resolve_ast_ty(&ty);
            return picked(ctx, &t, method, None, &last.generics, span);
        }
    }
    let full = full?;
    // `S::m`, `Box::<i32>::m`.
    if ctx.lookup_struct(&full).is_some()
        || ctx.lookup_enum(&full).is_some()
        || ty_part.last().is_some_and(|s| !s.generics.is_empty())
    {
        let t = ctx.resolve_ast_ty(&ty_ast(ty_part));
        return picked(ctx, &t, method, None, &last.generics, span);
    }
    // `Box::m` of a generic type: its instantiation comes from the
    // expected `fn` type at the call.
    let gm = ctx.env.generic_methods(&full, method)?.first()?;
    Some(Ok(Shape::of_def(&gm.def)))
}

/// The method `<T as Tr>::m` names.
fn qpath_shape(
    ctx: &mut LowerCtx<'_>,
    q: &rokc_ast::QPath,
    span: rokc_diagnostic::Span,
) -> Option<Result<Shape, HirExpr>> {
    let t = ctx.resolve_ast_ty(&q.self_ty);
    let segs: Vec<&str> = q
        .trait_path
        .segments
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let key = ctx
        .env
        .names
        .trait_key(&segs, &ctx.current_module, ctx.here());
    picked(ctx, &t, &q.item, Some(&key), &[], span)
}

/// `t`'s method `method` (of trait `key` when given).
fn picked(
    ctx: &mut LowerCtx<'_>,
    t: &HirTy,
    method: &str,
    key: Option<&str>,
    turbofish: &[Ty],
    span: rokc_diagnostic::Span,
) -> Option<Result<Shape, HirExpr>> {
    let pick = match pick_method(ctx, t, method, key) {
        Ok(p) => p?,
        Err(msg) => return Some(Err(ctx.error_expr(span, msg))),
    };
    let gm = match pick {
        MethodPick::Concrete(fid) => {
            let sig = ctx.sig_by_id(fid);
            return Some(Ok(Shape {
                arity: sig.param_tys.len(),
                has_ref: sig.param_refs.iter().any(|r| *r),
                throws: matches!(sig.ret_ty, HirTy::Throws { .. }),
                ty: Some((sig.param_tys, sig.ret_ty)),
            }));
        }
        MethodPick::Generic(gm) => gm,
    };
    let mut shape = Shape::of_def(&gm.def);
    // Instantiate it now when `Self` and the turbofish fix every type
    // parameter; else the expected `fn` type does at the call.
    if !shape.has_ref && !matches!(ctx.current_expected(), Some(HirTy::Fn(..))) {
        let Ok(bound) = bind_explicit(ctx, &gm.def, Some(&gm.self_ty), turbofish, method) else {
            return Some(Ok(shape));
        };
        ctx.explicit_generics = bound;
        let mono = monomorphize_on(ctx, &gm.def, Some(&gm.self_ty), Some(t), &[], span);
        ctx.explicit_generics.clear();
        let Some((id, ret)) = mono else {
            return Some(Err(poison(span)));
        };
        let params = ctx.env.sink.mono_sigs.borrow().get(&id)?.param_tys.clone();
        shape.ty = Some((params, ret));
    }
    Some(Ok(shape))
}
