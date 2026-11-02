//! `task::spawn` moves a future to another worker (stdlib 2.18.1), so its
//! coroutine frame must be `Send`. Future types are shared wrappers
//! (`AsyncFuture<T>` is `Send` when `T` is) and say nothing about the
//! frame, so this pass checks frames:
//!
//! 1. A frame is not `Send` when a capture, or a local or temporary held
//!    across an `await` (`f(cell, await x)`), is not. "Held across" is read
//!    off the HIR in evaluation order; loop bodies are walked twice.
//! 2. A whole-program flow tracks which such coroutines each value may
//!    hold: through locals, parameters, returns and yields, fields, buffers
//!    a callee stores into, captures, calls and other frames.
//! 3. No argument of `task::spawn`, `thread::spawn`, `Builder::spawn`,
//!    `Scope::spawn`, `task::spawn_blocking` or `par` may hold one. A frame
//!    whose captures are `Send` but not `Sync` may still be a task:
//!    `task::spawn` hands the future over, and the frame takes copies of
//!    them (`Context::prime`); a `[ref x]` cell stays shared.
//!
//! A type with no `fn` / closure value inside, or with an explicit
//! `unsafe impl Send` (`Task<T>`: the future stays with its worker), holds
//! none.
// The flow is context-insensitive, and a store through an alias the callee
// did not get as a parameter is missed: two `List<AsyncFuture<i32>>` filled
// by `push` look like one. Exact would be per-coroutine future types
// (4.1.11) checked by the `Send` bound itself.

use rokc_diagnostic::Code;
use std::collections::{BTreeSet, HashMap, HashSet};

use rokc_ir_common::{FunctionId, LocalId};

use super::{Env, LowerCtx};
use crate::{HirBlock, HirExpr, HirExprKind, HirFunction, HirPattern, HirStmt, HirTy};

/// Calls whose arguments must not hold a non-`Send` frame.
const SINKS: &[&str] = &[
    "rok::core::task::spawn",
    "rok::core::task::spawn_plain",
    "rok::core::task::spawn_blocking",
    "rok::std::thread::spawn",
    "rok::std::thread::Builder__spawn",
    "rok::core::par::Scope__spawn",
    // Their items go to workers (a future among them is run there).
    "rok::core::par::for_each",
    "rok::core::par::map",
    "rok::core::par::filter",
];

/// The coroutine wrappers: their `Send` impl does not see the frame.
const WRAPPERS: &[&str] = &[
    "AsyncFuture",
    "TryAsyncFuture",
    "AsyncStream",
    "TryAsyncStream",
    "GenIter",
    "TryGenIter",
];

fn sink_of(name: &str) -> Option<&'static str> {
    SINKS.iter().copied().find(|s| {
        name.strip_prefix(s)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("__"))
    })
}

