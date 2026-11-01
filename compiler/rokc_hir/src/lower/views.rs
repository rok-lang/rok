//! A `SliceMut` used after its list was resized (stdlib 2.9.7) is an
//! error where one function shows both.
//!
//! A forward flow over structured HIR, per function. A view local holds
//! the lists it views: a list made into a view (`xs` or `xs[a:b]` in a
//! `SliceMut` slot, both a `slice_mut` hook call) or any view a value is
//! made from (`s[1:]`, `s.split_at(2)`). A resizing `List` method called
//! on a list local makes every view of it stale; reading a stale view is
//! the error, assigning it re-arms it. Plain copies of a list local are
//! the same list. Branches join by union, loops run to a fixpoint. What
//! this cannot see (a resize inside a callee, a view kept in a struct)
//! the view's run-time version check catches.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rokc_diagnostic::{Code, RokDiagnostic, Span};
use rokc_ir_common::{FunctionId, LocalId};

use super::Env;
use super::send_check::{Child, each_child};
use crate::{HirBlock, HirExpr, HirExprKind, HirFunction, HirStmt, HirTy};

const HOOK: &str = "rok::core::index::slice_mut";
const LIST: &str = "rok::core::collections::list::List";
const VIEWS: &[&str] = &[
    "rok::core::collections::slice::SliceMut",
    "rok::core::collections::slice::SliceMutIter",
];

/// `List` methods that change its length or move its buffer.
const RESIZES: &[&str] = &[
    "push",
    "pop",
    "insert",
    "remove",
    "clear",
    "truncate",
    "drain",
    "set_len",
    "reserve",
    "shrink_to_fit",
    "extend",
    "retain",
    "dedup",
];

/// What a call does to views.
#[derive(Clone, Copy)]
enum Op {
    /// Makes a view of its first argument.
    View,
    /// Resizes its first argument (`.0`: the method as written).
    Resize(&'static str),
}

/// The base path of a function or type name (no instance or arguments).
fn base(name: &str) -> &str {
    let name = name.split(rokc_ir_common::MONO_MARK).next().unwrap_or(name);
    name.split('<').next().unwrap_or(name)
}

fn op_of(name: &str) -> Option<Op> {
    let path = base(name);
    if path == HOOK {
        return Some(Op::View);
    }
    // `xs += ..`: an `AddAssign` impl of the list module.
    if path.starts_with("rok::core::collections::list::__impl") && path.ends_with("_add_assign") {
        return Some(Op::Resize("+="));
    }
    let rest = path.strip_prefix(LIST)?.strip_prefix("__")?;
    RESIZES.iter().find(|m| **m == rest).map(|m| Op::Resize(m))
}

/// A resize that made a view stale.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Resize {
    at: Span,
    list: LocalId,
    /// The list local the method was called on (a copy of `list`).
    recv: LocalId,
    method: &'static str,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct St {
    /// The list locals each view local views.
    owners: BTreeMap<LocalId, BTreeSet<LocalId>>,
    /// Views made stale, and by what.
    stale: BTreeMap<LocalId, ResizeKey>,
    /// A copy of a list local: the local it copies.
    alias: BTreeMap<LocalId, LocalId>,
}

/// `Resize`, ordered for `St`'s maps.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ResizeKey(u32, u32, u32, u32);

fn join(a: Option<St>, b: Option<St>) -> Option<St> {
    let (mut a, b) = match (a, b) {
        (None, x) | (x, None) => return x,
        (Some(a), Some(b)) => (a, b),
    };
    for (v, o) in b.owners {
        a.owners.entry(v).or_default().extend(o);
    }
    for (v, r) in b.stale {
        a.stale.entry(v).or_insert(r);
    }
    a.alias.retain(|k, v| b.alias.get(k) == Some(v));
    Some(a)
}

struct Walk<'a> {
    ops: &'a HashMap<FunctionId, Op>,
    views: &'a HashSet<u32>,
    lists: &'a HashSet<u32>,
    /// Each local's name (`#` when it has none) and type.
    locals: HashMap<LocalId, (&'a str, &'a HirTy)>,
    /// Resizes by key (`ResizeKey`).
    resizes: Vec<Resize>,
    /// Break / continue targets: label, state at `break`, at `continue`.
    loops: Vec<(Option<String>, Option<St>, Option<St>)>,
    reported: HashSet<(LocalId, u32, u32)>,
    diags: Vec<RokDiagnostic>,
    names: &'a dyn Fn(u32) -> String,
}

