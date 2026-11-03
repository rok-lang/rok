//! Broadcast: a tree of dot operators and `f.(..)`
//! over core `List` / `Slice` / arrays lowers to one fused loop with no
//! intermediates: `let #n = #b0.len(); (lengths checked) let #out
//! = List::with_capacity(#n); for #i in 0..#n { #out.push(..) }`. All-array
//! trees give an array (lengths checked at compile time). A tree over
//! `Broadcastable` operands (`Array`) fuses the same way through the
//! `fused_*` hooks of `rok::core::array`, and `.=` / `.+=` .. write
//! into the target. `.&&` / `.||` don't short-circuit. Anything else
//! dispatches the dot trait.

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
            | "**"
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
            | "??"
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
        /// `Some((element type, rank))` for a `Broadcastable` operand.
        bc: Option<(HirTy, HirTy)>,
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

/// The element type and rank of a `Broadcastable` type.
pub(super) fn broadcastable(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<(HirTy, HirTy)> {
    let key = ctx.lang("broadcastable_trait");
    if key.is_empty() || !matches!(ty, HirTy::Struct(_) | HirTy::Enum(_) | HirTy::Param(_)) {
        return None;
    }
    let rank = ctx.impl_trait_args(ty, key)?.into_iter().next()?;
    let elem = ctx.project(ty, "Elem", Some(&[key.to_owned()]), &[])?;
    Some((elem, rank))
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
            let bc = if elem.is_none() {
                broadcastable(ctx, &h.ty)
            } else {
                None
            };
            let optional = operand && matches!(&h.ty, HirTy::Sum(ms) if ms.contains(&HirTy::None));
            leaves.push(Leaf::Value {
                h,
                elem,
                fixed,
                bc,
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
            if fused && op == ".??" {
                return Expr {
                    kind: ExprKind::NullishCoalesce(lhs, rhs),
                    span,
                };
            }
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

/// `e`, a dot operator or a function broadcast.
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
    let has_bc = leaves
        .iter()
        .any(|l| matches!(l, Leaf::Value { bc: Some(_), .. }));
    let has_core = leaves
        .iter()
        .any(|l| matches!(l, Leaf::Value { elem: Some(_), .. }));
    if has_bc && has_core {
        return mixed(ctx, span);
    }
    let elems: Vec<&HirTy> = leaves
        .iter()
        .filter_map(|l| match l {
            Leaf::Value { elem: Some(t), .. }
            | Leaf::Value {
                bc: Some((t, _)), ..
            } => Some(t),
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
                bc: None,
                operand: true,
                ..
            } => matches!(h.ty, HirTy::Prim(_)) || elems.contains(&&h.ty),
            _ => true,
        });
    let name = |k: usize| format!("#b{tag}_{k}");
    let path = |n: &str| super::throws::path_expr(n, span);
    if !fusible && has_coalesce(e) {
        return fail(
            ctx,
            span,
            "`.??` fills the `none` elements of a `List`, `Slice`, array or `Array`, but no \
             operand here is one"
                .to_owned(),
            "for one value write `x ?? y`".to_owned(),
        );
    }
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
                "a broadcast `f.(..)` needs a `List`, `Slice` or array argument".to_owned(),
            ),
        };
    }
    if has_bc {
        return fused(ctx, e, leaves, tag, None);
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
            format!("broadcast over arrays of lengths {} and {n}", fixed[0]),
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
            by_ref: false,
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

/// A `BROADCAST` error with `help`.
fn fail(ctx: &LowerCtx<'_>, span: Span, msg: String, help: String) -> HirExpr {
    ctx.emit_error_help(Code::BROADCAST, span, msg, help);
    poison(span)
}

/// A tree over both a core collection and a `Broadcastable` type.
fn mixed(ctx: &LowerCtx<'_>, span: Span) -> HirExpr {
    fail(
        ctx,
        span,
        "a dot expression over both a `List` / `Slice` / fixed array and an `Array` (or \
         another `Broadcastable` type)"
            .to_owned(),
        "make the list an array first: `Array::from_list(xs, [xs.len()])`".to_owned(),
    )
}

/// `e` holds a `.??`.
fn has_coalesce(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::OpInfix { op, lhs, rhs } if scalar_op(op).is_some() => {
            op == ".??" || has_coalesce(lhs) || has_coalesce(rhs)
        }
        ExprKind::Broadcast { args, .. } => args.iter().any(|a| match a {
            CallArg::Positional(v) | CallArg::Named { value: v, .. } => has_coalesce(v),
            _ => false,
        }),
        _ => false,
    }
}

