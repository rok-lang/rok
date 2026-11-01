//! A thread-local value held across an `await`
//! (`thread_local_across_await`). Resources left open or released twice
//! are compile errors now (stdlib 2.13.4).

use std::collections::{HashMap, HashSet};

use rokc_diagnostic::Span;
use rokc_hir::{HirBlock, HirExpr, HirExprKind, HirFunction, HirModule, HirStmt, HirTy, StructId};
use rokc_ir_common::{FunctionId, LocalId};

/// A finding of this pass.
pub(super) struct Hit {
    pub lint: &'static str,
    pub span: Span,
    pub msg: String,
    pub notes: Vec<String>,
    pub secondary: Option<(Span, String)>,
}

/// What the check needs of the whole module, made once.
pub(super) struct Resources<'h> {
    fns: HashMap<FunctionId, &'h HirFunction>,
    tls: HashSet<StructId>,
}

impl<'h> Resources<'h> {
    pub(super) fn new(hir: &'h HirModule) -> Self {
        let tls = hir
            .structs
            .iter()
            .filter(|s| s.name.split("__").next() == Some("rok::std::thread::ThreadLocal"))
            .map(|s| s.id)
            .collect();
        Self {
            fns: hir.functions.iter().map(|g| (g.id, g)).collect(),
            tls,
        }
    }
}

/// The checks of `f`.
pub(super) fn check(
    res: &Resources<'_>,
    f: &HirFunction,
    files: &HashSet<u32>,
    hits: &mut Vec<Hit>,
) {
    thread_local_across_await(res, f, files, hits);
}

/// A function's method or own name: `File.read`, `Struct7__read__by_Read`,
/// `m::Sender__send__mono__..`, `m::__impl1a2b_clone__mono__..`.
fn leaf(name: &str) -> &str {
    let n = name.split("__mono__").next().unwrap_or(name);
    let n = n.split("__by_").next().unwrap_or(n);
    let n = n.rsplit("::").next().unwrap_or(n);
    if let Some(rest) = n.strip_prefix("__impl") {
        return rest.split_once('_').map_or(rest, |(_, m)| m);
    }
    let n = n.rsplit('.').next().unwrap_or(n);
    n.rsplit("__").next().unwrap_or(n)
}

fn children(e: &HirExpr) -> Vec<&HirExpr> {
    use HirExprKind as K;
    match &e.kind {
        K::PrimBinOp { lhs, rhs, .. } => vec![lhs, rhs],
        K::PrimUnaryOp { operand: x, .. }
        | K::Cast { operand: x, .. }
        | K::ClosureValue { env: x, .. }
        | K::FieldRead { target: x, .. }
        | K::TupleField { target: x, .. }
        | K::ArrayRepeat { value: x, .. }
        | K::BufNew { len: x, .. }
        | K::RawRead { addr: x, .. }
        | K::AddrOf { place: x }
        | K::UnionWrite { value: x, .. }
        | K::UnionRead { target: x, .. }
        | K::Suspend { value: x, .. }
        | K::SumWrap { value: Some(x), .. } => vec![x],
        K::Call { args: xs, .. }
        | K::StructInit { fields: xs, .. }
        | K::EnumVariantInit { args: xs, .. }
        | K::Tuple { fields: xs }
        | K::ArrayInit { elements: xs, .. }
        | K::CoroNew { args: xs, .. } => xs.iter().collect(),
        K::CallIndirect {
            callee: c, args, ..
        }
        | K::CallClosure {
            closure: c, args, ..
        } => std::iter::once(&**c).chain(args).collect(),
        K::WithField { target, value, .. } => vec![target, value],
        K::RawWrite { addr, value, .. } => vec![addr, value],
        K::BufGet { buf, index, .. }
        | K::ArrayIndex {
            array: buf, index, ..
        } => {
            vec![buf, index]
        }
        K::BufSet {
            buf, index, value, ..
        } => vec![buf, index, value],
        K::BufAtomic {
            buf, index, args, ..
        } => [&**buf, &**index].into_iter().chain(args).collect(),
        _ => Vec::new(),
    }
}

