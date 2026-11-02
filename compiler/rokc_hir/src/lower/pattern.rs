//! `match`, patterns and exhaustiveness.

#[allow(clippy::wildcard_imports)]
use super::*;
use rokc_diagnostic::Code;

pub(super) fn lower_match(
    ctx: &mut LowerCtx<'_>,
    scrutinee: &Expr,
    arms: &[AstMatchArm],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let scrut_h = lower_expected(ctx, scrutinee, HirTy::Unknown);
    let scrut_ty = scrut_h.ty.clone();
    if scrut_ty == HirTy::Unknown {
        return scrut_h;
    }
    // A known slot type (`let x: T = match ...`) unifies the arms.
    let expected = ctx
        .current_expected()
        .cloned()
        .filter(|t| *t != HirTy::Unknown);
    let mut arm_h: Vec<HirMatchArm> = Vec::with_capacity(arms.len());
    let mut ended = Vec::new();
    // A pattern that failed to lower stands in as `_`: reachability
    // over it would report errors of its own.
    let mut bad_pattern = false;
    for arm in arms {
        let mark = ctx.unnarrowed.len();
        ctx.push_scope();
        let since = ctx.env.sink.diagnostics.borrow().len();
        let pat = inner_layer_binding(ctx, &arm.pat, &scrut_ty, &arm_h)
            .unwrap_or_else(|| lower_pattern(ctx, &arm.pat, &scrut_ty));
        bad_pattern |= ctx.env.sink.diagnostics.borrow()[since..]
            .iter()
            .any(rokc_diagnostic::RokDiagnostic::is_error);
        let guard = arm.guard.as_ref().map(|g| lower_cond(ctx, g));
        let cells = ctx.take_cells(arm.body.span);
        let body = super::expr::with_lets(cells, lower_expr(ctx, &arm.body));
        ctx.pop_scope();
        ended.extend(super::expr::branch_unnarrowings(ctx, mark));
        arm_h.push(HirMatchArm { pat, guard, body });
    }
    super::expr::rejoin_unnarrowings(ctx, ended);
    // `let x: T = v else { ... };` is parsed as a two-arm match whose
    // second arm is the else block (parser desugar).
    let is_let_else = matches!(
        arms.first().map(|a| &a.pat.kind),
        Some(AstPatternKind::Binding { name, .. } | AstPatternKind::TypeCheck { binder: Some(name), .. })
            if name == "__letelse_v"
    );
    // Spec 3.12.5 / 3.12.6: exhaustiveness and unreachable arms.
    if !is_let_else && !bad_pattern {
        // `if let` / `while let` desugar to `match { pat => .., _ => .. }`
        // with `_` at the `if` keyword; an irrefutable `pat` makes that
        // arm unreachable by construction, so don't warn (matched by where
        // it starts: no written arm starts at its `match`).
        // A `catch` dispatch reports its own unreachable arms.
        let catch_dispatch = matches!(&scrutinee.kind,
            ExprKind::Path(p) if p.segments.len() == 1 && p.segments[0].name.starts_with('#'));
        let synthetic: Vec<bool> = arms
            .iter()
            .map(|a| {
                catch_dispatch
                    || (matches!(a.pat.kind, AstPatternKind::Wildcard)
                        && a.pat.span.file == span.file
                        && a.pat.span.start == span.start)
            })
            .collect();
        check_match_arms(ctx, &scrut_ty, &arm_h, &synthetic);
    }

    // Result type: the expected slot, else the first arm that produces
    // a value; every other value-producing arm must agree.
    let inferred = expected.is_none();
    let result_ty = expected.unwrap_or_else(|| {
        arm_h
            .iter()
            .find(|a| !hir_expr_diverges(&a.body))
            .map_or(HirTy::Unit, |a| a.body.ty.clone())
    });
    let mut out = Vec::with_capacity(arm_h.len());
    for (i, mut a) in arm_h.into_iter().enumerate() {
        // A unit-typed match (statement position) discards arm values.
        if !hir_expr_diverges(&a.body) && (result_ty != HirTy::Unit || is_let_else) {
            if is_let_else && i == 1 {
                ctx.emit_error(
                    Code::LET_ELSE_NOT_DIVERGING,
                    "the `else` block of `let ... else` must diverge (end in `return`, \
                     `break`, `continue` or `throw`)"
                        .to_owned(),
                );
            } else if inferred
                && a.body.ty != HirTy::Unknown
                && !coercible(ctx, &result_ty, &a.body.ty)
            {
                // No sum type is inferred from differing arms.
                ctx.emit_error(
                    Code::BRANCH_TYPES,
                    format!(
                        "`match` arms have different types `{}` and `{}` (arm {}) - rok does \
                         not infer a sum type; annotate the slot (`let v: {} | {} = match ...`)",
                        ctx.ty_name(&result_ty),
                        ctx.ty_name(&a.body.ty),
                        i + 1,
                        ctx.ty_name(&result_ty),
                        ctx.ty_name(&a.body.ty)
                    ),
                );
            } else {
                a.body = coerce(ctx, &result_ty, a.body, &|| format!("match arm {}", i + 1));
            }
        }
        out.push(a);
    }
    HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(scrut_h),
            arms: out,
        },
        ty: result_ty,
        span,
    }
}

/// Spec 2.6.4 / 2.6.5: patterns act on a nested sum's outer layer; a
/// catch-all after arms covering every outer member binds the inner one
/// (`none => .., v => ..` on `(i32 | none)?` gives `v: i32 | none`).
fn inner_layer_binding(
    ctx: &mut LowerCtx<'_>,
    pat: &AstPattern,
    scrut_ty: &HirTy,
    before: &[HirMatchArm],
) -> Option<HirPattern> {
    let (HirTy::Sum(vs), AstPatternKind::Binding { name, sub: None }) = (scrut_ty, &pat.kind)
    else {
        return None;
    };
    if name == "_" || ctx.enum_path(&[name.as_str()]).is_some() {
        return None;
    }
    let mut covered = Vec::new();
    for a in before.iter().filter(|a| a.guard.is_none()) {
        covered_variants(&a.pat, &mut covered);
    }
    let mut rest =
        (0..vs.len()).filter(|i| !covered.contains(&u32::try_from(*i).unwrap_or(u32::MAX)));
    let (i, None) = (rest.next()?, rest.next()) else {
        return None;
    };
    let inner = vs[i].clone();
    if !matches!(inner, HirTy::Sum(_)) {
        return None;
    }
    let local = ctx.declare_bound(name, inner.clone(), pat.span);
    Some(HirPattern::SumVariant {
        sum_ty: scrut_ty.clone(),
        variant_index: u32::try_from(i).expect("variant overflow"),
        binder: Some(local),
        payload_ty: Some(inner),
    })
}

/// Matches every value of its type (no refutable sub-pattern).
pub(super) fn hir_pattern_irrefutable(p: &HirPattern) -> bool {
    p.is_irrefutable()
}

/// Enum variants / sum variants fully covered by `p`.
pub(super) fn covered_variants(p: &HirPattern, out: &mut Vec<u32>) {
    match p {
        HirPattern::EnumVariant {
            variant_index,
            args,
            ..
        } if args.iter().all(hir_pattern_irrefutable) => {
            out.push(*variant_index);
        }
        HirPattern::SumVariant { variant_index, .. } => out.push(*variant_index),
        HirPattern::At { sub, .. } => covered_variants(sub, out),
        HirPattern::Or(alts) => {
            for a in alts {
                covered_variants(a, out);
            }
        }
        _ => {}
    }
}

/// Pattern-matrix constructor (usefulness analysis, Maranget 2007).
#[derive(Clone, Debug, PartialEq)]
enum Ctor {
    Bool(bool),
    Variant(u32),
    /// The only constructor of a tuple / struct.
    Single,
    /// An integer / `char` literal or range as `lo..=hi`. Ranges never make a
    /// match exhaustive (3.12.5) but subsume each other for unreachable arms
    /// (3.12.6).
    Int(i128, i128),
    /// A literal of another open domain (strings, floats): only equal
    /// spellings are the same constructor.
    Open(String),
}

/// A literal's point in the `Ctor::Int` domain. `u128` above `i128::MAX`
/// saturates, so such patterns overlap.
fn int_point(v: &ConstVal) -> Option<i128> {
    match v {
        ConstVal::Int { value, .. } => Some(*value),
        ConstVal::UInt { value, .. } => Some(i128::try_from(*value).unwrap_or(i128::MAX)),
        ConstVal::Char(c) => Some(i128::from(*c)),
        _ => None,
    }
}

