//! Broadcast (spec 3.3, stdlib 2.6): a tree of dot operators and `f.(..)`
//! over core `List` / `Slice` / arrays lowers to one fused loop with no
//! intermediates (2.6.6): `let #n = #b0.len(); (lengths checked) let #out
//! = List::with_capacity(#n); for #i in 0..#n { #out.push(..) }`. All-array
//! trees give an array (lengths checked at compile time). `.&&` / `.||`
//! don't short-circuit (2.6.2). Anything else dispatches the dot trait.

use super::expr::{block_of, lower_infix, splice, spread_item_at};
#[allow(clippy::wildcard_imports)]
use super::*;
use rokc_diagnostic::Code;

const LIST: &str = "rok::core::collections::list::List";
const SLICE: &str = "rok::core::collections::slice::Slice";

/// A dot operator token (`.+`): its scalar operator.
pub(super) fn scalar_op(op: &str) -> Option<&str> {
    let s = op.strip_prefix('.')?;
    matches!(
        s,
        "+" | "-"
            | "*"
            | "/"
            | "%"
            | "%%"
            | "=="
            | "!="
            | "<"
            | ">"
            | "<="
            | ">="
            | "&&"
            | "||"
            | "&"
            | "|"
            | "^"
            | "<<"
            | ">>"
    )
    .then_some(s)
}

/// An operand of the tree, evaluated once in source order.
enum Leaf {
    /// An unsuffixed or plain literal: kept as source so it takes its
    /// type from the other operand.
    Lit(Expr),
    Value {
        h: HirExpr,
        /// `Some(element type)` for a `List` / `Slice` / array.
        elem: Option<HirTy>,
        /// The array length.
        fixed: Option<u32>,
        /// An operand of a dot operator (not a `f.(..)` argument).
        operand: bool,
        /// A `T | none` operand: `none` propagates.
        optional: bool,
    },
    /// A function broadcast's callee path.
    Callee(Expr),
}

fn is_literal(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Literal(_) => true,
        ExprKind::OpPrefix { op, operand } if op == "-" => is_literal(operand),
        _ => false,
    }
}

