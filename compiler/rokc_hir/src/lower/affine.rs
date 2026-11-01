//! Affine resources (stdlib 2.13.4): a value whose type implements `Drop`
//! (a `File`, a socket, a channel `Sender`, an `Allocation`, ..) has one
//! owner. `let b = a`, storing it into a struct, a tuple, an enum or a
//! container, capturing it in a closure or an `async` block and returning
//! it move it; using the old name afterwards is an error, and so is a
//! second release. A struct, tuple or sum holding one is affine too.
//! `clone()` makes an independent value; `marker::replace` / `take` move a
//! part out; `marker::share` makes a handle nobody follows (`unsafe`).
//!
//! A call takes a value only when the callee keeps it: stores or returns
//! it, closes it, or passes it to a call that keeps it. Any other call
//! borrows it, and the caller keeps it. What a callee keeps is a
//! per-function summary, a call-graph fixpoint that ships in ROKHIR.
//!
//! Releasing: the lowering binds each affine `let` (and parameter, and
//! coroutine capture) to a hidden flag and a release on every exit of its
//! scope while the flag is set, as `using` does; this pass clears the flag
//! where the value moves or is closed, and leaves it unset for a value the
//! body does not own. A type whose close can fail (`io::Close`) must be
//! closed explicitly, `try x.close()` or `using`: leaving its scope open on
//! a path is an error, except on a path an error leaves by (its release
//! there drops the close's own error).
//!
//! The walk is a forward flow over structured HIR, per value part
//! (`h.f`): live, moved, released. Joins are unions; loops run to a
//! fixpoint. Values taken out of a container or read through a raw
//! pointer are not followed. A cancelled task leaves through its
//! cleanups: that path is not checked for a missing close.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};

use rokc_ast::{Block, Expr, ExprKind, Stmt, StmtKind};
use rokc_diagnostic::{ErrorKind, RokDiagnostic, Span};
use rokc_ir_common::{ConstVal, FunctionId, LocalId, PrimTy};

use super::throws::{ErrFrame, path_expr};
use super::{Env, LowerCtx};
use crate::{HirBlock, HirExpr, HirExprKind, HirFunction, HirPattern, HirStmt, HirTy};

/// How a value of an affine type is released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// The compiler releases it (`Drop::drop` cannot fail).
    Auto,
    /// Its close can fail: the program closes it.
    Close,
}

/// Lock guards: only `using` releases one (stdlib 2.16.3), checked apart.
const GUARDS: &[&str] = &[
    "rok::core::sync::MutexGuard",
    "rok::core::sync::RwLockReadGuard",
    "rok::core::sync::RwLockWriteGuard",
];

/// Secrets and the keys held in them: their `drop` only wipes early, and
/// the collector wipes them anyway, so copies are fine.
const WIPED: &[&str] = &[
    "rok::core::crypto::Secret",
    "rok::core::crypto::SecretWords",
    "rok::core::crypto::Aead",
    "rok::core::crypto::x25519::PrivateKey",
    "rok::core::crypto::ed25519::SigningKey",
    "rok::core::crypto::ecdsa::SigningKey",
    "rok::core::crypto::rsa::PrivateKey",
    "rok::core::crypto::mlkem::DecapsulationKey768",
];

/// Configurations that name a handle without owning it (a child's
/// `Stdio::File` is the caller's file).
const VIEWS: &[&str] = &["rok::std::process::Stdio", "rok::std::process::Command"];

/// Name prefix of a release flag (`#own<local>`) and of the binding it
/// guards (`#rel<local>`).
const OWN: &str = "#own";
const REL: &str = "#rel";

/// Nesting past which a type is taken as not affine.
const DEPTH: u32 = 32;

/// The release kind of `ty`, if it is affine.
pub(super) fn affine(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<Kind> {
    if !matches!(
        ty,
        HirTy::Struct(_) | HirTy::Enum(_) | HirTy::Tuple(_) | HirTy::Sum(_)
    ) {
        return None;
    }
    affine_in(ctx, ty, 0)
}

/// `affine`, memoized per type: a type nesting itself twice per level
/// (`W<W<W<..>>>` with two fields) is walked once per distinct type. A
/// type met again while it is being classified counts as not affine.
fn affine_in(ctx: &LowerCtx<'_>, ty: &HirTy, depth: u32) -> Option<Kind> {
    if depth > DEPTH {
        return None;
    }
    if let Some(k) = ctx.env.affine_memo.borrow().get(ty) {
        return *k;
    }
    ctx.env.affine_memo.borrow_mut().insert(ty.clone(), None);
    let k = affine_walk(ctx, ty, depth);
    ctx.env.affine_memo.borrow_mut().insert(ty.clone(), k);
    k
}

fn affine_walk(ctx: &LowerCtx<'_>, ty: &HirTy, depth: u32) -> Option<Kind> {
    let join = |a: Option<Kind>, b: Option<Kind>| match (a, b) {
        (Some(Kind::Close), _) | (_, Some(Kind::Close)) => Some(Kind::Close),
        (Some(_), _) | (_, Some(_)) => Some(Kind::Auto),
        _ => None,
    };
    let parts = |ts: &[HirTy]| {
        ts.iter()
            .fold(None, |k, t| join(k, affine_in(ctx, t, depth + 1)))
    };
    match ty {
        HirTy::Struct(sid) => {
            let def = ctx.struct_def(*sid);
            let base = def.name.split("__").next().unwrap_or(&def.name);
            if GUARDS.contains(&base)
                || VIEWS.contains(&base)
                || WIPED.iter().any(|w| {
                    base.ends_with(w.rsplit("::").next().unwrap_or(w))
                        && base.starts_with("rok::core::crypto::")
                })
            {
                return None;
            }
            let own = ctx.has_trait(ty, ctx.lang("drop_method"));
            let close =
                !ctx.lang("close_trait").is_empty() && ctx.has_trait(ty, ctx.lang("close_trait"));
            let fields: Vec<HirTy> = def.fields.iter().map(|f| f.ty.clone()).collect();
            let inner = parts(&fields);
            if close {
                return Some(Kind::Close);
            }
            // Its own `drop` releases its parts and cannot fail.
            if own { Some(Kind::Auto) } else { inner }
        }
        HirTy::Enum(eid) => {
            let def = ctx.enum_def(*eid);
            if VIEWS.contains(&def.name.split("__").next().unwrap_or(&def.name)) {
                return None;
            }
            let tys: Vec<HirTy> = def
                .variants
                .iter()
                .flat_map(super::expr::variant_arg_tys)
                .collect();
            parts(&tys)
        }
        HirTy::Tuple(ts) | HirTy::Sum(ts) => parts(ts),
        _ => None,
    }
}

/// The finally block releasing local `n` when its flag is set.
fn release_block(n: u32, span: Span) -> Block {
    let call = Expr {
        span,
        kind: ExprKind::MethodCall {
            receiver: Box::new(path_expr(&format!("{REL}{n}"), span)),
            method: rokc_ast::USING_DROP_METHOD.to_owned(),
            type_args: Vec::new(),
            args: Vec::new(),
            trailing_closure: None,
            origin: None,
        },
    };
    let release = Expr {
        span,
        kind: ExprKind::If {
            cond: Box::new(path_expr(&format!("{OWN}{n}"), span)),
            then_branch: Block {
                stmts: vec![Stmt {
                    kind: StmtKind::Expr(call),
                    span,
                }],
                trailing: None,
                span,
                label: None,
            },
            else_branch: None,
        },
    };
    Block {
        stmts: vec![Stmt {
            kind: StmtKind::Expr(release),
            span,
        }],
        trailing: None,
        span,
        label: None,
    }
}

const fn flag_value(on: bool, span: Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Const(ConstVal::Bool(on)),
        ty: HirTy::Prim(PrimTy::Bool),
        span,
    }
}

/// `local` (of type `ty`) now holds a value the compiler releases on
/// every exit of the current scope while its flag is set: the flag's
/// `let`, when `ty` is released that way.
pub(super) fn owe(ctx: &mut LowerCtx<'_>, local: LocalId, ty: &HirTy) -> Option<HirStmt> {
    if affine(ctx, ty).is_none() || ctx.lang("drop_method").is_empty() {
        return None;
    }
    let n = local.0;
    let flag = ctx.declare(Some(format!("{OWN}{n}")), HirTy::Prim(PrimTy::Bool));
    if let Some(scope) = ctx.scopes.last_mut() {
        scope.insert(format!("{REL}{n}"), local);
    }
    ctx.err_frames.push(ErrFrame::Finally {
        block: release_block(n, Span::DUMMY),
        hir: Vec::new(),
        try_depth: ctx.try_depth,
        breakables: ctx.breakables.len(),
    });
    Some(HirStmt::Let {
        local: flag,
        init: flag_value(true, Span::DUMMY),
    })
}

/// The lets in `stmts` from `from` on that bind a released value: their
/// flags follow each.
pub(super) fn owe_lets(ctx: &mut LowerCtx<'_>, stmts: &mut Vec<HirStmt>, from: usize) {
    let mut i = from;
    while i < stmts.len() {
        let bound = match &stmts[i] {
            HirStmt::Let { local, init }
                if !is_flag_name(ctx.locals[local.0 as usize].name.as_deref()) =>
            {
                let ty = match ctx.local_ty(*local) {
                    HirTy::Unknown | HirTy::Var(_) => init.ty.clone(),
                    t => t,
                };
                Some((*local, ty))
            }
            _ => None,
        };
        i += 1;
        if let Some((local, ty)) = bound {
            if let Some(s) = owe(ctx, local, &ty) {
                stmts.insert(i, s);
                i += 1;
            }
        }
    }
}

