//! Desugarings: closures, `?` / `??` / `?.`, and `for` loops.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Lift a closure into an anonymous function and return its value.
/// Captures copy at creation; `[ref x]` shares `x`'s cell (spec 4.6.2). A
/// non-capturing closure is a plain function pointer.
pub(super) fn lower_closure_expr(
    ctx: &mut LowerCtx<'_>,
    capture_list: &[rokc_ast::CaptureSpec],
    params: &[Param],
    ret_ty: Option<&Ty>,
    body: &Expr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    // Names referenced but not bound inside the closure (exhaustive AST
    // walk), filtered to those that resolve to outer locals.
    let by_ref: Vec<&str> = capture_list
        .iter()
        .filter_map(|c| match c {
            rokc_ast::CaptureSpec::Ref(n) => Some(n.as_str()),
            rokc_ast::CaptureSpec::Value(_) => None,
        })
        .collect();
    // Inside a narrowing of `x`, `[ref x]` shares the cell itself.
    let cell_of = |l: LocalId| {
        let mut l = l;
        while let (false, Some(&(orig, _))) = (ctx.is_boxed(l), ctx.narrowed.get(&l)) {
            l = orig;
        }
        l
    };
    for n in &by_ref {
        match ctx.lookup(n).map(cell_of) {
            Some(l) if ctx.is_boxed(l) => {}
            Some(_) => ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "`[ref {n}]` needs `{n}` to be a `let` variable or a parameter (spec 4.6.2)"
                ),
            ),
            None => ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!("`[ref {n}]` names no variable in scope (spec 4.6.2)"),
            ),
        }
    }
    let captures: Vec<Capture> = crate::walk::free_names(params, body)
        .into_iter()
        .filter_map(|name| {
            let mut local = ctx.lookup(&name)?;
            let by_ref = by_ref.contains(&name.as_str()) && ctx.is_boxed(cell_of(local));
            if by_ref {
                local = cell_of(local);
            }
            let ty = if by_ref {
                ctx.local_ty(local)
            } else {
                ctx.var_ty(local)
            };
            Some((name, local, ty, by_ref))
        })
        .collect();
    for p in params.iter().filter(|p| p.by_ref) {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "closure parameter `{}` cannot be `ref` - a `fn` type has no `ref` \
                 parameters (spec 4.7.6)",
                p.name
            ),
        );
    }

    let closure_id = FunctionId(ctx.env.sink.next_id.get());
    ctx.env.sink.next_id.set(closure_id.0 + 1);

    // Spec 4.5 / 4.8: with an expected function type (or a sum holding
    // exactly one), unannotated parameters and the return come from it.
    let expected = ctx.current_expected().and_then(|t| match t {
        HirTy::Sum(vs) => {
            let mut fns = vs
                .iter()
                .filter(|v| matches!(v, HirTy::Fn(..) | HirTy::Closure { .. }));
            match (fns.next(), fns.next()) {
                (Some(f), None) => Some(f),
                _ => None,
            }
        }
        t => Some(t),
    });
    let (expected_param_tys, mut expected_ret): (Option<Vec<HirTy>>, Option<HirTy>) = match expected
    {
        Some(HirTy::Fn(args, ret, _) | HirTy::Closure { args, ret, .. }) => {
            (Some(args.clone()), Some((**ret).clone()))
        }
        _ => (None, None),
    };
    // An expected `fn(..) throws (E) -> T`: the body's value is a `T`; an
    // unknown `E` (a `rethrows` parameter) is inferred like no `E`.
    let mut expected_err = None;
    if let Some(HirTy::Throws { ok, err }) = expected_ret.clone() {
        expected_ret = Some(*ok);
        expected_err = Some(*err).filter(|e| *e != HirTy::Unknown);
    }
    let declared_ret = ret_ty.map(|t| ctx.resolve_ast_ty(t));
    // `return` inside the closure targets the closure: its declared (or
    // expected) return type, else the first `return` value's type.
    let mut fixed_ret = declared_ret.or_else(|| expected_ret.filter(|t| *t != HirTy::Unknown));

    // A throwing closure needs its error and value types before the body
    // (they shape its error returns): the expected ones, else a probe
    // lowering.
    let mut throws = expected_err;
    if (throws.is_some() || may_throw(body)) && (throws.is_none() || fixed_ret.is_none()) {
        let mark = ctx.probe_mark();
        let (mut probe, ..) =
            closure_ctx(ctx, params, &captures, expected_param_tys.as_ref(), span);
        probe.fn_ret_ty = fixed_ret.clone().unwrap_or(HirTy::Unknown);
        probe.infer_ret = fixed_ret.is_none();
        probe.throws_probe = Some(Vec::new());
        let hint = probe.fn_ret_ty.clone();
        super::expr::guard_sink(&mut probe, body);
        let value = lower_expected(&mut probe, body, hint);
        ctx.rollback(&mark);
        let errs = probe.throws_probe.take().unwrap_or_default();
        if throws.is_none() {
            throws = match crate::ty::throws_ty(HirTy::Unit, HirTy::Sum(errs)) {
                HirTy::Throws { err, .. } => Some(*err),
                _ => None,
            };
        }
        if throws.is_some() && fixed_ret.is_none() {
            fixed_ret = Some(match probe.fn_ret_ty {
                HirTy::Unknown => value.ty,
                t => t,
            });
        }
    }

    let (mut cl_ctx, hir_params, prelude, user_arg_tys) =
        closure_ctx(ctx, params, &captures, expected_param_tys.as_ref(), span);
    cl_ctx.fn_ret_ty = fixed_ret.clone().unwrap_or(HirTy::Unknown);
    cl_ctx.infer_ret = fixed_ret.is_none();
    cl_ctx.fn_throws.clone_from(&throws);
    let ret_hint = cl_ctx.fn_ret_ty.clone();
    super::expr::guard_sink(&mut cl_ctx, body);
    let body_h = lower_expected(&mut cl_ctx, body, ret_hint);
    let diverges = hir_expr_diverges(&body_h);
    let final_ret_ty = match (fixed_ret, cl_ctx.fn_ret_ty.clone()) {
        // A `return` fixed the type; a value-producing body must agree.
        (None, HirTy::Unknown) => body_h.ty.clone(),
        (Some(t), _) | (None, t) => t,
    };
    let body_h = if diverges || (body_h.ty == HirTy::Unit && final_ret_ty == HirTy::Unit) {
        body_h
    } else {
        coerce(&mut cl_ctx, &final_ret_ty, body_h, &|| {
            "the closure body".to_owned()
        })
    };

    let mut body_block = HirBlock {
        stmts: prelude,
        trailing: Some(body_h),
        label: None,
    };
    cl_ctx.fn_ret_ty = final_ret_ty.clone();
    finish_throwing_body(&cl_ctx, &mut body_block);
    let final_ret_ty = match throws {
        Some(err) => HirTy::Throws {
            ok: Box::new(final_ret_ty),
            err: Box::new(err),
        },
        None => final_ret_ty,
    };
    let cl_fn = HirFunction {
        id: closure_id,
        def_id: None,
        name: format!("__closure_{}", closure_id.0),
        params: hir_params,
        locals: cl_ctx.into_locals(),
        ret_ty: final_ret_ty.clone(),
        body: Some(body_block),
        is_async: false,
        is_generator: false,
        span,
        facts: rokc_ir_common::FnFacts::default(),
    };
    ctx.env.sink.extras.borrow_mut().push(cl_fn);

    let env_ty = (!captures.is_empty())
        .then(|| HirTy::Tuple(captures.iter().map(|(_, _, t, _)| t.clone()).collect()));
    let Some(env_ty_v) = env_ty else {
        return HirExpr {
            kind: HirExprKind::FnRef(closure_id),
            // No captures: Send + Sync like a named function.
            ty: HirTy::Fn(
                user_arg_tys,
                Box::new(final_ret_ty),
                ctx.env.names.all_auto(),
            ),
            span,
        };
    };
    // The env: captured values snapshotted at the creation site.
    let env_value = HirExpr {
        kind: HirExprKind::Tuple {
            fields: captures
                .iter()
                .map(|(_, outer_local, ty, by_ref)| {
                    if *by_ref {
                        hir_local(*outer_local, ty, span)
                    } else {
                        ctx.read_local(*outer_local, span)
                    }
                })
                .collect(),
        },
        ty: env_ty_v.clone(),
        span,
    };
    HirExpr {
        kind: HirExprKind::ClosureValue {
            fn_id: closure_id,
            env: Box::new(env_value),
            arg_tys: user_arg_tys.clone(),
            ret_ty: final_ret_ty.clone(),
            env_ty: env_ty_v.clone(),
        },
        ty: HirTy::Closure {
            args: user_arg_tys,
            ret: Box::new(final_ret_ty),
            env: Box::new(env_ty_v),
            shares: captures.iter().any(|c| c.3),
        },
        span,
    }
}

