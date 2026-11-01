//! Strip mining of counted loops. An innermost rotated loop `ctr < bound`
//! stepped by one, whose body reaches no safepoint, polls on every back
//! edge: its trip count is unknown. Split into strips of at most
//! `SHORT_TRIPS` iterations, `ctr < lim` with `lim = min(bound, ctr +
//! SHORT_TRIPS)`, the inner loop is short (`MirFunction::strips`) and
//! needs no poll; the strip loop around it polls once a strip. The
//! inner test replaces the loop's own, so a strip costs a few
//! instructions in front and one test behind.
//!
//! ```text
//! E:  lim = bound; if bound - ctr > K { lim = ctr + K }   (E1)
//! H:  body ..
//! L:  ctr += 1; if ctr < lim then H else S
//! S:  if ctr < bound then E else exit
//! ```

use std::collections::{HashMap, HashSet};

use rokc_ir_common::{BlockId, ConstVal, FunctionId, Linkage, LocalId, PrimOp, PrimTy};

use crate::rotate::{SHORT_TRIPS, back_edges, loop_body, retarget, succs};
use crate::version::{Test, same, test_into};
use crate::{BasicBlock, LocalDecl, MirFunction, MirTy, Operand, RValue, Statement, Terminator};

/// Statement `s` reaches no safepoint, calling only `leaf` functions.
fn quiet(s: &Statement, leaf: &HashSet<FunctionId>) -> bool {
    match s {
        Statement::Assign { rvalue, .. } => match rvalue {
            RValue::Call { callee, .. } => leaf.contains(callee),
            RValue::CallIndirect { .. }
            | RValue::StaticLoad { .. }
            | RValue::StaticAddr(_)
            | RValue::HeapBox { .. }
            | RValue::BufAlloc { .. }
            | RValue::HeapAlloc { .. } => false,
            _ => true,
        },
        Statement::StaticStore { .. } => false,
        _ => true,
    }
}

/// Block `b` reaches no safepoint, calling only `leaf` functions; with
/// `panics`, but in a call that never returns (a panic: codegen's rule,
/// `polled_back_edges`). Only nests take that: a loop whose panics are
/// quiet is a checked one, rarely the one that runs.
fn block_quiet(b: &BasicBlock, leaf: &HashSet<FunctionId>, panics: bool) -> bool {
    let skip = crate::rotate::noreturn_call(b).filter(|_| panics);
    b.stmts
        .iter()
        .enumerate()
        .all(|(i, s)| Some(i) == skip || quiet(s, leaf))
}

