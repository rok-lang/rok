//! Value-based errors (spec 4.14): `throws`, `throw`, `try` / `try?` /
//! `try!`, `catch` / `finally`.
//!
//! `throws (E) -> T` returns the carrier `HirTy::Throws { ok, err }` (MIR:
//! `is_err` plus one payload, spec 8.6.5). Nothing unwinds: `try` tests
//! `is_err` and `raise`s the payload through `err_frames` innermost first,
//! running `finally` blocks inline until a `catch` that may take the type
//! parks the value and breaks to its dispatch; with no frame left the
//! function returns the error. `return` / `break` / `continue` run the
//! `finally` blocks they leave the same way.

#[allow(clippy::wildcard_imports)]
use super::*;

use super::desugar::{optional_parts, sum_none};

#[derive(Clone)]
pub(super) enum ErrFrame {
    Catch(CatchFrame),
    Finally {
        block: Block,
        /// Lowered statements run after `block` (a deferred `ref`
        /// write-back around an `await`).
        hir: Vec<HirStmt>,
        /// `try` nesting and breakable count where the `finally` sits;
        /// its block is lowered in that context at every exit.
        try_depth: u32,
        breakables: usize,
    },
}

#[derive(Clone)]
pub(super) struct CatchFrame {
    /// The catch body runs in a block with this label; a raise breaks
    /// out of it.
    label: String,
    /// Error type each `catch` arm takes (`None`: any) and the local saying
    /// which parked error was raised; `None` for `try?` / `try!` / throwing
    /// `main`, which take all.
    arms: Option<(Vec<Option<HirTy>>, LocalId)>,
    /// Distinct error types raised into this frame, each parked in its
    /// own local.
    errs: Vec<(HirTy, LocalId)>,
    /// Some raise reached this frame.
    hit: bool,
    /// Every error type raised into this frame (`try?` checks what it
    /// drops).
    raised: Vec<HirTy>,
    /// Throwing `main`'s frame: a `str` local each raise sets to its
    /// position, for the uncaught-error report.
    site: Option<LocalId>,
}

impl CatchFrame {
    fn takes(&self, ety: &HirTy) -> bool {
        self.arms
            .as_ref()
            .is_none_or(|(filters, _)| filters.iter().any(|f| may_take(f.as_ref(), ety)))
    }
}

/// Can an arm filtering `filter` (`None`: any) take `ety`? Unresolved
/// filters are kept so lowering the arm reports them.
fn may_take(filter: Option<&HirTy>, ety: &HirTy) -> bool {
    let parts = |t: &HirTy| match t {
        HirTy::Sum(vs) => vs.clone(),
        t => vec![t.clone()],
    };
    match filter {
        None
        | Some(
            HirTy::Path(_)
            | HirTy::None
            | HirTy::NoThrow
            | HirTy::OpenConst(_)
            | HirTy::Var(_)
            | HirTy::Unknown,
        ) => true,
        // An untyped `throws` error (`any Error`) may hold any type.
        Some(_) if matches!(ety, HirTy::AnyTrait(_)) => true,
        Some(f) => {
            let ps = parts(ety);
            parts(f).iter().any(|x| ps.contains(x))
        }
    }
}

/// The AST return type callers see: `ret_ty` (unit when absent),
/// wrapped in `Throws` when the fn declares `throws (E)`.
pub(super) fn sig_ret_ast(f: &FnDef) -> Ty {
    let ok = f.ret_ty.clone().unwrap_or(Ty {
        kind: AstTyKind::Tuple(Vec::new()),
        span: f.span,
    });
    let rokc_ast::ThrowsKind::Typed(errs) = &f.throws else {
        return ok;
    };
    let err = match errs.as_slice() {
        [] => return ok,
        [one] => one.clone(),
        many => Ty {
            kind: AstTyKind::Sum(many.to_vec()),
            span: f.span,
        },
    };
    Ty {
        span: ok.span,
        kind: AstTyKind::Throws {
            ok: Box::new(ok),
            err: Box::new(err),
        },
    }
}

/// Unbound error parameters (`#E0` or plain `E`) → `Unknown`, so a
/// `fn(i32) throws (E) -> i32` hint still types a closure argument.
pub(super) fn erase_unbound_errors(ty: &mut HirTy) {
    match ty {
        HirTy::Throws { ok, err } => {
            let unbound = |t: &HirTy| {
                matches!(
                    t,
                    HirTy::Path(_)
                        | HirTy::None
                        | HirTy::NoThrow
                        | HirTy::OpenConst(_)
                        | HirTy::Var(_)
                )
            };
            if unbound(err) || matches!(&**err, HirTy::Sum(vs) if vs.iter().any(unbound)) {
                **err = HirTy::Unknown;
            }
            erase_unbound_errors(ok);
        }
        HirTy::Fn(args, ret, _) | HirTy::Closure { args, ret, .. } => {
            args.iter_mut().for_each(erase_unbound_errors);
            erase_unbound_errors(ret);
        }
        HirTy::Sum(ts) | HirTy::Tuple(ts) => ts.iter_mut().for_each(erase_unbound_errors),
        _ => {}
    }
}

fn fresh_label(ctx: &mut LowerCtx<'_>) -> String {
    ctx.next_synth += 1;
    format!("#catch{}", ctx.next_synth)
}

pub(super) const fn unit_value(span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Const(ConstVal::Unit),
        ty: HirTy::Unit,
        span,
    }
}

/// Placeholder for the unused payload slot of a carrier (codegen
/// materializes zero / undef).
fn undef(ty: &HirTy, span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Const(ConstVal::Unit),
        ty: ty.clone(),
        span,
    }
}

fn block_expr(stmts: Vec<HirStmt>, span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: None,
            label: None,
        })),
        ty: HirTy::Unit,
        span,
    }
}

/// A `()` / `!` ok side has no carrier slot (`rokc_mir`'s layout).
pub(super) const fn no_ok_slot(ok: &HirTy) -> bool {
    matches!(ok, HirTy::Unit | HirTy::Never)
}

/// Carrier field index of the error payload.
const fn err_index(ok: &HirTy) -> u32 {
    if no_ok_slot(ok) { 1 } else { 2 }
}

/// Build a carrier: `Ok(v)` → `(false, v, _)`, `Err(e)` → `(true, _, e)`.
pub(super) fn carrier(
    ok: &HirTy,
    err: &HirTy,
    value: Result<HirExpr, HirExpr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let (is_err, ok_v, err_v) = match value {
        Ok(v) => (false, v, undef(err, span)),
        Err(e) => (true, undef(ok, span), e),
    };
    let mut lets = Vec::new();
    let mut fields = vec![HirExpr {
        kind: HirExprKind::Const(ConstVal::Bool(is_err)),
        ty: HirTy::Prim(PrimTy::Bool),
        span,
    }];
    if no_ok_slot(ok) {
        // No slot, but keep the value's side effects.
        if !matches!(ok_v.kind, HirExprKind::Const(_)) {
            lets.push(HirStmt::Expr(ok_v));
        }
    } else {
        fields.push(ok_v);
    }
    fields.push(err_v);
    let ty = HirTy::Throws {
        ok: Box::new(ok.clone()),
        err: Box::new(err.clone()),
    };
    super::expr::with_lets(
        lets,
        HirExpr {
            kind: HirExprKind::Tuple { fields },
            ty,
            span,
        },
    )
}

/// A call whose type is a `throws` carrier: `{ let t = call; if
/// t.is_err { raise(t.err) } t.ok }`. It must sit under a `try`.
pub(super) fn unpack_call(ctx: &mut LowerCtx<'_>, call: HirExpr) -> HirExpr {
    let HirTy::Throws { ok, err } = call.ty.clone() else {
        return call;
    };
    ctx.throwing_calls += 1;
    let span = call.span;
    let carrier_ty = call.ty.clone();
    let tmp = ctx.declare(None, carrier_ty.clone());
    let field = |index: u32, ty: &HirTy| HirExpr {
        kind: HirExprKind::TupleField {
            target: Box::new(hir_local(tmp, &carrier_ty, span)),
            index,
            field_ty: ty.clone(),
        },
        ty: ty.clone(),
        span,
    };
    let mut stmts = vec![HirStmt::Let {
        local: tmp,
        init: call,
    }];
    if ctx.try_depth == 0 {
        let mut d = rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Type,
            ctx.here(),
            format!(
                "this call can throw `{}` and must be marked with `try` (`try f()`; \
                 `try? f()` / `try! f()` to drop / panic on the error) (spec 4.14)",
                ctx.ty_name(&err)
            ),
        );
        if ctx.in_try_for_body > 0 {
            d = d.with_help(
                "the `try` of `try for` covers the iteration, not the loop body".to_owned(),
            );
        }
        // A call written in the source (not one lowering made up).
        if ctx.src(span).is_some_and(|s| s.ends_with(')')) {
            let at = rokc_diagnostic::Span {
                end: span.start,
                ..span
            };
            d = d.with_fix(at, "try ", "mark the call with `try`");
        }
        ctx.emit_diag(d);
    } else {
        let raised = raise(ctx, field(err_index(&ok), &err), span);
        stmts.push(HirStmt::Expr(HirExpr {
            kind: HirExprKind::If {
                cond: Box::new(field(0, &HirTy::Prim(PrimTy::Bool))),
                then_block: Box::new(HirBlock {
                    stmts: raised,
                    trailing: None,
                    label: None,
                }),
                else_branch: None,
            },
            ty: HirTy::Unit,
            span,
        }));
    }
    let trailing = (!no_ok_slot(&ok)).then(|| field(1, &ok));
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing,
            label: None,
        })),
        ty: *ok,
        span,
    }
}