fn is_flag_name(n: Option<&str>) -> bool {
    n.is_some_and(|n| n.starts_with(OWN))
}

/// Close the releases owed since `mark` (err frames): on normal
/// completion of the scope they run after its value.
pub(super) fn settle(
    ctx: &mut LowerCtx<'_>,
    mark: usize,
    stmts: &mut Vec<HirStmt>,
    trailing: &mut Option<HirExpr>,
    span: Span,
) {
    if ctx.err_frames.len() <= mark {
        return;
    }
    let mut blocks = Vec::new();
    while ctx.err_frames.len() > mark {
        match ctx.err_frames.pop() {
            Some(ErrFrame::Finally { block, .. }) => blocks.push(block),
            Some(f) => {
                // Not ours: frames nest, so this cannot happen.
                ctx.err_frames.push(f);
                break;
            }
            None => break,
        }
    }
    let ends = trailing
        .as_ref()
        .map_or_else(|| stmts_diverge(stmts), super::hir_expr_diverges);
    if ends {
        return;
    }
    if let Some(t) = trailing.take() {
        if t.ty == HirTy::Unit {
            stmts.push(HirStmt::Expr(t));
        } else {
            let ty = t.ty.clone();
            let tmp = ctx.declare(Some(format!("#val{}", ctx.locals.len())), ty.clone());
            stmts.push(HirStmt::Let {
                local: tmp,
                init: t,
            });
            *trailing = Some(super::expr::hir_local(tmp, &ty, span));
        }
    }
    for b in blocks {
        ctx.push_expected(HirTy::Unit);
        let h = super::expr::lower_block(ctx, &b);
        ctx.pop_expected();
        stmts.push(HirStmt::Expr(HirExpr {
            kind: HirExprKind::Block(Box::new(h)),
            ty: HirTy::Unit,
            span,
        }));
    }
}

fn stmts_diverge(stmts: &[HirStmt]) -> bool {
    stmts.last().is_some_and(|s| match s {
        HirStmt::Return(_)
        | HirStmt::Panic(_)
        | HirStmt::Break { .. }
        | HirStmt::Continue { .. } => true,
        HirStmt::Expr(e) => super::hir_expr_diverges(e),
        _ => false,
    })
}

/// `settle` for a body that is one expression (a closure's, a coroutine's).
pub(super) fn settle_expr(ctx: &mut LowerCtx<'_>, mark: usize, e: HirExpr) -> HirExpr {
    if ctx.err_frames.len() <= mark {
        return e;
    }
    let (ty, span) = (e.ty.clone(), e.span);
    let mut stmts = Vec::new();
    let mut trailing = Some(e);
    settle(ctx, mark, &mut stmts, &mut trailing, span);
    let ty = if trailing.is_none() && ty != HirTy::Never {
        HirTy::Unit
    } else {
        ty
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

/// `x = value` for a local with a release flag: the old value is
/// released first (after `value`, which may move it), then the flag set.
pub(super) fn reassign(ctx: &mut LowerCtx<'_>, stmt: HirStmt, span: Span) -> HirStmt {
    let HirStmt::Assign { local, value } = stmt else {
        return stmt;
    };
    let Some(flag) = ctx.lookup(&format!("{OWN}{}", local.0)) else {
        return HirStmt::Assign { local, value };
    };
    let ty = value.ty.clone();
    let tmp = ctx.declare(Some(format!("#mv{}", local.0)), ty.clone());
    ctx.push_expected(HirTy::Unit);
    let release = super::expr::lower_block(ctx, &release_block(local.0, Span::DUMMY));
    ctx.pop_expected();
    let stmts = vec![
        HirStmt::Let {
            local: tmp,
            init: value,
        },
        HirStmt::Expr(HirExpr {
            kind: HirExprKind::Block(Box::new(release)),
            ty: HirTy::Unit,
            span,
        }),
        HirStmt::Assign {
            local,
            value: super::expr::hir_local(tmp, &ty, span),
        },
        HirStmt::Assign {
            local: flag,
            value: flag_value(true, Span::DUMMY),
        },
    ];
    HirStmt::Expr(HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: None,
            label: None,
        })),
        ty: HirTy::Unit,
        span,
    })
}

// The check.

const LIVE: u8 = 1;
const MOVED: u8 = 2;
const RELEASED: u8 = 4;

/// Pseudo parameter index of capture `k` of a closure or coroutine body.
const SLOT: u32 = 1 << 16;

/// Methods that close or release what they are called on.
const CLOSERS: &[&str] = &[
    "close",
    "close_async",
    "try_close",
    "unmap",
    "wipe",
    "wait_with_output",
];

/// What a function does with its parameters, as callers see it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Summary {
    /// Parameters (and captures, `SLOT + k`) the call keeps.
    keeps: BTreeSet<u32>,
    /// The result may be a value it does not own (a container's element).
    alias: bool,
}

impl Summary {
    fn encode(&self) -> String {
        let keeps: Vec<String> = self.keeps.iter().map(u32::to_string).collect();
        format!("{}|{}", keeps.join(","), u8::from(self.alias))
    }

    fn decode(s: &str) -> Option<Self> {
        let (k, a) = s.split_once('|')?;
        let keeps = k
            .split(',')
            .filter(|x| !x.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        Some(Self {
            keeps,
            alias: a == "1",
        })
    }
}

/// `rok::core::marker::share`: a handle that is not followed.
const SHARE: &str = "rok::core::marker::share";
/// `marker::replace(ref place, v)` / `marker::take(ref place)`: the part
/// moves out, the place keeps a new value.
const SWAPS: &[&str] = &["rok::core::marker::replace", "rok::core::marker::take"];

/// Key prefix of a summary in the stored list (beside the hand-off ones).
const KEY: &str = "#affine\u{1f}";

/// What an expression's value is, for the binding that takes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Val {
    /// Not affine (or `none`): nothing to follow.
    Plain,
    /// A value this body owns.
    Owned,
    /// An affine value it does not own (a container's element).
    Alias,
}

const fn combine(a: Val, b: Val) -> Val {
    match (a, b) {
        (Val::Alias, _) | (_, Val::Alias) => Val::Alias,
        (Val::Owned, _) | (_, Val::Owned) => Val::Owned,
        _ => Val::Plain,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Use,
    Move,
}

type State = Option<Vec<u8>>;

fn join(a: State, b: State) -> State {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(mut a), Some(b)) => {
            if a.len() < b.len() {
                a.resize(b.len(), 0);
            }
            for (x, y) in a.iter_mut().zip(b) {
                *x |= y;
            }
            Some(a)
        }
    }
}

fn get(st: &State, i: usize) -> u8 {
    st.as_ref().and_then(|v| v.get(i).copied()).unwrap_or(0)
}

fn set(st: &mut State, i: usize, v: u8) {
    if let Some(s) = st {
        if s.len() <= i {
            s.resize(i + 1, 0);
        }
        s[i] = v;
    }
}

struct Checker<'a, 'e> {
    env: &'a Env<'e>,
    ctx: &'a LowerCtx<'e>,
    fns: HashMap<FunctionId, &'a HirFunction>,
    sums: RefCell<HashMap<FunctionId, Summary>>,
    /// Captures a creator moved in: (body, capture index).
    owned_slots: RefCell<HashSet<(FunctionId, u32)>>,
    memo: RefCell<HashMap<HirTy, Option<Shape>>>,
}

/// A type's release kind and its affine parts (field paths).
type Shape = (Kind, Vec<Vec<u32>>);

/// Rewrites a final walk asks for.
#[derive(Default)]
struct Edits {
    /// Flag `let`s / assignments (their value) to start unset.
    unset: HashSet<usize>,
    /// Places a value moves out of: the flags to clear there.
    clears: HashMap<usize, Vec<LocalId>>,
}

fn addr(e: &HirExpr) -> usize {
    std::ptr::from_ref(e) as usize
}

impl Checker<'_, '_> {
    /// The release kind and the affine parts (field paths) of `ty`.
    fn shape(&self, ty: &HirTy) -> Option<Shape> {
        let kind = affine(self.ctx, ty)?;
        if let Some(s) = self.memo.borrow().get(ty) {
            return s.clone();
        }
        let mut leaves = Vec::new();
        self.leaves(ty, &mut Vec::new(), &mut leaves, 0);
        let s = Some((kind, leaves));
        self.memo.borrow_mut().insert(ty.clone(), s.clone());
        s
    }

    /// The affine parts of `ty` below `path`: a struct without its own
    /// `Drop` and a tuple are split into their fields.
    fn leaves(&self, ty: &HirTy, path: &mut Vec<u32>, out: &mut Vec<Vec<u32>>, depth: u32) {
        if affine(self.ctx, ty).is_none() {
            return;
        }
        let fields: Option<Vec<HirTy>> = match ty {
            HirTy::Struct(sid)
                if depth < 8 && !self.ctx.has_trait(ty, self.ctx.lang("drop_method")) =>
            {
                Some(
                    self.ctx
                        .struct_def(*sid)
                        .fields
                        .iter()
                        .map(|f| f.ty.clone())
                        .collect(),
                )
            }
            HirTy::Tuple(ts) if depth < 8 => Some(ts.clone()),
            _ => None,
        };
        match fields {
            Some(fs) => {
                for (i, t) in fs.iter().enumerate() {
                    path.push(u32::try_from(i).unwrap_or(u32::MAX));
                    self.leaves(t, path, out, depth + 1);
                    path.pop();
                }
            }
            None => out.push(path.clone()),
        }
    }