/// Functions without loops that reach no safepoint: a subset of what
/// codegen finds quiet, which decides the polls.
fn leaves(fns: &[MirFunction], panics: bool) -> HashSet<FunctionId> {
    let mut out: HashSet<FunctionId> = HashSet::new();
    loop {
        let before = out.len();
        for f in fns {
            if out.contains(&f.id)
                || f.blocks.is_empty()
                || f.facts.linkage != Linkage::Defined
                || !back_edges(f).is_empty()
            {
                continue;
            }
            if f.blocks.iter().all(|b| {
                !matches!(b.terminator, Terminator::Suspend { .. }) && block_quiet(b, &out, panics)
            }) {
                out.insert(f.id);
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

fn fresh(func: &mut MirFunction, ty: MirTy) -> LocalId {
    let id = LocalId(u32::try_from(func.locals.len()).expect("local count"));
    func.locals.push(LocalDecl { id, name: None, ty });
    id
}

const fn assign(target: LocalId, rvalue: RValue) -> Statement {
    Statement::Assign { target, rvalue }
}

const fn int(value: i128, ty: PrimTy) -> Operand {
    Operand::Const(ConstVal::Int { value, ty })
}

/// The unsigned type of `ty`'s width.
const fn unsigned(ty: PrimTy) -> Option<PrimTy> {
    Some(match ty {
        PrimTy::I8 | PrimTy::U8 => PrimTy::U8,
        PrimTy::I16 | PrimTy::U16 => PrimTy::U16,
        PrimTy::I32 | PrimTy::U32 => PrimTy::U32,
        PrimTy::I64 | PrimTy::U64 => PrimTy::U64,
        PrimTy::Isize | PrimTy::Usize => PrimTy::Usize,
        _ => return None,
    })
}

/// Whether `ctr < bound` holds on the edge from `from` to `to`: `from`
/// tests it, or sets neither and it holds on every way into `from` (the
/// bounds tests `version` puts in front of a loop).
fn holds(
    func: &MirFunction,
    preds: &HashMap<u32, Vec<u32>>,
    t: &Test,
    from: u32,
    to: u32,
    seen: &mut HashSet<u32>,
) -> bool {
    let Some(b) = func.blocks.iter().find(|b| b.id.0 == from) else {
        return false;
    };
    if test_into(b, to).is_some_and(|u| u.ctr == t.ctr && same(&u.bound, &t.bound) && u.ty == t.ty)
    {
        return true;
    }
    let sets = |l: LocalId| {
        b.params.contains(&l)
            || b.stmts
                .iter()
                .any(|s| crate::liveness::stmt_uses(s).1 == Some(l))
    };
    let bound_set = matches!(t.bound, Operand::Copy(n) if sets(n));
    let ps = preds.get(&from).map_or(&[][..], Vec::as_slice);
    seen.insert(from)
        && !sets(t.ctr)
        && !bound_set
        && !ps.is_empty()
        && ps.iter().all(|&p| holds(func, preds, t, p, from, seen))
}

/// Every latch of `h` tests `ctr < bound` into it, and it holds on every
/// other way in.
fn loop_test(func: &MirFunction, h: u32, ls: &[u32]) -> Option<Test> {
    let mut test: Option<Test> = None;
    for l in ls {
        let t = test_into(func.blocks.iter().find(|b| b.id.0 == *l)?, h)?;
        if let Some(seen) = &test {
            if seen.ctr != t.ctr || !same(&seen.bound, &t.bound) || seen.ty != t.ty {
                return None;
            }
        }
        test = Some(t);
    }
    let test = test?;
    let mut preds: HashMap<u32, Vec<u32>> = HashMap::new();
    for b in &func.blocks {
        for s in succs(b) {
            preds.entry(s).or_default().push(b.id.0);
        }
    }
    let entries: Vec<u32> = preds[&h]
        .iter()
        .copied()
        .filter(|p| !ls.contains(p))
        .collect();
    (!entries.is_empty()
        && entries
            .iter()
            .all(|&p| holds(func, &preds, &test, p, h, &mut HashSet::new())))
    .then_some(test)
}

/// The loop of header `h` (latches `ls`) to strip: its test, and its
/// blocks. `nested`: it may hold other loops (each bounding itself);
/// `panics`: see `block_quiet`.
fn eligible(
    func: &MirFunction,
    h: u32,
    ls: &[u32],
    headers: &HashSet<u32>,
    leaf: &HashSet<FunctionId>,
    nested: bool,
    panics: bool,
) -> Option<(Test, HashSet<u32>)> {
    let blocks: HashMap<u32, &BasicBlock> = func.blocks.iter().map(|b| (b.id.0, b)).collect();
    let body = loop_body(func, h, ls);
    let test = loop_test(func, h, ls)?;
    unsigned(test.ty)?;
    let addressed = crate::addressed(func);
    if test.hi.is_some()
        || !blocks.get(&h)?.params.is_empty()
        || addressed.contains(&test.ctr)
        || (!nested && body.iter().any(|b| *b != h && headers.contains(b)))
    {
        return None;
    }
    if let Operand::Copy(n) = &test.bound {
        if addressed.contains(n) || *n == test.ctr {
            return None;
        }
    }
    let step = |s: &Statement| {
        matches!(s, Statement::Assign {
            target,
            rvalue: RValue::BinOp {
                op: PrimOp::Add,
                op_ty: MirTy::Prim(t),
                lhs: Operand::Copy(x),
                rhs: Operand::Const(ConstVal::Int { value: 1, .. } | ConstVal::UInt { value: 1, .. }),
            },
        } if *target == test.ctr && *x == test.ctr && *t == test.ty)
    };
    for id in &body {
        let b = blocks[id];
        if matches!(b.terminator, Terminator::Suspend { .. })
            || b.params
                .iter()
                .any(|p| *p == test.ctr || same(&test.bound, &Operand::Copy(*p)))
            || !block_quiet(b, leaf, panics)
        {
            return None;
        }
        let mut steps = 0;
        for s in &b.stmts {
            match crate::liveness::stmt_uses(s).1 {
                Some(d) if d == test.ctr && step(s) => steps += 1,
                Some(d) if d == test.ctr || same(&test.bound, &Operand::Copy(d)) => return None,
                _ => {}
            }
        }
        // One step on every way back: in each latch (its test into `h`
        // follows it), nowhere else.
        if steps != usize::from(ls.contains(id)) {
            return None;
        }
    }
    Some((test, body))
}

/// Strip the loop of header `header`.
fn apply(func: &mut MirFunction, header: u32, latches: &[u32], test: &Test) {
    let Test {
        ctr, ref bound, ty, ..
    } = *test;
    let uty = unsigned(ty).expect("integer counter");
    let mut next = func.blocks.iter().map(|b| b.id.0 + 1).max().unwrap_or(0);
    let mut new_id = || {
        next += 1;
        next - 1
    };
    let (front, front1) = (new_id(), new_id());
    let lim = fresh(func, MirTy::Prim(ty));
    let (dist, udist, big) = (
        fresh(func, MirTy::Prim(ty)),
        fresh(func, MirTy::Prim(uty)),
        fresh(func, MirTy::Prim(PrimTy::Bool)),
    );
    // Into the loop `ctr < bound`, so `bound - ctr` fits the unsigned type.
    for b in &mut func.blocks {
        if !latches.contains(&b.id.0) {
            retarget(&mut b.terminator, header, front);
        }
    }
    let mut strips = Vec::new();
    for b in func.blocks.iter_mut().filter(|b| latches.contains(&b.id.0)) {
        let Terminator::If {
            cond: Operand::Copy(c),
            else_bb,
            else_args,
            ..
        } = &mut b.terminator
        else {
            unreachable!("a latch tests");
        };
        let c = *c;
        let end = new_id();
        let (exit, args) = (*else_bb, std::mem::take(else_args));
        *else_bb = BlockId(end);
        for st in b.stmts.iter_mut().rev() {
            if let Statement::Assign {
                target,
                rvalue: RValue::BinOp { rhs, .. },
            } = st
            {
                if *target == c {
                    *rhs = Operand::Copy(lim);
                    break;
                }
            }
        }
        strips.push(BasicBlock {
            id: BlockId(end),
            params: vec![],
            stmts: vec![assign(
                c,
                RValue::BinOp {
                    op: PrimOp::Lt,
                    op_ty: MirTy::Prim(ty),
                    lhs: Operand::Copy(ctr),
                    rhs: bound.clone(),
                },
            )],
            terminator: Terminator::If {
                cond: Operand::Copy(c),
                then_bb: BlockId(front),
                then_args: vec![],
                else_bb: exit,
                else_args: args,
            },
        });
    }
    func.blocks.extend(strips);
    func.blocks.push(BasicBlock {
        id: BlockId(front),
        params: vec![],
        stmts: vec![
            assign(
                dist,
                RValue::BinOp {
                    op: PrimOp::WrappingSub,
                    op_ty: MirTy::Prim(ty),
                    lhs: bound.clone(),
                    rhs: Operand::Copy(ctr),
                },
            ),
            assign(
                udist,
                RValue::Cast {
                    source: Operand::Copy(dist),
                    from_ty: MirTy::Prim(ty),
                    to_ty: MirTy::Prim(uty),
                },
            ),
            assign(
                big,
                RValue::BinOp {
                    op: PrimOp::Gt,
                    op_ty: MirTy::Prim(uty),
                    lhs: Operand::Copy(udist),
                    rhs: int(SHORT_TRIPS, uty),
                },
            ),
            assign(lim, RValue::Use(bound.clone())),
        ],
        terminator: Terminator::If {
            cond: Operand::Copy(big),
            then_bb: BlockId(front1),
            then_args: vec![],
            else_bb: BlockId(header),
            else_args: vec![],
        },
    });
    func.blocks.push(BasicBlock {
        id: BlockId(front1),
        params: vec![],
        stmts: vec![assign(
            lim,
            RValue::BinOp {
                op: PrimOp::WrappingAdd,
                op_ty: MirTy::Prim(ty),
                lhs: Operand::Copy(ctr),
                rhs: int(SHORT_TRIPS, ty),
            },
        )],
        terminator: Terminator::Goto {
            target: BlockId(header),
            args: vec![],
        },
    });
    func.strips.push(header);
}

/// The two-pointer loop of header `h` (`while lo < hi`) whose body
/// reaches no safepoint, `lo` stepped up by one on every way back and
/// `hi` at most down by one: it runs at most `hi0 - lo0` times. Its
/// test, and its blocks.
fn two_pointer(
    func: &MirFunction,
    h: u32,
    ls: &[u32],
    headers: &HashSet<u32>,
    leaf: &HashSet<FunctionId>,
) -> Option<(Test, HashSet<u32>)> {
    let blocks: HashMap<u32, &BasicBlock> = func.blocks.iter().map(|b| (b.id.0, b)).collect();
    let body = loop_body(func, h, ls);
    let hb = blocks.get(&h)?;
    let test = crate::version::two_test(hb, &body)?;
    let (lo, hi) = (test.ctr, test.hi?);
    unsigned(test.ty)?;
    let addressed = crate::addressed(func);
    if !hb.params.is_empty()
        || addressed.contains(&lo)
        || addressed.contains(&hi)
        || body.iter().any(|b| *b != h && headers.contains(b))
    {
        return None;
    }
    let step = |s: &Statement, l: LocalId, op: PrimOp| {
        matches!(s, Statement::Assign {
            target,
            rvalue: RValue::BinOp {
                op: o,
                op_ty: MirTy::Prim(t),
                lhs: Operand::Copy(x),
                rhs: Operand::Const(ConstVal::Int { value: 1, .. } | ConstVal::UInt { value: 1, .. }),
            },
        } if *target == l && *x == l && *o == op && *t == test.ty)
    };
    for id in &body {
        let b = blocks[id];
        if matches!(b.terminator, Terminator::Suspend { .. })
            || b.params.iter().any(|p| *p == lo || *p == hi)
            || !block_quiet(b, leaf, false)
        {
            return None;
        }
        // Each pointer is set only in a latch (whose way on is back to
        // `h`, where `lo < hi` is tested again), once: there `lo < hi`
        // still holds, so neither step wraps.
        let latch = ls.contains(id)
            && matches!(&b.terminator,
                Terminator::Goto { target, args } if target.0 == h && args.is_empty());
        let (mut ups, mut downs) = (0, 0);
        for s in &b.stmts {
            match crate::liveness::stmt_uses(s).1 {
                Some(d) if d == lo && latch && step(s, lo, PrimOp::Add) => ups += 1,
                Some(d) if d == hi && latch && step(s, hi, PrimOp::Sub) => downs += 1,
                Some(d) if d == lo || d == hi => return None,
                _ => {}
            }
        }
        if ups != usize::from(ls.contains(id)) || downs > 1 {
            return None;
        }
    }
    Some((test, body))
}

/// Give the two-pointer loop of header `h` a copy marked short
/// (`MirFunction::strips`), entered when `hi - lo <= SHORT_TRIPS`.
fn short_copy(func: &mut MirFunction, h: u32, body: &HashSet<u32>, test: &Test) {
    let (lo, hi, ty) = (test.ctr, test.hi.expect("two pointers"), test.ty);
    let uty = unsigned(ty).expect("integer pointers");
    let mut next = func.blocks.iter().map(|b| b.id.0 + 1).max().unwrap_or(0);
    let mut ids: Vec<u32> = body.iter().copied().collect();
    ids.sort_unstable();
    let map: HashMap<u32, u32> = ids
        .into_iter()
        .map(|b| {
            next += 1;
            (b, next - 1)
        })
        .collect();
    let front = next;
    let (dist, udist, small) = (
        fresh(func, MirTy::Prim(ty)),
        fresh(func, MirTy::Prim(uty)),
        fresh(func, MirTy::Prim(PrimTy::Bool)),
    );
    for b in &mut func.blocks {
        if !body.contains(&b.id.0) {
            retarget(&mut b.terminator, h, front);
        }
    }
    let mut copies: Vec<BasicBlock> = Vec::new();
    for b in func.blocks.iter().filter(|b| map.contains_key(&b.id.0)) {
        let mut c = b.clone();
        c.id = BlockId(map[&b.id.0]);
        for (&from, &to) in &map {
            retarget(&mut c.terminator, from, to);
        }
        copies.push(c);
    }
    func.blocks.extend(copies);
    // `lo >= hi` makes the distance wrap huge (or zero): the loop runs
    // no iteration on either side.
    func.blocks.push(BasicBlock {
        id: BlockId(front),
        params: vec![],
        stmts: vec![
            assign(
                dist,
                RValue::BinOp {
                    op: PrimOp::WrappingSub,
                    op_ty: MirTy::Prim(ty),
                    lhs: Operand::Copy(hi),
                    rhs: Operand::Copy(lo),
                },
            ),
            assign(
                udist,
                RValue::Cast {
                    source: Operand::Copy(dist),
                    from_ty: MirTy::Prim(ty),
                    to_ty: MirTy::Prim(uty),
                },
            ),
            assign(
                small,
                RValue::BinOp {
                    op: PrimOp::Le,
                    op_ty: MirTy::Prim(uty),
                    lhs: Operand::Copy(udist),
                    rhs: int(SHORT_TRIPS, uty),
                },
            ),
        ],
        terminator: Terminator::If {
            cond: Operand::Copy(small),
            then_bb: BlockId(map[&h]),
            then_args: vec![],
            else_bb: BlockId(h),
            else_args: vec![],
        },
    });
    func.strips.push(map[&h]);
}

/// A nest of counted loops whose bounds are at most this runs at most
/// `NEST * NEST` iterations of each inner loop: `SHORT_TRIPS`.
const NEST: i128 = 8;

/// Every way from `from` to `to` inside `body` passes through `via`.
fn through(func: &MirFunction, body: &HashSet<u32>, from: u32, via: u32, to: u32) -> bool {
    if via == from {
        return true;
    }
    let mut seen: HashSet<u32> = HashSet::from([from, via]);
    let mut work = vec![from];
    while let Some(b) = work.pop() {
        if b == to {
            return false;
        }
        let Some(blk) = func.blocks.iter().find(|x| x.id.0 == b) else {
            continue;
        };
        for s in succs(blk) {
            if body.contains(&s) && seen.insert(s) {
                work.push(s);
            }
        }
    }
    from != to
}

/// A nest of counted loops: its blocks, its inner headers and the
/// values each loop's bound is when the nest is entered.
struct Nest {
    body: HashSet<u32>,
    inner: Vec<u32>,
    bounds: Vec<(Operand, PrimTy)>,
}

/// The counted loop of header `h` holding counted loops, all reaching no
/// safepoint, with unsigned counters. With its bounds at most `NEST`,
/// every loop of it runs at most that often an entry, so the nest needs
/// no poll.
fn nest(
    func: &MirFunction,
    h: u32,
    latches: &HashMap<u32, Vec<u32>>,
    headers: &HashSet<u32>,
    leaf: &HashSet<FunctionId>,
) -> Option<Nest> {
    let (t, body) = eligible(func, h, &latches[&h], headers, leaf, true, true)?;
    let inner: Vec<u32> = {
        let mut v: Vec<u32> = body
            .iter()
            .copied()
            .filter(|b| *b != h && headers.contains(b))
            .collect();
        v.sort_unstable();
        v
    };
    if inner.is_empty() || !t.ty.is_unsigned_int() {
        return None;
    }
    let addressed = crate::addressed(func);
    let defs = |l: LocalId| -> Vec<(u32, &Statement)> {
        func.blocks
            .iter()
            .flat_map(|b| b.stmts.iter().map(move |s| (b.id.0, s)))
            .filter(|(_, s)| crate::liveness::stmt_uses(s).1 == Some(l))
            .collect()
    };
    let fixed = |op: &Operand| match op {
        Operand::Const(_) => true,
        Operand::Copy(l) => {
            !addressed.contains(l)
                && defs(*l).iter().all(|(b, _)| !body.contains(b))
                && func
                    .blocks
                    .iter()
                    .filter(|b| body.contains(&b.id.0))
                    .all(|b| !b.params.contains(l))
        }
    };
    let mut bounds = vec![(t.bound.clone(), t.ty)];
    for h2 in &inner {
        let (t2, _) = eligible(func, *h2, &latches[h2], headers, leaf, false, true)?;
        if !t2.ty.is_unsigned_int() {
            return None;
        }
        // Its bound: fixed through the nest, or set in it (once, on every
        // way to the inner loop) to a value that is.
        let at_entry = match &t2.bound {
            op if fixed(op) => op.clone(),
            Operand::Copy(b2) if !addressed.contains(b2) => {
                let ds = defs(*b2);
                let [
                    (
                        d,
                        Statement::Assign {
                            rvalue: RValue::Use(v),
                            ..
                        },
                    ),
                ] = ds.as_slice()
                else {
                    return None;
                };
                if !body.contains(d) || !fixed(v) || !through(func, &body, h, *d, *h2) {
                    return None;
                }
                v.clone()
            }
            _ => return None,
        };
        bounds.push((at_entry, t2.ty));
    }
    Some(Nest {
        body,
        inner,
        bounds,
    })
}

/// Give the nest of header `h` a copy marked short, every loop of it,
/// entered when each of its bounds is at most `NEST`.
fn nest_copy(func: &mut MirFunction, h: u32, n: &Nest) {
    let Nest {
        body,
        inner,
        bounds,
    } = n;
    let mut next = func.blocks.iter().map(|b| b.id.0 + 1).max().unwrap_or(0);
    let mut ids: Vec<u32> = body.iter().copied().collect();
    ids.sort_unstable();
    let map: HashMap<u32, u32> = ids
        .into_iter()
        .map(|b| {
            next += 1;
            (b, next - 1)
        })
        .collect();
    let front = next;
    for b in &mut func.blocks {
        if !body.contains(&b.id.0) {
            retarget(&mut b.terminator, h, front);
        }
    }
    let mut copies: Vec<BasicBlock> = Vec::new();
    for b in func.blocks.iter().filter(|b| map.contains_key(&b.id.0)) {
        let mut c = b.clone();
        c.id = BlockId(map[&b.id.0]);
        for (&from, &to) in &map {
            retarget(&mut c.terminator, from, to);
        }
        copies.push(c);
    }
    func.blocks.extend(copies);
    let mut stmts = Vec::new();
    let mut all: Option<LocalId> = None;
    for (op, ty) in bounds {
        let c = fresh(func, MirTy::Prim(PrimTy::Bool));
        stmts.push(assign(
            c,
            RValue::BinOp {
                op: PrimOp::Le,
                op_ty: MirTy::Prim(*ty),
                lhs: op.clone(),
                rhs: int(NEST, *ty),
            },
        ));
        all = Some(match all {
            None => c,
            Some(a) => {
                let both = fresh(func, MirTy::Prim(PrimTy::Bool));
                stmts.push(assign(
                    both,
                    RValue::BinOp {
                        op: PrimOp::BitAnd,
                        op_ty: MirTy::Prim(PrimTy::Bool),
                        lhs: Operand::Copy(a),
                        rhs: Operand::Copy(c),
                    },
                ));
                both
            }
        });
    }
    func.blocks.push(BasicBlock {
        id: BlockId(front),
        params: vec![],
        stmts,
        terminator: Terminator::If {
            cond: Operand::Copy(all.expect("a bound")),
            then_bb: BlockId(map[&h]),
            then_args: vec![],
            else_bb: BlockId(h),
            else_args: vec![],
        },
    });
    func.strips.push(map[&h]);
    func.strips.extend(inner.iter().map(|i| map[i]));
}

pub fn strip(fns: &mut [MirFunction]) {
    let leaf = leaves(fns, false);
    let quiet_but_panics = leaves(fns, true);
    for func in fns.iter_mut() {
        if func.facts.linkage != Linkage::Defined || func.blocks.is_empty() {
            continue;
        }
        let mut latches: HashMap<u32, Vec<u32>> = HashMap::new();
        for (latch, hs) in back_edges(func) {
            for h in hs {
                latches.entry(h).or_default().push(latch);
            }
        }
        let headers: HashSet<u32> = latches.keys().copied().collect();
        let mut hs: Vec<u32> = headers.iter().copied().collect();
        hs.sort_unstable();
        let nests: Vec<_> = hs
            .into_iter()
            .filter_map(|h| Some((h, nest(func, h, &latches, &headers, &quiet_but_panics)?)))
            .collect();
        for (h, n) in &nests {
            nest_copy(func, *h, n);
        }
        let mut latches: HashMap<u32, Vec<u32>> = HashMap::new();
        for (latch, hs) in back_edges(func) {
            for h in hs {
                latches.entry(h).or_default().push(latch);
            }
        }
        let headers: HashSet<u32> = latches.keys().copied().collect();
        let short = crate::rotate::short_loops(func);
        let mut todo: Vec<(u32, Vec<u32>)> = latches
            .into_iter()
            .filter(|(h, _)| !short.contains_key(h))
            .collect();
        todo.sort_unstable_by_key(|(h, _)| *h);
        let twos: Vec<(u32, HashSet<u32>, Test)> = todo
            .iter()
            .filter_map(|(h, ls)| {
                let (t, body) = two_pointer(func, *h, ls, &headers, &leaf)?;
                Some((*h, body, t))
            })
            .collect();
        let plans: Vec<(u32, Vec<u32>, Test)> = todo
            .into_iter()
            .filter_map(|(h, ls)| {
                let (t, _) = eligible(func, h, &ls, &headers, &leaf, false, false)?;
                Some((h, ls, t))
            })
            .collect();
        for (h, ls, t) in &plans {
            apply(func, *h, ls, t);
        }
        for (h, body, t) in &twos {
            short_copy(func, *h, body, t);
        }
    }
}

#[cfg(test)]
mod tests {
    use rokc_diagnostic::Span;

    use super::*;

    const fn usize_op(t: u32, op: PrimOp, lhs: u32, rhs: Operand) -> Statement {
        assign(
            LocalId(t),
            RValue::BinOp {
                op,
                op_ty: MirTy::Prim(PrimTy::Usize),
                lhs: Operand::Copy(LocalId(lhs)),
                rhs,
            },
        )
    }

    /// `while x0 < x1 { x0 += 1 }`, `x1` a parameter; rotated.
    fn counting() -> MirFunction {
        let test = |id, then_bb, else_bb, stmts: Vec<Statement>| BasicBlock {
            id: BlockId(id),
            params: vec![],
            stmts,
            terminator: Terminator::If {
                cond: Operand::Copy(LocalId(2)),
                then_bb: BlockId(then_bb),
                then_args: vec![],
                else_bb: BlockId(else_bb),
                else_args: vec![],
            },
        };
        let lt = || usize_op(2, PrimOp::Lt, 0, Operand::Copy(LocalId(1)));
        let one = Operand::Const(ConstVal::Int {
            value: 1,
            ty: PrimTy::Usize,
        });
        let mut f = MirFunction {
            id: FunctionId(0),
            def_id: None,
            name: "f".into(),
            facts: rokc_ir_common::FnFacts::default(),
            strips: Vec::new(),
            pins: Vec::new(),
            params: vec![LocalId(0), LocalId(1)],
            locals: (0..3)
                .map(|i| LocalDecl {
                    id: LocalId(i),
                    name: None,
                    ty: MirTy::Prim(if i == 2 { PrimTy::Bool } else { PrimTy::Usize }),
                })
                .collect(),
            blocks: vec![
                test(0, 1, 3, vec![lt()]),
                BasicBlock {
                    id: BlockId(1),
                    params: vec![],
                    stmts: vec![],
                    terminator: Terminator::Goto {
                        target: BlockId(2),
                        args: vec![],
                    },
                },
                test(2, 1, 3, vec![usize_op(0, PrimOp::Add, 0, one), lt()]),
                BasicBlock {
                    id: BlockId(3),
                    params: vec![],
                    stmts: vec![],
                    terminator: Terminator::Return(None),
                },
            ],
            param_tys: vec![MirTy::Prim(PrimTy::Usize); 2],
            ret_ty: MirTy::Unit,
            span: Span::default(),
        };
        f.facts.linkage = Linkage::Defined;
        f
    }

    #[test]
    fn a_counted_loop_runs_in_short_strips() {
        let mut fns = vec![counting()];
        assert!(crate::rotate::short_loops(&fns[0]).is_empty());
        strip(&mut fns);
        let f = &fns[0];
        assert_eq!(f.strips, [1]);
        let short = crate::rotate::short_loops(f);
        assert_eq!(short.keys().collect::<Vec<_>>(), [&1]);
        // The entry and the strip's end go to the strip's front, which
        // sets the limit the latch tests.
        let front = succs(&f.blocks[0])[0];
        assert!(
            f.blocks
                .iter()
                .any(|b| b.id.0 == front && succs(b).contains(&1))
        );
        let edges = back_edges(f);
        assert_eq!(edges[&2], HashSet::from([1]));
        assert!(edges.values().any(|hs| hs.contains(&front)));
        // A loop that may reach a safepoint (a lazy static) is left alone.
        let mut g = counting();
        g.blocks[1]
            .stmts
            .push(assign(LocalId(2), RValue::StaticAddr("s".into())));
        let mut fns = vec![g];
        strip(&mut fns);
        assert!(fns[0].strips.is_empty());
    }
}
