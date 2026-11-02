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
    /// A negative literal's: an unsigned use does not take it.
    neg: bool,
    fixed: rokc_diagnostic::Span,
    /// The first use that disagreed with the binding: where, and the type.
    conflict: Option<(rokc_diagnostic::Span, HirTy)>,
}

rokc_core::codec_struct!(Var {
    ty,
    weak,
    neg,
    fixed,
    conflict
});

pub(super) fn fresh(ctx: &LowerCtx<'_>) -> HirTy {
    let mut vars = ctx.env.sink.infer_vars.borrow_mut();
    vars.push(Var {
        ty: None,
        weak: false,
        neg: false,
        fixed: ctx.here(),
        conflict: None,
    });
    HirTy::Var(u32::try_from(vars.len() - 1).expect("inference variable overflow"))
}

/// An unsuffixed number literal's type: `prim` (its default), or, for one
/// of the open literals of an inferred `let`, a variable holding `prim`
/// until a use fixes it (`let c = 0; c += n_i64;`).
pub(super) fn literal_ty(
    ctx: &LowerCtx<'_>,
    at: Span,
    prim: PrimTy,
    open: bool,
    neg: bool,
) -> HirTy {
    if !(open && ctx.infer_vars && ctx.open_lits.contains(&at)) {
        return HirTy::Prim(prim);
    }
    let mut vars = ctx.env.sink.infer_vars.borrow_mut();
    vars.push(Var {
        ty: Some(HirTy::Prim(prim)),
        weak: true,
        neg,
        fixed: at,
        conflict: None,
    });
    HirTy::Var(u32::try_from(vars.len() - 1).expect("inference variable overflow"))
}

/// The literals of a `let` initializer whose type the later code picks:
/// the initializer itself (`0`, `-1`), or a direct element or argument of
/// it (`[0, 0]`, `(0, 1.5)`, `Atomic::new(0)`, `Pair { x: 7, y: 8 }`).
fn open_literals(init: &Expr, out: &mut Vec<Span>) {
    fn lit(e: &Expr, out: &mut Vec<Span>) {
        match &e.kind {
            ExprKind::Literal(AstLit::Int(_) | AstLit::Float(_)) => out.push(e.span),
            ExprKind::OpPrefix { op, operand } if op == "-" => {
                if matches!(
                    operand.kind,
                    ExprKind::Literal(AstLit::Int(_) | AstLit::Float(_))
                ) {
                    out.extend([e.span, operand.span]);
                }
            }
            // Only all-literal arithmetic: `2 * v` is whatever `v`'s
            // operator makes it.
            ExprKind::OpInfix { lhs, rhs, .. } if literal_tree(e).is_some() => {
                lit(lhs, out);
                lit(rhs, out);
            }
            _ => {}
        }
    }
    // `if c { 1 } else { 0 }` and `match`: each branch's value.
    fn branches(e: &Expr, out: &mut Vec<Span>) {
        match &e.kind {
            ExprKind::If {
                then_branch,
                else_branch: Some(other),
                ..
            } => {
                if let Some(t) = &then_branch.trailing {
                    lit(t, out);
                }
                branches(other, out);
            }
            ExprKind::Block(b) => {
                if let Some(t) = &b.trailing {
                    lit(t, out);
                    branches(t, out);
                }
            }
            ExprKind::Match { arms, .. } => {
                for a in arms {
                    lit(&a.body, out);
                    branches(&a.body, out);
                }
            }
            _ => {}
        }
    }
    lit(init, out);
    let positional = |args: &[CallArg], out: &mut Vec<Span>| {
        for a in args {
            if let CallArg::Positional(e) = a {
                lit(e, out);
            }
        }
    };
    branches(init, out);
    match &init.kind {
        ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
            es.iter().for_each(|e| lit(e, out));
        }
        ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => positional(args, out),
        ExprKind::StructInit { fields, .. } => fields.iter().for_each(|(_, e)| lit(e, out)),
        _ => {}
    }
}

/// Is `e` made of number literals only (`0`, `-1`, `2 * (3 + 4)`)?
/// `Some(true)` for integers, `Some(false)` with a float in it.
pub(super) fn literal_tree(e: &Expr) -> Option<bool> {
    match &e.kind {
        ExprKind::Literal(AstLit::Int(_)) => Some(true),
        ExprKind::Literal(AstLit::Float(_)) => Some(false),
        ExprKind::OpPrefix { op, operand } if op == "-" => literal_tree(operand),
        ExprKind::OpInfix { op, lhs, rhs } if expr::is_arith_op(op) => {
            Some(literal_tree(lhs)? && literal_tree(rhs)?)
        }
        _ => None,
    }
}

