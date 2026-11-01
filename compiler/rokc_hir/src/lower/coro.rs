//! Coroutines (spec 4.11 generators, 4.12 async, 4.13 streams).
//!
//! A coroutine block (`yield { .. }`, `async { .. }`, `async yield { .. }`)
//! is lifted like a closure into a body function (`is_generator` /
//! `is_async`) returning the *step* one resume hands back: `T | none`,
//! `Poll<T>` or `Poll<T | none>`, inside `throws` when it may throw.
//! `yield` and a pending `await` become `Suspend`; MIR (`rokc_mir::coro`)
//! makes the state machine. The block's value is a library wrapper over
//! `CoroNew` (a function value whose env is the frame):
//!
//! | form            | wrapper (`throws (E)` form)                       |
//! |-----------------|---------------------------------------------------|
//! | `yield`         | `iter::GenIter<T>` (`TryGenIter<T, E>`)           |
//! | `async`         | `future::AsyncFuture<T>` (`TryAsyncFuture<T, E>`) |
//! | `async yield`   | `stream::AsyncStream<T>` (`TryAsyncStream<T, E>`) |
//!
//! `yield fn` / `async fn` become a plain `fn` returning such a block
//! (labelled `FN_BLOCK_LABEL`); `yield ||` / `async ||` closures return one.
//!
//! The body function:
//! ```text
//! fn __coro_N(__env?, #ctx?) -> step {
//!     <captures>
//!     let #r = if true { '#coro: { body } } else { undef };
//!     suspend_final <final step: none / Ready(#r) / Ready(none)>;
//! }
//! ```
//! `return v` breaks out of `'#coro`; an escaping error returns the carrier
//! (MIR parks the coroutine). `if true` keeps the final point reachable
//! when the body diverges.

use rokc_ast::{Block, CallArg, Expr, ExprKind};
use rokc_diagnostic::Code;
use rokc_ir_common::{ConstVal, PrimTy};

use super::desugar::{closure_ctx, may_throw};
use super::expr::{hir_local, method_call_on, pick_method};
use super::throws::{carrier, unit_value};
use super::{LowerCtx, lower_expected, wrap_into_sum};
use crate::{
    HirBlock, HirExpr, HirExprKind, HirFunction, HirMatchArm, HirParam, HirPattern, HirStmt, HirTy,
};

/// Label of the block the driver wraps a `yield fn` / `async fn` body in
/// (its params are then the function's own, assignable).
pub const FN_BLOCK_LABEL: &str = "#coro_fn";
/// Body label of an `async ||` / `yield ||` closure: its `[ref x]` cells
/// pass into the coroutine, so every call's future shares them (4.6.2).
const CLOSURE_BLOCK_LABEL: &str = "#coro_closure";
const BODY_LABEL: &str = "#coro";
const CTX: &str = "#ctx";
/// A generator's resume argument: `true` from `GenIter::close`, which
/// leaves the body through its cleanups at the `yield` it waits at.
const CLOSE: &str = "#close";
/// Set in an async frame once a cancellation point fired: its cleanups
/// run, and no `await` in them checks for the cancel again.
const UNWINDING: &str = "#unwinding";
const CONTEXT: &str = "rok::core::future::Context";
const POLL: &str = "rok::core::future::Poll";
const PRIME_FRAME: &str = "rok::core::future::prime_frame";
const PRIME_STARTED: &str = "rok::core::future::prime_started";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CoroKind {
    Gen,
    Async,
    Stream,
}

impl CoroKind {
    const fn wrapper(self, throws: bool) -> &'static str {
        match (self, throws) {
            (Self::Gen, false) => "rok::core::iter::GenIter",
            (Self::Gen, true) => "rok::core::iter::TryGenIter",
            (Self::Async, false) => "rok::core::future::AsyncFuture",
            (Self::Async, true) => "rok::core::future::TryAsyncFuture",
            (Self::Stream, false) => "rok::core::stream::AsyncStream",
            (Self::Stream, true) => "rok::core::stream::TryAsyncStream",
        }
    }
    const fn is_async(self) -> bool {
        !matches!(self, Self::Gen)
    }
    const fn yields(self) -> bool {
        !matches!(self, Self::Async)
    }
}

/// The coroutine a body is being lowered for.
#[derive(Clone, Debug)]
pub(super) struct CoroCtx {
    pub kind: CoroKind,
    /// Item type of a generator / stream (`Unknown` until the first
    /// `yield` fixes it).
    pub item: HirTy,
    /// Probe pass: types are still being inferred, build no step values.
    pub probe: bool,
}

impl CoroCtx {
    /// The step type: what one resume hands back (without the carrier).
    pub fn step_ty(&self, ctx: &LowerCtx<'_>, out: &HirTy) -> HirTy {
        let opt = HirTy::sum([self.item.clone(), HirTy::None]);
        match self.kind {
            CoroKind::Gen => opt,
            CoroKind::Async => poll_ty(ctx, out.clone()),
            CoroKind::Stream => poll_ty(ctx, opt),
        }
    }
}