/// A closure capture: name, outer local, env field type (the cell for a
/// `[ref]` capture) and whether it is by reference.
type Capture = (String, LocalId, HirTy, bool);

/// Does a closure body `throw` or plain `try` (so its error type must be
/// inferred)? Nested closures count: an over-approximation costing a
/// probe.
pub(super) fn may_throw(body: &Expr) -> bool {
    struct V(bool);
    impl crate::walk::AstVisitor for V {
        fn expr(&mut self, e: &Expr) {
            if let ExprKind::Try {
                kind: rokc_ast::TryKind::Plain,
                ..
            } = e.kind
            {
                self.0 = true;
            }
        }
        fn stmt(&mut self, s: &Stmt) {
            if matches!(s.kind, StmtKind::Throw(_)) {
                self.0 = true;
            }
        }
    }
    let mut v = V(false);
    crate::walk::walk_expr(&mut v, body);
    v.0
}

/// A closure body's context: the `env` tuple unpacked into one local per
/// capture, then the parameters. Returns (context, parameters, capture
/// prelude, visible argument types).
pub(super) fn closure_ctx<'a>(
    ctx: &LowerCtx<'a>,
    params: &[Param],
    captures: &[Capture],
    expected_param_tys: Option<&Vec<HirTy>>,
    span: rokc_diagnostic::Span,
) -> (LowerCtx<'a>, Vec<HirParam>, Vec<HirStmt>, Vec<HirTy>) {
    let mut cl_ctx = LowerCtx::with_subst(
        ctx.env,
        ctx.ty_subst.clone(),
        ctx.self_ty.clone(),
        ctx.here(),
    );
    cl_ctx.current_module.clone_from(&ctx.current_module);
    cl_ctx.local_uses.clone_from(&ctx.local_uses);
    cl_ctx.nested_fns.clone_from(&ctx.nested_fns);
    cl_ctx.ref_captured.clone_from(&ctx.ref_captured);
    cl_ctx.sole_names.clone_from(&ctx.sole_names);
    cl_ctx.ty_bounds.clone_from(&ctx.ty_bounds);
    cl_ctx.ty_bound_args.clone_from(&ctx.ty_bound_args);
    cl_ctx.assoc_names.clone_from(&ctx.assoc_names);
    cl_ctx.unsafe_depth = ctx.unsafe_depth;
    cl_ctx.infer_vars = ctx.infer_vars;

    // `env` is declared first so its LocalId is stable.
    let env_ty = (!captures.is_empty())
        .then(|| HirTy::Tuple(captures.iter().map(|(_, _, t, _)| t.clone()).collect()));
    let mut hir_params: Vec<HirParam> = Vec::with_capacity(params.len() + 1);
    let mut prelude: Vec<HirStmt> = Vec::new();
    if let Some(et) = &env_ty {
        let env_local = cl_ctx.declare(Some("__env".to_owned()), et.clone());
        hir_params.push(HirParam {
            name: "__env".to_owned(),
            local: env_local,
            ty: et.clone(),
            by_ref: false,
        });
        for (i, (cap_name, _, cap_ty, by_ref)) in captures.iter().enumerate() {
            let inner_local = cl_ctx.declare(Some(cap_name.clone()), cap_ty.clone());
            if *by_ref {
                cl_ctx.locals[inner_local.0 as usize].boxed = true;
            } else {
                cl_ctx.value_captured.insert(inner_local);
            }
            prelude.push(HirStmt::Let {
                local: inner_local,
                init: HirExpr {
                    kind: HirExprKind::TupleField {
                        target: Box::new(HirExpr {
                            kind: HirExprKind::Local(env_local),
                            ty: et.clone(),
                            span,
                        }),
                        index: u32::try_from(i).expect("capture index overflow"),
                        field_ty: cap_ty.clone(),
                    },
                    ty: cap_ty.clone(),
                    span,
                },
            });
        }
    }

    let mut user_arg_tys: Vec<HirTy> = Vec::with_capacity(params.len());
    let mut cell_prelude: Vec<HirStmt> = Vec::new();
    for (pi, p) in params.iter().enumerate() {
        let ty = match &p.ty {
            Some(t) => ctx.resolve_ast_ty(t),
            None => match expected_param_tys.as_ref().and_then(|v| v.get(pi)) {
                Some(t) if *t != HirTy::Unknown => t.clone(),
                _ if ctx.infer_vars => super::infer::fresh(ctx),
                _ => {
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!(
                            "cannot infer the type of closure parameter `{}` - annotate \
                             it (`|{}: i32| ...`)",
                            p.name, p.name
                        ),
                    );
                    ctx.tag_last(rokc_diagnostic::Tag::ClosureParamInfer);
                    HirTy::Unknown
                }
            },
        };
        let local = cl_ctx.declare(Some(p.name.clone()), ty.clone());
        if cl_ctx.ref_captured.contains(&(p.name.clone(), p.span)) {
            let init = hir_local(local, &ty, span);
            cell_prelude.push(cl_ctx.cell_let(&p.name, ty.clone(), init));
        }
        hir_params.push(HirParam {
            name: p.name.clone(),
            local,
            ty: ty.clone(),
            by_ref: p.by_ref,
        });
        user_arg_tys.push(ty);
    }
    prelude.extend(cell_prelude);
    (cl_ctx, hir_params, prelude, user_arg_tys)
}