#[derive(Clone, Debug)]
enum Pat {
    Wild,
    C(Ctor, Vec<Pat>),
    Or(Vec<Pat>),
}

fn simplify(ctx: &LowerCtx<'_>, p: &HirPattern) -> Pat {
    match p {
        HirPattern::Wildcard | HirPattern::Binding { .. } => Pat::Wild,
        HirPattern::At { sub, .. } => simplify(ctx, sub),
        HirPattern::Literal {
            value: ConstVal::Bool(b),
            ..
        } => Pat::C(Ctor::Bool(*b), Vec::new()),
        HirPattern::Literal { value, .. } => match int_point(value) {
            Some(v) => Pat::C(Ctor::Int(v, v), Vec::new()),
            None => Pat::C(Ctor::Open(format!("{value:?}")), Vec::new()),
        },
        HirPattern::Range {
            start,
            end,
            inclusive,
            ..
        } => {
            let lo = start.as_ref().map(int_point);
            let hi = end.as_ref().map(int_point);
            let c = match (lo, hi) {
                (Some(None), _) | (_, Some(None)) => {
                    Ctor::Open(format!("{start:?}..{inclusive}{end:?}"))
                }
                (lo, hi) => {
                    let lo = lo.flatten().unwrap_or(i128::MIN);
                    let hi = match hi.flatten() {
                        Some(h) if !*inclusive => h.saturating_sub(1),
                        Some(h) => h,
                        None => i128::MAX,
                    };
                    Ctor::Int(lo, hi)
                }
            };
            Pat::C(c, Vec::new())
        }
        HirPattern::Or(alts) => Pat::Or(alts.iter().map(|a| simplify(ctx, a)).collect()),
        HirPattern::Tuple(parts) => Pat::C(
            Ctor::Single,
            parts.iter().map(|a| simplify(ctx, a)).collect(),
        ),
        HirPattern::Struct { struct_id, fields } => {
            let n = ctx.struct_def(*struct_id).fields.len();
            let mut args = vec![Pat::Wild; n];
            for (i, f) in fields {
                if let Some(slot) = args.get_mut(*i as usize) {
                    *slot = simplify(ctx, f);
                }
            }
            Pat::C(Ctor::Single, args)
        }
        HirPattern::EnumVariant {
            enum_id,
            variant_index,
            args,
        } => {
            let arity = ctx
                .enum_def(*enum_id)
                .variants
                .get(*variant_index as usize)
                .map_or(args.len(), |v| v.arity);
            let mut sub: Vec<Pat> = args.iter().map(|a| simplify(ctx, a)).collect();
            sub.resize(arity, Pat::Wild);
            Pat::C(Ctor::Variant(*variant_index), sub)
        }
        HirPattern::SumVariant { variant_index, .. } => {
            Pat::C(Ctor::Variant(*variant_index), vec![Pat::Wild])
        }
        HirPattern::SumMember {
            variant_index, sub, ..
        } => Pat::C(Ctor::Variant(*variant_index), vec![simplify(ctx, sub)]),
    }
}

/// Every constructor of `ty` with its field types; `None` = an open
/// domain (only `_` covers it).
fn ctors_of(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<Vec<(Ctor, Vec<HirTy>)>> {
    Some(match ty {
        HirTy::Prim(PrimTy::Bool) => vec![
            (Ctor::Bool(false), Vec::new()),
            (Ctor::Bool(true), Vec::new()),
        ],
        HirTy::Tuple(ts) => vec![(Ctor::Single, ts.clone())],
        HirTy::Struct(id) => vec![(
            Ctor::Single,
            ctx.struct_def(*id)
                .fields
                .iter()
                .map(|f| f.ty.clone())
                .collect(),
        )],
        HirTy::Enum(id) => ctx
            .enum_def(*id)
            .variants
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let tys = match (&v.payload_ty, v.arity) {
                    (_, 0) | (None, _) => Vec::new(),
                    (Some(t), 1) => vec![t.clone()],
                    (Some(HirTy::Tuple(ts)), _) => ts.clone(),
                    (Some(_), n) => vec![HirTy::Unknown; n],
                };
                (
                    Ctor::Variant(u32::try_from(i).expect("variant overflow")),
                    tys,
                )
            })
            .collect(),
        HirTy::Sum(vs) => vs
            .iter()
            .enumerate()
            .map(|(i, v)| {
                (
                    Ctor::Variant(u32::try_from(i).expect("variant overflow")),
                    vec![v.clone()],
                )
            })
            .collect(),
        _ => return None,
    })
}

fn sub_tys(ctx: &LowerCtx<'_>, ty: &HirTy, c: &Ctor, arity: usize) -> Vec<HirTy> {
    ctors_of(ctx, ty)
        .and_then(|cs| cs.into_iter().find(|(k, _)| k == c).map(|(_, t)| t))
        .filter(|t| t.len() == arity)
        .unwrap_or_else(|| vec![HirTy::Unknown; arity])
}

/// Rows with or-patterns in the first column expanded.
fn expand_rows(rows: &[Vec<Pat>]) -> Vec<Vec<Pat>> {
    let mut out = Vec::new();
    for r in rows {
        match r.first() {
            Some(Pat::Or(alts)) => {
                let alt_rows: Vec<Vec<Pat>> = alts
                    .iter()
                    .map(|a| {
                        std::iter::once(a.clone())
                            .chain(r[1..].iter().cloned())
                            .collect()
                    })
                    .collect();
                out.extend(expand_rows(&alt_rows));
            }
            _ => out.push(r.clone()),
        }
    }
    out
}

fn specialize(rows: &[Vec<Pat>], c: &Ctor, arity: usize) -> Vec<Vec<Pat>> {
    rows.iter()
        .filter_map(|r| {
            let head: Vec<Pat> = match &r[0] {
                Pat::C(k, args) if k == c => args.clone(),
                Pat::Wild => vec![Pat::Wild; arity],
                _ => return None,
            };
            Some(head.into_iter().chain(r[1..].iter().cloned()).collect())
        })
        .collect()
}

/// Is there a value matched by `q` and by no row?
fn useful(ctx: &LowerCtx<'_>, rows: &[Vec<Pat>], q: &[Pat], tys: &[HirTy]) -> bool {
    let Some(head) = q.first() else {
        return rows.is_empty();
    };
    let rows = expand_rows(rows);
    let ty = tys.first().cloned().unwrap_or(HirTy::Unknown);
    let rest_tys = tys.get(1..).unwrap_or(&[]);
    let with = |args: Vec<Pat>, sub: Vec<HirTy>| -> (Vec<Pat>, Vec<HirTy>) {
        (
            args.into_iter().chain(q[1..].iter().cloned()).collect(),
            sub.into_iter().chain(rest_tys.iter().cloned()).collect(),
        )
    };
    match head {
        Pat::Or(alts) => alts.iter().any(|a| {
            let q2: Vec<Pat> = std::iter::once(a.clone())
                .chain(q[1..].iter().cloned())
                .collect();
            useful(ctx, &rows, &q2, tys)
        }),
        Pat::C(Ctor::Int(lo, hi), _) => {
            // Split `lo..=hi` at every row interval's edge; each piece is
            // inside or outside each row, so it specializes exactly.
            let heads: Vec<(i128, i128)> = rows
                .iter()
                .filter_map(|r| match r.first() {
                    Some(&Pat::C(Ctor::Int(a, b), _)) if a <= b => Some((a, b)),
                    _ => None,
                })
                .collect();
            let mut cuts = vec![*lo];
            for &(a, b) in &heads {
                if *lo < a && a <= *hi {
                    cuts.push(a);
                }
                if *lo <= b && b < *hi {
                    cuts.push(b + 1);
                }
            }
            cuts.sort_unstable();
            cuts.dedup();
            (0..cuts.len()).filter(|_| lo <= hi).any(|i| {
                let (p, q_hi) = (cuts[i], cuts.get(i + 1).map_or(*hi, |n| n - 1));
                let sub: Vec<Vec<Pat>> = rows
                    .iter()
                    .filter(|r| match r.first() {
                        Some(Pat::Wild) => true,
                        Some(&Pat::C(Ctor::Int(a, b), _)) => a <= p && q_hi <= b,
                        _ => false,
                    })
                    .map(|r| r[1..].to_vec())
                    .collect();
                useful(ctx, &sub, &q[1..], rest_tys)
            })
        }
        Pat::C(c, args) => {
            let (q2, t2) = with(args.clone(), sub_tys(ctx, &ty, c, args.len()));
            useful(ctx, &specialize(&rows, c, args.len()), &q2, &t2)
        }
        Pat::Wild => {
            let used: Vec<&Ctor> = rows
                .iter()
                .filter_map(|r| match &r[0] {
                    Pat::C(k, _) => Some(k),
                    _ => None,
                })
                .collect();
            match ctors_of(ctx, &ty) {
                Some(all) if !all.is_empty() && all.iter().all(|(c, _)| used.contains(&c)) => {
                    all.into_iter().any(|(c, sub)| {
                        let n = sub.len();
                        let (q2, t2) = with(vec![Pat::Wild; n], sub);
                        useful(ctx, &specialize(&rows, &c, n), &q2, &t2)
                    })
                }
                _ => {
                    let default: Vec<Vec<Pat>> = rows
                        .iter()
                        .filter(|r| matches!(r[0], Pat::Wild))
                        .map(|r| r[1..].to_vec())
                        .collect();
                    useful(ctx, &default, &q[1..], rest_tys)
                }
            }
        }
    }
}