fn poll_ty(ctx: &LowerCtx<'_>, t: HirTy) -> HirTy {
    ctx.instantiate_enum(POLL, vec![t], ctx.here())
        .map_or(HirTy::Unknown, HirTy::Enum)
}

/// `Poll::<variant>(args)` for `poll` = `Poll<..>`.
fn poll_variant(ctx: &LowerCtx<'_>, poll: &HirTy, name: &str, args: Vec<HirExpr>) -> HirExpr {
    let span = rokc_diagnostic::Span::DUMMY;
    let HirTy::Enum(id) = poll else {
        return super::poison(span);
    };
    let idx = ctx
        .enum_def(*id)
        .variants
        .iter()
        .position(|v| v.name == name)
        .expect("Poll variant");
    HirExpr {
        kind: HirExprKind::EnumVariantInit {
            enum_id: *id,
            variant_index: u32::try_from(idx).expect("variant index"),
            args,
        },
        ty: poll.clone(),
        span,
    }
}

/// `#name` as an AST path (a local the lowering declared).
fn local_path(name: &str, span: rokc_diagnostic::Span) -> Expr {
    Expr {
        kind: ExprKind::Path(rokc_ast::Path {
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

/// What a hand-off's poll (`#ctx.prime`) of a frame that has not started
/// does, before parking: the frame takes copies of the collections and
/// futures it captured, which the sender still holds (stdlib 2.13.9),
/// `(a, b) = prime_frame((a, b), #ctx, cells)`, through the hand-off's
/// memo; `cells`: a `[ref x]` capture, which no copy may have.
fn prime_captures(
    c: &mut LowerCtx<'_>,
    captures: &[(String, rokc_ir_common::LocalId, HirTy, bool)],
    span: rokc_diagnostic::Span,
) -> Vec<HirStmt> {
    let Some(def) = c.env.generic_fn(PRIME_FRAME) else {
        return Vec::new();
    };
    let copied: Vec<&String> = captures
        .iter()
        .filter(|(_, _, ty, by_ref)| !by_ref && super::expr::transfer_needs(c, ty, 0))
        .map(|(name, ..)| name)
        .collect();
    let locals: Vec<_> = copied.iter().filter_map(|n| c.lookup(n)).collect();
    if locals.len() != copied.len() {
        return Vec::new();
    }
    let cells = captures.iter().any(|(.., by_ref)| *by_ref);
    let tuple = Expr {
        kind: ExprKind::Tuple(copied.iter().map(|n| local_path(n, span)).collect()),
        span,
    };
    let flag = Expr {
        kind: ExprKind::Literal(rokc_ast::Literal::Bool(cells)),
        span,
    };
    let args = [
        CallArg::Positional(tuple),
        CallArg::Positional(local_path(CTX, span)),
        CallArg::Positional(flag),
    ];
    let value = super::expr::call_generic(c, def, None, None, &args, PRIME_FRAME, span);
    if locals.is_empty() {
        return vec![HirStmt::Expr(value)];
    }
    let ty = value.ty.clone();
    let all = c.declare(None, ty.clone());
    let mut out = vec![HirStmt::Let {
        local: all,
        init: value,
    }];
    for (i, local) in locals.into_iter().enumerate() {
        let field_ty = c.local_ty(local);
        out.push(HirStmt::Assign {
            local,
            value: HirExpr {
                kind: HirExprKind::TupleField {
                    target: Box::new(hir_local(all, &ty, span)),
                    index: u32::try_from(i).expect("capture index overflow"),
                    field_ty: field_ty.clone(),
                },
                ty: field_ty,
                span,
            },
        });
    }
    out
}

/// What a hand-off's poll of a frame parked at an `await` does: report
/// that it started (`prime_started((), #ctx)`), so the hand-off fails.
fn prime_started(c: &mut LowerCtx<'_>, span: rokc_diagnostic::Span) -> Vec<HirStmt> {
    let Some(def) = c.env.generic_fn(PRIME_STARTED) else {
        return Vec::new();
    };
    let unit = Expr {
        kind: ExprKind::Tuple(Vec::new()),
        span,
    };
    let args = [
        CallArg::Positional(unit),
        CallArg::Positional(local_path(CTX, span)),
    ];
    let call = super::expr::call_generic(c, def, None, None, &args, PRIME_STARTED, span);
    vec![HirStmt::Expr(call)]
}

/// `#ctx.prime`: a hand-off's poll, which runs none of the body.
fn primed(c: &LowerCtx<'_>, span: rokc_diagnostic::Span) -> Option<HirExpr> {
    let cx = c.lookup(CTX)?;
    let HirTy::Struct(sid) = c.local_ty(cx) else {
        return None;
    };
    let index = c
        .struct_def(sid)
        .fields
        .iter()
        .position(|f| f.name == "prime")?;
    let bool_ty = HirTy::Prim(PrimTy::Bool);
    Some(HirExpr {
        kind: HirExprKind::FieldRead {
            target: Box::new(hir_local(cx, &HirTy::Struct(sid), span)),
            struct_id: sid,
            field_index: u32::try_from(index).expect("field index"),
            field_ty: bool_ty.clone(),
        },
        ty: bool_ty,
        span,
    })
}

/// The `Pending` step of the future being lowered.
fn pending_step(c: &LowerCtx<'_>) -> Option<HirExpr> {
    let coro = c.coro.as_ref().filter(|k| !k.probe)?;
    let poll = coro.step_ty(c, &c.fn_ret_ty);
    Some(step(c, poll_variant(c, &poll, "Pending", Vec::new())))
}

/// `while #ctx.prime { first; suspend Pending }`: a future parks there
/// through a hand-off's poll.
fn parked(
    c: &LowerCtx<'_>,
    first: Vec<HirStmt>,
    pending: HirExpr,
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    let mut stmts = first;
    stmts.push(HirStmt::Expr(suspend(pending, false, span)));
    Some(HirExpr {
        kind: HirExprKind::While {
            label: None,
            cond: Box::new(primed(c, span)?),
            body: Box::new(HirBlock {
                stmts,
                trailing: None,
                label: None,
            }),
        },
        ty: HirTy::Unit,
        span,
    })
}

/// A step value, wrapped into the ok carrier when the body throws.
fn step(ctx: &LowerCtx<'_>, value: HirExpr) -> HirExpr {
    match ctx.fn_throws.clone() {
        Some(err) => carrier(&value.ty.clone(), &err, Ok(value), value_span()),
        None => value,
    }
}

const fn value_span() -> rokc_diagnostic::Span {
    rokc_diagnostic::Span::DUMMY
}

fn suspend(value: HirExpr, is_final: bool, span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Suspend {
            value: Box::new(value),
            is_final,
        },
        ty: HirTy::Unit,
        span,
    }
}

/// `return v` inside a coroutine: leave the body with `v`.
pub(super) fn coro_return(value: Option<HirExpr>) -> HirStmt {
    HirStmt::Break {
        label: Some(BODY_LABEL.to_owned()),
        value,
    }
}

/// `yield e;` (spec 4.11.2).
pub(super) fn lower_yield(
    ctx: &mut LowerCtx<'_>,
    value: Option<&Expr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let Some(coro) = ctx.coro.clone().filter(|c| c.kind.yields()) else {
        return ctx.error_expr(
            Code::OUTSIDE_CONTEXT,
            span,
            "`yield` outside a generator: use it in a `yield fn` / `yield { .. }` / \
             `async yield` body (spec 4.11)"
                .to_owned(),
        );
    };
    let v = match value {
        Some(e) => {
            let v = lower_expected(ctx, e, coro.item.clone());
            if coro.item == HirTy::Unknown {
                v
            } else {
                super::coerce(ctx, &coro.item, v, &|| "the yielded value".to_owned())
            }
        }
        None => unit_value(span),
    };
    if coro.item == HirTy::Unknown && v.ty != HirTy::Unknown {
        if let Some(c) = &mut ctx.coro {
            c.item = v.ty.clone();
        }
    }
    let coro = ctx.coro.clone().expect("in a coroutine");
    if coro.probe {
        return HirExpr {
            kind: HirExprKind::Block(Box::new(HirBlock {
                stmts: vec![HirStmt::Expr(v)],
                trailing: None,
                label: None,
            })),
            ty: HirTy::Unit,
            span,
        };
    }
    let opt = HirTy::sum([coro.item.clone(), HirTy::None]);
    let some = wrap_into_sum(&opt, v);
    let value = match coro.kind {
        CoroKind::Stream => {
            let poll = poll_ty(ctx, opt);
            poll_variant(ctx, &poll, "Ready", vec![some])
        }
        _ => some,
    };
    let point = suspend(step(ctx, value), false, span);
    let Some(closing) = local_bool(ctx, CLOSE, span) else {
        return point;
    };
    // `if !#close { suspend } if #close { leave }`: a `yield` reached while
    // closing ends the generator too.
    let exit = super::throws::exit_through(ctx, None, None, coro_return, span);
    let not_closing = if_else(
        closing.clone(),
        Vec::new(),
        vec![HirStmt::Expr(point)],
        span,
    );
    let leave = if_else(closing, vec![exit], Vec::new(), span);
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![HirStmt::Expr(not_closing), HirStmt::Expr(leave)],
            trailing: None,
            label: None,
        })),
        ty: HirTy::Unit,
        span,
    }
}

