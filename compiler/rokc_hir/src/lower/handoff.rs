//! Use after hand-off (stdlib 2.13.9) is an error. rok has no moves: after
//! `Mutex::new(xs)`, `tx.send(xs)` or `Handoff::new(xs)`, `xs` still names
//! an object other threads now share, and a handle taken from a guard
//! (`g.get()`, `g.set(v)`, `g.update(f)`) is unprotected once its `using`
//! block ends. Such a handle is a scoped view: it may not be returned,
//! thrown, yielded or sent out of the block, nor passed to a callee that
//! keeps it. A `ThreadLocal::with` closure's value is one too, for the
//! closure's body (`VIEW`).
//!
//! A forward flow over structured HIR. A place is a local plus a field path
//! (`s`, `s.items`, `t.0`); it is gone on a path from the hand-off (or the
//! guard's drop) on, and using it, a place inside it or one containing it is
//! an error. Aliases are edges: `Same` (a copy, a struct/tuple field) makes
//! two places one object; `In` (a capture, an array/enum literal, a `match`
//! binding) holds one in the other both ways; `Into` (coroutine arguments)
//! one way only. A buffer's elements are one place, `buf ++ [ELEM]`: what is
//! stored into it and what is read out of it are that place, so handing over
//! either side takes the other along. An element taken out (`pop`, `remove`)
//! is `Taken` from it: handing it over takes what was stored there, not the
//! buffer, unless the buffer may hold one object twice (`St::dup`). Going
//! away takes every alias along; assigning a place re-arms it. A value read
//! out of a `Sync` object (an `unsafe impl Sync` type's field, a
//! thread-local's slot) is `WORLD`: other threads may reach it, so handing it
//! over is an error. Paths split on known tags (`bool`,
//! variant, carrier error) so `if`/`while`/`match`/`catch`/`try?`/`??`
//! pass only the paths they let through; equal paths merge, and past
//! `MAX_PATHS` or `MAX_SPLIT_PASSES` all do. Joins are unions; loops run to
//! a fixpoint. Exempt: `Sync` types, `unsafe` (spec 4.15.2), a `match` on a
//! variable (its bindings are the uses) and compiler-made locals.
//!
//! Calls go through per-function summaries: what happens to each parameter
//! (handed over, returned, stored into another, captured, tied to a guard),
//! the exit state on entry parameters and the return, split by the tag of
//! the result (error, `none`, `false`). An `async` future carries its
//! body's final tag. Summaries are a call-graph fixpoint and ship in ROKHIR.
//! A call through a function value may reach any function used as a value
//! with those parameter types (closed world, as for `any`); `extern` and
//! intrinsics keep nothing; a `Handoff::new` never leaves its function.
//! Elements are not told apart by index: copying elements from one buffer
//! into another takes them to be distinct objects unless the source may hold
//! one twice.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use rokc_diagnostic::{Code, RokDiagnostic, Span};
use rokc_ir_common::{ConstVal, FunctionId, LocalId, PrimOp, PrimTy};

use super::send_check::{Child, each_child};
use super::{Env, LowerCtx};
use crate::{
    HirBlock, HirExpr, HirExprKind, HirFunction, HirMatchArm, HirPattern, HirStmt, HirTy, StructId,
};

/// Who takes a value over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum What {
    Mutex,
    RwLock,
    Channel,
    Handoff,
    /// `task::spawn`: the task's future.
    Task,
}

impl What {
    const fn noun(self) -> &'static str {
        match self {
            Self::Mutex => "a `Mutex`",
            Self::RwLock => "an `RwLock`",
            Self::Channel => "a channel",
            Self::Handoff => "a `Handoff`",
            Self::Task => "a task",
        }
    }

    const fn tag(self) -> &'static str {
        match self {
            Self::Mutex => "mutex",
            Self::RwLock => "rwlock",
            Self::Channel => "channel",
            Self::Handoff => "handoff",
            Self::Task => "task",
        }
    }

    fn from_tag(s: &str) -> Option<Self> {
        [
            Self::Mutex,
            Self::RwLock,
            Self::Channel,
            Self::Handoff,
            Self::Task,
        ]
        .into_iter()
        .find(|w| w.tag() == s)
    }
}

/// Calls that hand argument `.1` to another owner `.2`.
const HANDOFFS: &[(&str, usize, What)] = &[
    ("rok::core::sync::Mutex__new", 0, What::Mutex),
    ("rok::core::sync::RwLock__new", 0, What::RwLock),
    ("rok::core::channel::Sender__send", 1, What::Channel),
    ("rok::core::channel::Sender__try_send", 1, What::Channel),
    ("rok::core::channel::Sender__send_async", 1, What::Channel),
    ("rok::core::marker::Handoff__new", 0, What::Handoff),
    ("rok::core::task::spawn", 0, What::Task),
    ("rok::core::task::spawn_plain", 0, What::Task),
];

/// Lock guards: `get` returns, `set` stores, and `update`'s closure
/// does both with a handle the lock protects until `drop`.
const GUARDS: &[(&str, What)] = &[
    ("rok::core::sync::MutexGuard", What::Mutex),
    ("rok::core::sync::RwLockReadGuard", What::RwLock),
    ("rok::core::sync::RwLockWriteGuard", What::RwLock),
];

/// A future of an `async` body that may throw, and its poll: an error
/// comes out of `poll` only if the body ends with one.
const FUTURE: &str = "rok::core::future::TryAsyncFuture";
const POLL: &str = "rok::core::future::TryAsyncFuture__poll";

/// `name` is `path` or an instance of it (`path__mono__..`).
fn is_fn(name: &str, path: &str) -> bool {
    name.strip_prefix(path)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("__"))
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Handoff(usize, What),
    Get,
    Set,
    Update,
    Drop,
}

/// The op of function `name`; `guard`: a method of a `GUARDS` type.
/// Method names end the path (`MutexGuard__get`, a `Drop` impl's
/// `__impl<hash>_drop`), before a generic instance's `__mono__..`.
fn op_of(name: &str, guard: bool) -> Option<Op> {
    if let Some(&(_, i, what)) = HANDOFFS.iter().find(|(p, ..)| is_fn(name, p)) {
        return Some(Op::Handoff(i, what));
    }
    if !guard {
        return None;
    }
    let path = name.split(rokc_ir_common::MONO_MARK).next()?;
    match path.rsplit('_').next()? {
        "get" => Some(Op::Get),
        "set" => Some(Op::Set),
        "update" => Some(Op::Update),
        "drop" => Some(Op::Drop),
        _ => None,
    }
}

/// Marks a tuple index in a place path (struct fields are bare).
const TUPLE: u32 = 1 << 31;
/// Longest field path followed (alias chains cannot grow past it).
const MAX_DEPTH: usize = 12;
/// Locals the walk adds: each parameter as it was on entry, the return
/// value, and a call's arguments and result.
const SHADOW: u32 = 0x4000_0000;
const TEMP: u32 = 0x5000_0000;
/// The places holding a tested or stored value's tag (`Walk::key`).
const TAG_KEY: u32 = 0x6000_0000;
const RET: LocalId = LocalId(0x7fff_ffff);
/// The return value as a summary root.
const RET_ROOT: u32 = u32::MAX;
/// What other threads may reach: a value read out of a `Sync` object
/// (an `unsafe impl Sync` type's field, a thread-local's slot).
const WORLD: LocalId = LocalId(0x7fff_fffd);
const WORLD_ROOT: u32 = u32::MAX - 1;
/// The guard of a `ThreadLocal::with` closure's value: it lives for the
/// whole closure body.
const VIEW: LocalId = LocalId(0x7fff_fffc);
const WITH: &str = "rok::std::thread::ThreadLocal__with";
/// A path step: the elements of the buffer the place holds.
const ELEM: u32 = 0x3fff_fffe;

/// Functions whose value is an element taken out of argument 0's buffer:
/// the buffer no longer holds it, unless it may hold it twice.
const TAKES: &[&str] = &[
    "rok::core::collections::list::List__pop",
    "rok::core::collections::list::List__remove",
    "rok::core::collections::deque::Deque__pop_front",
    "rok::core::collections::deque::Deque__pop_back",
    "rok::core::collections::dict::Dict__remove",
    "rok::core::collections::heap::BinaryHeap__pop",
];

/// Functions whose value may be argument 0 itself (no body to walk). A
/// coroutine's priming steps copy through the hand-off's memo, which the
/// hand-off itself accounts for.
const PASSES: &[&str] = &[
    "rok::core::reflect::cast",
    "rok::core::future::prime_frame",
    "rok::core::future::prime_started",
];

/// Collection literals (`[a, b]`, `{k: v}`): their elements are the
/// argument array's, so the result holds an object twice only if two
/// of the written elements are one.
const LITERALS: &[&str] = &[
    "rok::core::collections::list::list_literal",
    "rok::core::collections::set::set_literal",
    "rok::core::collections::dict::dict_literal",
];

/// Modules whose functions move elements between slots of one collection
/// and copy each element of another one once (`stores_twice`).
const TRUSTED: &[&str] = &["rok::core::collections::", "rok::core::par::"];

/// Functions that drop every element of argument 0's buffer.
const CLEARS: &[&str] = &[
    "rok::core::collections::list::List__clear",
    "rok::core::collections::deque::Deque__clear",
    "rok::core::collections::dict::Dict__clear",
];

/// Functions that store every element of argument 1 into argument 0's
/// buffer (`xs.extend(ys)`, `xs += ys`).
const COPIES: &[&str] = &["rok::core::collections::list::List__extend"];

/// `par` functions that give each item of argument 0 to a worker.
const PAR: &[&str] = &[
    "rok::core::par::for_each",
    "rok::core::par::map",
    "rok::core::par::filter",
];

/// The place up to its first `ELEM` step: the buffer's elements.
fn elem_of(p: &Place) -> Option<Place> {
    let i = p.1.iter().position(|&s| s == ELEM)?;
    Some((p.0, p.1[..=i].to_vec()))
}

/// A local and a field path into it.
type Place = (LocalId, Vec<u32>);

/// `a` is `b` or a place inside it.
fn within(a: &Place, b: &Place) -> bool {
    a.0 == b.0 && a.1.starts_with(&b.1)
}

/// One of `a`, `b` is inside the other: they share memory.
fn overlap(a: &Place, b: &Place) -> bool {
    within(a, b) || within(b, a)
}

fn extend(p: &Place, rest: &[u32]) -> Place {
    (p.0, [&p.1[..], rest].concat())
}

/// Where a hand-off inside a callee happened, as its callers report it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Why {
    what: What,
    /// The function that made the hand-off, as written (`Bag::store`).
    func: String,
    file: String,
    line: u32,
}

/// Why a place is gone.
#[derive(Clone, Copy, PartialEq)]
enum Gone {
    /// Handed over at this call.
    To(Span, What),
    /// Handed over by this call (a `Why` id).
    Via(Span, u32),
    /// Tied to this guard at this span; the guard was dropped.
    Guard(Span, LocalId, Tie),
}

/// How a place is tied to a guard.
#[derive(Clone, Copy, PartialEq)]
enum Tie {
    /// It holds what the guard gave out (`g.get()`).
    Taken,
    /// The guard holds it (`g.set(v)`).
    Given,
    /// It shares one of those (a copy, a part, what was stored in it).
    Shared,
    /// It is the guard (a copy of `g` stored or captured).
    Itself,
}

impl Gone {
    const fn origin(self) -> Span {
        match self {
            Self::To(s, _) | Self::Via(s, _) | Self::Guard(s, ..) => s,
        }
    }
}

/// An alias: `Same` places are one object; `In(a, b)` holds one inside
/// the other (both go away together); `Into(a, b)`: `b` goes away with
/// `a`, not the reverse; `Taken(a, b)`: `a` was taken out of the buffer
/// elements `b`, so it may be what was stored there, but `b` no longer
/// holds it (unless the buffer may hold an object twice).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Edge {
    Same(Place, Place),
    In(Place, Place),
    Into(Place, Place),
    Taken(Place, Place),
}

impl Edge {
    const fn ends(&self) -> (&Place, &Place) {
        let (Self::Same(a, b) | Self::In(a, b) | Self::Into(a, b) | Self::Taken(a, b)) = self;
        (a, b)
    }

    fn touches(&self, p: &Place) -> bool {
        let (a, b) = self.ends();
        within(a, p) || within(b, p)
    }
}

/// The edges, under the local of each end.
#[derive(Clone, Default, PartialEq)]
struct Edges(BTreeMap<LocalId, BTreeSet<Edge>>);

impl Edges {
    fn of(&self, l: LocalId) -> impl Iterator<Item = &Edge> {
        self.0.get(&l).into_iter().flatten()
    }

    fn insert(&mut self, e: Edge) {
        let (a, b) = (e.ends().0.0, e.ends().1.0);
        if a != b {
            self.0.entry(b).or_default().insert(e.clone());
        }
        self.0.entry(a).or_default().insert(e);
    }

    fn extend(&mut self, o: Self) {
        for (l, es) in o.0 {
            self.0.entry(l).or_default().extend(es);
        }
    }

    /// Remove and return every edge with an end on `l`.
    fn take_of(&mut self, l: LocalId) -> Vec<Edge> {
        let mine: Vec<Edge> = self.0.remove(&l).into_iter().flatten().collect();
        for e in &mine {
            let (a, b) = (e.ends().0.0, e.ends().1.0);
            let other = if a == l { b } else { a };
            if let Some(es) = self.0.get_mut(&other) {
                es.remove(e);
                if es.is_empty() {
                    self.0.remove(&other);
                }
            }
        }
        mine
    }
}

#[derive(Clone, Default, PartialEq)]
struct St {
    gone: BTreeMap<Place, Gone>,
    /// Guard local -> the places holding what it protects.
    tied: BTreeMap<LocalId, BTreeMap<Place, (Span, Tie)>>,
    edges: Edges,
    /// Buffers (their `ELEM` places) that may hold one object twice.
    dup: BTreeSet<Place>,
    /// Where values were stored into buffer elements (for messages).
    stored: BTreeMap<Place, BTreeSet<Span>>,
}

/// An edge worth keeping: between two places, of followed depth.
fn kept(e: &Edge) -> bool {
    let (a, b) = e.ends();
    a != b && a.1.len() <= MAX_DEPTH && b.1.len() <= MAX_DEPTH
}

fn join_st(mut a: St, b: St) -> St {
    for (l, g) in b.gone {
        a.gone.entry(l).or_insert(g);
    }
    for (g, ls) in b.tied {
        let t = a.tied.entry(g).or_default();
        for (l, s) in ls {
            t.entry(l).or_insert(s);
        }
    }
    a.edges.extend(b.edges);
    a.dup.extend(b.dup);
    for (p, sp) in b.stored {
        a.stored.entry(p).or_default().extend(sp);
    }
    a
}

/// What a path knows of the value at a place: a `bool`, a sum or enum
/// variant, a carrier's (`ERR`, `OK`, `OK_TAG` + its value's), or a
/// `TryAsyncFuture`'s (that of the carrier its body ends with).
type Tags = BTreeMap<Place, u32>;

/// An exit whose tag is not known (`Summary::exits`).
const ANY: u32 = u32::MAX;
/// Paths kept apart at one point; past it they are one.
const MAX_PATHS: usize = 8;
/// Loop passes with paths kept apart; then they are one.
const MAX_SPLIT_PASSES: usize = 8;

/// The paths reaching a point, apart by the tags they know (empty:
/// unreachable). Paths with the same tags are one, and so are paths
/// with the same places gone and tied (their aliases joined): only
/// those tell paths apart later.
#[derive(Clone, Default, PartialEq)]
struct Cur(Vec<(Tags, St)>);

impl Cur {
    fn start() -> Self {
        Self(vec![(Tags::new(), St::default())])
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn states(&mut self) -> impl Iterator<Item = &mut St> {
        self.0.iter_mut().map(|(_, s)| s)
    }

    fn norm(&mut self) {
        loop {
            let mut out: Vec<(Tags, St)> = Vec::with_capacity(self.0.len());
            let mut merged = false;
            for (t, s) in std::mem::take(&mut self.0) {
                match out
                    .iter_mut()
                    .find(|(u, v)| *u == t || v.gone.keys().eq(s.gone.keys()) && v.tied == s.tied)
                {
                    Some(o) => {
                        if o.0 != t {
                            o.0.retain(|k, v| t.get(k) == Some(v));
                        }
                        if o.1 != s {
                            o.1 = join_st(std::mem::take(&mut o.1), s);
                        }
                        merged = true;
                    }
                    None => out.push((t, s)),
                }
            }
            self.0 = out;
            if !merged {
                break;
            }
        }
        if self.0.len() > MAX_PATHS {
            self.flatten();
        }
        self.0.sort_by(|a, b| a.0.cmp(&b.0));
    }

    /// One path: the tags all know, the states joined.
    fn flatten(&mut self) {
        let mut it = std::mem::take(&mut self.0).into_iter();
        if let Some((mut t, mut s)) = it.next() {
            for (u, v) in it {
                t.retain(|k, x| u.get(k) == Some(x));
                s = join_st(s, v);
            }
            self.0.push((t, s));
        }
    }

    fn untag(&mut self, k: &Place) {
        let mut hit = false;
        for (t, _) in &mut self.0 {
            let n = t.len();
            t.retain(|q, _| !overlap(q, k));
            hit |= t.len() != n;
        }
        if hit {
            self.norm();
        }
    }
}

fn join(a: Cur, b: Cur) -> Cur {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let mut c = a;
    c.0.extend(b.0);
    c.norm();
    c
}

/// The place `e` names exactly (`x`, `x.f.0`).
fn tag_place(e: &HirExpr) -> Option<Place> {
    match &e.kind {
        HirExprKind::Local(l) => Some((*l, Vec::new())),
        HirExprKind::FieldRead {
            target,
            field_index,
            ..
        } => tag_place(target).map(|p| extend(&p, &[*field_index])),
        HirExprKind::TupleField { target, index, .. } => {
            tag_place(target).map(|p| extend(&p, &[index | TUPLE]))
        }
        _ => None,
    }
}

/// How a value leaves the body (`escapes_guard`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Exit {
    Return,
    Yield,
    Throw,
}

/// The error a returned carrier `(true, .., err)` throws.
fn thrown(e: &HirExpr) -> Option<&HirExpr> {
    let HirExprKind::Tuple { fields } = &e.kind else {
        return None;
    };
    match (&e.ty, fields.first().map(|f| &f.kind)) {
        (HirTy::Throws { .. }, Some(HirExprKind::Const(ConstVal::Bool(true)))) => fields.last(),
        _ => None,
    }
}

/// A carrier's tag: an error, a value of unknown tag, or `OK_TAG` plus
/// the value's tag.
const OK: u32 = 0;
const ERR: u32 = 1;
const OK_TAG: u32 = 2;

/// `ty` is a carrier with a value slot (`throws -> T`, `T` not `()`).
const fn ok_slot(ty: &HirTy) -> bool {
    matches!(ty, HirTy::Throws { ok, .. } if !matches!(**ok, HirTy::Unit | HirTy::Never))
}

/// The tag of `e`'s value on a path that knows `t`.
fn tag_of(e: &HirExpr, t: &Tags) -> Option<u32> {
    match &e.kind {
        HirExprKind::Const(ConstVal::Bool(b)) => Some(u32::from(*b)),
        HirExprKind::SumWrap {
            sum_ty,
            variant_index,
            ..
        } if is_sum(sum_ty) && *sum_ty == e.ty => Some(*variant_index),
        HirExprKind::EnumVariantInit { variant_index, .. } => Some(*variant_index),
        HirExprKind::Tuple { fields } if matches!(e.ty, HirTy::Throws { .. }) => {
            match (tag_of(fields.first()?, t)?, ok_slot(&e.ty)) {
                (1, _) => Some(ERR),
                (_, true) => Some(tag_of(&fields[1], t).map_or(OK, |v| v.saturating_add(OK_TAG))),
                (_, false) => Some(OK),
            }
        }
        HirExprKind::PrimUnaryOp {
            op: PrimOp::LogicalNot,
            operand,
        } => tag_of(operand, t).map(|b| b ^ 1),
        // A carrier's `is_err`, and its value.
        HirExprKind::TupleField {
            target, index: 0, ..
        } if matches!(target.ty, HirTy::Throws { .. }) => {
            t.get(&tag_place(target)?).map(|&v| u32::from(v == ERR))
        }
        HirExprKind::TupleField {
            target, index: 1, ..
        } if ok_slot(&target.ty) => t
            .get(&tag_place(target)?)
            .and_then(|&v| v.checked_sub(OK_TAG)),
        HirExprKind::Block(b) if b.stmts.is_empty() => tag_of(b.trailing.as_ref()?, t),
        _ => t.get(&tag_place(e)?).copied(),
    }
}

const fn is_sum(ty: &HirTy) -> bool {
    matches!(ty, HirTy::Sum(_))
}

/// Pattern `p` may match a value of type `ty` with tag `v`.
fn may_match(p: &HirPattern, v: u32, ty: &HirTy) -> bool {
    match p {
        HirPattern::Literal {
            value: ConstVal::Bool(b),
            ..
        } if *ty == HirTy::Prim(PrimTy::Bool) => u32::from(*b) == v,
        HirPattern::SumVariant {
            sum_ty,
            variant_index,
            ..
        }
        | HirPattern::SumMember {
            sum_ty,
            variant_index,
            ..
        } if is_sum(ty) && sum_ty == ty => *variant_index == v,
        HirPattern::EnumVariant {
            enum_id,
            variant_index,
            ..
        } if *ty == HirTy::Enum(*enum_id) => *variant_index == v,
        HirPattern::At { sub, .. } => may_match(sub, v, ty),
        HirPattern::Or(alts) => alts.iter().any(|a| may_match(a, v, ty)),
        _ => true,
    }
}

/// Pattern `p` matches every value of type `ty` with tag `v`.
fn catches(p: &HirPattern, v: u32, ty: &HirTy) -> bool {
    match p {
        HirPattern::Wildcard | HirPattern::Binding { .. } => true,
        HirPattern::Literal {
            value: ConstVal::Bool(b),
            ..
        } => *ty == HirTy::Prim(PrimTy::Bool) && u32::from(*b) == v,
        HirPattern::SumVariant {
            sum_ty,
            variant_index,
            ..
        } => is_sum(ty) && sum_ty == ty && *variant_index == v,
        HirPattern::SumMember {
            sum_ty,
            variant_index,
            sub,
            ..
        } => is_sum(ty) && sum_ty == ty && *variant_index == v && sub.is_irrefutable(),
        HirPattern::EnumVariant {
            enum_id,
            variant_index,
            args,
        } => {
            *ty == HirTy::Enum(*enum_id)
                && *variant_index == v
                && args.iter().all(HirPattern::is_irrefutable)
        }
        HirPattern::At { sub, .. } => catches(sub, v, ty),
        HirPattern::Or(alts) => alts.iter().any(|a| catches(a, v, ty)),
        _ => false,
    }
}