/// One value no row matches, one pattern per column (Maranget's
/// witness), or `None` when the rows are exhaustive.
fn witness(ctx: &LowerCtx<'_>, rows: &[Vec<Pat>], tys: &[HirTy]) -> Option<Vec<String>> {
    let Some(ty) = tys.first() else {
        return rows.is_empty().then(Vec::new);
    };
    let rows = expand_rows(rows);
    let rest = &tys[1..];
    let used: Vec<&Ctor> = rows
        .iter()
        .filter_map(|r| match &r[0] {
            Pat::C(k, _) => Some(k),
            _ => None,
        })
        .collect();
    let all = ctors_of(ctx, ty).filter(|a| !a.is_empty());
    if let Some(all) = all
        .as_ref()
        .filter(|a| a.iter().all(|(c, _)| used.contains(&c)))
    {
        return all.iter().find_map(|(c, sub)| {
            let n = sub.len();
            let tys2: Vec<HirTy> = sub.iter().chain(rest).cloned().collect();
            let mut w = witness(ctx, &specialize(&rows, c, n), &tys2)?;
            let args: Vec<String> = w.drain(..n).collect();
            w.insert(0, ctor_text(ctx, ty, c, &args));
            Some(w)
        });
    }
    let default: Vec<Vec<Pat>> = rows
        .iter()
        .filter(|r| matches!(r[0], Pat::Wild))
        .map(|r| r[1..].to_vec())
        .collect();
    let mut w = witness(ctx, &default, rest)?;
    let head = all
        .and_then(|a| a.into_iter().find(|(c, _)| !used.contains(&c)))
        .map_or_else(
            || "_".to_owned(),
            |(c, sub)| ctor_text(ctx, ty, &c, &vec!["_".to_owned(); sub.len()]),
        );
    w.insert(0, head);
    Some(w)
}

/// `c` applied to `args`, as source text.
fn ctor_text(ctx: &LowerCtx<'_>, ty: &HirTy, c: &Ctor, args: &[String]) -> String {
    match (c, ty) {
        (Ctor::Bool(b), _) => b.to_string(),
        (Ctor::Single, HirTy::Struct(id)) => {
            let def = ctx.struct_def(*id);
            let fields: Vec<String> = def
                .fields
                .iter()
                .zip(args)
                .filter(|(_, a)| *a != "_")
                .map(|(f, a)| format!("{}: {a}", f.name))
                .collect();
            let name = enum_base_name(&def.name);
            if fields.is_empty() {
                format!("{name} {{ .. }}")
            } else {
                format!("{name} {{ {}, .. }}", fields.join(", "))
            }
        }
        (Ctor::Single, _) if args.len() == 1 => format!("({},)", args[0]),
        (Ctor::Single, _) => format!("({})", args.join(", ")),
        (Ctor::Variant(i), HirTy::Enum(id)) => {
            let def = ctx.enum_def(*id);
            let v = &def.variants[*i as usize].name;
            let base = enum_base_name(&def.name);
            if args.is_empty() {
                format!("{base}::{v}")
            } else {
                format!("{base}::{v}({})", args.join(", "))
            }
        }
        (Ctor::Variant(i), HirTy::Sum(vs)) => ctx.ty_name(&vs[*i as usize]),
        _ => "_".to_owned(),
    }
}

/// Spec 3.12.5 / 3.12.6: report an arm no value can reach (an error),
/// then check that the unguarded arms cover the scrutinee's type.
pub(super) fn check_match_arms(
    ctx: &LowerCtx<'_>,
    scrut_ty: &HirTy,
    arms: &[HirMatchArm],
    synthetic: &[bool],
) {
    let tys = [scrut_ty.clone()];
    let mut rows: Vec<Vec<Pat>> = Vec::new();
    // The catch-all a foreign `#[non_exhaustive]` type requires is never
    // unreachable.
    let foreign = foreign_non_exhaustive(ctx, scrut_ty);
    // In an instance, reachability turns on the type arguments; the
    // definition check reported what doesn't.
    let instance = in_instance(ctx);
    for (i, a) in arms.iter().enumerate() {
        let p = simplify(ctx, &a.pat);
        let exempt = synthetic.get(i).copied().unwrap_or(false)
            || (foreign.is_some() && hir_pattern_irrefutable(&a.pat));
        if !exempt && !instance && !useful(ctx, &rows, std::slice::from_ref(&p), &tys) {
            // At the arm (its body), not the whole `match`.
            let at = if a.body.span.is_dummy() {
                ctx.here()
            } else {
                a.body.span
            };
            ctx.emit_error_at(
                Code::UNREACHABLE_ARM,
                at,
                format!(
                    "unreachable match arm {}: earlier arms already match every value it \
                     matches",
                    i + 1
                ),
            );
        }
        if a.guard.is_none() {
            rows.push(vec![p]);
        }
    }
    let catch_all = arms
        .iter()
        .any(|a| a.guard.is_none() && hir_pattern_irrefutable(&a.pat));
    if let Some(name) = foreign.filter(|_| !catch_all) {
        ctx.emit_error(
            Code::NON_EXHAUSTIVE,
            format!(
                "match on `{name}` needs a `_` arm: it is `#[non_exhaustive]` in another \
                 package"
            ),
        );
        return;
    }
    if useful(ctx, &rows, &[Pat::Wild], &tys) {
        let w = witness(ctx, &rows, &tys).and_then(|mut w| w.pop());
        check_match_exhaustiveness(ctx, scrut_ty, arms, w);
    }
}

