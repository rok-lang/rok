//! Type inference inside a body.
//!
//! Every unsuffixed number literal of a body is an integer or float
//! variable holding its default (`i64` / `f64`). Lowering unifies it with
//! each use (slots, operands, bounds, impls, associated types); a use that
//! needs one type now (a method call, an `any` slot) takes the one number
//! type that works, else the default. When the body ends (`close_scope`)
//! every type in its HIR is resolved, the instances asked for over open
//! types are made for the solved ones, and literals take their types:
//! the body is lowered once.
//!
//! An unannotated `let` whose initializer leaves a type parameter open
//! (`let xs = List::new(); xs.push(1);`) is a probe: the initializer is
//! lowered with the parameters as variables, the rest of the block
//! against that open type, then everything is rolled back and the
//! initializer lowered again under the solved type. Instances over open
//! types are signatures only (`mono::var_instance`); open struct / enum
//! instances are dropped at the end (`drop_var_instances`).

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
    /// An unsuffixed literal's: always holds a number type.
    lit: bool,
}

rokc_core::codec_struct!(Var {
    ty,
    weak,
    neg,
    fixed,
    conflict,
    lit
});

pub(super) fn fresh(ctx: &LowerCtx<'_>) -> HirTy {
    let mut vars = ctx.env.sink.infer_vars.borrow_mut();
    vars.push(Var {
        ty: None,
        weak: false,
        neg: false,
        fixed: ctx.here(),
        conflict: None,
        lit: false,
    });
    HirTy::Var(u32::try_from(vars.len() - 1).expect("inference variable overflow"))
}

/// An unsuffixed number literal's type: inside a body, a variable holding
/// `prim` (its default) until a use fixes it (`let c = 0; c += n_i64;`);
/// elsewhere `prim`.
pub(super) fn literal_ty(
    ctx: &LowerCtx<'_>,
    at: Span,
    prim: PrimTy,
    open: bool,
    neg: bool,
) -> HirTy {
    // A literal meeting an open literal's variable shares it
    // (`[1, 2, 3]`: one element type for the later uses to fix).
    if let Some(t @ HirTy::Var(_)) = ctx.current_expected() {
        if active(ctx) && zonk(ctx, t) == HirTy::Prim(prim) {
            return t.clone();
        }
    }
    let sink = &ctx.env.sink;
    let all = sink.lit_open.get().is_some();
    if !(open && all) {
        return HirTy::Prim(prim);
    }
    let mut vars = sink.infer_vars.borrow_mut();
    let v = u32::try_from(vars.len()).expect("inference variable overflow");
    if all {
        sink.lit_vars.borrow_mut().push((at, v));
    }
    vars.push(Var {
        ty: Some(HirTy::Prim(prim)),
        weak: true,
        neg,
        fixed: at,
        conflict: None,
        lit: true,
    });
    HirTy::Var(v)
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

/// The number type an open literal variable `t` holds while its default
/// is not fixed by a use.
fn open_literal(ctx: &LowerCtx<'_>, t: &HirTy) -> Option<(usize, PrimTy)> {
    let HirTy::Var(v) = t else {
        return None;
    };
    let v = root(ctx, *v as usize);
    let vars = ctx.env.sink.infer_vars.borrow();
    match (&vars[v].ty, vars[v].weak) {
        (Some(HirTy::Prim(p)), true) => Some((v, *p)),
        _ => None,
    }
}

/// `f` without its effect on the inference variables: a question asked
/// of a candidate type binds nothing.
pub(super) fn trial(ctx: &LowerCtx<'_>, f: &dyn Fn() -> bool) -> bool {
    let saved = ctx.env.sink.infer_vars.borrow().clone();
    let ok = f();
    *ctx.env.sink.infer_vars.borrow_mut() = saved;
    ok
}

/// An open literal variable fixed to its default (a method call on it
/// that no single other type decides).
pub(super) fn force_default(ctx: &LowerCtx<'_>, t: &HirTy) -> Option<HirTy> {
    let (v, d) = open_literal(ctx, t)?;
    let mut vars = ctx.env.sink.infer_vars.borrow_mut();
    vars[v].weak = false;
    Some(HirTy::Prim(d))
}

/// The default an open literal variable `t` holds, if it is one.
pub(super) fn literal_default(ctx: &LowerCtx<'_>, t: &HirTy) -> Option<HirTy> {
    open_literal(ctx, t).map(|(_, p)| HirTy::Prim(p))
}

/// An open literal that only one number type of its kind makes work
/// (`9.once()`, `area_of(1)`, `let s: any Shape = 7` with the trait
/// implemented for `i32` alone) takes that type; its default wins when it
/// works too. Returns the type picked.
pub(super) fn pick_literal_type(
    ctx: &LowerCtx<'_>,
    t: &HirTy,
    works: &dyn Fn(&HirTy) -> bool,
) -> Option<HirTy> {
    use PrimTy as P;
    if !active(ctx) {
        return None;
    }
    let (v, default) = open_literal(ctx, t)?;
    let d = HirTy::Prim(default);
    if trial(ctx, &|| works(&d)) {
        return None;
    }
    let kinds: &[PrimTy] = if default.is_float() {
        &[P::F32, P::F64]
    } else {
        &[
            P::I32,
            P::I64,
            P::U8,
            P::U16,
            P::U32,
            P::U64,
            P::Usize,
            P::Isize,
            P::I8,
            P::I16,
        ]
    };
    let hits: Vec<HirTy> = kinds
        .iter()
        .map(|p| HirTy::Prim(*p))
        .filter(|c| trial(ctx, &|| works(c)))
        .collect();
    let [one] = hits.as_slice() else {
        return None;
    };
    let mut vars = ctx.env.sink.infer_vars.borrow_mut();
    vars[v].ty = Some(one.clone());
    vars[v].weak = false;
    Some(one.clone())
}

/// The open literal variables (roots) in `t`.
pub(super) fn open_literals_in(ctx: &LowerCtx<'_>, t: &HirTy, out: &mut Vec<usize>) {
    let mut rec = |t: &HirTy| open_literals_in(ctx, t, out);
    match t {
        HirTy::Var(_) => {
            if let Some((v, _)) = open_literal(ctx, t) {
                if !out.contains(&v) {
                    out.push(v);
                }
            }
        }
        HirTy::Struct(id) => ctx.struct_def(*id).generic_args.iter().for_each(rec),
        HirTy::Enum(id) => ctx.enum_def(*id).generic_args.iter().for_each(rec),
        HirTy::Sum(ps) | HirTy::Tuple(ps) => ps.iter().for_each(rec),
        HirTy::Array(e, _) | HirTy::GcBuf(e) => rec(e),
        HirTy::Fn(a, r, _) => {
            a.iter().for_each(&mut rec);
            rec(r);
        }
        HirTy::Closure { args, ret, env, .. } => {
            args.iter().for_each(&mut rec);
            rec(ret);
            rec(env);
        }
        HirTy::Throws { ok, err } => {
            rec(ok);
            rec(err);
        }
        _ => {}
    }
}

/// The number types a literal variable holding `p` can take.
const fn kind_of(p: PrimTy) -> &'static [PrimTy] {
    use PrimTy as P;
    if p.is_float() {
        &[P::F32, P::F64]
    } else {
        &[
            P::I32,
            P::I64,
            P::U8,
            P::U16,
            P::U32,
            P::U64,
            P::Usize,
            P::Isize,
            P::I8,
            P::I16,
        ]
    }
}