    fn summary(&self, id: FunctionId) -> Option<Summary> {
        self.sums.borrow().get(&id).cloned()
    }

    fn ty_name(&self, ty: &HirTy) -> String {
        self.ctx.ty_name(ty)
    }
}

/// One followed value: a local and its parts.
struct Root {
    name: String,
    ty: HirTy,
    kind: Kind,
    /// This body must release it.
    owned: bool,
    /// A borrowed parameter (or capture): moving it keeps it.
    param: Option<u32>,
    leaves: Vec<Vec<u32>>,
    base: usize,
    at: Span,
}

struct Target {
    label: Option<String>,
    is_loop: bool,
    depth: usize,
    breaks: State,
    conts: State,
    /// What the `break`s carry out.
    val: Option<Val>,
}

struct Walk<'c, 'a, 'e> {
    c: &'c Checker<'a, 'e>,
    f: &'a HirFunction,
    report: bool,
    /// On a path that is not checked (a cancelled task's way out).
    quiet: bool,
    /// Binding an assignment's value.
    assigning: bool,
    roots: Vec<Root>,
    root_of: HashMap<LocalId, usize>,
    /// Pattern bindings that name a part of a followed place.
    alias_of: HashMap<LocalId, (usize, Vec<u32>)>,
    slots: usize,
    /// Slot -> where it last moved, and whether that released it.
    why: HashMap<usize, (Span, bool)>,
    scopes: Vec<Vec<usize>>,
    targets: Vec<Target>,
    out: Summary,
    /// Params (and captures) owned at entry.
    owned_params: HashSet<u32>,
    flags: HashMap<LocalId, LocalId>,
    flag_locals: HashSet<LocalId>,
    /// Var -> whether its last binding was owned (for its flag).
    bound: HashMap<LocalId, bool>,
    edits: Edits,
    /// The result of the last call with `ref` parameters.
    ref_result: Val,
    inits: HashMap<LocalId, &'a HirExpr>,
    env_local: Option<LocalId>,
    errors: Vec<RokDiagnostic>,
    seen: HashSet<(u32, u32, String)>,
    last: Span,
}