/// Spec 3.12.5: arms must cover the scrutinee. `bool` needs both literals,
/// enums / sums every variant (guarded arms cover nothing), others a
/// catch-all.
pub(super) fn check_match_exhaustiveness(
    ctx: &LowerCtx<'_>,
    scrut_ty: &HirTy,
    arms: &[HirMatchArm],
    witness: Option<String>,
) {
    let unguarded: Vec<&HirPattern> = arms
        .iter()
        .filter(|a| a.guard.is_none())
        .map(|a| &a.pat)
        .collect();
    if unguarded.iter().any(|p| hir_pattern_irrefutable(p)) {
        return;
    }
    let arms_to_add = missing_arms(ctx, scrut_ty, &unguarded);
    let mut missing: Vec<String> = match scrut_ty {
        HirTy::Prim(PrimTy::Bool) => {
            fn bools(p: &HirPattern, seen: &mut [bool; 2]) {
                match p {
                    HirPattern::Literal {
                        value: ConstVal::Bool(b),
                        ..
                    } => seen[usize::from(*b)] = true,
                    HirPattern::Or(alts) => alts.iter().for_each(|a| bools(a, seen)),
                    HirPattern::At { sub, .. } => bools(sub, seen),
                    _ => {}
                }
            }
            let mut seen = [false, false];
            for p in &unguarded {
                bools(p, &mut seen);
            }
            [("false", seen[0]), ("true", seen[1])]
                .iter()
                .filter(|(_, s)| !s)
                .map(|(n, _)| format!("`{n}`"))
                .collect()
        }
        HirTy::Enum(eid) => {
            let mut covered = Vec::new();
            for p in &unguarded {
                covered_variants(p, &mut covered);
            }
            let def = ctx.enum_def(*eid);
            let base = enum_base_name(&def.name);
            def.variants
                .iter()
                .enumerate()
                .filter(|(i, _)| !covered.contains(&u32::try_from(*i).expect("variant overflow")))
                .map(|(_, v)| format!("`{base}::{}`", v.name))
                .collect()
        }
        HirTy::Sum(variants) => {
            let mut covered = Vec::new();
            for p in &unguarded {
                covered_variants(p, &mut covered);
            }
            variants
                .iter()
                .enumerate()
                .filter(|(i, _)| !covered.contains(&u32::try_from(*i).expect("variant overflow")))
                .map(|(_, v)| format!("`{}`", ctx.ty_name(v)))
                .collect()
        }
        HirTy::Tuple(_) | HirTy::Struct(_) => Vec::new(),
        other => vec![format!(
            "values of `{}` (add a `_` arm)",
            ctx.ty_name(other)
        )],
    };
    if missing.is_empty() {
        // Every top-level case appears but a nested one is missing
        // (`(true, _)` without `(false, _)`).
        missing.push(witness.map_or_else(
            || "some nested values (add a `_` arm)".to_owned(),
            |w| format!("`{w}`"),
        ));
    }
    ctx.emit_error(
        Code::NON_EXHAUSTIVE,
        format!(
            "non-exhaustive match on `{}`: {} not covered",
            ctx.ty_name(scrut_ty),
            missing.join(", ")
        ),
    );
    // Else the cases named above, when each is a pattern (`false`); a
    // nested witness is one of maybe several, so it gets no fix.
    let arms_to_add = arms_to_add.filter(|a| !a.is_empty());
    let complete = arms_to_add.is_some() || *scrut_ty == HirTy::Prim(PrimTy::Bool);
    let arms = arms_to_add.unwrap_or_else(|| {
        missing
            .iter()
            .filter_map(|m| m.strip_prefix('`')?.strip_suffix('`'))
            .filter(|m| !m.contains('`'))
            .map(str::to_owned)
            .collect()
    });
    let (arms, complete) = if arms.is_empty() || arms.len() > 3 {
        (vec!["_".to_owned()], true)
    } else {
        (arms, complete)
    };
    let list: Vec<String> = arms.iter().map(|a| format!("`{a} => ..`")).collect();
    let help = if arms.len() == 1 {
        format!("add {}", list[0])
    } else {
        format!("add the arms {}", list.join(", "))
    };
    ctx.help_last(Some(help));
    if complete {
        add_arms_fix(ctx, &arms);
    }
}