/// `ty` is or holds (in a tuple, sum or array) a struct of `set`.
fn holds(ty: &HirTy, set: &HashSet<u32>) -> bool {
    match ty {
        HirTy::Struct(id) => set.contains(&id.0),
        HirTy::Tuple(ts) | HirTy::Sum(ts) => ts.iter().any(|t| holds(t, set)),
        HirTy::Array(t, _) => holds(t, set),
        _ => false,
    }
}

impl Walk<'_> {
    fn holds_view(&self, ty: &HirTy) -> bool {
        holds(ty, self.views)
    }

    fn is_list(&self, ty: &HirTy) -> bool {
        matches!(ty, HirTy::Struct(id) if self.lists.contains(&id.0))
    }

    fn local(&self, l: LocalId) -> Option<(&str, &HirTy)> {
        self.locals.get(&l).copied()
    }

    fn shown(&self, l: LocalId) -> Option<String> {
        self.local(l)
            .map(|(n, _)| n)
            .filter(|n| !n.starts_with('#'))
            .map(str::to_owned)
    }

    /// The list a list-typed expression names: a local, through copies.
    fn list_root(st: &St, e: &HirExpr) -> Option<LocalId> {
        let HirExprKind::Local(l) = &e.kind else {
            return None;
        };
        Some(st.alias.get(l).copied().unwrap_or(*l))
    }

    fn key(&mut self, r: Resize) -> ResizeKey {
        let i = self.resizes.len();
        self.resizes.push(r);
        #[allow(clippy::cast_possible_truncation)]
        ResizeKey(r.at.file, r.at.start, r.list.0, i as u32)
    }

    fn resize(&self, k: ResizeKey) -> Resize {
        self.resizes[k.3 as usize]
    }

    fn report(&mut self, l: LocalId, k: ResizeKey, at: Span) {
        let r = self.resize(k);
        if at.is_dummy() || !self.reported.insert((l, r.at.file, r.at.start)) {
            return;
        }
        let list = self.shown(r.list).unwrap_or_else(|| "its list".to_owned());
        let recv = self.shown(r.recv).unwrap_or_else(|| list.clone());
        let call = if r.method == "+=" {
            format!("`{recv} += ..`")
        } else {
            format!("`{recv}.{}(..)`", r.method)
        };
        let line = if r.at.file == at.file {
            format!("line {}", r.at.line)
        } else {
            format!("{}:{}", (self.names)(r.at.file), r.at.line)
        };
        let (msg, again) = match self.shown(l) {
            Some(v) => (
                format!("`{v}` views `{list}`, which {call} resized at {line}"),
                format!("`{v}`"),
            ),
            None => (
                format!("a view of `{list}` is used here after {call} resized it at {line}"),
                "the view".to_owned(),
            ),
        };
        let d = RokDiagnostic::error(Code::VIEW_AFTER_RESIZE, at, msg)
            .with_secondary(r.at, format!("`{list}` is resized here"))
            .with_note(format!(
                "help: a resize may move the list's elements, so a view made before it cannot \
                 be used after it: make {again} again after the resize (`{list}[a:b]` where a \
                 `SliceMut` is expected), or resize before making the view (stdlib 2.9.7a)"
            ));
        self.diags.push(d);
    }

    /// Walk `e`; the lists its value views, if it holds a view.
    fn expr(&mut self, st: &mut Option<St>, e: &HirExpr) -> BTreeSet<LocalId> {
        let mut out = BTreeSet::new();
        match &e.kind {
            HirExprKind::Local(l) => {
                if let Some(s) = st.as_ref() {
                    if let Some(o) = s.owners.get(l) {
                        out.extend(o.iter().copied());
                    }
                    if let Some(&k) = s.stale.get(l) {
                        self.report(*l, k, e.span);
                    }
                }
            }
            HirExprKind::Call { callee, args } => {
                for a in args {
                    out.extend(self.expr(st, a));
                }
                let first = args.first();
                match (self.ops.get(callee), st.as_mut(), first) {
                    (Some(Op::View), Some(state), Some(arg)) => {
                        if let Some(list) = Self::list_root(state, arg) {
                            out.insert(list);
                        }
                    }
                    (Some(Op::Resize(method)), Some(state), Some(arg)) if self.is_list(&arg.ty) => {
                        if let (Some(list), HirExprKind::Local(recv)) =
                            (Self::list_root(state, arg), &arg.kind)
                        {
                            let hit: Vec<LocalId> = state
                                .owners
                                .iter()
                                .filter(|(_, owners)| owners.contains(&list))
                                .map(|(view, _)| *view)
                                .collect();
                            let recv = *recv;
                            let key = self.key(Resize {
                                at: e.span,
                                list,
                                recv,
                                method,
                            });
                            let state = st.as_mut().expect("live state");
                            for view in hit {
                                state.stale.entry(view).or_insert(key);
                            }
                        }
                    }
                    _ => {}
                }
            }
            HirExprKind::Block(b) => return self.block(st, b),
            HirExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                self.expr(st, cond);
                let mut t = st.clone();
                out.extend(self.block(&mut t, then_block));
                let mut f = st.clone();
                if let Some(x) = else_branch {
                    out.extend(self.expr(&mut f, x));
                }
                *st = join(t, f);
            }
            HirExprKind::Match { scrutinee, arms } => {
                let o = self.expr(st, scrutinee);
                let mut end: Option<St> = None;
                for arm in arms {
                    let mut a = st.clone();
                    if let Some(s) = a.as_mut() {
                        arm.pat.each_binding(&mut |l| {
                            s.stale.remove(&l);
                            s.owners.remove(&l);
                            if !o.is_empty() {
                                s.owners.insert(l, o.clone());
                            }
                        });
                    }
                    if let Some(g) = &arm.guard {
                        self.expr(&mut a, g);
                    }
                    out.extend(self.expr(&mut a, &arm.body));
                    end = join(end, a);
                }
                *st = end;
            }
            HirExprKind::While { label, cond, body } => {
                self.looped(st, label.clone(), Some(cond), body);
            }
            HirExprKind::Loop { label, body, .. } => {
                self.looped(st, label.clone(), None, body);
            }
            _ => {
                let mut kids = Vec::new();
                each_child(e, &mut |c| kids.push(c));
                for c in kids {
                    match c {
                        Child::E(x) => out.extend(self.expr(st, x)),
                        Child::B(b) => out.extend(self.block(st, b)),
                    }
                }
            }
        }
        if !self.holds_view(&e.ty) {
            out.clear();
        }
        out
    }

    /// A loop to a fixpoint: the state at its head joins the entry and
    /// each `continue` / end of the body; it ends where the condition
    /// fails or at a `break`.
    fn looped(
        &mut self,
        st: &mut Option<St>,
        label: Option<String>,
        cond: Option<&HirExpr>,
        body: &HirBlock,
    ) {
        let entry = st.take();
        // Re-assigned each pass; clippy sees only the first value.
        #[allow(clippy::redundant_clone)]
        let mut head = entry.clone();
        let mut exit = None;
        for _ in 0..8 {
            self.loops.push((label.clone(), None, None));
            let mut s = head.clone();
            if let Some(c) = cond {
                self.expr(&mut s, c);
            }
            let after_cond = s.clone();
            self.block(&mut s, body);
            let (_, brk, cont) = self.loops.pop().expect("loop pushed");
            let next = join(join(entry.clone(), s), cont);
            exit = if cond.is_some() {
                join(after_cond, brk)
            } else {
                brk
            };
            if next == head {
                break;
            }
            head = next;
        }
        *st = exit;
    }

    fn block(&mut self, st: &mut Option<St>, b: &HirBlock) -> BTreeSet<LocalId> {
        let labeled = b.label.is_some();
        if labeled {
            self.loops.push((b.label.clone(), None, None));
        }
        for s in &b.stmts {
            self.stmt(st, s);
        }
        let out = b
            .trailing
            .as_ref()
            .map(|t| self.expr(st, t))
            .unwrap_or_default();
        if labeled {
            let (_, brk, _) = self.loops.pop().expect("block pushed");
            *st = join(st.take(), brk);
        }
        out
    }

    fn bind(&self, st: &mut Option<St>, local: LocalId, init: &HirExpr, o: BTreeSet<LocalId>) {
        let Some(s) = st.as_mut() else {
            return;
        };
        s.stale.remove(&local);
        s.owners.remove(&local);
        s.alias.remove(&local);
        s.alias.retain(|_, v| *v != local);
        let ty = self.local(local).map(|(_, t)| t.clone());
        if ty.as_ref().is_some_and(|t| self.holds_view(t)) && !o.is_empty() {
            s.owners.insert(local, o);
        }
        if ty.as_ref().is_some_and(|t| self.is_list(t)) {
            if let Some(r) = Self::list_root(s, init).filter(|r| *r != local) {
                s.alias.insert(local, r);
            }
        }
    }

    fn target(&mut self, st: &mut Option<St>, label: Option<&String>, brk: bool) {
        let s = st.take();
        let i = match label {
            Some(l) => self.loops.iter().rposition(|(x, ..)| x.as_ref() == Some(l)),
            None => self.loops.len().checked_sub(1),
        };
        if let Some(i) = i {
            let slot = &mut self.loops[i];
            if brk {
                slot.1 = join(slot.1.take(), s);
            } else {
                slot.2 = join(slot.2.take(), s);
            }
        }
    }

    fn stmt(&mut self, st: &mut Option<St>, s: &HirStmt) {
        match s {
            HirStmt::Let { local, init } | HirStmt::Assign { local, value: init } => {
                let o = self.expr(st, init);
                self.bind(st, *local, init, o);
            }
            HirStmt::ArrayAssign { index, value, .. } => {
                self.expr(st, index);
                self.expr(st, value);
            }
            HirStmt::StaticAssign { value, .. } | HirStmt::Expr(value) => {
                self.expr(st, value);
            }
            HirStmt::Return(v) => {
                if let Some(v) = v {
                    self.expr(st, v);
                }
                *st = None;
            }
            HirStmt::Panic(v) => {
                self.expr(st, v);
                *st = None;
            }
            HirStmt::Break { label, value } => {
                if let Some(v) = value {
                    self.expr(st, v);
                }
                self.target(st, label.as_ref(), true);
            }
            HirStmt::Continue { label } => self.target(st, label.as_ref(), false),
        }
    }
}