/// `q` and every place aliased to it go away in `s`.
fn mark_gone(s: &mut St, p: Place, why: Gone) {
    let first = p.clone();
    // Each place with the buffer elements it must not reach: what was
    // taken out of a buffer is not in it any more.
    let mut work: Vec<(Place, Rc<Vec<Place>>)> = vec![(p, Rc::new(Vec::new()))];
    while let Some((q, blocked)) = work.pop() {
        // What a guard gave out: its aliases share it.
        let why = match why {
            Gone::Guard(at, g, Tie::Taken) if q != first => Gone::Guard(at, g, Tie::Shared),
            w => w,
        };
        if q.1.len() > MAX_DEPTH || s.gone.keys().any(|g| within(&q, g)) {
            continue;
        }
        let mut next: Vec<(Place, Rc<Vec<Place>>)> = step(s, &q)
            .into_iter()
            .map(|x| (x, blocked.clone()))
            .collect();
        for e in s.edges.of(q.0) {
            let Edge::Taken(a, b) = e else { continue };
            if !overlap(&q, a) {
                continue;
            }
            if is_dup(s, b) {
                next.push((b.clone(), blocked.clone()));
                continue;
            }
            // What was stored into the buffer, not the buffer.
            let elems = elem_aliases(s, b);
            let mut bl = (*blocked).clone();
            bl.extend(elems.iter().cloned());
            let bl = Rc::new(bl);
            for h in &elems {
                for x in step(s, h) {
                    if !elems.iter().any(|h| overlap(&x, h)) {
                        next.push((x, bl.clone()));
                    }
                }
            }
        }
        work.extend(
            next.into_iter()
                .filter(|(x, bl)| !bl.iter().any(|b| overlap(x, b))),
        );
        s.gone.retain(|g, _| !within(g, &q));
        s.gone.insert(q, why);
    }
}

/// What holds `p` and what `p` was taken out of (`t = o` with
/// `o = xs.pop()`, `o` forgotten): the holder was taken out of it too, not
/// tied to the buffer, which no longer holds it. Each end: (other end,
/// holds or is `p`, taken: 1 `p` out of it, 2 it out of `p`).
fn taken_through(one: bool, a: (&Place, bool, u8), b: (&Place, bool, u8)) -> Option<Edge> {
    if !one {
        return None;
    }
    let (x1, both1, tk1) = a;
    let (x2, both2, tk2) = b;
    match (both1, tk1, both2, tk2) {
        (true, _, _, 1) | (_, 2, true, _) => Some(Edge::Taken(x1.clone(), x2.clone())),
        (true, _, _, 2) | (_, 1, true, _) => Some(Edge::Taken(x2.clone(), x1.clone())),
        _ => None,
    }
}

/// `p` and every copy of it or place holding it go away, not what it
/// holds (a dropped guard: not its lock). Another guard variable goes only
/// as a direct copy of `p` (`using h = g`): reached through a slot that may
/// hold either (`if c { (a, b) } else { (b, a) }`) it is its own guard, but
/// what holds it may hold `p` and goes.
fn mark_gone_whole(st: &mut St, place: Place, why: Gone, guards: &HashSet<LocalId>) {
    let first = place.clone();
    let mut seen: HashSet<Place> = HashSet::new();
    // (place, a direct copy of `place`)
    let mut work = vec![(place, true)];
    while let Some((q, direct)) = work.pop() {
        if q.1.len() > MAX_DEPTH || st.gone.keys().any(|g| within(&q, g)) || !seen.insert(q.clone())
        {
            continue;
        }
        for e in st.edges.of(q.0) {
            match e {
                Edge::Same(one, two) => {
                    for (other, end) in [(one, two), (two, one)] {
                        if within(&q, end) {
                            let copy = extend(other, &q.1[end.1.len()..]);
                            let whole = direct && q.1.is_empty() && copy.1.is_empty();
                            work.push((copy, whole));
                        }
                    }
                }
                // A coroutine frame holds its arguments one way (`Into`).
                Edge::In(holder, held) | Edge::Into(holder, held) if within(&q, held) => {
                    work.push((holder.clone(), false));
                }
                _ => {}
            }
        }
        if q != first && !direct && q.1.is_empty() && guards.contains(&q.0) {
            continue;
        }
        st.gone.retain(|g, _| !within(g, &q));
        st.gone.insert(q, why);
    }
}

/// Every `a` of an `Into(a, b)` whose `b` went away since `before` goes
/// too, to a fixpoint.
fn holders_go(st: &mut St, before: &BTreeSet<Place>, why: Gone) {
    loop {
        let mut more = Vec::new();
        for es in st.edges.0.values() {
            for e in es {
                if let Edge::Into(holder, held) = e {
                    let new =
                        |x: &Place| st.gone.keys().any(|g| !before.contains(g) && within(x, g));
                    let gone = |x: &Place| st.gone.keys().any(|g| within(x, g));
                    if new(held) && !gone(holder) {
                        more.push(holder.clone());
                    }
                }
            }
        }
        if more.is_empty() {
            return;
        }
        for h in more {
            mark_gone(st, h, why);
        }
    }
}

/// The places that go away with `q` through one edge (not `Taken`).
fn step(s: &St, q: &Place) -> Vec<Place> {
    let mut out = Vec::new();
    for e in s.edges.of(q.0) {
        match e {
            Edge::Same(a, b) => {
                for (x, y) in [(a, b), (b, a)] {
                    if within(q, y) {
                        out.push(extend(x, &q.1[y.1.len()..]));
                    } else if within(y, q) {
                        out.push(x.clone());
                    }
                }
            }
            Edge::In(a, b) => {
                if overlap(q, b) {
                    out.push(a.clone());
                }
                if overlap(q, a) {
                    out.push(b.clone());
                }
            }
            Edge::Into(a, b) => {
                if overlap(q, a) {
                    out.push(b.clone());
                }
            }
            Edge::Taken(..) => {}
        }
    }
    out
}

/// The buffer elements `at` now hold the values flowing in `fl`
/// (`check`: a store through a call; inside a collection's own methods
/// elements move between slots of one buffer).
fn hold_st(s: &mut St, at: &Place, fl: &[Flow], check: bool) {
    if fl.is_empty() {
        return;
    }
    if check {
        let mut probe = St {
            gone: BTreeMap::new(),
            tied: BTreeMap::new(),
            edges: s.edges.clone(),
            dup: BTreeSet::new(),
            stored: BTreeMap::new(),
        };
        mark_gone(&mut probe, at.clone(), Gone::To(Span::DUMMY, What::Mutex));
        if fl
            .iter()
            .any(|(_, q)| probe.gone.keys().any(|g| overlap(g, q)))
        {
            s.dup.insert(at.clone());
        }
    }
    for (k, q) in fl {
        if within(q, at) {
            continue;
        }
        let e = match k {
            Fk::Is(sub) => Edge::Same(extend(at, sub), q.clone()),
            Fk::Both | Fk::Holds => Edge::In(at.clone(), q.clone()),
        };
        if kept(&e) {
            s.edges.insert(e);
        }
    }
}

/// The `ELEM` places of every alias of the buffer whose elements `b`
/// is (`b` itself among them).
fn elem_aliases(s: &St, b: &Place) -> Vec<Place> {
    let Some(b) = elem_of(b) else {
        return vec![b.clone()];
    };
    let holder = (b.0, b.1[..b.1.len() - 1].to_vec());
    let mut out: Vec<Place> = same_reach(s, &holder)
        .into_iter()
        .filter(|(sub, _)| sub.is_empty())
        .map(|(_, h)| extend(&h, &[ELEM]))
        .collect();
    if !out.contains(&b) {
        out.push(b);
    }
    out
}

/// Summary `s`'s edges at a call (`map`: a root to the caller's place),
/// and its stores into a buffer: (its elements, the stored value).
/// `takes`: the value is an element taken out of argument 0's buffer.
fn summary_edges(
    s: &Summary,
    map: &dyn Fn(&Root) -> Option<Place>,
    takes: bool,
) -> (Vec<Edge>, Vec<(Place, Place)>) {
    let mut add: Vec<Edge> = Vec::new();
    let mut stores: Vec<(Place, Place)> = Vec::new();
    for edge in &s.edges {
        let mapped = match edge {
            SEdge::Same(x, y) if takes && (x.0 == RET_ROOT) != (y.0 == RET_ROOT) => {
                let (ret, from) = if x.0 == RET_ROOT { (x, y) } else { (y, x) };
                match (map(ret), map(from).and_then(|p| elem_of(&p))) {
                    (Some(ret), Some(from)) => Some(Edge::Taken(ret, from)),
                    _ => map(x).zip(map(y)).map(|(x, y)| Edge::Same(x, y)),
                }
            }
            SEdge::Same(x, y) => map(x).zip(map(y)).map(|(x, y)| Edge::Same(x, y)),
            SEdge::Into(x, y) => map(x).zip(map(y)).map(|(x, y)| Edge::Into(x, y)),
            SEdge::Taken(x, y) => map(x).zip(map(y)).map(|(x, y)| Edge::Taken(x, y)),
        };
        let Some(mapped) = mapped.filter(|m| m.ends().0 != m.ends().1) else {
            continue;
        };
        if let (SEdge::Same(rx, ry), Edge::Same(x, y)) = (edge, &mapped) {
            if rx.0 != RET_ROOT && ry.0 != RET_ROOT && rx.0 != ry.0 {
                match (elem_of(x), elem_of(y)) {
                    (Some(at), _) => stores.push((at, y.clone())),
                    (None, Some(at)) => stores.push((at, x.clone())),
                    (None, None) => {}
                }
            }
        }
        add.push(mapped);
    }
    (add, stores)
}

/// The places aliased to `start`, not walking past the elements of a
/// buffer outside `own` (those are returned apart): elements read out
/// of one buffer are taken to be distinct objects unless that buffer
/// may hold one twice.
fn reach(s: &St, start: &Place, own: &[Place]) -> (Vec<Place>, Vec<Place>) {
    let (mut inner, mut stops) = (Vec::new(), Vec::new());
    let mut seen: HashSet<Place> = HashSet::new();
    let mut work = vec![start.clone()];
    while let Some(q) = work.pop() {
        if q.1.len() > MAX_DEPTH || !seen.insert(q.clone()) {
            continue;
        }
        let foreign = q.1.contains(&ELEM) && !own.iter().any(|h| within(&q, h));
        if foreign && q != *start {
            stops.push(q);
            continue;
        }
        for e in s.edges.of(q.0) {
            match e {
                Edge::Same(a, b) => {
                    for (x, y) in [(a, b), (b, a)] {
                        if within(&q, y) {
                            work.push(extend(x, &q.1[y.1.len()..]));
                        } else if within(y, &q) {
                            work.push(x.clone());
                        }
                    }
                }
                Edge::In(a, b) => {
                    if overlap(&q, b) {
                        work.push(a.clone());
                    }
                    if overlap(&q, a) {
                        work.push(b.clone());
                    }
                }
                Edge::Into(a, b) | Edge::Taken(a, b) => {
                    if overlap(&q, a) {
                        work.push(b.clone());
                    }
                }
            }
        }
        inner.push(q);
    }
    (inner, stops)
}

/// Storing `v` into the buffer elements `at` makes the buffer hold one
/// object twice: `v` is already among them, or both come from the
/// elements of one other buffer (elements are not told apart by index).
/// `trusted`: a collection's own method, which moves elements between its
/// buffers and copies each element of another buffer once: only a plain
/// value (a parameter, a callback's result) stored twice counts.
fn stores_twice(s: &St, at: &Place, v: &Place, trusted: bool) -> bool {
    let own = elem_aliases(s, at);
    let (a_in, a_stop) = reach(s, at, &own);
    let (v_in, v_stop) = reach(s, v, &own);
    // A call's argument slot is reused on each pass of a loop.
    let moved = |p: &Place| p.1.contains(&ELEM) || (TEMP..TAG_KEY).contains(&p.0.0);
    let inner = v_in.iter().any(|x| {
        a_in.iter()
            .any(|y| overlap(x, y) && !(trusted && (moved(x) || moved(y))))
    });
    inner
        || v_stop
            .iter()
            .any(|x| a_stop.iter().any(|y| overlap(x, y)) && (!trusted || is_dup(s, x)))
}

/// The buffer whose elements `b` is may hold one object twice.
fn is_dup(s: &St, b: &Place) -> bool {
    !s.dup.is_empty()
        && elem_aliases(s, b)
            .iter()
            .any(|x| s.dup.iter().any(|d| within(x, d)))
}

/// The places that are `start ++ sub` itself, through `Same` edges.
fn same_reach(s: &St, start: &Place) -> Vec<(Vec<u32>, Place)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut work = vec![(Vec::new(), start.clone())];
    while let Some((sub, c)) = work.pop() {
        if c.1.len() > MAX_DEPTH || sub.len() > MAX_DEPTH || !seen.insert(c.clone()) {
            continue;
        }
        for e in s.edges.of(c.0) {
            let Edge::Same(a, b) = e else { continue };
            for (x, y) in [(a, b), (b, a)] {
                if within(y, &c) {
                    work.push(([&sub[..], &y.1[c.1.len()..]].concat(), x.clone()));
                } else if within(&c, y) {
                    work.push((sub.clone(), extend(x, &c.1[y.1.len()..])));
                }
            }
        }
        out.push((sub, c));
    }
    out
}

/// A parameter (`RET_ROOT`: the return value) and a path into it.
type Root = (u32, Vec<u32>);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SEdge {
    Same(Root, Root),
    /// The first going away takes the second along.
    Into(Root, Root),
    /// The first was taken out of the buffer elements the second is.
    Taken(Root, Root),
}

/// What a function does with its parameters, as its callers see it.
#[derive(Clone, Debug, Default)]
struct Summary {
    /// The parts of parameters the call hands over.
    gives: BTreeMap<Root, Why>,
    edges: BTreeSet<SEdge>,
    /// `(guard parameter, root)`: tied to that guard.
    ties: BTreeSet<(u32, Root)>,
    /// Exit tag (of the return value; `ANY`: not known) -> the parts of
    /// `gives` handed over on those exits. Empty: every exit hands over
    /// all of them.
    exits: BTreeMap<u32, BTreeSet<Root>>,
    /// Buffer elements that may now hold one object twice.
    dups: BTreeSet<Root>,
    /// The call may hand something over (to a channel, a lock).
    sends: bool,
    /// Parameter parts the body uses.
    uses: BTreeSet<Root>,
    /// Buffer elements of parameters whose taken-out elements the call
    /// hands over.
    sent_from: BTreeSet<Root>,
    /// Parts of `ref` parameters handed over and then given a new value on
    /// every path that hands them over: the caller's variable is usable.
    renews: BTreeSet<Root>,
    /// Parameters the call keeps past it: stored into another parameter
    /// (a field, a `Cell`, a capture) or into what other threads reach.
    keeps: BTreeSet<u32>,
}

impl Summary {
    fn is_empty(&self) -> bool {
        self.gives.is_empty()
            && self.edges.is_empty()
            && self.ties.is_empty()
            && self.dups.is_empty()
            && !self.sends
            && self.uses.is_empty()
            && self.renews.is_empty()
            && self.keeps.is_empty()
    }

    /// The same facts (which hand-off is named does not matter).
    fn same_facts(&self, o: &Self) -> bool {
        self.gives.keys().eq(o.gives.keys())
            && self.edges == o.edges
            && self.ties == o.ties
            && self.exits == o.exits
            && self.dups == o.dups
            && self.sends == o.sends
            && self.uses == o.uses
            && self.sent_from == o.sent_from
            && self.renews == o.renews
            && self.keeps == o.keeps
    }

    /// `exits`, with every exit kind spelled out.
    fn exit_kinds(&self) -> BTreeMap<u32, BTreeSet<Root>> {
        if self.exits.is_empty() {
            BTreeMap::from([(ANY, self.gives.keys().cloned().collect())])
        } else {
            self.exits.clone()
        }
    }

    /// Drop `exits` when no exit of a known tag hands over less.
    fn trim(&mut self) {
        let any = self.exits.get(&ANY).cloned().unwrap_or_default();
        let less = self
            .exits
            .iter()
            .any(|(t, r)| *t != ANY && r.union(&any).count() < self.gives.len());
        if !less {
            self.exits.clear();
        }
    }

    fn roots(&self) -> impl Iterator<Item = &Root> {
        self.gives
            .keys()
            .chain(self.edges.iter().flat_map(|e| {
                let (SEdge::Same(a, b) | SEdge::Into(a, b) | SEdge::Taken(a, b)) = e;
                [a, b]
            }))
            .chain(self.ties.iter().map(|(_, r)| r))
            .chain(self.dups.iter())
    }

    /// Parameter `i` of `self` as parameter `map(i)` (`None`: dropped).
    fn remap(&self, map: &dyn Fn(u32) -> Option<u32>) -> Self {
        let root = |r: &Root| {
            if r.0 == RET_ROOT || r.0 == WORLD_ROOT {
                Some(r.clone())
            } else {
                map(r.0).map(|i| (i, r.1.clone()))
            }
        };
        Self {
            gives: self
                .gives
                .iter()
                .filter_map(|(r, w)| Some((root(r)?, w.clone())))
                .collect(),
            edges: self
                .edges
                .iter()
                .filter_map(|e| match e {
                    SEdge::Same(a, b) => Some(SEdge::Same(root(a)?, root(b)?)),
                    SEdge::Into(a, b) => Some(SEdge::Into(root(a)?, root(b)?)),
                    SEdge::Taken(a, b) => Some(SEdge::Taken(root(a)?, root(b)?)),
                })
                .collect(),
            ties: self
                .ties
                .iter()
                .filter_map(|(g, r)| Some((map(*g)?, root(r)?)))
                .collect(),
            exits: self
                .exits
                .iter()
                .map(|(t, rs)| (*t, rs.iter().filter_map(root).collect()))
                .collect(),
            dups: self.dups.iter().filter_map(root).collect(),
            sends: self.sends,
            uses: self.uses.iter().filter_map(root).collect(),
            sent_from: self.sent_from.iter().filter_map(root).collect(),
            renews: self.renews.iter().filter_map(root).collect(),
            keeps: self.keeps.iter().filter_map(|&i| map(i)).collect(),
        }
    }

    fn merge(&mut self, o: &Self) {
        let mut exits = self.exit_kinds();
        for (t, rs) in o.exit_kinds() {
            exits.entry(t).or_default().extend(rs);
        }
        for (r, w) in &o.gives {
            self.gives.entry(r.clone()).or_insert_with(|| w.clone());
        }
        self.edges.extend(o.edges.iter().cloned());
        self.ties.extend(o.ties.iter().cloned());
        self.dups.extend(o.dups.iter().cloned());
        self.sends |= o.sends;
        self.uses.extend(o.uses.iter().cloned());
        self.sent_from.extend(o.sent_from.iter().cloned());
        self.keeps.extend(o.keeps.iter().copied());
        // A part renewed by every candidate.
        self.renews = self.renews.intersection(&o.renews).cloned().collect();
        self.exits = exits;
        self.trim();
    }

    /// One line per fact, fields split by tabs (ROKHIR).
    fn encode(&self) -> String {
        let root = |r: &Root| {
            let head = match r.0 {
                RET_ROOT => "r".to_owned(),
                WORLD_ROOT => "w".to_owned(),
                i => i.to_string(),
            };
            std::iter::once(head)
                .chain(r.1.iter().map(u32::to_string))
                .collect::<Vec<_>>()
                .join(".")
        };
        let mut out = String::new();
        for (r, w) in &self.gives {
            out.push_str(&format!(
                "g\t{}\t{}\t{}\t{}\t{}\n",
                root(r),
                w.what.tag(),
                w.line,
                w.func,
                w.file
            ));
        }
        for e in &self.edges {
            let (k, a, b) = match e {
                SEdge::Same(a, b) => ("s", a, b),
                SEdge::Into(a, b) => ("i", a, b),
                SEdge::Taken(a, b) => ("k", a, b),
            };
            out.push_str(&format!("{k}\t{}\t{}\n", root(a), root(b)));
        }
        for (g, r) in &self.ties {
            out.push_str(&format!("t\t{g}\t{}\n", root(r)));
        }
        for r in &self.dups {
            out.push_str(&format!("d\t{}\n", root(r)));
        }
        if self.sends {
            out.push_str("h\n");
        }
        for i in &self.keeps {
            out.push_str(&format!("p\t{i}\n"));
        }
        for (k, set) in [
            ("u", &self.uses),
            ("f", &self.sent_from),
            ("n", &self.renews),
        ] {
            for r in set {
                out.push_str(&format!("{k}\t{}\n", root(r)));
            }
        }
        for (t, rs) in &self.exits {
            let t = if *t == ANY {
                "*".to_owned()
            } else {
                t.to_string()
            };
            if rs.is_empty() {
                out.push_str(&format!("x\t{t}\n"));
            }
            for r in rs {
                out.push_str(&format!("x\t{t}\t{}\n", root(r)));
            }
        }
        out
    }

    fn decode(text: &str) -> Option<Self> {
        let root = |s: &str| -> Option<Root> {
            let mut it = s.split('.');
            let head = it.next()?;
            let i = match head {
                "r" => RET_ROOT,
                "w" => WORLD_ROOT,
                _ => head.parse().ok()?,
            };
            Some((i, it.map(str::parse).collect::<Result<_, _>>().ok()?))
        };
        let mut s = Self::default();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f[..] {
                ["g", r, what, l, func, file] => {
                    s.gives.insert(
                        root(r)?,
                        Why {
                            what: What::from_tag(what)?,
                            func: func.to_owned(),
                            file: file.to_owned(),
                            line: l.parse().ok()?,
                        },
                    );
                }
                ["s", a, b] => {
                    s.edges.insert(SEdge::Same(root(a)?, root(b)?));
                }
                ["i", a, b] => {
                    s.edges.insert(SEdge::Into(root(a)?, root(b)?));
                }
                ["k", a, b] => {
                    s.edges.insert(SEdge::Taken(root(a)?, root(b)?));
                }
                ["d", r] => {
                    s.dups.insert(root(r)?);
                }
                ["h"] => s.sends = true,
                ["p", i] => {
                    s.keeps.insert(i.parse().ok()?);
                }
                ["u", r] => {
                    s.uses.insert(root(r)?);
                }
                ["f", r] => {
                    s.sent_from.insert(root(r)?);
                }
                ["n", r] => {
                    s.renews.insert(root(r)?);
                }
                ["t", g, r] => {
                    s.ties.insert((g.parse().ok()?, root(r)?));
                }
                ["x", t, ref rest @ ..] if rest.len() <= 1 => {
                    let t = if t == "*" { ANY } else { t.parse().ok()? };
                    let rs = s.exits.entry(t).or_default();
                    if let [r] = rest {
                        rs.insert(root(r)?);
                    }
                }
                _ => return None,
            }
        }
        Some(s)
    }
}