/// The bool local `name` of the coroutine being lowered (`None` in the
/// probe pass).
fn local_bool(ctx: &LowerCtx<'_>, name: &str, span: rokc_diagnostic::Span) -> Option<HirExpr> {
    if ctx.coro.as_ref().is_none_or(|c| c.probe) {
        return None;
    }
    let l = ctx.lookup(name)?;
    Some(hir_local(l, &HirTy::Prim(PrimTy::Bool), span))
}

/// `if cond { then } else { other }` as a unit statement.
fn if_else(
    cond: HirExpr,
    then: Vec<HirStmt>,
    other: Vec<HirStmt>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let block = |stmts: Vec<HirStmt>| HirBlock {
        stmts,
        trailing: None,
        label: None,
    };
    HirExpr {
        kind: HirExprKind::If {
            cond: Box::new(cond),
            then_block: Box::new(block(then)),
            else_branch: (!other.is_empty()).then(|| {
                Box::new(HirExpr {
                    kind: HirExprKind::Block(Box::new(block(other))),
                    ty: HirTy::Unit,
                    span,
                })
            }),
        },
        ty: HirTy::Unit,
        span,
    }
}

/// `rok::core::task::<name>(#ctx)`, through its lang slot (`None` when
/// the core library lacks it).
fn task_call(ctx: &mut LowerCtx<'_>, name: &str, span: rokc_diagnostic::Span) -> Option<HirExpr> {
    let path = |p: rokc_ast::Path| Expr {
        kind: ExprKind::Path(p),
        span,
    };
    let local = rokc_ast::Path {
        segments: vec![rokc_ast::PathSegment {
            name: CTX.to_owned(),
            generics: Vec::new(),
        }],
        span,
        origin: None,
    };
    let call = Expr {
        kind: ExprKind::Call {
            callee: Box::new(path(super::expr::lang_fn("task", name, span))),
            args: vec![CallArg::Positional(path(local))],
            trailing_closure: None,
        },
        span,
    };
    let e = super::expr::lower_expr(ctx, &call);
    (e.ty != HirTy::Unknown).then_some(e)
}