/// Route error value `err` from the current point (see module doc).
/// The returned statements never complete normally.
pub(super) fn raise(
    ctx: &mut LowerCtx<'_>,
    err: HirExpr,
    span: rokc_diagnostic::Span,
) -> Vec<HirStmt> {
    let ety = err.ty.clone();
    let tmp = ctx.declare(None, ety.clone());
    let mut out = vec![HirStmt::Let {
        local: tmp,
        init: err,
    }];
    let value = hir_local(tmp, &ety, span);
    if ety == HirTy::Unknown {
        out.push(HirStmt::Return(None));
        return out;
    }
    // Under `catch` arms a sum error is raised variant by variant, so
    // each variant finds its own handler (or propagates on its own).
    if let HirTy::Sum(variants) = &ety {
        let has_arms = ctx
            .err_frames
            .iter()
            .any(|f| matches!(f, ErrFrame::Catch(c) if c.arms.is_some()));
        if has_arms {
            let arms = variants
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let b = ctx.declare(None, v.clone());
                    let body = block_expr(raise(ctx, hir_local(b, v, span), span), span);
                    HirMatchArm {
                        pat: HirPattern::SumVariant {
                            sum_ty: ety.clone(),
                            variant_index: u32::try_from(i).expect("variant overflow"),
                            binder: Some(b),
                            payload_ty: Some(v.clone()),
                        },
                        guard: None,
                        body,
                    }
                })
                .collect();
            out.push(HirStmt::Expr(HirExpr {
                kind: HirExprKind::Match {
                    scrutinee: Box::new(value),
                    arms,
                },
                ty: HirTy::Unit,
                span,
            }));
            return out;
        }
    }
    for i in (0..ctx.err_frames.len()).rev() {
        let (label, which) = match &ctx.err_frames[i] {
            ErrFrame::Finally { .. } => {
                out.push(run_finally(ctx, i));
                continue;
            }
            ErrFrame::Catch(c) if c.takes(&ety) => (c.label.clone(), c.arms.as_ref().map(|a| a.1)),
            ErrFrame::Catch(_) => continue,
        };
        if let Some(which) = which {
            let parked = match &ctx.err_frames[i] {
                ErrFrame::Catch(c) => c.errs.iter().position(|(t, _)| *t == ety),
                ErrFrame::Finally { .. } => None,
            };
            let k = parked.unwrap_or_else(|| {
                let local = ctx.declare(None, ety.clone());
                let ErrFrame::Catch(c) = &mut ctx.err_frames[i] else {
                    unreachable!("frame {i} is a catch")
                };
                c.errs.push((ety.clone(), local));
                c.errs.len() - 1
            });
            let ErrFrame::Catch(c) = &ctx.err_frames[i] else {
                unreachable!("frame {i} is a catch")
            };
            out.push(HirStmt::Assign {
                local: c.errs[k].1,
                value,
            });
            out.push(HirStmt::Assign {
                local: which,
                value: hir_int(
                    i128::try_from(k).expect("error index fits"),
                    PrimTy::I32,
                    span,
                ),
            });
            if let Some(site) = c.site {
                out.push(HirStmt::Assign {
                    local: site,
                    value: HirExpr {
                        kind: HirExprKind::Const(ConstVal::Str(ctx.env.sources.location(span))),
                        ty: HirTy::Prim(PrimTy::Str),
                        span,
                    },
                });
            }
        }
        if let ErrFrame::Catch(c) = &mut ctx.err_frames[i] {
            c.hit = true;
            if !c.raised.contains(&ety) {
                c.raised.push(ety);
            }
        }
        out.push(HirStmt::Break {
            label: Some(label),
            value: None,
        });
        return out;
    }
    // No frame takes it: it leaves the function.
    if let Some(found) = &mut ctx.throws_probe {
        if !found.contains(&ety) {
            found.push(ety);
        }
    } else if let Some(decl) = ctx.fn_throws.clone() {
        if let Some(v) = convert_err(ctx, value, &decl) {
            let ok = carrier_ok(ctx);
            // `ref` parameters go back on the error path too (spec 4.7).
            let c = super::mono::ref_pack(ctx, carrier(&ok, &decl, Err(v), span), span);
            out.extend(super::coro::frame_end(ctx, span));
            out.push(HirStmt::Return(Some(c)));
            return out;
        }
        // An untyped `throws` takes any `Error`; `throw "msg"` (Python,
        // JS) throws a `str`, which is none.
        if any_error(&decl) {
            let name = ctx.ty_name(&ety);
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!("`{name}` can't be thrown: it does not implement `Error` (spec 4.14)"),
            );
            // A `str`'s help quotes it (`lower_throw`).
            if ety != HirTy::Prim(PrimTy::Str) {
                ctx.help_last(Some(format!(
                    "add `impl Error for {name} {{}}` (it needs `Display` and `Debug`), or \
                     throw a `ValueError`: `throw ValueError::new(..);`"
                )));
            }
        } else {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "error `{}` is not covered by the enclosing function's `throws ({})` - \
                     catch it or add it to the `throws` list (spec 4.14)",
                    ctx.ty_name(&ety),
                    ctx.ty_name(&decl)
                ),
            );
        }
    } else {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "error `{}` is not handled: catch it, or declare `throws ({0})` on the \
                 enclosing function (spec 4.14)",
                ctx.ty_name(&ety)
            ),
        );
    }
    out.push(HirStmt::Return(None));
    out
}

/// Error `value` as a `to` error: itself, injected into sum `to`, or
/// re-wrapped member by member; `None` if `to` can't hold it.
fn convert_err(ctx: &mut LowerCtx<'_>, value: HirExpr, to: &HirTy) -> Option<HirExpr> {
    let from = value.ty.clone();
    if !err_fits(ctx, &from, to) {
        return None;
    }
    if from == *to {
        return Some(value);
    }
    let HirTy::Sum(vs) = &from else {
        // Into a sum, or into `any Error` (untyped `throws`) boxed.
        let boxed = box_into_any(ctx, to, value);
        return Some(wrap_into_sum(to, boxed));
    };
    // A sum: convert variant by variant.
    let span = value.span;
    let arms = vs
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let b = ctx.declare(None, v.clone());
            let body = convert_err(ctx, hir_local(b, v, span), to).expect("checked by err_fits");
            HirMatchArm {
                pat: HirPattern::SumVariant {
                    sum_ty: from.clone(),
                    variant_index: u32::try_from(i).expect("variant overflow"),
                    binder: Some(b),
                    payload_ty: Some(v.clone()),
                },
                guard: None,
                body,
            }
        })
        .collect();
    Some(HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(value),
            arms,
        },
        ty: to.clone(),
        span,
    })
}

/// Can an error of type `from` leave through `throws (to)`?
fn err_fits(ctx: &LowerCtx<'_>, from: &HirTy, to: &HirTy) -> bool {
    from == to
        || match from {
            HirTy::Sum(vs) => vs.iter().all(|v| err_fits(ctx, v, to)),
            _ => matches!(to, HirTy::Sum(ws) if ws.contains(from)) || coercible(ctx, to, from),
        }
}

/// Does a captured function value of type `ty` meet `fn` slot bounds
/// `want` (a thunk capturing it is `Send` only if the callee is)?
fn value_meets(ctx: &LowerCtx<'_>, ty: &HirTy, want: &crate::FnBounds) -> bool {
    match ty {
        HirTy::Fn(_, _, have) => {
            want.auto.iter().all(|b| have.auto.contains(b)) && (!want.unsafe_fn || have.unsafe_fn)
        }
        t => !want.unsafe_fn && want.auto.iter().all(|b| ctx.has_trait(t, b)),
    }
}