/// The name prefix of a ROKHIR entry for the functions used as values
/// with these parameter types (`e`: a closure, its environment first).
const FN_VALUE: &str = "#fn";

/// A function used as a value: a call through a function value of its
/// parameter types may reach it.
struct Cand {
    /// A closure: parameter 0 is its environment.
    env: bool,
    keys: Vec<String>,
    from: CandFrom,
}

enum CandFrom {
    Fn(FunctionId),
    Stored(Rc<Summary>),
}

/// How a value flows when stored at a place `P`.
#[derive(Clone)]
enum Fk {
    /// `P ++ sub` is this object.
    Is(Vec<u32>),
    /// Held inside `P`, both going away together.
    Both,
    /// Held inside `P`: it goes away with `P`.
    Holds,
}

type Flow = (Fk, Place);

/// The place `e` reads as is (`x`, `x.f.0`, `x` wrapped into a sum
/// slot or cast).
fn place(e: &HirExpr) -> Option<Place> {
    match &e.kind {
        HirExprKind::Local(l) => Some((*l, Vec::new())),
        HirExprKind::FieldRead {
            target,
            field_index,
            ..
        } => place(target).map(|p| extend(&p, &[*field_index])),
        HirExprKind::TupleField { target, index, .. } => {
            place(target).map(|p| extend(&p, &[index | TUPLE]))
        }
        HirExprKind::SumWrap { value: Some(x), .. } | HirExprKind::Cast { operand: x, .. } => {
            place(x)
        }
        HirExprKind::Block(b) if b.stmts.is_empty() => place(b.trailing.as_ref()?),
        _ => None,
    }
}

struct Frame<'h> {
    label: Option<&'h str>,
    is_loop: bool,
    brk: Cur,
    cont: Cur,
    /// Where the tag of a `break`'s value goes.
    dest: Option<Place>,
}

/// What every walk shares.
struct Checker<'a> {
    env: &'a Env<'a>,
    ctx: &'a LowerCtx<'a>,
    sync: &'a str,
    fns: HashMap<FunctionId, &'a HirFunction>,
    ops: HashMap<FunctionId, Op>,
    /// `TryAsyncFuture::poll` and its instances.
    polls: HashSet<FunctionId>,
    /// `TAKES`, `PASSES` and `PAR` functions and their instances.
    takes: HashSet<FunctionId>,
    passes: HashSet<FunctionId>,
    par: HashSet<FunctionId>,
    literals: HashSet<FunctionId>,
    clears: HashSet<FunctionId>,
    copies: HashSet<FunctionId>,
    /// `TryAsyncFuture` and its instances.
    futures: HashSet<StructId>,
    sums: RefCell<HashMap<FunctionId, Rc<Summary>>>,
    /// A closure's or coroutine body's function -> the function it is in.
    parent: HashMap<FunctionId, FunctionId>,
    /// A call that handed something over (its span start) -> the callee.
    callees: RefCell<HashMap<(u32, u32), String>>,
    /// `ThreadLocal::with` closures -> the parameter holding the value.
    views: HashMap<FunctionId, usize>,
    /// Where views a callee keeps were taken out, and those calls: what
    /// follows from them is not reported again.
    kept_at: RefCell<HashSet<(u32, u32)>>,
    cands: HashMap<usize, Vec<Cand>>,
    tracked_memo: RefCell<HashMap<HirTy, bool>>,
    whys: RefCell<(Vec<Why>, HashMap<Why, u32>)>,
    /// A `Mutex::new` / `RwLock::new` / `Handoff::new` call (its span
    /// start) -> the variable that holds what it made, and its block.
    owners: RefCell<HashMap<(u32, u32), (String, usize)>>,
    /// A hand-off call (its span start) of a variable or field -> it as
    /// written, and its type.
    direct: RefCell<HashMap<(u32, u32), (String, String)>>,
    unsafe_regions: RefCell<HashMap<u32, Vec<(u32, u32)>>>,
    /// A hand-off call (its span start) -> its argument as written.
    handed: RefCell<HashMap<(u32, u32), String>>,
    /// Errors (by position) about a value a call handed over, with the
    /// call: dropped when an error inside the call (a closure's) is the
    /// cause.
    via_at: RefCell<HashMap<(u32, u32), Span>>,
}

impl Checker<'_> {
    /// A value of this type is not `Sync` (the only ones followed).
    fn tracked(&self, t: &HirTy) -> bool {
        if matches!(
            t,
            HirTy::Unknown | HirTy::Prim(_) | HirTy::Unit | HirTy::Never | HirTy::None
        ) {
            return false;
        }
        if let Some(&v) = self.tracked_memo.borrow().get(t) {
            return v;
        }
        // A buffer is shared by the views of it (`Slice`), whatever its
        // elements.
        let v = !super::bounds::has_param(self.ctx, t)
            && (matches!(t, HirTy::GcBuf(_)) || !self.ctx.has_trait(t, self.sync));
        self.tracked_memo.borrow_mut().insert(t.clone(), v);
        v
    }

    fn why_id(&self, w: Why) -> u32 {
        let mut t = self.whys.borrow_mut();
        if let Some(&i) = t.1.get(&w) {
            return i;
        }
        let i = u32::try_from(t.0.len()).unwrap_or(u32::MAX);
        t.0.push(w.clone());
        t.1.insert(w, i);
        i
    }

    fn summary(&self, id: FunctionId) -> Option<Rc<Summary>> {
        self.sums.borrow().get(&id).cloned()
    }

    fn src(&self, s: Span) -> Option<&str> {
        let (_, text) = self.env.sources.file(s.file)?;
        text.get(s.start as usize..s.end as usize)
    }

    fn file_name(&self, file: u32) -> String {
        self.env.sources.path(file).unwrap_or_default().to_owned()
    }

    /// `s` lies inside an `unsafe { .. }` block or an `unsafe fn` body.
    fn in_unsafe(&self, s: Span) -> bool {
        let mut memo = self.unsafe_regions.borrow_mut();
        let regions = memo.entry(s.file).or_insert_with(|| {
            self.env
                .sources
                .file(s.file)
                .map(|(_, text)| unsafe_regions(text))
                .unwrap_or_default()
        });
        regions.iter().any(|&(a, b)| a <= s.start && s.end <= b)
    }
}

/// The byte ranges of `unsafe { .. }` blocks and `unsafe fn` bodies.
fn unsafe_regions(text: &str) -> Vec<(u32, u32)> {
    use rokc_lexer::TokenKind as K;
    let (tokens, _) = rokc_lexer::Lexer::new(text, String::new()).lex_all();
    let close = |open: usize| {
        let mut depth = 0i32;
        tokens[open..].iter().find_map(|t| {
            match t.kind {
                K::LBrace => depth += 1,
                K::RBrace => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(t.span.end)
        })
    };
    let mut out = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if t.kind != K::Unsafe {
            continue;
        }
        let open = match tokens.get(i + 1).map(|t| &t.kind) {
            Some(K::LBrace) => Some(i + 1),
            // The body: the first `{` outside the parameter list.
            Some(K::Fn) => {
                let mut depth = 0i32;
                tokens[i + 2..]
                    .iter()
                    .position(|t| {
                        match t.kind {
                            K::LParen | K::LBracket => depth += 1,
                            K::RParen | K::RBracket => depth -= 1,
                            _ => {}
                        }
                        depth == 0 && matches!(t.kind, K::LBrace | K::Semi)
                    })
                    .map(|k| i + 2 + k)
                    .filter(|&k| tokens[k].kind == K::LBrace)
            }
            _ => None,
        };
        if let Some(o) = open {
            if let Some(end) = close(o) {
                out.push((tokens[o].span.start, end));
            }
        }
    }
    out
}

/// The function as written: `store`, `Bag::store` (a closure or an
/// `async` body: the function it is in).
fn shown_fn(c: &Checker<'_>, f: &HirFunction) -> String {
    let mut f = f;
    while let Some(p) = c.parent.get(&f.id).and_then(|p| c.fns.get(p)) {
        f = p;
    }
    let path = f.facts.path(&f.name);
    let path = path.split("__by_").next().unwrap_or(path);
    let leaf = path.rsplit("::").next().unwrap_or(path);
    // A method of a struct of this build: `Struct<id>__name`.
    if let Some((id, m)) = leaf
        .strip_prefix("Struct")
        .and_then(|r| r.split_once("__"))
        .filter(|(id, _)| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    {
        let structs = c.env.defs.structs.borrow();
        if let Some(s) = id.parse::<usize>().ok().and_then(|i| structs.get(i)) {
            let ty = s.name.split(['<', '_']).next().unwrap_or(&s.name);
            let ty = ty.rsplit("::").next().unwrap_or(ty);
            return format!("{ty}::{m}");
        }
    }
    leaf.replace('.', "::").replace("__", "::")
}

/// Check every body; report each use after hand-off. `stored`: the
/// summaries of prebuilt libraries (ROKHIR); returned: those of this
/// build's bodies, for its ROKHIR when `export`.
pub(super) fn check(
    env: &Env<'_>,
    functions: &[HirFunction],
    stored: &[(String, String)],
    export: bool,
) -> Vec<(String, String)> {
    let sync = env.names.lang_item("sync_trait");
    if sync.is_empty() {
        return Vec::new();
    }
    let extras = env.sink.extras.borrow();
    let all: Vec<&HirFunction> = functions.iter().chain(extras.iter()).collect();
    let structs = env.defs.structs.borrow();
    let guard = |f: &HirFunction| {
        let Some(HirTy::Struct(id)) = f.params.first().filter(|p| p.name == "self").map(|p| &p.ty)
        else {
            return false;
        };
        structs.get(id.0 as usize).is_some_and(|s| {
            let base = s.name.split("__").next().unwrap_or_default();
            let base = base.split('<').next().unwrap_or_default();
            GUARDS.iter().any(|(g, _)| *g == base)
        })
    };
    let mut ops = HashMap::new();
    let mut polls = HashSet::new();
    let (mut takes, mut passes, mut par) = (HashSet::new(), HashSet::new(), HashSet::new());
    let mut literals = HashSet::new();
    let (mut clears, mut copies) = (HashSet::new(), HashSet::new());
    for f in &all {
        if let Some(op) = op_of(&f.name, guard(f)) {
            ops.insert(f.id, op);
        }
        if is_fn(&f.name, POLL) {
            polls.insert(f.id);
        }
        // `xs += ys` (`AddAssign<List<T>>`): both sides the same list type.
        if f.name.contains("_add_assign") && f.params.len() == 2 && f.params[0].ty == f.params[1].ty
        {
            copies.insert(f.id);
        }
        for (set, names) in [
            (&mut takes, TAKES),
            (&mut passes, PASSES),
            (&mut par, PAR),
            (&mut literals, LITERALS),
            (&mut clears, CLEARS),
            (&mut copies, COPIES),
        ] {
            if names.iter().any(|n| is_fn(&f.name, n)) {
                set.insert(f.id);
            }
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    let futures = structs
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            let base = s.name.split("__").next().unwrap_or_default();
            base.split('<').next() == Some(FUTURE)
        })
        .map(|(i, _)| StructId(i as u32))
        .collect();
    drop(structs);
    let no_subst = HashMap::new();
    let ctx = LowerCtx::new(env, &no_subst, None, Span::DUMMY);
    let by_name: HashMap<&str, &str> = stored
        .iter()
        .map(|(n, s)| (n.as_str(), s.as_str()))
        .collect();
    let mut sums = HashMap::new();
    for f in all.iter().filter(|f| f.body.is_none()) {
        if let Some(s) = by_name
            .get(f.name.as_str())
            .and_then(|t| Summary::decode(t))
        {
            sums.insert(f.id, Rc::new(s));
        }
    }
    let c = Checker {
        env,
        ctx: &ctx,
        sync,
        fns: all.iter().map(|f| (f.id, *f)).collect(),
        ops,
        polls,
        takes,
        passes,
        par,
        literals,
        clears,
        copies,
        futures,
        sums: RefCell::new(sums),
        parent: parents(&all),
        callees: RefCell::default(),
        views: with_closures(&all),
        kept_at: RefCell::default(),
        cands: HashMap::new(),
        tracked_memo: RefCell::default(),
        whys: RefCell::default(),
        owners: RefCell::default(),
        direct: RefCell::default(),
        unsafe_regions: RefCell::default(),
        handed: RefCell::default(),
        via_at: RefCell::default(),
    };
    let mut c = c;
    c.cands = candidates(&c, &all, stored);
    let c = c;

    // The bodies worth a walk: one holding a value that is not `Sync`.
    let walked: Vec<&HirFunction> = all
        .iter()
        .copied()
        .filter(|f| {
            f.body.is_some()
                && !c.ops.contains_key(&f.id)
                && (c.tracked(&f.ret_ty) || f.locals.iter().any(|l| c.tracked(&l.ty)))
        })
        .collect();

    // Summaries to a fixpoint, callees first.
    let order = postorder(&walked);
    let mut readers: HashMap<FunctionId, HashSet<FunctionId>> = HashMap::new();
    let mut queue: std::collections::VecDeque<FunctionId> = order.iter().copied().collect();
    let mut queued: HashSet<FunctionId> = queue.iter().copied().collect();
    let mut budget = 40 * order.len() + 1000;
    // A function's last walk saw its callees' final summaries (a change
    // walks its readers again): its errors are those of that walk.
    let mut found: HashMap<FunctionId, Vec<RokDiagnostic>> = HashMap::new();
    while let Some(id) = queue.pop_front() {
        queued.remove(&id);
        let f = c.fns[&id];
        let ((s, deps), errs) = walk(&c, f);
        found.insert(id, errs);
        if budget == 0 {
            continue;
        }
        budget -= 1;
        for d in deps {
            readers.entry(d).or_default().insert(id);
        }
        let changed = c
            .summary(id)
            .is_none_or(|old| !(old.same_facts(&s) || old.is_empty() && s.is_empty()));
        if changed {
            c.sums.borrow_mut().insert(id, Rc::new(s));
            for r in readers.get(&id).into_iter().flatten() {
                if queued.insert(*r) {
                    queue.push_back(*r);
                }
            }
        }
    }
    let mut diags: Vec<RokDiagnostic> = walked
        .iter()
        .filter_map(|f| found.remove(&f.id))
        .flatten()
        .collect();
    let mut seen = HashSet::new();
    diags.retain(|d: &RokDiagnostic| {
        let at = d.primary.as_ref().map(|l| (l.span.file, l.span.start));
        seen.insert((at, d.message.clone()))
    });
    // A value a call handed over because of an error reported inside the
    // call (a closure argument's own): that error is the one to fix.
    let via = c.via_at.borrow();
    let spans: Vec<Span> = diags
        .iter()
        .filter_map(|d| d.primary.as_ref().map(|l| l.span))
        .collect();
    diags.retain(|d| {
        let Some(at) = d.primary.as_ref().map(|l| l.span) else {
            return true;
        };
        let Some(site) = via.get(&(at.file, at.start)) else {
            return true;
        };
        !spans
            .iter()
            .any(|x| x.file == site.file && x.start >= site.start && x.end <= site.end && *x != at)
    });
    env.sink.diagnostics.borrow_mut().extend(diags);

    if !export {
        return Vec::new();
    }
    let sums = c.sums.borrow();
    let mut out: Vec<(String, String)> = walked
        .iter()
        .filter(|f| !f.name.contains(rokc_ir_common::MONO_MARK))
        .filter_map(|f| {
            let s = sums.get(&f.id).filter(|s| !s.is_empty())?;
            Some((f.name.clone(), s.encode()))
        })
        .collect();
    // The functions used as values, by parameter types.
    let mut by_sig: BTreeMap<String, Summary> = BTreeMap::new();
    for cs in c.cands.values() {
        for k in cs {
            let CandFrom::Fn(id) = &k.from else { continue };
            let Some(s) = sums.get(id).filter(|s| !s.is_empty()) else {
                continue;
            };
            let name = std::iter::once(format!("{FN_VALUE}{}", if k.env { "e" } else { "" }))
                .chain(k.keys.iter().cloned())
                .collect::<Vec<_>>()
                .join("\u{1f}");
            by_sig.entry(name).or_default().merge(s);
        }
    }
    out.extend(by_sig.into_iter().map(|(n, s)| (n, s.encode())));
    out
}

/// The functions used as values (and the stored ones of prebuilt
/// libraries), by their parameter count without the environment.
fn candidates(
    c: &Checker<'_>,
    all: &[&HirFunction],
    stored: &[(String, String)],
) -> HashMap<usize, Vec<Cand>> {
    let mut used: BTreeMap<FunctionId, bool> = BTreeMap::new();
    for f in all {
        let Some(body) = &f.body else { continue };
        crate::each_block(body, &mut |e| match &e.kind {
            HirExprKind::FnRef(id) => {
                used.entry(*id).or_insert(false);
            }
            HirExprKind::ClosureValue { fn_id, .. } => {
                used.insert(*fn_id, true);
            }
            _ => {}
        });
    }
    let mut out: HashMap<usize, Vec<Cand>> = HashMap::new();
    for (id, env) in used {
        let Some(f) = c.fns.get(&id) else { continue };
        if c.ops.contains_key(&id) {
            continue;
        }
        let keys: Vec<String> = f
            .params
            .iter()
            .skip(usize::from(env))
            .map(|p| p.ty.stable_key())
            .collect();
        out.entry(keys.len()).or_default().push(Cand {
            env,
            keys,
            from: CandFrom::Fn(id),
        });
    }
    for (name, text) in stored {
        let mut parts = name.split('\u{1f}');
        let Some(head) = parts.next().and_then(|h| h.strip_prefix(FN_VALUE)) else {
            continue;
        };
        let Some(s) = Summary::decode(text) else {
            continue;
        };
        let keys: Vec<String> = parts.map(str::to_owned).collect();
        out.entry(keys.len()).or_default().push(Cand {
            env: head == "e",
            keys,
            from: CandFrom::Stored(Rc::new(s)),
        });
    }
    out
}

/// The functions passed to `ThreadLocal::with` (a closure, a function
/// name, or a variable bound to one) -> their value parameter.
fn with_closures<'h>(all: &[&'h HirFunction]) -> HashMap<FunctionId, usize> {
    let ids: HashMap<FunctionId, &str> = all.iter().map(|f| (f.id, f.name.as_str())).collect();
    let mut out = HashMap::new();
    for f in all {
        let Some(body) = &f.body else { continue };
        let mut inits: HashMap<LocalId, &'h HirExpr> = HashMap::new();
        let mut note = |b: &'h HirBlock| {
            for s in &b.stmts {
                if let HirStmt::Let { local, init } = s {
                    inits.insert(*local, init);
                }
            }
        };
        note(body);
        crate::each_block(body, &mut |e| {
            if let HirExprKind::Block(b) = &e.kind {
                note(b);
            }
        });
        crate::each_block(body, &mut |e| {
            let HirExprKind::Call { callee, args } = &e.kind else {
                return;
            };
            if !ids.get(callee).is_some_and(|n| is_fn(n, WITH)) {
                return;
            }
            let mut x = args.get(1);
            for _ in 0..8 {
                match x.map(|x| &x.kind) {
                    Some(HirExprKind::ClosureValue { fn_id, .. }) => {
                        out.insert(*fn_id, 1);
                        return;
                    }
                    Some(HirExprKind::FnRef(id)) => {
                        out.insert(*id, 0);
                        return;
                    }
                    Some(HirExprKind::Local(l)) => x = inits.get(l).copied(),
                    Some(HirExprKind::Block(b)) if b.stmts.is_empty() => x = b.trailing.as_ref(),
                    _ => return,
                }
            }
        });
    }
    out
}

/// Parameter `start` is kept past the call in `exit`: the whole of it, or
/// a field of it, is the same object as a part of another parameter or
/// as what other threads reach. Elements shared both ways (`xs.extend(ys)`)
/// and the return value do not count: the caller follows those.
fn keeps_param(
    exit: &St,
    start: &Place,
    own: u32,
    root_of: &dyn Fn(LocalId) -> Option<u32>,
) -> bool {
    let mut seen = HashSet::new();
    let mut work = vec![(Vec::new(), start.clone())];
    while let Some((sub, c)) = work.pop() {
        if c.1.len() > MAX_DEPTH || sub.len() > MAX_DEPTH || !seen.insert(c.clone()) {
            continue;
        }
        if let Some(r) = root_of(c.0).filter(|&r| r != own) {
            if r == RET_ROOT {
                continue;
            }
            if r == WORLD_ROOT || !c.1.is_empty() && !sub.contains(&ELEM) {
                return true;
            }
        }
        for e in exit.edges.of(c.0) {
            let Edge::Same(a, b) = e else { continue };
            for (x, y) in [(a, b), (b, a)] {
                if within(y, &c) {
                    work.push(([&sub[..], &y.1[c.1.len()..]].concat(), x.clone()));
                } else if within(&c, y) {
                    work.push((sub.clone(), extend(x, &c.1[y.1.len()..])));
                }
            }
        }
    }
    false
}

/// `t` (`g.get()`, `xs`) as a deep copy: `g.get_clone()`, `xs.clone()`.
fn deep_copy(t: &str) -> String {
    t.strip_suffix(".get()")
        .map_or_else(|| format!("{t}.clone()"), |g| format!("{g}.get_clone()"))
}

/// Each closure's and coroutine body's function -> the one making it.
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

/// `fns` callees before callers (by direct calls).
fn postorder(fns: &[&HirFunction]) -> Vec<FunctionId> {
    let ids: HashSet<FunctionId> = fns.iter().map(|f| f.id).collect();
    let callees: HashMap<FunctionId, Vec<FunctionId>> = fns
        .iter()
        .map(|f| {
            let mut v = Vec::new();
            if let Some(b) = &f.body {
                crate::each_block(b, &mut |e| {
                    if let HirExprKind::Call { callee: id, .. }
                    | HirExprKind::CoroNew { body_fn: id, .. }
                    | HirExprKind::ClosureValue { fn_id: id, .. } = &e.kind
                    {
                        if ids.contains(id) {
                            v.push(*id);
                        }
                    }
                });
            }
            (f.id, v)
        })
        .collect();
    let mut done = HashSet::new();
    let mut out = Vec::new();
    for f in fns {
        let mut stack = vec![(f.id, 0usize)];
        while let Some((id, i)) = stack.pop() {
            if i == 0 && !done.insert(id) {
                continue;
            }
            let next = callees[&id].get(i).copied();
            match next {
                Some(n) => {
                    stack.push((id, i + 1));
                    if !done.contains(&n) {
                        stack.push((n, 0));
                    }
                }
                None => out.push(id),
            }
        }
    }
    out
}