/// The open literals of `t` take the one choice of number types under
/// which `works(t)` holds (`(1, 2).foo()` with `Foo` on `(i32, i32)`
/// alone); with none or several, their defaults. Either way they are
/// fixed here.
pub(super) fn pick_literals(ctx: &LowerCtx<'_>, t: &HirTy, works: &dyn Fn(&HirTy) -> bool) {
    pick_literals_or(ctx, t, works, &|_| false, true);
}

/// `pick_literals`, falling back on `then` when `works` picks no one
/// choice (`3.wave()` with `Wave` out of scope: its impl's type, for the
/// error to name the trait); without `default`, no choice leaves them
/// open.
pub(super) fn pick_literals_or(
    ctx: &LowerCtx<'_>,
    t: &HirTy,
    works: &dyn Fn(&HirTy) -> bool,
    then: &dyn Fn(&HirTy) -> bool,
    default: bool,
) {
    fn go(
        i: usize,
        choice: &mut Vec<PrimTy>,
        held: &[PrimTy],
        hits: &mut Vec<Vec<PrimTy>>,
        test: &dyn Fn(&[PrimTy]) -> bool,
    ) {
        if hits.len() > 1 {
            return;
        }
        if i == held.len() {
            if test(choice) {
                hits.push(choice.clone());
            }
            return;
        }
        for p in kind_of(held[i]) {
            choice[i] = *p;
            go(i + 1, choice, held, hits, test);
        }
    }
    let mut vs = Vec::new();
    open_literals_in(ctx, t, &mut vs);
    if vs.is_empty() {
        return;
    }
    let held: Vec<PrimTy> = {
        let vars = ctx.env.sink.infer_vars.borrow();
        vs.iter()
            .map(|v| match vars[*v].ty {
                Some(HirTy::Prim(p)) => p,
                _ => PrimTy::I64,
            })
            .collect()
    };
    let set = |choice: &[PrimTy]| {
        let mut vars = ctx.env.sink.infer_vars.borrow_mut();
        for (v, p) in vs.iter().zip(choice) {
            vars[*v].ty = Some(HirTy::Prim(*p));
            vars[*v].weak = false;
        }
    };
    let unique = |works: &dyn Fn(&HirTy) -> bool| {
        let test = |c: &[PrimTy]| {
            trial(ctx, &|| {
                set(c);
                works(&zonk(ctx, t))
            })
        };
        let mut hits = Vec::new();
        // Every choice over a few variables; more keep their defaults.
        if vs.len() <= 3 {
            go(0, &mut held.clone(), &held, &mut hits, &test);
        }
        match <[Vec<PrimTy>; 1]>::try_from(hits) {
            Ok([one]) => Some(one),
            Err(_) => None,
        }
    };
    let held_works = || {
        trial(ctx, &|| {
            set(&held);
            works(&zonk(ctx, t))
        })
    };
    match unique(works).or_else(|| unique(then)) {
        Some(one) => set(&one),
        // Several choices: the defaults, as a bare literal's.
        None if default || held_works() => set(&held),
        None => {}
    }
}

/// `t` with its open literals fixed to their defaults (a use that needs
/// the type now).
pub(super) fn fix_literals(ctx: &LowerCtx<'_>, t: &HirTy) -> HirTy {
    if !has_var(ctx, t) {
        return t.clone();
    }
    pick_literals(ctx, t, &|_| false);
    resolve(ctx, t)
}