impl<'c, 'a, 'e> Walk<'c, 'a, 'e> {
    fn new(c: &'c Checker<'a, 'e>, f: &'a HirFunction, report: bool) -> Self {
        let mut flags = HashMap::new();
        let mut flag_locals = HashSet::new();
        for l in &f.locals {
            if let Some(n) = l.name.as_deref().and_then(|n| n.strip_prefix(OWN)) {
                if let Ok(v) = n.parse::<u32>() {
                    flags.insert(LocalId(v), l.id);
                    flag_locals.insert(l.id);
                }
            }
        }
        let mut inits = HashMap::new();
        if let Some(b) = &f.body {
            let mut lets = |b: &'a HirBlock| {
                for s in &b.stmts {
                    if let HirStmt::Let { local, init } = s {
                        inits.insert(*local, init);
                    }
                }
            };
            lets(b);
            crate::each_block(b, &mut |e| {
                if let HirExprKind::Block(b)
                | HirExprKind::Loop { body: b, .. }
                | HirExprKind::While { body: b, .. }
                | HirExprKind::If { then_block: b, .. } = &e.kind
                {
                    lets(b);
                }
            });
        }
        let env_local = f
            .params
            .first()
            .filter(|p| p.name == "__env")
            .map(|p| p.local);
        Self {
            c,
            f,
            report,
            quiet: false,
            assigning: false,
            roots: Vec::new(),
            root_of: HashMap::new(),
            alias_of: HashMap::new(),
            slots: 0,
            why: HashMap::new(),
            scopes: vec![Vec::new()],
            targets: Vec::new(),
            out: Summary::default(),
            owned_params: HashSet::new(),
            flags,
            flag_locals,
            bound: HashMap::new(),
            edits: Edits::default(),
            ref_result: Val::Plain,
            inits,
            env_local,
            errors: Vec::new(),
            seen: HashSet::new(),
            last: f.span,
        }
    }

    fn local_name(&self, l: LocalId) -> Option<&'a str> {
        self.f
            .locals
            .get(l.0 as usize)
            .filter(|x| x.id == l)
            .or_else(|| self.f.locals.iter().find(|x| x.id == l))
            .and_then(|x| x.name.as_deref())
            .filter(|n| !n.starts_with('#') && !n.starts_with("__"))
    }

    fn local_ty(&self, l: LocalId) -> Option<&'a HirTy> {
        self.f
            .locals
            .get(l.0 as usize)
            .filter(|x| x.id == l)
            .or_else(|| self.f.locals.iter().find(|x| x.id == l))
            .map(|x| &x.ty)
    }

    fn error(&mut self, at: Span, msg: String, extra: impl FnOnce(RokDiagnostic) -> RokDiagnostic) {
        if !self.report || self.quiet || at.is_dummy() {
            return;
        }
        if !self.seen.insert((at.file, at.start, msg.clone())) {
            return;
        }
        let d = extra(RokDiagnostic::error(ErrorKind::Handoff, at, msg));
        self.errors.push(d);
    }

    fn line(&self, s: Span, from: Span) -> String {
        if s.file == from.file {
            format!("line {}", s.line)
        } else {
            let path = self.c.env.sources.path(s.file).unwrap_or_default();
            let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
            format!("{name}:{}", s.line)
        }
    }

    /// Follow local `l` (of `ty`) from here: a new root, or the old one
    /// reset.
    fn bind(
        &mut self,
        l: LocalId,
        ty: &HirTy,
        owned: bool,
        param: Option<u32>,
        at: Span,
        st: &mut State,
    ) -> Option<usize> {
        let (kind, leaves) = self.c.shape(ty)?;
        if leaves.is_empty() {
            return None;
        }
        let fresh = !self.root_of.contains_key(&l);
        let r = if let Some(&r) = self.root_of.get(&l) {
            self.roots[r].owned = owned;
            self.roots[r].param = param;
            r
        } else {
            let name = self.local_name(l).unwrap_or("this value").to_owned();
            let base = self.slots;
            self.slots += leaves.len();
            self.roots.push(Root {
                name,
                ty: ty.clone(),
                kind,
                owned,
                param,
                leaves,
                base,
                at,
            });
            let r = self.roots.len() - 1;
            self.root_of.insert(l, r);
            r
        };
        self.roots[r].at = at;
        let root = &self.roots[r];
        for i in 0..root.leaves.len() {
            set(st, root.base + i, LIVE);
            self.why.remove(&(root.base + i));
        }
        // A variable leaves with the scope that declared it (an
        // assignment declares nothing).
        if fresh || !self.assigning {
            if let Some(s) = self.scopes.last_mut() {
                if !s.contains(&r) {
                    s.push(r);
                }
            }
        }
        Some(r)
    }

    /// Stop following local `l` (it now holds what this body does not own).
    fn unbind(&self, l: LocalId, st: &mut State) {
        if let Some(&r) = self.root_of.get(&l) {
            let root = &self.roots[r];
            for i in 0..root.leaves.len() {
                set(st, root.base + i, 0);
            }
        }
    }

    /// The root and field path `e` reads, if it is a followed place.
    fn place(&self, e: &HirExpr) -> Option<(usize, Vec<u32>)> {
        match &e.kind {
            HirExprKind::Local(l) => self
                .alias_of
                .get(l)
                .cloned()
                .or_else(|| self.root_of.get(l).map(|&r| (r, Vec::new()))),
            HirExprKind::FieldRead {
                target,
                field_index,
                ..
            } => {
                let (r, mut p) = self.place(target)?;
                p.push(*field_index);
                Some((r, p))
            }
            HirExprKind::TupleField { target, index, .. } => {
                let (r, mut p) = self.place(target)?;
                p.push(*index);
                Some((r, p))
            }
            _ => None,
        }
    }

    /// The slots of root `r` at `path` and below (or the one `path` is
    /// inside of).
    fn slots_at(&self, r: usize, path: &[u32]) -> Vec<usize> {
        let root = &self.roots[r];
        root.leaves
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with(path) || path.starts_with(l))
            .map(|(i, _)| root.base + i)
            .collect()
    }

    fn shown(&self, r: usize, path: &[u32]) -> String {
        let root = &self.roots[r];
        let mut s = root.name.clone();
        let mut ty = root.ty.clone();
        for &i in path {
            match &ty {
                HirTy::Struct(sid) => {
                    let def = self.c.ctx.struct_def(*sid);
                    match def.fields.get(i as usize) {
                        Some(f) => {
                            s = format!("{s}.{}", f.name);
                            ty = f.ty.clone();
                        }
                        None => break,
                    }
                }
                HirTy::Tuple(ts) => {
                    s = format!("{s}.{i}");
                    ty = ts.get(i as usize).cloned().unwrap_or(HirTy::Unknown);
                }
                _ => break,
            }
        }
        s
    }

    /// A use of the place `(r, path)` at `at`: an error after a move.
    fn use_at(&mut self, r: usize, path: &[u32], at: Span, st: &State, releasing: bool) {
        for s in self.slots_at(r, path) {
            let v = get(st, s);
            if v & (MOVED | RELEASED) == 0 {
                continue;
            }
            let who = self.shown(r, path);
            let Some(&(site, released)) = self.why.get(&s) else {
                continue;
            };
            let maybe = v & LIVE != 0;
            let did = if released { "closed" } else { "moved" };
            let line = self.line(site, at);
            let head = if maybe {
                format!("`{who}` may have been {did} at {line}")
            } else {
                format!("`{who}` was {did} at {line}")
            };
            let msg = if releasing {
                format!("{head}; releasing it again here would release it twice")
            } else {
                format!("{head}, and is used here")
            };
            let ty = self.c.ty_name(&self.roots[r].ty);
            let help = if released {
                format!(
                    "help: a `{ty}` is released once; do the work before it is closed, or open \
                     a new one"
                )
            } else {
                format!(
                    "help: a `{ty}` has one owner, and `{who}` gave it away; use the new owner, \
                     or keep a copy with `.clone()` before it moves (stdlib 2.13.4)"
                )
            };
            let here = if released {
                "closed here"
            } else {
                "moved here"
            };
            self.error(at, msg, |d| d.with_secondary(site, here).with_note(help));
            return;
        }
    }

    /// Move (or release) the place `(r, path)`.
    fn move_at(&mut self, r: usize, path: &[u32], e: &HirExpr, st: &mut State, release: bool) {
        let at = e.span;
        self.use_at(r, path, at, st, false);
        let slots = self.slots_at(r, path);
        for &s in &slots {
            set(st, s, if release { RELEASED } else { MOVED });
            self.why.insert(s, (at, release));
        }
        let root = &self.roots[r];
        if !root.owned {
            if let Some(p) = root.param {
                self.out.keeps.insert(p);
            }
        }
        if self.report {
            let flag = self.root_local(r).and_then(|l| self.flags.get(&l).copied());
            if let Some(flag) = flag {
                self.edits.clears.entry(addr(e)).or_default().push(flag);
            }
        }
    }

    fn root_local(&self, r: usize) -> Option<LocalId> {
        self.root_of.iter().find(|&(_, &x)| x == r).map(|(l, _)| *l)
    }

    /// A `drop` (from `using`, or a release the compiler adds) of `e`.
    fn dropped(&mut self, e: &'a HirExpr, st: &mut State) {
        let Some((r, path)) = self.place(e) else {
            let s = std::mem::take(st);
            *st = self.expr(e, s, Mode::Use).0;
            return;
        };
        for s in self.slots_at(r, &path) {
            let v = get(st, s);
            if v & MOVED != 0 {
                self.use_at(r, &path, e.span, st, true);
                break;
            }
        }
        for s in self.slots_at(r, &path) {
            if get(st, s) & LIVE != 0 {
                set(st, s, RELEASED);
                self.why.insert(s, (e.span, true));
            }
        }
        if !self.roots[r].owned {
            if let Some(p) = self.roots[r].param {
                self.out.keeps.insert(p);
            }
        }
        if self.report {
            let flag = self.root_local(r).and_then(|l| self.flags.get(&l).copied());
            if let Some(flag) = flag {
                self.edits.clears.entry(addr(e)).or_default().push(flag);
            }
        }
    }

    /// Roots of the scopes from `depth` on leave here: a value whose close
    /// can fail must be closed by now.
    fn leave(&mut self, st: &State, depth: usize, at: Span) {
        let roots: Vec<usize> = self.scopes[depth.min(self.scopes.len())..]
            .iter()
            .flatten()
            .copied()
            .collect();
        for r in roots {
            self.leaves_open(r, st, at);
        }
    }

    fn leaves_open(&mut self, r: usize, st: &State, at: Span) {
        let root = &self.roots[r];
        if !root.owned || root.kind != Kind::Close {
            return;
        }
        let open: Vec<usize> = (0..root.leaves.len())
            .filter(|&i| get(st, root.base + i) & LIVE != 0)
            .collect();
        let Some(&first) = open.first() else { return };
        let path = root.leaves[first].clone();
        let who = self.shown(r, &path);
        let somewhere = get(st, root.base + first) & (MOVED | RELEASED) != 0;
        let ty = {
            let mut t = root.ty.clone();
            for &i in &path {
                t = match &t {
                    HirTy::Struct(sid) => self
                        .c
                        .ctx
                        .struct_def(*sid)
                        .fields
                        .get(i as usize)
                        .map_or(HirTy::Unknown, |f| f.ty.clone()),
                    HirTy::Tuple(ts) => ts.get(i as usize).cloned().unwrap_or(HirTy::Unknown),
                    _ => t,
                };
            }
            self.c.ty_name(&t)
        };
        let bound = root.at;
        let msg = if somewhere {
            format!("`{who}` holds a `{ty}` that is not closed on every path")
        } else {
            format!("`{who}` holds a `{ty}` that is never closed")
        };
        let help = format!(
            "help: closing a `{ty}` can fail, so it is not closed for you: add `try {who}.close();` \
             before it leaves its scope, or bind it with `using {} = .. {{ .. }}` (stdlib 2.13.4)",
            root.name
        );
        self.error(bound, msg, |d| {
            d.with_secondary(at, "it is still open when it leaves here")
                .with_note(help)
        });
    }

    fn block(&mut self, b: &'a HirBlock, mut st: State, mode: Mode) -> (State, Val) {
        self.scopes.push(Vec::new());
        if b.label.is_some() {
            self.targets.push(Target {
                label: b.label.clone(),
                is_loop: false,
                depth: self.scopes.len() - 1,
                breaks: None,
                conts: None,
                val: None,
            });
        }
        for s in &b.stmts {
            st = self.stmt(s, st);
        }
        let mut val = Val::Plain;
        if let Some(t) = &b.trailing {
            let (s, v) = self.expr(t, st, mode);
            st = s;
            val = v;
        }
        let depth = self.scopes.len() - 1;
        if st.is_some() {
            let at = b.trailing.as_ref().map_or(self.last, |t| t.span);
            self.leave(&st, depth, at);
        }
        if b.label.is_some() {
            let t = self.targets.pop().expect("the block's target");
            if t.breaks.is_some() {
                let bv = t.val.unwrap_or(Val::Plain);
                val = if st.is_some() { combine(val, bv) } else { bv };
            }
            st = join(st, t.breaks);
        }
        let scope = self.scopes.pop().unwrap_or_default();
        for r in scope {
            let root = &self.roots[r];
            for i in 0..root.leaves.len() {
                set(&mut st, root.base + i, 0);
            }
        }
        (st, val)
    }

    fn stmt(&mut self, s: &'a HirStmt, st: State) -> State {
        st.as_ref()?;
        match s {
            HirStmt::Let { local, init } => {
                if self.flag_locals.contains(local) {
                    self.flag_init(*local, init);
                    return st;
                }
                self.bind_value(*local, init, st)
            }
            HirStmt::Assign { local, value } => {
                if self.flag_locals.contains(local) {
                    self.flag_init(*local, value);
                    return st;
                }
                if self.write_back(*local, value) {
                    return self.expr(value, st, Mode::Use).0;
                }
                if let HirExprKind::WithField {
                    target,
                    field_index,
                    value: v,
                    ..
                } = &value.kind
                {
                    if matches!(target.kind, HirExprKind::Local(l) if l == *local) {
                        return self.field_write(*local, *field_index, v, st);
                    }
                }
                let mut st = st;
                if let Some(&r) = self.root_of.get(local) {
                    let open = self.roots[r].owned
                        && self.roots[r].kind == Kind::Close
                        && (0..self.roots[r].leaves.len())
                            .any(|i| get(&st, self.roots[r].base + i) & LIVE != 0);
                    if open {
                        let who = self.roots[r].name.clone();
                        let ty = self.c.ty_name(&self.roots[r].ty);
                        self.error(
                            value.span,
                            format!("`{who}` still holds an open `{ty}` when this replaces it"),
                            |d| {
                                d.with_note(format!(
                                    "help: close it first: `try {who}.close();` (stdlib 2.13.4)"
                                ))
                            },
                        );
                        let root = &self.roots[r];
                        for i in 0..root.leaves.len() {
                            set(&mut st, root.base + i, 0);
                        }
                    }
                }
                self.assigning = true;
                let st = self.bind_value(*local, value, st);
                self.assigning = false;
                st
            }
            HirStmt::ArrayAssign { index, value, .. } => {
                let st = self.expr(index, st, Mode::Use).0;
                self.expr(value, st, Mode::Move).0
            }
            HirStmt::StaticAssign { value, .. } => self.expr(value, st, Mode::Move).0,
            HirStmt::Expr(e) => {
                if self.release_if(e) {
                    return st;
                }
                let (st, v) = self.expr(e, st, Mode::Use);
                if v == Val::Owned && e.ty != HirTy::Never {
                    if let Some((Kind::Close, _)) = self.c.shape(&e.ty) {
                        let ty = self.c.ty_name(&e.ty);
                        self.error(
                            e.span,
                            format!("the `{ty}` made here is never closed"),
                            |d| {
                                d.with_note(format!(
                                    "help: closing a `{ty}` can fail, so it is not closed for \
                                     you: bind it and close it (`let x = ..; try x.close();`), or \
                                     use `using` (stdlib 2.13.4)"
                                ))
                            },
                        );
                    }
                }
                st
            }
            HirStmt::Return(v) => {
                let st = match v {
                    Some(v) => {
                        let (st, val) = self.expr(v, st, Mode::Move);
                        if val == Val::Alias {
                            self.out.alias = true;
                        }
                        st
                    }
                    None => st,
                };
                if let Some(v) = v {
                    if !v.span.is_dummy() {
                        self.last = v.span;
                    }
                }
                // An error leaving releases what is still open: its own
                // error is the one reported.
                if st.is_some() && !v.as_ref().is_some_and(is_throw) {
                    let at = v.as_ref().map_or(self.last, |v| v.span);
                    self.leave(&st, 0, at);
                }
                None
            }
            HirStmt::Break { label, value } => {
                let (st, bv) = match value {
                    Some(v) => self.expr(v, st, Mode::Move),
                    None => (st, Val::Plain),
                };
                // A cancelled task's way out reaches no code that is
                // checked: its release flags are not known here.
                if self.quiet {
                    return None;
                }
                let i = self.target(label.as_deref(), false);
                if let Some(i) = i {
                    let depth = self.targets[i].depth;
                    let raise = value.is_none()
                        && label.as_deref().is_some_and(|l| l.starts_with("#catch"));
                    if st.is_some() && !raise {
                        self.leave(&st, depth + usize::from(self.targets[i].is_loop), self.last);
                    }
                    let t = &mut self.targets[i];
                    if st.is_some() {
                        t.val = Some(t.val.map_or(bv, |x| combine(x, bv)));
                    }
                    t.breaks = join(t.breaks.take(), st);
                }
                None
            }
            HirStmt::Continue { label } => {
                let i = self.target(label.as_deref(), true);
                if let Some(i) = i {
                    let depth = self.targets[i].depth + 1;
                    if st.is_some() {
                        self.leave(&st, depth, self.last);
                    }
                    let t = &mut self.targets[i];
                    t.conts = join(t.conts.take(), st);
                }
                None
            }
            HirStmt::Panic(e) => {
                self.expr(e, st, Mode::Use);
                None
            }
        }
    }

    fn target(&self, label: Option<&str>, needs_loop: bool) -> Option<usize> {
        self.targets.iter().rposition(|t| match label {
            None => t.is_loop,
            Some(l) => t.label.as_deref() == Some(l) && (t.is_loop || !needs_loop),
        })
    }

    /// A flag's `let` or assignment: unset when its value is not owned.
    fn flag_init(&mut self, flag: LocalId, value: &'a HirExpr) {
        if !self.report {
            return;
        }
        let var = self
            .flags
            .iter()
            .find(|&(_, &f)| f == flag)
            .map(|(v, _)| *v);
        let owned = var
            .and_then(|v| self.bound.get(&v).copied())
            .unwrap_or(false);
        if owned {
            self.edits.unset.remove(&addr(value));
        } else {
            self.edits.unset.insert(addr(value));
        }
    }

    /// `local = <call>.k`: a `ref` argument written back.
    fn write_back(&self, local: LocalId, value: &HirExpr) -> bool {
        let HirExprKind::TupleField { target, index, .. } = &value.kind else {
            return false;
        };
        if *index == 0 {
            return false;
        }
        let call = match &target.kind {
            HirExprKind::Call { .. } => Some(&**target),
            HirExprKind::Local(t) => self.inits.get(t).copied(),
            _ => None,
        };
        let Some(HirExpr {
            kind: HirExprKind::Call { callee, args },
            ..
        }) = call
        else {
            return false;
        };
        let Some(f) = self.c.fns.get(callee) else {
            return false;
        };
        let refs: Vec<usize> = f
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| p.by_ref)
            .map(|(i, _)| i)
            .collect();
        refs.get(*index as usize - 1)
            .and_then(|&i| args.get(i))
            .is_some_and(|a| base_local(a) == Some(local))
    }

    /// `local` takes `value` (a `let` or an assignment).
    fn bind_value(&mut self, local: LocalId, value: &'a HirExpr, st: State) -> State {
        let ty = self
            .local_ty(local)
            .filter(|t| !matches!(t, HirTy::Unknown | HirTy::Var(_)))
            .unwrap_or(&value.ty)
            .clone();
        let affine = self.c.shape(&ty).is_some();
        // A capture read out of a closure's or coroutine's environment.
        if let (Some(env), HirExprKind::TupleField { target, index, .. }) =
            (self.env_local, &value.kind)
        {
            if matches!(target.kind, HirExprKind::Local(l) if l == env) {
                if affine {
                    let mut st = st;
                    let k = SLOT + index;
                    let owned = self.owned_params.contains(&k);
                    self.bind(local, &ty, owned, Some(k), value.span, &mut st);
                    self.bound.insert(local, owned);
                    return st;
                }
                return st;
            }
        }
        // A temporary the lowering binds to a place (a `for` loop's
        // iterator), and the parts a `using` pattern binds, are views.
        let view = self.place(value).is_some_and(|(r, _)| {
            let l = self.root_local(r);
            let temp = self
                .f
                .locals
                .iter()
                .find(|x| x.id == local)
                .and_then(|x| x.name.as_deref())
                .is_some_and(|n| n.starts_with("#for"));
            temp || l
                .and_then(|l| self.f.locals.iter().find(|x| x.id == l))
                .and_then(|x| x.name.as_deref())
                .is_some_and(|n| n.starts_with("#using"))
        });
        let mode = if affine && !view {
            Mode::Move
        } else {
            Mode::Use
        };
        let (mut st, val) = self.expr(value, st, mode);
        if !affine {
            return st;
        }
        let val = if view { Val::Alias } else { val };
        if val == Val::Owned {
            self.bind(local, &ty, true, None, value.span, &mut st);
        } else {
            self.unbind(local, &mut st);
        }
        self.bound.insert(local, val == Val::Owned);
        st
    }

    /// `local.f = value`.
    fn field_write(&mut self, local: LocalId, field: u32, value: &'a HirExpr, st: State) -> State {
        // `ref h.f` written back by a call.
        if self.write_back(local, value) {
            return self.expr(value, st, Mode::Use).0;
        }
        // `h.f.x = v`: `h.f` updated in place.
        if let HirExprKind::WithField { target, .. } = &value.kind {
            if self
                .place(target)
                .is_some_and(|(r, p)| self.root_of.get(&local) == Some(&r) && p == [field])
            {
                return self.expr(value, st, Mode::Use).0;
            }
        }
        let affine = self.c.shape(&value.ty).is_some();
        let mode = if affine { Mode::Move } else { Mode::Use };
        let (mut st, val) = self.expr(value, st, mode);
        let Some(&r) = self.root_of.get(&local) else {
            return st;
        };
        let slots = self.slots_at(r, &[field]);
        for s in slots {
            set(&mut st, s, if val == Val::Owned { LIVE } else { 0 });
            self.why.remove(&s);
        }
        st
    }

    /// The release the lowering adds: `if #own { drop(x) }`, not walked.
    fn release_if(&self, e: &HirExpr) -> bool {
        match &e.kind {
            HirExprKind::If { cond, .. } => {
                matches!(cond.kind, HirExprKind::Local(l) if self.flag_locals.contains(&l))
            }
            HirExprKind::Block(b) => {
                b.trailing.is_none()
                    && b.stmts.len() == 1
                    && matches!(&b.stmts[0], HirStmt::Expr(x) if self.release_if(x))
            }
            _ => false,
        }
    }

    /// Parts moved into a new value: it is owned unless a part is not.
    fn exprs(&mut self, es: impl IntoIterator<Item = &'a HirExpr>, mut st: State) -> (State, Val) {
        let mut val = Val::Owned;
        let mut any = false;
        for x in es {
            let affine = self.c.shape(&x.ty).is_some();
            let mode = if affine { Mode::Move } else { Mode::Use };
            let (s, v) = self.expr(x, st, mode);
            st = s;
            if affine && v != Val::Plain {
                val = if any { combine(val, v) } else { v };
                any = true;
            }
        }
        (st, val)
    }

    #[allow(clippy::cognitive_complexity)] // one arm per expression kind
    fn expr(&mut self, e: &'a HirExpr, st: State, mode: Mode) -> (State, Val) {
        if st.is_none() {
            return (None, Val::Plain);
        }
        if !e.span.is_dummy() {
            self.last = e.span;
        }
        let affine = self.c.shape(&e.ty).is_some();
        let mode = if affine { mode } else { Mode::Use };
        if let Some((r, path)) = self.place(e) {
            let mut st = st;
            // A place's own reads: `f.fd` reads `f`.
            if mode == Mode::Move {
                let owned = self.roots[r].owned || self.roots[r].param.is_some();
                self.move_at(r, &path, e, &mut st, false);
                return (st, if owned { Val::Owned } else { Val::Alias });
            }
            // Plain data of a closed value (`f.path`) can still be read.
            let moved = self
                .slots_at(r, &path)
                .iter()
                .any(|&x| self.why.get(&x).is_some_and(|w| !w.1) && get(&st, x) & MOVED != 0);
            if affine || moved {
                self.use_at(r, &path, e.span, &st, false);
            }
            return (st, if affine { Val::Alias } else { Val::Plain });
        }
        match &e.kind {
            HirExprKind::Local(l) => {
                // A temporary the lowering made around a value.
                let _ = l;
                (
                    st,
                    if affine {
                        self.origin(e, 0)
                    } else {
                        Val::Plain
                    },
                )
            }
            HirExprKind::Block(b) => self.block(b, st, mode),
            HirExprKind::Call { callee, args } => self.call(*callee, args, e, st),
            HirExprKind::CallIndirect { callee, args, .. } => {
                let (st, _) = self.expr(callee, st, Mode::Use);
                let st = args.iter().fold(st, |s, a| self.expr(a, s, Mode::Use).0);
                (st, if affine { Val::Owned } else { Val::Plain })
            }
            HirExprKind::CallClosure { closure, args, .. } => {
                let (st, _) = self.expr(closure, st, Mode::Use);
                let st = args.iter().fold(st, |s, a| self.expr(a, s, Mode::Use).0);
                (st, if affine { Val::Owned } else { Val::Plain })
            }
            HirExprKind::StructInit { fields, .. } | HirExprKind::Tuple { fields } => {
                let (st, v) = self.exprs(fields, st);
                (st, if affine { v } else { Val::Plain })
            }
            HirExprKind::EnumVariantInit { args, .. } => {
                let (st, v) = self.exprs(args, st);
                (st, if affine { v } else { Val::Plain })
            }
            HirExprKind::ArrayInit { elements, .. } => {
                let (st, v) = self.exprs(elements, st);
                (st, if affine { v } else { Val::Plain })
            }
            HirExprKind::ArrayRepeat { value, .. } => {
                let (st, v) = self.exprs(std::iter::once(&**value), st);
                (st, if affine { v } else { Val::Plain })
            }
            HirExprKind::SumWrap { value, .. } => match value {
                Some(v) => {
                    let (st, val) = self.expr(v, st, mode);
                    (st, if affine { val } else { Val::Plain })
                }
                None => (st, Val::Plain),
            },
            HirExprKind::WithField { target, value, .. } => {
                // An update in place: the target keeps its owner.
                let (st, a) = match self.place(target) {
                    Some((r, _)) => {
                        let owned = self.roots[r].owned;
                        let (st, _) = self.expr(target, st, Mode::Use);
                        (st, if owned { Val::Owned } else { Val::Alias })
                    }
                    None => self.expr(target, st, mode),
                };
                let (st, b) = self.exprs(std::iter::once(&**value), st);
                (st, if affine { combine(a, b) } else { Val::Plain })
            }
            HirExprKind::FieldRead { target, .. } | HirExprKind::TupleField { target, .. } => {
                // A part of a value that is not a followed place: a call's
                // result, a `ref` call's parts.
                if let HirExprKind::TupleField { index, .. } = &e.kind {
                    if let Some(v) = self.ref_part(target, *index, st.clone()) {
                        return v;
                    }
                }
                let (st, v) = self.expr(target, st, mode);
                let v = match (affine, v) {
                    (false, _) => Val::Plain,
                    (true, Val::Plain) => self.origin(e, 0),
                    (true, v) => v,
                };
                (st, v)
            }
            HirExprKind::Cast { operand, .. } => {
                let (st, v) = self.expr(operand, st, mode);
                (st, if affine { v } else { Val::Plain })
            }
            HirExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                let (st, _) = self.expr(cond, st, Mode::Use);
                // A cancellation point leaves through the cleanups.
                let cancel = self.cancel_check(cond);
                let (a, va) = if cancel {
                    let saved = self.quiet;
                    self.quiet = true;
                    let r = self.block(then_block, st.clone(), mode);
                    self.quiet = saved;
                    (None, r.1)
                } else {
                    self.block(then_block, st.clone(), mode)
                };
                // A coroutine's `if true { '#coro: { body } } else { undef }`:
                // the `else` is never taken.
                let (b, vb) = match else_branch {
                    _ if matches!(cond.kind, HirExprKind::Const(ConstVal::Bool(true))) => {
                        (None, Val::Plain)
                    }
                    Some(x) => self.expr(x, st, mode),
                    None => (st, Val::Plain),
                };
                let v = match (a.is_some(), b.is_some()) {
                    (true, true) => combine(va, vb),
                    (true, false) => va,
                    (false, true) => vb,
                    _ => Val::Plain,
                };
                (join(a, b), v)
            }
            HirExprKind::Match { scrutinee, arms } => self.matched(scrutinee, arms, st, mode),
            HirExprKind::Loop { label, body, .. } => self.looped(label.as_deref(), None, body, st),
            HirExprKind::While { label, cond, body } => {
                self.looped(label.as_deref(), Some(cond), body, st)
            }
            HirExprKind::ClosureValue { fn_id, env, .. } => {
                (self.captures(*fn_id, env, st), Val::Plain)
            }
            HirExprKind::CoroNew { body_fn, args } => {
                let mut st = st;
                for a in args {
                    st = self.captures(*body_fn, a, st);
                }
                (st, Val::Plain)
            }
            HirExprKind::BufSet {
                buf, index, value, ..
            } => {
                let (st, _) = self.expr(buf, st, Mode::Use);
                let (st, _) = self.expr(index, st, Mode::Use);
                let st = self.exprs(std::iter::once(&**value), st).0;
                (st, Val::Plain)
            }
            // Its address taken: raw access from here on, not followed.
            HirExprKind::AddrOf { place } => {
                let mut st = st;
                if let Some((r, path)) = self.place(place) {
                    for x in self.slots_at(r, &path) {
                        set(&mut st, x, 0);
                    }
                    let flag = self.root_local(r).and_then(|l| self.flags.get(&l).copied());
                    if let (true, Some(flag)) = (self.report, flag) {
                        self.edits.clears.entry(addr(e)).or_default().push(flag);
                    }
                    return (st, Val::Plain);
                }
                (self.expr(place, st, Mode::Use).0, Val::Plain)
            }
            HirExprKind::Suspend { value, .. } => {
                let st = self.exprs(std::iter::once(&**value), st).0;
                (st, Val::Plain)
            }
            _ => {
                let mut st = st;
                for c in children(e) {
                    st = self.expr(c, st, Mode::Use).0;
                }
                (
                    st,
                    if affine {
                        self.origin(e, 0)
                    } else {
                        Val::Plain
                    },
                )
            }
        }
    }

    /// Where the value of `e` comes from, without walking it: a value
    /// through the temporaries the lowering makes (a `throws` carrier).
    fn origin(&self, e: &HirExpr, depth: u32) -> Val {
        if depth > 16 {
            return Val::Alias;
        }
        match &e.kind {
            HirExprKind::Local(l) => {
                if self.root_of.contains_key(l) {
                    return Val::Alias;
                }
                match self.inits.get(l) {
                    Some(i) if self.local_name(*l).is_none() => self.origin(i, depth + 1),
                    _ => Val::Alias,
                }
            }
            HirExprKind::Call { callee, .. } => {
                if self.c.summary(*callee).is_some_and(|s| s.alias) {
                    Val::Alias
                } else {
                    Val::Owned
                }
            }
            HirExprKind::CallIndirect { .. }
            | HirExprKind::CallClosure { .. }
            | HirExprKind::StructInit { .. }
            | HirExprKind::Tuple { .. }
            | HirExprKind::EnumVariantInit { .. } => Val::Owned,
            HirExprKind::TupleField { target, index, .. } => {
                let refs = match &target.kind {
                    HirExprKind::Call { callee, .. } => self
                        .c
                        .fns
                        .get(callee)
                        .is_some_and(|f| f.params.iter().any(|p| p.by_ref)),
                    _ => false,
                };
                if refs && *index > 0 {
                    Val::Alias
                } else {
                    self.origin(target, depth + 1)
                }
            }
            HirExprKind::FieldRead { target, .. }
            | HirExprKind::UnionRead { target, .. }
            | HirExprKind::Cast {
                operand: target, ..
            }
            | HirExprKind::SumWrap {
                value: Some(target),
                ..
            } => self.origin(target, depth + 1),
            HirExprKind::Block(b) => b
                .trailing
                .as_ref()
                .map_or(Val::Plain, |t| self.origin(t, depth + 1)),
            _ => Val::Alias,
        }
    }

    /// `e` asks whether the task was cancelled (an `await`'s lowering).
    fn cancel_check(&self, e: &HirExpr) -> bool {
        let mut hit = false;
        crate::each_expr(e, &mut |x| {
            if let HirExprKind::Call { callee, .. } = &x.kind {
                hit |= self.c.fns.get(callee).is_some_and(|f| {
                    matches!(
                        f.name.as_str(),
                        "rok::core::task::cancel_point" | "rok::core::task::cancel_left"
                    )
                });
            }
        });
        hit
    }

    /// Part `index` of a call with `ref` parameters: the result (0) or a
    /// `ref` argument written back.
    fn ref_part(&mut self, target: &'a HirExpr, index: u32, st: State) -> Option<(State, Val)> {
        let HirExprKind::Call { callee, .. } = &target.kind else {
            return None;
        };
        let f = self.c.fns.get(callee)?;
        if !f.params.iter().any(|p| p.by_ref) {
            return None;
        }
        let (st, _) = self.expr(target, st, Mode::Use);
        Some((
            st,
            if index == 0 {
                self.ref_result
            } else {
                Val::Alias
            },
        ))
    }

    /// Captured values: an owned one moves in; a borrowed one is lent
    /// unless the body keeps it.
    fn captures(&mut self, body: FunctionId, env: &'a HirExpr, mut st: State) -> State {
        let HirExprKind::Tuple { fields } = &env.kind else {
            return self.expr(env, st, Mode::Use).0;
        };
        let keeps = self.c.summary(body).map(|s| s.keeps).unwrap_or_default();
        for (k, x) in fields.iter().enumerate() {
            let k = u32::try_from(k).unwrap_or(u32::MAX);
            let affine = self.c.shape(&x.ty).is_some();
            if !affine {
                st = self.expr(x, st, Mode::Use).0;
                continue;
            }
            let owned_place = self.place(x).is_some_and(|(r, _)| self.roots[r].owned);
            let fresh = self.place(x).is_none();
            let mode = if owned_place || keeps.contains(&(SLOT + k)) {
                Mode::Move
            } else {
                Mode::Use
            };
            let (s, v) = self.expr(x, st, mode);
            st = s;
            if self.report
                && (owned_place || (fresh && v == Val::Owned) || keeps.contains(&(SLOT + k)))
            {
                self.c.owned_slots.borrow_mut().insert((body, k));
            }
        }
        st
    }

    fn call(
        &mut self,
        callee: FunctionId,
        args: &'a [HirExpr],
        e: &'a HirExpr,
        mut st: State,
    ) -> (State, Val) {
        let f = self.c.fns.get(&callee).copied();
        let sum = self.c.summary(callee);
        let method = f.map_or("", |f| method_name(&f.name));
        let own_method = f.is_some_and(|f| {
            f.params.first().is_some_and(|p| p.name == "self")
                && args.first().is_some_and(|a| a.ty == f.params[0].ty)
        });
        // `marker::share(x)`: a second handle nobody follows.
        if f.is_some_and(|f| f.name.split("__mono__").next() == Some(SHARE)) {
            for a in args {
                st = self.expr(a, st, Mode::Use).0;
            }
            return (st, Val::Alias);
        }
        if let Some(f) =
            f.filter(|f| SWAPS.contains(&f.name.split("__mono__").next().unwrap_or("")))
        {
            let owned = args
                .first()
                .and_then(|a| self.place(a))
                .is_some_and(|(r, _)| self.roots[r].owned);
            let _ = f;
            for (i, a) in args.iter().enumerate() {
                let mode = if i == 0 { Mode::Use } else { Mode::Move };
                st = self.expr(a, st, mode).0;
            }
            self.ref_result = if owned { Val::Owned } else { Val::Alias };
            return (st, Val::Alias);
        }
        // `using`'s release, or the compiler's.
        if method == "drop" && f.is_some_and(|f| f.params.len() == 1) {
            if let Some(a) = args.first() {
                self.dropped(a, &mut st);
            }
            return (st, Val::Plain);
        }
        for (i, a) in args.iter().enumerate() {
            let p = f.and_then(|f| f.params.get(i));
            let by_ref = p.is_some_and(|p| p.by_ref);
            let i = u32::try_from(i).unwrap_or(u32::MAX);
            let closer = i == 0 && own_method && CLOSERS.contains(&method);
            let iter = method == "into_iter";
            let into = i == 0 && own_method && method.starts_with("into_") && !iter;
            let keeps = !iter && sum.as_ref().is_some_and(|s| s.keeps.contains(&i));
            let affine = self.c.shape(&a.ty).is_some();
            if !affine || by_ref || !(closer || into || keeps) {
                st = self.expr(a, st, Mode::Use).0;
                continue;
            }
            if closer {
                if let Some((r, path)) = self.place(a) {
                    self.move_at(r, &path, a, &mut st, true);
                    continue;
                }
            }
            st = self.expr(a, st, Mode::Move).0;
        }
        let ret = f.map_or(&e.ty, |f| &f.ret_ty);
        let affine = self.c.shape(&e.ty).is_some() || self.c.shape(ret).is_some();
        let v = if !affine {
            Val::Plain
        } else if method == "into_iter" || sum.is_some_and(|s| s.alias) {
            Val::Alias
        } else {
            Val::Owned
        };
        // A call with `ref` parameters returns `(result, ref args..)`: the
        // tuple is not one value; its part 0 is the result.
        if f.is_some_and(|f| f.params.iter().any(|p| p.by_ref)) {
            self.ref_result = v;
            return (st, Val::Alias);
        }
        (st, v)
    }

    fn matched(
        &mut self,
        scrutinee: &'a HirExpr,
        arms: &'a [crate::HirMatchArm],
        st: State,
        mode: Mode,
    ) -> (State, Val) {
        let mut binds = false;
        for a in arms {
            pattern_bindings(&a.pat, &mut |_, ty| binds |= self.c.shape(ty).is_some());
        }
        let at = self
            .place(scrutinee)
            .filter(|(r, p)| self.slots_at(*r, p).len() == 1);
        let mode_s = if binds && at.is_none() {
            Mode::Move
        } else {
            Mode::Use
        };
        let (st, sv) = self.expr(scrutinee, st, mode_s);
        let mut out: State = None;
        let mut val: Option<Val> = None;
        let members = match &scrutinee.ty {
            HirTy::Sum(ts) => Some(ts.clone()),
            _ => None,
        };
        let mut taken: HashSet<u32> = HashSet::new();
        for a in arms {
            let mut s = st.clone();
            // The members this arm may see; none of them a resource: the
            // place holds nothing to release there.
            if let (Some(ms), Some((r, p))) = (&members, &at) {
                let here: Vec<u32> = match &a.pat {
                    HirPattern::SumVariant { variant_index, .. }
                    | HirPattern::SumMember { variant_index, .. } => vec![*variant_index],
                    HirPattern::Wildcard | HirPattern::Binding { .. } => (0..ms.len())
                        .filter_map(|i| u32::try_from(i).ok())
                        .filter(|i| !taken.contains(i))
                        .collect(),
                    _ => (0..ms.len())
                        .filter_map(|i| u32::try_from(i).ok())
                        .collect(),
                };
                if a.guard.is_none() {
                    if let HirPattern::SumVariant { variant_index, .. }
                    | HirPattern::SumMember { variant_index, .. } = &a.pat
                    {
                        taken.insert(*variant_index);
                    }
                }
                let empty = here
                    .iter()
                    .all(|&i| ms.get(i as usize).is_none_or(|t| self.c.shape(t).is_none()));
                if empty {
                    for x in self.slots_at(*r, p) {
                        if get(&s, x) & LIVE != 0 {
                            set(&mut s, x, 0);
                        }
                    }
                }
            }
            self.scopes.push(Vec::new());
            let mut locals = Vec::new();
            pattern_bindings(&a.pat, &mut |l, ty| locals.push((l, ty.clone())));
            for (l, ty) in locals {
                if let Some(p) = &at {
                    if self.c.shape(&ty).is_some() {
                        self.alias_of.insert(l, p.clone());
                    }
                    continue;
                }
                if sv == Val::Owned {
                    self.bind(l, &ty, true, None, scrutinee.span, &mut s);
                } else {
                    self.unbind(l, &mut s);
                }
            }
            if let Some(g) = &a.guard {
                s = self.expr(g, s, Mode::Use).0;
            }
            let (s, v) = self.expr(&a.body, s, mode);
            let depth = self.scopes.len() - 1;
            if s.is_some() {
                self.leave(&s, depth, a.body.span);
            }
            let scope = self.scopes.pop().unwrap_or_default();
            let mut s = s;
            for r in scope {
                let root = &self.roots[r];
                for i in 0..root.leaves.len() {
                    set(&mut s, root.base + i, 0);
                }
            }
            if s.is_some() {
                val = Some(val.map_or(v, |x| combine(x, v)));
            }
            out = join(out, s);
        }
        (out, val.unwrap_or(Val::Plain))
    }

    fn looped(
        &mut self,
        label: Option<&str>,
        cond: Option<&'a HirExpr>,
        body: &'a HirBlock,
        st: State,
    ) -> (State, Val) {
        let mut head = st.clone();
        let entry = st;
        let mut exit: State = None;
        let mut breaks: State = None;
        let mut lv: Option<Val> = None;
        for _ in 0..8 {
            self.targets.push(Target {
                label: label.map(str::to_owned),
                is_loop: true,
                depth: self.scopes.len(),
                breaks: None,
                conts: None,
                val: None,
            });
            let mut s = head.clone();
            if let Some(c) = cond {
                s = self.expr(c, s, Mode::Use).0;
                exit = join(exit.take(), s.clone());
            }
            let (end, _) = self.block(body, s, Mode::Use);
            let t = self.targets.pop().expect("the loop's target");
            breaks = join(breaks.take(), t.breaks);
            if let Some(v) = t.val {
                lv = Some(lv.map_or(v, |x| combine(x, v)));
            }
            let next = join(entry.clone(), join(end, t.conts));
            if next == head {
                break;
            }
            head = next;
        }
        let out = if cond.is_some() {
            join(exit, breaks)
        } else {
            breaks
        };
        (out, lv.unwrap_or(Val::Plain))
    }
}