/// Spec 4.4.9: a function value throwing less than its slot goes through
/// `fn #thunk(args) throws (E) -> T { try f(args) }`; a named fn is called
/// directly, other values captured.
pub(super) fn adapt_fn_value(ctx: &LowerCtx<'_>, slot: &HirTy, value: HirExpr) -> HirExpr {
    let (
        HirTy::Fn(args, sret, want),
        HirTy::Fn(vargs, vret, _)
        | HirTy::Closure {
            args: vargs,
            ret: vret,
            ..
        },
    ) = (slot, &value.ty)
    else {
        return value;
    };
    // An `extern "C" fn` slot takes a C function as is (no thunk).
    if want.c_abi {
        return value;
    }
    let target = match &value.kind {
        HirExprKind::FnRef(t) => Some(*t),
        _ => None,
    };
    let HirTy::Throws { ok, err } = &**sret else {
        return value;
    };
    let fits = args == vargs
        && match &**vret {
            HirTy::Throws { ok: vok, err: verr } => {
                vok == ok && verr != err && err_fits(ctx, verr, err)
            }
            t => t == &**ok,
        };
    // The thunk captures a non-static callee: it is `Send` only if that
    // callee is.
    let bounds_ok = target.is_some() || value_meets(ctx, &value.ty, want);
    if !fits || !bounds_ok {
        return value;
    }
    let span = value.span;
    let callee_ty = HirTy::Fn(vargs.clone(), vret.clone(), want.clone());
    let env_ty = HirTy::Tuple(vec![callee_ty.clone()]);
    // One thunk per (callee, slot type): a named callee by id, a
    // captured one by its type.
    let key = thunk_key("throws", target, slot, &callee_ty);
    let cached = ctx.env.sink.thunks.borrow().get(&key).copied();
    super::incr::log(
        || format!("h|{key}"),
        || {
            let k = super::incr::Key::Thunk("throws", target, slot.clone(), callee_ty.clone());
            super::incr::Lookup::Thunk(k, cached)
        },
    );
    let id = cached.unwrap_or_else(|| {
        let id = FunctionId(ctx.env.sink.next_id.get());
        ctx.env.sink.next_id.set(id.0 + 1);
        let mut a = LowerCtx::with_subst(ctx.env, ctx.ty_subst.clone(), None, ctx.here());
        // The captured callee: `env.0` of a one-field env tuple.
        let env_param = target.is_none().then(|| HirParam {
            name: "#env".to_owned(),
            local: a.declare(None, env_ty.clone()),
            ty: env_ty.clone(),
            by_ref: false,
        });
        let params: Vec<HirParam> = args
            .iter()
            .enumerate()
            .map(|(i, t)| HirParam {
                name: format!("#a{i}"),
                local: a.declare(None, t.clone()),
                ty: t.clone(),
                by_ref: false,
            })
            .collect();
        let call_args: Vec<HirExpr> = params
            .iter()
            .map(|p| hir_local(p.local, &p.ty, span))
            .collect();
        let kind = match (target, &env_param) {
            (Some(t), _) => HirExprKind::Call {
                callee: t,
                args: call_args,
            },
            (None, Some(env)) => HirExprKind::CallIndirect {
                callee: Box::new(HirExpr {
                    kind: HirExprKind::TupleField {
                        target: Box::new(hir_local(env.local, &env_ty, span)),
                        index: 0,
                        field_ty: callee_ty.clone(),
                    },
                    ty: callee_ty.clone(),
                    span,
                }),
                args: call_args,
                ret_ty: (**vret).clone(),
            },
            (None, None) => unreachable!("env param exists without a static target"),
        };
        let call = HirExpr {
            kind,
            ty: (**vret).clone(),
            span,
        };
        a.fn_throws = Some((**err).clone());
        a.fn_ret_ty = (**ok).clone();
        a.try_depth = 1;
        let v = unpack_call(&mut a, call);
        let mut body = if no_ok_slot(ok) {
            HirBlock {
                stmts: vec![HirStmt::Expr(v)],
                trailing: None,
                label: None,
            }
        } else {
            HirBlock {
                stmts: Vec::new(),
                trailing: Some(v),
                label: None,
            }
        };
        finish_throwing_body(&a, &mut body);
        let mut all_params: Vec<HirParam> = env_param.into_iter().collect();
        all_params.extend(params);
        ctx.env.sink.extras.borrow_mut().push(HirFunction {
            id,
            def_id: None,
            name: format!("__thunk_{}", id.0),
            params: all_params,
            locals: a.into_locals(),
            ret_ty: (**sret).clone(),
            body: Some(body),
            is_async: false,
            is_generator: false,
            span,
            facts: rokc_ir_common::FnFacts::default(),
        });
        super::incr::touch(id);
        super::incr::touch_key(|| {
            super::incr::Key::Thunk("throws", target, slot.clone(), callee_ty.clone())
        });
        ctx.env.sink.thunks.borrow_mut().insert(key, id);
        id
    });
    let kind = if target.is_some() {
        HirExprKind::FnRef(id)
    } else {
        let mut v = value;
        v.ty = callee_ty;
        HirExprKind::ClosureValue {
            fn_id: id,
            env: Box::new(HirExpr {
                kind: HirExprKind::Tuple { fields: vec![v] },
                ty: env_ty.clone(),
                span,
            }),
            env_ty,
            arg_tys: args.clone(),
            ret_ty: (**sret).clone(),
        }
    };
    HirExpr {
        kind,
        ty: slot.clone(),
        span,
    }
}

/// Spec 4.4: `fn(..) -> !` fits a `fn(..) -> R` slot through a thunk
/// `fn #thunk(args) -> R { f(args) }` (a throwing one passes its error
/// on); a named fn is called directly, other values captured.
pub(super) fn adapt_never_fn(ctx: &LowerCtx<'_>, slot: &HirTy, value: HirExpr) -> HirExpr {
    let (
        HirTy::Fn(args, sret, want),
        HirTy::Fn(vargs, vret, _)
        | HirTy::Closure {
            args: vargs,
            ret: vret,
            ..
        },
    ) = (slot, &value.ty)
    else {
        return value;
    };
    // An `extern "C" fn` slot takes a C function as is (no thunk).
    if want.c_abi {
        return value;
    }
    let split = |t: &HirTy| match t {
        HirTy::Throws { ok, err } => ((**ok).clone(), Some((**err).clone())),
        t => (t.clone(), None),
    };
    let ((v_ok, v_err), (s_ok, s_err)) = (split(vret), split(sret));
    if v_ok != HirTy::Never || s_ok == HirTy::Never || args != vargs || v_err != s_err {
        return value;
    }
    let target = match &value.kind {
        HirExprKind::FnRef(t) => Some(*t),
        _ => None,
    };
    let bounds_ok = target.is_some() || value_meets(ctx, &value.ty, want);
    if !bounds_ok {
        return value;
    }
    let span = value.span;
    let callee_ty = HirTy::Fn(vargs.clone(), vret.clone(), want.clone());
    let env_ty = HirTy::Tuple(vec![callee_ty.clone()]);
    // One thunk per (callee, slot type): a named callee by id, a
    // captured one by its type.
    let key = thunk_key("never", target, slot, &callee_ty);
    let cached = ctx.env.sink.thunks.borrow().get(&key).copied();
    super::incr::log(
        || format!("h|{key}"),
        || {
            let k = super::incr::Key::Thunk("never", target, slot.clone(), callee_ty.clone());
            super::incr::Lookup::Thunk(k, cached)
        },
    );
    let id = cached.unwrap_or_else(|| {
        let id = FunctionId(ctx.env.sink.next_id.get());
        ctx.env.sink.next_id.set(id.0 + 1);
        let mut a = LowerCtx::with_subst(ctx.env, ctx.ty_subst.clone(), None, ctx.here());
        let env_param = target.is_none().then(|| HirParam {
            name: "#env".to_owned(),
            local: a.declare(None, env_ty.clone()),
            ty: env_ty.clone(),
            by_ref: false,
        });
        let params: Vec<HirParam> = args
            .iter()
            .enumerate()
            .map(|(i, t)| HirParam {
                name: format!("#a{i}"),
                local: a.declare(None, t.clone()),
                ty: t.clone(),
                by_ref: false,
            })
            .collect();
        let call_args: Vec<HirExpr> = params
            .iter()
            .map(|p| hir_local(p.local, &p.ty, span))
            .collect();
        let kind = match (target, &env_param) {
            (Some(t), _) => HirExprKind::Call {
                callee: t,
                args: call_args,
            },
            (None, Some(env)) => HirExprKind::CallIndirect {
                callee: Box::new(HirExpr {
                    kind: HirExprKind::TupleField {
                        target: Box::new(hir_local(env.local, &env_ty, span)),
                        index: 0,
                        field_ty: callee_ty.clone(),
                    },
                    ty: callee_ty.clone(),
                    span,
                }),
                args: call_args,
                ret_ty: (**vret).clone(),
            },
            (None, None) => unreachable!("env param exists without a static target"),
        };
        let stmts = match &v_err {
            None => vec![HirStmt::Panic(HirExpr {
                kind,
                ty: HirTy::Unit,
                span,
            })],
            Some(e) => {
                let t = a.declare(None, (**vret).clone());
                let err = HirExpr {
                    kind: HirExprKind::TupleField {
                        target: Box::new(hir_local(t, vret, span)),
                        index: err_index(&HirTy::Never),
                        field_ty: e.clone(),
                    },
                    ty: e.clone(),
                    span,
                };
                vec![
                    HirStmt::Let {
                        local: t,
                        init: HirExpr {
                            kind,
                            ty: (**vret).clone(),
                            span,
                        },
                    },
                    HirStmt::Return(Some(carrier(&s_ok, e, Err(err), span))),
                ]
            }
        };
        let mut all_params: Vec<HirParam> = env_param.into_iter().collect();
        all_params.extend(params);
        let params = all_params;
        ctx.env.sink.extras.borrow_mut().push(HirFunction {
            id,
            def_id: None,
            name: format!("__thunk_{}", id.0),
            params,
            locals: a.into_locals(),
            ret_ty: (**sret).clone(),
            body: Some(HirBlock {
                stmts,
                trailing: None,
                label: None,
            }),
            is_async: false,
            is_generator: false,
            span,
            facts: rokc_ir_common::FnFacts::default(),
        });
        super::incr::touch(id);
        super::incr::touch_key(|| {
            super::incr::Key::Thunk("never", target, slot.clone(), callee_ty.clone())
        });
        ctx.env.sink.thunks.borrow_mut().insert(key, id);
        id
    });
    let kind = if target.is_some() {
        HirExprKind::FnRef(id)
    } else {
        let mut v = value;
        v.ty = callee_ty;
        HirExprKind::ClosureValue {
            fn_id: id,
            env: Box::new(HirExpr {
                kind: HirExprKind::Tuple { fields: vec![v] },
                ty: env_ty.clone(),
                span,
            }),
            env_ty,
            arg_tys: args.clone(),
            ret_ty: (**sret).clone(),
        }
    };
    HirExpr {
        kind,
        ty: slot.clone(),
        span,
    }
}