/// The element type of a core collection, and an array's length.
fn collection(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<(HirTy, Option<u32>)> {
    match ty {
        HirTy::Array(t, n) => Some(((**t).clone(), Some(*n))),
        HirTy::Struct(id) => {
            let def = ctx.struct_def(*id);
            let base = enum_base_name(&def.name);
            (base == LIST || base == SLICE)
                .then(|| def.generic_args.first().cloned())
                .flatten()
                .map(|t| (t, None))
        }
        _ => None,
    }
}

/// Lower the leaves of `e` left to right into `leaves`; the tree with
/// each leaf replaced by `#leaf<k>` (rebuilt by `rebuild`).
fn collect(ctx: &mut LowerCtx<'_>, e: &Expr, operand: bool, leaves: &mut Vec<Leaf>) {
    match &e.kind {
        ExprKind::OpInfix { op, lhs, rhs } if scalar_op(op).is_some() => {
            collect(ctx, lhs, true, leaves);
            collect(ctx, rhs, true, leaves);
        }
        ExprKind::Broadcast { callee, args } => {
            if matches!(callee.kind, ExprKind::Path(_)) {
                leaves.push(Leaf::Callee((**callee).clone()));
            } else {
                collect(ctx, callee, false, leaves);
            }
            for a in args {
                if let CallArg::Positional(v) | CallArg::Named { value: v, .. } = a {
                    collect(ctx, v, false, leaves);
                }
            }
        }
        _ if is_literal(e) => leaves.push(Leaf::Lit(e.clone())),
        _ => {
            let h = lower_expected(ctx, e, HirTy::Unknown);
            let (elem, fixed) = collection(ctx, &h.ty).map_or((None, None), |(t, n)| (Some(t), n));
            let optional = operand && matches!(&h.ty, HirTy::Sum(ms) if ms.contains(&HirTy::None));
            leaves.push(Leaf::Value {
                h,
                elem,
                fixed,
                operand,
                optional,
            });
        }
    }
}

/// `e` with leaves (in `collect` order) replaced by `leaf(k)`; dot
/// operators become `op(..)`, broadcasts calls when `fused`.
fn rebuild(e: &Expr, next: &mut usize, fused: bool, leaf: &mut dyn FnMut(usize) -> Expr) -> Expr {
    let span = e.span;
    let kind = match &e.kind {
        ExprKind::OpInfix { op, lhs, rhs } if scalar_op(op).is_some() => {
            let lhs = Box::new(rebuild(lhs, next, fused, leaf));
            let rhs = Box::new(rebuild(rhs, next, fused, leaf));
            let op = match (fused, scalar_op(op)) {
                (true, Some("&&")) => "&".to_owned(),
                (true, Some("||")) => "|".to_owned(),
                (true, Some(s)) => s.to_owned(),
                _ => op.clone(),
            };
            ExprKind::OpInfix { op, lhs, rhs }
        }
        ExprKind::Broadcast { callee, args } => {
            let callee = Box::new(rebuild(callee, next, fused, leaf));
            let args = args
                .iter()
                .map(|a| match a {
                    CallArg::Positional(v) => CallArg::Positional(rebuild(v, next, fused, leaf)),
                    CallArg::Named { name, value } => CallArg::Named {
                        name: name.clone(),
                        value: rebuild(value, next, fused, leaf),
                    },
                    other => other.clone(),
                })
                .collect();
            if fused {
                ExprKind::Call {
                    callee,
                    args,
                    trailing_closure: None,
                }
            } else {
                ExprKind::Broadcast { callee, args }
            }
        }
        _ => {
            let k = *next;
            *next += 1;
            return leaf(k);
        }
    };
    Expr { kind, span }
}

/// `e`, a dot operator or a function broadcast (spec 3.3).
pub(super) fn lower(ctx: &mut LowerCtx<'_>, e: &Expr) -> HirExpr {
    let span = e.span;
    let mut leaves = Vec::new();
    collect(ctx, e, false, &mut leaves);
    let poisoned = leaves
        .iter()
        .any(|l| matches!(l, Leaf::Value { h, .. } if h.ty == HirTy::Unknown));
    if poisoned {
        return poison(span);
    }
    let tag = ctx.locals.len();
    if leaves
        .iter()
        .any(|l| matches!(l, Leaf::Value { optional: true, .. }))
    {
        return propagate_none(ctx, e, leaves, tag);
    }
    let elems: Vec<&HirTy> = leaves
        .iter()
        .filter_map(|l| match l {
            Leaf::Value { elem: Some(t), .. } => Some(t),
            _ => None,
        })
        .collect();
    // A scalar operand of a dot operator: a primitive, or the element
    // type itself (what core's `List op T` impls take).
    let fusible = !elems.is_empty()
        && leaves.iter().all(|l| match l {
            Leaf::Value {
                h,
                elem: None,
                operand: true,
                ..
            } => matches!(h.ty, HirTy::Prim(_)) || elems.contains(&&h.ty),
            _ => true,
        });
    let name = |k: usize| format!("#b{tag}_{k}");
    let path = |n: &str| super::throws::path_expr(n, span);
    if !fusible {
        let mut leaves: Vec<Option<Leaf>> = leaves.into_iter().map(Some).collect();
        let mut take = |k: usize| match leaves[k].take() {
            Some(Leaf::Lit(x) | Leaf::Callee(x)) => x,
            Some(Leaf::Value { h, .. }) => splice(ctx, h),
            None => unreachable!("each leaf once"),
        };
        let tree = rebuild(e, &mut 0, false, &mut take);
        return match &tree.kind {
            ExprKind::OpInfix { op, lhs, rhs } => lower_infix(ctx, op, lhs, rhs, span),
            _ => ctx.error_expr(
                Code::BROADCAST,
                span,
                "a broadcast `f.(..)` needs a `List`, `Slice` or array argument (spec 3.3.3)"
                    .to_owned(),
            ),
        };
    }
    let fixed: Vec<u32> = leaves
        .iter()
        .filter_map(|l| match l {
            Leaf::Value { fixed: Some(n), .. } => Some(*n),
            _ => None,
        })
        .collect();
    let all_fixed = fixed.len() == elems.len();
    if let Some(n) = fixed.iter().find(|n| **n != fixed[0]) {
        return ctx.error_expr(
            Code::BROADCAST,
            span,
            format!(
                "broadcast over arrays of lengths {} and {n} (stdlib 2.6.4)",
                fixed[0]
            ),
        );
    }
    // Leaves into temps, in order; the element expression reads them.
    let mut stmts: Vec<Stmt> = Vec::new();
    let mut many: Vec<String> = Vec::new();
    let mut reads: Vec<Expr> = Vec::new();
    let i = format!("#bi{tag}");
    for (k, l) in leaves.into_iter().enumerate() {
        reads.push(match l {
            Leaf::Lit(x) | Leaf::Callee(x) => x,
            Leaf::Value { h, elem, .. } => {
                let n = name(k);
                let init = splice(ctx, h);
                stmts.push(Stmt {
                    kind: StmtKind::Let {
                        name: n.clone(),
                        ty: None,
                        init,
                    },
                    span,
                });
                if elem.is_some() {
                    many.push(n.clone());
                    spread_item_at(&n, &path(&i), span)
                } else {
                    path(&n)
                }
            }
        });
    }
    let mut reads = reads.into_iter().map(Some).collect::<Vec<_>>();
    let mut read = |k: usize| reads[k].take().expect("each leaf once");
    let item = rebuild(e, &mut 0, true, &mut read);
    let ast = |kind: ExprKind| Expr { kind, span };
    let usize_lit = |n: u32| ast(ExprKind::Literal(AstLit::Int(format!("{n}usize"))));
    let method = |recv: Expr, m: &str, args: Vec<Expr>| {
        ast(ExprKind::MethodCall {
            receiver: Box::new(recv),
            method: m.to_owned(),
            type_args: Vec::new(),
            args: args.into_iter().map(CallArg::Positional).collect(),
            trailing_closure: None,
            origin: None,
        })
    };
    let for_range = |from: Expr, to: Expr, body: StmtKind| {
        ast(ExprKind::For {
            label: None,
            pat: rokc_ast::Pattern {
                kind: rokc_ast::PatternKind::Binding {
                    name: i.clone(),
                    sub: None,
                },
                span,
            },
            iter: Box::new(ast(ExprKind::Range {
                start: Some(Box::new(from)),
                end: Some(Box::new(to)),
                inclusive: false,
            })),
            body: Block {
                stmts: vec![Stmt { kind: body, span }],
                trailing: None,
                span,
                label: None,
            },
            is_await: false,
        })
    };
    let out = format!("#bo{tag}");
    let let_of = |stmts: &mut Vec<Stmt>, n: &str, init: Expr| {
        stmts.push(Stmt {
            kind: StmtKind::Let {
                name: n.to_owned(),
                ty: None,
                init,
            },
            span,
        });
    };
    if all_fixed && fixed[0] > 0 {
        // `[item(0); N]`, then items 1.. in place.
        let first = {
            let mut first_reads = item.clone();
            replace_path(&mut first_reads, &i, &usize_lit(0));
            first_reads
        };
        let_of(
            &mut stmts,
            &out,
            ast(ExprKind::ArrayRepeat {
                value: Box::new(first),
                len: rokc_ast::ConstExpr::Literal(AstLit::Int(fixed[0].to_string())),
            }),
        );
        let store = StmtKind::Assign(spread_item_at(&out, &path(&i), span), item);
        stmts.push(Stmt {
            kind: StmtKind::Expr(for_range(usize_lit(1), usize_lit(fixed[0]), store)),
            span,
        });
        return lower_expr(ctx, &block_of(stmts, path(&out), span));
    }
    let n = format!("#bn{tag}");
    let_of(&mut stmts, &n, method(path(&many[0]), "len", Vec::new()));
    for other in &many[1..] {
        let check = ast(ExprKind::Call {
            callee: Box::new(ast(ExprKind::Path(super::expr::lang_fn_in(
                "broadcast_len_fn",
                "collections::broadcast",
                "check_len",
                span,
            )))),
            args: vec![
                CallArg::Positional(method(path(other), "len", Vec::new())),
                CallArg::Positional(path(&n)),
            ],
            trailing_closure: None,
        });
        stmts.push(Stmt {
            kind: StmtKind::Expr(check),
            span,
        });
    }
    let new = ast(ExprKind::Call {
        callee: Box::new(super::bignum::path_expr(LIST, "with_capacity", span)),
        args: vec![CallArg::Positional(path(&n))],
        trailing_closure: None,
    });
    let_of(&mut stmts, &out, new);
    let push = StmtKind::Expr(method(path(&out), "push", vec![item]));
    stmts.push(Stmt {
        kind: StmtKind::Expr(for_range(usize_lit(0), path(&n), push)),
        span,
    });
    lower_expr(ctx, &block_of(stmts, path(&out), span))
}

/// Stdlib 2.6.7: a `T | none` operand propagates `none`. The first optional
/// leaf is matched; its `T` arm runs the tree (recursing for further
/// optional operands), anything else gives `none`.
fn propagate_none(ctx: &mut LowerCtx<'_>, e: &Expr, leaves: Vec<Leaf>, tag: usize) -> HirExpr {
    let span = e.span;
    let path = |n: &str| super::throws::path_expr(n, span);
    let v = format!("#v{tag}");
    let mut stmts = Vec::new();
    let mut reads = Vec::new();
    let mut opt: Option<(LocalId, HirTy, u32, HirTy)> = None;
    ctx.push_scope();
    for (k, l) in leaves.into_iter().enumerate() {
        reads.push(Some(match l {
            Leaf::Lit(x) | Leaf::Callee(x) => x,
            Leaf::Value { h, optional, .. } => {
                let temp = format!("#b{tag}_{k}");
                let ty = h.ty.clone();
                let local = ctx.declare(Some(temp.clone()), ty.clone());
                stmts.push(HirStmt::Let { local, init: h });
                let member = match &ty {
                    HirTy::Sum(ms) if optional && opt.is_none() => {
                        let mut rest = ms.iter().enumerate().filter(|(_, m)| !m.is_none());
                        match (rest.next(), rest.next()) {
                            (Some((at, inner)), None) => Some((at, inner.clone())),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some((at, inner)) = member {
                    let at = u32::try_from(at).expect("variant overflow");
                    opt = Some((local, ty, at, inner));
                    path(&v)
                } else {
                    path(&temp)
                }
            }
        }));
    }
    let mut read = |k: usize| reads[k].take().expect("each leaf once");
    let tree = rebuild(e, &mut 0, false, &mut read);
    let trailing = if let Some((local, sum_ty, variant_index, member)) = opt {
        ctx.push_scope();
        let bound = ctx.declare(Some(v), member.clone());
        let body = lower_expr(ctx, &tree);
        ctx.pop_scope();
        let out = HirTy::sum([body.ty.clone(), HirTy::None]);
        let some = coerce(ctx, &out, body, &|| "the broadcast value".to_owned());
        let none = lower_expected(
            ctx,
            &Expr {
                kind: ExprKind::Literal(AstLit::None),
                span,
            },
            out.clone(),
        );
        let none = coerce(ctx, &out, none, &|| "the broadcast value".to_owned());
        HirExpr {
            kind: HirExprKind::Match {
                scrutinee: Box::new(hir_local(local, &sum_ty, span)),
                arms: vec![
                    HirMatchArm {
                        pat: HirPattern::SumVariant {
                            sum_ty: sum_ty.clone(),
                            variant_index,
                            binder: Some(bound),
                            payload_ty: Some(member),
                        },
                        guard: None,
                        body: some,
                    },
                    HirMatchArm {
                        pat: HirPattern::Wildcard,
                        guard: None,
                        body: none,
                    },
                ],
            },
            ty: out,
            span,
        }
    } else {
        lower_expr(ctx, &tree)
    };
    ctx.pop_scope();
    HirExpr {
        ty: trailing.ty.clone(),
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: Some(trailing),
            label: None,
        })),
        span,
    }
}

/// Replace every path `name` in `e` by `with` (the element index of the
/// first array item).
fn replace_path(e: &mut Expr, name: &str, with: &Expr) {
    if let ExprKind::Path(p) = &e.kind {
        if p.segments.len() == 1 && p.segments[0].name == name {
            *e = with.clone();
            return;
        }
    }
    match &mut e.kind {
        ExprKind::OpInfix { lhs, rhs, .. } => {
            replace_path(lhs, name, with);
            replace_path(rhs, name, with);
        }
        ExprKind::Index(a, b) => {
            replace_path(a, name, with);
            replace_path(b, name, with);
        }
        ExprKind::Call { callee, args, .. } => {
            replace_path(callee, name, with);
            for a in args {
                if let CallArg::Positional(v) | CallArg::Named { value: v, .. } = a {
                    replace_path(v, name, with);
                }
            }
        }
        _ => {}
    }
}