/// Arm patterns for the cases of `ty` no unguarded arm covers; `None`
/// when they are not cases (`bool` is, its literals).
fn missing_arms(ctx: &LowerCtx<'_>, ty: &HirTy, unguarded: &[&HirPattern]) -> Option<Vec<String>> {
    let mut covered = Vec::new();
    for p in unguarded {
        covered_variants(p, &mut covered);
    }
    let open = |i: usize| !covered.contains(&u32::try_from(i).expect("variant overflow"));
    match ty {
        HirTy::Enum(eid) => {
            let def = ctx.enum_def(*eid);
            let base = ctx.ty_name(ty);
            let base = base.split('<').next().unwrap_or_default().to_owned();
            Some(
                def.variants
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| open(*i))
                    .map(|(_, v)| {
                        let n = super::expr::variant_arg_tys(v).len();
                        if !v.named_fields.is_empty() {
                            format!("{base}::{} {{ .. }}", v.name)
                        } else if n > 0 {
                            format!("{base}::{}({})", v.name, vec!["_"; n].join(", "))
                        } else {
                            format!("{base}::{}", v.name)
                        }
                    })
                    .collect(),
            )
        }
        HirTy::Sum(cases) => Some(
            cases
                .iter()
                .enumerate()
                .filter(|(i, _)| open(*i))
                .map(|(_, c)| match c {
                    HirTy::None => "none".to_owned(),
                    t => format!("x: {}", ctx.ty_name(t)),
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Insert `arms` before the closing brace of the match just reported,
/// when it spans lines.
fn add_arms_fix(ctx: &LowerCtx<'_>, arms: &[String]) {
    let at = ctx.here();
    let Some(text) = ctx.src(at) else {
        return;
    };
    let Some(close) = text.strip_suffix('}').map(str::len) else {
        return;
    };
    let line_start = text[..close].rfind('\n').map_or(0, |i| i + 1);
    let indent = &text[line_start..close];
    if line_start == 0 || !indent.chars().all(|c| c == ' ' || c == '\t') {
        return;
    }
    let mut body = String::new();
    for a in arms {
        body.push_str(&format!("    {a} => panic(\"todo\"),\n{indent}"));
    }
    if let Some(span) = ctx.sub_span(at, close, close) {
        ctx.fix_last(span, body, "add the missing arms");
    }
}

/// Is a generic body being lowered for concrete type arguments (not
/// for its definition check)?
fn in_instance(ctx: &LowerCtx<'_>) -> bool {
    !ctx.ty_subst.is_empty()
        && !ctx
            .ty_subst
            .values()
            .any(|t| super::bounds::has_param(ctx, t))
}

/// Spec 6.2.10: a struct pattern `pat` on `ty`, a `#[non_exhaustive]`
/// struct of another package, ends in `..` (its package may add fields).
pub(super) fn struct_pattern_rest(ctx: &LowerCtx<'_>, pat: &AstPattern, ty: &HirTy) {
    let AstPatternKind::Struct {
        path,
        fields,
        rest: false,
    } = &pat.kind
    else {
        return;
    };
    let listed: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    missing_rest(ctx, pat.span, path, &listed, ty);
}

/// `struct_pattern_rest` of a pattern without `..` that lists `listed`.
pub(super) fn missing_rest(
    ctx: &LowerCtx<'_>,
    at: rokc_diagnostic::Span,
    path: &rokc_ast::Path,
    listed: &[&str],
    ty: &HirTy,
) {
    let Some(name) = foreign_non_exhaustive(ctx, ty) else {
        return;
    };
    let segs: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
    let help = if listed.is_empty() {
        format!("{} {{ .. }}", segs.join("::"))
    } else {
        format!("{} {{ {}, .. }}", segs.join("::"), listed.join(", "))
    };
    ctx.emit_error_at(
        Code::PATTERN_ARITY,
        at,
        format!(
            "pattern on `{name}` needs `..`: it is `#[non_exhaustive]` in another package, \
             which may add fields; write `{help}`"
        ),
    );
}

/// `ty`'s name when it is a `#[non_exhaustive]` struct / enum of another
/// package.
fn foreign_non_exhaustive(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<String> {
    let name = match ty {
        HirTy::Enum(id) => enum_base_name(&ctx.enum_def(*id).name).to_owned(),
        HirTy::Struct(id) => enum_base_name(&ctx.struct_def(*id).name).to_owned(),
        _ => return None,
    };
    (ctx.env.is_non_exhaustive(&name)
        && package_of(&module_of(&name)) != package_of(&ctx.current_module))
    .then(|| ctx.ty_name(ty))
}

pub(super) fn lower_pattern(
    ctx: &mut LowerCtx<'_>,
    pat: &AstPattern,
    scrut_ty: &HirTy,
) -> HirPattern {
    let typed = bare_type_patterns(ctx, pat, scrut_ty);
    let pat = typed.as_ref().unwrap_or(pat);
    let p = lower_pattern_inner(ctx, pat, scrut_ty);
    // A pattern that failed to lower still declares its names (as
    // poisoned locals) so their uses don't cascade into "cannot find".
    if matches!(p, HirPattern::Wildcard) {
        for name in pattern_bound_names(pat) {
            if ctx.lookup_in_current_scope(&name).is_none() {
                ctx.declare(Some(name), HirTy::Unknown);
            }
        }
    }
    p
}

/// Spec 3.11.1: against a sum or `any`, a bare type name (`f64 =>`) is a
/// type pattern, not a binding; elsewhere it stays a binding.
fn bare_type_patterns(
    ctx: &LowerCtx<'_>,
    pat: &AstPattern,
    scrut_ty: &HirTy,
) -> Option<AstPattern> {
    if !matches!(scrut_ty, HirTy::Sum(_) | HirTy::AnyTrait(_)) {
        return None;
    }
    let as_check = |p: &AstPattern| {
        let AstPatternKind::Binding { name, sub: None } = &p.kind else {
            return None;
        };
        if ctx.enum_path(&[name.as_str()]).is_some() {
            return None;
        }
        let ty = rokc_ast::Ty {
            kind: rokc_ast::TyKind::Path(rokc_ast::Path {
                segments: vec![rokc_ast::PathSegment {
                    name: name.clone(),
                    generics: Vec::new(),
                }],
                span: p.span,
                origin: None,
            }),
            span: p.span,
        };
        let resolved = ctx.resolve_ast_ty(&ty);
        (!matches!(resolved, HirTy::Path(_) | HirTy::None | HirTy::Unknown)).then_some(AstPattern {
            kind: AstPatternKind::TypeCheck { binder: None, ty },
            span: p.span,
        })
    };
    match &pat.kind {
        AstPatternKind::Or(alts) => {
            let new: Vec<Option<AstPattern>> = alts.iter().map(as_check).collect();
            new.iter().any(Option::is_some).then(|| AstPattern {
                kind: AstPatternKind::Or(
                    new.into_iter()
                        .zip(alts)
                        .map(|(n, a)| n.unwrap_or_else(|| a.clone()))
                        .collect(),
                ),
                span: pat.span,
            })
        }
        _ => as_check(pat),
    }
}

pub(super) fn lower_pattern_inner(
    ctx: &mut LowerCtx<'_>,
    pat: &AstPattern,
    scrut_ty: &HirTy,
) -> HirPattern {
    // A path pattern resolves where it was written (spec 6.9.9); its
    // sub-patterns set their own.
    let origin = match &pat.kind {
        AstPatternKind::Struct { path, .. } | AstPatternKind::EnumVariant { path, .. } => {
            path.origin.as_deref()
        }
        _ => None,
    };
    let outer = ctx.set_access(origin);
    let p = lower_pattern_here(ctx, pat, scrut_ty);
    ctx.access.replace(outer);
    p
}

#[allow(clippy::cognitive_complexity)] // one arm per pattern kind
fn lower_pattern_here(ctx: &mut LowerCtx<'_>, pat: &AstPattern, scrut_ty: &HirTy) -> HirPattern {
    let mismatch = |ctx: &LowerCtx<'_>, what: String| {
        if *scrut_ty != HirTy::Unknown {
            ctx.emit_error(
                Code::PATTERN_TYPE,
                format!(
                    "{what} can't match a value of type `{}`",
                    ctx.ty_name(scrut_ty)
                ),
            );
        }
        HirPattern::Wildcard
    };
    // `MyError { code }` / `E::A(x)` on an `any` value: narrow to that
    // type, then match its payload (`catch` of an untyped `throws`).
    if let (
        HirTy::AnyTrait(bounds),
        AstPatternKind::Struct { .. } | AstPatternKind::EnumVariant { .. },
    ) = (scrut_ty, &pat.kind)
    {
        if let Some(t) = super::throws::arm_filter(ctx, pat)
            .filter(|t| bounds.iter().all(|b| ctx.implements_trait(t, b, 0)))
        {
            return HirPattern::SumMember {
                sum_ty: scrut_ty.clone(),
                variant_index: ctx.any_tag(&t),
                sub: Box::new(lower_pattern_inner(ctx, pat, &t)),
                payload_ty: t,
            };
        }
    }
    match &pat.kind {
        AstPatternKind::Wildcard => HirPattern::Wildcard,
        AstPatternKind::Binding { name, sub: None } if name == "_" => HirPattern::Wildcard,
        // A bare name that is an imported variant (`use E::*` then
        // `A =>`) matches the variant rather than binding.
        AstPatternKind::Binding { name, sub: None }
            if ctx.enum_path(&[name.as_str()]).is_some() =>
        {
            let segments = vec![rokc_ast::PathSegment {
                name: name.clone(),
                generics: Vec::new(),
            }];
            let path = rokc_ast::Path {
                segments,
                span: pat.span,
                origin: None,
            };
            let variant = AstPattern {
                kind: AstPatternKind::EnumVariant {
                    path,
                    args: Vec::new(),
                },
                span: pat.span,
            };
            lower_pattern(ctx, &variant, scrut_ty)
        }
        // A bare name that is a variant of the scrutinee's enum is that
        // variant (spec 3.11, 5.7.1).
        AstPatternKind::Binding { name, sub: None }
            if variant_named(ctx, scrut_ty, name).is_some() =>
        {
            let (enum_id, idx, base) = variant_named(ctx, scrut_ty, name).expect("guard");
            let args = ctx.enum_def(enum_id).variants[idx].arity;
            if args != 0 {
                ctx.emit_error_at(
                    Code::PATTERN_ARITY,
                    pat.span,
                    format!("variant `{base}::{name}` has a payload: match it as `{name}(..)`"),
                );
            }
            HirPattern::EnumVariant {
                enum_id,
                variant_index: u32::try_from(idx).expect("variant overflow"),
                args: vec![HirPattern::Wildcard; args],
            }
        }
        AstPatternKind::Binding { name, sub } => {
            // `name @ sub`: bind the whole value, match like `sub`.
            let at = |ctx: &mut LowerCtx<'_>, local: LocalId| match sub {
                None => HirPattern::Binding {
                    local,
                    ty: scrut_ty.clone(),
                },
                Some(p) => HirPattern::At {
                    local,
                    ty: scrut_ty.clone(),
                    sub: Box::new(lower_pattern(ctx, p, scrut_ty)),
                },
            };
            binds_const(ctx, name, scrut_ty, pat.span);
            let local = ctx.declare_bound(name, scrut_ty.clone(), pat.span);
            at(ctx, local)
        }
        AstPatternKind::Literal(AstLit::None) => {
            // `none` against a sum scrutinee → match the none variant.
            if let HirTy::Sum(variants) = scrut_ty {
                if let Some(idx) = variants.iter().position(HirTy::is_none) {
                    return HirPattern::SumVariant {
                        sum_ty: scrut_ty.clone(),
                        variant_index: u32::try_from(idx).expect("variant overflow"),
                        binder: None,
                        payload_ty: None,
                    };
                }
            }
            mismatch(ctx, "a `none` pattern".to_owned())
        }
        // A literal / range on a sum tests the one member it can match
        // (spec 3.11.1: `'a'` on `char | none`).
        AstPatternKind::Literal(_) | AstPatternKind::Range { .. }
            if matches!(scrut_ty, HirTy::Sum(_)) =>
        {
            let HirTy::Sum(vs) = scrut_ty else {
                unreachable!()
            };
            let fits = |v: &HirTy| match &pat.kind {
                AstPatternKind::Literal(lit) => literal_to_const(ctx, lit, v).is_some(),
                AstPatternKind::Range { start, end, .. } => {
                    matches!(v, HirTy::Prim(p) if p.is_int())
                        && [start, end].iter().all(|b| {
                            b.as_ref()
                                .is_none_or(|l| literal_to_const(ctx, l, v).is_some())
                        })
                }
                _ => false,
            };
            let Some(i) = vs.iter().position(fits) else {
                return mismatch(ctx, "this literal pattern".to_owned());
            };
            let member = vs[i].clone();
            HirPattern::SumMember {
                sum_ty: scrut_ty.clone(),
                variant_index: u32::try_from(i).expect("variant overflow"),
                sub: Box::new(lower_pattern_inner(ctx, pat, &member)),
                payload_ty: member,
            }
        }
        // A tuple pattern on a sum tests its one tuple member of that
        // arity (`let (a, b) = pair() else { .. }` on `(i32, str) | none`).
        AstPatternKind::Tuple(ps) if matches!(scrut_ty, HirTy::Sum(_)) => {
            let HirTy::Sum(vs) = scrut_ty else {
                unreachable!()
            };
            let has_rest = ps.iter().any(|p| matches!(p.kind, AstPatternKind::Rest(_)));
            let fits = |v: &HirTy| {
                matches!(v, HirTy::Tuple(ts)
                    if ts.len() == ps.len() || (has_rest && ts.len() + 1 >= ps.len()))
            };
            let mut hits = vs.iter().enumerate().filter(|(_, v)| fits(v));
            let (Some((i, member)), None) = (hits.next(), hits.next()) else {
                return mismatch(ctx, format!("a {}-tuple pattern", ps.len()));
            };
            let member = member.clone();
            HirPattern::SumMember {
                sum_ty: scrut_ty.clone(),
                variant_index: u32::try_from(i).expect("variant overflow"),
                sub: Box::new(lower_pattern_inner(ctx, pat, &member)),
                payload_ty: member,
            }
        }
        AstPatternKind::Literal(lit) => match literal_to_const(ctx, lit, scrut_ty) {
            Some((value, prim)) => HirPattern::Literal { value, prim },
            None => mismatch(ctx, "this literal pattern".to_owned()),
        },
        AstPatternKind::TypeCheck { binder, ty } => {
            // `name: T` - narrow a sum scrutinee to variant `T` (or, on a
            // non-sum of exactly type `T`, bind it).
            let target_ty = ctx.resolve_ast_ty(ty);
            // An unknown name, said as such (not as a mismatch).
            let mut unknown = Vec::new();
            super::collect::collect_unknown_paths(&target_ty, &mut unknown);
            if let Some(name) = unknown.iter().find(|n| !n.starts_with(['_', '#'])) {
                let segs: Vec<&str> = name.split("::").collect();
                ctx.emit_error_at(
                    Code::UNRESOLVED_TYPE,
                    ty.span,
                    format!("cannot find type `{name}` in this scope"),
                );
                ctx.help_last(ctx.type_help(&segs));
                if let Some(name) = binder {
                    ctx.declare_bound(name, HirTy::Unknown, pat.span);
                }
                return HirPattern::Wildcard;
            }
            let bind = |ctx: &mut LowerCtx<'_>| {
                binder
                    .as_ref()
                    .map(|name| ctx.declare_bound(name, target_ty.clone(), pat.span))
            };
            if let HirTy::Sum(variants) = scrut_ty {
                if let Some(idx) = sum_variant_index(variants, &target_ty) {
                    let bind_local = bind(ctx);
                    return HirPattern::SumVariant {
                        sum_ty: scrut_ty.clone(),
                        variant_index: u32::try_from(idx).expect("variant overflow"),
                        binder: bind_local,
                        payload_ty: Some(target_ty),
                    };
                }
                // `v: A | B` against `A | B | C` (a generic `T` bound to a
                // sum): any of those tags, rebound as `A | B`.
                if let HirTy::Sum(ws) = &target_ty {
                    let idxs: Vec<usize> = ws
                        .iter()
                        .filter_map(|w| sum_variant_index(variants, w))
                        .collect();
                    if idxs.len() == ws.len() {
                        let alts = idxs
                            .into_iter()
                            .map(|i| HirPattern::SumVariant {
                                sum_ty: scrut_ty.clone(),
                                variant_index: u32::try_from(i).expect("variant overflow"),
                                binder: None,
                                payload_ty: None,
                            })
                            .collect();
                        let alts = HirPattern::Or(alts);
                        let Some(binder) = bind(ctx) else {
                            return alts;
                        };
                        let whole = ctx.declare(None, scrut_ty.clone());
                        ctx.pending_resums.push((binder, whole));
                        return HirPattern::At {
                            local: whole,
                            ty: scrut_ty.clone(),
                            sub: Box::new(alts),
                        };
                    }
                }
            } else if let (HirTy::AnyTrait(bounds), HirTy::Sum(ws)) = (scrut_ty, &target_ty) {
                // `x: A | B` on `any`: the tag of a stored `A | B` value
                // or of either member; the binder is the value as `A | B`.
                if ws
                    .iter()
                    .all(|w| bounds.iter().all(|b| ctx.implements_trait(w, b, 0)))
                {
                    let alts = HirPattern::Or(
                        std::iter::once(&target_ty)
                            .chain(ws)
                            .map(|w| HirPattern::SumVariant {
                                sum_ty: scrut_ty.clone(),
                                variant_index: ctx.any_tag(w),
                                binder: None,
                                payload_ty: None,
                            })
                            .collect(),
                    );
                    let Some(binder) = bind(ctx) else {
                        return alts;
                    };
                    let whole = ctx.declare(None, scrut_ty.clone());
                    ctx.pending_resums.push((binder, whole));
                    return HirPattern::At {
                        local: whole,
                        ty: scrut_ty.clone(),
                        sub: Box::new(alts),
                    };
                }
            } else if let (HirTy::AnyTrait(bounds), false) =
                (scrut_ty, matches!(target_ty, HirTy::AnyTrait(_)))
            {
                // Narrow `any Tr` to a concrete implementor (tag test).
                if bounds
                    .iter()
                    .all(|b| ctx.implements_trait(&target_ty, b, 0))
                {
                    let bind_local = bind(ctx);
                    return HirPattern::SumVariant {
                        sum_ty: scrut_ty.clone(),
                        variant_index: ctx.any_tag(&target_ty),
                        binder: bind_local,
                        payload_ty: Some(target_ty),
                    };
                }
            } else if *scrut_ty == target_ty {
                return match bind(ctx) {
                    Some(local) => HirPattern::Binding {
                        local,
                        ty: target_ty,
                    },
                    None => HirPattern::Wildcard,
                };
            } else if matches!(&target_ty, HirTy::Sum(ws) if ws.contains(scrut_ty)) {
                // `v: A | B` against an `A`: always matches; the binder
                // is the value wrapped as `A | B`.
                let Some(binder) = bind(ctx) else {
                    return HirPattern::Wildcard;
                };
                let whole = ctx.declare(None, scrut_ty.clone());
                ctx.pending_resums.push((binder, whole));
                return HirPattern::Binding {
                    local: whole,
                    ty: scrut_ty.clone(),
                };
            }
            // Spec 2.10.9: every type supports `is`. A test on a type
            // parameter is opaque at the definition check and decided per
            // instance.
            if super::bounds::has_param(ctx, scrut_ty) || super::bounds::has_param(ctx, &target_ty)
            {
                let binder = bind(ctx);
                return HirPattern::SumVariant {
                    sum_ty: scrut_ty.clone(),
                    variant_index: ctx.any_tag(&target_ty),
                    binder,
                    payload_ty: Some(target_ty),
                };
            }
            if in_instance(ctx) {
                bind(ctx);
                return HirPattern::Or(Vec::new());
            }
            mismatch(ctx, format!("type pattern `{}`", ctx.ty_name(&target_ty)))
        }
        AstPatternKind::Or(alts) => {
            // Spec 3.11.4: every alternative binds the same names. One
            // shared local per name, typed by the first alternative.
            let name_sets: Vec<std::collections::BTreeSet<String>> =
                alts.iter().map(pattern_bound_names).collect();
            let bound_names = name_sets.first().cloned().unwrap_or_default();
            for (i, set) in name_sets.iter().enumerate().skip(1) {
                if *set != bound_names {
                    let names = |v: Vec<&String>| {
                        v.iter()
                            .map(|n| format!("`{n}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    let missing = names(bound_names.difference(set).collect());
                    let extra = names(set.difference(&bound_names).collect());
                    let mut detail = String::new();
                    if !missing.is_empty() {
                        detail.push_str(&format!(" missing {missing}"));
                    }
                    if !extra.is_empty() {
                        detail.push_str(&format!(" extra {extra}"));
                    }
                    ctx.emit_error_at(
                        Code::OR_PATTERN_BINDINGS,
                        alts[i].span,
                        format!(
                            "or-pattern alternatives bind different name sets \
                             (alternative {i}:{detail})"
                        ),
                    );
                }
            }
            let mut shared: HashMap<String, LocalId> = HashMap::new();
            for name in &bound_names {
                let local = ctx.declare(Some(name.clone()), HirTy::Unknown);
                shared.insert(name.clone(), local);
            }
            ctx.or_pattern_binders.push(shared);
            let lowered: Vec<HirPattern> = alts
                .iter()
                .map(|a| lower_pattern(ctx, a, scrut_ty))
                .collect();
            ctx.or_pattern_binders.pop();
            HirPattern::Or(lowered)
        }
        // `Color::Red` or `Result::Ok(n)` patterns.
        AstPatternKind::EnumVariant { path, args } => {
            let segs: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
            let (enum_id, idx) = match resolve_enum_variant(ctx, &segs, Some(scrut_ty), None) {
                EnumRes::Found(e, i) => (e, i),
                EnumRes::Error => return HirPattern::Wildcard,
                EnumRes::NotEnum => {
                    return ctx.at_span(pat.span, |ctx| {
                        tuple_struct_pattern(ctx, &segs, args, scrut_ty)
                    });
                }
            };
            if *scrut_ty != HirTy::Enum(enum_id) {
                return mismatch(ctx, format!("pattern `{}`", segs.join("::")));
            }
            let variant = ctx.enum_def(enum_id).variants[idx].clone();
            let slots = variant_arg_tys(&variant);
            // `V(a, ..)` / `V(.., z)` / `V(..)`: one `..` stands for the
            // fields not written, each matching anything.
            let rest_at: Vec<usize> = args
                .iter()
                .enumerate()
                .filter(|(_, a)| matches!(a.kind, AstPatternKind::Rest(_)))
                .map(|(i, _)| i)
                .collect();
            if let [at] = rest_at.as_slice() {
                let (before, after) = (&args[..*at], &args[at + 1..]);
                if before.len() + after.len() <= slots.len() {
                    let fill = slots.len() - before.len() - after.len();
                    let mut lowered = Vec::with_capacity(slots.len());
                    for (p, t) in before.iter().zip(&slots) {
                        lowered.push(lower_pattern(ctx, p, t));
                    }
                    lowered.extend(std::iter::repeat_n(HirPattern::Wildcard, fill));
                    for (p, t) in after.iter().zip(&slots[before.len() + fill..]) {
                        lowered.push(lower_pattern(ctx, p, t));
                    }
                    return HirPattern::EnumVariant {
                        enum_id,
                        variant_index: u32::try_from(idx).expect("variant overflow"),
                        args: lowered,
                    };
                }
            } else if rest_at.len() > 1 {
                ctx.emit_error(
                    Code::REST_PATTERN,
                    format!("pattern `{}` has more than one `..`", segs.join("::")),
                );
                return HirPattern::Wildcard;
            }
            if args.len() != slots.len() {
                ctx.emit_error(
                    Code::PATTERN_ARITY,
                    format!(
                        "pattern `{}` has {} field{}, but the variant has {}",
                        segs.join("::"),
                        args.len(),
                        if args.len() == 1 { "" } else { "s" },
                        slots.len()
                    ),
                );
                return HirPattern::Wildcard;
            }
            let lowered_args: Vec<HirPattern> = args
                .iter()
                .zip(&slots)
                .map(|(p, t)| lower_pattern(ctx, p, t))
                .collect();
            HirPattern::EnumVariant {
                enum_id,
                variant_index: u32::try_from(idx).expect("variant overflow"),
                args: lowered_args,
            }
        }
        AstPatternKind::Tuple(pats) => match scrut_ty {
            HirTy::Tuple(elem_tys) if pats.len() == elem_tys.len() => HirPattern::Tuple(
                pats.iter()
                    .zip(elem_tys.iter())
                    .map(|(p, t)| lower_pattern(ctx, p, t))
                    .collect(),
            ),
            // `()` matches the one unit value.
            HirTy::Unit if pats.is_empty() => HirPattern::Wildcard,
            _ => mismatch(ctx, format!("a {}-tuple pattern", pats.len())),
        },
        AstPatternKind::Range {
            start,
            end,
            inclusive,
        } => {
            let prim = match scrut_ty {
                HirTy::Prim(p) if p.is_int() => *p,
                _ => return mismatch(ctx, "a range pattern".to_owned()),
            };
            let bound = |ctx: &LowerCtx<'_>, lit: &Option<AstLit>| match lit {
                None => Some(None),
                Some(l) => literal_to_const(ctx, l, scrut_ty).map(|(v, _)| Some(v)),
            };
            match (bound(ctx, start), bound(ctx, end)) {
                (Some(s), Some(e)) => HirPattern::Range {
                    start: s,
                    end: e,
                    inclusive: *inclusive,
                    prim,
                },
                _ => mismatch(ctx, "this range pattern".to_owned()),
            }
        }
        AstPatternKind::Struct { path, fields, .. } => {
            let segs: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
            // `Enum::Variant { x, y }`: field names map to positions in
            // the variant's tuple payload (spec 2.4).
            match resolve_enum_variant(ctx, &segs, Some(scrut_ty), None) {
                EnumRes::Error => return HirPattern::Wildcard,
                EnumRes::Found(enum_id, idx) => {
                    if *scrut_ty != HirTy::Enum(enum_id) {
                        return mismatch(ctx, format!("pattern `{}`", segs.join("::")));
                    }
                    let variant = ctx.enum_def(enum_id).variants[idx].clone();
                    let field_tys = variant_arg_tys(&variant);
                    let mut args = vec![HirPattern::Wildcard; variant.named_fields.len()];
                    for fp in fields {
                        let Some(i) = variant.named_fields.iter().position(|n| *n == fp.name)
                        else {
                            ctx.emit_error(
                                Code::NO_FIELD,
                                format!(
                                    "enum variant `{}` has no field `{}`",
                                    segs.join("::"),
                                    fp.name
                                ),
                            );
                            continue;
                        };
                        let ty = field_tys[i].clone();
                        args[i] = if let Some(p) = &fp.pat {
                            lower_pattern(ctx, p, &ty)
                        } else {
                            let local = ctx.declare_bound(&fp.name, ty.clone(), pat.span);
                            HirPattern::Binding { local, ty }
                        };
                    }
                    return HirPattern::EnumVariant {
                        enum_id,
                        variant_index: u32::try_from(idx).expect("variant overflow"),
                        args,
                    };
                }
                EnumRes::NotEnum => {}
            }
            let struct_id = super::expr::self_struct_id(ctx, &segs).or_else(|| {
                ctx.canon_type(&segs)
                    .and_then(|full| ctx.lookup_struct(&full))
            });
            let Some(struct_id) = struct_id else {
                let help = ctx.type_help(&segs);
                ctx.unknown_name(
                    Code::UNRESOLVED_TYPE,
                    pat.span,
                    format!("cannot find struct `{}` in this scope", segs.join("::")),
                    help,
                );
                return HirPattern::Wildcard;
            };
            if *scrut_ty != HirTy::Struct(struct_id) {
                return mismatch(ctx, format!("struct pattern `{}`", segs.join("::")));
            }
            struct_pattern_rest(ctx, pat, scrut_ty);
            let def = ctx.struct_def(struct_id);
            let mut lowered: Vec<(u32, HirPattern)> = Vec::new();
            for fp in fields {
                let Some(idx) = def.fields.iter().position(|f| f.name == fp.name) else {
                    ctx.no_field(struct_id, &fp.name, pat.span);
                    continue;
                };
                ctx.check_field_visible(struct_id, &fp.name);
                let field_ty = def.fields[idx].ty.clone();
                let sub = match &fp.pat {
                    Some(p) => lower_pattern(ctx, p, &field_ty),
                    // Shorthand `Foo { x }` - bind a local of the field's name.
                    None => {
                        let local = ctx.declare_bound(&fp.name, field_ty.clone(), pat.span);
                        HirPattern::Binding {
                            local,
                            ty: field_ty,
                        }
                    }
                };
                lowered.push((u32::try_from(idx).expect("field index overflow"), sub));
            }
            HirPattern::Struct {
                struct_id,
                fields: lowered,
            }
        }
        AstPatternKind::Rest(_) => {
            ctx.emit_error(
                Code::REST_PATTERN,
                "a rest pattern `..` is only allowed in tuple destructuring `let`".to_owned(),
            );
            HirPattern::Wildcard
        }
    }
}

/// `Pair(x, 4)` - a tuple struct's positional fields.
fn tuple_struct_pattern(
    ctx: &mut LowerCtx<'_>,
    segs: &[&str],
    args: &[AstPattern],
    scrut_ty: &HirTy,
) -> HirPattern {
    let full = ctx.canon_type(segs);
    let HirTy::Struct(sid) = scrut_ty else {
        ctx.emit_error(
            Code::UNRESOLVED_VARIANT,
            format!("cannot find enum variant `{}`", segs.join("::")),
        );
        ctx.help_last(ctx.value_help(segs));
        // Rust's `Some(x)` on a `T | none`: a type-test pattern.
        if let (["Some"], [arg], HirTy::Sum(vs)) = (segs, args, scrut_ty) {
            let inner: Vec<String> = vs
                .iter()
                .filter(|v| **v != HirTy::None)
                .map(|v| ctx.ty_name(v))
                .collect();
            if let (Some(x), true) = (ctx.src(arg.span), vs.contains(&HirTy::None)) {
                ctx.help_last(Some(format!(
                    "rok has no `Some`: match on the type; write `{x}: {} => ..` and `none => ..`",
                    inner.join(" | ")
                )));
            }
        }
        return HirPattern::Wildcard;
    };
    let def = ctx.struct_def(*sid);
    let tuple_form = def
        .fields
        .iter()
        .enumerate()
        .all(|(i, f)| f.name == i.to_string());
    if full.as_deref() != Some(enum_base_name(&def.name)) || !tuple_form {
        ctx.emit_error(
            Code::PATTERN_TYPE,
            format!(
                "pattern `{}(..)` can't match a value of type `{}`",
                segs.join("::"),
                ctx.ty_name(scrut_ty)
            ),
        );
        return HirPattern::Wildcard;
    }
    if args.len() != def.fields.len() {
        ctx.emit_error(
            Code::PATTERN_ARITY,
            format!(
                "pattern `{}` has {} field(s), but the struct has {}",
                segs.join("::"),
                args.len(),
                def.fields.len()
            ),
        );
        return HirPattern::Wildcard;
    }
    let fields = args
        .iter()
        .zip(&def.fields)
        .enumerate()
        .map(|(i, (p, f))| {
            // `_` reads nothing; any other sub-pattern needs the field visible.
            if !matches!(p.kind, AstPatternKind::Wildcard) {
                ctx.check_field_visible(*sid, &f.name);
            }
            (
                u32::try_from(i).expect("field index overflow"),
                lower_pattern(ctx, p, &f.ty),
            )
        })
        .collect();
    HirPattern::Struct {
        struct_id: *sid,
        fields,
    }
}

/// Names bound by a pattern (sorted, for or-pattern consistency).
pub(super) fn pattern_bound_names(pat: &AstPattern) -> std::collections::BTreeSet<String> {
    struct Names(std::collections::BTreeSet<String>);
    impl crate::walk::AstVisitor for Names {
        fn bind(&mut self, name: &str) {
            self.0.insert(name.to_owned());
        }
    }
    let mut n = Names(std::collections::BTreeSet::new());
    match &pat.kind {
        // Spec 3.11.4: alternatives bind the same names; the first one
        // stands for the whole or-pattern.
        AstPatternKind::Or(alts) => {
            if let Some(first) = alts.first() {
                crate::walk::walk_pat(&mut n, first);
            }
        }
        _ => crate::walk::walk_pat(&mut n, pat),
    }
    n.0
}

/// A pattern literal as a `ConstVal` and its comparison type; `None` if it
/// doesn't fit the scrutinee (the caller reports).
pub(super) fn literal_to_const(
    ctx: &LowerCtx<'_>,
    lit: &AstLit,
    scrut_ty: &HirTy,
) -> Option<(ConstVal, PrimTy)> {
    let HirTy::Prim(sp) = scrut_ty else {
        return None;
    };
    match lit {
        AstLit::Int(text) => {
            // Bare literals take the scrutinee's type.
            let (value, mut ty) = parse_int_literal(text)?;
            if !has_int_suffix(text) {
                ty = *sp;
            }
            // A `char` is no integer: `0` on `i32 | char` is the `i32`.
            if ty != *sp || !sp.is_int() || *sp == PrimTy::Char {
                return None;
            }
            if !int_fits(value, ty) {
                ctx.emit_error(
                    Code::LITERAL_OUT_OF_RANGE,
                    format!("literal `{text}` out of range for `{}`", ty.name()),
                );
            }
            Some((ConstVal::Int { value, ty }, ty))
        }
        AstLit::Bool(b) if *sp == PrimTy::Bool => Some((ConstVal::Bool(*b), PrimTy::Bool)),
        AstLit::Char(s) if *sp == PrimTy::Char => {
            parse_char_literal(s).map(|c| (ConstVal::Char(c), PrimTy::Char))
        }
        // An ASCII `'a'` against a `c_char` scrutinee takes that type.
        AstLit::Char(s) if *sp == PrimTy::CChar => parse_char_literal(s)
            .filter(|c| *c < 0x80)
            .map(|c| (ConstVal::Char(c), PrimTy::CChar)),
        AstLit::CChar(s) if *sp == PrimTy::CChar => {
            parse_char_literal(s).map(|c| (ConstVal::Char(c), PrimTy::CChar))
        }
        // String patterns compare contents (codegen calls `rok_str_cmp`).
        AstLit::Str(s) if *sp == PrimTy::Str => Some((
            ConstVal::Str(super::expr::decode_str_escapes(s)),
            PrimTy::Str,
        )),
        AstLit::RawStr(s) if *sp == PrimTy::Str => Some((ConstVal::Str(s.clone()), PrimTy::Str)),
        // Matching is `==` (spec 3.11.1): `0.0` matches `-0.0`, nothing
        // matches NaN. Bare literals take the scrutinee's type.
        AstLit::Float(text) if sp.is_float() => {
            let suffixed = text.ends_with("f32") || text.ends_with("f64");
            let text = if suffixed {
                text.clone()
            } else {
                format!("{text}{}", sp.name())
            };
            let (bits, ty) = super::expr::parse_float_literal(&text)?;
            (ty == *sp).then_some((ConstVal::Float { bits, ty }, ty))
        }
        _ => None,
    }
}

/// Does `v` fit in primitive integer type `p` on the target?
pub(super) fn int_fits(v: i128, p: PrimTy) -> bool {
    let bits = p.bit_width(super::const_eval::pointer_width());
    if bits == 0 || bits >= 128 {
        return true;
    }
    if p.is_signed_int() {
        let lim = 1i128 << (bits - 1);
        (-lim..lim).contains(&v)
    } else {
        (0..(1i128 << bits)).contains(&v)
    }
}

/// Reject binding pattern `name` when it looks meant as a value: it
/// names a const or static (any case), or is capitalised and close to a
/// variant of the scrutinee's enum. Either way it would bind every value.
fn binds_const(ctx: &LowerCtx<'_>, name: &str, scrut_ty: &HirTy, at: rokc_diagnostic::Span) {
    let segs = [name];
    let what = if ctx.resolve_in(&segs, false).is_some() {
        "const"
    } else if ctx.resolve_in(&segs, true).is_some() {
        "static"
    } else {
        if !name.starts_with(|c: char| c.is_uppercase()) {
            return;
        }
        let HirTy::Enum(id) = scrut_ty else {
            return;
        };
        let def = ctx.enum_def(*id);
        let Some(v) = def
            .variants
            .iter()
            .map(|v| (rokc_core::suggest::edit_distance(&v.name, name), v))
            .filter(|(d, v)| *d <= 2 && *d < v.name.chars().count())
            .min_by_key(|(d, _)| *d)
            .map(|(_, v)| v)
        else {
            return;
        };
        let base = enum_base_name(&def.name);
        let rest = if v.arity == 0 { "" } else { "(..)" };
        ctx.emit_error_help(
            Code::BINDING_SHADOWS_VARIANT,
            at,
            format!("`{name}` here would be a new variable that matches every value"),
            format!("did you mean the variant `{base}::{}{rest}`?", v.name),
        );
        // Only a unit variant is a drop-in replacement for the name.
        if v.arity == 0 && ctx.src(at) == Some(name) {
            let text = format!("{base}::{}", v.name);
            ctx.fix_last(at, text.clone(), format!("write `{text}`"));
        }
        return;
    };
    ctx.emit_error_help(
        Code::BINDING_SHADOWS_VARIANT,
        at,
        format!(
            "`{name}` here would be a new variable that matches every value, not the {what} \
             `{name}`"
        ),
        format!("to compare with the {what}, write `x if x == {name}`"),
    );
}

/// The variant of enum `scrut_ty` named `name`: its enum, index, and the
/// enum's name as a pattern writes it.
fn variant_named(
    ctx: &LowerCtx<'_>,
    scrut_ty: &HirTy,
    name: &str,
) -> Option<(EnumId, usize, String)> {
    let HirTy::Enum(id) = scrut_ty else {
        return None;
    };
    let (id, idx) = super::expr::target_variant(ctx, name, &HirTy::Enum(*id))?;
    Some((id, idx, enum_base_name(&ctx.enum_def(id).name).to_owned()))
}