/// `T | none`-shaped sum: index of `none` plus the non-none variants.
pub(super) fn optional_parts(ty: &HirTy) -> Option<(usize, Vec<(usize, HirTy)>)> {
    let HirTy::Sum(variants) = ty else {
        return None;
    };
    let none_idx = variants.iter().position(HirTy::is_none)?;
    let some: Vec<(usize, HirTy)> = variants
        .iter()
        .enumerate()
        .filter(|(_, v)| !v.is_none())
        .map(|(i, v)| (i, v.clone()))
        .collect();
    Some((none_idx, some))
}

/// The value type left after removing `none`: the single payload, or a
/// sum of the remaining variants.
pub(super) fn without_none(some: &[(usize, HirTy)]) -> HirTy {
    match some {
        [(_, t)] => t.clone(),
        many => HirTy::sum(many.iter().map(|(_, t)| t.clone())),
    }
}

/// `match scrut { v: T_k => wrap(v), ..., none => none_body }` with one
/// arm per non-none variant; each payload is re-wrapped into `result_ty`.
#[allow(clippy::too_many_arguments)] // match pieces are independent inputs; a bundle struct only adds noise
pub(super) fn optional_match(
    ctx: &mut LowerCtx<'_>,
    scrut: HirExpr,
    none_idx: usize,
    some: &[(usize, HirTy)],
    some_body: &mut dyn FnMut(&mut LowerCtx<'_>, HirExpr) -> HirExpr,
    none_body: HirExpr,
    result_ty: HirTy,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let sum_ty = scrut.ty.clone();
    let mut arms = Vec::with_capacity(some.len() + 1);
    for (idx, payload_ty) in some {
        let bind_local = ctx.declare(None, payload_ty.clone());
        let value = HirExpr {
            kind: HirExprKind::Local(bind_local),
            ty: payload_ty.clone(),
            span,
        };
        arms.push(HirMatchArm {
            pat: HirPattern::SumVariant {
                sum_ty: sum_ty.clone(),
                variant_index: u32::try_from(*idx).expect("variant overflow"),
                binder: Some(bind_local),
                payload_ty: Some(payload_ty.clone()),
            },
            guard: None,
            body: some_body(ctx, value),
        });
    }
    arms.push(HirMatchArm {
        pat: HirPattern::SumVariant {
            sum_ty,
            variant_index: u32::try_from(none_idx).expect("variant overflow"),
            binder: None,
            payload_ty: None,
        },
        guard: None,
        body: none_body,
    });
    HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(scrut),
            arms,
        },
        ty: result_ty,
        span,
    }
}

pub(super) fn sum_none(sum_ty: &HirTy, span: rokc_diagnostic::Span) -> HirExpr {
    wrap_into_sum(
        sum_ty,
        HirExpr {
            kind: HirExprKind::Const(ConstVal::None),
            ty: HirTy::None,
            span,
        },
    )
}

/// `x?.tail` → `match x { v: T => v.tail, none => none }`, typed
/// `tail_ty | none` (`?.` chains compose, spec 3.8.2).
pub(super) fn desugar_question_dot(
    ctx: &mut LowerCtx<'_>,
    recv: HirExpr,
    tails: &[rokc_ast::AccessTail],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if recv.ty == HirTy::Unknown {
        return recv;
    }
    let Some((none_idx, some)) = optional_parts(&recv.ty) else {
        // Spec 3.8: `?.` on a non-optional receiver is an error.
        return ctx.error_expr(
            span,
            format!(
                "`?.` needs an optional (`T | none`) receiver, found `{}` - it is not \
                 optional, so use `.`",
                ctx.ty_name(&recv.ty)
            ),
        );
    };
    if some.len() != 1 {
        return ctx.error_expr(
            span,
            format!(
                "`?.` needs a `T | none` receiver with a single payload type, found `{}`",
                ctx.ty_name(&recv.ty)
            ),
        );
    }
    // Lower the tail once against a placeholder local to learn its type.
    let payload_ty = some[0].1.clone();
    let bind_local = ctx.declare(None, payload_ty.clone());
    let unwrapped = HirExpr {
        kind: HirExprKind::Local(bind_local),
        ty: payload_ty.clone(),
        span,
    };
    let mut inner = unwrapped;
    for tail in tails {
        inner = apply_access_tail(ctx, inner, tail, span);
        if inner.ty == HirTy::Unknown {
            return inner;
        }
    }
    let inner_optional = optional_parts(&inner.ty).is_some();
    let result_ty = if inner_optional {
        inner.ty.clone()
    } else {
        HirTy::sum([inner.ty.clone(), HirTy::None])
    };
    let some_body = if inner_optional {
        inner
    } else {
        wrap_into_sum(&result_ty, inner)
    };
    let none_body = sum_none(&result_ty, span);
    HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(recv.clone()),
            arms: vec![
                HirMatchArm {
                    pat: HirPattern::SumVariant {
                        sum_ty: recv.ty.clone(),
                        variant_index: u32::try_from(some[0].0).expect("variant overflow"),
                        binder: Some(bind_local),
                        payload_ty: Some(payload_ty),
                    },
                    guard: None,
                    body: some_body,
                },
                HirMatchArm {
                    pat: HirPattern::SumVariant {
                        sum_ty: recv.ty,
                        variant_index: u32::try_from(none_idx).expect("variant overflow"),
                        binder: None,
                        payload_ty: None,
                    },
                    guard: None,
                    body: none_body,
                },
            ],
        },
        ty: result_ty,
        span,
    }
}