/// `a op b` during an inference: an open side takes the other's type, and
/// both are seen through their variables. Returns the open type, for an
/// arithmetic result to carry on (`let d = c + 1; f_i64(d);`).
pub(super) fn meet(ctx: &LowerCtx<'_>, l: &mut HirExpr, r: &mut HirExpr) -> Option<HirTy> {
    if !active(ctx) || !(has_var(ctx, &l.ty) || has_var(ctx, &r.ty)) {
        return None;
    }
    let open = if has_var(ctx, &l.ty) {
        l.ty.clone()
    } else {
        r.ty.clone()
    };
    unify_weak(ctx, &l.ty, &r.ty, is_literal(l) || is_literal(r));
    l.ty = zonk(ctx, &l.ty);
    r.ty = zonk(ctx, &r.ty);
    Some(open)
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
            moved,
        } => HirTy::Closure {
            args: all(args),
            ret: Box::new(rec(ret)),
            env: Box::new(rec(env)),
            shares: *shares,
            moved: *moved,
        },
        HirTy::Throws { ok, err } => HirTy::Throws {
            ok: Box::new(rec(ok)),
            err: Box::new(rec(err)),
        },
        _ => t.clone(),
    }
}

/// Is `e` a number literal, or a choice among them (`if c { 1 } else
/// { -1 }`): its type is only the default?
fn is_literal(e: &HirExpr) -> bool {
    let block = |b: &HirBlock| b.trailing.as_ref().is_some_and(is_literal);
    match &e.kind {
        HirExprKind::Const(
            ConstVal::Int { .. } | ConstVal::UInt { .. } | ConstVal::Float { .. },
        ) => true,
        HirExprKind::If {
            then_block,
            else_branch: Some(other),
            ..
        } => block(then_block) && is_literal(other),
        HirExprKind::Block(b) => block(b),
        HirExprKind::Match { arms, .. } => {
            !arms.is_empty() && arms.iter().all(|a| is_literal(&a.body))
        }
        _ => false,
    }
}