/// `if #unwinding { task::<name>(#ctx) }`: tell the executor a frame left
/// by the cancel is in its cleanups (`cleanup_running`) or done with them
/// (`cleanup_done`).
fn cleanup_note(
    ctx: &mut LowerCtx<'_>,
    name: &str,
    span: rokc_diagnostic::Span,
) -> Option<HirStmt> {
    if !ctx.coro.as_ref().is_some_and(|c| c.kind.is_async()) {
        return None;
    }
    let unwinding = local_bool(ctx, UNWINDING, span)?;
    let call = task_call(ctx, name, span)?;
    Some(HirStmt::Expr(if_else(
        unwinding,
        vec![HirStmt::Expr(call)],
        Vec::new(),
        span,
    )))
}

/// Where an async frame ends, normally or by an escaping error.
pub(super) fn frame_end(ctx: &mut LowerCtx<'_>, span: rokc_diagnostic::Span) -> Option<HirStmt> {
    cleanup_note(ctx, "cleanup_done", span)
}

/// Name prefix of the local an `await` polls its future in.
pub(super) const AWAITED: &str = "#await";

/// `await e` (spec 4.12.2): poll `e` (or `e.into_future()`) until ready,
/// suspending with `Pending`. A throwing `poll` needs a `try` like any
/// throwing call.
pub(super) fn lower_await(
    ctx: &mut LowerCtx<'_>,
    e: &Expr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    // `let v: T = await e`: `e` is expected to be a future of `T` (an
    // `AsyncFuture<T>` hint, so `await s.collect()` picks `T`), not a `T`.
    let hint = ctx
        .current_expected()
        .filter(|t| !matches!(t, HirTy::Unknown | HirTy::Unit))
        .cloned()
        .and_then(|t| ctx.instantiate_struct(CoroKind::Async.wrapper(false), vec![t], ctx.here()))
        .map_or(HirTy::Unknown, HirTy::Struct);
    // An `async` method's `ref self` call made here writes its receiver
    // back once the future completes (`mono::ref_unpack`).
    let outer = (
        ctx.awaited.replace(e.span),
        std::mem::take(&mut ctx.await_write_back),
    );
    let fut = super::lower_expected(ctx, e, hint);
    let write_back = std::mem::replace(&mut ctx.await_write_back, outer.1);
    ctx.awaited = outer.0;
    if !write_back.is_empty() {
        return await_writing_back(ctx, fut, write_back, span);
    }
    let has_poll =
        |ctx: &LowerCtx<'_>, t: &HirTy| matches!(pick_method(ctx, t, "poll", None), Ok(Some(_)));
    let fut = if fut.ty == HirTy::Unknown || has_poll(ctx, &fut.ty) {
        fut
    } else {
        method_call_on(ctx, fut, "into_future", &[], span)
    };
    if fut.ty == HirTy::Unknown {
        return fut;
    }
    let fty = fut.ty.clone();
    let f = ctx.declare(Some(format!("{AWAITED}{}", ctx.locals.len())), fty.clone());
    let lp = poll_loop(ctx, f, &fty, "poll", span);
    HirExpr {
        ty: lp.ty.clone(),
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![HirStmt::Let {
                local: f,
                init: fut,
            }],
            trailing: Some(lp),
            label: None,
        })),
        span,
    }
}

