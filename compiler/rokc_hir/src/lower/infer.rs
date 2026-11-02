//! An unannotated `let`'s type from the code after it:
//! `let xs = List::new(); xs.push(1);` makes `xs: List<i32>`.
//!
//! Lowering is eager, so this is a probe: the initializer is lowered with
//! unfixed type parameters as inference variables (`#var N`), the rest of
//! the block against that open type, and every place a value meets a slot
//! unifies them. Then everything is rolled back and the initializer
//! lowered for real under the solved type. Nested `let`s share the
//! variables, so `let a = List::new(); let b = List::new(); b.push(1);
//! a.push(b);` solves both. Instances over open types are signatures only
//! (`mono::var_instance`); open struct / enum instances are dropped at the
//! end (`drop_var_instances`).

#[allow(clippy::wildcard_imports)]
use super::*;
use rokc_diagnostic::Code;

/// An inference variable: what it is bound to, and where.
#[derive(Clone)]
pub(super) struct Var {
    ty: Option<HirTy>,
    /// Bound from an unsuffixed literal: a later concrete use of the same
    /// number kind replaces it (`xs.push(1); let n: i64 = xs[0];`).
    weak: bool,
    fixed: rokc_diagnostic::Span,
    /// The first use that disagreed with the binding: where, and the type.
    conflict: Option<(rokc_diagnostic::Span, HirTy)>,
}

rokc_core::codec_struct!(Var {
    ty,
    weak,
    fixed,
    conflict
});

pub(super) fn fresh(ctx: &LowerCtx<'_>) -> HirTy {
    let mut vars = ctx.env.sink.infer_vars.borrow_mut();
    vars.push(Var {
        ty: None,
        weak: false,
        fixed: ctx.here(),
        conflict: None,
    });
    HirTy::Var(u32::try_from(vars.len() - 1).expect("inference variable overflow"))
}

const fn var_of(t: &HirTy) -> Option<usize> {
    match t {
        HirTy::Var(v) => Some(*v as usize),
        _ => None,
    }
}

/// Is a `let` inference running (unification only then)?
pub(super) fn active(ctx: &LowerCtx<'_>) -> bool {
    let sink = &ctx.env.sink;
    // Only once a variable exists: a `let` whose initializer opens none
    // pays nothing.
    sink.infer_depth.get() > 0 && sink.infer_vars.borrow().len() > sink.infer_floor.get()
}

pub(super) fn has_var(ctx: &LowerCtx<'_>, t: &HirTy) -> bool {
    let rec = |t: &HirTy| has_var(ctx, t);
    match t {
        HirTy::Var(_) => true,
        HirTy::Struct(id) => ctx.struct_def(*id).generic_args.iter().any(rec),
        HirTy::Enum(id) => ctx.enum_def(*id).generic_args.iter().any(rec),
        HirTy::Sum(ps) | HirTy::Tuple(ps) => ps.iter().any(rec),
        HirTy::Array(e, _) | HirTy::GcBuf(e) => rec(e),
        HirTy::Fn(a, r, _) => a.iter().any(rec) || rec(r),
        HirTy::Closure { args, ret, env, .. } => args.iter().any(rec) || rec(ret) || rec(env),
        HirTy::Throws { ok, err } => rec(ok) || rec(err),
        _ => false,
    }
}

/// `t` with every bound variable replaced by its type (struct / enum
/// instances re-instantiated).
pub(super) fn zonk(ctx: &LowerCtx<'_>, t: &HirTy) -> HirTy {
    if !has_var(ctx, t) {
        return t.clone();
    }
    let rec = |t: &HirTy| zonk(ctx, t);
    let all = |ts: &[HirTy]| ts.iter().map(rec).collect::<Vec<_>>();
    match t {
        HirTy::Var(_) => {
            let bound = var_of(t).and_then(|v| ctx.env.sink.infer_vars.borrow()[v].ty.clone());
            bound.map_or_else(|| t.clone(), |b| rec(&b))
        }
        HirTy::Struct(id) => {
            let d = ctx.struct_def(*id);
            let base = d.name.split("__").next().unwrap_or_default();
            ctx.instantiate_struct(base, all(&d.generic_args), d.span)
                .map_or_else(|| t.clone(), HirTy::Struct)
        }
        HirTy::Enum(id) => {
            let d = ctx.enum_def(*id);
            let base = d.name.split("__").next().unwrap_or_default();
            ctx.instantiate_enum(base, all(&d.generic_args), d.span)
                .map_or_else(|| t.clone(), HirTy::Enum)
        }
        HirTy::Sum(ps) => HirTy::sum(all(ps)),
        HirTy::Tuple(ps) => HirTy::Tuple(all(ps)),
        HirTy::Array(e, n) => HirTy::Array(Box::new(rec(e)), *n),
        HirTy::GcBuf(e) => HirTy::GcBuf(Box::new(rec(e))),
        HirTy::Fn(a, r, b) => HirTy::Fn(all(a), Box::new(rec(r)), b.clone()),
        HirTy::Closure {
            args,
            ret,
            env,
            shares,
        } => HirTy::Closure {
            args: all(args),
            ret: Box::new(rec(ret)),
            env: Box::new(rec(env)),
            shares: *shares,
        },
        HirTy::Throws { ok, err } => HirTy::Throws {
            ok: Box::new(rec(ok)),
            err: Box::new(rec(err)),
        },
        _ => t.clone(),
    }
}