/// AST pieces of the fused loops.
struct Ast {
    span: Span,
}

impl Ast {
    const fn e(&self, kind: ExprKind) -> Expr {
        Expr {
            kind,
            span: self.span,
        }
    }
    fn path(&self, n: &str) -> Expr {
        super::throws::path_expr(n, self.span)
    }
    fn usize(&self, n: u32) -> Expr {
        self.e(ExprKind::Literal(AstLit::Int(format!("{n}usize"))))
    }
    fn call(&self, callee: Expr, args: Vec<Expr>) -> Expr {
        self.e(ExprKind::Call {
            callee: Box::new(callee),
            args: args.into_iter().map(CallArg::Positional).collect(),
            trailing_closure: None,
        })
    }
    /// `rok::core::array::<name>(args..)`, through its lang slot.
    fn hook(&self, name: &str, args: Vec<Expr>) -> Expr {
        let callee = self.e(ExprKind::Path(super::expr::lang_fn(
            "array", name, self.span,
        )));
        self.call(callee, args)
    }
    fn method(&self, recv: Expr, m: &str) -> Expr {
        self.method_with(recv, m, Vec::new())
    }
    fn method_with(&self, recv: Expr, m: &str, args: Vec<Expr>) -> Expr {
        self.e(ExprKind::MethodCall {
            receiver: Box::new(recv),
            method: m.to_owned(),
            type_args: Vec::new(),
            args: args.into_iter().map(CallArg::Positional).collect(),
            trailing_closure: None,
            origin: None,
        })
    }
    const fn stmt(&self, kind: StmtKind) -> Stmt {
        Stmt {
            kind,
            span: self.span,
        }
    }
    fn let_(&self, n: &str, init: Expr) -> Stmt {
        self.stmt(StmtKind::Let {
            name: n.to_owned(),
            ty: None,
            init,
        })
    }
    fn block(&self, stmts: Vec<Stmt>, trailing: Option<Expr>) -> Block {
        Block {
            stmts,
            trailing: trailing.map(Box::new),
            span: self.span,
            label: None,
        }
    }
    /// `for <var> in from..to { body }`.
    fn for_(&self, var: &str, from: Expr, to: Expr, body: Vec<Stmt>) -> Stmt {
        let span = self.span;
        self.stmt(StmtKind::Expr(self.e(ExprKind::For {
            label: None,
            pat: rokc_ast::Pattern {
                kind: rokc_ast::PatternKind::Binding {
                    name: var.to_owned(),
                    sub: None,
                },
                span,
            },
            iter: Box::new(self.e(ExprKind::Range {
                start: Some(Box::new(from)),
                end: Some(Box::new(to)),
                inclusive: false,
            })),
            body: self.block(body, None),
            is_await: false,
            by_ref: false,
        })))
    }
}

/// The rank of a fused result: the greatest of `ranks`, at the first
/// operand that has it. Ranks not known here (a generic `N`) must all
/// be the same.
fn lead_rank(ranks: &[(usize, HirTy)]) -> Result<(usize, HirTy), String> {
    let mut lead = ranks[0].clone();
    for (k, r) in &ranks[1..] {
        match (r, &lead.1) {
            (HirTy::Const(a, _), HirTy::Const(b, _)) => {
                if a > b {
                    lead = (*k, r.clone());
                }
            }
            _ if *r == lead.1 => {}
            _ => {
                return Err(
                    "the operands have ranks that are not known here and may differ".to_owned(),
                );
            }
        }
    }
    Ok(lead)
}

/// `rank` is at most `of`.
fn rank_fits(rank: &HirTy, of: &HirTy) -> bool {
    match (rank, of) {
        (HirTy::Const(a, _), HirTy::Const(b, _)) => a <= b,
        _ => rank == of,
    }
}

/// The text of a rank for a message.
fn rank_text(r: &HirTy) -> String {
    match r {
        HirTy::Const(v, _) => v.to_string(),
        HirTy::OpenConst(n) => n.clone(),
        _ => "?".to_owned(),
    }
}

/// The destination of an in-place broadcast: the target's value
/// (lowered), and whether it is leaf 0 of the tree (`.+=`).
struct Into {
    h: Option<HirExpr>,
    rank: HirTy,
}