/// Branches of an `if` / `match` meeting during an inference: an open
/// one takes the other's type (`true`: they agree).
pub(super) fn join(ctx: &LowerCtx<'_>, a: &HirTy, b: &HirTy) -> bool {
    active(ctx) && (has_var(ctx, a) || has_var(ctx, b)) && unify_weak(ctx, a, b, true)
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

/// The variable `v` is linked to (`a = b` of two literals), or `v`.
fn root(ctx: &LowerCtx<'_>, mut v: usize) -> usize {
    let vars = ctx.env.sink.infer_vars.borrow();
    while let Some(HirTy::Var(w)) = vars[v].ty {
        v = w as usize;
    }
    v
}

/// Both integers / both floats.
fn same_kind(a: &HirTy, b: &HirTy) -> bool {
    let kind = |t: &HirTy| match t {
        HirTy::Prim(p) if p.is_int() => Some(true),
        HirTy::Prim(p) if p.is_float() => Some(false),
        _ => None,
    };
    kind(a).is_some() && kind(a) == kind(b)
}

/// Two variables meet: one holding a literal's default follows the
/// other, linked, so a later fix of either reaches both.
fn bind_vars(ctx: &LowerCtx<'_>, v: usize, w: usize, weak: bool) -> Option<bool> {
    if v == w {
        return Some(true);
    }
    let (bv, wv, bw, ww) = {
        let vars = ctx.env.sink.infer_vars.borrow();
        (
            vars[v].ty.clone()?,
            vars[v].weak,
            vars[w].ty.clone()?,
            vars[w].weak,
        )
    };
    if !same_kind(&bv, &bw) {
        return Some(unify_weak(ctx, &bv, &bw, weak));
    }
    let link = |from: usize, to: usize| {
        ctx.env.sink.infer_vars.borrow_mut()[from].ty = Some(HirTy::Var(to as u32));
        true
    };
    Some(match (wv, ww) {
        (true, _) => link(v, w),
        (false, true) => link(w, v),
        (false, false) => bv == bw,
    })
}

fn bind(ctx: &LowerCtx<'_>, v: usize, t: &HirTy, weak: bool) -> bool {
    let v = root(ctx, v);
    if let HirTy::Var(w) = t {
        let w = root(ctx, *w as usize);
        if let Some(ok) = bind_vars(ctx, v, w, weak) {
            return ok;
        }
        if w == v {
            return true;
        }
        // An open `w` takes a bound `v`; an open `v` links to `w` below.
        let (v_open, w_open) = {
            let vars = ctx.env.sink.infer_vars.borrow();
            (vars[v].ty.is_none(), vars[w].ty.is_none())
        };
        if w_open && !v_open {
            return bind(ctx, w, &HirTy::Var(v as u32), weak);
        }
    }
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
            let same_kind = same_kind(&b, t);
            let unsigned = matches!(t, HirTy::Prim(p) if p.is_int() && !p.is_signed_int());
            if was_weak && ctx.env.sink.infer_vars.borrow()[v].neg && unsigned {
                // `let i = -1; xs[i]`: the slot decides (an index from the end).
                true
            } else if was_weak && !weak && same_kind {
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

/// Lower what follows a `let` in a throwaway context, unifying as it goes.
fn probe_tail(ctx: &LowerCtx<'_>, rest: &[Stmt], block: &Block) {
    let mut probe = ctx.clone();
    probe.infer_vars = true;
    let tail = Block {
        stmts: rest.to_vec(),
        trailing: block.trailing.clone(),
        span: block.span,
        label: None,
    };
    let hint = ctx.current_expected().cloned().unwrap_or(HirTy::Unknown);
    probe.expected_ty_stack.push(hint.clone());
    let b = expr::lower_block(&mut probe, &tail);
    // The tail meets the block's slot (a body's result).
    if let Some(t) = b.trailing.as_ref().filter(|_| hint != HirTy::Unknown) {
        unify_value(&probe, &hint, t);
    }
}

/// `let (a, b) = init;` without annotation, as `lower_let`:
/// `let (tx, rx) = channel(); tx.send(1);`.
#[allow(clippy::too_many_arguments)]
pub(super) fn lower_let_tuple(
    ctx: &mut LowerCtx<'_>,
    names: &[String],
    tuple_rest: Option<&(usize, Option<String>)>,
    init: &Expr,
    span: Span,
    rest: &[Stmt],
    block: &Block,
    out: &mut Vec<HirStmt>,
) {
    let sink = &ctx.env.sink;
    let mark = ctx.probe_mark();
    let first = sink.infer_vars.borrow().len();
    let outer = std::mem::replace(&mut ctx.infer_vars, true);
    if sink.infer_depth.get() == 0 {
        sink.infer_floor.set(first);
    }
    sink.infer_depth.set(sink.infer_depth.get() + 1);
    let mut lits = Vec::new();
    open_literals(init, &mut lits);
    let outer_lits = std::mem::replace(&mut ctx.open_lits, lits);
    let mut stmts = Vec::new();
    expr::lower_let_tuple(ctx, names, tuple_rest, None, init, span, &mut stmts);
    ctx.open_lits = outer_lits;
    ctx.infer_vars = outer;
    let own = first..sink.infer_vars.borrow().len();
    if own.is_empty() || outer {
        sink.infer_depth.set(sink.infer_depth.get() - 1);
        out.extend(stmts);
        return;
    }
    let open = match stmts.first() {
        Some(HirStmt::Let { init, .. }) => init.ty.clone(),
        _ => HirTy::Unknown,
    };
    let solved = has_var(ctx, &open).then(|| {
        probe_tail(ctx, rest, block);
        zonk(ctx, &open)
    });
    sink.infer_depth.set(sink.infer_depth.get() - 1);
    let conflict = own.into_iter().find_map(|v| {
        let vars = sink.infer_vars.borrow();
        let var = &vars[v];
        let (at, used) = var.conflict.clone()?;
        let have = var.ty.clone()?;
        Some((at, used, var.fixed, have))
    });
    let conflict = conflict.map(|(at, used, fixed, have)| {
        let (used, have) = (
            ctx.ty_name(&zonk(ctx, &used)),
            ctx.ty_name(&zonk(ctx, &have)),
        );
        rokc_diagnostic::RokDiagnostic::error(
            Code::MISMATCHED_TYPES,
            at,
            format!(
                "conflicting uses of the destructured value: `{used}` here, but `{have}` fixed \
                 at {}:{}",
                fixed.line, fixed.col
            ),
        )
        .with_secondary(fixed, "its type fixed here".to_owned())
    });
    ctx.rollback(&mark);
    let outer_span = ctx.at.replace(span);
    match (solved, conflict) {
        (Some(_), Some(d)) => {
            ctx.env.sink.diagnostics.borrow_mut().push(d);
            let _ = lower_expected(ctx, init, HirTy::Unknown);
            for n in names {
                ctx.declare(Some(n.clone()), HirTy::Unknown);
            }
        }
        (Some(t), None) if !has_var(ctx, &t) => {
            expr::lower_let_tuple(ctx, names, tuple_rest, Some(t), init, span, out);
        }
        _ => expr::lower_let_tuple(ctx, names, tuple_rest, None, init, span, out),
    }
    ctx.at.set(outer_span);
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
    let mut lits = Vec::new();
    open_literals(init, &mut lits);
    let lit_spans = lits.clone();
    let outer_lits = std::mem::replace(&mut ctx.open_lits, lits);
    let stmt = expr::lower_stmt(ctx, s);
    ctx.open_lits = outer_lits;
    ctx.infer_vars = outer;
    let own = first..sink.infer_vars.borrow().len();
    // Nothing open, or inside an enclosing inference (which solves it).
    if own.is_empty() || outer {
        sink.infer_depth.set(sink.infer_depth.get() - 1);
        return stmt;
    }
    let open = ctx.lookup(name).map_or(HirTy::Unknown, |l| ctx.var_ty(l));
    let lit_init = !lit_spans.is_empty() && literal_tree(init).is_some();
    let solved = has_var(ctx, &open).then(|| {
        probe_tail(ctx, rest, block);
        // A closure's result is only what its body returns.
        match zonk(ctx, &open) {
            HirTy::Closure {
                args,
                ret,
                env,
                shares,
                moved,
            } if has_var(ctx, &ret) => HirTy::Closure {
                args,
                ret: Box::new(HirTy::Unknown),
                env,
                shares,
                moved,
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
        let (at, used) = var.conflict.clone()?;
        let have = var.ty.clone()?;
        Some((at, used, var.fixed, have, var.weak))
    });
    // A literal's other conflicts (`if c`, a float as an `i32`) are left to
    // the plain mismatch at the use.
    let number = |t: &HirTy| matches!(zonk(ctx, t), HirTy::Prim(p) if p.is_int() || p.is_float());
    let float = |t: &HirTy| matches!(zonk(ctx, t), HirTy::Prim(p) if p.is_float());
    let conflict = conflict.filter(|(_, used, _, have, weak)| {
        !lit_init || (number(used) && number(have) && (!weak || (float(used) && !float(have))))
    });
    let conflict = conflict.map(|(at, used, fixed, have, weak)| {
        let (used, have) = (
            ctx.ty_name(&zonk(ctx, &used)),
            ctx.ty_name(&zonk(ctx, &have)),
        );
        if lit_init && weak {
            literal_conflict(ctx, name, init, at, &used, &have)
        } else {
            let shown = if lit_init {
                String::new()
            } else {
                format!(" (`{}`)", ctx.ty_name(&open))
            };
            let d = rokc_diagnostic::RokDiagnostic::error(
                Code::MISMATCHED_TYPES,
                at,
                format!(
                    "conflicting uses of `{name}`{shown}: `{used}` here, but `{have}` fixed at \
                     {}:{}",
                    fixed.line, fixed.col
                ),
            )
            .with_secondary(fixed, format!("`{name}`'s type fixed here"));
            if lit_init {
                d.with_help(format!(
                    "rok never converts numbers implicitly; say the type at the declaration \
                     (`let {name}: {have} = ..`) and convert the other use with `as`"
                ))
            } else {
                d
            }
        }
    });
    // Undo the open attempt (its binding included).
    ctx.rollback(&mark);
    let outer_span = ctx.at.replace(s.span);
    let out = match solved {
        // The open variables are not in the binding's type
        // (`List::new().len()`): lowered again, they are errors.
        None => expr::lower_stmt(ctx, s),
        Some(t) => {
            if let Some(d) = conflict {
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

/// A literal-bound `let` used as another type: the fix goes on the
/// declaration (`let c = 0.0`, `let c: str = ..`), not a cast at the use.
fn literal_conflict(
    ctx: &LowerCtx<'_>,
    name: &str,
    init: &Expr,
    at: Span,
    used: &str,
    have: &str,
) -> rokc_diagnostic::RokDiagnostic {
    let int = literal_tree(init).unwrap_or(true);
    let kind = if int { "an integer" } else { "a float" };
    let src = ctx.src(init.span).unwrap_or("..").to_owned();
    let float_use = matches!(used, "f32" | "f64");
    let decimal = src
        .chars()
        .all(|c| c.is_ascii_digit() || c == '_' || c == '-');
    let help = if int && float_use && decimal {
        format!(
            "rok never converts numbers implicitly; make it a float at the declaration: \
             `let {name} = {src}.0;` (or `let {name}: {used} = {src};`)"
        )
    } else if int && float_use {
        format!(
            "rok never converts numbers implicitly; make it a float at the declaration: \
             `let {name}: {used} = {src};`"
        )
    } else if !int && used.starts_with(['i', 'u']) && used.len() <= 5 {
        format!(
            "rok never converts numbers implicitly; declare an integer (`let {name}: {used} = \
             ..;`) or convert at the use: `{name} as {used}`"
        )
    } else {
        format!(
            "a number literal is never a `{used}`; give `{name}` a `{used}` value at the declaration"
        )
    };
    rokc_diagnostic::RokDiagnostic::error(
        Code::MISMATCHED_TYPES,
        at,
        format!(
            "`{name}` is used as `{used}` here, but it holds {kind} (`{have}`) from `{src}` at \
             {}:{}",
            init.span.line, init.span.col
        ),
    )
    .with_secondary(init.span, format!("`{name}` declared here"))
    .with_help(help)
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