/// `a op b` during an inference: an open side takes the other's type, and
/// both are seen through their variables. Returns the open type, for an
/// arithmetic result to carry on (`let d = c + 1; f_i64(d);`).
pub(super) fn meet(ctx: &LowerCtx<'_>, l: &mut HirExpr, r: &mut HirExpr) -> Option<HirTy> {
    if !active(ctx) || !(has_var(ctx, &l.ty) || has_var(ctx, &r.ty)) {
        return None;
    }
    // Only numbers meet: `1 ∈ s` leaves the literal to `s.contains`.
    let number = |t: &HirTy| {
        matches!(t, HirTy::Var(_))
            || matches!(zonk(ctx, t), HirTy::Prim(p) if p.is_int() || p.is_float())
    };
    if !(number(&l.ty) && number(&r.ty)) {
        return None;
    }
    let open = if has_var(ctx, &l.ty) {
        l.ty.clone()
    } else {
        r.ty.clone()
    };
    unify_weak(ctx, &l.ty, &r.ty, is_default(ctx, l) || is_default(ctx, r));
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
    // pays nothing. A body's literals are open throughout it.
    sink.infer_depth.get() > 0
        && (sink.lit_open.get().is_some()
            || sink.infer_vars.borrow().len() > sink.infer_floor.get())
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

/// Does `t` hold a variable nothing bound (a literal's always holds a
/// number type)?
pub(super) fn unsolved(ctx: &LowerCtx<'_>, t: &HirTy) -> bool {
    let rec = |t: &HirTy| unsolved(ctx, t);
    match t {
        HirTy::Var(v) => {
            let bound = ctx.env.sink.infer_vars.borrow()[root(ctx, *v as usize)]
                .ty
                .clone();
            bound.is_none_or(|b| rec(&b))
        }
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

/// `t` with its bound variables replaced, keeping an open literal's
/// variable (a later use may still fix it).
pub(super) fn resolve(ctx: &LowerCtx<'_>, t: &HirTy) -> HirTy {
    if !has_var(ctx, t) {
        return t.clone();
    }
    let rec = |t: &HirTy| resolve(ctx, t);
    let all = |ts: &[HirTy]| ts.iter().map(rec).collect::<Vec<_>>();
    match t {
        HirTy::Var(v) => {
            let r = root(ctx, *v as usize);
            let (bound, weak) = {
                let vars = ctx.env.sink.infer_vars.borrow();
                (vars[r].ty.clone(), vars[r].weak && vars[r].lit)
            };
            match bound {
                Some(_) if weak => {
                    HirTy::Var(u32::try_from(r).expect("inference variable overflow"))
                }
                Some(b) => rec(&b),
                None => t.clone(),
            }
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

/// Branches of an `if` / `match` meeting during an inference: an open
/// one takes the other's type (`true`: they agree).
pub(super) fn join(ctx: &LowerCtx<'_>, a: &HirExpr, b: &HirExpr) -> bool {
    let weak = is_default(ctx, a) || is_default(ctx, b);
    agree(ctx, &a.ty, &b.ty, weak)
}

/// Two values that must have one type (branches, `break` values), one of
/// them open: unified (`true`: they agree). A `none` and a value make no
/// sum here.
pub(super) fn agree(ctx: &LowerCtx<'_>, a: &HirTy, b: &HirTy, weak: bool) -> bool {
    let plain = |t: &HirTy| {
        !matches!(
            zonk(ctx, t),
            HirTy::Sum(_) | HirTy::None | HirTy::Unknown | HirTy::Never
        )
    };
    active(ctx)
        && (has_var(ctx, a) || has_var(ctx, b))
        && plain(a)
        && plain(b)
        && unify_weak(ctx, a, b, weak)
}

/// A branch value typed by a literal's default (nothing expected it), as
/// opposed to one its slot or a typed operand fixed.
fn is_default(ctx: &LowerCtx<'_>, e: &HirExpr) -> bool {
    match &e.kind {
        HirExprKind::Const(_) => ctx.env.sink.default_lits.borrow().contains(&e.span),
        HirExprKind::Block(b) => b.trailing.as_ref().is_some_and(|t| is_default(ctx, t)),
        _ => false,
    }
}

/// `coerce` during an inference: a value meeting a slot where either
/// side is open unifies them (`true`: they agree as they are). A value
/// the slot converts (into a sum, an `any`) has its type fixed first.
pub(super) fn unify_value(ctx: &LowerCtx<'_>, slot: &HirTy, value: &mut HirExpr) -> bool {
    if !active(ctx) || !(has_var(ctx, slot) || has_var(ctx, &value.ty)) {
        return false;
    }
    match slot {
        // `x = i` into an `i32 | none`: the one number member of its kind.
        HirTy::Sum(ms) => {
            if let Some(d) = literal_default(ctx, &value.ty) {
                let mut hits = ms.iter().filter(|m| same_kind(m, &d));
                match (hits.next(), hits.next()) {
                    (Some(m), None) => {
                        unify_weak(ctx, m, &value.ty, false);
                    }
                    _ => {
                        force_default(ctx, &value.ty);
                    }
                }
            } else if let HirTy::Sum(vs) = &value.ty {
                // `(0..0).min()` into an `i32 | none`: member by member.
                if vs.len() == ms.len() && unify_weak(ctx, slot, &value.ty, false) {
                    return true;
                }
            } else {
                unify_member(ctx, ms, &value.ty);
            }
        }
        // `let s: any Shape = 7`: the number type that has the bounds.
        HirTy::AnyTrait(bounds) => {
            pick_literals(ctx, &value.ty, &|t| {
                bounds.iter().all(|b| ctx.implements_trait(t, b, 0))
            });
        }
        HirTy::Any => pick_literals(ctx, &value.ty, &|_| false),
        _ => {
            // Agreeing types need nothing more; a closure into a `fn` slot
            // still meets the slot's bounds (`coerce`).
            // A typed slot is a use that fixes the literal.
            let weak = is_default(ctx, value) && has_var(ctx, slot);
            if unify_weak(ctx, slot, &value.ty, weak) {
                value.ty = resolve(ctx, &value.ty);
                return resolve(ctx, slot) == value.ty;
            }
        }
    }
    value.ty = zonk(ctx, &value.ty);
    false
}

/// A generic type instantiated over open literals (`B { v: 3 }`): its
/// bounds fix them where one number type meets them.
pub(super) fn unify_def_bounds(ctx: &LowerCtx<'_>, base: &str, args: &[HirTy]) {
    if !active(ctx) || !args.iter().any(|a| has_var(ctx, a)) {
        return;
    }
    let defs = &ctx.env.defs;
    let Some((generics, where_clause)) = defs
        .generic_struct(base)
        .map(|d| (&d.generics, &d.where_clause))
        .or_else(|| {
            defs.generic_union(base)
                .map(|u| (&u.generics, &u.where_clause))
        })
        .or_else(|| {
            defs.generic_enum(base)
                .map(|e| (&e.generics, &e.where_clause))
        })
    else {
        return;
    };
    let subst = super::mono::generic_subst(generics, args);
    let mut rcx = LowerCtx::new(ctx.env, &subst, None, ctx.here());
    rcx.current_module = module_of(base);
    let mut goals: Vec<(HirTy, &Vec<Ty>)> = Vec::new();
    for gp in &generics.params {
        if let rokc_ast::GenericParam::Type { name, bounds, .. } = gp {
            if let Some(t) = subst.get(name).filter(|t| has_var(ctx, t)) {
                goals.push((t.clone(), bounds));
            }
        }
    }
    for w in where_clause {
        let t = rcx.resolve_ast_ty(&w.ty);
        if has_var(ctx, &t) {
            goals.push((t, &w.bounds));
        }
    }
    for (t, bounds) in goals {
        for b in bounds {
            if let Some(want) = rcx.bound_ref(b, &rcx.current_module, |x| rcx.resolve_ast_ty(x)) {
                let _ = rcx.implements_trait(&t, &want, 0);
            }
        }
    }
}

/// An open value meeting a sum (`reader = spawn(..)` into a
/// `Task<i32> | none`): the one member of its shape takes it.
pub(super) fn unify_member(ctx: &LowerCtx<'_>, ms: &[HirTy], t: &HirTy) -> bool {
    if !active(ctx) || !has_var(ctx, t) || matches!(t, HirTy::Var(_)) {
        return false;
    }
    let base = |t: &HirTy| match t {
        HirTy::Struct(id) => ctx
            .struct_def(*id)
            .name
            .split("__")
            .next()
            .map(str::to_owned),
        HirTy::Enum(id) => ctx.enum_def(*id).name.split("__").next().map(str::to_owned),
        HirTy::Tuple(ts) => Some(format!("({})", ts.len())),
        _ => None,
    };
    let Some(want) = base(t) else {
        return false;
    };
    let mut hits = ms
        .iter()
        .filter(|m| base(m).as_deref() == Some(want.as_str()));
    match (hits.next(), hits.next()) {
        (Some(m), None) => unify_weak(ctx, m, t, false),
        _ => false,
    }
}

/// `a` and `b` agree, binding open variables.
pub(super) fn unify(ctx: &LowerCtx<'_>, a: &HirTy, b: &HirTy) -> bool {
    unify_weak(ctx, a, b, false)
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
        // A function that throws nothing fits one that may throw.
        _ => match (fn_sig(lhs), fn_sig(rhs)) {
            (Some((lp, lr)), Some((rp, rr))) => {
                all(lp, rp)
                    && match (lr, rr) {
                        (HirTy::Throws { ok, .. }, r) if !matches!(r, HirTy::Throws { .. }) => {
                            rec(ok, r)
                        }
                        (l, HirTy::Throws { ok, .. }) if !matches!(l, HirTy::Throws { .. }) => {
                            rec(l, ok)
                        }
                        _ => rec(lr, rr),
                    }
            }
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
        let mut t = t.clone();
        unify_value(&probe, &hint, &mut t);
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
    let mut stmts = Vec::new();
    expr::lower_let_tuple(ctx, names, tuple_rest, None, init, span, &mut stmts);
    ctx.infer_vars = outer;
    let own = first..sink.infer_vars.borrow().len();
    if own.is_empty() || outer || !own.clone().any(|v| unsolved(ctx, &HirTy::Var(v as u32))) {
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
        resolve(ctx, &open)
    });
    sink.infer_depth.set(sink.infer_depth.get() - 1);
    let conflict = own.into_iter().find_map(|v| {
        let r = root(ctx, v);
        let vars = sink.infer_vars.borrow();
        let (at, used) = vars[v]
            .conflict
            .clone()
            .or_else(|| vars[r].conflict.clone())?;
        let have = vars[r].ty.clone()?;
        Some((at, used, vars[v].fixed, have))
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
        (Some(t), None) if !unsolved(ctx, &t) => {
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
    let stmt = expr::lower_stmt(ctx, s);
    ctx.infer_vars = outer;
    let own = first..sink.infer_vars.borrow().len();
    // Nothing open, inside an enclosing inference (which solves it), or
    // only literals, which the body's end solves.
    if own.is_empty() || outer || !own.clone().any(|v| unsolved(ctx, &HirTy::Var(v as u32))) {
        sink.infer_depth.set(sink.infer_depth.get() - 1);
        return stmt;
    }
    let open = ctx.lookup(name).map_or(HirTy::Unknown, |l| ctx.var_ty(l));
    let solved = has_var(ctx, &open).then(|| {
        probe_tail(ctx, rest, block);
        // A closure's result is only what its body returns.
        match resolve(ctx, &open) {
            HirTy::Closure {
                args,
                ret,
                env,
                shares,
                moved,
            } if unsolved(ctx, &ret) => HirTy::Closure {
                args,
                ret: Box::new(HirTy::Unknown),
                env,
                shares,
                moved,
            },
            HirTy::Fn(args, ret, b) if unsolved(ctx, &ret) => {
                HirTy::Fn(args, Box::new(HirTy::Unknown), b)
            }
            t => t,
        }
    });
    sink.infer_depth.set(sink.infer_depth.get() - 1);
    let conflict = own.into_iter().find_map(|v| {
        let r = root(ctx, v);
        let vars = sink.infer_vars.borrow();
        let (at, used) = vars[v]
            .conflict
            .clone()
            .or_else(|| vars[r].conflict.clone())?;
        let have = vars[r].ty.clone()?;
        Some((at, used, vars[v].fixed, have, vars[r].weak))
    });
    let conflict = conflict.map(|(at, used, fixed, have, _)| {
        let (used, have) = (
            ctx.ty_name(&zonk(ctx, &used)),
            ctx.ty_name(&zonk(ctx, &have)),
        );
        rokc_diagnostic::RokDiagnostic::error(
            Code::MISMATCHED_TYPES,
            at,
            format!(
                "conflicting uses of `{name}` (`{}`): `{used}` here, but `{have}` fixed at {}:{}",
                ctx.ty_name(&open),
                fixed.line,
                fixed.col
            ),
        )
        .with_secondary(fixed, format!("`{name}`'s type fixed here"))
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
            } else if unsolved(ctx, &t) {
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

/// A literal-bound `let` used as another number type than the one it
/// has: the fix goes on the declaration (`let c: i32 = 0`, `let c =
/// 0.0`), not a cast at the use. `None` for other mismatches.
pub(super) fn let_literal_conflict(
    ctx: &LowerCtx<'_>,
    slot: &HirTy,
    value: &HirExpr,
) -> Option<rokc_diagnostic::RokDiagnostic> {
    let HirExprKind::Local(l) = value.kind else {
        return None;
    };
    let (src, decl) = ctx.lit_lets.get(&l)?.clone();
    let name = ctx.locals.get(l.0 as usize)?.name.clone()?;
    let HirTy::Var(v) = ctx.var_ty(l) else {
        return None;
    };
    let r = root(ctx, v as usize);
    let (have, weak, fixed) = {
        let vars = ctx.env.sink.infer_vars.borrow();
        (vars[r].ty.clone()?, vars[r].weak, vars[r].fixed)
    };
    let number = |t: &HirTy| matches!(t, HirTy::Prim(p) if p.is_int() || p.is_float());
    let float = |t: &HirTy| matches!(t, HirTy::Prim(p) if p.is_float());
    let used_ty = zonk(ctx, slot);
    if !(number(&used_ty) && number(&have)) || used_ty == have {
        return None;
    }
    let (used, have_s) = (ctx.ty_name(&used_ty), ctx.ty_name(&have));
    let at = value.span;
    if !weak {
        return Some(
            rokc_diagnostic::RokDiagnostic::error(
                Code::MISMATCHED_TYPES,
                at,
                format!(
                    "conflicting uses of `{name}`: `{used}` here, but `{have_s}` fixed at {}:{}",
                    fixed.line, fixed.col
                ),
            )
            .with_secondary(fixed, format!("`{name}`'s type fixed here"))
            .with_help(format!(
                "rok never converts numbers implicitly; say the type at the declaration \
                 (`let {name}: {have_s} = ..`) and convert the other use with `as`"
            )),
        );
    }
    if !float(&used_ty) || float(&have) {
        return None;
    }
    let int = !float(&have);
    let kind = if int { "an integer" } else { "a float" };
    let decimal = src
        .chars()
        .all(|c| c.is_ascii_digit() || c == '_' || c == '-');
    let help = if int && decimal {
        format!(
            "rok never converts numbers implicitly; make it a float at the declaration: \
             `let {name} = {src}.0;` (or `let {name}: {used} = {src};`)"
        )
    } else {
        format!(
            "rok never converts numbers implicitly; make it a float at the declaration: \
             `let {name}: {used} = {src};`"
        )
    };
    Some(
        rokc_diagnostic::RokDiagnostic::error(
            Code::MISMATCHED_TYPES,
            at,
            format!(
                "`{name}` is used as `{used}` here, but it holds {kind} (`{have_s}`) from `{src}` \
                 at {}:{}",
                decl.line, decl.col
            ),
        )
        .with_secondary(decl, format!("`{name}` declared here"))
        .with_help(help),
    )
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

/// A body whose unsuffixed literals are integer / float variables: they
/// unify with their uses as the body is lowered, and at its end each takes
/// what it was fixed to, or its default (`close_scope`).
pub(super) struct Scope {
    outer: Option<Span>,
    lits: usize,
    insts: usize,
    extras: usize,
}

/// Open the literal scope of a body: every unsuffixed literal lowered
/// until it closes is a variable of it (parameter defaults and field
/// defaults the body fills in too).
pub(super) fn open_scope(env: &Env<'_>, at: Span) -> Scope {
    let sink = &env.sink;
    let s = Scope {
        outer: sink.lit_open.get(),
        lits: sink.lit_vars.borrow().len(),
        insts: sink.open_insts.borrow().len(),
        extras: sink.extras.borrow().len(),
    };
    sink.lit_open.set(Some(at));
    sink.infer_depth.set(sink.infer_depth.get() + 1);
    s
}

/// End the scope: every type in `f`, its closures and `ret_tys` is
/// resolved, the instances asked for over open types are made for the
/// solved ones, and literals take their types.
pub(super) fn close_scope(
    ctx: &LowerCtx<'_>,
    s: Scope,
    f: &mut HirFunction,
    ret_tys: Option<&mut Vec<HirTy>>,
) {
    let sink = &ctx.env.sink;
    // A probe of a body over open arguments is undone; what it returns
    // may name them.
    if sink.implicit_depth.get() == 0 {
        fix_body(ctx, &s, Root::Fn(f), ret_tys);
    } else if let Some(tys) = ret_tys {
        for t in tys.iter_mut() {
            *t = resolve(ctx, t);
        }
    }
    sink.lit_vars.borrow_mut().truncate(s.lits);
    sink.open_insts.borrow_mut().truncate(s.insts);
    sink.infer_depth.set(sink.infer_depth.get() - 1);
    sink.lit_open.set(s.outer);
}

/// A constant expression (a `const` item's value, a field or parameter
/// default) is its own scope: its literals are solved before it is
/// evaluated.
pub(super) fn scoped(
    ctx: &mut LowerCtx<'_>,
    lower: impl FnOnce(&mut LowerCtx<'_>) -> HirExpr,
) -> HirExpr {
    let s = open_scope(ctx.env, ctx.here());
    let mut e = lower(ctx);
    let sink = &ctx.env.sink;
    if sink.implicit_depth.get() == 0 {
        fix_body(ctx, &s, Root::Expr(&mut e), None);
    }
    sink.lit_vars.borrow_mut().truncate(s.lits);
    sink.open_insts.borrow_mut().truncate(s.insts);
    sink.infer_depth.set(sink.infer_depth.get() - 1);
    sink.lit_open.set(s.outer);
    e
}

/// What a scope lowered.
enum Root<'r> {
    Fn(&'r mut HirFunction),
    Expr(&'r mut HirExpr),
}

fn fix_body(ctx: &LowerCtx<'_>, s: &Scope, mut root: Root<'_>, ret_tys: Option<&mut Vec<HirTy>>) {
    let sink = &ctx.env.sink;
    // Instances the body calls, made for the solved arguments.
    let mut insts: HashMap<FunctionId, Option<FunctionId>> = HashMap::default();
    {
        let pending: rokc_core::HashSet<FunctionId> = sink.open_insts.borrow()[s.insts..]
            .iter()
            .map(|p| p.id)
            .collect();
        let mut seen = |b: &HirBlock| {
            crate::each_block(b, &mut |e| {
                if let Some(id) = callee_of(e).filter(|id| pending.contains(id)) {
                    insts.insert(id, None);
                }
            });
        };
        match &root {
            Root::Fn(f) => {
                if let Some(b) = &f.body {
                    seen(b);
                }
            }
            Root::Expr(e) => seen(&HirBlock {
                stmts: Vec::new(),
                trailing: Some((**e).clone()),
                label: None,
            }),
        }
        for x in &sink.extras.borrow()[s.extras..] {
            if let Some(b) = &x.body {
                seen(b);
            }
        }
    }
    let wanted: Vec<super::mono::OpenInst> = sink.open_insts.borrow()[s.insts..]
        .iter()
        .filter(|p| insts.contains_key(&p.id))
        .cloned()
        .collect();
    let made_from = sink.extras.borrow().len();
    for p in wanted {
        let real = super::mono::solve_instance(ctx, &p);
        insts.insert(p.id, real);
    }
    let fix = Fixer { ctx, insts: &insts };
    match &mut root {
        Root::Fn(f) => fix.function(f),
        Root::Expr(e) => {
            let mut b = HirBlock {
                stmts: Vec::new(),
                trailing: Some(std::mem::replace(*e, poison(e.span))),
                label: None,
            };
            fix.block(&mut b);
            if let Some(t) = b.trailing {
                **e = t;
            }
        }
    }
    {
        let mut extras = sink.extras.borrow_mut();
        let end = made_from.min(extras.len());
        let mut own: Vec<HirFunction> = extras.drain(s.extras..end).collect();
        drop(extras);
        for x in &mut own {
            fix.function(x);
        }
        sink.extras.borrow_mut().splice(s.extras..s.extras, own);
    }
    if let Some(tys) = ret_tys {
        for t in tys.iter_mut() {
            *t = zonk(ctx, t);
        }
    }
}

const fn callee_of(e: &HirExpr) -> Option<FunctionId> {
    match &e.kind {
        HirExprKind::Call { callee: id, .. }
        | HirExprKind::FnRef(id)
        | HirExprKind::ClosureValue { fn_id: id, .. }
        | HirExprKind::CoroNew { body_fn: id, .. } => Some(*id),
        _ => None,
    }
}

/// Resolves the types of a lowered body.
struct Fixer<'c, 'a> {
    ctx: &'c LowerCtx<'a>,
    insts: &'c HashMap<FunctionId, Option<FunctionId>>,
}

impl Fixer<'_, '_> {
    fn ty(&self, t: &mut HirTy) {
        if has_var(self.ctx, t) {
            *t = zonk(self.ctx, t);
        }
    }

    fn struct_id(&self, id: &mut StructId) {
        if let HirTy::Struct(n) = zonk(self.ctx, &HirTy::Struct(*id)) {
            *id = n;
        }
    }

    fn enum_id(&self, id: &mut EnumId) {
        if let HirTy::Enum(n) = zonk(self.ctx, &HirTy::Enum(*id)) {
            *id = n;
        }
    }

    /// An f-string hole over an open literal: the runtime call is told
    /// the solved type (`rok_fmt_int`'s sign and width, `rok_fmt_f64`'s
    /// `f32`).
    fn fmt_call(&self, e: &mut HirExpr) {
        let HirExprKind::Call { callee, args } = &mut e.kind else {
            return;
        };
        let Some(HirExprKind::Cast { operand, .. }) = args.get(3).map(|a| &a.kind) else {
            return;
        };
        if !matches!(operand.ty, HirTy::Var(_)) {
            return;
        }
        let HirTy::Prim(p) = zonk(self.ctx, &operand.ty) else {
            return;
        };
        let name = self.ctx.env.decl(*callee).map(|d| d.def().name.clone());
        let int = |v: u64, ty: PrimTy, span| HirExpr {
            kind: HirExprKind::Const(ConstVal::Int {
                value: i128::from(v),
                ty,
            }),
            ty: HirTy::Prim(ty),
            span,
        };
        let set = |a: &mut HirExpr, v: u64| {
            if let HirTy::Prim(ty) = a.ty {
                *a = int(v, ty, a.span);
            }
        };
        match name.as_deref().map(leaf_name) {
            Some("rok_fmt_int") if args.len() == 6 => {
                set(&mut args[4], u64::from(p.is_signed_int()));
                set(
                    &mut args[5],
                    u64::from(p.bit_width(super::const_eval::pointer_width())),
                );
            }
            Some("rok_fmt_f64") if args.len() == 5 => {
                set(&mut args[4], u64::from(p == PrimTy::F32));
            }
            _ => {}
        }
    }

    /// A sum's members are in canonical order: a solved member may move.
    fn sum_member(&self, sum_ty: &mut HirTy, index: &mut u32) {
        if !has_var(self.ctx, sum_ty) {
            return;
        }
        let member = match &*sum_ty {
            HirTy::Sum(ms) => ms.get(*index as usize).map(|m| zonk(self.ctx, m)),
            _ => None,
        };
        self.ty(sum_ty);
        if let (Some(m), HirTy::Sum(ms)) = (member, &*sum_ty) {
            if let Some(i) = ms.iter().position(|n| *n == m) {
                *index = u32::try_from(i).expect("variant overflow");
            }
        }
    }

    fn function(&self, f: &mut HirFunction) {
        for p in &mut f.params {
            self.ty(&mut p.ty);
        }
        for l in &mut f.locals {
            self.ty(&mut l.ty);
        }
        self.ty(&mut f.ret_ty);
        if let Some(b) = &mut f.body {
            self.block(b);
        }
    }

    fn block(&self, b: &mut HirBlock) {
        self.stmts(b);
        crate::each_block_mut(b, &mut |e| self.expr(e));
    }

    /// The statements of `b` itself; the expression walk reaches the
    /// nested blocks.
    fn stmts(&self, b: &mut HirBlock) {
        for s in &mut b.stmts {
            if let HirStmt::ArrayAssign { elem_ty, .. } = s {
                self.ty(elem_ty);
            }
        }
    }

    fn expr(&self, e: &mut HirExpr) {
        self.fmt_call(e);
        self.ty(&mut e.ty);
        if let HirExprKind::Block(b)
        | HirExprKind::Loop { body: b, .. }
        | HirExprKind::While { body: b, .. }
        | HirExprKind::If { then_block: b, .. } = &mut e.kind
        {
            self.stmts(b);
        }
        match &mut e.kind {
            HirExprKind::Const(c) => self.literal(c, &e.ty, e.span),
            HirExprKind::Call { callee: id, .. }
            | HirExprKind::FnRef(id)
            | HirExprKind::ClosureValue { fn_id: id, .. }
            | HirExprKind::CoroNew { body_fn: id, .. } => {
                if let Some(Some(real)) = self.insts.get(id) {
                    *id = *real;
                }
            }
            _ => {}
        }
        match &mut e.kind {
            HirExprKind::CallIndirect { ret_ty, .. }
            | HirExprKind::Loop {
                result_ty: ret_ty, ..
            } => {
                self.ty(ret_ty);
            }
            HirExprKind::Cast { from_ty, to_ty, .. } => {
                self.ty(from_ty);
                self.ty(to_ty);
            }
            HirExprKind::ClosureValue {
                env_ty,
                arg_tys,
                ret_ty,
                ..
            }
            | HirExprKind::CallClosure {
                arg_tys,
                ret_ty,
                env_ty,
                ..
            } => {
                self.ty(env_ty);
                arg_tys.iter_mut().for_each(|t| self.ty(t));
                self.ty(ret_ty);
            }
            HirExprKind::Match { arms, scrutinee } => {
                self.ty(&mut scrutinee.ty);
                for a in arms {
                    self.pattern(&mut a.pat);
                    self.retype(&mut a.pat, &scrutinee.ty);
                }
            }
            HirExprKind::FieldRead {
                struct_id,
                field_ty,
                ..
            } => {
                self.struct_id(struct_id);
                self.ty(field_ty);
            }
            HirExprKind::StructInit { struct_id, .. }
            | HirExprKind::WithField { struct_id, .. } => {
                self.struct_id(struct_id);
            }
            HirExprKind::EnumVariantInit { enum_id, .. } => self.enum_id(enum_id),
            HirExprKind::SumWrap {
                sum_ty,
                variant_index,
                ..
            } => self.sum_member(sum_ty, variant_index),
            HirExprKind::TupleField { field_ty, .. } => self.ty(field_ty),
            HirExprKind::ArrayInit { elem_ty, .. }
            | HirExprKind::ArrayRepeat { elem_ty, .. }
            | HirExprKind::ArrayIndex { elem_ty, .. }
            | HirExprKind::BufNew { elem: elem_ty, .. }
            | HirExprKind::BufGet { elem: elem_ty, .. }
            | HirExprKind::BufSet { elem: elem_ty, .. }
            | HirExprKind::BufAtomic { elem: elem_ty, .. }
            | HirExprKind::RawRead { ty: elem_ty, .. }
            | HirExprKind::RawWrite { ty: elem_ty, .. }
            | HirExprKind::SizeOf { ty: elem_ty, .. } => self.ty(elem_ty),
            HirExprKind::UnionWrite {
                struct_id,
                from_ty,
                storage_ty,
                ..
            } => {
                self.struct_id(struct_id);
                self.ty(from_ty);
                self.ty(storage_ty);
            }
            HirExprKind::UnionRead {
                struct_id,
                storage_ty,
                field_ty,
                ..
            } => {
                self.struct_id(struct_id);
                self.ty(storage_ty);
                self.ty(field_ty);
            }
            _ => {}
        }
    }

    fn pattern(&self, p: &mut HirPattern) {
        match p {
            HirPattern::Binding { ty, .. } => self.ty(ty),
            HirPattern::At { ty, sub, .. } => {
                self.ty(ty);
                self.pattern(sub);
            }
            HirPattern::Or(ps) | HirPattern::Tuple(ps) => {
                ps.iter_mut().for_each(|p| self.pattern(p));
            }
            HirPattern::Struct { struct_id, fields } => {
                self.struct_id(struct_id);
                fields.iter_mut().for_each(|(_, p)| self.pattern(p));
            }
            HirPattern::EnumVariant { enum_id, args, .. } => {
                self.enum_id(enum_id);
                args.iter_mut().for_each(|p| self.pattern(p));
            }
            HirPattern::SumVariant {
                sum_ty,
                payload_ty,
                variant_index,
                ..
            } => {
                self.sum_member(sum_ty, variant_index);
                if let Some(t) = payload_ty {
                    self.ty(t);
                }
            }
            HirPattern::SumMember {
                sum_ty,
                payload_ty,
                sub,
                variant_index,
            } => {
                self.sum_member(sum_ty, variant_index);
                self.ty(payload_ty);
                self.pattern(sub);
            }
            HirPattern::Wildcard | HirPattern::Literal { .. } | HirPattern::Range { .. } => {}
        }
    }

    /// Literal and range patterns take the solved scrutinee's type.
    fn retype(&self, p: &mut HirPattern, ty: &HirTy) {
        match (p, ty) {
            (HirPattern::Literal { value, prim }, HirTy::Prim(to)) if *prim != *to => {
                if matches!(value, ConstVal::Int { .. } | ConstVal::Float { .. }) {
                    *prim = *to;
                    self.literal(value, ty, self.ctx.here());
                }
            }
            (
                HirPattern::Range {
                    start, end, prim, ..
                },
                HirTy::Prim(to),
            ) if *prim != *to => {
                *prim = *to;
                for v in [start, end].into_iter().flatten() {
                    self.literal(v, ty, self.ctx.here());
                }
            }
            (HirPattern::Tuple(ps), HirTy::Tuple(ts)) if ps.len() == ts.len() => {
                for (p, t) in ps.iter_mut().zip(ts) {
                    self.retype(p, t);
                }
            }
            (HirPattern::Or(ps), _) => ps.iter_mut().for_each(|p| self.retype(p, ty)),
            (HirPattern::At { sub, .. }, _) => self.retype(sub, ty),
            (
                HirPattern::SumMember {
                    sub, payload_ty, ..
                },
                _,
            ) => {
                let t = payload_ty.clone();
                self.retype(sub, &t);
            }
            (HirPattern::Struct { struct_id, fields }, _) => {
                let d = self.ctx.struct_def(*struct_id);
                for (i, p) in fields {
                    if let Some(f) = d.fields.get(*i as usize) {
                        self.retype(p, &f.ty);
                    }
                }
            }
            (
                HirPattern::EnumVariant {
                    enum_id,
                    variant_index,
                    args,
                },
                _,
            ) => {
                let d = self.ctx.enum_def(*enum_id);
                let Some(t) = d
                    .variants
                    .get(*variant_index as usize)
                    .and_then(|v| v.payload_ty.clone())
                else {
                    return;
                };
                match (args.as_mut_slice(), &t) {
                    ([one], _) => self.retype(one, &t),
                    (ps, HirTy::Tuple(ts)) if ps.len() == ts.len() => {
                        for (p, t) in ps.iter_mut().zip(ts) {
                            self.retype(p, t);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// A literal takes the type its variable was solved to.
    fn literal(&self, c: &mut ConstVal, ty: &HirTy, at: Span) {
        let HirTy::Prim(to) = *ty else {
            return;
        };
        match c {
            ConstVal::Int { value, ty } if *ty != to && to.is_int() => {
                if !super::pattern::int_fits(*value, to) {
                    let src = self.ctx.src(at).unwrap_or("..").to_owned();
                    self.ctx.emit_error_at(
                        Code::LITERAL_OUT_OF_RANGE,
                        at,
                        format!("integer literal `{src}` out of range for `{}`", to.name()),
                    );
                }
                *ty = to;
            }
            ConstVal::Float { bits, ty } if *ty == PrimTy::F64 && to == PrimTy::F32 => {
                #[allow(clippy::cast_possible_truncation)]
                let f = f64::from_bits(*bits) as f32;
                *bits = u64::from(f.to_bits());
                *ty = to;
            }
            _ => {}
        }
    }
}