type WalkOut = ((Summary, HashSet<FunctionId>), Vec<RokDiagnostic>);

fn walk(c: &Checker<'_>, f: &HirFunction) -> WalkOut {
    let mut w = Walk {
        c,
        f,
        cur: Cur::start(),
        frames: Vec::new(),
        exits: BTreeMap::new(),
        mixed: false,
        keys: HashMap::new(),
        dests: Vec::new(),
        pinned: HashSet::new(),
        reported: HashSet::new(),
        temps: HashMap::new(),
        temp_tys: HashMap::new(),
        results: HashMap::new(),
        calls: Vec::new(),
        scopes: Vec::new(),
        returning: false,
        sends: false,
        uses: BTreeSet::new(),
        sent_from: BTreeSet::new(),
        renewed: BTreeSet::new(),
        not_renewed: BTreeSet::new(),
        deps: HashSet::new(),
        diags: Vec::new(),
    };
    // Each parameter as it came in: what the caller passed.
    for (i, p) in f.params.iter().enumerate() {
        if c.tracked(&p.ty) {
            let s = shadow(i);
            for st in w.cur.states() {
                st.edges
                    .insert(Edge::Same((s, Vec::new()), (p.local, Vec::new())));
            }
        }
    }
    let body = f.body.as_ref().expect("walked bodies have one");
    crate::each_block(body, &mut |e| {
        if let HirExprKind::AddrOf { place } = &e.kind {
            let mut x = &**place;
            while let HirExprKind::FieldRead { target: t, .. }
            | HirExprKind::TupleField { target: t, .. }
            | HirExprKind::ArrayIndex { array: t, .. } = &x.kind
            {
                x = t;
            }
            if let HirExprKind::Local(l) = x.kind {
                w.pinned.insert(l);
            }
        }
    });
    // A `with` closure's value is tied to `VIEW` for the whole body.
    let view = c
        .views
        .get(&f.id)
        .and_then(|&i| f.params.get(i).map(|p| (i, p)));
    let view = view.filter(|(_, p)| c.tracked(&p.ty));
    if let Some((_, p)) = view {
        w.tie(VIEW, (p.local, Vec::new()), f.span, Tie::Taken);
        // An `async` closure's body is its future: `Future { coro_new .. }`.
        let coro = |e: &HirExpr| match &e.kind {
            HirExprKind::CoroNew { .. } => true,
            HirExprKind::StructInit { fields, .. } => fields
                .iter()
                .any(|x| matches!(x.kind, HirExprKind::CoroNew { .. })),
            _ => false,
        };
        if f.is_async || body.trailing.as_ref().is_some_and(coro) {
            w.diags.push(
                RokDiagnostic::error(
                    Code::THREAD_LOCAL_ESCAPE,
                    f.span,
                    "an `async` closure can't be given to `with`: its future would keep this \
                     thread's `ThreadLocal` value past the call, and may resume on another \
                     thread",
                )
                .with_note(
                    "help: give `with` a plain closure, and `await` outside it on a copy \
                     (`.clone()`) or on what you computed from the value (stdlib 3.4.4)",
                ),
            );
        }
    }
    w.block_to(body, Some(&RET_TAG));
    if let (Some(t), false) = (&body.trailing, w.cur.is_empty()) {
        w.store(&(RET, Vec::new()), t, false);
        if view.is_some() {
            w.escapes_guard(t, Exit::Return);
        }
    }
    w.note_renews();
    w.exit_with(Some(&RET_TAG));
    let s = w.summarize();
    #[allow(clippy::cast_possible_truncation)]
    if let Some((i, p)) = view {
        if s.keeps.contains(&(i as u32)) && !w.diags.iter().any(RokDiagnostic::is_error) {
            w.view_kept(&p.name, f.span);
        }
    }
    ((s, w.deps), w.diags)
}

#[allow(clippy::cast_possible_truncation)]
const fn shadow(i: usize) -> LocalId {
    LocalId(SHADOW + i as u32)
}

/// Where a returned (or a coroutine's step) value's tag goes.
const RET_TAG: Place = (LocalId(0x7fff_fffe), Vec::new());

struct Walk<'c, 'h> {
    c: &'c Checker<'c>,
    f: &'h HirFunction,
    cur: Cur,
    frames: Vec<Frame<'h>>,
    /// The states at the returns, by the returned value's tag.
    exits: BTreeMap<u32, St>,
    /// A coroutine step before the last one may be an error.
    mixed: bool,
    /// The places holding the tags of values being tested or stored (by
    /// the expression's address).
    keys: HashMap<usize, LocalId>,
    /// Calls (by address) whose value's tag goes to a place, innermost
    /// last.
    dests: Vec<(usize, Place)>,
    /// Locals whose address is taken: no tag is kept for them.
    pinned: HashSet<LocalId>,
    reported: HashSet<(Place, u32, u32)>,
    /// A call's (by address) argument / result slot -> its local.
    temps: HashMap<(usize, u32), LocalId>,
    temp_tys: HashMap<LocalId, HirTy>,
    /// What a call's value carries (by address).
    results: HashMap<usize, Vec<Flow>>,
    /// The calls being walked, innermost last (for the hints).
    calls: Vec<&'h HirExpr>,
    /// The blocks being walked (by address), innermost last.
    scopes: Vec<usize>,
    /// Walking a `return`'s value.
    returning: bool,
    /// This body may hand something over.
    sends: bool,
    /// Summary facts gathered on the way (`Summary::uses` / `sent_from`),
    /// and `ref` parameter parts renewed / not renewed at a return.
    uses: BTreeSet<Root>,
    sent_from: BTreeSet<Root>,
    renewed: BTreeSet<Root>,
    not_renewed: BTreeSet<Root>,
    /// The summaries this walk read.
    deps: HashSet<FunctionId>,
    diags: Vec<RokDiagnostic>,
}