pub(super) fn check(env: &Env<'_>, functions: &[HirFunction]) {
    let extras = env.sink.extras.borrow();
    let all: HashMap<FunctionId, &HirFunction> = functions
        .iter()
        .chain(extras.iter())
        .map(|f| (f.id, f))
        .collect();
    if !all.values().any(|f| sink_of(&f.name).is_some()) {
        return;
    }
    let send = env.names.lang_item("send_trait").to_owned();
    if send.is_empty() {
        return;
    }
    let sync = Some(env.names.lang_item("sync_trait").to_owned()).filter(|s| !s.is_empty());
    let no_subst = HashMap::new();
    let ctx = LowerCtx::new(env, &no_subst, None, rokc_diagnostic::Span::DUMMY);

    // 1. Each coroutine's own frame.
    let mut bad: HashMap<FunctionId, String> = HashMap::new();
    // Frames that capture a `Send` but not `Sync` value: fine inside
    // one task, not as a task of their own (the spawner keeps its copy).
    let mut shared: HashMap<FunctionId, String> = HashMap::new();
    // Frames with a `[ref x]` capture: never a task of their own
    // (`task::spawn` copies the other captures).
    let mut cells: HashMap<FunctionId, String> = HashMap::new();
    // Of those, frames whose capture is a lock guard.
    let mut guards: HashSet<FunctionId> = HashSet::new();
    let mut held: HashMap<FunctionId, Vec<LocalId>> = HashMap::new();
    let mut ids: Vec<FunctionId> = all.keys().copied().collect();
    ids.sort();
    for &id in &ids {
        let f = all[&id];
        if !(f.is_async || f.is_generator) {
            continue;
        }
        let (why, why_shared, why_cell, h) = frame(&ctx, &send, sync.as_deref(), f);
        if let Some(w) = why {
            bad.insert(id, w);
        }
        if let Some(w) = why_shared {
            shared.insert(id, w);
        }
        if let Some((w, guard)) = why_cell {
            cells.insert(id, w);
            if guard {
                guards.insert(id);
            }
        }
        held.insert(id, h);
    }
    if bad.is_empty() && shared.is_empty() {
        return;
    }

    // 2. Where they flow.
    let mut flow = Flow {
        ctx: &ctx,
        send: &send,
        all: &all,
        bad: &bad,
        shared: &shared,
        held: &held,
        nodes: HashMap::new(),
        readers: HashMap::new(),
        dirty: ids.iter().copied().collect(),
        cut: HashMap::new(),
        holds: HashMap::new(),
        cur: FunctionId(0),
        ref_params: Vec::new(),
        breaks: Vec::new(),
        sinks: Vec::new(),
    };
    let mut sinks = HashMap::new();
    while let Some(id) = flow.dirty.pop_first() {
        flow.function(all[&id]);
        sinks.insert(id, std::mem::take(&mut flow.sinks));
    }

    // 3. The sinks.
    let mut sinks: Vec<_> = sinks.into_values().flatten().collect();
    sinks.sort_by_key(|(span, _, _)| (span.file, span.start));
    let mut seen = HashSet::new();
    for (span, sink, items) in sinks {
        // `task::spawn` copies what the future captured, but a cell stays
        // shared; other sinks run the future with its captures as they are.
        let spawn = sink == SINKS[0] || sink == SINKS[1];
        let found = items
            .iter()
            .find(|i| matches!(i, Item::Bad(_)))
            .or_else(|| {
                items.iter().find(|i| match i {
                    Item::Shared(c) => !spawn || cells.contains_key(c),
                    _ => false,
                })
            });
        // A guard is wrong in any task, `spawn_local` too.
        let guard = matches!(found, Some(Item::Shared(c)) if spawn && guards.contains(c));
        let why = match found {
            Some(Item::Bad(c)) => bad[c].clone(),
            Some(Item::Shared(c)) if spawn => cells[c].clone(),
            Some(Item::Shared(c)) if cells.contains_key(c) || !all[c].is_async => shared[c].clone(),
            Some(Item::Shared(c)) => format!(
                "{}; make each a task instead (`task::spawn` gives it its own copies)",
                shared[c]
            ),
            _ => continue,
        };
        if !seen.insert((span.file, span.start)) {
            continue;
        }
        ctx.emit_error_at(
            Code::THREAD_SAFETY,
            span,
            if guard {
                format!("`task::spawn` cannot take a guard: {why}")
            } else if spawn {
                format!(
                    "`task::spawn` needs a `Send` future: {why}, and the task may resume on \
                     another worker; use `task::spawn_local`"
                )
            } else {
                let short = sink
                    .trim_start_matches("rok::core::")
                    .trim_start_matches("rok::std::")
                    .replace("__", "::");
                format!("`{short}` would move a future to another thread: {why}")
            },
        );
    }
}

/// What [`frame`] finds; the third is `(why, the capture is a guard)`.
type FrameWhy = (
    Option<String>,
    Option<String>,
    Option<(String, bool)>,
    Vec<LocalId>,
);