/// Apply an `AccessTail` to an already-lowered HIR receiver. Supports
/// the same forms `?.` does: field, tuple field, and method call.
pub(super) fn apply_access_tail(
    ctx: &mut LowerCtx<'_>,
    recv: HirExpr,
    tail: &rokc_ast::AccessTail,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    match tail {
        rokc_ast::AccessTail::Field(name) => field_access(ctx, recv, name, span),
        rokc_ast::AccessTail::TupleField(idx) => match recv.ty.clone() {
            HirTy::Tuple(parts) => match parts.get(*idx as usize) {
                Some(field_ty) => HirExpr {
                    kind: HirExprKind::TupleField {
                        target: Box::new(recv),
                        index: *idx,
                        field_ty: field_ty.clone(),
                    },
                    ty: field_ty.clone(),
                    span,
                },
                None => ctx.error_expr(
                    span,
                    format!("tuple index {idx} out of range for a {}-tuple", parts.len()),
                ),
            },
            other => ctx.error_expr(
                span,
                format!("no field `{idx}` on type `{}`", ctx.ty_name(&other)),
            ),
        },
        rokc_ast::AccessTail::Method { method, args, .. } => {
            let call = method_call_on(ctx, recv, method, args, span);
            unpack_call(ctx, call)
        }
        rokc_ast::AccessTail::Index(i) => {
            let target = super::expr::splice(ctx, recv);
            let e = rokc_ast::Expr {
                kind: ExprKind::Index(Box::new(target), i.clone()),
                span,
            };
            lower_expr(ctx, &e)
        }
    }
}

/// `expr?` (spec 3.14): unwrap `T | none` or return `none` from the
/// enclosing function (whose return must hold `none`). Several payload
/// members keep the rest of the sum.
pub(super) fn desugar_try_q(
    ctx: &mut LowerCtx<'_>,
    inner: HirExpr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if inner.ty == HirTy::Unknown {
        return inner;
    }
    let Some((none_idx, some)) = optional_parts(&inner.ty) else {
        return ctx.error_expr(
            span,
            format!(
                "`?` only works on `T | none`, found `{}` - to propagate errors use `throws` \
                 and `try` (spec 3.14 / 4.14)",
                ctx.ty_name(&inner.ty)
            ),
        );
    };
    let ret_ty = ctx.fn_ret_ty.clone();
    if optional_parts(&ret_ty).is_none() {
        return ctx.error_expr(
            span,
            format!(
                "`?` propagates `none`, but the enclosing function returns `{}` - its \
                 return type must include `none`",
                ctx.ty_name(&ret_ty)
            ),
        );
    }
    let value_ty = without_none(&some);
    let ret = lower_return(ctx, Some(sum_none(&ret_ty, span)), span);
    let none_body = HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![ret],
            trailing: None,
            label: None,
        })),
        ty: value_ty.clone(),
        span,
    };
    let vt = value_ty.clone();
    optional_match(
        ctx,
        inner,
        none_idx,
        &some,
        &mut |_, v| wrap_into_sum(&vt, v),
        none_body,
        value_ty,
        span,
    )
}

/// Desugar `lhs ?? rhs` to `match lhs { v: T => v, none => rhs }`; the
/// default has the payload type, or both go into the receiving slot.
pub(super) fn desugar_nullish_coalesce(
    ctx: &mut LowerCtx<'_>,
    lhs: HirExpr,
    rhs: &Expr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let Some((none_idx, some)) = optional_parts(&lhs.ty) else {
        let rhs_h = lower_expected(ctx, rhs, HirTy::Unknown);
        if lhs.ty == HirTy::Unknown || rhs_h.ty == HirTy::Unknown {
            return poison(span);
        }
        return ctx.error_expr(
            span,
            format!(
                "`??` needs an optional left operand (`T | none`), found `{}`",
                ctx.ty_name(&lhs.ty)
            ),
        );
    };
    let mut value_ty = without_none(&some);
    let rhs_h = lower_expected(ctx, rhs, value_ty.clone());
    // `a ?? b` with `b: T | none` too is a fallback chain typed `T | none`
    // (spec 3.8.4). Only that case; other sums still need `T`.
    if rhs_h.ty == lhs.ty {
        value_ty = lhs.ty.clone();
    } else if rhs_h.ty != value_ty && rhs_h.ty != HirTy::Unknown {
        // Branches of different types take the receiving slot's type,
        // never a widened sum (spec 3.8.4: `let r: i32 | str = a ?? "d"`).
        if let Some(slot) = ctx
            .current_expected()
            .filter(|t| matches!(t, HirTy::Sum(_)))
        {
            value_ty = slot.clone();
        }
    }
    let rhs_h = coerce(ctx, &value_ty, rhs_h, &|| "the `??` default".to_owned());
    let vt = value_ty.clone();
    optional_match(
        ctx,
        lhs,
        none_idx,
        &some,
        &mut |_, v| wrap_into_sum(&vt, v),
        rhs_h,
        value_ty,
        span,
    )
}