/// Is `e` a number literal (its type is only the default)?
const fn is_literal(e: &HirExpr) -> bool {
    matches!(
        e.kind,
        HirExprKind::Const(ConstVal::Int { .. } | ConstVal::UInt { .. } | ConstVal::Float { .. })
    )
}

/// `coerce` during an inference: a value meeting a slot where either
/// side is open unifies them instead of checking (`true`: done).
pub(super) fn unify_value(ctx: &LowerCtx<'_>, slot: &HirTy, value: &HirExpr) -> bool {
    if !active(ctx) || !(has_var(ctx, slot) || has_var(ctx, &value.ty)) {
        return false;
    }
    unify_weak(ctx, slot, &value.ty, is_literal(value));
    true
}

/// A `fn` / closure type's parameters and result.
fn fn_sig(ty: &HirTy) -> Option<(&[HirTy], &HirTy)> {
    match ty {
        HirTy::Fn(params, ret, _)
        | HirTy::Closure {
            args: params, ret, ..
        } => Some((params, ret)),
        _ => None,
    }
}

fn unify_weak(ctx: &LowerCtx<'_>, lhs: &HirTy, rhs: &HirTy, weak: bool) -> bool {
    let rec = |l: &HirTy, r: &HirTy| unify_weak(ctx, l, r, weak);
    let all = |ls: &[HirTy], rs: &[HirTy]| {
        ls.len() == rs.len() && ls.iter().zip(rs).all(|(l, r)| rec(l, r))
    };
    match (lhs, rhs) {
        _ if lhs == rhs => true,
        (HirTy::Unknown | HirTy::Never | HirTy::Any, _)
        | (_, HirTy::Unknown | HirTy::Never | HirTy::Any) => true,
        (HirTy::Var(_), _) => bind(ctx, var_of(lhs).unwrap_or(0), rhs, weak),
        (_, HirTy::Var(_)) => bind(ctx, var_of(rhs).unwrap_or(0), lhs, weak),
        (HirTy::Struct(l), HirTy::Struct(r)) => {
            let (l, r) = (ctx.struct_def(*l), ctx.struct_def(*r));
            l.name.split("__").next() == r.name.split("__").next()
                && all(&l.generic_args, &r.generic_args)
        }
        (HirTy::Enum(l), HirTy::Enum(r)) => {
            let (l, r) = (ctx.enum_def(*l), ctx.enum_def(*r));
            l.name.split("__").next() == r.name.split("__").next()
                && all(&l.generic_args, &r.generic_args)
        }
        (HirTy::Tuple(ls), HirTy::Tuple(rs)) | (HirTy::Sum(ls), HirTy::Sum(rs)) => all(ls, rs),
        // A member of an open sum: the one open member takes it.
        (HirTy::Sum(ps), other) | (other, HirTy::Sum(ps)) => {
            if ps.contains(other) {
                return true;
            }
            let open: Vec<&HirTy> = ps.iter().filter(|p| has_var(ctx, p)).collect();
            matches!(open.as_slice(), [one] if rec(one, other))
        }
        (HirTy::Array(le, ln), HirTy::Array(re, rn)) => ln == rn && rec(le, re),
        (HirTy::GcBuf(le), HirTy::GcBuf(re)) => rec(le, re),
        (HirTy::Closure { env: le, .. }, HirTy::Closure { env: re, .. }) if !rec(le, re) => false,
        (HirTy::Throws { ok: lo, err: le }, HirTy::Throws { ok: ro, err: re }) => {
            rec(lo, ro) && rec(le, re)
        }
        _ => match (fn_sig(lhs), fn_sig(rhs)) {
            (Some((lp, lr)), Some((rp, rr))) => all(lp, rp) && rec(lr, rr),
            _ => false,
        },
    }
}