/// `await` of an `async` method's future with its `ref self` write-back,
/// run however the await ends: ready, its error raised, or cancelled.
fn await_writing_back(
    ctx: &mut LowerCtx<'_>,
    fut: HirExpr,
    write_back: Vec<HirStmt>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let fty = fut.ty.clone();
    let f = ctx.declare(Some(format!("{AWAITED}{}", ctx.locals.len())), fty.clone());
    ctx.err_frames.push(super::throws::ErrFrame::Finally {
        block: Block {
            stmts: Vec::new(),
            trailing: None,
            span,
            label: None,
        },
        hir: write_back.clone(),
        try_depth: ctx.try_depth,
        breakables: ctx.breakables.len(),
    });
    let lp = poll_loop(ctx, f, &fty, "poll", span);
    ctx.err_frames.pop();
    let ty = lp.ty.clone();
    let mut stmts = vec![HirStmt::Let {
        local: f,
        init: fut,
    }];
    let trailing = if super::hir_expr_diverges(&lp) {
        stmts.push(HirStmt::Expr(lp));
        None
    } else if ty == HirTy::Unit {
        stmts.push(HirStmt::Expr(lp));
        stmts.extend(write_back);
        None
    } else {
        let v = ctx.declare(None, ty.clone());
        stmts.push(HirStmt::Let { local: v, init: lp });
        stmts.extend(write_back);
        Some(hir_local(v, &ty, span))
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

/// `loop { match f.<method>(ref #ctx) { Ready(v) => break v, Pending =>
/// suspend Pending } }`, `fut` in a local so a `ref self` poll persists.
pub(super) fn poll_loop(
    ctx: &mut LowerCtx<'_>,
    f: rokc_ir_common::LocalId,
    fty: &HirTy,
    method: &str,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if !ctx.coro.as_ref().is_some_and(|c| c.kind.is_async()) {
        return ctx.error_expr(
            Code::OUTSIDE_CONTEXT,
            span,
            "`await` outside an async body: use it in an `async fn` / `async { .. }` / \
             `async ||` body (spec 4.12.10)"
                .to_owned(),
        );
    }
    let ctx_arg = CallArg::Ref(Expr {
        kind: ExprKind::Path(rokc_ast::Path {
            segments: vec![rokc_ast::PathSegment {
                name: CTX.to_owned(),
                generics: Vec::new(),
            }],
            span,
            origin: None,
        }),
        span,
    });
    let polled = method_call_on(ctx, hir_local(f, fty, span), method, &[ctx_arg], span);
    let HirTy::Enum(pid) = polled.ty else {
        if polled.ty == HirTy::Unknown {
            return polled;
        }
        return ctx.error_expr(
            Code::COROUTINE,
            span,
            format!(
                "`{method}` must return `Poll<T>`, found `{}`",
                ctx.ty_name(&polled.ty)
            ),
        );
    };
    let def = ctx.enum_def(pid);
    let (Some(ready), Some(pending), Some(out)) = (
        def.variants.iter().position(|v| v.name == "Ready"),
        def.variants.iter().position(|v| v.name == "Pending"),
        def.generic_args.first().cloned(),
    ) else {
        return ctx.error_expr(
            Code::COROUTINE,
            span,
            "`await` needs a `Poll<T>`".to_owned(),
        );
    };
    let v = ctx.declare(None, out.clone());
    let coro = ctx.coro.clone().expect("in a coroutine");
    let pending_step = if coro.probe {
        unit_value(span)
    } else {
        let out_ty = ctx.fn_ret_ty.clone();
        let poll = coro.step_ty(ctx, &out_ty);
        step(ctx, poll_variant(ctx, &poll, "Pending", Vec::new()))
    };
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
            pat: HirPattern::EnumVariant {
                enum_id: pid,
                variant_index: u32::try_from(ready).expect("variant"),
                args: vec![HirPattern::Binding {
                    local: v,
                    ty: out.clone(),
                }],
            },
            guard: None,
            body: block(
                cancel_check(ctx, "cancel_left", span)
                    .into_iter()
                    .chain([HirStmt::Break {
                        label: None,
                        value: Some(hir_local(v, &out, span)),
                    }])
                    .collect(),
            ),
        },
        HirMatchArm {
            pat: HirPattern::EnumVariant {
                enum_id: pid,
                variant_index: u32::try_from(pending).expect("variant"),
                args: Vec::new(),
            },
            guard: None,
            body: block(
                cancel_check(ctx, "cancel_point", span)
                    .into_iter()
                    .chain(if coro.probe {
                        vec![HirStmt::Expr(pending_step)]
                    } else {
                        // Past a hand-off's poll too, before polling again.
                        let again = if coro.kind == CoroKind::Async {
                            let first = prime_started(ctx, span);
                            parked(ctx, first, pending_step.clone(), span)
                        } else {
                            None
                        };
                        [Some(suspend(pending_step, false, span)), again]
                            .into_iter()
                            .flatten()
                            .map(HirStmt::Expr)
                            .collect()
                    })
                    .collect(),
            ),
        },
    ];
    let m = HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(polled),
            arms,
        },
        ty: HirTy::Unit,
        span,
    };
    // Run on every resume too, before the poll.
    let note = cleanup_note(ctx, "cleanup_running", span);
    let lp = HirExpr {
        kind: HirExprKind::Loop {
            label: None,
            body: Box::new(HirBlock {
                stmts: note.into_iter().chain([HirStmt::Expr(m)]).collect(),
                trailing: None,
                label: None,
            }),
            result_ty: out.clone(),
        },
        ty: out.clone(),
        span,
    };
    // Check on entry, after a ready poll (`cancel_left`, not the bare flag:
    // a hand-written future's value, e.g. a received message, is kept and
    // the next check leaves) and before suspending (the awaited future is
    // polled first after a resume, so nested coroutines clean up first).
    // None fires in a frame already unwinding: its cleanups' awaits
    // complete.
    let Some(entry) = cancel_check(ctx, "cancel_point", span) else {
        return lp;
    };
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![entry],
            trailing: Some(lp),
            label: None,
        })),
        ty: out,
        span,
    }
}