/// The local a place expression starts from.
fn base_local(e: &HirExpr) -> Option<LocalId> {
    match &e.kind {
        HirExprKind::Local(l) => Some(*l),
        HirExprKind::FieldRead { target, .. } | HirExprKind::TupleField { target, .. } => {
            base_local(target)
        }
        _ => None,
    }
}

/// `return` of an error carrier: a `throw` or a `try` passing one on.
fn is_throw(v: &HirExpr) -> bool {
    let HirExprKind::Tuple { fields } = &v.kind else {
        return false;
    };
    matches!(v.ty, HirTy::Throws { .. })
        && matches!(
            fields.first().map(|f| &f.kind),
            Some(HirExprKind::Const(ConstVal::Bool(true)))
        )
}

/// The locals a pattern binds, with their types.
fn pattern_bindings(p: &HirPattern, f: &mut impl FnMut(LocalId, &HirTy)) {
    match p {
        HirPattern::Binding { local, ty } => f(*local, ty),
        HirPattern::At { local, ty, sub, .. } => {
            f(*local, ty);
            pattern_bindings(sub, f);
        }
        HirPattern::Or(ps) | HirPattern::Tuple(ps) => {
            for q in ps {
                pattern_bindings(q, f);
            }
        }
        HirPattern::Struct { fields, .. } => {
            for (_, q) in fields {
                pattern_bindings(q, f);
            }
        }
        HirPattern::EnumVariant { args, .. } => {
            for q in args {
                pattern_bindings(q, f);
            }
        }
        HirPattern::SumVariant {
            binder: Some(l),
            payload_ty: Some(ty),
            ..
        } => f(*l, ty),
        HirPattern::SumMember { sub, .. } => pattern_bindings(sub, f),
        _ => {}
    }
}