/// A tree over `Broadcastable` operands as one loop. Each operand is
/// viewed as an array (`broadcast_view`) stretched to the result's
/// shape; the result is a fresh array handed to the leading operand's
/// `Materialize`, or, with `into`, the target's own elements. When the
/// result and every view are contiguous the loop is one run over the
/// buffers; otherwise it walks rows of the last axis.
fn fused(
    ctx: &mut LowerCtx<'_>,
    e: &Expr,
    leaves: Vec<Leaf>,
    tag: usize,
    into: Option<Into>,
) -> HirExpr {
    let span = e.span;
    let ast = Ast { span };
    let ranks: Vec<(usize, HirTy)> = leaves
        .iter()
        .enumerate()
        .filter_map(|(k, l)| match l {
            Leaf::Value {
                bc: Some((_, rank)),
                ..
            } => Some((k, rank.clone())),
            _ => None,
        })
        .collect();
    let lead = match &into {
        Some(dest) => {
            if let Some((_, rank)) = ranks.iter().find(|(_, r)| !rank_fits(r, &dest.rank)) {
                return fail(
                    ctx,
                    span,
                    format!(
                        "an operand of rank {} does not fit a destination of rank {}",
                        rank_text(rank),
                        rank_text(&dest.rank)
                    ),
                    "the result has the destination's shape; select a part of the operand \
                     (`x[0]`) or assign to a new array: `let y = ..;`"
                        .to_owned(),
                );
            }
            None
        }
        None => match lead_rank(&ranks) {
            Ok(l) => Some(l.0),
            Err(msg) => {
                return fail(
                    ctx,
                    span,
                    msg,
                    "give the operands one rank, adding axes of length 1 where needed \
                     (`v[none, :]`)"
                        .to_owned(),
                );
            }
        },
    };
    let mat = ctx.lang("materialize_trait");
    if let (Some(k), false) = (lead, mat.is_empty()) {
        if let Leaf::Value { h, .. } = &leaves[k] {
            if !ctx.has_trait(&h.ty, mat) {
                let t = ctx.ty_name(&h.ty);
                return fail(
                    ctx,
                    span,
                    format!(
                        "`{t}` leads this dot expression (its first operand of the highest \
                         rank) but cannot hold its result: it does not implement `Materialize`"
                    ),
                    format!(
                        "implement `rok::core::ops::Materialize<U, N>` for `{t}`, or put an \
                         `Array` operand of that rank first"
                    ),
                );
            }
        }
    }
    let name = |k: usize| format!("#b{tag}_{k}");
    let view_of = |k: usize| format!("#fv{tag}_{k}");
    let wide = |k: usize| format!("#fw{tag}_{k}");
    let base = |k: usize| format!("#fo{tag}_{k}");
    let step = |k: usize| format!("#ft{tag}_{k}");
    let (dest, shape_n, count) = (
        format!("#fd{tag}"),
        format!("#fs{tag}"),
        format!("#fn{tag}"),
    );
    let (od, sd) = (format!("#fod{tag}"), format!("#fsd{tag}"));
    let (idx, row_i, inner, rows) = (
        format!("#bi{tag}"),
        format!("#br{tag}"),
        format!("#bin{tag}"),
        format!("#brows{tag}"),
    );
    let mut stmts: Vec<Stmt> = Vec::new();
    // Method calls: the receiver's impl fixes the trait's rank.
    let view = |x: Expr| ast.method(x, "broadcast_view");
    // The target first, then the operands in source order.
    let leaf0_dest = matches!(&into, Some(Into { h: None, .. }));
    if let Some(Into { h: Some(h), .. }) = into {
        let t = format!("#bt{tag}");
        let init = splice(ctx, h);
        stmts.push(ast.let_(&t, init));
        stmts.push(ast.let_(&dest, view(ast.path(&t))));
    }
    let mut reads: Vec<Option<Expr>> = Vec::new();
    let mut bcs: Vec<usize> = Vec::new();
    for (k, l) in leaves.into_iter().enumerate() {
        reads.push(match l {
            Leaf::Lit(x) | Leaf::Callee(x) => Some(x),
            Leaf::Value { h, bc, .. } => {
                let init = splice(ctx, h);
                stmts.push(ast.let_(&name(k), init));
                if bc.is_some() {
                    stmts.push(ast.let_(&view_of(k), view(ast.path(&name(k)))));
                    bcs.push(k);
                    None
                } else {
                    Some(ast.path(&name(k)))
                }
            }
        });
    }
    if leaf0_dest {
        stmts.push(ast.let_(&dest, ast.path(&view_of(0))));
    }
    // The shape: from the lead (or the destination), each operand folded in.
    let shape = match lead {
        Some(k) => ast.method(ast.path(&view_of(k)), "shape"),
        None => ast.method(ast.path(&dest), "shape"),
    };
    let mut acc = format!("#fs{tag}_");
    stmts.push(ast.let_(&acc, shape));
    for &k in &bcs {
        if Some(k) != lead {
            let next = ast.hook(
                "fused_shape",
                vec![ast.path(&acc), ast.method(ast.path(&view_of(k)), "shape")],
            );
            acc = format!("#fs{tag}_{k}");
            stmts.push(ast.let_(&acc, next));
        }
    }
    stmts.push(ast.let_(&shape_n, ast.path(&acc)));
    if lead.is_none() {
        let check = ast.hook(
            "fused_dest",
            vec![ast.method(ast.path(&dest), "shape"), ast.path(&shape_n)],
        );
        stmts.push(ast.stmt(StmtKind::Expr(check)));
    }
    for &k in &bcs {
        let mut view = ast.hook(
            "fused_view",
            vec![ast.path(&view_of(k)), ast.path(&shape_n)],
        );
        if lead.is_none() {
            view = ast.hook("fused_source", vec![ast.path(&dest), view]);
        }
        stmts.push(ast.let_(&wide(k), view));
    }
    if lead.is_some() {
        stmts.push(ast.let_(&dest, ast.hook("fused_alloc", vec![ast.path(&shape_n)])));
    }
    stmts.push(ast.let_(&count, ast.method(ast.path(&dest), "len")));
    // One run when everything is contiguous.
    let mut flat = ast.hook("fused_flat", vec![ast.path(&dest)]);
    for &k in &bcs {
        flat = ast.e(ExprKind::OpInfix {
            op: "&&".to_owned(),
            lhs: Box::new(flat),
            rhs: Box::new(ast.hook("fused_flat", vec![ast.path(&wide(k))])),
        });
    }
    let item = |reads: &[Option<Expr>], read: &dyn Fn(usize) -> Expr| {
        let mut at = |k: usize| reads[k].clone().unwrap_or_else(|| read(k));
        rebuild(e, &mut 0, true, &mut at)
    };
    let run = {
        let mut body = vec![ast.let_(
            &od,
            ast.hook(
                "fused_row",
                vec![ast.path(&dest), ast.usize(0), ast.path(&count)],
            ),
        )];
        for &k in &bcs {
            body.push(ast.let_(
                &base(k),
                ast.hook(
                    "fused_row",
                    vec![ast.path(&wide(k)), ast.usize(0), ast.path(&count)],
                ),
            ));
        }
        let x = item(&reads, &|k| {
            ast.hook(
                "fused_at",
                vec![ast.path(&wide(k)), ast.path(&base(k)), ast.path(&idx)],
            )
        });
        let put = ast.hook(
            "fused_put",
            vec![ast.path(&dest), ast.path(&od), ast.path(&idx), x],
        );
        body.push(ast.for_(
            &idx,
            ast.usize(0),
            ast.path(&count),
            vec![ast.stmt(StmtKind::Expr(put))],
        ));
        ast.block(body, None)
    };
    let walk = {
        let mut body = vec![
            ast.let_(&inner, ast.hook("fused_inner", vec![ast.path(&shape_n)])),
            ast.let_(
                &rows,
                ast.hook("fused_rows", vec![ast.path(&count), ast.path(&inner)]),
            ),
            ast.let_(&sd, ast.hook("fused_step", vec![ast.path(&dest)])),
        ];
        for &k in &bcs {
            body.push(ast.let_(&step(k), ast.hook("fused_step", vec![ast.path(&wide(k))])));
        }
        let mut row = vec![ast.let_(
            &od,
            ast.hook(
                "fused_row",
                vec![ast.path(&dest), ast.path(&row_i), ast.path(&inner)],
            ),
        )];
        for &k in &bcs {
            row.push(ast.let_(
                &base(k),
                ast.hook(
                    "fused_row",
                    vec![ast.path(&wide(k)), ast.path(&row_i), ast.path(&inner)],
                ),
            ));
        }
        let x = item(&reads, &|k| {
            ast.hook(
                "fused_get",
                vec![
                    ast.path(&wide(k)),
                    ast.path(&base(k)),
                    ast.path(&idx),
                    ast.path(&step(k)),
                ],
            )
        });
        let set = ast.hook(
            "fused_set",
            vec![
                ast.path(&dest),
                ast.path(&od),
                ast.path(&idx),
                ast.path(&sd),
                x,
            ],
        );
        row.push(ast.for_(
            &idx,
            ast.usize(0),
            ast.path(&inner),
            vec![ast.stmt(StmtKind::Expr(set))],
        ));
        body.push(ast.for_(&row_i, ast.usize(0), ast.path(&rows), row));
        ast.block(body, None)
    };
    stmts.push(ast.stmt(StmtKind::Expr(ast.e(ExprKind::If {
        cond: Box::new(flat),
        then_branch: run,
        else_branch: Some(Box::new(ast.e(ExprKind::Block(walk)))),
    }))));
    let tail = match lead {
        Some(k) => ast.method_with(ast.path(&name(k)), "materialize", vec![ast.path(&dest)]),
        None => ast.e(ExprKind::Tuple(Vec::new())),
    };
    lower_expr(ctx, &block_of(stmts, tail, span))
}