/// Why coroutine `f`'s own frame is not `Send` (`None`: it is), why it
/// cannot run with its captures as they are on another thread (one is not
/// `Sync`), why not even with copies (a `[ref x]` cell or a guard), and
/// the locals its frame holds (captures and locals live across a suspend).
fn frame(ctx: &LowerCtx<'_>, send: &str, sync: Option<&str>, f: &HirFunction) -> FrameWhy {
    let name = |l: LocalId| {
        f.locals
            .iter()
            .find(|x| x.id == l)
            .and_then(|x| x.name.clone())
            .unwrap_or_else(|| "a temporary".to_owned())
    };
    let ty_of = |l: LocalId| f.locals.iter().find(|x| x.id == l).map(|x| &x.ty);
    // A `[ref x]` cell: written by both sides, never `Sync` (spec 4.6.9).
    let cell = |l: LocalId| f.locals.iter().any(|x| x.id == l && x.boxed);
    let lacks = |t: &HirTy, k: &str| {
        !matches!(
            t,
            HirTy::Unknown | HirTy::Prim(_) | HirTy::Unit | HirTy::Never
        ) && !ctx.has_trait(t, k)
    };
    let non_send = |t: &HirTy| lacks(t, send);
    let non_sync = |t: &HirTy| sync.is_some_and(|k| lacks(t, k));
    let env_params: HashSet<LocalId> = f
        .params
        .iter()
        .filter(|p| !p.name.starts_with('#'))
        .map(|p| p.local)
        .collect();
    let mut w = Walk {
        non_send_local: &|l| ty_of(l).is_some_and(non_send),
        non_sync_local: &|l| cell(l) || ty_of(l).is_some_and(non_sync),
        cell_local: &|l| cell(l) || ty_of(l).is_some_and(|t| is_guard(ctx, t)),
        non_send_ty: &non_send,
        env_params: &env_params,
        declared: Vec::new(),
        crossed: HashSet::new(),
        held: env_params.clone(),
        suspends: 0,
        found: None,
        shared: None,
        cell: None,
    };
    let Some(body) = f.body.as_ref() else {
        return (None, None, None, Vec::new());
    };
    w.block(body);
    let mut held: Vec<LocalId> = w.held.into_iter().collect();
    held.sort();
    // The component that makes a type not `key` (`auto_trait_cause`).
    let cause = |t: Option<&HirTy>, key: &str| {
        t.map(|t| ctx.auto_trait_cause(t, key, 0))
            .unwrap_or_default()
    };
    let why = w.found.map(|found| match found {
        Found::Capture(l) => format!(
            "it captures `{}: {}`, which is not `Send`{}",
            name(l),
            ty_of(l).map(|t| ctx.ty_name(t)).unwrap_or_default(),
            cause(ty_of(l), send)
        ),
        Found::Held(l) => format!(
            "it holds `{}: {}`, which is not `Send`{}, across an `await`",
            name(l),
            ty_of(l).map(|t| ctx.ty_name(t)).unwrap_or_default(),
            cause(ty_of(l), send)
        ),
        Found::Temp(t) => format!(
            "it holds a temporary `{}`, which is not `Send`{}, across an `await`",
            ctx.ty_name(&t),
            cause(Some(&t), send)
        ),
    });
    let cell_why = |l: LocalId| {
        format!(
            "it has a `[ref {}]` capture, which the code that made it could change at the same \
             time",
            name(l)
        )
    };
    let why_cell = w.cell.map(|l| {
        if cell(l) {
            return (cell_why(l), false);
        }
        let n = name(l);
        (
            format!(
                "it captures the guard `{n}`, which stays with its `using` block: read the \
                 value first (`let v = {n}.get();`) and capture `v`"
            ),
            true,
        )
    });
    let why_shared = w.shared.map(|l| {
        if cell(l) {
            return cell_why(l);
        }
        format!(
            "it captures `{}: {}`, which is not `Sync`{} (the spawner keeps its \
             copy)",
            name(l),
            ty_of(l).map(|t| ctx.ty_name(t)).unwrap_or_default(),
            sync.map(|k| cause(ty_of(l), k)).unwrap_or_default()
        )
    });
    (why, why_shared, why_cell, held)
}

/// A lock guard (good only inside its `using` block).
fn is_guard(ctx: &LowerCtx<'_>, ty: &HirTy) -> bool {
    let HirTy::Struct(id) = ty else { return false };
    let name = ctx.struct_def(*id).name;
    ["MutexGuard", "RwLockReadGuard", "RwLockWriteGuard"]
        .iter()
        .any(|g| name.starts_with(&format!("rok::core::sync::{g}")))
}

enum Found {
    Capture(LocalId),
    Held(LocalId),
    Temp(HirTy),
}

struct Walk<'a> {
    non_send_local: &'a dyn Fn(LocalId) -> bool,
    non_sync_local: &'a dyn Fn(LocalId) -> bool,
    cell_local: &'a dyn Fn(LocalId) -> bool,
    non_send_ty: &'a dyn Fn(&HirTy) -> bool,
    env_params: &'a HashSet<LocalId>,
    /// Locals declared so far.
    declared: Vec<LocalId>,
    /// Those live at a suspend point seen so far.
    crossed: HashSet<LocalId>,
    /// Those used after a suspend point they were live at (plus the
    /// captures): the frame.
    held: HashSet<LocalId>,
    /// Suspend points walked so far.
    suspends: u32,
    /// The first offender.
    found: Option<Found>,
    /// The first capture that is not `Sync`.
    shared: Option<LocalId>,
    /// The first `[ref x]` capture.
    cell: Option<LocalId>,
}