/// A `rethrows` function as a value: the instance the expected `fn` type
/// asks for, else the throwing one with unbound errors as `any Error`.
/// `None` for real type parameters.
pub(super) fn rethrows_value(
    ctx: &LowerCtx<'_>,
    def: &FnDef,
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    let mut def = def.clone();
    for gp in &mut def.generics.params {
        match gp {
            rokc_ast::GenericParam::Type { name, default, .. } if name.starts_with('#') => {
                *default = Some(rokc_ast::untyped_throws_ty(span));
            }
            _ => return None,
        }
    }
    let args: Vec<HirExpr> = match ctx.current_expected() {
        Some(HirTy::Fn(ps, _, _)) if ps.len() == def.params.len() => {
            ps.iter().map(|t| undef(t, span)).collect()
        }
        _ => Vec::new(),
    };
    let (id, ret) = monomorphize(ctx, &def, None, &args, span)?;
    let params = ctx
        .env
        .sink
        .mono_sigs
        .borrow()
        .get(&id)
        .map(|s| s.param_tys.clone())
        .unwrap_or_default();
    Some(HirExpr {
        kind: HirExprKind::FnRef(id),
        ty: HirTy::Fn(params, Box::new(ret), ctx.env.names.all_auto()),
        span,
    })
}

/// Lower frame `i`'s `finally` block inline, outside that frame and
/// everything nested in it (a raise inside it goes outward).
fn run_finally(ctx: &mut LowerCtx<'_>, i: usize) -> HirStmt {
    let ErrFrame::Finally {
        block,
        hir,
        try_depth,
        ..
    } = &ctx.err_frames[i]
    else {
        unreachable!("frame {i} is a finally")
    };
    let (block, hir, try_depth) = (block.clone(), hir.clone(), *try_depth);
    let inner = ctx.err_frames.split_off(i);
    let saved = std::mem::replace(&mut ctx.try_depth, try_depth);
    ctx.push_expected(HirTy::Unit);
    let mut b = lower_block(ctx, &block);
    ctx.pop_expected();
    ctx.try_depth = saved;
    ctx.err_frames.extend(inner);
    b.stmts.extend(hir);
    HirStmt::Expr(HirExpr {
        kind: HirExprKind::Block(Box::new(b)),
        ty: HirTy::Unit,
        span: block.span,
    })
}

/// `jump` (`return` / `break` / `continue` with `value`) after the
/// `finally` blocks it leaves: all for `return` (`depth` `None`), else
/// those inside breakable `depth`.
pub(super) fn exit_through(
    ctx: &mut LowerCtx<'_>,
    depth: Option<usize>,
    value: Option<HirExpr>,
    jump: impl FnOnce(Option<HirExpr>) -> HirStmt,
    span: rokc_diagnostic::Span,
) -> HirStmt {
    let fins: Vec<usize> = (0..ctx.err_frames.len())
        .rev()
        .filter(|&i| {
            matches!(&ctx.err_frames[i], ErrFrame::Finally { breakables, .. }
                if depth.is_none_or(|d| *breakables > d))
        })
        .collect();
    if fins.is_empty() {
        return jump(value);
    }
    let mut stmts = Vec::new();
    let value = value.and_then(|v| {
        if v.ty == HirTy::Unit {
            stmts.push(HirStmt::Expr(v));
            return None;
        }
        let ty = v.ty.clone();
        let t = ctx.declare(None, ty.clone());
        stmts.push(HirStmt::Let { local: t, init: v });
        Some(hir_local(t, &ty, span))
    });
    for i in fins {
        stmts.push(run_finally(ctx, i));
    }
    stmts.push(jump(value));
    HirStmt::Expr(block_expr(stmts, span))
}

/// `return value` from the current point: wrapped into the ok carrier in
/// a throwing function, after the `finally` blocks it leaves.
pub(super) fn lower_return(
    ctx: &mut LowerCtx<'_>,
    value: Option<HirExpr>,
    span: rokc_diagnostic::Span,
) -> HirStmt {
    if ctx.coro.is_some() {
        return exit_through(ctx, None, value, super::coro::coro_return, span);
    }
    let value = match ctx.fn_throws.clone() {
        Some(err) => {
            let ok = carrier_ok(ctx);
            let v = value.unwrap_or_else(|| unit_value(span));
            Some(super::mono::ref_pack(
                ctx,
                carrier(&ok, &err, Ok(v), span),
                span,
            ))
        }
        // `ref` parameters (spec 4.7): hand their values back too.
        None if !ctx.ref_params.is_empty() => Some(super::mono::ref_pack(
            ctx,
            value.unwrap_or_else(|| unit_value(span)),
            span,
        )),
        None => value,
    };
    exit_through(ctx, None, value, HirStmt::Return, span)
}

/// Index in `ctx.breakables` of the target of a `break` / `continue`.
pub(super) fn jump_index(ctx: &LowerCtx<'_>, label: Option<&str>, needs_loop: bool) -> usize {
    ctx.breakables
        .iter()
        .rposition(|(bl, is_loop)| match label {
            None => *is_loop,
            Some(l) => bl.as_deref() == Some(l) && (*is_loop || !needs_loop),
        })
        .unwrap_or(usize::MAX)
}

/// An untyped `throws`: `any Error`.
fn any_error(t: &HirTy) -> bool {
    matches!(t, HirTy::AnyTrait(b) if b.iter().any(|t| t.path == "rok::core::error::Error"))
}