/// The direct subexpressions of `e`.
fn children(e: &HirExpr) -> Vec<&HirExpr> {
    let mut out = Vec::new();
    super::send_check::each_child(e, &mut |c| {
        if let super::send_check::Child::E(x) = c {
            out.push(x);
        }
    });
    out
}

/// A function's method name: `File.close`, `Struct7__close__by_Close`,
/// `m::Sender__close__mono__..`, `m::__impl1a2b_drop`.
fn method_name(name: &str) -> &str {
    let n = name.split("__mono__").next().unwrap_or(name);
    let n = n.split("__by_").next().unwrap_or(n);
    let n = n.rsplit("::").next().unwrap_or(n);
    if let Some(rest) = n.strip_prefix("__impl") {
        return rest.split_once('_').map_or(rest, |(_, m)| m);
    }
    let n = n.rsplit('.').next().unwrap_or(n);
    n.rsplit("__").next().unwrap_or(n)
}

/// The parents of closure and coroutine bodies.
fn parents(all: &[&HirFunction]) -> HashMap<FunctionId, FunctionId> {
    let mut out = HashMap::new();
    for f in all {
        let Some(body) = &f.body else { continue };
        crate::each_block(body, &mut |e| {
            if let HirExprKind::ClosureValue { fn_id: id, .. }
            | HirExprKind::CoroNew { body_fn: id, .. } = &e.kind
            {
                if *id != f.id {
                    out.entry(*id).or_insert(f.id);
                }
            }
        });
    }
    out
}