fn bind(ctx: &LowerCtx<'_>, v: usize, t: &HirTy, weak: bool) -> bool {
    let here = ctx.here();
    let bound = ctx.env.sink.infer_vars.borrow()[v].ty.clone();
    let ok = match bound {
        None => {
            // Occurs check: `xs.push(xs)`.
            if zonk_has(ctx, t, v) {
                false
            } else {
                let mut vars = ctx.env.sink.infer_vars.borrow_mut();
                vars[v].ty = Some(t.clone());
                vars[v].weak = weak;
                vars[v].fixed = here;
                true
            }
        }
        Some(b) => {
            let was_weak = ctx.env.sink.infer_vars.borrow()[v].weak;
            // Both integers / both floats.
            let kind = |t: &HirTy| match t {
                HirTy::Prim(p) if p.is_int() => Some(true),
                HirTy::Prim(p) if p.is_float() => Some(false),
                _ => None,
            };
            let same_kind = kind(&b).is_some() && kind(&b) == kind(t);
            if was_weak && !weak && same_kind {
                let mut vars = ctx.env.sink.infer_vars.borrow_mut();
                vars[v].ty = Some(t.clone());
                vars[v].weak = false;
                vars[v].fixed = here;
                true
            } else {
                unify_weak(ctx, &b, t, weak) || (weak && same_kind)
            }
        }
    };
    if !ok {
        let mut vars = ctx.env.sink.infer_vars.borrow_mut();
        if vars[v].conflict.is_none() {
            vars[v].conflict = Some((here, t.clone()));
        }
    }
    ok
}

/// Does `t` (resolved) mention variable `v`?
fn zonk_has(ctx: &LowerCtx<'_>, t: &HirTy, v: usize) -> bool {
    fn walk(ctx: &LowerCtx<'_>, t: &HirTy, v: usize) -> bool {
        let rec = |t: &HirTy| walk(ctx, t, v);
        match t {
            HirTy::Var(_) => match var_of(t) {
                Some(w) if w == v => true,
                Some(w) => ctx.env.sink.infer_vars.borrow()[w]
                    .ty
                    .clone()
                    .is_some_and(|b| rec(&b)),
                None => false,
            },
            HirTy::Struct(id) => ctx.struct_def(*id).generic_args.iter().any(rec),
            HirTy::Enum(id) => ctx.enum_def(*id).generic_args.iter().any(rec),
            HirTy::Sum(ps) | HirTy::Tuple(ps) => ps.iter().any(rec),
            HirTy::Array(e, _) | HirTy::GcBuf(e) => rec(e),
            HirTy::Fn(a, r, _) => a.iter().any(rec) || rec(r),
            HirTy::Closure { args, ret, env, .. } => args.iter().any(rec) || rec(ret) || rec(env),
            HirTy::Throws { ok, err } => rec(ok) || rec(err),
            _ => false,
        }
    }
    has_var(ctx, t) && walk(ctx, t, v)
}