pub(super) fn lower_throw(
    ctx: &mut LowerCtx<'_>,
    value: &Expr,
    span: rokc_diagnostic::Span,
) -> HirStmt {
    let in_catch = ctx
        .err_frames
        .iter()
        .any(|f| matches!(f, ErrFrame::Catch(_)));
    let expected = if in_catch {
        HirTy::Unknown
    } else {
        ctx.fn_throws.clone().unwrap_or(HirTy::Unknown)
    };
    let v = lower_expected(ctx, value, expected);
    let since = ctx.env.sink.diagnostics.borrow().len();
    let is_str = v.ty == HirTy::Prim(PrimTy::Str) && ctx.fn_throws.as_ref().is_some_and(any_error);
    let stmts = raise(ctx, v, span);
    if let (true, Some(msg)) = (is_str, ctx.src(value.span)) {
        ctx.help_since(
            since,
            format!("throw a `ValueError`: `throw ValueError::new({msg});`"),
        );
    }
    HirStmt::Expr(block_expr(stmts, span))
}

/// Push a frame that takes every error (`try?` / `try!` / throwing
/// `main`).
pub(super) fn catch_all_frame(ctx: &mut LowerCtx<'_>) {
    let label = fresh_label(ctx);
    ctx.err_frames.push(ErrFrame::Catch(CatchFrame {
        label,
        arms: None,
        errs: Vec::new(),
        hit: false,
        raised: Vec::new(),
        site: None,
    }));
}

fn pop_catch(ctx: &mut LowerCtx<'_>) -> CatchFrame {
    match ctx.err_frames.pop() {
        Some(ErrFrame::Catch(c)) => c,
        _ => unreachable!("frames are pushed and popped in pairs"),
    }
}

/// `'done: { '<frame>: { break 'done body }; handler }`: a raise breaks
/// into the handler, normal completion skips it.
fn shell(
    ctx: &mut LowerCtx<'_>,
    frame: &CatchFrame,
    body: HirExpr,
    result_ty: HirTy,
    handler: (Vec<HirStmt>, Option<HirExpr>),
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let done = fresh_label(ctx);
    let inner_stmts = if hir_expr_diverges(&body) {
        vec![HirStmt::Expr(body)]
    } else if result_ty == HirTy::Unit {
        vec![
            HirStmt::Expr(body),
            HirStmt::Break {
                label: Some(done.clone()),
                value: None,
            },
        ]
    } else {
        vec![HirStmt::Break {
            label: Some(done.clone()),
            value: Some(body),
        }]
    };
    let inner = HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: inner_stmts,
            trailing: None,
            label: Some(frame.label.clone()),
        })),
        ty: HirTy::Unit,
        span,
    };
    let (handler_stmts, trailing) = handler;
    let mut stmts = vec![HirStmt::Expr(inner)];
    stmts.extend(handler_stmts);
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing,
            label: Some(done),
        })),
        ty: result_ty,
        span,
    }
}

/// `msg` to stderr through never-returning runtime hook `hook(msg, len)`.
pub(super) fn hook_call(
    ctx: &LowerCtx<'_>,
    hook: &str,
    msg: &str,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let Some(sig) = ctx.env.sig(hook) else {
        return ctx.error_expr(
            span,
            format!("internal: runtime hook `{hook}` is not declared"),
        );
    };
    HirExpr {
        kind: HirExprKind::Call {
            callee: sig.id,
            args: vec![HirExpr {
                kind: HirExprKind::Const(ConstVal::Str(msg.to_owned())),
                ty: HirTy::Prim(PrimTy::Str),
                span,
            }],
        },
        ty: HirTy::Unit,
        span,
    }
}

/// Prefix `try e` / `try? e` / `try! e`.
pub(super) fn lower_try(
    ctx: &mut LowerCtx<'_>,
    kind: rokc_ast::TryKind,
    operand: &Expr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    use rokc_ast::TryKind;
    let expected = ctx.current_expected().cloned().unwrap_or(HirTy::Unknown);
    let calls = ctx.throwing_calls;
    let mark = ctx.env.sink.diagnostics.borrow().len();
    ctx.try_for = matches!(operand.kind, ExprKind::For { .. }).then_some(ctx.try_depth);
    // An operand that failed to lower (an error, or a poisoned value) may
    // hide its throwing call.
    let warn_if_idle = |ctx: &LowerCtx<'_>, e: &HirExpr| {
        let failed = ctx
            .env
            .sink
            .diagnostics
            .borrow()
            .iter()
            .skip(mark)
            .any(|d| d.severity == rokc_diagnostic::Severity::Error);
        if ctx.throwing_calls == calls && !failed && e.ty != HirTy::Unknown {
            ctx.emit_warning("this `try` covers no throwing call (spec 3.14.2)".to_owned());
        }
    };
    // `try! xs.pop()`: no throwing call, an optional value.
    let optional = |ctx: &LowerCtx<'_>, e: &HirExpr| {
        if ctx.throwing_calls != calls {
            return None;
        }
        let some = super::suggest::some_part(&e.ty)?;
        let op = super::suggest::short_src(ctx, operand.span).unwrap_or("value");
        let t = ctx.ty_name(&e.ty);
        let d = super::suggest::default_lit(&some).unwrap_or("default");
        let word = if kind == TryKind::Force {
            "try!"
        } else {
            "try"
        };
        let fix = format!("{op} ?? {d}");
        ctx.emit_diag(
            rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Type,
                span,
                format!("`{word}` is for calls that throw; `{op}` gives `{t}`, which may be none"),
            )
            .with_help(format!(
                "write `{fix}`, or `if let x: {} = {op} {{ .. }}`",
                ctx.ty_name(&some)
            ))
            .with_fix(span, fix.clone(), format!("use `{fix}`")),
        );
        Some(poison(span))
    };
    if kind == TryKind::Plain {
        ctx.try_depth += 1;
        let e = lower_expected(ctx, operand, expected);
        ctx.try_depth -= 1;
        if let Some(p) = optional(ctx, &e) {
            return p;
        }
        warn_if_idle(ctx, &e);
        return e;
    }
    // `let x: i32 | none = try? f()` - the operand's slot is the payload.
    let inner_expected = match (kind, optional_parts(&expected)) {
        (TryKind::Optional, Some((_, some))) => super::desugar::without_none(&some),
        (TryKind::Optional, None) if expected != HirTy::Unit => HirTy::Unknown,
        _ => expected.clone(),
    };
    if kind == TryKind::Force {
        main_frame(ctx);
    } else {
        catch_all_frame(ctx);
    }
    ctx.try_depth += 1;
    let body = lower_expected(ctx, operand, inner_expected);
    ctx.try_depth -= 1;
    if kind == TryKind::Force {
        if let Some(p) = optional(ctx, &body) {
            pop_catch(ctx);
            return p;
        }
    }
    warn_if_idle(ctx, &body);
    let frame = pop_catch(ctx);
    if kind == TryKind::Force {
        if !frame.hit {
            return body;
        }
        let report = force_report(ctx, &frame, span);
        let ty = body.ty.clone();
        return shell(ctx, &frame, body, ty, (report, None), span);
    }
    dropped_resources(ctx, &frame, span);
    // `try?` in statement position just drops the error; elsewhere a
    // `()` operand gives `() | none` (spec 3.14.3).
    if expected == HirTy::Unit {
        if !frame.hit {
            return body;
        }
        return shell(ctx, &frame, body, HirTy::Unit, (Vec::new(), None), span);
    }
    // `T?` with the operand as `T` (spec 2.6.4): an optional operand
    // nests, so a thrown error and a returned `none` stay apart.
    let result_ty = if body.ty == HirTy::Unknown {
        body.ty.clone()
    } else {
        HirTy::sum([body.ty.clone(), HirTy::None])
    };
    let body = wrap_into_sum(&result_ty, body);
    if !frame.hit {
        return body;
    }
    let none = sum_none(&result_ty, span);
    shell(ctx, &frame, body, result_ty, (Vec::new(), Some(none)), span)
}

/// `try?` drops the error it catches: one that can hold a value whose
/// close can fail would leave it open (stdlib 2.13.4).
fn dropped_resources(ctx: &LowerCtx<'_>, frame: &CatchFrame, span: rokc_diagnostic::Span) {
    for ety in &frame.raised {
        if super::affine::affine(ctx, ety) != Some(super::affine::Kind::Close) {
            continue;
        }
        let t = ctx.ty_name(ety);
        ctx.emit_diag(
            rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Handoff,
                span,
                format!(
                    "`try?` drops the `{t}` it catches, which can hold a value that must be \
                     closed (stdlib 2.13.4)"
                ),
            )
            .with_help(format!(
                "catch it instead: `try .. catch e {{ .. }}`, take the value out of the `{t}` \
                 and close it (`try x.close();`)"
            )),
        );
    }
}