/// An `async` body in evaluation order (`thread_local_across_await`).
#[derive(Default)]
struct Seq {
    n: u32,
    defs: Vec<(LocalId, u32, Span)>,
    uses: Vec<(LocalId, u32, Span)>,
    awaits: Vec<(u32, Span)>,
    loops: Vec<(u32, u32)>,
}
fn seq_block(res: &Resources<'_>, b: &HirBlock, s: &mut Seq) {
    for st in &b.stmts {
        match st {
            HirStmt::Let { local, init } => {
                seq_expr(res, init, s);
                let mut x = init;
                while let HirExprKind::Block(b) = &x.kind {
                    match &b.trailing {
                        Some(t) if b.stmts.is_empty() => x = t,
                        _ => break,
                    }
                }
                let got = match &x.kind {
                    HirExprKind::Call { callee, args } => args.first().is_some_and(|a| {
                        matches!(&a.ty, HirTy::Struct(id) if res.tls.contains(id))
                            && res.fns.get(callee).is_some_and(|f| leaf(&f.name) == "get")
                    }),
                    // `let m = l` of one.
                    HirExprKind::Local(y) => s.defs.iter().any(|d| d.0 == *y),
                    _ => false,
                };
                s.n += 1;
                if got {
                    s.defs.push((*local, s.n, init.span));
                }
            }
            HirStmt::Assign { value: e, .. }
            | HirStmt::StaticAssign { value: e, .. }
            | HirStmt::Expr(e)
            | HirStmt::Panic(e)
            | HirStmt::Return(Some(e))
            | HirStmt::Break { value: Some(e), .. } => seq_expr(res, e, s),
            HirStmt::ArrayAssign {
                array_local,
                index,
                value,
                ..
            } => {
                seq_expr(res, index, s);
                seq_expr(res, value, s);
                s.n += 1;
                s.uses.push((*array_local, s.n, value.span));
            }
            HirStmt::Return(None) | HirStmt::Break { .. } | HirStmt::Continue { .. } => {}
        }
    }
    if let Some(t) = &b.trailing {
        seq_expr(res, t, s);
    }
}
fn seq_expr(res: &Resources<'_>, e: &HirExpr, s: &mut Seq) {
    match &e.kind {
        HirExprKind::Local(x) => {
            s.n += 1;
            s.uses.push((*x, s.n, e.span));
        }
        HirExprKind::Suspend { value, is_final } => {
            seq_expr(res, value, s);
            s.n += 1;
            if !is_final {
                s.awaits.push((s.n, e.span));
            }
        }
        HirExprKind::Block(b) => seq_block(res, b, s),
        HirExprKind::Loop { body, .. } => {
            let start = s.n;
            seq_block(res, body, s);
            s.loops.push((start, s.n));
        }
        HirExprKind::While { cond, body, .. } => {
            let start = s.n;
            seq_expr(res, cond, s);
            seq_block(res, body, s);
            s.loops.push((start, s.n));
        }
        HirExprKind::If {
            cond,
            then_block,
            else_branch,
        } => {
            seq_expr(res, cond, s);
            seq_block(res, then_block, s);
            if let Some(x) = else_branch {
                seq_expr(res, x, s);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            seq_expr(res, scrutinee, s);
            for a in arms {
                if let Some(g) = &a.guard {
                    seq_expr(res, g, s);
                }
                seq_expr(res, &a.body, s);
            }
        }
        _ => {
            for c in children(e) {
                seq_expr(res, c, s);
            }
        }
    }
}
/// `thread_local_across_await`: in an `async` body, a local bound to a
/// `ThreadLocal::get()` result and used after a suspension point.
fn thread_local_across_await(
    res: &Resources<'_>,
    f: &HirFunction,
    files: &HashSet<u32>,
    hits: &mut Vec<Hit>,
) {
    if !f.is_async || res.tls.is_empty() {
        return;
    }
    let Some(body) = &f.body else { return };
    let mut s = Seq::default();
    seq_block(res, body, &mut s);
    let mut seen = HashSet::new();
    for &(l, def, at) in &s.defs {
        if !files.contains(&at.file) {
            continue;
        }
        let after = s.uses.iter().filter(|u| u.0 == l && u.1 > def);
        let hit = after.clone().find_map(|u| {
            let direct = s.awaits.iter().find(|a| def < a.0 && a.0 < u.1);
            // An `await` in a loop the local is used in, made before it.
            let looped = s.loops.iter().find_map(|&(ls, le)| {
                (def <= ls && ls < u.1 && u.1 <= le)
                    .then(|| s.awaits.iter().find(|a| ls < a.0 && a.0 <= le))
                    .flatten()
            });
            direct.or(looped).map(|a| (u.2, a.1))
        });
        let Some((used, waited)) = hit else { continue };
        if !seen.insert((at.file, at.start)) {
            continue;
        }
        let name = f
            .locals
            .iter()
            .find(|x| x.id == l)
            .and_then(|x| x.name.as_deref())
            .filter(|n| !n.starts_with('#'))
            .unwrap_or("it");
        hits.push(Hit {
            lint: "thread_local_across_await",
            span: used,
            msg: format!(
                "`{name}` is a `ThreadLocal::get()` value used after an `await`: the task may \
                 resume on another thread, and this one's value is then shared"
            ),
            notes: vec![format!(
                "help: call `.get()` again after the `await` instead of keeping `{name}` across \
                 it (stdlib 3.4)"
            )],
            secondary: Some((
                waited,
                "the task may move to another thread here".to_owned(),
            )),
        });
    }
}