/// `let name = init;` without annotation: lower `init` with open
/// parameters; if any remain, probe the rest of the block, roll back, and
/// lower `init` again under the solved type.
pub(super) fn lower_let(
    ctx: &mut LowerCtx<'_>,
    s: &Stmt,
    name: &str,
    init: &Expr,
    rest: &[Stmt],
    block: &Block,
) -> Option<HirStmt> {
    let sink = &ctx.env.sink;
    let mark = ctx.probe_mark();
    let first = sink.infer_vars.borrow().len();
    let outer = std::mem::replace(&mut ctx.infer_vars, true);
    if sink.infer_depth.get() == 0 {
        sink.infer_floor.set(first);
    }
    sink.infer_depth.set(sink.infer_depth.get() + 1);
    let stmt = expr::lower_stmt(ctx, s);
    ctx.infer_vars = outer;
    let own = first..sink.infer_vars.borrow().len();
    // Nothing open, or inside an enclosing inference (which solves it).
    if own.is_empty() || outer {
        sink.infer_depth.set(sink.infer_depth.get() - 1);
        return stmt;
    }
    let open = ctx.lookup(name).map_or(HirTy::Unknown, |l| ctx.local_ty(l));
    let solved = has_var(ctx, &open).then(|| {
        let mut probe = ctx.clone();
        probe.infer_vars = true;
        let tail = Block {
            stmts: rest.to_vec(),
            trailing: block.trailing.clone(),
            span: block.span,
            label: None,
        };
        let hint = ctx.current_expected().cloned().unwrap_or(HirTy::Unknown);
        probe.expected_ty_stack.push(hint);
        expr::lower_block(&mut probe, &tail);
        // A closure's result is only what its body returns.
        match zonk(ctx, &open) {
            HirTy::Closure {
                args,
                ret,
                env,
                shares,
            } if has_var(ctx, &ret) => HirTy::Closure {
                args,
                ret: Box::new(HirTy::Unknown),
                env,
                shares,
            },
            HirTy::Fn(args, ret, b) if has_var(ctx, &ret) => {
                HirTy::Fn(args, Box::new(HirTy::Unknown), b)
            }
            t => t,
        }
    });
    sink.infer_depth.set(sink.infer_depth.get() - 1);
    let conflict = own.into_iter().find_map(|v| {
        let vars = sink.infer_vars.borrow();
        let var = &vars[v];
        var.conflict
            .clone()
            .map(|(at, t)| (at, t, var.fixed, var.ty.clone()))
    });
    // Undo the open attempt (its binding included).
    ctx.rollback(&mark);
    let outer_span = ctx.at.replace(s.span);
    let out = match solved {
        // The open variables are not in the binding's type
        // (`List::new().len()`): lowered again, they are errors.
        None => expr::lower_stmt(ctx, s),
        Some(t) => {
            if let Some((at, used, fixed, Some(have))) = conflict {
                let d = rokc_diagnostic::RokDiagnostic::error(
                    Code::MISMATCHED_TYPES,
                    at,
                    format!(
                        "conflicting uses of `{name}` (`{}`): `{}` here, but `{}` fixed at {}:{}",
                        ctx.ty_name(&open),
                        ctx.ty_name(&zonk(ctx, &used)),
                        ctx.ty_name(&zonk(ctx, &have)),
                        fixed.line,
                        fixed.col
                    ),
                )
                .with_secondary(fixed, format!("`{name}`'s type fixed here"));
                ctx.env.sink.diagnostics.borrow_mut().push(d);
                // Its uses are not checked against either type again.
                lower_expected(ctx, init, t);
                ctx.declare(Some(name.to_owned()), HirTy::Unknown);
                None
            } else if has_var(ctx, &t) {
                ctx.emit_uninferred(format!(
                    "cannot infer the type of `{name}` (`{}`) from its initializer or later \
                     uses - add an annotation (`let {name}: T = ..`)",
                    ctx.ty_name(&t).replace("{unknown}", "_")
                ));
                ctx.declare(Some(name.to_owned()), HirTy::Unknown);
                None
            } else {
                let v = lower_expected(ctx, init, t.clone());
                // A closure takes the parameter types; its own type is
                // what its body makes of them.
                let (t, v) = if matches!(t, HirTy::Closure { .. } | HirTy::Fn(..)) {
                    (v.ty.clone(), v)
                } else {
                    let v = coerce(ctx, &t, v, &|| format!("`let {name}`"));
                    (t, v)
                };
                Some(ctx.declare_var(name, t, v, s.span))
            }
        }
    };
    ctx.at.set(outer_span);
    out
}

/// Struct / enum instances over inference variables (made by probes)
/// are never used: empty them so nothing lays them out.
pub(super) fn drop_var_instances(structs: &mut [HirStructDef], enums: &mut [HirEnumDef]) {
    let open = |args: &[HirTy], s: &[bool], e: &[bool]| {
        fn walk(ty: &HirTy, open_s: &[bool], open_e: &[bool]) -> bool {
            let rec = |t: &HirTy| walk(t, open_s, open_e);
            match ty {
                HirTy::Var(_) => true,
                HirTy::Struct(id) => open_s.get(id.0 as usize).copied().unwrap_or(false),
                HirTy::Enum(id) => open_e.get(id.0 as usize).copied().unwrap_or(false),
                HirTy::Sum(ps) | HirTy::Tuple(ps) => ps.iter().any(rec),
                HirTy::Array(elem, _) | HirTy::GcBuf(elem) => rec(elem),
                HirTy::Fn(params, ret, _) => params.iter().any(rec) || rec(ret),
                HirTy::Closure { args, ret, env, .. } => {
                    args.iter().any(rec) || rec(ret) || rec(env)
                }
                HirTy::Throws { ok, err } => rec(ok) || rec(err),
                _ => false,
            }
        }
        args.iter().any(|a| walk(a, s, e))
    };
    let mut s = vec![false; structs.len()];
    let mut e = vec![false; enums.len()];
    loop {
        let ns: Vec<bool> = structs
            .iter()
            .map(|d| open(&d.generic_args, &s, &e))
            .collect();
        let ne: Vec<bool> = enums
            .iter()
            .map(|d| open(&d.generic_args, &s, &e))
            .collect();
        if ns == s && ne == e {
            break;
        }
        (s, e) = (ns, ne);
    }
    for (d, o) in structs.iter_mut().zip(&s) {
        if *o {
            d.fields.clear();
        }
    }
    for (d, o) in enums.iter_mut().zip(&e) {
        if *o {
            d.variants.clear();
        }
    }
}