/// A cancellation point (stdlib 2.18.4): `if !#unwinding &&
/// task::<check>(#ctx) { #unwinding = true; leave like a return }`,
/// running the cleanups passed; the result is a zero value the executor
/// drops. `None` in the probe pass.
fn cancel_check(
    ctx: &mut LowerCtx<'_>,
    check: &str,
    span: rokc_diagnostic::Span,
) -> Option<HirStmt> {
    let unwinding = local_bool(ctx, UNWINDING, span)?;
    let HirExprKind::Local(flag) = unwinding.kind else {
        return None;
    };
    let cond = task_call(ctx, check, span)?;
    if cond.ty != HirTy::Prim(PrimTy::Bool) {
        return None;
    }
    let out = ctx.fn_ret_ty.clone();
    let value = (out != HirTy::Unit).then_some(HirExpr {
        kind: HirExprKind::Const(ConstVal::Unit),
        ty: out,
        span,
    });
    let exit = super::throws::exit_through(ctx, None, value, coro_return, span);
    let set = HirStmt::Assign {
        local: flag,
        value: HirExpr {
            kind: HirExprKind::Const(ConstVal::Bool(true)),
            ty: HirTy::Prim(PrimTy::Bool),
            span,
        },
    };
    let fire = if_else(cond, vec![set, exit], Vec::new(), span);
    Some(HirStmt::Expr(if_else(
        unwinding,
        Vec::new(),
        vec![HirStmt::Expr(fire)],
        span,
    )))
}

/// `async || body` = `|| async { body }` (spec 4.11.5 / 4.12.5): the
/// closure body to lift (the body itself for a plain closure).
pub(super) fn closure_body(body: &Expr, is_async: bool, is_generator: bool) -> Expr {
    if !(is_async || is_generator) {
        return body.clone();
    }
    let block = match &body.kind {
        ExprKind::Block(b) => b.clone(),
        _ => Block {
            stmts: Vec::new(),
            trailing: Some(Box::new(body.clone())),
            span: body.span,
            label: None,
        },
    };
    // A labelled body (`'a: { .. }`) keeps its label on an inner block.
    let block = if block.label.is_some() {
        Block {
            stmts: Vec::new(),
            trailing: Some(Box::new(Expr {
                kind: ExprKind::Block(block),
                span: body.span,
            })),
            span: body.span,
            label: Some(CLOSURE_BLOCK_LABEL.to_owned()),
        }
    } else {
        Block {
            label: Some(CLOSURE_BLOCK_LABEL.to_owned()),
            ..block
        }
    };
    let kind = match (is_async, is_generator) {
        (true, true) => ExprKind::AsyncYieldBlock(block),
        (true, false) => ExprKind::AsyncBlock(block),
        _ => ExprKind::YieldBlock(block),
    };
    Expr {
        kind,
        span: body.span,
    }
}

/// A coroutine block expression.
pub(super) fn lower_coro_expr(
    ctx: &mut LowerCtx<'_>,
    kind: &ExprKind,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    match kind {
        ExprKind::AsyncBlock(b) => lower_coro_block(ctx, CoroKind::Async, b, span),
        ExprKind::YieldBlock(b) => lower_coro_block(ctx, CoroKind::Gen, b, span),
        ExprKind::AsyncYieldBlock(b) => lower_coro_block(ctx, CoroKind::Stream, b, span),
        _ => unreachable!("not a coroutine block"),
    }
}