/// Bind an irrefutable `for` / destructuring pattern to `value`:
/// a name, `_`, `name @ pat`, or (nested) tuples / structs of those.
pub(super) fn bind_irrefutable(
    ctx: &mut LowerCtx<'_>,
    pat: &AstPattern,
    value: HirExpr,
    stmts: &mut Vec<HirStmt>,
) {
    let span = value.span;
    match (&pat.kind, value.ty.clone()) {
        (AstPatternKind::Binding { name, sub: None }, ty) => {
            if name == "_" {
                return;
            }
            stmts.push(ctx.declare_var(name, ty, value, pat.span));
        }
        // `whole @ (a, b)`: bind the whole value, then destructure it.
        (
            AstPatternKind::Binding {
                name,
                sub: Some(sub),
            },
            ty,
        ) => {
            stmts.push(ctx.declare_var(name, ty, value, pat.span));
            if let Some(local) = ctx.lookup(name) {
                let read = ctx.read_local(local, span);
                bind_irrefutable(ctx, sub, read, stmts);
            }
        }
        (AstPatternKind::Tuple(pats), HirTy::Tuple(tys)) if pats.len() == tys.len() => {
            let tmp = ctx.declare(None, value.ty.clone());
            let tuple_ty = value.ty.clone();
            stmts.push(HirStmt::Let {
                local: tmp,
                init: value,
            });
            for (i, (p, t)) in pats.iter().zip(tys).enumerate() {
                let read = HirExpr {
                    kind: HirExprKind::TupleField {
                        target: Box::new(hir_local(tmp, &tuple_ty, span)),
                        index: u32::try_from(i).expect("tuple index overflow"),
                        field_ty: t.clone(),
                    },
                    ty: t.clone(),
                    span,
                };
                bind_irrefutable(ctx, p, read, stmts);
            }
        }
        // `Point { x, y: py, .. }` / tuple struct `Pair(a, b)`.
        (
            AstPatternKind::Struct { path, .. } | AstPatternKind::EnumVariant { path, .. },
            HirTy::Struct(sid),
        ) if ctx
            .canon_type(
                &path
                    .segments
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .as_deref()
            == Some(enum_base_name(&ctx.struct_def(sid).name)) =>
        {
            let parts: Vec<(String, Option<&AstPattern>)> = match &pat.kind {
                AstPatternKind::Struct { fields, .. } => fields
                    .iter()
                    .map(|f| (f.name.clone(), f.pat.as_ref()))
                    .collect(),
                AstPatternKind::EnumVariant { args, .. } => args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (i.to_string(), Some(a)))
                    .collect(),
                _ => Vec::new(),
            };
            let tmp = ctx.declare(None, value.ty.clone());
            let struct_ty = value.ty.clone();
            stmts.push(HirStmt::Let {
                local: tmp,
                init: value,
            });
            let def = ctx.struct_def(sid);
            for (fname, fpat) in parts {
                let Some(idx) = def.fields.iter().position(|f| f.name == fname) else {
                    ctx.no_field(sid, &fname, span);
                    continue;
                };
                if !fpat.is_some_and(|p| matches!(p.kind, AstPatternKind::Wildcard)) {
                    ctx.check_field_visible(sid, &fname);
                }
                let field_ty = def.fields[idx].ty.clone();
                let read = HirExpr {
                    ty: field_ty.clone(),
                    span,
                    kind: HirExprKind::FieldRead {
                        target: Box::new(hir_local(tmp, &struct_ty, span)),
                        struct_id: sid,
                        field_index: u32::try_from(idx).expect("field idx overflow"),
                        field_ty: field_ty.clone(),
                    },
                };
                match fpat {
                    Some(p) => bind_irrefutable(ctx, p, read, stmts),
                    None => stmts.push(ctx.declare_var(&fname, field_ty, read, pat.span)),
                }
            }
        }
        (AstPatternKind::Wildcard, _) | (_, HirTy::Unknown) => {}
        // `let (a: i32, b: str) = ..`: an annotated element is a slot of
        // that type, not a runtime test.
        (AstPatternKind::TypeCheck { binder, ty }, _) => {
            let want = ctx.resolve_ast_ty(ty);
            let name = binder.clone().unwrap_or_else(|| "_".to_owned());
            let value = coerce(ctx, &want, value, &|| format!("the pattern `{name}`"));
            if binder.is_some() {
                stmts.push(ctx.declare_var(&name, want, value, pat.span));
            }
        }
        (_, ty) => {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "this pattern can't destructure a value of type `{}` here - it must be \
                     irrefutable: a name, `_`, `name @ pat`, or a tuple / struct of those",
                    ctx.ty_name(&ty)
                ),
            );
        }
    }
}

/// `for pat in iter` (spec 3.9) over an integer range (bounds evaluated
/// once) or an array: `{ setup; while cond { step; let pat = ..; body } }`,
/// stepping first so `continue` can't skip it; `label` goes on the
/// `While`.
pub(super) fn lower_for(
    ctx: &mut LowerCtx<'_>,
    label: Option<&String>,
    pat: &AstPattern,
    iter: &Expr,
    body: &Block,
    is_await: bool,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let mut out: Vec<HirStmt> = Vec::new();
    ctx.push_scope();
    ctx.breakables.push((label.cloned(), true));
    let while_expr = match &iter.kind {
        _ if is_await => {
            let s = lower_expected(ctx, iter, HirTy::Unknown);
            Some(lower_for_stream(ctx, label, pat, s, body, span, &mut out))
        }
        ExprKind::Range {
            start: Some(start_e),
            end,
            inclusive,
        } if end.is_some() || !*inclusive => lower_for_range(
            ctx,
            label,
            pat,
            iter,
            start_e,
            end.as_deref(),
            *inclusive,
            body,
            span,
            &mut out,
        ),
        ExprKind::Range { .. } => Some(ctx.error_expr(
            span,
            "`for` over an unbounded range needs both bounds (`a..b`)".to_owned(),
        )),
        _ => {
            let iter_h = lower_expected(ctx, iter, HirTy::Unknown);
            match iter_h.ty.clone() {
                HirTy::Array(elem, len) => Some(lower_for_array(
                    ctx,
                    label,
                    pat,
                    iter_h,
                    (*elem).clone(),
                    len,
                    body,
                    span,
                    &mut out,
                )),
                HirTy::Unknown => None,
                _ => {
                    // A generator the loop made (not one in a variable,
                    // which may go on after the loop) is closed when the
                    // loop leaves early.
                    let owned = !matches!(
                        iter.kind,
                        ExprKind::Path(_)
                            | ExprKind::QPath(_)
                            | ExprKind::SelfLower
                            | ExprKind::Field(..)
                            | ExprKind::TupleField(..)
                            | ExprKind::Index(..)
                    );
                    let w = lower_for_iter(ctx, label, pat, iter_h, body, span, &mut out, owned);
                    Some(w)
                }
            }
        }
    };
    ctx.breakables.pop();
    ctx.pop_scope();
    match while_expr {
        Some(w) => out.push(HirStmt::Expr(w)),
        None => return poison(span),
    }
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: out,
            trailing: None,
            label: None,
        })),
        ty: HirTy::Unit,
        span,
    }
}