/// Check every body; report each use after a move and each value whose
/// close can fail left open, and fix up the release flags. `stored`: the
/// summaries of prebuilt libraries; returned: this build's, when
/// `export`.
pub(super) fn check(
    env: &Env<'_>,
    functions: &mut [HirFunction],
    stored: &[(String, String)],
    export: bool,
) -> Vec<(String, String)> {
    if env.names.lang_item("drop_method").is_empty() {
        return Vec::new();
    }
    let no_subst = HashMap::new();
    let ctx = LowerCtx::new(env, &no_subst, None, Span::DUMMY);
    let (out, edits) = {
        let extras = env.sink.extras.borrow();
        let all: Vec<&HirFunction> = functions.iter().chain(extras.iter()).collect();
        let by_name: HashMap<&str, &str> = stored
            .iter()
            .filter_map(|(n, s)| Some((n.strip_prefix(KEY)?, s.as_str())))
            .collect();
        let mut sums = HashMap::new();
        for f in all.iter().filter(|f| f.body.is_none()) {
            if let Some(s) = by_name
                .get(f.name.as_str())
                .and_then(|t| Summary::decode(t))
            {
                sums.insert(f.id, s);
            }
        }
        let c = Checker {
            env,
            ctx: &ctx,
            fns: all.iter().map(|f| (f.id, *f)).collect(),
            sums: RefCell::new(sums),
            owned_slots: RefCell::default(),
            memo: RefCell::default(),
        };
        // The bodies holding an affine value.
        let walked: Vec<&HirFunction> = all
            .iter()
            .copied()
            .filter(|f| {
                f.body.is_some()
                    && (f.params.iter().any(|p| c.shape(&p.ty).is_some())
                        || f.locals.iter().any(|l| c.shape(&l.ty).is_some()))
            })
            .collect();
        // Summaries to a fixpoint: every parameter borrowed.
        for _ in 0..12 {
            let mut changed = false;
            for f in &walked {
                let mut w = Walk::new(&c, f, false);
                run(&mut w);
                let mut s = w.out;
                if let Some(old) = c.summary(f.id) {
                    s.keeps.extend(old.keeps);
                    s.alias |= old.alias;
                }
                if c.summary(f.id).as_ref() != Some(&s) {
                    c.sums.borrow_mut().insert(f.id, s);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // The final walk, creators before the bodies they make.
        let parent = parents(&all);
        let depth = |mut id: FunctionId| {
            let mut d = 0;
            while let Some(p) = parent.get(&id) {
                id = *p;
                d += 1;
                if d > 64 {
                    break;
                }
            }
            d
        };
        let mut order = walked.clone();
        order.sort_by_key(|f| depth(f.id));
        let mut diags = Vec::new();
        let mut edits: HashMap<FunctionId, Edits> = HashMap::new();
        for f in &order {
            let mut w = Walk::new(&c, f, true);
            let keeps = c.summary(f.id).map(|s| s.keeps).unwrap_or_default();
            w.owned_params = keeps.into_iter().collect();
            for k in c
                .owned_slots
                .borrow()
                .iter()
                .filter(|(b, _)| *b == f.id)
                .map(|(_, k)| *k)
            {
                w.owned_params.insert(SLOT + k);
            }
            run(&mut w);
            diags.extend(w.errors);
            edits.insert(f.id, w.edits);
        }
        let out = if export {
            let sums = c.sums.borrow();
            walked
                .iter()
                .filter(|f| !f.name.contains(rokc_ir_common::MONO_MARK))
                .filter_map(|f| {
                    let s = sums.get(&f.id).filter(|s| !s.keeps.is_empty() || s.alias)?;
                    Some((format!("{KEY}{}", f.name), s.encode()))
                })
                .collect()
        } else {
            Vec::new()
        };
        env.sink.diagnostics.borrow_mut().extend(diags);
        (out, edits)
    };
    for f in functions.iter_mut() {
        if let Some(e) = edits.get(&f.id) {
            apply(f, e);
        }
    }
    for f in env.sink.extras.borrow_mut().iter_mut() {
        if let Some(e) = edits.get(&f.id) {
            apply(f, e);
        }
    }
    out
}

/// Walk `w`'s function from entry to its exits.
fn run(w: &mut Walk<'_, '_, '_>) {
    let f = w.f;
    let Some(body) = &f.body else { return };
    let mut st: State = Some(Vec::new());
    for (i, p) in f.params.iter().enumerate() {
        if p.by_ref || p.name == "__env" {
            continue;
        }
        let i = u32::try_from(i).unwrap_or(u32::MAX);
        let owned = w.owned_params.contains(&i);
        let ty = p.ty.clone();
        if w.bind(p.local, &ty, owned, Some(i), f.span, &mut st)
            .is_some()
        {
            w.bound.insert(p.local, owned);
        }
    }
    let (st, val) = w.block(body, st, Mode::Move);
    if val == Val::Alias {
        w.out.alias = true;
    }
    if st.is_some() {
        let at = body.trailing.as_ref().map_or(w.last, |t| t.span);
        w.leave(&st, 0, at);
    }
}

fn apply(f: &mut HirFunction, e: &Edits) {
    if e.unset.is_empty() && e.clears.is_empty() {
        return;
    }
    let Some(body) = &mut f.body else { return };
    // Pre-order: a node is rewritten before its children, which live on
    // the heap and keep the addresses the walk saw.
    crate::each_block_mut(body, &mut |x| {
        let a = std::ptr::from_ref::<HirExpr>(x) as usize;
        if e.unset.contains(&a) {
            x.kind = HirExprKind::Const(ConstVal::Bool(false));
            return;
        }
        let Some(flags) = e.clears.get(&a) else {
            return;
        };
        let span = x.span;
        let ty = x.ty.clone();
        let inner = std::mem::replace(x, flag_value(false, span));
        let stmts = flags
            .iter()
            .map(|&f| HirStmt::Assign {
                local: f,
                value: flag_value(false, Span::DUMMY),
            })
            .collect();
        *x = HirExpr {
            kind: HirExprKind::Block(Box::new(HirBlock {
                stmts,
                trailing: Some(inner),
                label: None,
            })),
            ty,
            span,
        };
    });
}