impl<'h> Walk<'_, 'h> {
    fn name(&self, l: LocalId) -> String {
        self.f
            .locals
            .iter()
            .find(|x| x.id == l)
            .and_then(|x| x.name.clone())
            .filter(|n| !n.starts_with('#'))
            .unwrap_or_else(|| "the value".to_owned())
    }

    /// `e` (a place) as written: `s.items`, `t.0`.
    fn show(&self, e: &HirExpr) -> String {
        match &e.kind {
            HirExprKind::Local(l) => self.name(*l),
            HirExprKind::FieldRead {
                target,
                struct_id,
                field_index,
                ..
            } => {
                let structs = self.c.env.defs.structs.borrow();
                let f = structs
                    .get(struct_id.0 as usize)
                    .and_then(|s| s.fields.get(*field_index as usize))
                    .map_or_else(|| field_index.to_string(), |f| f.name.clone());
                drop(structs);
                format!("{}.{f}", self.show(target))
            }
            HirExprKind::TupleField { target, index, .. } => {
                format!("{}.{index}", self.show(target))
            }
            HirExprKind::SumWrap { value: Some(x), .. } | HirExprKind::Cast { operand: x, .. } => {
                self.show(x)
            }
            HirExprKind::Block(b) => b
                .trailing
                .as_ref()
                .map_or_else(String::new, |t| self.show(t)),
            _ => "the value".to_owned(),
        }
    }

    fn local_ty(&self, l: LocalId) -> Option<HirTy> {
        if l == RET {
            return Some(self.f.ret_ty.clone());
        }
        if (SHADOW..TEMP).contains(&l.0) {
            return self
                .f
                .params
                .get((l.0 - SHADOW) as usize)
                .map(|p| p.ty.clone());
        }
        if let Some(t) = self.temp_tys.get(&l) {
            return Some(t.clone());
        }
        self.f
            .locals
            .iter()
            .find(|x| x.id == l)
            .map(|x| x.ty.clone())
    }

    /// The fields of a value of type `t` worth following, by path step.
    fn fields(&self, t: &HirTy) -> Option<Vec<(u32, HirTy)>> {
        let all: Vec<(u32, HirTy)> = match t {
            HirTy::Struct(id) => {
                let structs = self.c.env.defs.structs.borrow();
                let s = structs.get(id.0 as usize)?;
                #[allow(clippy::cast_possible_truncation)]
                s.fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (i as u32, f.ty.clone()))
                    .collect()
            }
            #[allow(clippy::cast_possible_truncation)]
            HirTy::Tuple(ts) => ts
                .iter()
                .enumerate()
                .map(|(i, t)| (i as u32 | TUPLE, t.clone()))
                .collect(),
            _ => return None,
        };
        Some(all.into_iter().filter(|(_, t)| self.c.tracked(t)).collect())
    }

    fn place_ty(&self, p: &Place) -> Option<HirTy> {
        let mut t = self.local_ty(p.0)?;
        for step in &p.1 {
            t = self.fields(&t)?.into_iter().find(|(k, _)| k == step)?.1;
        }
        Some(t)
    }

    /// The paths below `base` (relative to it) that cover it except
    /// `base ++ rel` (`None`: its type is not known).
    fn split(&self, base: &Place, rel: &[u32]) -> Option<Vec<Vec<u32>>> {
        let Some((&k0, rest)) = rel.split_first() else {
            return Some(Vec::new());
        };
        let t = self.place_ty(base)?;
        let mut out = Vec::new();
        for (k, _) in self.fields(&t)? {
            if k == k0 {
                let below = self.split(&extend(base, &[k]), rest)?;
                out.extend(below.into_iter().map(|s| [&[k][..], &s].concat()));
            } else {
                out.push(vec![k]);
            }
        }
        Some(out)
    }

    /// A use of place `p`, written `e` (`None`: the whole local `p.0`).
    #[allow(clippy::many_single_char_names)]
    fn use_place(&mut self, p: &Place, ty: &HirTy, written: Option<&HirExpr>, at: Span) {
        if let Some(i) = self.param_index(p.0).filter(|_| p.1.len() <= MAX_DEPTH) {
            self.uses.insert((i, p.1.clone()));
        }
        let Some((gp, g)) = self.cur.0.iter().find_map(|(_, st)| {
            st.gone
                .iter()
                .find(|(q, _)| overlap(p, q))
                .map(|(q, why)| (q.clone(), *why))
        }) else {
            return;
        };
        if !self.c.tracked(ty) || self.c.in_unsafe(at) || self.copy_out(written, at) {
            return;
        }
        // A view a callee kept was reported at the call.
        let o = g.origin();
        if self.c.kept_at.borrow().contains(&(o.file, o.start)) {
            return;
        }
        // A compiler-made local (`#..`) is not reported: what it is copied
        // into is. One holding a `using` block's value is the block's value
        // leaving the lock.
        if self.hidden(p.0) {
            // A `return`'s value: `escapes_guard` reports it.
            if let Gone::Guard(site, guard, tie) = g {
                if matches!(tie, Tie::Taken | Tie::Shared) && !self.returning {
                    self.block_value_escapes(site, guard, at);
                }
            }
            return;
        }
        if let Gone::Guard(site, guard, _) = g {
            if self
                .reported
                .contains(&((guard, Vec::new()), site.file, site.start))
            {
                return;
            }
        }
        // What was handed over is still among `p`'s elements.
        let in_elems = gp.0 == p.0 && gp.1.len() > p.1.len() && gp.1[p.1.len()..].contains(&ELEM);
        if in_elems && self.holder_only() {
            return;
        }
        let origin = g.origin();
        if !self
            .reported
            .insert((gp.clone(), origin.file, origin.start))
        {
            return;
        }
        let mut x = written.map_or_else(|| self.name(p.0), |e| self.show(e));
        // Only a field of it went: name the field (`b.buf`).
        if gp.0 == p.0 && gp.1.len() > p.1.len() && !gp.1.contains(&ELEM) {
            if let Some(f) = self.field_path(p, &gp.1[p.1.len()..]) {
                x = format!("{x}{f}");
            }
        }
        if let Gone::Guard(site, guard, Tie::Itself) = g {
            let gname = self.name(guard);
            let line = self.line(site, at);
            let d = if p.1.is_empty() && self.is_guard(p.0) {
                RokDiagnostic::error(
                    Code::USE_AFTER_UNLOCK,
                    at,
                    format!(
                        "`{x}` is the guard `{gname}`, and `{gname}`'s `using` block released \
                         the lock at {line}"
                    ),
                )
                .with_secondary(site, "the lock is released here")
                .with_note(format!(
                    "help: bind a guard with `using` once, and use `{x}` itself inside its block \
                     (stdlib 2.16.3)"
                ))
            } else {
                RokDiagnostic::error(
                    Code::USE_AFTER_UNLOCK,
                    at,
                    format!(
                        "`{x}` holds a copy of the guard `{gname}`, whose `using` block released \
                         the lock at {line}"
                    ),
                )
                .with_secondary(site, "the lock is released here")
                .with_note(format!(
                    "help: a guard is only good inside its `using` block; do the work there, or \
                     keep a deep copy of what it protects (`{gname}.get_clone()`) (stdlib \
                     2.13.9)"
                ))
            };
            self.diags.push(d);
            return;
        }
        if let Gone::Via(site, _) = g {
            self.c.via_at.borrow_mut().insert((at.file, at.start), site);
        }
        // A spawned future that captures a guard is an error at the spawn
        // (send_check); the guard's release is no second error.
        if matches!(g, Gone::To(_, What::Task)) && p.1.is_empty() && self.is_guard(p.0) {
            return;
        }
        let mut d = self.diagnostic(&x, g, written, at);
        // What was handed over, as written there (`rows[0]`).
        let handed = self
            .c
            .handed
            .borrow()
            .get(&(origin.file, origin.start))
            .cloned()
            .unwrap_or_else(|| "a value".to_owned());
        let elem = elem_of(&gp);
        let twice = in_elems
            && elem
                .as_ref()
                .is_some_and(|el| self.cur.0.iter().any(|(_, st)| is_dup(st, el)));
        if twice {
            let el = elem.unwrap_or_else(|| gp.clone());
            let mut lines: BTreeSet<u32> = BTreeSet::new();
            for (_, st) in &self.cur.0 {
                for a in elem_aliases(st, &el) {
                    for (p, sp) in &st.stored {
                        if within(p, &a) || within(&a, p) {
                            lines.extend(sp.iter().filter(|x| x.file == at.file).map(|x| x.line));
                        }
                    }
                }
            }
            let at_lines = match lines.iter().take(2).collect::<Vec<_>>()[..] {
                [a, b] => format!(" (stored at lines {a} and {b})"),
                [a] => format!(" (stored at line {a})"),
                _ => String::new(),
            };
            let given = match g {
                Gone::To(site, _) => self
                    .c
                    .direct
                    .borrow()
                    .get(&(site.file, site.start))
                    .map(|(n, _)| n.clone()),
                _ => None,
            }
            .unwrap_or_else(|| "the value".to_owned());
            let line = self.line(origin, at);
            d.message = format!(
                "`{x}` may hold one object twice{at_lines}, and one of them was handed to another \
                 owner at {line}"
            );
            d.notes = vec![format!(
                "help: store separate values (`.clone()` what you store), or hand over \
                 `{given}.clone()` (stdlib 2.13.9)"
            )];
        } else if in_elems {
            let line = self.line(origin, at);
            d.message = format!(
                "`{x}` still holds {handed}, which was handed to another owner at {line} (as \
                 an element, or a value one shares)"
            );
            let take = self.take_out(p, &x);
            let copy = if handed.starts_with('`') {
                format!("`{}.clone()`", handed.trim_matches('`'))
            } else {
                "a copy (`.clone()`)".to_owned()
            };
            d.notes = vec![format!(
                "help: take it out of `{x}` first ({take}) and hand that over, or hand over \
                 {copy} (stdlib 2.13.9)"
            )];
        }
        self.diags.push(d);
    }

    /// The innermost call is a collection method that neither reads nor
    /// returns its elements (`len`, `push`, `clear`): a use of the
    /// collection whose elements were handed over is fine there.
    fn holder_only(&self) -> bool {
        const SAFE: &[&str] = &[
            "rok::core::collections::list::List__len",
            "rok::core::collections::list::List__is_empty",
            "rok::core::collections::list::List__capacity",
            "rok::core::collections::list::List__clear",
            "rok::core::collections::list::List__truncate",
            "rok::core::collections::list::List__reserve",
            "rok::core::collections::list::List__shrink_to_fit",
            "rok::core::collections::list::List__push",
            "rok::core::collections::list::List__insert",
            "rok::core::collections::deque::Deque__len",
            "rok::core::collections::deque::Deque__is_empty",
            "rok::core::collections::deque::Deque__clear",
            "rok::core::collections::deque::Deque__push_back",
            "rok::core::collections::deque::Deque__push_front",
            "rok::core::collections::dict::Dict__len",
            "rok::core::collections::dict::Dict__is_empty",
            "rok::core::collections::dict::Dict__clear",
        ];
        let Some(HirExprKind::Call { callee, .. }) = self.calls.last().map(|c| &c.kind) else {
            return false;
        };
        self.c
            .fns
            .get(callee)
            .is_some_and(|f| SAFE.iter().any(|n| is_fn(&f.name, n)))
    }

    fn hidden(&self, l: LocalId) -> bool {
        self.f
            .locals
            .iter()
            .find(|x| x.id == l)
            .is_some_and(|x| x.name.as_ref().is_none_or(|n| n.starts_with('#')))
    }

    /// `e` is the value of a `ref` parameter going back to the caller
    /// (not written in the source): the caller sees what is gone of it.
    fn copy_out(&self, e: Option<&HirExpr>, at: Span) -> bool {
        let Some(HirExprKind::Local(l)) = e.map(|e| &e.kind) else {
            return false;
        };
        self.f.params.iter().any(|p| p.local == *l && p.by_ref)
            && self.c.src(at).is_some_and(|t| t != self.name(*l))
    }

    /// `at`: where the line of `s` is, said from the use at `from`.
    fn line(&self, s: Span, from: Span) -> String {
        if s.file == from.file {
            format!("line {}", s.line)
        } else {
            let path = self.c.file_name(s.file);
            let name = path.rsplit(['/', '\\']).next().unwrap_or(&path);
            format!("{name}:{}", s.line)
        }
    }

    /// The call `e` is an argument (or receiver) of, rewritten with
    /// `with` in place of `e`: `xs.push(i)` -> `g.get().push(i)`.
    fn in_call(&self, e: Option<&HirExpr>, with: &str) -> String {
        let fallback = with.to_owned();
        let Some(e) = e else { return fallback };
        let Some(call) = self.calls.last().filter(|c| {
            let (HirExprKind::Call { args, .. }
            | HirExprKind::CallIndirect { args, .. }
            | HirExprKind::CallClosure { args, .. }) = &c.kind
            else {
                return false;
            };
            args.iter().any(|a| a.span == e.span)
        }) else {
            return fallback;
        };
        let (Some(text), true) = (
            self.c.src(call.span),
            call.span.file == e.span.file
                && call.span.start <= e.span.start
                && e.span.end <= call.span.end,
        ) else {
            return fallback;
        };
        let (a, b) = (
            (e.span.start - call.span.start) as usize,
            (e.span.end - call.span.start) as usize,
        );
        if text.len() > 60 || text.contains('\n') || !text.is_char_boundary(a) {
            return fallback;
        }
        format!("{}{with}{}", &text[..a], &text[b..])
    }

    fn diagnostic(
        &self,
        who: &str,
        gone: Gone,
        written: Option<&HirExpr>,
        at: Span,
    ) -> RokDiagnostic {
        let chk = self.c;
        let err = |code: Code, msg: String| RokDiagnostic::error(code, at, msg);
        let keep = format!("pass `{who}.clone()` to keep a separate copy");
        match gone {
            Gone::To(site, what) => {
                let owner = chk
                    .owners
                    .borrow()
                    .get(&(site.file, site.start))
                    .filter(|(_, b)| self.scopes.contains(b))
                    .map(|(n, _)| n.clone());
                let (arg, arg_ty) = chk
                    .direct
                    .borrow()
                    .get(&(site.file, site.start))
                    .cloned()
                    .unzip();
                let line = self.line(site, at);
                let (moved, here) = match what {
                    What::Mutex => ("moved into the `Mutex`", "moved into the `Mutex` here"),
                    What::RwLock => ("moved into the `RwLock`", "moved into the `RwLock` here"),
                    What::Channel => ("sent through a channel", "sent here"),
                    What::Handoff => ("handed to a `Handoff`", "handed over here"),
                    What::Task => ("spawned as a task", "spawned here"),
                };
                // The hand-off itself again, on the loop's next pass.
                if self.calls.last().is_some_and(|k| k.span == site) {
                    return err(Code::USE_AFTER_HANDOFF, format!(
                        "`{who}` was already {moved} by this call, on an earlier pass of the loop"
                    ))
                    .with_note(format!(
                        "help: hand over `{who}.clone()` so each pass hands over its own copy, or \
                         make `{who}` inside the loop (stdlib 2.13.9)"
                    ));
                }
                // Another handle to what was handed over.
                let (head, given) = match arg.as_deref().filter(|arg| *arg != who) {
                    Some(arg) => (
                        format!("`{who}` shares `{arg}`, which was {moved}"),
                        arg.to_owned(),
                    ),
                    None => (format!("`{who}` was {moved}"), who.to_owned()),
                };
                let keep = format!("hand over `{given}.clone()` to keep a separate copy");
                let diag = err(Code::USE_AFTER_HANDOFF, format!("{head} at {line}"))
                    .with_secondary(site, format!("`{given}` {here}"));
                match what {
                    What::Mutex | What::RwLock => {
                        let (ty, lock) = if what == What::Mutex {
                            ("Mutex", "lock")
                        } else {
                            ("RwLock", "write")
                        };
                        let through = match owner {
                            Some(lock_var) => {
                                let same = written
                                    .is_some_and(|e| arg_ty.as_ref() == Some(&e.ty.stable_key()));
                                let body = if same {
                                    self.in_call(written, "g.get()")
                                } else {
                                    ".. g.get() ..".to_owned()
                                };
                                format!(
                                    "use it through the lock: `using g = {lock_var}.{lock}() {{ {body} }}`"
                                )
                            }
                            None => format!(
                                "use it through the `{ty}`'s lock (`g.get()` inside its `using` \
                                 block)"
                            ),
                        };
                        diag.with_note(format!(
                            "help: it belongs to the `{ty}` now; {through}, or {keep} (stdlib \
                             2.13.9)"
                        ))
                    }
                    What::Channel => diag.with_note(format!(
                        "help: the receiver owns it now; {keep}, or give `{who}` a new value before \
                         using it again (stdlib 2.13.9)"
                    )),
                    What::Task => {
                        let future = written.is_some_and(|e| match &e.ty {
                            HirTy::Struct(id) => {
                                let name = chk.ctx.struct_def(*id).name;
                                name.starts_with("rok::core::future::AsyncFuture")
                                    || name.starts_with("rok::core::future::TryAsyncFuture")
                            }
                            _ => false,
                        });
                        diag.with_note(if future {
                            format!(
                                "help: the task runs it now; call the async fn again for a \
                                 second future, or give `{who}` a new value before using it \
                                 again (stdlib 2.18.1)"
                            )
                        } else {
                            format!(
                                "help: the task works on its own copy, so changes it makes \
                                 are not seen here: return them from the task (`await`), \
                                 {keep}, or share read-only data through `Frozen` (stdlib \
                                 2.18.1)"
                            )
                        })
                    }
                    What::Handoff => {
                        let getter = owner.map_or_else(
                            || "the `Handoff`'s `get()`".to_owned(),
                            |own| format!("`{own}.get()`"),
                        );
                        diag.with_note(format!(
                            "help: the `Handoff` owns it now; read it through {getter}, or {keep} \
                             (stdlib 2.13.9)"
                        ))
                    }
                }
            }
            Gone::Via(site, id) => {
                let why = chk.whys.borrow().0[id as usize].clone();
                let callee = chk.callees.borrow().get(&(site.file, site.start)).cloned();
                let text = chk.src(site).filter(|t| t.len() <= 60 && !t.contains('\n'));
                let named = text.is_some_and(|t| t.contains(who));
                let call = text
                    .map(|t| format!("`{t}`"))
                    .or_else(|| callee.map(|n| format!("this call of `{n}`")))
                    .unwrap_or_else(|| "this call".to_owned());
                let verb = match why.what {
                    What::Mutex | What::RwLock => format!("moved into {}", why.what.noun()),
                    What::Channel => "sent through a channel".to_owned(),
                    What::Handoff => "handed to a `Handoff`".to_owned(),
                    What::Task => "spawned as a task".to_owned(),
                };
                let file = why.file.rsplit(['/', '\\']).next().unwrap_or(&why.file);
                let inner = if chk.file_name(site.file) == why.file {
                    format!("line {}", why.line)
                } else {
                    format!("{file}:{}", why.line)
                };
                let head = if named {
                    format!("`{who}` was {verb} by {call}")
                } else {
                    format!("`{who}` shares what {call} {verb}")
                };
                err(Code::USE_AFTER_HANDOFF, format!(
                    "{head} at {} (inside `{}`, {inner})",
                    self.line(site, at),
                    why.func
                ))
                .with_secondary(site, format!("{verb} here"))
                .with_note(format!(
                    "help: {} now; pass a `.clone()` to keep a separate copy, or give `{who}` a new \
                     value before using it again (stdlib 2.13.9)",
                    match why.what {
                        What::Channel => "the receiver owns it".to_owned(),
                        What::Handoff => "the `Handoff` owns it".to_owned(),
                        What::Task => "the task owns it".to_owned(),
                        What::Mutex | What::RwLock => format!("it belongs to {}", why.what.noun()),
                    }
                ))
            }
            Gone::Guard(site, guard, tie_kind) => {
                let gname = self.name(guard);
                let what = self.guard_what(guard);
                let ty = if what == What::RwLock {
                    "RwLock"
                } else {
                    "Mutex"
                };
                let line = self.line(site, at);
                let tie = chk
                    .src(site)
                    .filter(|t| t.len() <= 24 && !t.contains('\n'))
                    .map(str::to_owned);
                if tie_kind == Tie::Given {
                    err(
                        Code::USE_AFTER_HANDOFF,
                        format!("`{who}` was given to the `{ty}` at {line}"),
                    )
                    .with_secondary(site, format!("`{who}` given to the lock here"))
                    .with_note(format!(
                        "help: `{who}` belongs to the `{ty}` now; use it through the lock \
                             (`{gname}.get()` inside a `using` block), or {keep} (stdlib 2.13.9)"
                    ))
                } else if self.returning {
                    let copy = tie.map_or_else(
                        || "a `.clone()` of it".to_owned(),
                        |t| format!("`{t}.clone()`"),
                    );
                    err(
                        Code::USE_AFTER_UNLOCK,
                        format!(
                            "this returns a value out of the `{ty}` after `{gname}`'s `using` \
                         block released the lock"
                        ),
                    )
                    .with_secondary(site, "taken out of the lock here")
                    .with_note(format!(
                        "help: the lock no longer protects it; return {copy}, or do the work \
                         inside the `using` block (stdlib 2.13.9)"
                    ))
                } else if tie_kind == Tie::Shared {
                    let from = tie
                        .as_ref()
                        .map_or_else(String::new, |t| format!(" (`{t}`)"));
                    err(
                        Code::USE_AFTER_UNLOCK,
                        format!(
                            "`{who}` shares the value the `{ty}` protects{from} at {line}, and \
                         `{gname}`'s `using` block released the lock"
                        ),
                    )
                    .with_secondary(site, "the lock's value, taken out here")
                    .with_note(format!(
                        "help: the lock no longer protects `{who}`; use it inside the `using` \
                         block, or store a copy: `.clone()` what you take out of `{}` (stdlib \
                         2.13.9)",
                        tie.unwrap_or_else(|| format!("{gname}.get()"))
                    ))
                } else {
                    let copy = format!("keep a deep copy there with `{gname}.get_clone()`");
                    err(Code::USE_AFTER_UNLOCK, format!(
                        "`{who}` came out of the `{ty}` at {line}, and `{gname}`'s `using` block \
                         released the lock"
                    ))
                    .with_secondary(site, format!("`{who}` taken out of the lock here"))
                    .with_note(format!(
                        "help: the lock no longer protects `{who}`; use it inside the `using` \
                         block, or {copy} (stdlib 2.13.9)"
                    ))
                }
            }
        }
    }

    /// The lock a guard local belongs to.
    /// Local `l` is a lock guard.
    fn is_guard(&self, l: LocalId) -> bool {
        let structs = self.c.env.defs.structs.borrow();
        match self.local_ty(l) {
            Some(HirTy::Struct(id)) => structs
                .get(id.0 as usize)
                .is_some_and(|s| GUARDS.iter().any(|(g, _)| s.name.starts_with(g))),
            _ => false,
        }
    }

    fn guard_what(&self, g: LocalId) -> What {
        let structs = self.c.env.defs.structs.borrow();
        let ty = self.local_ty(g);
        let name = match &ty {
            Some(HirTy::Struct(id)) => structs.get(id.0 as usize).map(|s| s.name.clone()),
            _ => None,
        };
        drop(structs);
        name.and_then(|n| {
            GUARDS
                .iter()
                .find(|(p, _)| n.starts_with(p))
                .map(|(_, w)| *w)
        })
        .unwrap_or(What::Mutex)
    }

    fn mark_gone(&mut self, p: Place, why: Gone) {
        for s in self.cur.states() {
            mark_gone(s, p.clone(), why);
        }
    }

    /// `p` is not gone any more; a gone place it is a field of: its other
    /// fields stay gone.
    fn regain(&self, s: &mut St, p: &Place) {
        let wider: Vec<(Place, Gone)> = s
            .gone
            .iter()
            .filter(|(g, _)| within(p, g) && g.1.len() < p.1.len())
            .map(|(g, w)| (g.clone(), *w))
            .collect();
        s.gone.retain(|g, _| !within(g, p) && !within(p, g));
        for (g, why) in wider {
            match self.split(&g, &p.1[g.1.len()..]) {
                Some(rest) => {
                    for r in rest {
                        s.gone.insert(extend(&g, &r), why);
                    }
                }
                None => {
                    s.gone.insert(g, why);
                }
            }
        }
    }

    /// `p` gets a new value: it is ours again and aliases nothing. Edges
    /// through it are joined past it first: what aliased its old value
    /// still aliases each other.
    fn assign(&mut self, p: &Place) {
        let mut cur = std::mem::take(&mut self.cur);
        for (_, s) in &mut cur.0 {
            *s = self.assign_st(std::mem::take(s), p);
        }
        cur.untag(p);
        self.cur = cur;
    }

    /// The edges of a copy that `p` is a field of, `p` given a new value:
    /// its other fields stay copies, and its copy of `p` is still the old
    /// value (`rep`).
    fn field_copies(
        &self,
        keep: Vec<Edge>,
        p: &Place,
        rep: Option<&Place>,
        edges: &mut BTreeSet<Edge>,
    ) {
        for e in keep {
            let Edge::Same(a, b) = &e else {
                edges.insert(e);
                continue;
            };
            let (base, other) = if within(p, a) {
                (a, b)
            } else if within(p, b) {
                (b, a)
            } else {
                edges.insert(e);
                continue;
            };
            match self.split(base, &p.1[base.1.len()..]) {
                Some(rest) => {
                    for r in rest {
                        edges.insert(Edge::Same(extend(base, &r), extend(other, &r)));
                    }
                    if let Some(r) = rep {
                        edges.insert(Edge::Same(extend(other, &p.1[base.1.len()..]), r.clone()));
                    }
                }
                None => {
                    edges.insert(e);
                }
            }
        }
    }

    fn assign_st(&self, mut s: St, p: &Place) -> St {
        self.regain(&mut s, p);
        for t in s.tied.values_mut() {
            t.retain(|q, _| !within(q, p));
        }
        let (touch, keep): (Vec<Edge>, Vec<Edge>) =
            s.edges.take_of(p.0).into_iter().partition(|e| e.touches(p));
        let mut edges: BTreeSet<Edge> = BTreeSet::new();
        // A copy of all of `p`'s old value stands in for it in the rest.
        let rep = touch.iter().find_map(|e| match e {
            Edge::Same(a, b) if a == p && !within(b, p) => Some(b.clone()),
            Edge::Same(a, b) if b == p && !within(a, p) => Some(a.clone()),
            _ => None,
        });
        self.field_copies(keep, p, rep.as_ref(), &mut edges);
        if let Some(r) = rep {
            let swap = |u: &Place| {
                if within(u, p) {
                    extend(&r, &u.1[p.1.len()..])
                } else {
                    u.clone()
                }
            };
            for e in &touch {
                let e = match e {
                    Edge::Same(a, b) => Edge::Same(swap(a), swap(b)),
                    Edge::In(a, b) => Edge::In(swap(a), swap(b)),
                    Edge::Into(a, b) => Edge::Into(swap(a), swap(b)),
                    Edge::Taken(a, b) => Edge::Taken(swap(a), swap(b)),
                };
                edges.insert(e);
            }
            for e in edges.into_iter().filter(kept) {
                s.edges.insert(e);
            }
            return s;
        }
        // Each end in `p`: (end, other end, other ⇒ end, end ⇒ other, same,
        // taken: 1 the end was taken out of the other, 2 the reverse).
        let mut ends: Vec<(Place, Place, bool, bool, bool, u8)> = Vec::new();
        for e in &touch {
            let (a, b) = e.ends();
            for (u, x, fwd) in [(a, b, true), (b, a, false)] {
                if !within(u, p) || within(x, p) {
                    continue;
                }
                let (to_u, from_u, same) = match e {
                    Edge::Same(..) => (true, true, true),
                    Edge::In(..) => (true, true, false),
                    // `Into(a, b)`: a ⇒ b; a taken element as one.
                    Edge::Into(..) | Edge::Taken(..) => (!fwd, fwd, false),
                };
                let taken = match e {
                    Edge::Taken(..) if fwd => 1,
                    Edge::Taken(..) => 2,
                    _ => 0,
                };
                ends.push((u.clone(), x.clone(), to_u, from_u, same, taken));
            }
        }
        for (i, (u1, x1, to1, from1, same1, tk1)) in ends.iter().enumerate() {
            for (u2, x2, to2, from2, same2, tk2) in &ends[i + 1..] {
                if !overlap(u1, u2) || x1 == x2 {
                    continue;
                }
                let both1 = *to1 && *from1 && *tk1 == 0;
                let both2 = *to2 && *from2 && *tk2 == 0;
                if let Some(e) = taken_through(u1 == u2, (x1, both1, *tk1), (x2, both2, *tk2)) {
                    edges.insert(e);
                    continue;
                }
                if *same1 && *same2 {
                    let e = if within(u2, u1) {
                        Edge::Same(extend(x1, &u2.1[u1.1.len()..]), x2.clone())
                    } else {
                        Edge::Same(x1.clone(), extend(x2, &u1.1[u2.1.len()..]))
                    };
                    edges.insert(e);
                    continue;
                }
                let fwd = *to1 && *from2;
                let back = *to2 && *from1;
                match (fwd, back) {
                    (true, true) => edges.insert(Edge::In(x1.clone(), x2.clone())),
                    (true, false) => edges.insert(Edge::Into(x1.clone(), x2.clone())),
                    (false, true) => edges.insert(Edge::Into(x2.clone(), x1.clone())),
                    (false, false) => false,
                };
            }
        }
        for e in edges.into_iter().filter(kept) {
            s.edges.insert(e);
        }
        s
    }

    /// What `e`'s value carries, of the places this walk knows.
    fn flows(&self, e: &HirExpr, out: &mut Vec<Flow>) {
        if self.c.tracked(&e.ty) {
            self.flows_any(e, out);
        }
    }

    /// `flows`, also for a `Sync` value whose field is followed (a list's
    /// head holding its buffer).
    fn flows_any(&self, e: &HirExpr, out: &mut Vec<Flow>) {
        if self.shared_read(e) {
            out.push((Fk::Is(Vec::new()), (WORLD, Vec::new())));
            return;
        }
        if let Some(p) = place(e) {
            out.push((Fk::Is(Vec::new()), p));
            return;
        }
        let sub = |s: &Self, i: u32, x: &HirExpr, out: &mut Vec<Flow>| {
            let mut v = Vec::new();
            s.flows(x, &mut v);
            out.extend(v.into_iter().map(|(k, q)| match k {
                Fk::Is(s) => (Fk::Is([&[i][..], &s].concat()), q),
                k => (k, q),
            }));
        };
        let as_kind = |s: &Self, x: &HirExpr, k: &Fk, out: &mut Vec<Flow>| {
            let mut v = Vec::new();
            s.flows(x, &mut v);
            out.extend(v.into_iter().map(|(_, q)| (k.clone(), q)));
        };
        match &e.kind {
            HirExprKind::StructInit { fields, .. } => {
                for (i, x) in fields.iter().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    sub(self, i as u32, x, out);
                }
            }
            HirExprKind::Tuple { fields } => {
                for (i, x) in fields.iter().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    sub(self, i as u32 | TUPLE, x, out);
                }
            }
            HirExprKind::WithField {
                target,
                field_index,
                value,
                ..
            } => {
                self.flows(target, out);
                sub(self, *field_index, value, out);
            }
            HirExprKind::FieldRead {
                target,
                field_index: i,
                ..
            }
            | HirExprKind::TupleField {
                target, index: i, ..
            } => {
                let i = if matches!(e.kind, HirExprKind::TupleField { .. }) {
                    i | TUPLE
                } else {
                    *i
                };
                let mut v = Vec::new();
                self.flows_any(target, &mut v);
                for (k, q) in v {
                    match k {
                        Fk::Is(s) if s.is_empty() => out.push((Fk::Is(s), extend(&q, &[i]))),
                        Fk::Is(s) if s[0] == i => out.push((Fk::Is(s[1..].to_vec()), q)),
                        Fk::Is(_) => {}
                        k => out.push((k, q)),
                    }
                }
            }
            HirExprKind::SumWrap { value: Some(x), .. } | HirExprKind::Cast { operand: x, .. } => {
                self.flows(x, out);
            }
            // A capture, or an element of a fixed array: held by it.
            // A closure's captures are fields of its value.
            HirExprKind::ClosureValue { env: x, .. } => self.flows(x, out),
            HirExprKind::ArrayIndex { array: x, .. } => {
                as_kind(self, x, &Fk::Both, out);
            }
            HirExprKind::ArrayInit { elements: xs, .. }
            | HirExprKind::EnumVariantInit { args: xs, .. } => {
                xs.iter().for_each(|x| as_kind(self, x, &Fk::Both, out));
            }
            HirExprKind::Block(b) => self.block_flows(b, out),
            HirExprKind::If {
                then_block,
                else_branch,
                ..
            } => {
                self.block_flows(then_block, out);
                if let Some(x) = else_branch {
                    self.flows(x, out);
                }
            }
            HirExprKind::Match { arms, .. } => arms.iter().for_each(|a| self.flows(&a.body, out)),
            // An element of the buffer: what was stored into it.
            HirExprKind::BufGet { buf, .. } => {
                let mut hs = Vec::new();
                self.holders(buf, &mut hs);
                out.extend(hs.iter().map(|h| (Fk::Is(Vec::new()), extend(h, &[ELEM]))));
            }
            HirExprKind::Call { .. }
            | HirExprKind::CallIndirect { .. }
            | HirExprKind::CallClosure { .. }
            | HirExprKind::CoroNew { .. } => {
                if let Some(v) = self.results.get(&addr(e)) {
                    out.extend(v.iter().cloned());
                }
            }
            _ => {}
        }
    }

    /// `e` reads a value that is not `Sync` out of a `Sync` object (an
    /// `unsafe impl Sync` type): other threads may reach it too.
    fn shared_read(&self, e: &HirExpr) -> bool {
        // These `Sync` types hand a value over (a channel, a task's
        // result, a `Handoff`, a lock's guard): what comes out is the
        // receiver's alone.
        const OWNERS: &[&str] = &[
            "rok::core::channel::",
            "rok::core::marker::",
            "rok::core::par::",
            "rok::core::sync::",
            "rok::core::task::",
        ];
        if OWNERS.iter().any(|m| self.f.name.starts_with(m)) {
            return false;
        }
        match &e.kind {
            HirExprKind::FieldRead { target: t, .. }
            | HirExprKind::TupleField { target: t, .. }
            | HirExprKind::BufGet { buf: t, .. }
            | HirExprKind::ArrayIndex { array: t, .. } => {
                let shared = !matches!(t.ty, HirTy::Unknown | HirTy::Any)
                    && !super::bounds::has_param(self.c.ctx, &t.ty)
                    && !self.c.tracked(&t.ty);
                // A buffer is followed whatever its elements; a `Sync` struct
                // holding one is not shared state.
                let buf = matches!(e.ty, HirTy::GcBuf(_));
                (shared && !buf && self.c.tracked(&e.ty)) || self.shared_read(t)
            }
            HirExprKind::SumWrap { value: Some(x), .. } | HirExprKind::Cast { operand: x, .. } => {
                self.shared_read(x)
            }
            HirExprKind::Block(b) if b.stmts.is_empty() => {
                b.trailing.as_ref().is_some_and(|t| self.shared_read(t))
            }
            _ => false,
        }
    }

    fn block_flows(&self, b: &HirBlock, out: &mut Vec<Flow>) {
        if let Some(t) = &b.trailing {
            self.flows(t, out);
        }
    }

    /// `p` now holds `e`'s value (`part`: a part of it).
    fn store(&mut self, p: &Place, e: &HirExpr, part: bool) {
        self.assign(p);
        if let HirExprKind::ArrayInit { elements, .. } = &e.kind {
            self.array_dup(p, elements);
        }
        let mut fl = Vec::new();
        self.flows(e, &mut fl);
        for s in self.cur.states() {
            // What is gone of the value stays gone in `p` (a call's result
            // that its callee handed over).
            let mut inherit = Vec::new();
            for (k, q) in &fl {
                for (g, why) in s.gone.iter().filter(|(g, _)| overlap(g, q)) {
                    match k {
                        Fk::Is(sub) if !part => {
                            let rest = g.1.get(q.1.len()..).unwrap_or_default();
                            inherit.push((extend(p, &[&sub[..], rest].concat()), *why));
                        }
                        Fk::Is(_) | Fk::Both => inherit.push((p.clone(), *why)),
                        Fk::Holds => {}
                    }
                }
            }
            for (k, q) in &fl {
                let edge = match k {
                    Fk::Is(sub) if !part => Edge::Same(extend(p, sub), q.clone()),
                    Fk::Is(_) | Fk::Both => Edge::In(p.clone(), q.clone()),
                    Fk::Holds => Edge::Into(p.clone(), q.clone()),
                };
                if kept(&edge) {
                    s.edges.insert(edge);
                }
            }
            for (q, why) in inherit {
                mark_gone(s, q, why);
            }
        }
        if let Some(g) = self.got_from(e) {
            self.tie(g, p.clone(), e.span, Tie::Taken);
        }
    }

    /// Place `p` becomes `e`. A field update of `p` itself
    /// (`p.f = v`, lowered to `p = p with f: v`) stores into `p.f`.
    fn assign_into(&mut self, p: Place, e: &'h HirExpr) {
        if let HirExprKind::WithField {
            target,
            field_index,
            value,
            ..
        } = &e.kind
        {
            if place(target).as_ref() == Some(&p) {
                return self.assign_into(extend(&p, &[*field_index]), value);
            }
        }
        let k = self.key(e);
        self.expr_to(e, &k);
        self.store(&p, e, false);
        self.retag(&k, &p);
        // The variable that holds a new lock / `Handoff`: named in hints.
        let mut init = e;
        while let HirExprKind::Block(blk) = &init.kind {
            match (&blk.trailing, blk.stmts.is_empty()) {
                (Some(t), true) => init = t,
                _ => break,
            }
        }
        if let HirExprKind::Call { callee, .. } = &init.kind {
            if matches!(self.c.ops.get(callee), Some(Op::Handoff(_, w)) if *w != What::Channel)
                && p.1.is_empty()
            {
                let owner = (
                    self.name(p.0),
                    self.scopes.last().copied().unwrap_or_default(),
                );
                self.c
                    .owners
                    .borrow_mut()
                    .insert((init.span.file, init.span.start), owner);
            }
        }
    }

    fn tie(&mut self, guard: LocalId, p: Place, at: Span, given: Tie) {
        for s in self.cur.states() {
            s.tied
                .entry(guard)
                .or_default()
                .entry(p.clone())
                .or_insert((at, given));
        }
    }

    /// The guard a `g.get()` (or a field of it) reads from.
    fn got_from(&self, e: &HirExpr) -> Option<LocalId> {
        match &e.kind {
            HirExprKind::Call { callee, args } if self.c.ops.get(callee) == Some(&Op::Get) => {
                Some(place(args.first()?)?.0)
            }
            HirExprKind::FieldRead { target: x, .. }
            | HirExprKind::TupleField { target: x, .. }
            | HirExprKind::SumWrap { value: Some(x), .. } => self.got_from(x),
            HirExprKind::Block(b) if b.stmts.is_empty() => self.got_from(b.trailing.as_ref()?),
            _ => None,
        }
    }

    fn block(&mut self, b: &'h HirBlock) {
        self.block_to(b, None);
    }

    /// Block `b`; the tag of its value goes to `k`.
    fn block_to(&mut self, b: &'h HirBlock, k: Option<&Place>) {
        self.scopes.push(std::ptr::from_ref(b) as usize);
        if b.label.is_some() {
            self.frames.push(Frame {
                label: b.label.as_deref(),
                is_loop: false,
                brk: Cur::default(),
                cont: Cur::default(),
                dest: k.cloned(),
            });
        }
        for s in &b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &b.trailing {
            self.value(t, k);
        }
        if b.label.is_some() {
            let f = self.frames.pop().expect("block frame");
            self.cur = join(std::mem::take(&mut self.cur), f.brk);
        }
        self.scopes.pop();
    }

    fn value(&mut self, e: &'h HirExpr, k: Option<&Place>) {
        match k {
            Some(k) => self.expr_to(e, k),
            None => self.expr(e),
        }
    }

    /// The place holding the tag of `e`'s value while it is tested or
    /// stored.
    fn key(&mut self, e: &HirExpr) -> Place {
        let n = self.keys.len();
        #[allow(clippy::cast_possible_truncation)]
        let l = *self
            .keys
            .entry(addr(e))
            .or_insert(LocalId(TAG_KEY + n as u32));
        (l, Vec::new())
    }

    /// The tags at `from` move to `to`.
    fn retag(&mut self, from: &Place, to: &Place) {
        let pinned = self.pinned.contains(&to.0);
        for (t, _) in &mut self.cur.0 {
            let moved: Vec<(Place, u32)> = t
                .iter()
                .filter(|(q, _)| within(q, from))
                .map(|(q, v)| (extend(to, &q.1[from.1.len()..]), *v))
                .collect();
            t.retain(|q, _| !within(q, from));
            if !pinned {
                t.extend(moved);
            }
        }
    }

    /// `e`; the tag of its value goes to `k` on the paths that know it.
    fn expr_to(&mut self, e: &'h HirExpr, k: &Place) {
        if self.cur.is_empty() {
            return;
        }
        for (t, _) in &mut self.cur.0 {
            t.retain(|q, _| !within(q, k));
        }
        match &e.kind {
            HirExprKind::Block(b) => {
                let returning = std::mem::take(&mut self.returning);
                self.returning = returning && b.stmts.is_empty();
                self.block_to(b, Some(k));
            }
            HirExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                self.returning = false;
                self.branch(cond, then_block, else_branch.as_deref(), Some(k));
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.returning = false;
                self.matched(scrutinee, arms, Some(k));
            }
            HirExprKind::Loop { label, body, .. } => {
                self.returning = false;
                self.looped(label.as_deref(), None, body, Some(k));
            }
            HirExprKind::Call { .. }
            | HirExprKind::CallIndirect { .. }
            | HirExprKind::CallClosure { .. }
            | HirExprKind::CoroNew { .. } => self.expr_into(e, e, k),
            HirExprKind::PrimUnaryOp {
                op: PrimOp::LogicalNot,
                operand,
            } => {
                self.expr_to(operand, k);
                for (t, _) in &mut self.cur.0 {
                    if let Some(v) = t.get_mut(k) {
                        *v ^= 1;
                    }
                }
            }
            // A future's tag is its body's.
            HirExprKind::StructInit { struct_id, fields }
                if self.c.futures.contains(struct_id)
                    && matches!(&fields[..], [x] if matches!(x.kind, HirExprKind::CoroNew { .. })) =>
            {
                self.expr_into(e, &fields[0], k);
            }
            _ => {
                self.expr(e);
                let copied = tag_place(e).filter(|_| {
                    !matches!(&e.kind, HirExprKind::TupleField { target, .. }
                        if matches!(target.ty, HirTy::Throws { .. }))
                });
                for (t, _) in &mut self.cur.0 {
                    if let Some(q) = &copied {
                        let got: Vec<(Place, u32)> = t
                            .iter()
                            .filter(|(p, _)| within(p, q))
                            .map(|(p, v)| (extend(k, &p.1[q.1.len()..]), *v))
                            .collect();
                        t.extend(got);
                    } else if let Some(v) = tag_of(e, t) {
                        t.insert(k.clone(), v);
                    }
                }
            }
        }
    }

    /// `e`, whose call `call` gives the tag of its value to `k`.
    fn expr_into(&mut self, e: &'h HirExpr, call: &HirExpr, k: &Place) {
        self.dests.push((addr(call), k.clone()));
        self.expr(e);
        self.dests.pop();
    }

    /// Walk `cond`: the paths on which it may be true, and false.
    fn test(&mut self, cond: &'h HirExpr) -> (Cur, Cur) {
        let k = self.key(cond);
        self.expr_to(cond, &k);
        let (mut yes, mut no) = (Cur::default(), Cur::default());
        for (mut t, s) in std::mem::take(&mut self.cur.0) {
            match t.remove(&k) {
                Some(1) => yes.0.push((t, s)),
                Some(0) => no.0.push((t, s)),
                _ => {
                    no.0.push((t.clone(), s.clone()));
                    yes.0.push((t, s));
                }
            }
        }
        yes.norm();
        no.norm();
        (yes, no)
    }

    fn branch(
        &mut self,
        cond: &'h HirExpr,
        then_block: &'h HirBlock,
        else_branch: Option<&'h HirExpr>,
        k: Option<&Place>,
    ) {
        let (yes, no) = self.test(cond);
        self.cur = yes;
        self.block_to(then_block, k);
        let then = std::mem::replace(&mut self.cur, no);
        if let Some(x) = else_branch {
            self.value(x, k);
        }
        self.cur = join(then, std::mem::take(&mut self.cur));
    }

    /// Matching a variable reads its tag (in the value itself) and binds
    /// its parts: the uses are the bindings'. An arm sees the paths whose
    /// tag it may match and no arm before it takes.
    fn matched(&mut self, scrutinee: &'h HirExpr, arms: &'h [HirMatchArm], k: Option<&Place>) {
        let key = if place(scrutinee).is_none() {
            let ks = self.key(scrutinee);
            self.expr_to(scrutinee, &ks);
            Some((ks, true))
        } else {
            tag_place(scrutinee).map(|p| (p, false))
        };
        // Every arm's bindings are new each time the `match` runs: what an
        // earlier pass left of them (one handed over) is not theirs.
        for a in arms {
            a.pat.each_binding(&mut |l| self.assign(&(l, Vec::new())));
        }
        let mut base = std::mem::take(&mut self.cur);
        let ty = &scrutinee.ty;
        let mut out = Cur::default();
        for (i, a) in arms.iter().enumerate() {
            let mut cur = Cur::default();
            for (t, s) in &base.0 {
                if let Some(&v) = key.as_ref().and_then(|(p, _)| t.get(p)) {
                    let taken = arms[..i]
                        .iter()
                        .any(|b| b.guard.is_none() && catches(&b.pat, v, ty));
                    if taken || !may_match(&a.pat, v, ty) {
                        continue;
                    }
                }
                cur.0.push((t.clone(), s.clone()));
            }
            if let Some((ks, true)) = &key {
                cur.untag(ks);
            }
            self.cur = cur;
            a.pat.each_binding(&mut |l| {
                self.store(&(l, Vec::new()), scrutinee, true);
            });
            if let Some(g) = &a.guard {
                self.expr(g);
            }
            self.value(&a.body, k);
            out = join(out, std::mem::take(&mut self.cur));
        }
        if let Some((ks, true)) = &key {
            base.untag(ks);
        }
        self.cur = if arms.is_empty() { base } else { out };
    }

    /// The paths reach an exit; `k` holds the returned value's tag.
    fn exit_with(&mut self, k: Option<&Place>) {
        for (t, s) in std::mem::take(&mut self.cur.0) {
            let tag = k.and_then(|k| t.get(k)).copied().unwrap_or(ANY);
            let s = match self.exits.remove(&tag) {
                Some(e) => join_st(e, s),
                None => s,
            };
            self.exits.insert(tag, s);
        }
    }

    fn target(&mut self, label: Option<&str>, cont: bool) -> Option<&mut Frame<'h>> {
        self.frames.iter_mut().rev().find(|f| match label {
            Some(l) => f.label == Some(l),
            None => f.is_loop,
        } && (!cont || f.is_loop))
    }

    fn stmt(&mut self, s: &'h HirStmt) {
        match s {
            HirStmt::Let { local: l, init: e } | HirStmt::Assign { local: l, value: e } => {
                self.assign_into((*l, Vec::new()), e);
            }
            HirStmt::ArrayAssign {
                array_local,
                path,
                index,
                value,
                ..
            } => {
                self.expr(index);
                self.expr(value);
                let arr = (*array_local, path.clone());
                if let Some(ty) = self.local_ty(*array_local) {
                    self.use_place(&arr, &ty, None, index.span);
                }
                self.hold(&arr, value);
            }
            HirStmt::StaticAssign { value: e, .. } | HirStmt::Expr(e) => self.expr(e),
            HirStmt::Return(e) => {
                if let Some(e) = e {
                    self.returning = place(e).is_some();
                    self.expr_to(e, &RET_TAG);
                    self.returning = false;
                    self.store(&(RET, Vec::new()), e, false);
                    self.escapes_guard(e, Exit::Return);
                    if let Some(err) = thrown(e) {
                        self.throw_escapes(err);
                    }
                }
                self.note_renews();
                self.exit_with(Some(&RET_TAG));
            }
            HirStmt::Panic(e) => {
                self.expr(e);
                self.cur = Cur::default();
            }
            HirStmt::Break { label, value } => {
                if let Some(e) = value {
                    let k = self
                        .target(label.as_deref(), false)
                        .and_then(|f| f.dest.clone());
                    self.value(e, k.as_ref());
                }
                let st = std::mem::take(&mut self.cur);
                if let Some(f) = self.target(label.as_deref(), false) {
                    f.brk = join(std::mem::take(&mut f.brk), st);
                }
            }
            HirStmt::Continue { label } => {
                let st = std::mem::take(&mut self.cur);
                if let Some(f) = self.target(label.as_deref(), true) {
                    f.cont = join(std::mem::take(&mut f.cont), st);
                }
            }
        }
    }

    /// The places whose objects hold the buffer `e` (`self.h[0].buf`
    /// is held by `self.h`).
    fn holders(&self, e: &HirExpr, out: &mut Vec<Place>) {
        if let Some(p) = place(e) {
            out.push(p);
            return;
        }
        match &e.kind {
            HirExprKind::BufGet { buf: x, .. }
            | HirExprKind::ArrayIndex { array: x, .. }
            | HirExprKind::FieldRead { target: x, .. }
            | HirExprKind::TupleField { target: x, .. }
            | HirExprKind::Cast { operand: x, .. } => self.holders(x, out),
            _ => {
                let mut fl = Vec::new();
                self.flows(e, &mut fl);
                out.extend(fl.into_iter().map(|(_, q)| q));
            }
        }
    }

    /// The buffer at `c` now holds `v` among its elements: they share it
    /// both ways. Storing what it may hold already makes it hold an
    /// object twice.
    fn hold(&mut self, c: &Place, v: &HirExpr) {
        let mut fl = Vec::new();
        self.flows(v, &mut fl);
        let at = extend(c, &[ELEM]);
        for s in self.cur.states() {
            hold_st(s, &at, &fl, false);
        }
    }

    /// `while` (with `cond`) or `loop`: the body runs until the state at
    /// its head stops growing.
    fn looped(
        &mut self,
        label: Option<&'h str>,
        cond: Option<&'h HirExpr>,
        body: &'h HirBlock,
        k: Option<&Place>,
    ) {
        let mut head = std::mem::take(&mut self.cur);
        for pass in 0.. {
            self.frames.push(Frame {
                label,
                is_loop: true,
                brk: Cur::default(),
                cont: Cur::default(),
                dest: k.cloned(),
            });
            self.cur.clone_from(&head);
            let mut exit = Cur::default();
            if let Some(c) = cond {
                let (yes, no) = self.test(c);
                self.cur = yes;
                exit = no;
            }
            self.block(body);
            let f = self.frames.pop().expect("loop frame");
            let cur = std::mem::take(&mut self.cur);
            let mut next = join(head.clone(), join(cur, f.cont));
            if pass >= MAX_SPLIT_PASSES {
                next.flatten();
            }
            if next == head {
                self.cur = join(f.brk, exit);
                return;
            }
            head = next;
        }
    }

    fn expr(&mut self, e: &'h HirExpr) {
        if self.cur.is_empty() {
            return;
        }
        if let Some(p) = place(e).filter(|_| {
            matches!(
                e.kind,
                HirExprKind::Local(_)
                    | HirExprKind::FieldRead { .. }
                    | HirExprKind::TupleField { .. }
            )
        }) {
            return self.use_place(&p, &e.ty, Some(e), e.span);
        }
        let returning = std::mem::take(&mut self.returning);
        match &e.kind {
            HirExprKind::Block(b) => {
                self.returning = returning && b.stmts.is_empty();
                self.block(b);
            }
            HirExprKind::If {
                cond,
                then_block,
                else_branch,
            } => self.branch(cond, then_block, else_branch.as_deref(), None),
            HirExprKind::While { label, cond, body } => {
                self.looped(label.as_deref(), Some(cond), body, None);
            }
            HirExprKind::Loop { label, body, .. } => {
                self.looped(label.as_deref(), None, body, None);
            }
            HirExprKind::Match { scrutinee, arms } => self.matched(scrutinee, arms, None),
            // A step handed to the resumer; the last one ends the body.
            HirExprKind::Suspend { value, is_final } => {
                self.expr_to(value, &RET_TAG);
                // A step leaves like a return; the resumer may keep it past
                // the lock (`yield g.get()` inside `using`).
                if !*is_final {
                    let kept = self.cur.clone();
                    self.store(&(RET, Vec::new()), value, false);
                    self.escapes_guard(value, Exit::Yield);
                    self.cur = kept;
                }
                if *is_final {
                    self.exit_with(Some(&RET_TAG));
                } else {
                    let mut mixed = false;
                    for (t, _) in &mut self.cur.0 {
                        mixed |= t.remove(&RET_TAG).is_none_or(|v| v == ERR);
                    }
                    self.mixed |= mixed;
                    self.cur.norm();
                }
            }
            _ => {
                let is_call = matches!(
                    e.kind,
                    HirExprKind::Call { .. }
                        | HirExprKind::CallIndirect { .. }
                        | HirExprKind::CallClosure { .. }
                );
                if is_call {
                    self.calls.push(e);
                }
                let mut kids = Vec::new();
                each_child(e, &mut |c| kids.push(c));
                for c in kids {
                    match c {
                        Child::E(x) => self.expr(x),
                        Child::B(b) => self.block(b),
                    }
                }
                if is_call {
                    self.calls.pop();
                }
                self.after(e);
            }
        }
    }

    /// The effect of `e` once its operands ran.
    fn after(&mut self, e: &'h HirExpr) {
        match &e.kind {
            HirExprKind::Call { callee, args } => {
                if let Some(op) = self.c.ops.get(callee).copied() {
                    return self.op(op, args, e.span);
                }
                let args: Vec<&HirExpr> = args.iter().collect();
                if self.c.passes.contains(callee) {
                    let mut fl = Vec::new();
                    if let Some(a) = args.first() {
                        self.flows(a, &mut fl);
                    }
                    self.results.insert(addr(e), fl);
                    return;
                }
                let takes = self.c.takes.contains(callee);
                let literal = self.c.literals.contains(callee);
                // Only a direct call says the callee itself hands something
                // over (a call through a function value may reach any
                // function of its type).
                if let Some(s) = self.c.summary(*callee).filter(|s| s.sends) {
                    self.sends = true;
                    let name = self.c.fns.get(callee).map(|f| shown_fn(self.c, f));
                    self.aliased_args(&s, &args, e, name.as_deref());
                }
                if self.c.copies.contains(callee) {
                    self.copy_dup(&args);
                }
                if let Some(s) = self.callee(*callee) {
                    self.kept_views(&s, &args, e, None);
                    let s = if literal {
                        Rc::new(Summary {
                            dups: BTreeSet::new(),
                            ..(*s).clone()
                        })
                    } else {
                        s
                    };
                    if !s.gives.is_empty() {
                        if let Some(f) = self.c.fns.get(callee) {
                            let n = shown_fn(self.c, f);
                            self.c
                                .callees
                                .borrow_mut()
                                .insert((e.span.file, e.span.start), n);
                        }
                    }
                    self.apply(&s, &args, e, true, takes);
                    if literal {
                        self.literal_dup(&s, e);
                    }
                }
                if self.c.polls.contains(callee) {
                    self.polled(e, args.first().copied());
                }
                if self.c.par.contains(callee) {
                    self.par_result(*callee, &args, e);
                }
                if self.c.clears.contains(callee) {
                    if let Some(p) = args.first().and_then(|a| place(a)) {
                        self.cleared(&p);
                    }
                }
            }
            HirExprKind::CallIndirect { callee, args, .. } => {
                let mut all = vec![&**callee];
                all.extend(args.iter());
                let s = match &callee.kind {
                    HirExprKind::FnRef(id) => {
                        self.callee(*id).map(|s| Rc::new(s.remap(&|i| Some(i + 1))))
                    }
                    _ => self.indirect(args),
                };
                if let Some(s) = s {
                    self.kept_views(&s, &all, e, Some(callee));
                    self.apply(&s, &all, e, true, false);
                }
            }
            HirExprKind::CallClosure { closure, args, .. } => {
                let mut all = vec![&**closure];
                all.extend(args.iter());
                let s = match &closure.kind {
                    HirExprKind::ClosureValue { fn_id, .. } => self.callee(*fn_id),
                    _ => self.indirect(args),
                };
                if let Some(s) = s {
                    self.kept_views(&s, &all, e, Some(closure));
                    self.apply(&s, &all, e, true, false);
                }
            }
            HirExprKind::CoroNew { body_fn, args } => {
                let args: Vec<&HirExpr> = args.iter().collect();
                if let Some(s) = self.callee(*body_fn) {
                    self.apply(&s, &args, e, false, false);
                }
                // The frame holds the arguments until it runs.
                let mut v = Vec::new();
                for a in &args {
                    let mut fl = Vec::new();
                    self.flows(a, &mut fl);
                    v.extend(fl.into_iter().map(|(_, q)| (Fk::Holds, q)));
                }
                self.results.insert(addr(e), v);
            }
            HirExprKind::BufSet { buf, value, .. } => {
                let mut cs = Vec::new();
                self.holders(buf, &mut cs);
                for c in cs {
                    self.hold(&c, value);
                }
            }
            _ => {}
        }
    }

    /// An array literal two of whose elements are one object holds it
    /// twice (`p ++ [ELEM]` in `dup`).
    fn array_dup(&mut self, p: &Place, elements: &[HirExpr]) {
        let fls: Vec<Vec<Place>> = elements
            .iter()
            .map(|x| {
                let mut fl = Vec::new();
                self.flows(x, &mut fl);
                fl.into_iter().map(|(_, q)| q).collect()
            })
            .collect();
        if fls.iter().filter(|f| !f.is_empty()).count() < 2 {
            return;
        }
        for st in self.cur.states() {
            let walks: Vec<(Vec<Place>, Vec<Place>)> = fls
                .iter()
                .map(|f| {
                    let (mut inner, mut stops) = (Vec::new(), Vec::new());
                    for q in f {
                        let (x, y) = reach(st, q, &[]);
                        inner.extend(x);
                        stops.extend(y);
                    }
                    (inner, stops)
                })
                .collect();
            let mut twice = false;
            for (i, (ai, ao)) in walks.iter().enumerate() {
                for (bi, bo) in &walks[i + 1..] {
                    // Two reads of one buffer may be one element.
                    twice |= ai.iter().any(|x| bi.iter().any(|y| overlap(x, y)))
                        || ao.iter().any(|x| bo.iter().any(|y| overlap(x, y)));
                }
            }
            if twice {
                st.dup.insert(extend(p, &[ELEM]));
            }
        }
    }

    /// A call that hands something over must not get one object through
    /// two arguments (`f(tx, a, a)`, `h.go(tx, h.items)`): it may hand one
    /// over and keep using the other.
    /// Handing over `qs`, taken out of a parameter's buffer: the call
    /// hands over that buffer's elements (`Summary::sent_from`).
    fn note_sent_from(&mut self, qs: &[Place]) {
        let mut found = Vec::new();
        for (_, st) in &self.cur.0 {
            let near: Vec<Place> = qs
                .iter()
                .flat_map(|q| {
                    let (mut inner, _) = reach(st, q, &[]);
                    inner.push(q.clone());
                    inner
                })
                .collect();
            for q in &near {
                for e in st.edges.of(q.0) {
                    let Edge::Taken(a, b) = e else { continue };
                    if !overlap(q, a) {
                        continue;
                    }
                    for (sub, x) in same_reach(st, b) {
                        if let (true, Some(i)) = (sub.is_empty(), self.param_index(x.0)) {
                            found.push((i, x.1.clone()));
                        }
                    }
                }
            }
        }
        self.sent_from.extend(found);
    }

    /// How to take an element out of the collection at `p` (named `x`).
    fn take_out(&self, p: &Place, x: &str) -> String {
        let base = match self.place_ty(p) {
            Some(HirTy::Struct(id)) => {
                let structs = self.c.env.defs.structs.borrow();
                structs.get(id.0 as usize).map(|s| {
                    let n = s.name.split("__").next().unwrap_or_default();
                    n.split('<')
                        .next()
                        .unwrap_or_default()
                        .rsplit("::")
                        .next()
                        .unwrap_or_default()
                        .to_owned()
                })
            }
            _ => None,
        };
        match base.as_deref() {
            Some("Deque") => format!("`{x}.pop_front()`, `{x}.pop_back()`"),
            Some("Dict") => format!("`{x}.remove(key)`"),
            Some("BinaryHeap") => format!("`{x}.pop()`"),
            _ => format!("`{x}.remove(i)`, `{x}.pop()`"),
        }
    }

    /// `.f.g` for the field steps `rest` below place `p`.
    fn field_path(&self, p: &Place, rest: &[u32]) -> Option<String> {
        let mut t = self.place_ty(p)?;
        let mut out = String::new();
        for &k in rest {
            if k & TUPLE != 0 {
                out.push_str(&format!(".{}", k & !TUPLE));
                let HirTy::Tuple(ts) = t else { return None };
                t = ts.get((k & !TUPLE) as usize)?.clone();
                continue;
            }
            let HirTy::Struct(id) = t else { return None };
            let structs = self.c.env.defs.structs.borrow();
            let f = structs.get(id.0 as usize)?.fields.get(k as usize)?;
            out.push_str(&format!(".{}", f.name));
            t = f.ty.clone();
        }
        Some(out)
    }

    /// The index of the parameter whose local is `l`.
    fn param_index(&self, l: LocalId) -> Option<u32> {
        #[allow(clippy::cast_possible_truncation)]
        self.f
            .params
            .iter()
            .position(|p| p.local == l)
            .map(|i| i as u32)
    }

    /// Callee facts about parameters that are, at this call, parts of this
    /// function's own parameters (`uses`, `sent_from`).
    fn lift_params(&mut self, s: &Summary, args: &[&HirExpr]) {
        let lift = |w: &Self, set: &BTreeSet<Root>| -> Vec<Root> {
            set.iter()
                .filter_map(|(j, sub)| {
                    let q = place(args.get(*j as usize)?)?;
                    let path = [&q.1[..], &sub[..]].concat();
                    // Recursion through a field would grow the path forever.
                    (path.len() <= MAX_DEPTH).then_some(())?;
                    Some((w.param_index(q.0)?, path))
                })
                .collect()
        };
        let (u, f) = (lift(self, &s.uses), lift(self, &s.sent_from));
        self.uses.extend(u);
        self.sent_from.extend(f);
    }

    /// At a return: which parts of `ref` parameters handed over on this
    /// path were given a new value (the caller's variable gets it back).
    fn note_renews(&mut self) {
        let refs: Vec<(u32, LocalId)> = self
            .f
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| p.by_ref)
            .map(|(i, p)| {
                #[allow(clippy::cast_possible_truncation)]
                (i as u32, p.local)
            })
            .collect();
        for (i, l) in refs {
            let sh = shadow(i as usize);
            for (_, st) in &self.cur.0 {
                for g in st.gone.keys().filter(|g| g.0 == sh) {
                    let mine = (l, g.1.clone());
                    if st.gone.keys().any(|x| overlap(x, &mine)) {
                        self.not_renewed.insert((i, g.1.clone()));
                    } else {
                        self.renewed.insert((i, g.1.clone()));
                    }
                }
            }
        }
    }

    /// A call that hands something over must not get, through another
    /// argument, an object it hands over and then uses (`f(tx, a, a)`,
    /// `h.go(tx, h.items)` when `go` uses `self.items`). Distinct elements
    /// of one collection are taken to be distinct.
    fn aliased_args(&mut self, s: &Summary, args: &[&HirExpr], call: &HirExpr, name: Option<&str>) {
        if self.c.in_unsafe(call.span) {
            return;
        }
        let at = |w: &Self, k: usize, sub: &[u32]| -> Vec<Place> {
            let Some(arg) = args.get(k) else {
                return Vec::new();
            };
            let mut fl = Vec::new();
            w.flows(arg, &mut fl);
            fl.into_iter().map(|(_, q)| extend(&q, sub)).collect()
        };
        let handed: Vec<&Root> = s
            .gives
            .keys()
            .chain(s.sent_from.iter())
            .filter(|r| r.0 != RET_ROOT && r.0 != WORLD_ROOT)
            .collect();
        let mut hit: Option<(usize, usize)> = None;
        for (hi, hsub) in handed {
            let hs = at(self, *hi as usize, hsub);
            for (ui, usub) in s.uses.iter().filter(|(u, _)| u != hi) {
                let us = at(self, *ui as usize, usub);
                // Either side reaches the other; the elements of a
                // collection are reached as its element place.
                let shared = self.cur.0.iter().any(|(_, st)| {
                    hs.iter().any(|h| {
                        let (inner, _) = reach(st, h, &[]);
                        let hs_all = if elem_of(h).is_some() {
                            elem_aliases(st, h)
                        } else {
                            vec![h.clone()]
                        };
                        us.iter().any(|u| {
                            let (u_in, _) = reach(st, u, &[]);
                            inner.iter().any(|x| overlap(x, u))
                                || u_in.iter().any(|x| hs_all.iter().any(|y| overlap(x, y)))
                                || (elem_of(h).is_some() && {
                                    // A taken element: anything stored there.
                                    let mut probe = St {
                                        gone: BTreeMap::new(),
                                        tied: BTreeMap::new(),
                                        edges: st.edges.clone(),
                                        dup: BTreeSet::new(),
                                        stored: BTreeMap::new(),
                                    };
                                    mark_gone(
                                        &mut probe,
                                        u.clone(),
                                        Gone::To(Span::DUMMY, What::Mutex),
                                    );
                                    probe
                                        .gone
                                        .keys()
                                        .any(|x| hs_all.iter().any(|y| overlap(x, y)))
                                })
                        })
                    })
                });
                if shared {
                    hit = Some((*hi as usize, *ui as usize));
                }
            }
        }
        let Some((given, used)) = hit else { return };
        let show = |w: &Self, k: usize| {
            w.c.src(args[k].span)
                .filter(|t| t.len() <= 30 && !t.contains('\n'))
                .map_or_else(|| format!("argument {}", k + 1), |t| format!("`{t}`"))
        };
        let (a, b) = (show(self, given), show(self, used));
        let callee = name.map_or_else(|| "the call".to_owned(), |n| format!("`{n}`"));
        let head = if a == b {
            format!(
                "{a} is passed twice to {callee}, which hands one over and keeps using the other"
            )
        } else {
            format!(
                "{a} and {b} share an object, and {callee} hands over the one and keeps using \
                 the other"
            )
        };
        self.diags.push(
            RokDiagnostic::error(Code::SHARED_HANDOFF, call.span, head).with_note(
                "help: pass separate values (`.clone()` one of them) (stdlib 2.13.9)".to_owned(),
            ),
        );
    }

    /// `par::filter`'s result holds items of argument 0; `par::map`'s
    /// does when the function may return its argument.
    fn par_result(&mut self, callee: FunctionId, args: &[&HirExpr], e: &HirExpr) {
        let Some(f) = self.c.fns.get(&callee) else {
            return;
        };
        if !self.c.tracked(&e.ty) {
            return;
        }
        let filter = is_fn(&f.name, "rok::core::par::filter");
        let map = is_fn(&f.name, "rok::core::par::map");
        if !filter && !map {
            return;
        }
        if map {
            let returns_arg = match args.get(1).map(|a| &a.kind) {
                Some(HirExprKind::ClosureValue { fn_id, .. }) => {
                    self.callee(*fn_id).is_some_and(|s| {
                        s.edges.iter().any(|e| {
                            let (SEdge::Same(a, b) | SEdge::Into(a, b) | SEdge::Taken(a, b)) = e;
                            (a.0 == RET_ROOT && b.0 == 1) || (b.0 == RET_ROOT && a.0 == 1)
                        })
                    })
                }
                Some(HirExprKind::FnRef(id)) => self.callee(*id).is_some_and(|s| {
                    s.edges.iter().any(|e| {
                        let (SEdge::Same(a, b) | SEdge::Into(a, b) | SEdge::Taken(a, b)) = e;
                        (a.0 == RET_ROOT && b.0 == 0) || (b.0 == RET_ROOT && a.0 == 0)
                    })
                }),
                _ => true,
            };
            if !returns_arg {
                return;
            }
        }
        let Some(items) = args.first() else { return };
        let mut fl = Vec::new();
        self.flows(items, &mut fl);
        if fl.is_empty() {
            return;
        }
        let key = addr(e);
        let t = if let Some((_, q)) = self.results.get(&key).and_then(|v| v.first()) {
            q.clone()
        } else {
            let t = (self.temp(key, RET_ROOT, &e.ty), Vec::new());
            self.assign(&t);
            self.results
                .insert(key, vec![(Fk::Is(Vec::new()), t.clone())]);
            t
        };
        let at = extend(&t, &[0, ELEM]);
        for st in self.cur.states() {
            for (_, q) in &fl {
                st.edges
                    .insert(Edge::Same(at.clone(), extend(q, &[0, ELEM])));
            }
        }
    }

    /// `dst.extend(src)`: `dst` holds an object twice if it already
    /// holds one of `src`'s, or `src` is `dst`.
    fn copy_dup(&mut self, args: &[&HirExpr]) {
        let (Some(dst), Some(src)) = (args.first().and_then(|a| place(a)), args.get(1)) else {
            return;
        };
        let mut fl = Vec::new();
        self.flows(src, &mut fl);
        for st in self.cur.states() {
            let mut probe = St {
                gone: BTreeMap::new(),
                tied: BTreeMap::new(),
                edges: st.edges.clone(),
                dup: BTreeSet::new(),
                stored: BTreeMap::new(),
            };
            for (_, q) in &fl {
                mark_gone(&mut probe, q.clone(), Gone::To(Span::DUMMY, What::Mutex));
            }
            let held = probe
                .gone
                .keys()
                .any(|g| within(g, &dst) && g.1[dst.1.len()..].contains(&ELEM));
            if held || fl.iter().any(|(_, q)| overlap(q, &dst)) {
                st.dup.insert(extend(&dst, &[0, ELEM]));
            }
        }
    }

    /// Every element of the buffer at `p` was dropped (`clear`): what
    /// was handed over or stored there is no longer in it.
    fn cleared(&mut self, p: &Place) {
        for st in self.cur.states() {
            let aliases: Vec<Place> = same_reach(st, p)
                .into_iter()
                .filter(|(sub, _)| sub.is_empty())
                .map(|(_, a)| a)
                .collect();
            let inside = |q: &Place| {
                aliases
                    .iter()
                    .any(|a| within(q, a) && q.1[a.1.len()..].contains(&ELEM))
            };
            st.gone.retain(|g, _| !inside(g));
            st.dup.retain(|d| !inside(d));
            let locals: BTreeSet<LocalId> = aliases.iter().map(|a| a.0).collect();
            for l in locals {
                for e in st.edges.take_of(l) {
                    let (a, b) = e.ends();
                    if !inside(a) && !inside(b) {
                        st.edges.insert(e);
                    }
                }
            }
        }
    }

    /// A collection literal built from an array that holds one object
    /// twice holds it twice too.
    fn literal_dup(&mut self, s: &Summary, e: &HirExpr) {
        let key = addr(e);
        let Some(&ret) = self.temps.get(&(key, RET_ROOT)) else {
            return;
        };
        let elems: BTreeSet<Place> = s
            .edges
            .iter()
            .flat_map(|e| {
                let (SEdge::Same(a, b) | SEdge::Into(a, b) | SEdge::Taken(a, b)) = e;
                [a, b]
            })
            .filter(|r| r.0 == RET_ROOT)
            .filter_map(|r| elem_of(&(ret, r.1.clone())))
            .collect();
        let args: Vec<LocalId> = (0..8)
            .filter_map(|i| self.temps.get(&(key, i)).copied())
            .collect();
        for st in self.cur.states() {
            let mut probe = St {
                gone: BTreeMap::new(),
                tied: BTreeMap::new(),
                edges: st.edges.clone(),
                dup: BTreeSet::new(),
                stored: BTreeMap::new(),
            };
            for a in &args {
                mark_gone(
                    &mut probe,
                    (*a, Vec::new()),
                    Gone::To(Span::DUMMY, What::Mutex),
                );
            }
            let twice = probe.gone.keys().any(|g| st.dup.iter().any(|d| d.0 == g.0));
            if twice {
                st.dup.extend(elems.iter().cloned());
            }
        }
    }

    fn callee(&mut self, id: FunctionId) -> Option<Rc<Summary>> {
        self.deps.insert(id);
        self.c.summary(id).filter(|s| !s.is_empty())
    }

    /// A call through a function value with these arguments: every
    /// function used as a value with their types (argument 0 is the
    /// function value).
    fn indirect(&mut self, args: &[HirExpr]) -> Option<Rc<Summary>> {
        let c = self.c;
        let cands = c.cands.get(&args.len())?;
        let mut out = Summary::default();
        for k in cands {
            let fits = k.keys.iter().zip(args).all(|(key, a)| {
                matches!(a.ty, HirTy::Unknown | HirTy::Any) || *key == a.ty.stable_key()
            });
            if !fits {
                continue;
            }
            let s = match &k.from {
                CandFrom::Fn(id) => {
                    self.deps.insert(*id);
                    match c.summary(*id) {
                        Some(s) => s,
                        None => continue,
                    }
                }
                CandFrom::Stored(s) => s.clone(),
            };
            let env = k.env;
            out.merge(&s.remap(&|i| Some(if env { i } else { i + 1 })));
        }
        (!out.is_empty()).then(|| Rc::new(out))
    }

    /// Run summary `s` at call `e` with these arguments (`ret`: its
    /// value is the function's return value).
    fn apply(&mut self, s: &Summary, args: &[&HirExpr], e: &HirExpr, ret: bool, takes: bool) {
        self.lift_params(s, args);
        let key = addr(e);
        let used: BTreeSet<u32> = s.roots().map(|r| r.0).collect();
        // Each parameter's object: a local holding the argument.
        let mut slot: HashMap<u32, Place> = HashMap::new();
        for (i, a) in args.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let i = i as u32;
            if !used.contains(&i) || !self.c.tracked(&a.ty) {
                continue;
            }
            let t = (self.temp(key, i, &a.ty), Vec::new());
            self.store(&t, a, false);
            slot.insert(i, t);
        }
        if ret && used.contains(&RET_ROOT) && self.c.tracked(&e.ty) {
            let t = (self.temp(key, RET_ROOT, &e.ty), Vec::new());
            self.assign(&t);
            self.results
                .insert(key, vec![(Fk::Is(Vec::new()), t.clone())]);
            slot.insert(RET_ROOT, t);
        }
        if used.contains(&WORLD_ROOT) {
            slot.insert(WORLD_ROOT, (WORLD, Vec::new()));
        }
        let map = |r: &Root| slot.get(&r.0).map(|p| extend(p, &r.1));
        let (add, stores) = summary_edges(s, &map, takes);
        let trusted = TRUSTED.iter().any(|m| self.f.name.starts_with(m));
        for st in self.cur.states() {
            let mut dup = Vec::new();
            for (at, v) in &stores {
                if st.dup.contains(at) || dup.contains(at) {
                    continue;
                }
                // A copy from a buffer that may hold one object twice.
                let from_dup = elem_of(v).is_some() && is_dup(st, v);
                if from_dup || stores_twice(st, at, v, trusted) {
                    dup.push(at.clone());
                }
            }
            st.dup.extend(dup);
            for (at, _) in &stores {
                st.stored.entry(at.clone()).or_default().insert(e.span);
            }
            for edge in &add {
                if kept(edge) {
                    st.edges.insert(edge.clone());
                }
            }
            st.dup.extend(s.dups.iter().filter_map(&map));
        }
        for (g, r) in &s.ties {
            let guard = args
                .get(*g as usize)
                .and_then(|a| place(a))
                .filter(|p| p.1.is_empty());
            if let (Some(guard), Some(p)) = (guard, map(r)) {
                self.tie(guard.0, p, e.span, Tie::Taken);
            }
        }
        let given: Vec<(&Root, Place, Gone)> = s
            .gives
            .iter()
            .filter_map(|(r, why)| {
                let id = self.c.why_id(why.clone());
                Some((r, map(r)?, Gone::Via(e.span, id)))
            })
            .collect();
        // Apart by how the call ends, when its value's tag is followed.
        let dest = self.dest_of(e);
        let (Some(dest), false) = (dest, s.exits.is_empty()) else {
            for (_, p, why) in given {
                self.mark_gone(p, why);
            }
            self.renew(s, args, &slot);
            return;
        };
        let none = BTreeSet::new();
        let any = s.exits.get(&ANY).unwrap_or(&none);
        let mut out = Cur::default();
        for (t, st) in std::mem::take(&mut self.cur.0) {
            for (tag, roots) in &s.exits {
                let (mut t, mut st) = (t.clone(), st.clone());
                if *tag != ANY {
                    t.insert(dest.clone(), *tag);
                }
                for (r, p, why) in &given {
                    if roots.contains(*r) || *tag != ANY && any.contains(*r) {
                        mark_gone(&mut st, p.clone(), *why);
                    }
                }
                out.0.push((t, st));
            }
        }
        out.norm();
        self.cur = out;
        self.renew(s, args, &slot);
    }

    /// A `ref` parameter part the callee handed over and then replaced:
    /// the argument gets the new value back, its other aliases stay gone.
    fn renew(&mut self, s: &Summary, args: &[&HirExpr], slot: &HashMap<u32, Place>) {
        for (i, sub) in &s.renews {
            let mut ps: Vec<Place> = slot.get(i).map(|t| extend(t, sub)).into_iter().collect();
            if let Some(q) = args.get(*i as usize).and_then(|a| place(a)) {
                ps.push(extend(&q, sub));
            }
            // The value copied back out to the caller (`ref`): part of the
            // call's value.
            if let Some(ret) = slot.get(&RET_ROOT) {
                for e in &s.edges {
                    let SEdge::Same(x, y) = e else { continue };
                    for (r, q) in [(x, y), (y, x)] {
                        if r.0 == RET_ROOT && q.0 == *i && q.1.is_empty() {
                            ps.push(extend(ret, &[&r.1[..], &sub[..]].concat()));
                        }
                    }
                }
            }
            let mut cur = std::mem::take(&mut self.cur);
            for (_, st) in &mut cur.0 {
                for q in &ps {
                    self.regain(st, q);
                }
            }
            self.cur = cur;
        }
    }

    /// Where the tag of call `e`'s value goes.
    fn dest_of(&self, e: &HirExpr) -> Option<Place> {
        let a = addr(e);
        self.dests
            .iter()
            .rev()
            .find(|(x, _)| *x == a)
            .map(|(_, p)| p.clone())
    }

    /// `e` polled future `fut` (`poll(ref self, ref ctx)`): its value
    /// is `(step, fut, ctx)`, and the step is no error if `fut`'s body
    /// ends with none.
    fn polled(&mut self, e: &HirExpr, fut: Option<&HirExpr>) {
        let (Some(dest), Some(f)) = (self.dest_of(e), fut.and_then(tag_place)) else {
            return;
        };
        for (t, _) in &mut self.cur.0 {
            if let Some(&v) = t.get(&f) {
                t.insert(extend(&dest, &[1 | TUPLE]), v);
                if v != ERR {
                    t.insert(extend(&dest, &[TUPLE]), OK);
                }
            }
        }
    }

    fn temp(&mut self, key: usize, slot: u32, ty: &HirTy) -> LocalId {
        let n = self.temps.len();
        let l = *self.temps.entry((key, slot)).or_insert_with(|| {
            #[allow(clippy::cast_possible_truncation)]
            LocalId(TEMP + n as u32)
        });
        self.temp_tys.insert(l, ty.clone());
        l
    }

    /// A lock op or a hand-off.
    fn op(&mut self, op: Op, args: &[HirExpr], at: Span) {
        let carried = |w: &Self, i: usize| {
            let mut fl = Vec::new();
            if let Some(a) = args.get(i) {
                w.flows(a, &mut fl);
            }
            fl.into_iter().map(|(_, q)| q).collect::<Vec<_>>()
        };
        let guard = args.first().and_then(place).map(|p| p.0);
        match op {
            Op::Handoff(i, what) => {
                self.sends |= what != What::Handoff;
                if let Some(a) = args.get(i).filter(|a| place(a).is_some()) {
                    let shown = self.show(a);
                    self.c
                        .direct
                        .borrow_mut()
                        .insert((at.file, at.start), (shown, a.ty.stable_key()));
                }
                if let Some(t) = args
                    .get(i)
                    .and_then(|a| self.c.src(a.span))
                    .filter(|t| t.len() <= 40 && !t.contains('\n'))
                {
                    self.c
                        .handed
                        .borrow_mut()
                        .insert((at.file, at.start), format!("`{t}`"));
                }
                let qs = carried(self, i);
                self.note_sent_from(&qs);
                if what != What::Handoff && !self.c.in_unsafe(at) {
                    self.shared_handoff(&qs, args.get(i), what);
                    self.guarded_handoff(&qs, args.get(i), what);
                }
                for q in qs {
                    self.mark_gone(q, Gone::To(at, what));
                }
            }
            // The lock now guards the new value, not what `get` returned.
            Op::Set => {
                let Some(g) = guard else { return };
                for s in self.cur.states() {
                    s.tied.remove(&g);
                }
                for q in carried(self, 1) {
                    self.tie(g, q, at, Tie::Given);
                }
            }
            // The closure's captures may receive the guarded value or
            // hand theirs to the lock.
            Op::Update => {
                let Some(g) = guard else { return };
                for q in carried(self, 1) {
                    self.tie(g, q, at, Tie::Shared);
                }
            }
            // What it protects is unprotected now, and so is every copy of
            // the guard itself (pushed, captured, stored by a callee).
            Op::Drop => {
                let Some(g) = guard else { return };
                let guards: HashSet<LocalId> = self
                    .f
                    .locals
                    .iter()
                    .map(|l| l.id)
                    .filter(|&l| self.is_guard(l))
                    .collect();
                for s in self.cur.states() {
                    let tied = s.tied.remove(&g).unwrap_or_default();
                    let at_tie = tied.values().next().map(|(sp, _)| *sp);
                    let before: BTreeSet<Place> = s.gone.keys().cloned().collect();
                    for (q, (span, tie)) in tied {
                        mark_gone(s, q, Gone::Guard(span, g, tie));
                    }
                    // What holds a gone value one way (a coroutine frame
                    // of an `async` / `yield` fn's arguments) goes too.
                    if let Some(span) = at_tie {
                        holders_go(s, &before, Gone::Guard(span, g, Tie::Shared));
                    }
                    mark_gone_whole(s, (g, Vec::new()), Gone::Guard(at, g, Tie::Itself), &guards);
                }
            }
            Op::Get => {}
        }
    }

    /// Handing over what a lock still guards (`tx.send(g.get())` inside
    /// `using`) shares it with the lock's other users.
    fn guarded_handoff(&mut self, qs: &[Place], arg: Option<&HirExpr>, what: What) {
        let Some(arg) = arg else { return };
        let Some((g, site)) = self.view_of(qs, arg) else {
            return;
        };
        let text = self
            .c
            .src(arg.span)
            .filter(|t| t.len() <= 40 && !t.contains('\n'))
            .map_or_else(|| "this value".to_owned(), |t| format!("`{t}`"));
        let to = match what {
            What::Channel => "sent through a channel",
            What::Task => "spawned as a task",
            _ => "moved into a lock",
        };
        if g == VIEW {
            self.diags.push(
                RokDiagnostic::error(
                    Code::THREAD_LOCAL_ESCAPE,
                    arg.span,
                    format!(
                        "{text} can't be {to}: it is this thread's `ThreadLocal` value, lent to \
                         `with`'s closure"
                    ),
                )
                .with_note(format!(
                    "help: hand over a separate copy (`{}.clone()`), or take the value out with \
                     `replace` / `take` (stdlib 3.4.4)",
                    self.c.src(arg.span).unwrap_or("value")
                )),
            );
            return;
        }
        let gname = self.name(g);
        let ty = if self.guard_what(g) == What::RwLock {
            "RwLock"
        } else {
            "Mutex"
        };
        self.diags.push(
            RokDiagnostic::error(
                Code::SHARED_HANDOFF,
                arg.span,
                format!("{text} can't be {to}: the `{ty}` behind `{gname}` still holds it"),
            )
            .with_secondary(site, "taken out of the lock here")
            .with_note(format!(
                "help: hand over a deep copy (`{}`), or take it out of the lock first \
                 (`{gname}.set(..)` a new value) (stdlib 2.13.9)",
                deep_copy(self.c.src(arg.span).unwrap_or("value"))
            )),
        );
    }

    /// The guard (or `VIEW`) whose protected value `arg` (carrying `qs`)
    /// is or holds, and where it was taken out.
    fn view_of(&self, qs: &[Place], arg: &HirExpr) -> Option<(LocalId, Span)> {
        let mut hit: Option<(LocalId, Span)> = self.got_from(arg).map(|g| (g, arg.span));
        for (_, s) in &self.cur.0 {
            for (g, tied) in &s.tied {
                let mut probe = St {
                    gone: BTreeMap::new(),
                    tied: BTreeMap::new(),
                    edges: s.edges.clone(),
                    dup: BTreeSet::new(),
                    stored: BTreeMap::new(),
                };
                for q in tied.keys() {
                    mark_gone(&mut probe, q.clone(), Gone::To(Span::DUMMY, What::Mutex));
                }
                if qs.iter().any(|q| probe.gone.keys().any(|x| overlap(x, q))) {
                    hit = tied.values().next().map(|(sp, _)| (*g, *sp));
                }
            }
        }
        hit
    }

    /// A call whose callee keeps an argument that is a scoped view (what
    /// a lock protects, a `with` closure's value): the view would outlive
    /// its scope.
    fn kept_views(
        &mut self,
        s: &Summary,
        args: &[&HirExpr],
        call: &HirExpr,
        callee: Option<&HirExpr>,
    ) {
        if s.keeps.is_empty() || self.c.in_unsafe(call.span) {
            return;
        }
        let trusted = match &call.kind {
            HirExprKind::Call { callee, .. } => self
                .c
                .fns
                .get(callee)
                .is_some_and(|f| TRUSTED.iter().any(|m| f.name.starts_with(m))),
            _ => false,
        };
        let params: HashSet<LocalId> = self.f.params.iter().map(|p| p.local).collect();
        for &i in &s.keeps {
            let Some(arg) = args.get(i as usize).copied() else {
                continue;
            };
            if !self.c.tracked(&arg.ty)
                || place(arg).is_some_and(|p| p.1.is_empty() && self.is_guard(p.0))
            {
                continue;
            }
            // A collection of this body keeping it is followed like the
            // view itself (`live.push(t)`, then `g.set(live)`).
            let into_local = trusted
                && args.iter().enumerate().all(|(j, a)| {
                    j == i as usize
                        || !self.c.tracked(&a.ty)
                        || place(a).is_some_and(|p| !params.contains(&p.0))
                });
            if into_local {
                continue;
            }
            let mut fl = Vec::new();
            self.flows(arg, &mut fl);
            let qs: Vec<Place> = fl.into_iter().map(|(_, q)| q).collect();
            let Some((g, site)) = self.view_of(&qs, arg) else {
                continue;
            };
            {
                let mut k = self.c.kept_at.borrow_mut();
                for sp in [site, call.span, arg.span] {
                    k.insert((sp.file, sp.start));
                }
            }
            if !self
                .reported
                .insert(((g, vec![u32::MAX]), arg.span.file, arg.span.start))
            {
                continue;
            }
            let text = self
                .c
                .src(arg.span)
                .filter(|t| t.len() <= 40 && !t.contains('\n'));
            let shown = text.map_or_else(|| "this value".to_owned(), |t| format!("`{t}`"));
            let named = callee.filter(|c| place(c).is_some_and(|p| !self.hidden(p.0)));
            let who = match (callee.map(|c| &c.kind), &call.kind) {
                (_, HirExprKind::Call { callee: id, .. }) | (Some(HirExprKind::FnRef(id)), _) => {
                    self.c
                        .fns
                        .get(id)
                        .map(|f| format!("`{}`", shown_fn(self.c, f)))
                }
                _ => named.map(|c| format!("`{}`", self.show(c))),
            }
            .unwrap_or_else(|| "the function called".to_owned());
            let d = if g == VIEW {
                RokDiagnostic::error(
                    Code::THREAD_LOCAL_ESCAPE,
                    arg.span,
                    format!(
                        "{shown} is this thread's `ThreadLocal` value, lent to `with`'s closure, \
                         and {who} keeps it past the call"
                    ),
                )
                .with_note(format!(
                    "help: the value may not leave the closure; pass a copy (`{}`), or take it \
                     out with `replace` / `take` (stdlib 3.4.4)",
                    deep_copy(text.unwrap_or("value"))
                ))
            } else {
                let gname = self.name(g);
                let ty = if self.guard_what(g) == What::RwLock {
                    "RwLock"
                } else {
                    "Mutex"
                };
                let copy = self
                    .c
                    .src(site)
                    .filter(|t| t.len() <= 40 && !t.contains('\n'))
                    .map_or_else(|| format!("{gname}.get_clone()"), deep_copy);
                let d = RokDiagnostic::error(
                    Code::USE_AFTER_UNLOCK,
                    arg.span,
                    format!(
                        "{shown} is a view of what the `{ty}` behind `{gname}` protects, and {who} \
                         keeps it past the call"
                    ),
                );
                let d = if site == arg.span {
                    d
                } else {
                    d.with_secondary(site, "taken out of the lock here")
                };
                d.with_note(format!(
                    "help: `{gname}`'s `using` block releases the lock while {who} still holds \
                     it; pass a deep copy, `{copy}` (stdlib 2.16.3)"
                ))
            };
            self.diags.push(d);
        }
    }

    /// A `with` closure keeps its value (a capture, what other threads
    /// reach) past the call.
    fn view_kept(&mut self, name: &str, at: Span) {
        self.diags.push(
            RokDiagnostic::error(
                Code::THREAD_LOCAL_ESCAPE,
                at,
                format!(
                    "this closure keeps `{name}`, this thread's `ThreadLocal` value, past \
                     `with`: the value is only lent to the closure"
                ),
            )
            .with_note(format!(
                "help: keep a copy (`{name}.clone()`), or take the value out with `replace` / \
                 `take` (stdlib 3.4.4)"
            )),
        );
    }

    /// A `using` block whose value is what its lock protects: the block's
    /// end releases the lock before that value is used.
    fn block_value_escapes(&mut self, site: Span, guard: LocalId, at: Span) {
        if self.reported.contains(&((guard, Vec::new()), 0, 0)) {
            return;
        }
        if !self
            .reported
            .insert(((guard, Vec::new()), site.file, site.start))
        {
            return;
        }
        let gname = self.name(guard);
        let ty = if self.guard_what(guard) == What::RwLock {
            "RwLock"
        } else {
            "Mutex"
        };
        let copy = self
            .c
            .src(site)
            .filter(|t| t.len() <= 40 && !t.contains('\n'))
            .map_or_else(
                || format!("`{gname}.get_clone()`"),
                |t| format!("`{}`", deep_copy(t)),
            );
        self.diags.push(
            RokDiagnostic::error(
                Code::USE_AFTER_UNLOCK,
                at,
                format!(
                    "the value of `{gname}`'s `using` block is what the `{ty}` protects, and the \
                     block releases the lock before it is used"
                ),
            )
            .with_secondary(site, "taken out of the lock here")
            .with_note(format!(
                "help: use it inside the `using` block, or make the block's value {copy} \
                 (stdlib 2.13.9)"
            )),
        );
    }

    /// A `throw` of `err`: its error leaves like a returned value, so it
    /// is checked as one. What that store itself reports is dropped; an
    /// error already at the `throw` stands alone.
    fn throw_escapes(&mut self, err: &'h HirExpr) {
        let at = err.span;
        if self.diags.iter().any(|d| {
            d.is_error()
                && d.primary
                    .as_ref()
                    .is_some_and(|p| p.span.file == at.file && p.span.start == at.start)
        }) {
            return;
        }
        let (cur, n, reported) = (self.cur.clone(), self.diags.len(), self.reported.clone());
        let returning = std::mem::replace(&mut self.returning, true);
        self.store(&(RET, Vec::new()), err, false);
        self.returning = returning;
        self.diags.truncate(n);
        self.reported.clone_from(&reported);
        self.escapes_guard(err, Exit::Throw);
        self.reported = reported;
        self.cur = cur;
    }

    /// The guard whose `get()` value `e` holds as a part (a field of a
    /// thrown struct, through the compiler's temporaries), and where.
    fn holds_get(&self, e: &HirExpr, depth: u32) -> Option<(LocalId, Span)> {
        if depth > 8 {
            return None;
        }
        if let Some(g) = self.got_from(e) {
            return Some((g, e.span));
        }
        match &e.kind {
            HirExprKind::StructInit { fields: xs, .. }
            | HirExprKind::Tuple { fields: xs }
            | HirExprKind::EnumVariantInit { args: xs, .. }
            | HirExprKind::ArrayInit { elements: xs, .. } => {
                xs.iter().find_map(|x| self.holds_get(x, depth + 1))
            }
            HirExprKind::SumWrap { value: Some(x), .. } => self.holds_get(x, depth + 1),
            HirExprKind::Block(b) => self.holds_get(b.trailing.as_ref()?, depth + 1),
            HirExprKind::Local(x) if self.hidden(*x) => {
                let mut init = None;
                let body = self.f.body.as_ref()?;
                let mut find = |b: &'h HirBlock| {
                    for s in &b.stmts {
                        if let HirStmt::Let { local, init: i } = s {
                            if local == x {
                                init = Some(i);
                            }
                        }
                    }
                };
                find(body);
                crate::each_block(body, &mut |e| {
                    if let HirExprKind::Block(b) = &e.kind {
                        find(b);
                    }
                });
                self.holds_get(init?, depth + 1)
            }
            _ => None,
        }
    }

    /// A `return`, `yield` or `throw` inside a `using` block hands out
    /// what the lock protects: the block's end releases the lock before the
    /// caller is done with it. So does a `with` closure's value leaving it.
    fn escapes_guard(&mut self, e: &HirExpr, exit: Exit) {
        let yields = exit == Exit::Yield;
        let at = e.span;
        let params: HashSet<LocalId> = self.f.params.iter().map(|p| p.local).collect();
        // A variable's use reports it; a compiler temporary's does not.
        let temp = place(e).is_none_or(|p| self.hidden(p.0));
        let mut hit: Option<(LocalId, Span)> = None;
        for (_, s) in &self.cur.0 {
            // Taken out of the lock into a compiler temporary, which the
            // block's end released (`return g.get()` inside `using`).
            for (q, why) in &s.gone {
                if let (true, true, Gone::Guard(site, g, _)) = (temp, q.0 == RET, why) {
                    hit = Some((*g, *site));
                }
            }
            for (g, tied) in &s.tied {
                if params.contains(g) {
                    continue;
                }
                let mut probe = St {
                    gone: BTreeMap::new(),
                    tied: BTreeMap::new(),
                    edges: s.edges.clone(),
                    dup: BTreeSet::new(),
                    stored: BTreeMap::new(),
                };
                for q in tied.keys() {
                    mark_gone(&mut probe, q.clone(), Gone::To(Span::DUMMY, What::Mutex));
                }
                if probe.gone.keys().any(|q| q.0 == RET) {
                    let site = tied.values().map(|(sp, _)| *sp).next().unwrap_or(at);
                    hit = Some((*g, site));
                }
            }
        }
        if exit == Exit::Throw {
            hit = hit.or_else(|| self.holds_get(e, 0));
        }
        let Some((g, site)) = hit else { return };
        // One report: not again as the block's value.
        self.reported.insert(((g, Vec::new()), 0, 0));
        if !self
            .reported
            .insert(((RET, Vec::new()), site.file, site.start))
        {
            return;
        }
        if g == VIEW {
            let how = match exit {
                Exit::Return => "returns",
                Exit::Yield => "yields",
                Exit::Throw => "throws",
            };
            let d = RokDiagnostic::error(
                Code::THREAD_LOCAL_ESCAPE,
                at,
                format!(
                    "this {how} this thread's `ThreadLocal` value out of `with`'s closure: the \
                     value is only lent to it"
                ),
            );
            self.diags.push(d.with_note(
                "help: return a copy (`.clone()`) or what you computed from it, or take the \
                 value out with `replace` / `take` (stdlib 3.4.4)",
            ));
            return;
        }
        let gname = self.name(g);
        let ty = if self.guard_what(g) == What::RwLock {
            "RwLock"
        } else {
            "Mutex"
        };
        let clone_trait = self.c.env.names.lang_item("clone_trait");
        let cloneable = clone_trait.is_empty() || self.c.ctx.has_trait(&e.ty, clone_trait);
        // A copy of what is returned (an element, not the whole list).
        let copy = self
            .c
            .src(at)
            .map(|t| {
                t.trim_start_matches("return")
                    .trim_start_matches("yield")
                    .trim()
                    .trim_end_matches(';')
                    .trim()
            })
            .filter(|t| !t.is_empty())
            .or_else(|| self.c.src(site))
            .filter(|t| t.len() <= 40 && !t.contains('\n'))
            .map_or_else(
                || "a separate copy (`.clone()`)".to_owned(),
                |t| format!("`{}`", deep_copy(t)),
            );
        let copy = if cloneable {
            copy
        } else {
            "a value built before it was shared (keep one outside the lock, and give the lock \
             its own)"
                .to_owned()
        };
        if exit == Exit::Throw {
            let taken = self
                .c
                .src(site)
                .filter(|t| t.len() <= 40 && !t.contains('\n'))
                .map_or_else(
                    || "a deep copy (`.clone()`)".to_owned(),
                    |t| format!("a deep copy, `{}`", deep_copy(t)),
                );
            self.diags.push(
                RokDiagnostic::error(
                    Code::USE_AFTER_UNLOCK,
                    at,
                    format!(
                        "this throws a value out of the `{ty}`, and `{gname}`'s `using` block \
                         releases the lock before the catcher uses it"
                    ),
                )
                .with_secondary(site, "taken out of the lock here")
                .with_note(format!(
                    "help: the lock stops protecting it; build the error from {taken}, or \
                     handle it inside the `using` block (stdlib 2.13.9)"
                )),
            );
            return;
        }
        self.diags.push(
            RokDiagnostic::error(
                Code::USE_AFTER_UNLOCK,
                at,
                if yields {
                    format!(
                        "this yields a value out of the `{ty}`, and `{gname}`'s `using` block \
                         releases the lock while the caller may still use it"
                    )
                } else {
                    format!(
                        "this returns a value out of the `{ty}`, and `{gname}`'s `using` block \
                         releases the lock before the caller uses it"
                    )
                },
            )
            .with_secondary(site, "taken out of the lock here")
            .with_note(format!(
                "help: the lock stops protecting it; {} {copy}, or do the work inside the \
                 `using` block (stdlib 2.13.9)",
                if yields { "yield" } else { "return" }
            )),
        );
    }

    /// Handing over a value other threads may reach without this hand-off
    /// (a thread-local's value, a field of a `Sync` object) is an error.
    fn shared_handoff(&mut self, qs: &[Place], arg: Option<&HirExpr>, what: What) {
        let Some(arg) = arg else { return };
        let shared = self.cur.0.iter().any(|(_, s)| {
            let mut probe = St {
                gone: BTreeMap::new(),
                tied: BTreeMap::new(),
                edges: s.edges.clone(),
                dup: s.dup.clone(),
                stored: BTreeMap::new(),
            };
            for q in qs {
                mark_gone(&mut probe, q.clone(), Gone::To(Span::DUMMY, What::Mutex));
            }
            probe.gone.keys().any(|g| g.0 == WORLD)
        });
        if !shared {
            return;
        }
        let text = self
            .c
            .src(arg.span)
            .filter(|t| t.len() <= 40 && !t.contains('\n'))
            .map_or_else(|| "this value".to_owned(), |t| format!("`{t}`"));
        let to = match what {
            What::Channel => "sent through a channel",
            What::Task => "spawned as a task",
            _ => "moved into a lock",
        };
        // The object it is read out of: a call's receiver (`TL.get()`).
        let from = match &arg.kind {
            HirExprKind::Call { args, .. } => args.first().and_then(|a| self.c.src(a.span)),
            HirExprKind::FieldRead { target, .. } => self.c.src(target.span),
            _ => None,
        }
        .filter(|t| t.len() <= 30 && !t.contains('\n'))
        .map_or_else(String::new, |t| format!(" `{t}`"));
        self.diags.push(
            RokDiagnostic::error(
                Code::SHARED_HANDOFF,
                arg.span,
                format!(
                    "{text} can't be {to}: other code reaches it through{from} (a thread-local \
                     or a `Sync` object), so it would be used from two threads"
                ),
            )
            .with_note(format!(
                "help: hand over a separate copy: `{}.clone()` (stdlib 2.13.9)",
                self.c.src(arg.span).unwrap_or("value")
            )),
        );
    }

    /// What `exit` has handed over of the parameters.
    fn gives(&self, exit: &St) -> BTreeMap<Root, Why> {
        let mut out = BTreeMap::new();
        let root_of = |l: LocalId| -> Option<u32> {
            if l == RET {
                Some(RET_ROOT)
            } else if (SHADOW..TEMP).contains(&l.0) {
                Some(l.0 - SHADOW)
            } else {
                None
            }
        };
        let me = shown_fn(self.c, self.f);
        for (q, g) in &exit.gone {
            let Some(r) = root_of(q.0).filter(|&r| r != RET_ROOT) else {
                continue;
            };
            // A `Handoff` protocol is the `unsafe` code's: it ends before
            // the function that made it returns.
            if matches!(g, Gone::To(_, What::Handoff)) {
                continue;
            }
            let why = match *g {
                Gone::To(at, what) => Why {
                    what,
                    func: me.clone(),
                    file: self.c.file_name(at.file),
                    line: at.line,
                },
                Gone::Via(_, id) => self.c.whys.borrow().0[id as usize].clone(),
                Gone::Guard(at, guard, _) => Why {
                    what: self.guard_what(guard),
                    func: me.clone(),
                    file: self.c.file_name(at.file),
                    line: at.line,
                },
            };
            out.insert((r, q.1.clone()), why);
        }
        out
    }

    /// The walk's exit state as its callers see it.
    fn summarize(&self) -> Summary {
        let mut s = Summary {
            sends: self.sends,
            uses: self.uses.clone(),
            sent_from: self.sent_from.clone(),
            renews: self
                .renewed
                .difference(&self.not_renewed)
                .cloned()
                .collect(),
            ..Summary::default()
        };
        let Some(exit) = self.exits.values().cloned().reduce(join_st) else {
            return s;
        };
        let exit = &exit;
        s.gives = self.gives(exit);
        if !self.mixed && self.exits.keys().any(|&t| t != ANY) {
            s.exits = self
                .exits
                .iter()
                .map(|(t, st)| (*t, self.gives(st).into_keys().collect()))
                .collect();
            s.trim();
        }
        let root_of = |l: LocalId| -> Option<u32> {
            if l == RET {
                Some(RET_ROOT)
            } else if l == WORLD {
                Some(WORLD_ROOT)
            } else if (SHADOW..TEMP).contains(&l.0) {
                Some(l.0 - SHADOW)
            } else {
                None
            }
        };
        let to_roots = |p: &Place| -> Vec<Root> {
            same_reach(exit, p)
                .into_iter()
                .filter(|(sub, _)| sub.is_empty())
                .filter_map(|(_, x)| Some((root_of(x.0)?, x.1)))
                .collect()
        };
        for e in exit.edges.0.values().flatten() {
            if let Edge::Taken(a, b) = e {
                for ra in to_roots(a) {
                    for rb in to_roots(b) {
                        s.edges.insert(SEdge::Taken(ra.clone(), rb));
                    }
                }
            }
        }
        for d in &exit.dup {
            s.dups.extend(to_roots(d));
        }
        let mut roots: Vec<(u32, Place)> = self
            .f
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| self.c.tracked(&p.ty))
            .map(|(i, _)| {
                #[allow(clippy::cast_possible_truncation)]
                (i as u32, (shadow(i), Vec::new()))
            })
            .collect();
        if self.c.tracked(&self.f.ret_ty) {
            roots.push((RET_ROOT, (RET, Vec::new())));
        }
        roots.push((WORLD_ROOT, (WORLD, Vec::new())));
        for (r, start) in &roots {
            let same = same_reach(exit, start);
            for (sub, q) in &same {
                if let Some(r2) = root_of(q.0) {
                    if (r2, &q.1) != (*r, sub) {
                        let (a, b) = ((*r, sub.clone()), (r2, q.1.clone()));
                        s.edges
                            .insert(SEdge::Same(a.clone().min(b.clone()), a.max(b)));
                    }
                }
            }
            let mut probe = St {
                gone: BTreeMap::new(),
                tied: BTreeMap::new(),
                edges: exit.edges.clone(),
                dup: exit.dup.clone(),
                stored: BTreeMap::new(),
            };
            mark_gone(
                &mut probe,
                start.clone(),
                Gone::To(Span::DUMMY, What::Mutex),
            );
            for q in probe.gone.keys() {
                let Some(r2) = root_of(q.0).filter(|r2| r2 != r) else {
                    continue;
                };
                let implied = same.iter().any(|(_, p)| within(q, p));
                if !implied {
                    s.edges
                        .insert(SEdge::Into((*r, Vec::new()), (r2, q.1.clone())));
                }
            }
        }
        for (r, start) in &roots {
            if *r != RET_ROOT && *r != WORLD_ROOT && keeps_param(exit, start, *r, &root_of) {
                s.keeps.insert(*r);
            }
        }
        for (g, tied) in &exit.tied {
            let Some(i) = self.f.params.iter().position(|p| p.local == *g) else {
                continue;
            };
            let mut probe = St {
                gone: BTreeMap::new(),
                tied: BTreeMap::new(),
                edges: exit.edges.clone(),
                dup: exit.dup.clone(),
                stored: BTreeMap::new(),
            };
            for q in tied.keys() {
                mark_gone(&mut probe, q.clone(), Gone::To(Span::DUMMY, What::Mutex));
            }
            for q in probe.gone.keys() {
                if let Some(r) = root_of(q.0) {
                    #[allow(clippy::cast_possible_truncation)]
                    s.ties.insert((i as u32, (r, q.1.clone())));
                }
            }
        }
        s
    }
}

fn addr(e: &HirExpr) -> usize {
    std::ptr::from_ref(e) as usize
}