/// Wrap a throwing `main`'s body (spec 4.16.3): an error escaping it
/// prints a message and exits with status 1.
pub(super) fn uncaught_in_main(
    ctx: &mut LowerCtx<'_>,
    block: HirBlock,
    span: rokc_diagnostic::Span,
) -> HirBlock {
    let frame = pop_catch(ctx);
    if !frame.hit {
        return block;
    }
    let ty = if block.trailing.is_some() {
        ctx.fn_ret_ty.clone()
    } else {
        HirTy::Unit
    };
    let body = HirExpr {
        kind: HirExprKind::Block(Box::new(block)),
        ty: ty.clone(),
        span,
    };
    // `if #which == 0 { report(describe(#err0)) } ... report(describe(#errN))`
    let Some(sig) = ctx.env.sig("rok_uncaught_error_at").cloned() else {
        return HirBlock {
            stmts: vec![HirStmt::Expr(ctx.error_expr(
                span,
                "internal: runtime hook `rok_uncaught_error_at` is not declared".to_owned(),
            ))],
            trailing: None,
            label: None,
        };
    };
    let site = hir_local(
        frame.site.expect("main frame records sites"),
        &HirTy::Prim(PrimTy::Str),
        span,
    );
    let report = per_error(ctx, &frame, span, |_, msg| {
        HirStmt::Panic(HirExpr {
            kind: HirExprKind::Call {
                callee: sig.id,
                args: vec![msg, site.clone()],
            },
            ty: HirTy::Unit,
            span,
        })
    });
    let valued = ty != HirTy::Unit;
    let wrapped = shell(ctx, &frame, body, ty, (report, None), span);
    // A unit / diverging body stays a statement: falling off the end of
    // an `i32` main is not a unit return.
    let (stmts, trailing) = if valued {
        (Vec::new(), Some(wrapped))
    } else {
        (vec![HirStmt::Expr(wrapped)], None)
    };
    HirBlock {
        stmts,
        trailing,
        label: None,
    }
}

/// A failed `try!`'s panic (spec 3.14.8): the error as an uncaught one
/// prints (4.16.4) and where it was thrown; the panic's own position is
/// the `try!`.
fn force_report(
    ctx: &mut LowerCtx<'_>,
    frame: &CatchFrame,
    span: rokc_diagnostic::Span,
) -> Vec<HirStmt> {
    let site = hir_local(
        frame.site.expect("try! frame records sites"),
        &HirTy::Prim(PrimTy::Str),
        span,
    );
    per_error(ctx, frame, span, |ctx, msg| {
        let text = concat_strs(
            ctx,
            &[
                Piece::Text("try! failed: "),
                Piece::Value(msg),
                Piece::Text("\\n  raised at "),
                Piece::Value(site.clone()),
            ],
            span,
        );
        let call = hook_call(ctx, "rok_panic_at", "", span);
        let HirExprKind::Call { callee, .. } = call.kind else {
            return HirStmt::Panic(call);
        };
        HirStmt::Panic(HirExpr {
            kind: HirExprKind::Call {
                callee,
                args: vec![text],
            },
            ty: HirTy::Unit,
            span,
        })
    })
}

/// `if #which == 0 { report(describe(#err0)) } ... report(describe(#errN))`
/// over the errors `frame` parked.
fn per_error(
    ctx: &mut LowerCtx<'_>,
    frame: &CatchFrame,
    span: rokc_diagnostic::Span,
    mut report_one: impl FnMut(&mut LowerCtx<'_>, HirExpr) -> HirStmt,
) -> Vec<HirStmt> {
    let which = frame
        .arms
        .as_ref()
        .map(|a| a.1)
        .expect("frame parks errors");
    let mut report = Vec::new();
    for (k, (ety, local)) in frame.errs.iter().enumerate() {
        let msg = describe_err(ctx, hir_local(*local, ety, span), span);
        let call = report_one(ctx, msg);
        if k + 1 == frame.errs.len() {
            report.push(call);
            break;
        }
        let cond = hir_binop(
            PrimOp::Eq,
            hir_local(which, &HirTy::Prim(PrimTy::I32), span),
            hir_int(
                i128::try_from(k).expect("error index fits"),
                PrimTy::I32,
                span,
            ),
            HirTy::Prim(PrimTy::Bool),
            span,
        );
        report.push(HirStmt::Expr(HirExpr {
            kind: HirExprKind::If {
                cond: Box::new(cond),
                then_block: Box::new(HirBlock {
                    stmts: vec![call],
                    trailing: None,
                    label: None,
                }),
                else_branch: None,
            },
            ty: HirTy::Unit,
            span,
        }));
    }
    report
}

/// Push the frame of a throwing `main`'s body: it takes every error and
/// parks it for the report.
pub(super) fn main_frame(ctx: &mut LowerCtx<'_>) {
    let which = ctx.declare(None, HirTy::Prim(PrimTy::I32));
    let site = ctx.declare(None, HirTy::Prim(PrimTy::Str));
    let label = fresh_label(ctx);
    ctx.err_frames.push(ErrFrame::Catch(CatchFrame {
        label,
        arms: Some((vec![None], which)),
        errs: Vec::new(),
        hit: false,
        raised: Vec::new(),
        site: Some(site),
    }));
}

/// Spec 4.16.4: an uncaught error prints as its `Display`, else its
/// `Debug`, else its type name.
fn describe_err(ctx: &mut LowerCtx<'_>, err: HirExpr, span: rokc_diagnostic::Span) -> HirExpr {
    if ctx.has_trait(&err.ty, ctx.lang("display_trait")) {
        return format_value(ctx, err, false, span);
    }
    if ctx.has_trait(&err.ty, ctx.lang("debug_trait")) {
        return format_value(ctx, err, true, span);
    }
    HirExpr {
        kind: HirExprKind::Const(ConstVal::Str(ctx.ty_name(&err.ty))),
        ty: HirTy::Prim(PrimTy::Str),
        span,
    }
}

/// `{ let #eN = v; f"{#eN}" }`, `{#eN:?}` when `debug`; the caller checks
/// the trait.
pub(super) fn format_value(
    ctx: &mut LowerCtx<'_>,
    v: HirExpr,
    debug: bool,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let spec = if debug {
        vec![rokc_ast::SpecPiece::Text("?".to_owned())]
    } else {
        Vec::new()
    };
    ctx.push_scope();
    let name = format!("#e{}", ctx.locals.len());
    let local = ctx.declare(Some(name.clone()), v.ty.clone());
    let hole = rokc_ast::FStringPart::Hole {
        expr: Expr {
            kind: ExprKind::Path(AstPath {
                segments: vec![rokc_ast::PathSegment {
                    name,
                    generics: Vec::new(),
                }],
                span,
                origin: None,
            }),
            span,
        },
        spec,
    };
    let text = super::fstring::lower_fstring(ctx, &[hole], span);
    ctx.pop_scope();
    HirExpr {
        ty: HirTy::Prim(PrimTy::Str),
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![HirStmt::Let { local, init: v }],
            trailing: Some(text),
            label: None,
        })),
        span,
    }
}

/// A piece of [`concat_strs`]: literal text as an f-string writes it, or
/// a `str` value.
enum Piece<'a> {
    Text(&'a str),
    Value(HirExpr),
}

/// `{ let #s0 = v0; ...; f"text{#s0}..." }`.
fn concat_strs(
    ctx: &mut LowerCtx<'_>,
    pieces: &[Piece<'_>],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    ctx.push_scope();
    let mut stmts = Vec::new();
    let mut parts = Vec::new();
    for p in pieces {
        match p {
            Piece::Text(t) => parts.push(rokc_ast::FStringPart::Lit((*t).to_owned())),
            Piece::Value(v) => {
                let name = format!("#s{}", ctx.locals.len());
                let local = ctx.declare(Some(name.clone()), HirTy::Prim(PrimTy::Str));
                stmts.push(HirStmt::Let {
                    local,
                    init: v.clone(),
                });
                parts.push(rokc_ast::FStringPart::Hole {
                    expr: Expr {
                        kind: ExprKind::Path(AstPath {
                            segments: vec![rokc_ast::PathSegment {
                                name,
                                generics: Vec::new(),
                            }],
                            span,
                            origin: None,
                        }),
                        span,
                    },
                    spec: Vec::new(),
                });
            }
        }
    }
    let text = super::fstring::lower_fstring(ctx, &parts, span);
    ctx.pop_scope();
    HirExpr {
        ty: HirTy::Prim(PrimTy::Str),
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: Some(text),
            label: None,
        })),
        span,
    }
}

/// The ok side of the current function's carrier.
fn carrier_ok(ctx: &LowerCtx<'_>) -> HirTy {
    // A coroutine body's errors ride in its step carrier.
    if let Some(c) = &ctx.coro {
        return c.step_ty(ctx, &ctx.fn_ret_ty);
    }
    ctx.fn_ret_ty.clone()
}