/// `for pat in xs` otherwise (stdlib 2.7.2): `let it = xs.into_iter();
/// loop { match it.next() { v: T => { let pat = v; body } none => break } }`.
/// A generator the loop owns (`owned`) is closed after the loop and on
/// every exit leaving it (spec 4.11.10): a `finally { it.close() }` that
/// `continue` and `break` of this loop skip.
#[allow(clippy::too_many_arguments)]
pub(super) fn lower_for_iter(
    ctx: &mut LowerCtx<'_>,
    label: Option<&String>,
    pat: &AstPattern,
    iter_h: HirExpr,
    body: &Block,
    span: rokc_diagnostic::Span,
    out: &mut Vec<HirStmt>,
    owned: bool,
) -> HirExpr {
    let has_into_iter = !matches!(
        super::expr::pick_method(ctx, &iter_h.ty, "into_iter", None),
        Ok(None)
    );
    // A throwing iterator (`TryGenIter`) is stepped directly; its `next`
    // must sit under a `try` covering the loop (`try for x in g { .. }`).
    let has_next = matches!(
        super::expr::pick_method(ctx, &iter_h.ty, "next", None),
        Ok(Some(_))
    );
    if !has_into_iter && !has_next {
        return ctx.error_expr(
            span,
            format!(
                "cannot iterate over a value of type `{}` - `for` needs an integer range, \
                 an array, or an `IntoIterator` / `Iterator` impl (stdlib 2.7.2)",
                ctx.ty_name(&iter_h.ty)
            ),
        );
    }
    let it_h = if has_into_iter {
        method_call_on(ctx, iter_h, "into_iter", &[], span)
    } else {
        iter_h
    };
    if it_h.ty == HirTy::Unknown {
        return it_h;
    }
    let it_ty = it_h.ty.clone();
    let close = owned && is_generator(ctx, &it_ty);
    let name = close.then(|| format!("#for_gen{}", ctx.locals.len()));
    let it = ctx.declare(name.clone(), it_ty.clone());
    out.push(HirStmt::Let {
        local: it,
        init: it_h,
    });
    let next = method_call_on(ctx, hir_local(it, &it_ty, span), "next", &[], span);
    let Some(name) = name else {
        return for_next_loop(ctx, label, pat, next, body, span);
    };
    let call = Expr {
        kind: ExprKind::MethodCall {
            receiver: Box::new(Expr {
                kind: ExprKind::Path(rokc_ast::Path {
                    segments: vec![rokc_ast::PathSegment {
                        name,
                        generics: Vec::new(),
                    }],
                    span,
                }),
                span,
            }),
            method: "close".to_owned(),
            type_args: Vec::new(),
            args: Vec::new(),
            trailing_closure: None,
        },
        span,
    };
    let fin = Block {
        stmts: vec![rokc_ast::Stmt {
            kind: rokc_ast::StmtKind::Expr(call),
            span,
        }],
        trailing: None,
        span,
        label: None,
    };
    // Outside this loop's breakable: its own `break` / `continue` do not
    // run it.
    ctx.err_frames.push(ErrFrame::Finally {
        block: fin,
        hir: Vec::new(),
        try_depth: ctx.try_depth,
        breakables: ctx.breakables.len() - 1,
    });
    let lp = for_next_loop(ctx, label, pat, next, body, span);
    ctx.err_frames.pop();
    let closed = method_call_on(ctx, hir_local(it, &it_ty, span), "close", &[], span);
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![HirStmt::Expr(lp), HirStmt::Expr(closed)],
            trailing: None,
            label: None,
        })),
        ty: HirTy::Unit,
        span,
    }
}

/// A `GenIter` / `TryGenIter`.
fn is_generator(ctx: &LowerCtx<'_>, ty: &HirTy) -> bool {
    let HirTy::Struct(sid) = ty else {
        return false;
    };
    let def = ctx.struct_def(*sid);
    ["rok::core::iter::GenIter", "rok::core::iter::TryGenIter"]
        .iter()
        .any(|base| def.name == mangle_inst(base, &def.generic_args))
}

/// `for await pat in s` (stdlib 2.8.2): `s.into_stream()` unless already
/// a `Stream`, then loop awaiting `poll_next(ref ctx)`.
#[allow(clippy::too_many_arguments)]
fn lower_for_stream(
    ctx: &mut LowerCtx<'_>,
    label: Option<&String>,
    pat: &AstPattern,
    s: HirExpr,
    body: &Block,
    span: rokc_diagnostic::Span,
    out: &mut Vec<HirStmt>,
) -> HirExpr {
    if s.ty == HirTy::Unknown {
        return s;
    }
    let is_stream = matches!(
        super::expr::pick_method(ctx, &s.ty, "poll_next", None),
        Ok(Some(_))
    );
    let s = if is_stream {
        s
    } else {
        method_call_on(ctx, s, "into_stream", &[], span)
    };
    let ty = s.ty.clone();
    let st = ctx.declare(None, ty.clone());
    out.push(HirStmt::Let { local: st, init: s });
    let next = super::coro::poll_loop(ctx, st, &ty, "poll_next", span);
    for_next_loop(ctx, label, pat, next, body, span)
}