/// `target .= value` and `target .op= value`: the tree's elements
/// written into the target, which keeps its shape. `.op=` is `.=` of
/// `target .op value`, the target read once. The target is a `List` or
/// fixed array (lengths checked, as for a fused tree) or a
/// `Broadcastable` type.
pub(super) fn lower_into(
    ctx: &mut LowerCtx<'_>,
    target: &Expr,
    op: &str,
    value: &Expr,
    span: Span,
) -> HirExpr {
    let dot = op.trim_end_matches('=');
    let combined;
    let tree = if dot == "." {
        value
    } else {
        combined = Expr {
            kind: ExprKind::OpInfix {
                op: dot.to_owned(),
                lhs: Box::new(target.clone()),
                rhs: Box::new(value.clone()),
            },
            span,
        };
        &combined
    };
    let tag = ctx.locals.len();
    // `.=`: the target is lowered first, then the operands.
    let dest = (dot == ".").then(|| lower_expected(ctx, target, HirTy::Unknown));
    let mut leaves = Vec::new();
    collect(ctx, tree, dot != ".", &mut leaves);
    let poisoned = dest.as_ref().is_some_and(|h| h.ty == HirTy::Unknown)
        || leaves
            .iter()
            .any(|l| matches!(l, Leaf::Value { h, .. } if h.ty == HirTy::Unknown));
    if poisoned {
        return poison(span);
    }
    let dest_ty = match (&dest, leaves.first()) {
        (Some(h), _) | (None, Some(Leaf::Value { h, .. })) => h.ty.clone(),
        _ => unreachable!("`.op=` has the target as leaf 0"),
    };
    let elems = |ls: &[Leaf]| -> Vec<HirTy> {
        ls.iter()
            .filter_map(|l| match l {
                Leaf::Value { elem: Some(t), .. }
                | Leaf::Value {
                    bc: Some((t, _)), ..
                } => Some(t.clone()),
                _ => None,
            })
            .collect()
    };
    let scalars_fit = |ls: &[Leaf], es: &[HirTy]| {
        ls.iter().all(|l| match l {
            Leaf::Value {
                h,
                elem: None,
                bc: None,
                operand: true,
                ..
            } => matches!(h.ty, HirTy::Prim(_)) || es.contains(&h.ty),
            _ => true,
        })
    };
    if let Some((elem, rank)) = broadcastable(ctx, &dest_ty) {
        if leaves
            .iter()
            .any(|l| matches!(l, Leaf::Value { elem: Some(_), .. }))
        {
            return mixed(ctx, span);
        }
        let mut es = elems(&leaves);
        es.push(elem);
        if !scalars_fit(&leaves, &es) {
            return not_fusible(ctx, op, span);
        }
        let into = Into { h: dest, rank };
        return fused(ctx, tree, leaves, tag, Some(into));
    }
    if let Some((elem, fixed)) = collection(ctx, &dest_ty) {
        if !matches!(&dest_ty, HirTy::Array(..)) && !is_list(ctx, &dest_ty) {
            return slice_target(ctx, span);
        }
        if leaves
            .iter()
            .any(|l| matches!(l, Leaf::Value { bc: Some(_), .. }))
        {
            return mixed(ctx, span);
        }
        let mut es = elems(&leaves);
        es.push(elem);
        if !scalars_fit(&leaves, &es) {
            return not_fusible(ctx, op, span);
        }
        return core_into(ctx, target, tree, dest, leaves, fixed, tag);
    }
    let t = ctx.ty_name(&dest_ty);
    fail(
        ctx,
        span,
        format!("`{op}` writes element-wise into a `List`, fixed array or `Array`, not `{t}`"),
        if dot == "." {
            "assign the value: `x = ..;`".to_owned()
        } else {
            format!("write the operation out: `x = x {dot} ..;`")
        },
    )
}