impl Walk<'_> {
    fn declare(&mut self, l: LocalId, init: Option<&HirExpr>) {
        // A capture: unpacked from the env parameter in the prelude.
        let from_env = init.is_some_and(|e| {
            matches!(&e.kind, HirExprKind::TupleField { target, .. }
                if matches!(&target.kind, HirExprKind::Local(p) if self.env_params.contains(p)))
        });
        if from_env && self.found.is_none() && (self.non_send_local)(l) {
            self.found = Some(Found::Capture(l));
        }
        if from_env && self.shared.is_none() && (self.non_sync_local)(l) {
            self.shared = Some(l);
        }
        if from_env && self.cell.is_none() && (self.cell_local)(l) {
            self.cell = Some(l);
        }
        self.declared.push(l);
    }

    fn use_local(&mut self, l: LocalId) {
        if self.crossed.contains(&l) {
            self.held.insert(l);
            if self.found.is_none() && (self.non_send_local)(l) {
                self.found = Some(Found::Held(l));
            }
        }
    }

    fn block(&mut self, b: &HirBlock) {
        for s in &b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &b.trailing {
            self.expr(t);
        }
    }

    fn stmt(&mut self, s: &HirStmt) {
        match s {
            HirStmt::Let { local, init } => {
                self.expr(init);
                self.declare(*local, Some(init));
            }
            HirStmt::Assign { value, .. } | HirStmt::StaticAssign { value, .. } => self.expr(value),
            HirStmt::ArrayAssign {
                array_local,
                index,
                value,
                ..
            } => {
                self.expr(index);
                self.expr(value);
                self.use_local(*array_local);
            }
            HirStmt::Expr(e)
            | HirStmt::Panic(e)
            | HirStmt::Return(Some(e))
            | HirStmt::Break { value: Some(e), .. } => self.expr(e),
            _ => {}
        }
    }

    fn pattern(&mut self, p: &HirPattern) {
        p.each_binding(&mut |l| self.declare(l, None));
    }

    fn expr(&mut self, e: &HirExpr) {
        match &e.kind {
            HirExprKind::Local(l) => self.use_local(*l),
            HirExprKind::Suspend { value, is_final } => {
                self.expr(value);
                if !is_final {
                    self.suspends += 1;
                    self.crossed.extend(self.declared.iter().copied());
                }
            }
            HirExprKind::While { cond, body, .. } => {
                for _ in 0..2 {
                    self.expr(cond);
                    self.block(body);
                }
            }
            HirExprKind::Loop { body, .. } => {
                self.block(body);
                self.block(body);
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms {
                    self.pattern(&a.pat);
                    if let Some(g) = &a.guard {
                        self.expr(g);
                    }
                    self.expr(&a.body);
                }
            }
            kind => {
                // Operands evaluated before a later operand suspends are
                // temporaries held across that suspend (not a branch's
                // condition: it is consumed before the branch runs).
                let operands = !matches!(kind, HirExprKind::If { .. } | HirExprKind::Block(_));
                let mut kids = Vec::new();
                each_child(e, &mut |c| kids.push(c));
                // Operand types are asked only once a later one suspends.
                let mut prior: Vec<&HirTy> = Vec::new();
                for c in kids {
                    let before = self.suspends;
                    match c {
                        Child::E(x) => self.expr(x),
                        Child::B(b) => self.block(b),
                    }
                    if self.suspends > before && self.found.is_none() {
                        if let Some(t) = prior.iter().find(|t| (self.non_send_ty)(t)) {
                            self.found = Some(Found::Temp((*t).clone()));
                        }
                        prior.clear();
                    }
                    if let Child::E(x) = c {
                        if operands && self.found.is_none() {
                            prior.push(&x.ty);
                        }
                    }
                }
            }
        }
    }
}

/// What a value may hold: a coroutine with a non-`Send` frame, or a
/// function / closure body (to route calls through `fn` values).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Item {
    Bad(FunctionId),
    /// A coroutine whose capture is not `Sync` (`shared`): only the
    /// coroutine built right there, not one held inside another frame.
    Shared(FunctionId),
    Code(FunctionId),
}

type Set = BTreeSet<Item>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Node {
    Local(FunctionId, LocalId),
    Ret(FunctionId),
    /// What function `f` stores into (through) its parameter `i`.
    Stored(FunctionId, usize),
    Static(String),
}

struct Flow<'a> {
    ctx: &'a LowerCtx<'a>,
    send: &'a str,
    all: &'a HashMap<FunctionId, &'a HirFunction>,
    bad: &'a HashMap<FunctionId, String>,
    shared: &'a HashMap<FunctionId, String>,
    held: &'a HashMap<FunctionId, Vec<LocalId>>,
    nodes: HashMap<Node, Set>,
    /// The functions whose walk read each node.
    readers: HashMap<Node, BTreeSet<FunctionId>>,
    /// Functions to walk (again): a node they read grew.
    dirty: BTreeSet<FunctionId>,
    /// Per type: holds nothing.
    cut: HashMap<HirTy, bool>,
    /// Per struct / enum: may hold a `fn` value.
    holds: HashMap<(bool, u32), bool>,
    cur: FunctionId,
    /// The current function's `ref` parameters.
    ref_params: Vec<LocalId>,
    /// Values of the enclosing loops / labelled blocks' `break`s.
    breaks: Vec<Set>,
    /// Sink calls of the current function's walk: span and what their
    /// arguments hold.
    sinks: Vec<(rokc_diagnostic::Span, &'static str, Set)>,
}