/// `loop { match <next> { v: T => { let pat = v; body } none => break } }`
/// for `next` of type `T | none`.
fn for_next_loop(
    ctx: &mut LowerCtx<'_>,
    label: Option<&String>,
    pat: &AstPattern,
    next: HirExpr,
    body: &Block,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let sum_ty = next.ty.clone();
    let parts = match &sum_ty {
        HirTy::Sum(parts) => parts.clone(),
        HirTy::Unknown => return next,
        other => {
            return ctx.error_expr(
                span,
                format!(
                    "`next` of an iterator must return `T | none`, found `{}`",
                    ctx.ty_name(other)
                ),
            );
        }
    };
    let (Some(none_i), Some(item_i)) = (
        parts.iter().position(HirTy::is_none),
        parts.iter().position(|p| !p.is_none()),
    ) else {
        return ctx.error_expr(span, "`next` must return `T | none`".to_owned());
    };
    if parts.len() > 2 {
        return for_next_loop_sum(ctx, label, pat, next, &parts, none_i, body, span);
    }
    let item_ty = parts[item_i].clone();
    let v = ctx.declare(None, item_ty.clone());
    let mut arm_stmts: Vec<HirStmt> = Vec::new();
    bind_irrefutable(ctx, pat, hir_local(v, &item_ty, span), &mut arm_stmts);
    let lowered_body = lower_loop_body(ctx, body);
    arm_stmts.push(HirStmt::Expr(HirExpr {
        kind: HirExprKind::Block(Box::new(lowered_body)),
        ty: HirTy::Unit,
        span,
    }));
    let block = |stmts: Vec<HirStmt>| HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: None,
            label: None,
        })),
        ty: HirTy::Unit,
        span,
    };
    let arms = vec![
        HirMatchArm {
            pat: HirPattern::SumVariant {
                sum_ty: sum_ty.clone(),
                variant_index: u32::try_from(item_i).expect("variant overflow"),
                binder: Some(v),
                payload_ty: Some(item_ty),
            },
            guard: None,
            body: block(arm_stmts),
        },
        HirMatchArm {
            pat: HirPattern::SumVariant {
                sum_ty,
                variant_index: u32::try_from(none_i).expect("variant overflow"),
                binder: None,
                payload_ty: None,
            },
            guard: None,
            body: block(vec![HirStmt::Break {
                label: label.cloned(),
                value: None,
            }]),
        },
    ];
    let step = HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(next),
            arms,
        },
        ty: HirTy::Unit,
        span,
    };
    HirExpr {
        kind: HirExprKind::Loop {
            label: label.cloned(),
            body: Box::new(HirBlock {
                stmts: vec![HirStmt::Expr(step)],
                trailing: None,
                label: None,
            }),
            result_ty: HirTy::Unit,
        },
        ty: HirTy::Unit,
        span,
    }
}

/// `for_next_loop` for a sum item (`next` is `A | B | none`): match it into
/// `item: A | B` or `break`.
#[allow(clippy::too_many_arguments)]
fn for_next_loop_sum(
    ctx: &mut LowerCtx<'_>,
    label: Option<&String>,
    pat: &AstPattern,
    next: HirExpr,
    parts: &[HirTy],
    none_i: usize,
    body: &Block,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let sum_ty = next.ty.clone();
    let item_ty = HirTy::sum(
        parts
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != none_i)
            .map(|(_, p)| p.clone()),
    );
    let mut arms = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let index = u32::try_from(i).expect("variant overflow");
        if i == none_i {
            arms.push(HirMatchArm {
                pat: HirPattern::SumVariant {
                    sum_ty: sum_ty.clone(),
                    variant_index: index,
                    binder: None,
                    payload_ty: None,
                },
                guard: None,
                body: HirExpr {
                    kind: HirExprKind::Block(Box::new(HirBlock {
                        stmts: vec![HirStmt::Break {
                            label: label.cloned(),
                            value: None,
                        }],
                        trailing: None,
                        label: None,
                    })),
                    ty: HirTy::Never,
                    span,
                },
            });
            continue;
        }
        let v = ctx.declare(None, p.clone());
        let value = coerce(ctx, &item_ty, hir_local(v, p, span), &|| {
            "the loop item".to_owned()
        });
        arms.push(HirMatchArm {
            pat: HirPattern::SumVariant {
                sum_ty: sum_ty.clone(),
                variant_index: index,
                binder: Some(v),
                payload_ty: Some(p.clone()),
            },
            guard: None,
            body: value,
        });
    }
    let item = ctx.declare(None, item_ty.clone());
    let mut stmts = vec![HirStmt::Let {
        local: item,
        init: HirExpr {
            kind: HirExprKind::Match {
                scrutinee: Box::new(next),
                arms,
            },
            ty: item_ty.clone(),
            span,
        },
    }];
    bind_irrefutable(ctx, pat, hir_local(item, &item_ty, span), &mut stmts);
    let lowered_body = lower_loop_body(ctx, body);
    stmts.push(HirStmt::Expr(HirExpr {
        kind: HirExprKind::Block(Box::new(lowered_body)),
        ty: HirTy::Unit,
        span,
    }));
    HirExpr {
        kind: HirExprKind::Loop {
            label: label.cloned(),
            body: Box::new(HirBlock {
                stmts,
                trailing: None,
                label: None,
            }),
            result_ty: HirTy::Unit,
        },
        ty: HirTy::Unit,
        span,
    }
}