/// The base path of a `List`.
fn is_list(ctx: &LowerCtx<'_>, ty: &HirTy) -> bool {
    matches!(ty, HirTy::Struct(id) if enum_base_name(&ctx.struct_def(*id).name) == LIST)
}

fn slice_target(ctx: &LowerCtx<'_>, span: Span) -> HirExpr {
    fail(
        ctx,
        span,
        "an in-place broadcast cannot write into a `Slice`, which is a read-only view".to_owned(),
        "write into the list it views, or assign its elements: `xs[a:b] = ys;`".to_owned(),
    )
}

/// An operand neither a container, a primitive nor the element type.
fn not_fusible(ctx: &LowerCtx<'_>, op: &str, span: Span) -> HirExpr {
    fail(
        ctx,
        span,
        format!(
            "`{op}` takes containers, primitives and values of the element type; an operand \
             here is none of them"
        ),
        "convert that operand to the element type first".to_owned(),
    )
}

/// `.=` into a `List` or fixed array: `for #i in 0..#n { target[#i] =
/// item }`, every container's length checked against the target's.
fn core_into(
    ctx: &mut LowerCtx<'_>,
    target: &Expr,
    tree: &Expr,
    dest: Option<HirExpr>,
    leaves: Vec<Leaf>,
    fixed: Option<u32>,
    tag: usize,
) -> HirExpr {
    let span = tree.span;
    let ast = Ast { span };
    let idx = format!("#bi{tag}");
    let count = format!("#bn{tag}");
    let mut stmts = Vec::new();
    let len_of = match dest {
        Some(h) => {
            let tmp = format!("#bt{tag}");
            let init = splice(ctx, h);
            stmts.push(ast.let_(&tmp, init));
            tmp
        }
        None => format!("#b{tag}_0"),
    };
    let mut many = Vec::new();
    let mut reads = Vec::new();
    for (k, l) in leaves.into_iter().enumerate() {
        reads.push(Some(match l {
            Leaf::Lit(x) | Leaf::Callee(x) => x,
            Leaf::Value {
                h,
                elem,
                fixed: f,
                ..
            } => {
                let nm = format!("#b{tag}_{k}");
                let init = splice(ctx, h);
                stmts.push(ast.let_(&nm, init));
                if elem.is_some() {
                    if let (Some(want), Some(have)) = (fixed, f) {
                        if want != have {
                            return fail(
                                ctx,
                                span,
                                format!("broadcast into an array of length {want} from one of length {have}"),
                                "make both the same length".to_owned(),
                            );
                        }
                    }
                    many.push(nm.clone());
                    spread_item_at(&nm, &ast.path(&idx), span)
                } else {
                    ast.path(&nm)
                }
            }
        }));
    }
    stmts.push(ast.let_(&count, ast.method(ast.path(&len_of), "len")));
    for other in &many {
        let check = ast.call(
            ast.e(ExprKind::Path(super::expr::lang_fn_in(
                "broadcast_len_fn",
                "collections::broadcast",
                "check_len",
                span,
            ))),
            vec![ast.method(ast.path(other), "len"), ast.path(&count)],
        );
        stmts.push(ast.stmt(StmtKind::Expr(check)));
    }
    let mut read = |k: usize| reads[k].take().expect("each leaf once");
    let item = rebuild(tree, &mut 0, true, &mut read);
    let store = ast.stmt(StmtKind::Assign(
        spread_item_at_expr(target, &ast.path(&idx), span),
        item,
    ));
    stmts.push(ast.for_(&idx, ast.usize(0), ast.path(&count), vec![store]));
    lower_expr(
        ctx,
        &block_of(stmts, ast.e(ExprKind::Tuple(Vec::new())), span),
    )
}

/// `target[at]`.
fn spread_item_at_expr(target: &Expr, at: &Expr, span: Span) -> Expr {
    Expr {
        kind: ExprKind::Index(Box::new(target.clone()), Box::new(at.clone())),
        span,
    }
}

/// A `T | none` operand propagates `none`. The first optional
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