impl Flow<'_> {
    fn get(&mut self, n: &Node) -> Set {
        match self.readers.get_mut(n) {
            Some(r) => {
                r.insert(self.cur);
            }
            None => {
                self.readers.insert(n.clone(), BTreeSet::from([self.cur]));
            }
        }
        self.nodes.get(n).cloned().unwrap_or_default()
    }

    fn add(&mut self, n: Node, s: &Set) {
        if s.is_empty() {
            return;
        }
        let e = self.nodes.entry(n.clone()).or_default();
        let before = e.len();
        e.extend(s.iter().copied());
        if e.len() > before {
            if let Some(r) = self.readers.get(&n) {
                self.dirty.extend(r.iter().copied());
            }
        }
    }

    fn function(&mut self, f: &HirFunction) {
        let Some(body) = &f.body else { return };
        self.cur = f.id;
        self.breaks.clear();
        self.ref_params = f
            .params
            .iter()
            .filter(|p| p.by_ref)
            .map(|p| p.local)
            .collect();
        for s in &body.stmts {
            self.stmt(s);
        }
        if let Some(t) = &body.trailing {
            let r = self.ret_value(t);
            self.add(Node::Ret(f.id), &r);
        }
    }

    /// A read of a `ref` parameter, packed into the result for the
    /// caller to write back: the caller keeps its own value, and the
    /// callee's stores come back through `Stored` (so one caller's value
    /// does not reach every caller).
    fn is_ref_read(&self, e: &HirExpr) -> bool {
        matches!(&e.kind, HirExprKind::Local(l) if self.ref_params.contains(l))
    }

    fn ret_value(&mut self, e: &HirExpr) -> Set {
        if self.is_ref_read(e) {
            Set::new()
        } else {
            self.expr(e)
        }
    }

    fn param_index(&self, f: FunctionId, l: LocalId) -> Option<usize> {
        self.all.get(&f)?.params.iter().position(|p| p.local == l)
    }

    /// `l` (of the current function) received `s` by a store into it.
    fn store(&mut self, l: LocalId, s: &Set) {
        self.add(Node::Local(self.cur, l), s);
        if let Some(i) = self.param_index(self.cur, l) {
            self.add(Node::Stored(self.cur, i), s);
        }
    }

    fn block(&mut self, b: &HirBlock) -> Set {
        let labelled = b.label.is_some();
        if labelled {
            self.breaks.push(Set::new());
        }
        for s in &b.stmts {
            self.stmt(s);
        }
        let mut r = b
            .trailing
            .as_ref()
            .map(|t| self.expr(t))
            .unwrap_or_default();
        if labelled {
            r.extend(self.breaks.pop().unwrap_or_default());
        }
        r
    }

    fn stmt(&mut self, s: &HirStmt) {
        match s {
            HirStmt::Let { local, init } => {
                let v = self.expr(init);
                self.add(Node::Local(self.cur, *local), &v);
            }
            HirStmt::Assign { local, value } => {
                let v = self.assigned(value, *local);
                self.store(*local, &v);
            }
            HirStmt::ArrayAssign {
                array_local,
                index,
                value,
                ..
            } => {
                self.expr(index);
                let v = self.expr(value);
                self.store(*array_local, &v);
            }
            HirStmt::StaticAssign { name, value } => {
                let v = self.expr(value);
                self.add(Node::Static(name.clone()), &v);
            }
            HirStmt::Return(Some(e)) => {
                let v = self.ret_value(e);
                self.add(Node::Ret(self.cur), &v);
            }
            HirStmt::Break { value: Some(e), .. } => {
                let v = self.expr(e);
                // Labels are not matched: every enclosing loop / labelled
                // block may get the value.
                for b in &mut self.breaks {
                    b.extend(v.iter().copied());
                }
            }
            HirStmt::Expr(e) | HirStmt::Panic(e) => {
                self.expr(e);
            }
            _ => {}
        }
    }

    /// What `value` assigned to `l` adds to it: `p.x = v` (a
    /// `WithField` chain on `l` itself) adds only `v`.
    fn assigned(&mut self, value: &HirExpr, l: LocalId) -> Set {
        let s = match &value.kind {
            HirExprKind::Local(x) if *x == l => return Set::new(),
            HirExprKind::WithField {
                target, value: v, ..
            } => {
                let mut s = self.assigned(target, l);
                s.extend(self.expr(v));
                s
            }
            _ => return self.expr(value),
        };
        if self.is_cut(&value.ty) {
            Set::new()
        } else {
            s
        }
    }

    /// A value of `ty` holds nothing: its type cannot hold a `fn` value,
    /// or opts into `Send` by an explicit impl (not a coroutine wrapper).
    fn is_cut(&mut self, ty: &HirTy) -> bool {
        if let Some(c) = self.cut.get(ty) {
            return *c;
        }
        let c = !self.may_hold(ty) || self.opted_in(ty);
        self.cut.insert(ty.clone(), c);
        c
    }

    fn opted_in(&self, ty: &HirTy) -> bool {
        let HirTy::Struct(id) = ty else { return false };
        let name = self.ctx.struct_def(*id).name;
        let base = name.split("__").next().unwrap_or(&name);
        let wrapper = WRAPPERS.iter().any(|w| {
            base.strip_suffix(w)
                .is_some_and(|rest| rest.is_empty() || rest.ends_with("::"))
        });
        !wrapper && self.ctx.has_impl(ty, &crate::TraitRef::plain(self.send), 0)
    }

    fn may_hold(&mut self, ty: &HirTy) -> bool {
        match ty {
            HirTy::Prim(_) | HirTy::Unit | HirTy::Never => false,
            HirTy::Tuple(ts) | HirTy::Sum(ts) => ts.iter().any(|t| self.may_hold(t)),
            HirTy::Array(e, _) | HirTy::GcBuf(e) => self.may_hold(e),
            HirTy::Throws { ok, err } => self.may_hold(ok) || self.may_hold(err),
            HirTy::Struct(id) => {
                let key = (true, id.0);
                if let Some(h) = self.holds.get(&key) {
                    return *h;
                }
                self.holds.insert(key, false); // a recursive type: its other parts decide
                let def = self.ctx.struct_def(*id);
                let h = def.fields.iter().any(|f| self.may_hold(&f.ty));
                self.holds.insert(key, h);
                h
            }
            HirTy::Enum(id) => {
                let key = (false, id.0);
                if let Some(h) = self.holds.get(&key) {
                    return *h;
                }
                self.holds.insert(key, false);
                let def = self.ctx.enum_def(*id);
                let h = def
                    .variants
                    .iter()
                    .any(|v| v.payload_ty.as_ref().is_some_and(|t| self.may_hold(t)));
                self.holds.insert(key, h);
                h
            }
            HirTy::Fn(..)
            | HirTy::Closure { .. }
            | HirTy::AnyTrait(_)
            | HirTy::Any
            | HirTy::Unknown
            | HirTy::Path(_)
            | HirTy::None
            | HirTy::Const(..)
            | HirTy::OpenConst(_)
            | HirTy::NoThrow
            | HirTy::Var(_)
            | HirTy::Param(_) => true,
        }
    }

    /// What the value of `e` may hold (and every flow inside it).
    fn expr(&mut self, e: &HirExpr) -> Set {
        let s = self.expr_inner(e);
        if s.is_empty() || self.is_cut(&e.ty) {
            return Set::new();
        }
        s
    }

    fn exprs(&mut self, es: &[HirExpr]) -> Vec<Set> {
        es.iter().map(|a| self.expr(a)).collect()
    }

    fn union(sets: Vec<Set>) -> Set {
        sets.into_iter().flatten().collect()
    }

    /// A call of `callee` (`skip` leading parameters are bound
    /// elsewhere: a closure's env): arguments flow into its parameters,
    /// its stores into an argument's place flow back, its result out.
    fn call(&mut self, callee: FunctionId, args: &[HirExpr], vals: &[Set]) -> Set {
        let Some(f) = self.all.get(&callee) else {
            return Self::union(vals.to_vec());
        };
        if f.body.is_none() {
            return Self::union(vals.to_vec());
        }
        let skip = f.params.len().saturating_sub(args.len());
        let params: Vec<LocalId> = f.params.iter().map(|p| p.local).collect();
        for (i, v) in vals.iter().enumerate() {
            if let Some(p) = params.get(skip + i) {
                self.add(Node::Local(callee, *p), v);
            }
            let stored = self.get(&Node::Stored(callee, skip + i));
            if let Some(r) = root(&args[i]) {
                self.store(r, &stored);
            }
        }
        self.get(&Node::Ret(callee))
    }

    fn expr_inner(&mut self, e: &HirExpr) -> Set {
        use HirExprKind as K;
        match &e.kind {
            K::Const(_) | K::SizeOf { .. } | K::UnresolvedPath(_) => Set::new(),
            K::Local(l) => self.get(&Node::Local(self.cur, *l)),
            K::StaticRef(n) => self.get(&Node::Static(n.clone())),
            K::Block(b) => self.block(b),
            // A step goes to the resumer like a return (futures a
            // generator yields, then spawned).
            K::Suspend { value, .. } => {
                let v = self.expr(value);
                self.add(Node::Ret(self.cur), &v);
                Set::new()
            }
            K::FnRef(id) => Set::from([Item::Code(*id)]),
            K::Call { callee, args } => {
                let vals = self.exprs(args);
                if let Some(sink) = self.all.get(callee).and_then(|f| sink_of(&f.name)) {
                    self.sinks.push((e.span, sink, Self::union(vals.clone())));
                }
                self.call(*callee, args, &vals)
            }
            K::CallIndirect { callee, args, .. }
            | K::CallClosure {
                closure: callee,
                args,
                ..
            } => {
                let target = self.expr(callee);
                let vals = self.exprs(args);
                let codes: Vec<FunctionId> = target
                    .iter()
                    .filter_map(|i| match i {
                        Item::Code(f) => Some(*f),
                        Item::Bad(_) | Item::Shared(_) => None,
                    })
                    .collect();
                if codes.is_empty() {
                    // An unknown target: it may return what it holds or gets.
                    let mut r = target;
                    r.extend(Self::union(vals));
                    return r;
                }
                let mut r = Set::new();
                for c in codes {
                    r.extend(self.call(c, args, &vals));
                }
                r
            }
            K::ClosureValue { fn_id, env, .. } => {
                let mut s = self.expr(env);
                if let Some(p) = self.all.get(fn_id).and_then(|f| f.params.first()) {
                    self.add(Node::Local(*fn_id, p.local), &s);
                }
                s.insert(Item::Code(*fn_id));
                s
            }
            K::CoroNew { body_fn, args } => {
                let vals = self.exprs(args);
                let params: Vec<LocalId> = self
                    .all
                    .get(body_fn)
                    .map(|f| f.params.iter().map(|p| p.local).collect())
                    .unwrap_or_default();
                for (p, v) in params.iter().zip(&vals) {
                    self.add(Node::Local(*body_fn, *p), v);
                }
                let mut s = Self::union(vals);
                // The futures its frame holds are part of it.
                for l in self.held.get(body_fn).into_iter().flatten() {
                    s.extend(self.get(&Node::Local(*body_fn, *l)));
                }
                // Inside this frame they share nothing with a spawner.
                s.retain(|i| !matches!(i, Item::Shared(_)));
                s.insert(Item::Code(*body_fn));
                if self.bad.contains_key(body_fn) {
                    s.insert(Item::Bad(*body_fn));
                }
                if self.shared.contains_key(body_fn) {
                    s.insert(Item::Shared(*body_fn));
                }
                s
            }
            K::If {
                cond,
                then_block,
                else_branch,
            } => {
                self.expr(cond);
                let mut r = self.block(then_block);
                if let Some(x) = else_branch {
                    r.extend(self.expr(x));
                }
                r
            }
            K::While { cond, body, .. } => {
                self.expr(cond);
                self.breaks.push(Set::new());
                self.block(body);
                self.breaks.pop();
                Set::new()
            }
            K::Loop { body, .. } => {
                self.breaks.push(Set::new());
                self.block(body);
                self.breaks.pop().unwrap_or_default()
            }
            K::Match { scrutinee, arms } => {
                let v = self.expr(scrutinee);
                let mut r = Set::new();
                for a in arms {
                    let mut ls = Vec::new();
                    a.pat.each_binding(&mut |l| ls.push(l));
                    for l in ls {
                        self.add(Node::Local(self.cur, l), &v);
                    }
                    if let Some(g) = &a.guard {
                        self.expr(g);
                    }
                    r.extend(self.expr(&a.body));
                }
                r
            }
            K::BufSet {
                buf, index, value, ..
            } => {
                self.expr(buf);
                self.expr(index);
                let v = self.expr(value);
                if let Some(r) = root(buf) {
                    self.store(r, &v);
                }
                Set::new()
            }
            K::FieldRead { target: x, .. }
            | K::TupleField { target: x, .. }
            | K::UnionRead { target: x, .. }
            | K::Cast { operand: x, .. }
            | K::ArrayRepeat { value: x, .. }
            | K::UnionWrite { value: x, .. }
            | K::SumWrap { value: Some(x), .. } => self.expr(x),
            K::BufGet { buf: x, index, .. }
            | K::ArrayIndex {
                array: x, index, ..
            } => {
                self.expr(index);
                self.expr(x)
            }
            K::Tuple { fields: xs } => {
                let mut r = Set::new();
                for x in xs {
                    if self.is_ref_read(x) {
                        continue;
                    }
                    r.extend(self.expr(x));
                }
                r
            }
            K::StructInit { fields: xs, .. }
            | K::EnumVariantInit { args: xs, .. }
            | K::ArrayInit { elements: xs, .. } => {
                let vals = self.exprs(xs);
                Self::union(vals)
            }
            K::WithField { target, value, .. } => {
                let mut s = self.expr(target);
                s.extend(self.expr(value));
                s
            }
            // Scalars and raw memory: their operands only.
            K::PrimBinOp { .. }
            | K::PrimUnaryOp { .. }
            | K::BufNew { .. }
            | K::RawRead { .. }
            | K::AddrOf { .. }
            | K::RawWrite { .. }
            | K::BufAtomic { .. }
            | K::SumWrap { value: None, .. } => {
                let mut kids = Vec::new();
                each_child(e, &mut |c| kids.push(c));
                for c in kids {
                    match c {
                        Child::E(x) => {
                            self.expr(x);
                        }
                        Child::B(b) => {
                            self.block(b);
                        }
                    }
                }
                Set::new()
            }
        }
    }
}