/// `yield { .. }` / `async { .. }` / `async yield { .. }`.
fn lower_coro_block<'a>(
    ctx: &mut LowerCtx<'a>,
    kind: CoroKind,
    body: &Block,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let from_fn = body.label.as_deref() == Some(FN_BLOCK_LABEL);
    let from_closure = body.label.as_deref() == Some(CLOSURE_BLOCK_LABEL);
    let body_expr = Expr {
        kind: ExprKind::Block(Block {
            label: None,
            ..body.clone()
        }),
        span: body.span,
    };
    // Item / output and error types from the expected wrapper type, else
    // inferred.
    let (mut val_ty, mut throws) = (HirTy::Unknown, None);
    // The probe's errors, re-reported when it cannot infer a type (else
    // the body's real error would vanish behind a poisoned value).
    let mut probe_diags = Vec::new();
    if let Some(HirTy::Struct(sid)) = ctx.current_expected().cloned() {
        let def = ctx.struct_def(sid);
        for t in [false, true] {
            let base = kind.wrapper(t);
            if def.name == super::mangle_inst(base, &def.generic_args) {
                val_ty = def.generic_args.first().cloned().unwrap_or(HirTy::Unknown);
                throws = def.generic_args.get(1).cloned();
            }
        }
    }
    let captures: Vec<(String, rokc_ir_common::LocalId, HirTy, bool)> =
        crate::walk::free_names(&[], &body_expr)
            .into_iter()
            .filter_map(|name| {
                let local = ctx.lookup(&name)?;
                // An `async` method's `ref self` cell: the call hands it
                // back, read once the future completes.
                let ref_cell =
                    from_fn && ctx.ref_cells && ctx.ref_params.iter().any(|(l, _)| *l == local);
                if (from_closure || ref_cell) && ctx.is_boxed(local) {
                    return Some((name, local, ctx.local_ty(local), true));
                }
                let ty = ctx.var_ty(local);
                Some((name, local, ty, false))
            })
            .collect();

    let make_ctx = |ctx: &LowerCtx<'a>,
                    val: &HirTy,
                    throws: &Option<HirTy>,
                    probe: bool|
     -> (LowerCtx<'a>, Vec<HirParam>, Vec<HirStmt>) {
        let (mut c, mut params, prelude, _) = closure_ctx(ctx, &[], &captures, None, span);
        if from_fn {
            c.value_captured.clear();
            // 4.16.5: an async implicit `main`'s body keeps its rules.
            c.implicit_main = ctx.implicit_main;
        }
        if kind.is_async() {
            let cty = ctx
                .lookup_struct(CONTEXT)
                .map_or(HirTy::Unknown, HirTy::Struct);
            let l = c.declare(Some(CTX.to_owned()), cty.clone());
            params.push(HirParam {
                name: CTX.to_owned(),
                local: l,
                ty: cty,
                by_ref: false,
            });
        } else {
            let bool_ty = HirTy::Prim(PrimTy::Bool);
            let l = c.declare(Some(CLOSE.to_owned()), bool_ty.clone());
            params.push(HirParam {
                name: CLOSE.to_owned(),
                local: l,
                ty: bool_ty,
                by_ref: false,
            });
        }
        let (item, out) = if kind.yields() {
            (val.clone(), HirTy::Unit)
        } else {
            (HirTy::Unit, val.clone())
        };
        c.coro = Some(CoroCtx { kind, item, probe });
        c.infer_ret = out == HirTy::Unknown;
        c.fn_ret_ty = out;
        c.fn_throws.clone_from(throws);
        (c, params, prelude)
    };

    // Probe pass: infer what the body yields / returns and throws.
    let needs_throws = throws.is_none() && may_throw(&body_expr);
    if val_ty == HirTy::Unknown || needs_throws {
        let mark = ctx.probe_mark();
        let (mut probe, ..) = make_ctx(ctx, &val_ty, &throws, true);
        if needs_throws {
            probe.throws_probe = Some(Vec::new());
        }
        let hint = probe.fn_ret_ty.clone();
        let value = lower_expected(&mut probe, &body_expr, hint);
        probe_diags = ctx.rollback(&mark);
        if val_ty == HirTy::Unknown {
            val_ty = if kind.yields() {
                probe
                    .coro
                    .as_ref()
                    .map_or(HirTy::Unknown, |c| c.item.clone())
            } else {
                match probe.fn_ret_ty {
                    HirTy::Unknown if !probe.infer_ret => HirTy::Unknown,
                    HirTy::Unknown => value.ty,
                    t => t,
                }
            };
            if val_ty == HirTy::Never {
                val_ty = HirTy::Unit;
            }
            if val_ty == HirTy::Unknown && kind.yields() {
                val_ty = HirTy::Unit;
            }
        }
        if needs_throws {
            let errs = probe.throws_probe.take().unwrap_or_default();
            throws = match crate::ty::throws_ty(HirTy::Unit, HirTy::Sum(errs)) {
                HirTy::Throws { err, .. } => Some(*err),
                _ => None,
            };
        }
    }
    if val_ty == HirTy::Unknown {
        ctx.env.sink.diagnostics.borrow_mut().extend(probe_diags);
        return super::poison(span);
    }

    let body_id = rokc_ir_common::FunctionId(ctx.env.sink.next_id.get());
    ctx.env.sink.next_id.set(body_id.0 + 1);
    let (mut c, params, mut prelude) = make_ctx(ctx, &val_ty, &throws, false);
    if kind.is_async() {
        let bool_ty = HirTy::Prim(PrimTy::Bool);
        let l = c.declare(Some(UNWINDING.to_owned()), bool_ty.clone());
        prelude.push(HirStmt::Let {
            local: l,
            init: HirExpr {
                kind: HirExprKind::Const(ConstVal::Bool(false)),
                ty: bool_ty,
                span,
            },
        });
    }
    if kind == CoroKind::Async {
        let first = prime_captures(&mut c, &captures, span);
        let pending = pending_step(&c);
        prelude.extend(
            pending
                .and_then(|p| parked(&c, first, p, span))
                .map(HirStmt::Expr),
        );
    }
    // Captured values the coroutine owns are released when it finishes
    // (or is cancelled).
    let owed = c.err_frames.len();
    let captured: Vec<(rokc_ir_common::LocalId, HirTy)> = prelude
        .iter()
        .filter_map(|s| match s {
            HirStmt::Let { local, init } if !c.is_boxed(*local) => Some((*local, init.ty.clone())),
            _ => None,
        })
        .collect();
    for (l, ty) in captured {
        prelude.extend(super::affine::owe(&mut c, l, &ty));
    }
    let out = c.fn_ret_ty.clone();
    let body_h = lower_expected(&mut c, &body_expr, out.clone());
    let body_h = if super::hir_expr_diverges(&body_h) || out == HirTy::Unit {
        body_h
    } else {
        super::coerce(&mut c, &out, body_h, &|| "the async body".to_owned())
    };
    let body_h = super::affine::settle_expr(&mut c, owed, body_h);
    let coro = c.coro.clone().expect("coroutine ctx");
    let step_ty = coro.step_ty(&c, &out);
    let r = c.declare(None, out.clone());
    // A generator closed before its first `next` runs none of its body.
    let unstarted: Vec<HirStmt> = local_bool(&c, CLOSE, span)
        .map(|closing| HirStmt::Expr(if_else(closing, vec![coro_return(None)], Vec::new(), span)))
        .into_iter()
        .collect();
    let labelled = HirExpr {
        ty: out.clone(),
        span,
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: unstarted,
            trailing: Some(body_h),
            label: Some(BODY_LABEL.to_owned()),
        })),
    };
    let run = HirExpr {
        kind: HirExprKind::If {
            cond: Box::new(HirExpr {
                kind: HirExprKind::Const(ConstVal::Bool(true)),
                ty: HirTy::Prim(PrimTy::Bool),
                span,
            }),
            then_block: Box::new(HirBlock {
                stmts: Vec::new(),
                trailing: Some(labelled),
                label: None,
            }),
            else_branch: Some(Box::new(HirExpr {
                kind: HirExprKind::Const(ConstVal::Unit),
                ty: out.clone(),
                span,
            })),
        },
        ty: out.clone(),
        span,
    };
    let opt = HirTy::sum([coro.item, HirTy::None]);
    let none = || super::desugar::sum_none(&opt, span);
    let final_value = match kind {
        CoroKind::Gen => none(),
        CoroKind::Async => poll_variant(&c, &step_ty, "Ready", vec![hir_local(r, &out, span)]),
        CoroKind::Stream => poll_variant(&c, &step_ty, "Ready", vec![none()]),
    };
    let final_value = step(&c, final_value);
    let ret_ty = final_value.ty.clone();
    let mut stmts = prelude;
    stmts.push(if out == HirTy::Unit {
        HirStmt::Expr(run)
    } else {
        HirStmt::Let {
            local: r,
            init: run,
        }
    });
    stmts.extend(frame_end(&mut c, span));
    stmts.push(HirStmt::Expr(suspend(final_value, true, span)));
    let resume_tys: Vec<HirTy> = params
        .iter()
        .filter(|p| p.name == CTX || p.name == CLOSE)
        .map(|p| p.ty.clone())
        .collect();
    ctx.env.sink.extras.borrow_mut().push(HirFunction {
        id: body_id,
        def_id: None,
        name: format!("__coro_{}", body_id.0),
        params,
        locals: c.into_locals(),
        ret_ty: ret_ty.clone(),
        body: Some(HirBlock {
            stmts,
            trailing: None,
            label: None,
        }),
        is_async: kind.is_async(),
        is_generator: kind.yields(),
        span,
        facts: rokc_ir_common::FnFacts::default(),
    });

    let env = (!captures.is_empty()).then(|| {
        let fields: Vec<HirExpr> = captures
            .iter()
            .map(|(_, l, ty, by_ref)| {
                if *by_ref {
                    hir_local(*l, ty, span)
                } else {
                    ctx.read_local(*l, span)
                }
            })
            .collect();
        HirExpr {
            ty: HirTy::Tuple(captures.iter().map(|(_, _, t, _)| t.clone()).collect()),
            kind: HirExprKind::Tuple { fields },
            span,
        }
    });
    let mut args = vec![val_ty];
    args.extend(throws.clone());
    let Some(sid) = ctx.instantiate_struct(kind.wrapper(throws.is_some()), args, ctx.here()) else {
        return ctx.error_expr(
            Code::INTERNAL,
            span,
            format!(
                "`{}` is missing from the core library",
                kind.wrapper(throws.is_some())
            ),
        );
    };
    let field_ty = ctx
        .struct_def(sid)
        .fields
        .first()
        .map_or(HirTy::Unknown, |f| f.ty.clone());
    let coro_fn = HirExpr {
        kind: HirExprKind::CoroNew {
            body_fn: body_id,
            args: env.into_iter().collect(),
        },
        ty: match &field_ty {
            HirTy::Fn(_, _, bounds) => HirTy::Fn(resume_tys, Box::new(ret_ty), bounds.clone()),
            _ => field_ty,
        },
        span,
    };
    HirExpr {
        kind: HirExprKind::StructInit {
            struct_id: sid,
            fields: vec![coro_fn],
        },
        ty: HirTy::Struct(sid),
        span,
    }
}