pub(super) fn check(env: &Env<'_>, functions: &[HirFunction]) {
    let extras = env.sink.extras.borrow();
    let all: Vec<&HirFunction> = functions.iter().chain(extras.iter()).collect();
    let ops: HashMap<FunctionId, Op> = all
        .iter()
        .filter_map(|f| op_of(&f.name).map(|o| (f.id, o)))
        .collect();
    if !ops.values().any(|o| matches!(o, Op::View)) {
        return;
    }
    let structs = env.defs.structs.borrow();
    let ids = |names: &[&str]| -> HashSet<u32> {
        #[allow(clippy::cast_possible_truncation)]
        structs
            .iter()
            .enumerate()
            .filter(|(_, s)| names.contains(&super::expr::enum_base_name(&s.name)))
            .map(|(i, _)| i as u32)
            .collect()
    };
    let views = ids(VIEWS);
    let lists = ids(&[LIST]);
    drop(structs);
    let names = |file: u32| {
        let p = env.sources.path(file).unwrap_or_default();
        p.rsplit(['/', '\\']).next().unwrap_or(p).to_owned()
    };
    let mut diags = Vec::new();
    for f in &all {
        let Some(body) = &f.body else {
            continue;
        };
        let locals: HashMap<LocalId, (&str, &HirTy)> = f
            .locals
            .iter()
            .map(|x| (x.id, (x.name.as_deref().unwrap_or("#"), &x.ty)))
            .chain(f.params.iter().map(|p| (p.local, (p.name.as_str(), &p.ty))))
            .collect();
        // No local holds a view: nothing to follow.
        if !locals.values().any(|(_, ty)| holds(ty, &views)) {
            continue;
        }
        let mut w = Walk {
            ops: &ops,
            views: &views,
            lists: &lists,
            locals,
            resizes: Vec::new(),
            loops: Vec::new(),
            reported: HashSet::new(),
            diags: Vec::new(),
            names: &names,
        };
        let mut st = Some(St::default());
        w.block(&mut st, body);
        diags.extend(w.diags);
    }
    let mut seen = HashSet::new();
    diags.retain(|d: &RokDiagnostic| {
        let at = d.primary.as_ref().map(|l| (l.span.file, l.span.start));
        seen.insert((at, d.message.clone()))
    });
    drop(extras);
    env.sink.diagnostics.borrow_mut().extend(diags);
}