/// The local a place expression reads from (`x`, `x.f`, `x[i]` ..).
pub(super) fn root(e: &HirExpr) -> Option<LocalId> {
    match &e.kind {
        HirExprKind::Local(l) => Some(*l),
        HirExprKind::FieldRead { target: x, .. }
        | HirExprKind::TupleField { target: x, .. }
        | HirExprKind::UnionRead { target: x, .. }
        | HirExprKind::Cast { operand: x, .. }
        | HirExprKind::BufGet { buf: x, .. }
        | HirExprKind::ArrayIndex { array: x, .. } => root(x),
        HirExprKind::Block(b) if b.stmts.is_empty() => root(b.trailing.as_ref()?),
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub(super) enum Child<'e> {
    E(&'e HirExpr),
    B(&'e HirBlock),
}

/// The direct sub-expressions and sub-blocks of `e` in evaluation order
/// (not `Match`: callers handle its patterns).
pub(super) fn each_child<'e>(e: &'e HirExpr, out: &mut dyn FnMut(Child<'e>)) {
    use HirExprKind as K;
    match &e.kind {
        K::Block(b) => out(Child::B(b)),
        K::PrimBinOp { lhs, rhs, .. } => {
            out(Child::E(lhs));
            out(Child::E(rhs));
        }
        K::PrimUnaryOp { operand: x, .. }
        | K::Cast { operand: x, .. }
        | K::FieldRead { target: x, .. }
        | K::TupleField { target: x, .. }
        | K::ArrayRepeat { value: x, .. }
        | K::BufNew { len: x, .. }
        | K::RawRead { addr: x, .. }
        | K::AddrOf { place: x }
        | K::UnionWrite { value: x, .. }
        | K::UnionRead { target: x, .. }
        | K::Suspend { value: x, .. }
        | K::ClosureValue { env: x, .. }
        | K::SumWrap { value: Some(x), .. } => out(Child::E(x)),
        K::Call { args, .. }
        | K::CoroNew { args, .. }
        | K::StructInit { fields: args, .. }
        | K::EnumVariantInit { args, .. }
        | K::Tuple { fields: args }
        | K::ArrayInit { elements: args, .. } => args.iter().for_each(|a| out(Child::E(a))),
        K::CallIndirect { callee, args, .. } => {
            out(Child::E(callee));
            args.iter().for_each(|a| out(Child::E(a)));
        }
        K::CallClosure { closure, args, .. } => {
            out(Child::E(closure));
            args.iter().for_each(|a| out(Child::E(a)));
        }
        K::If {
            cond,
            then_block,
            else_branch,
        } => {
            out(Child::E(cond));
            out(Child::B(then_block));
            if let Some(x) = else_branch {
                out(Child::E(x));
            }
        }
        K::While { cond, body, .. } => {
            out(Child::E(cond));
            out(Child::B(body));
        }
        K::Loop { body, .. } => out(Child::B(body)),
        K::WithField { target, value, .. } => {
            out(Child::E(target));
            out(Child::E(value));
        }
        K::RawWrite { addr, value, .. } => {
            out(Child::E(addr));
            out(Child::E(value));
        }
        K::BufGet { buf, index, .. } => {
            out(Child::E(buf));
            out(Child::E(index));
        }
        K::BufSet {
            buf, index, value, ..
        } => {
            out(Child::E(buf));
            out(Child::E(index));
            out(Child::E(value));
        }
        K::BufAtomic {
            buf, index, args, ..
        } => {
            out(Child::E(buf));
            out(Child::E(index));
            args.iter().for_each(|a| out(Child::E(a)));
        }
        K::ArrayIndex { array, index, .. } => {
            out(Child::E(array));
            out(Child::E(index));
        }
        K::Match { .. }
        | K::Const(_)
        | K::Local(_)
        | K::StaticRef(_)
        | K::UnresolvedPath(_)
        | K::FnRef(_)
        | K::SizeOf { .. }
        | K::SumWrap { value: None, .. } => {}
    }
}