/// Integer range loop. Inclusive ranges use a `go` flag so `a..=MAX` ends;
/// other bounds (a user `Step`) iterate `range` like any iterable.
#[allow(clippy::too_many_arguments)]
pub(super) fn lower_for_range(
    ctx: &mut LowerCtx<'_>,
    label: Option<&String>,
    pat: &AstPattern,
    range: &Expr,
    start_e: &Expr,
    end_e: Option<&Expr>,
    inclusive: bool,
    body: &Block,
    span: rokc_diagnostic::Span,
    out: &mut Vec<HirStmt>,
) -> Option<HirExpr> {
    // `a..` (spec 3.7.8) counts up until a `break` (or overflow panics).
    let (start_h, end_h) = match end_e {
        Some(end_e) => {
            let (s, e) = lower_sibling_pair(ctx, start_e, end_e, HirTy::Unknown);
            (s, Some(e))
        }
        None => (lower_expected(ctx, start_e, HirTy::Unknown), None),
    };
    if start_h.ty == HirTy::Unknown || end_h.as_ref().is_some_and(|e| e.ty == HirTy::Unknown) {
        return None;
    }
    let elem_ty = start_h.ty.clone();
    if !matches!(elem_ty, HirTy::Prim(p) if p.is_int()) {
        let iter_h = lower_expected(ctx, range, HirTy::Unknown);
        return Some(lower_for_iter(
            ctx, label, pat, iter_h, body, span, out, false,
        ));
    }
    let prim = match &elem_ty {
        HirTy::Prim(p) if p.is_int() && end_h.as_ref().is_none_or(|e| e.ty == start_h.ty) => *p,
        _ => {
            return Some(ctx.error_expr(
                span,
                format!(
                    "range bounds must be integers of the same type, found `{}` and `{}`",
                    ctx.ty_name(&start_h.ty),
                    ctx.ty_name(end_h.as_ref().map_or(&start_h.ty, |e| &e.ty))
                ),
            ));
        }
    };
    let bool_ty = HirTy::Prim(PrimTy::Bool);
    let counter = ctx.declare(None, elem_ty.clone());
    let end = ctx.declare(None, elem_ty.clone());
    out.push(HirStmt::Let {
        local: counter,
        init: start_h,
    });
    let unbounded = end_h.is_none();
    if let Some(end_h) = end_h {
        out.push(HirStmt::Let {
            local: end,
            init: end_h,
        });
    }
    let c = || hir_local(counter, &elem_ty, span);
    let e = || hir_local(end, &elem_ty, span);
    let inc = |c: HirExpr| {
        hir_binop(
            PrimOp::Add,
            c,
            hir_int(1, prim, span),
            elem_ty.clone(),
            span,
        )
    };

    let mut body_stmts: Vec<HirStmt> = Vec::new();
    let snap = ctx.declare(None, elem_ty.clone());
    body_stmts.push(HirStmt::Let {
        local: snap,
        init: c(),
    });
    let cond = if inclusive {
        // go = start <= end; while go { snap = c; go = c < end; if go { c += 1 } ... }
        let go = ctx.declare(None, bool_ty.clone());
        out.push(HirStmt::Let {
            local: go,
            init: hir_binop(PrimOp::Le, c(), e(), bool_ty.clone(), span),
        });
        body_stmts.push(HirStmt::Assign {
            local: go,
            value: hir_binop(PrimOp::Lt, c(), e(), bool_ty.clone(), span),
        });
        body_stmts.push(HirStmt::Expr(HirExpr {
            kind: HirExprKind::If {
                cond: Box::new(hir_local(go, &bool_ty, span)),
                then_block: Box::new(HirBlock {
                    stmts: vec![HirStmt::Assign {
                        local: counter,
                        value: inc(c()),
                    }],
                    trailing: None,
                    label: None,
                }),
                else_branch: None,
            },
            ty: HirTy::Unit,
            span,
        }));
        hir_local(go, &bool_ty, span)
    } else {
        body_stmts.push(HirStmt::Assign {
            local: counter,
            value: inc(c()),
        });
        if unbounded {
            HirExpr {
                kind: HirExprKind::Const(ConstVal::Bool(true)),
                ty: bool_ty,
                span,
            }
        } else {
            hir_binop(PrimOp::Lt, c(), e(), bool_ty, span)
        }
    };
    bind_irrefutable(ctx, pat, hir_local(snap, &elem_ty, span), &mut body_stmts);
    let lowered_body = lower_loop_body(ctx, body);
    body_stmts.push(HirStmt::Expr(HirExpr {
        kind: HirExprKind::Block(Box::new(lowered_body)),
        ty: HirTy::Unit,
        span,
    }));
    Some(HirExpr {
        ty: HirTy::Unit,
        span,
        kind: HirExprKind::While {
            label: label.cloned(),
            cond: Box::new(cond),
            body: Box::new(HirBlock {
                stmts: body_stmts,
                trailing: None,
                label: None,
            }),
        },
    })
}

/// `for x in arr` over a `[T; N]` value: the array is bound to a local
/// once, then indexed by a counter.
#[allow(clippy::too_many_arguments)]
pub(super) fn lower_for_array(
    ctx: &mut LowerCtx<'_>,
    label: Option<&String>,
    pat: &AstPattern,
    iter_h: HirExpr,
    elem_ty: HirTy,
    len: u32,
    body: &Block,
    span: rokc_diagnostic::Span,
    out: &mut Vec<HirStmt>,
) -> HirExpr {
    let arr_ty = iter_h.ty.clone();
    let arr_local = if let HirExprKind::Local(l) = iter_h.kind {
        l
    } else {
        let l = ctx.declare(None, arr_ty.clone());
        out.push(HirStmt::Let {
            local: l,
            init: iter_h,
        });
        l
    };
    let idx_prim = PrimTy::I32;
    let idx_ty = HirTy::Prim(idx_prim);
    let counter = ctx.declare(None, idx_ty.clone());
    out.push(HirStmt::Let {
        local: counter,
        init: hir_int(0, idx_prim, span),
    });
    let cond = hir_binop(
        PrimOp::Lt,
        hir_local(counter, &idx_ty, span),
        hir_int(i128::from(len), idx_prim, span),
        HirTy::Prim(PrimTy::Bool),
        span,
    );
    let mut body_stmts: Vec<HirStmt> = Vec::new();
    let snap = ctx.declare(None, idx_ty.clone());
    body_stmts.push(HirStmt::Let {
        local: snap,
        init: hir_local(counter, &idx_ty, span),
    });
    body_stmts.push(HirStmt::Assign {
        local: counter,
        value: hir_binop(
            PrimOp::Add,
            hir_local(counter, &idx_ty, span),
            hir_int(1, idx_prim, span),
            idx_ty.clone(),
            span,
        ),
    });
    let elem_read = HirExpr {
        ty: elem_ty.clone(),
        span,
        kind: HirExprKind::ArrayIndex {
            array: Box::new(hir_local(arr_local, &arr_ty, span)),
            index: Box::new(hir_local(snap, &idx_ty, span)),
            elem_ty,
        },
    };
    bind_irrefutable(ctx, pat, elem_read, &mut body_stmts);
    let lowered_body = lower_loop_body(ctx, body);
    body_stmts.push(HirStmt::Expr(HirExpr {
        kind: HirExprKind::Block(Box::new(lowered_body)),
        ty: HirTy::Unit,
        span,
    }));
    HirExpr {
        ty: HirTy::Unit,
        span,
        kind: HirExprKind::While {
            label: label.cloned(),
            cond: Box::new(cond),
            body: Box::new(HirBlock {
                stmts: body_stmts,
                trailing: None,
                label: None,
            }),
        },
    }
}