/// A throwing function's body yields the ok carrier.
pub(super) fn finish_throwing_body(ctx: &LowerCtx<'_>, block: &mut HirBlock) {
    let Some(err) = ctx.fn_throws.clone() else {
        return;
    };
    let ok = carrier_ok(ctx);
    match block.trailing.take() {
        Some(t) if hir_expr_diverges(&t) => block.trailing = Some(t),
        Some(t) => {
            let span = t.span;
            block.trailing = Some(carrier(&ok, &err, Ok(t), span));
        }
        None if hir_block_diverges(block) => {}
        None => {
            let span = rokc_diagnostic::Span::DUMMY;
            block.trailing = Some(carrier(&ok, &err, Ok(unit_value(span)), span));
        }
    }
}

/// Reject `return`, `break` and `continue` that leave `fin`: they would
/// drop an error in flight. Jumps inside a closure, or to a loop or labelled
/// block within `fin`, stay.
fn finally_jumps(ctx: &LowerCtx<'_>, fin: &Block) {
    #[derive(Default)]
    struct Jumps {
        bodies: u32,
        loops: Vec<(rokc_diagnostic::Span, Option<String>)>,
        blocks: Vec<(rokc_diagnostic::Span, String)>,
        jumps: Vec<(rokc_diagnostic::Span, &'static str, Option<String>)>,
    }
    impl crate::walk::AstVisitor for Jumps {
        fn expr(&mut self, e: &Expr) {
            match &e.kind {
                ExprKind::Loop { label, .. }
                | ExprKind::While { label, .. }
                | ExprKind::For { label, .. } => self.loops.push((e.span, label.clone())),
                ExprKind::Block(b) | ExprKind::Unsafe(b) => {
                    if let Some(l) = &b.label {
                        self.blocks.push((b.span, l.clone()));
                    }
                }
                _ => {}
            }
        }
        fn stmt(&mut self, s: &rokc_ast::Stmt) {
            if self.bodies > 0 {
                return;
            }
            let (what, label) = match &s.kind {
                StmtKind::Return(_) => ("return", None),
                StmtKind::Break { label, .. } => ("break", label.clone()),
                StmtKind::Continue { label } => ("continue", label.clone()),
                _ => return,
            };
            self.jumps.push((s.span, what, label));
        }
        fn enter_body(&mut self) {
            self.bodies += 1;
        }
        fn leave_body(&mut self) {
            self.bodies -= 1;
        }
    }
    let mut w = Jumps::default();
    crate::walk::walk_block(&mut w, fin);
    let within = |outer: rokc_diagnostic::Span, at: rokc_diagnostic::Span| {
        outer.start <= at.start && at.end <= outer.end
    };
    for (at, what, label) in w.jumps {
        let stays = what != "return"
            && match &label {
                None => w.loops.iter().any(|(s, _)| within(*s, at)),
                Some(l) => {
                    w.loops
                        .iter()
                        .any(|(s, ll)| ll.as_deref() == Some(l.as_str()) && within(*s, at))
                        || w.blocks.iter().any(|(s, bl)| bl == l && within(*s, at))
                }
            };
        if stays {
            continue;
        }
        ctx.emit_error_help(
            at,
            format!("`{what}` cannot leave a `finally` block: it would drop the error in flight"),
            format!("move the `{what}` after the `try` expression; `finally` only cleans up"),
        );
    }
}

/// Postfix `body catch pat { .. } ... finally { .. }`.
pub(super) fn lower_catch(
    ctx: &mut LowerCtx<'_>,
    body: &Expr,
    arms: &[rokc_ast::CatchArm],
    finally: Option<&Block>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let expected = ctx.current_expected().cloned().unwrap_or(HirTy::Unknown);
    if let Some(fin) = finally {
        finally_jumps(ctx, fin);
        ctx.err_frames.push(ErrFrame::Finally {
            block: fin.clone(),
            hir: Vec::new(),
            try_depth: ctx.try_depth,
            breakables: ctx.breakables.len(),
        });
    }
    let value = if arms.is_empty() {
        lower_expected(ctx, body, expected)
    } else {
        lower_catch_arms(ctx, body, arms, &expected, span)
    };
    if finally.is_none() {
        return value;
    }
    let Some(ErrFrame::Finally { block, .. }) = ctx.err_frames.pop() else {
        unreachable!("frames are pushed and popped in pairs")
    };
    if hir_expr_diverges(&value) {
        return value;
    }
    // Normal completion (body or handler value): run `finally`, then
    // yield the value.
    ctx.push_expected(HirTy::Unit);
    let fin = lower_block(ctx, &block);
    ctx.pop_expected();
    let ty = value.ty.clone();
    let mut stmts = Vec::new();
    let trailing = if ty == HirTy::Unit {
        stmts.push(HirStmt::Expr(value));
        None
    } else {
        let t = ctx.declare(None, ty.clone());
        stmts.push(HirStmt::Let {
            local: t,
            init: value,
        });
        Some(hir_local(t, &ty, span))
    };
    stmts.push(HirStmt::Expr(HirExpr {
        kind: HirExprKind::Block(Box::new(fin)),
        ty: HirTy::Unit,
        span,
    }));
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

fn lower_catch_arms(
    ctx: &mut LowerCtx<'_>,
    body: &Expr,
    arms: &[rokc_ast::CatchArm],
    expected: &HirTy,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let filters: Vec<Option<HirTy>> = arms.iter().map(|a| arm_filter(ctx, &a.pat)).collect();
    let which = ctx.declare(None, HirTy::Prim(PrimTy::I32));
    let label = fresh_label(ctx);
    ctx.err_frames.push(ErrFrame::Catch(CatchFrame {
        label,
        arms: Some((filters.clone(), which)),
        errs: Vec::new(),
        hit: false,
        raised: Vec::new(),
        site: None,
    }));
    let body_h = lower_expected(ctx, body, expected.clone());
    let frame = pop_catch(ctx);
    // Arms no raised error type can take never run, but are still
    // type-checked (and flagged).
    let check_hint = if *expected == HirTy::Unknown && !hir_expr_diverges(&body_h) {
        body_h.ty.clone()
    } else {
        expected.clone()
    };
    if frame.errs.is_empty() {
        ctx.emit_warning("no error reaches this `catch`: its body can't throw".to_owned());
    }
    for (arm, filter) in arms.iter().zip(&filters) {
        if frame.errs.iter().any(|(t, _)| may_take(filter.as_ref(), t)) {
            continue;
        }
        if !frame.errs.is_empty() {
            ctx.emit_warning(format!(
                "this `catch` arm never runs: no `{}` error reaches it",
                filter
                    .as_ref()
                    .map_or_else(|| "_".to_owned(), |f| ctx.ty_name(f))
            ));
        }
        check_dead_arm(ctx, arm, filter.as_ref(), check_hint.clone(), span);
    }
    if frame.errs.is_empty() {
        return body_h;
    }
    // Dispatch on the parked error, built as AST so arms lower like any
    // `match`: `if #which == k { match #err_k { <arms for E_k>, _ => throw
    // #err_k } } else ..`.
    ctx.next_synth += 1;
    let n = ctx.next_synth;
    ctx.push_scope();
    let which_name = format!("#which{n}");
    bind_name(ctx, &which_name, which);
    let mut branches = Vec::with_capacity(frame.errs.len());
    for (k, (ety, local)) in frame.errs.iter().enumerate() {
        let name = format!("#err{n}_{k}");
        bind_name(ctx, &name, *local);
        branches.push(dispatch_one(ctx, arms, &filters, ety, &name, span));
    }
    let mut chain = branches.pop().expect("at least one parked error");
    for (k, b) in branches.into_iter().enumerate().rev() {
        chain = Expr {
            kind: ExprKind::If {
                cond: Box::new(Expr {
                    kind: ExprKind::OpInfix {
                        op: "==".to_owned(),
                        lhs: Box::new(path_expr(&which_name, span)),
                        rhs: Box::new(Expr {
                            kind: ExprKind::Literal(AstLit::Int(k.to_string())),
                            span,
                        }),
                    },
                    span,
                }),
                then_branch: Block {
                    stmts: Vec::new(),
                    trailing: Some(Box::new(b)),
                    span,
                    label: None,
                },
                else_branch: Some(Box::new(chain)),
            },
            span,
        };
    }
    let body_div = hir_expr_diverges(&body_h);
    let hint = if *expected != HirTy::Unknown || body_div {
        expected.clone()
    } else {
        body_h.ty.clone()
    };
    let handler = lower_expected(ctx, &chain, hint.clone());
    ctx.pop_scope();
    let result_ty = if hint == HirTy::Unknown {
        handler.ty.clone()
    } else {
        hint
    };
    let fit = |ctx: &mut LowerCtx<'_>, e: HirExpr, what: &'static str| {
        if result_ty == HirTy::Unit || hir_expr_diverges(&e) {
            e
        } else {
            coerce(ctx, &result_ty, e, &|| what.to_owned())
        }
    };
    let body_h = fit(ctx, body_h, "the `catch` body");
    let handler = fit(ctx, handler, "the `catch` handler");
    shell(
        ctx,
        &frame,
        body_h,
        result_ty,
        (Vec::new(), Some(handler)),
        span,
    )
}

/// Lower a `catch` arm no error reaches only for its diagnostics; result,
/// locals and raised errors are discarded.
fn check_dead_arm(
    ctx: &mut LowerCtx<'_>,
    arm: &rokc_ast::CatchArm,
    filter: Option<&HirTy>,
    hint: HirTy,
    span: rokc_diagnostic::Span,
) {
    let ety = filter.cloned().unwrap_or(HirTy::Unknown);
    let saved_frames = std::mem::take(&mut ctx.err_frames);
    let saved_calls = ctx.throwing_calls;
    let saved_locals = ctx.locals.len();
    catch_all_frame(ctx);
    ctx.push_scope();
    ctx.next_synth += 1;
    let name = format!("#dead{}", ctx.next_synth);
    let local = ctx.declare(None, ety.clone());
    bind_name(ctx, &name, local);
    let m = match &arm.pat.kind {
        // No type to match against: lower the body with the binding
        // poisoned (a `match` on an `Unknown` scrutinee checks nothing).
        AstPatternKind::Wildcard | AstPatternKind::Binding { sub: None, .. }
            if filter.is_none() =>
        {
            if let AstPatternKind::Binding { name, .. } = &arm.pat.kind {
                ctx.declare(Some(name.clone()), HirTy::Unknown);
            }
            Expr {
                kind: ExprKind::Block(arm.body.clone()),
                span: arm.body.span,
            }
        }
        _ => {
            let filters = [filter.cloned()];
            dispatch_one(ctx, std::slice::from_ref(arm), &filters, &ety, &name, span)
        }
    };
    let h = lower_expected(ctx, &m, hint.clone());
    if !matches!(hint, HirTy::Unit | HirTy::Unknown) && !hir_expr_diverges(&h) {
        let _ = coerce(ctx, &hint, h, &|| "the `catch` handler".to_owned());
    }
    ctx.pop_scope();
    ctx.err_frames = saved_frames;
    ctx.throwing_calls = saved_calls;
    // Its locals (some typed `Unknown`) belong to the dropped HIR.
    ctx.locals.truncate(saved_locals);
}

fn bind_name(ctx: &mut LowerCtx<'_>, name: &str, local: LocalId) {
    if let Some(scope) = ctx.scopes.last_mut() {
        scope.insert(name.to_owned(), local);
    }
}

pub(super) fn path_expr(name: &str, span: rokc_diagnostic::Span) -> Expr {
    Expr {
        kind: ExprKind::Path(AstPath {
            segments: vec![rokc_ast::PathSegment {
                name: name.to_owned(),
                generics: Vec::new(),
            }],
            span,
            origin: None,
        }),
        span,
    }
}

/// `match <err> { <arms that can take ety>, _ => throw <err> }`; the
/// rethrow arm is left out when the arms cover `ety`.
fn dispatch_one(
    ctx: &LowerCtx<'_>,
    arms: &[rokc_ast::CatchArm],
    filters: &[Option<HirTy>],
    ety: &HirTy,
    err_name: &str,
    span: rokc_diagnostic::Span,
) -> Expr {
    let mut m_arms: Vec<AstMatchArm> = arms
        .iter()
        .zip(filters)
        .filter(|(_, f)| may_take(f.as_ref(), ety))
        .map(|(a, f)| {
            let mut pat = a.pat.clone();
            let mut body = a.body.clone();
            // `catch e: A | B` sees the error variant by variant: take
            // the variant whole and rebind it as the arm's sum type.
            if let (AstPatternKind::TypeCheck { binder, ty }, Some(HirTy::Sum(vs))) =
                (&a.pat.kind, f)
            {
                if vs.contains(ety) {
                    pat.kind = AstPatternKind::Wildcard;
                    if let Some(name) = binder {
                        body.stmts.insert(
                            0,
                            Stmt {
                                kind: StmtKind::Let {
                                    name: name.clone(),
                                    ty: Some(ty.clone()),
                                    init: path_expr(err_name, span),
                                },
                                span,
                            },
                        );
                    }
                }
            }
            AstMatchArm {
                pat,
                guard: None,
                body: Expr {
                    span: body.span,
                    kind: ExprKind::Block(body),
                },
            }
        })
        .collect();
    if !covers(ctx, &m_arms, ety) {
        m_arms.push(AstMatchArm {
            pat: AstPattern {
                kind: AstPatternKind::Wildcard,
                span,
            },
            guard: None,
            body: Expr {
                kind: ExprKind::Block(Block {
                    stmts: vec![Stmt {
                        kind: StmtKind::Throw(path_expr(err_name, span)),
                        span,
                    }],
                    trailing: None,
                    span,
                    label: None,
                }),
                span,
            },
        });
    }
    Expr {
        kind: ExprKind::Match {
            scrutinee: Box::new(path_expr(err_name, span)),
            arms: m_arms,
        },
        span,
    }
}

/// Do `arms` cover every `ety`: a catch-all, its exact type, type patterns
/// over a sum's members, or every variant of an enum?
fn covers(ctx: &LowerCtx<'_>, arms: &[AstMatchArm], ety: &HirTy) -> bool {
    let irrefutable = |p: &AstPattern| {
        matches!(
            p.kind,
            AstPatternKind::Wildcard | AstPatternKind::Binding { sub: None, .. }
        )
    };
    let mut tys = Vec::new();
    let mut variants = Vec::new();
    for a in arms {
        match &a.pat.kind {
            _ if irrefutable(&a.pat) => return true,
            AstPatternKind::TypeCheck { ty, .. } => {
                let t = ctx.resolve_ast_ty(ty);
                if t == *ety || matches!(&t, HirTy::Sum(vs) if vs.contains(ety)) {
                    return true;
                }
                tys.push(t);
            }
            // `HttpError { code, .. }`: every value of that struct.
            AstPatternKind::Struct { fields, .. }
                if fields
                    .iter()
                    .all(|f| f.pat.as_ref().is_none_or(irrefutable)) =>
            {
                if let Some(t) = arm_filter(ctx, &a.pat) {
                    if t == *ety {
                        return true;
                    }
                    tys.push(t);
                }
            }
            AstPatternKind::EnumVariant { path, args } if args.iter().all(irrefutable) => {
                let segs: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
                if let EnumRes::Found(_, i) = resolve_enum_variant(ctx, &segs, Some(ety), None) {
                    variants.push(i);
                }
            }
            _ => {}
        }
    }
    match ety {
        HirTy::Sum(vs) => vs.iter().all(|v| tys.contains(v)),
        HirTy::Enum(eid) => (0..ctx.enum_def(*eid).variants.len()).all(|i| variants.contains(&i)),
        _ => false,
    }
}

/// The error type a catch arm's pattern names (`None`: any type).
pub(super) fn arm_filter(ctx: &LowerCtx<'_>, pat: &AstPattern) -> Option<HirTy> {
    match &pat.kind {
        AstPatternKind::TypeCheck { ty, .. } => Some(ctx.resolve_ast_ty(ty)),
        AstPatternKind::EnumVariant { path, .. } | AstPatternKind::Struct { path, .. } => {
            let segs: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
            if let Some(e) = ctx
                .enum_path(&segs)
                .and_then(|(full, _)| ctx.lookup_enum(&full))
            {
                return Some(HirTy::Enum(e));
            }
            ctx.canon_type(&segs)
                .and_then(|full| ctx.lookup_struct(&full))
                .map(HirTy::Struct)
        }
        _ => None,
    }
}

/// The `thunks` key of a `kind` adapter of `target` (a captured callee:
/// `None`) into `slot`.
pub(super) fn thunk_key(
    kind: &str,
    target: Option<FunctionId>,
    slot: &HirTy,
    callee: &HirTy,
) -> String {
    format!(
        "{kind}|{target:?}|{}|{}",
        ty_mangle(slot),
        ty_mangle(callee)
    )
}
