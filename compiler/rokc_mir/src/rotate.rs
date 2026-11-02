//! Loops and loop rotation. A counted loop whose header tests its exit
//! (a range `for`, `while i < n` with `n` fixed) is rotated: back edges
//! go to a copy of the header, so the test sits at the bottom. LLVM's
//! own rotation skips a loop whose latch already exits (the back-edge
//! poll's yield path), and a load hoisted out of an unrotated body is
//! speculated and loses its TBAA tag; rotated, a length load leaves the
//! loop with its tag and stays valid across the poll's yield
//! (`REF_ONLY_CALLS`). The counter's step moves into the bottom test
//! (`sink_step`).

use std::collections::{BTreeMap, HashMap, HashSet};

use rokc_ir_common::{BlockId, ConstVal, LocalId, PrimOp};

use crate::{BasicBlock, MirFunction, Operand, RValue, Statement, Terminator};

/// A header copied for the back edges has at most this many statements.
const MAX_HEADER: usize = 16;

pub fn succs(b: &BasicBlock) -> Vec<u32> {
    match &b.terminator {
        Terminator::Goto { target, .. } => vec![target.0],
        Terminator::If {
            then_bb, else_bb, ..
        } => vec![then_bb.0, else_bb.0],
        Terminator::Suspend { resume, .. } => vec![resume.0],
        Terminator::Return(_) | Terminator::Unreachable => Vec::new(),
    }
}

/// Each block with an edge to a block on the current DFS path (a loop
/// back edge), with the loop headers its back edges go to; an iterative
/// DFS from the entry block.
#[must_use]
pub fn back_edges(f: &MirFunction) -> HashMap<u32, HashSet<u32>> {
    let index: HashMap<u32, usize> = f
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.0, i))
        .collect();
    let mut out: HashMap<u32, HashSet<u32>> = HashMap::new();
    // 0 = unvisited, 1 = on the DFS path, 2 = done.
    let mut state = vec![0u8; f.blocks.len()];
    let mut stack: Vec<(usize, Vec<u32>)> = Vec::new();
    if let Some(b) = f.blocks.first() {
        state[0] = 1;
        stack.push((0, succs(b)));
    }
    while let Some((i, rest)) = stack.last_mut() {
        let i = *i;
        let Some(t) = rest.pop() else {
            state[i] = 2;
            stack.pop();
            continue;
        };
        let Some(&j) = index.get(&t) else { continue };
        if state[j] == 0 {
            state[j] = 1;
            stack.push((j, succs(&f.blocks[j])));
        } else if state[j] == 1 {
            out.entry(f.blocks[i].id.0).or_default().insert(t);
        }
    }
    out
}

/// Index of the call that ends block `b` without returning (the last
/// statement before an `Unreachable`, e.g. a panic): nothing of this
/// frame is used after it, so it needs no roots here.
#[must_use]
pub fn noreturn_call(b: &BasicBlock) -> Option<usize> {
    if !matches!(b.terminator, Terminator::Unreachable) {
        return None;
    }
    let (i, s) = b
        .stmts
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| !matches!(s, Statement::Loc(..) | Statement::StorageDead(_)))?;
    matches!(
        s,
        Statement::Assign {
            rvalue: RValue::Call { .. },
            ..
        }
    )
    .then_some(i)
}

/// A loop running at most this many times needs no back-edge poll when
/// its body reaches no safepoint: it ends in bounded time.
pub const SHORT_TRIPS: i128 = 64;

/// The loops of `f` that run at most `SHORT_TRIPS` times, by header,
/// with their blocks.
///
/// Such a loop has a counter starting at constants, stepped by a
/// constant once per iteration, and tested against a constant on every
/// iteration; or `strip` made it (`MirFunction::strips`).
#[must_use]
pub fn short_loops(f: &MirFunction) -> HashMap<u32, HashSet<u32>> {
    let mut latches: HashMap<u32, Vec<u32>> = HashMap::new();
    for (l, hs) in back_edges(f) {
        for h in hs {
            latches.entry(h).or_default().push(l);
        }
    }
    let mut defs: Defs<'_> = HashMap::new();
    let mut other_defs: HashSet<LocalId> = crate::addressed(f);
    other_defs.extend(f.params.iter().copied());
    for b in &f.blocks {
        other_defs.extend(b.params.iter().copied());
        for s in &b.stmts {
            match s {
                Statement::Assign { target, rvalue } => {
                    defs.entry(*target).or_default().push((b.id.0, rvalue));
                }
                s => other_defs.extend(crate::liveness::stmt_uses(s).1),
            }
        }
    }
    defs.retain(|l, _| !other_defs.contains(l));
    let blocks: HashMap<u32, &BasicBlock> = f.blocks.iter().map(|b| (b.id.0, b)).collect();
    latches
        .into_iter()
        .filter_map(|(h, ls)| {
            let body = loop_body(f, h, &ls);
            let every = |x| every_iteration(&blocks, h, &ls, &body, x);
            let short = f.strips.contains(&h)
                || body.iter().any(|x| {
                    trips(blocks[x], &body, &defs, every).is_some_and(|n| n <= SHORT_TRIPS)
                        && every(*x)
                });
            short.then_some((h, body))
        })
        .collect()
}

/// Whether block `x` of the loop `body` (header `h`, latches `ls`) runs
/// on every iteration: no path from `h` to a latch avoids it.
fn every_iteration(
    blocks: &HashMap<u32, &BasicBlock>,
    h: u32,
    ls: &[u32],
    body: &HashSet<u32>,
    x: u32,
) -> bool {
    if x == h {
        return true;
    }
    let mut seen: HashSet<u32> = HashSet::from([h, x]);
    let mut work = vec![h];
    while let Some(b) = work.pop() {
        if ls.contains(&b) {
            return false;
        }
        for s in succs(blocks[&b]) {
            if body.contains(&s) && seen.insert(s) {
                work.push(s);
            }
        }
    }
    true
}

/// The assignments of each local assigned only by `Assign` (not a
/// parameter, never addressed), with their blocks.
type Defs<'a> = HashMap<LocalId, Vec<(u32, &'a RValue)>>;

/// An integer constant operand, or an assignment of one.
fn lit(r: &RValue) -> Option<i128> {
    match r {
        RValue::Use(Operand::Const(ConstVal::Int { value, .. })) => Some(*value),
        RValue::Use(Operand::Const(ConstVal::UInt { value, .. })) => i128::try_from(*value).ok(),
        _ => None,
    }
}

/// The constant a local always holds (every assignment the same
/// constant), as an integer.
fn const_int(op: &Operand, defs: &Defs<'_>) -> Option<i128> {
    match op {
        Operand::Const(_) => lit(&RValue::Use(op.clone())),
        Operand::Copy(l) => {
            let mut vals = defs.get(l)?.iter().map(|(_, r)| lit(r));
            let first = vals.next()??;
            vals.all(|v| v == Some(first)).then_some(first)
        }
    }
}

/// Bits of an integer type, taking `isize` / `usize` at their narrowest.
const fn int_bits(ty: rokc_ir_common::PrimTy) -> Option<(u32, bool)> {
    use rokc_ir_common::PrimTy as P;
    Some(match ty {
        P::I8 => (8, true),
        P::I16 => (16, true),
        P::I32 | P::Isize => (32, true),
        P::I64 => (64, true),
        P::U8 => (8, false),
        P::U16 => (16, false),
        P::U32 | P::Usize => (32, false),
        P::U64 => (64, false),
        _ => return None,
    })
}

/// How many times the loop `body` can run when block `exit` ends in
/// its exit test `c < n` (or `<=`, `>`, `>=`, either side), `c` a
/// counter stepped toward `n` by a constant once in the loop (in a block
/// `every` accepts), from constants outside it.
fn trips(
    exit: &BasicBlock,
    body: &HashSet<u32>,
    defs: &Defs<'_>,
    every: impl Fn(u32) -> bool,
) -> Option<i128> {
    let Terminator::If {
        cond: Operand::Copy(cond),
        then_bb,
        else_bb,
        ..
    } = &exit.terminator
    else {
        return None;
    };
    let stay_if = match (body.contains(&then_bb.0), body.contains(&else_bb.0)) {
        (true, false) => true,
        (false, true) => false,
        _ => return None,
    };
    let (op, ty, lhs, rhs) = exit.stmts.iter().rev().find_map(|s| match s {
        Statement::Assign {
            target,
            rvalue:
                RValue::BinOp {
                    op,
                    op_ty: crate::MirTy::Prim(ty),
                    lhs,
                    rhs,
                },
        } if target == cond => Some((*op, *ty, lhs, rhs)),
        _ => None,
    })?;
    let (bits, signed) = int_bits(ty)?;
    let (lo, hi) = if signed {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    };
    // Normalize to `c < n` / `c <= n` (up) or `c > n` / `c >= n` (down)
    // holding while the loop stays.
    let (ctr, bound, up, strict) = match (op, lhs, rhs) {
        (PrimOp::Lt | PrimOp::Le | PrimOp::Gt | PrimOp::Ge, Operand::Copy(ctr), bound)
            if const_int(bound, defs).is_some() =>
        {
            (
                *ctr,
                bound,
                matches!(op, PrimOp::Lt | PrimOp::Le),
                matches!(op, PrimOp::Lt | PrimOp::Gt),
            )
        }
        (PrimOp::Lt | PrimOp::Le | PrimOp::Gt | PrimOp::Ge, bound, Operand::Copy(ctr))
            if const_int(bound, defs).is_some() =>
        {
            (
                *ctr,
                bound,
                matches!(op, PrimOp::Gt | PrimOp::Ge),
                matches!(op, PrimOp::Lt | PrimOp::Gt),
            )
        }
        _ => return None,
    };
    // A test that holds while leaving swaps the direction.
    let (up, strict) = if stay_if {
        (up, strict)
    } else {
        (!up, !strict)
    };
    let n = const_int(bound, defs)?;
    let mut step = None;
    let mut starts = Vec::new();
    for (b, r) in defs.get(&ctr)? {
        if body.contains(b) {
            let RValue::BinOp {
                op: op @ (PrimOp::Add | PrimOp::Sub),
                lhs: Operand::Copy(l),
                rhs: k,
                ..
            } = r
            else {
                return None;
            };
            let k = const_int(k, defs)?;
            let k = if *op == PrimOp::Sub { -k } else { k };
            if *l != ctr || step.is_some() || !every(*b) {
                return None;
            }
            step = Some(k);
        } else {
            starts.push(lit(r)?);
        }
    }
    let k = step?;
    let start = if up {
        starts.iter().min()
    } else {
        starts.iter().max()
    }
    .copied()?;
    // The last value the counter takes stays in range, so it cannot wrap.
    let (dist, last) = if up {
        (n - start, n + k - i128::from(strict))
    } else {
        (start - n, n + k + i128::from(strict))
    };
    if (up && k <= 0) || (!up && k >= 0) || last < lo || last > hi {
        return None;
    }
    // At the j-th test the counter has moved at least j steps.
    let (dist, k) = (dist.max(0), k.abs());
    Some(if strict {
        (dist + k - 1) / k
    } else {
        dist / k + 1
    })
}

/// A header cheap to copy: no parameters, a few statements that call
/// nothing and allocate nothing, an `if` at the end.
fn copyable(b: &BasicBlock) -> bool {
    b.params.is_empty()
        && matches!(b.terminator, Terminator::If { .. })
        && b.stmts.len() <= MAX_HEADER
        && b.stmts.iter().all(|s| match s {
            Statement::Assign { rvalue, .. } => matches!(
                rvalue,
                RValue::Use(_)
                    | RValue::BinOp { .. }
                    | RValue::UnaryOp { .. }
                    | RValue::Cast { .. }
                    | RValue::FieldRead { .. }
            ),
            Statement::Loc(..) | Statement::StorageDead(_) => true,
            _ => false,
        })
}

pub fn retarget(t: &mut Terminator, from: u32, to: u32) {
    let fix = |b: &mut BlockId| {
        if b.0 == from {
            *b = BlockId(to);
        }
    };
    match t {
        Terminator::Goto { target, .. } => fix(target),
        Terminator::If {
            then_bb, else_bb, ..
        } => {
            fix(then_bb);
            fix(else_bb);
        }
        Terminator::Suspend { resume, .. } => fix(resume),
        Terminator::Return(_) | Terminator::Unreachable => {}
    }
}

/// Rotate the loops of `f` whose header tests the exit (see the module
/// doc): the back edges of header `H` go to a copy of `H`.
pub fn rotate(f: &mut MirFunction) {
    let mut latches: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (l, hs) in back_edges(f) {
        for h in hs {
            latches.entry(h).or_default().push(l);
        }
    }
    let mut next = f.blocks.iter().map(|b| b.id.0 + 1).max().unwrap_or(0);
    for (h, ls) in latches {
        let Some(hb) = f.blocks.iter().find(|b| b.id.0 == h) else {
            continue;
        };
        if !copyable(hb) {
            continue;
        }
        let body = loop_body(f, h, &ls);
        if !succs(hb).iter().any(|s| !body.contains(s)) || !counted(f, hb, &body) {
            continue;
        }
        let mut copy = hb.clone();
        copy.id = BlockId(next);
        retarget(&mut copy.terminator, h, next);
        for b in &mut f.blocks {
            if ls.contains(&b.id.0) {
                retarget(&mut b.terminator, h, next);
            }
        }
        f.blocks.push(copy);
        sink_step(f, h, next, &body);
        next += 1;
    }
}

/// The counter step of a rotated loop (header `h`, its test's copy
/// `copy`) moved from the top of the body into `copy`, in front of the
/// test: `for i in a..b` steps the counter before its body runs, and a
/// step left at the top of the rotated body is a block away from the
/// body's own `i + 1`s, which LLVM then computes again. Only a counter
/// the loop reads nowhere after its step, nor anything after the loop.
fn sink_step(f: &mut MirFunction, h: u32, copy: u32, body: &HashSet<u32>) {
    let Some(hb) = f.blocks.iter().find(|b| b.id.0 == h) else {
        return;
    };
    let Terminator::If {
        then_bb, else_bb, ..
    } = &hb.terminator
    else {
        return;
    };
    let first = if body.contains(&then_bb.0) {
        then_bb.0
    } else {
        else_bb.0
    };
    if f.blocks
        .iter()
        .any(|b| b.id.0 != h && b.id.0 != copy && succs(b).contains(&first))
    {
        return;
    }
    let Some(bi) = f.blocks.iter().position(|b| b.id.0 == first) else {
        return;
    };
    let Some(k) = f.blocks[bi].stmts.iter().position(|s| {
        matches!(s, Statement::Assign {
            target,
            rvalue: RValue::BinOp { op: PrimOp::Add, lhs: Operand::Copy(l), rhs: Operand::Const(_), .. },
        } if target == l)
    }) else {
        return;
    };
    let Statement::Assign { target: c, .. } = f.blocks[bi].stmts[k] else {
        return;
    };
    if crate::addressed(f).contains(&c) {
        return;
    }
    // Besides the tests (`h`, `copy`), `c` is read only before the step
    // in its block, and the loop writes it only there.
    for b in &f.blocks {
        let id = b.id.0;
        if id == h || id == copy {
            continue;
        }
        if b.params.contains(&c) || crate::liveness::term_uses(&b.terminator).contains(&c) {
            return;
        }
        for (i, s) in b.stmts.iter().enumerate() {
            if id == first && i <= k {
                continue;
            }
            let (uses, def) = crate::liveness::stmt_uses(s);
            if uses.contains(&c) || (def == Some(c) && body.contains(&id)) {
                return;
            }
        }
    }
    let step = f.blocks[bi].stmts.remove(k);
    if let Some(cb) = f.blocks.iter_mut().find(|b| b.id.0 == copy) {
        cb.stmts.insert(0, step);
    }
}

/// The blocks of the loop of header `h` (back edges from `latches`): the
/// ones reaching a latch without passing `h`.
pub fn loop_body(f: &MirFunction, h: u32, latches: &[u32]) -> HashSet<u32> {
    let mut preds: HashMap<u32, Vec<u32>> = HashMap::new();
    for b in &f.blocks {
        for s in succs(b) {
            preds.entry(s).or_default().push(b.id.0);
        }
    }
    let mut body: HashSet<u32> = HashSet::from([h]);
    let mut work: Vec<u32> = latches.to_vec();
    while let Some(b) = work.pop() {
        if body.insert(b) {
            work.extend(preds.get(&b).into_iter().flatten().copied());
        }
    }
    body
}

/// Whether header `hb` tests an ordering of a local the loop changes
/// against a value it does not (a counter against its bound): such a
/// loop is worth rotating; a test of two moving values (`lo < hi`) gets
/// worse induction variables rotated.
fn counted(f: &MirFunction, hb: &BasicBlock, body: &HashSet<u32>) -> bool {
    let Terminator::If {
        cond: Operand::Copy(c),
        ..
    } = &hb.terminator
    else {
        return false;
    };
    let Some((lhs, rhs)) = hb.stmts.iter().rev().find_map(|s| match s {
        Statement::Assign {
            target,
            rvalue:
                RValue::BinOp {
                    op: PrimOp::Lt | PrimOp::Le | PrimOp::Gt | PrimOp::Ge,
                    lhs,
                    rhs,
                    ..
                },
        } if target == c => Some((lhs, rhs)),
        _ => None,
    }) else {
        return false;
    };
    let mut moved: HashSet<LocalId> = HashSet::new();
    for b in f.blocks.iter().filter(|b| body.contains(&b.id.0)) {
        moved.extend(b.params.iter().copied());
        for s in &b.stmts {
            if let Statement::Assign { target, .. } = s {
                moved.insert(*target);
            }
        }
    }
    let fixed = |o: &Operand| {
        matches!(o, Operand::Const(_)) || matches!(o, Operand::Copy(l) if !moved.contains(l))
    };
    fixed(lhs) != fixed(rhs)
}

#[cfg(test)]
mod tests {
    use rokc_diagnostic::Span;
    use rokc_ir_common::{ConstVal, FunctionId, PrimTy};

    use super::*;
    use crate::{LocalDecl, MirTy};

    const fn block(id: u32, stmts: Vec<Statement>, terminator: Terminator) -> BasicBlock {
        BasicBlock {
            id: BlockId(id),
            params: vec![],
            stmts,
            terminator,
        }
    }

    const fn op(t: u32, op: PrimOp, lhs: u32, rhs: Operand) -> Statement {
        Statement::Assign {
            target: LocalId(t),
            rvalue: RValue::BinOp {
                op,
                op_ty: MirTy::Prim(PrimTy::Usize),
                lhs: Operand::Copy(LocalId(lhs)),
                rhs,
            },
        }
    }

    /// bb0: goto bb1; bb1: x2 = x0 < x1; if x2 -> bb2 / bb3;
    /// bb2: x0 += 1 (and x1 -= 1 when `both`); goto bb1; bb3: return.
    fn counting(both: bool) -> MirFunction {
        let one = || {
            Operand::Const(ConstVal::UInt {
                value: 1,
                ty: PrimTy::Usize,
            })
        };
        let mut step = vec![op(0, PrimOp::Add, 0, one())];
        if both {
            step.push(op(1, PrimOp::Sub, 1, one()));
        }
        MirFunction {
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
                    ty: MirTy::Prim(PrimTy::Usize),
                })
                .collect(),
            blocks: vec![
                block(
                    0,
                    vec![],
                    Terminator::Goto {
                        target: BlockId(1),
                        args: vec![],
                    },
                ),
                block(
                    1,
                    vec![op(2, PrimOp::Lt, 0, Operand::Copy(LocalId(1)))],
                    Terminator::If {
                        cond: Operand::Copy(LocalId(2)),
                        then_bb: BlockId(2),
                        then_args: vec![],
                        else_bb: BlockId(3),
                        else_args: vec![],
                    },
                ),
                block(
                    2,
                    step,
                    Terminator::Goto {
                        target: BlockId(1),
                        args: vec![],
                    },
                ),
                block(3, vec![], Terminator::Return(None)),
            ],
            param_tys: vec![MirTy::Prim(PrimTy::Usize); 2],
            ret_ty: MirTy::Unit,
            span: Span::default(),
        }
    }

    /// `counting(false)` with its counter and bound set in bb0 instead
    /// of passed: `x0 = start; x1 = bound`.
    fn constant_count(start: u128, bound: u128) -> MirFunction {
        let mut f = counting(false);
        let c = |value| {
            RValue::Use(Operand::Const(ConstVal::UInt {
                value,
                ty: PrimTy::Usize,
            }))
        };
        f.params.clear();
        f.param_tys.clear();
        f.blocks[0].stmts = vec![
            Statement::Assign {
                target: LocalId(0),
                rvalue: c(start),
            },
            Statement::Assign {
                target: LocalId(1),
                rvalue: c(bound),
            },
        ];
        f
    }

    #[test]
    fn short_loops_have_a_small_constant_count() {
        let short = short_loops(&constant_count(0, SHORT_TRIPS as u128));
        assert_eq!(short.keys().collect::<Vec<_>>(), [&1]);
        assert_eq!(short[&1], HashSet::from([1, 2]));
        assert!(short_loops(&constant_count(0, SHORT_TRIPS as u128 + 1)).is_empty());
        assert_eq!(short_loops(&constant_count(10, 70)).len(), 1);
        // Rotated, the bottom test bounds it the same.
        let mut f = constant_count(0, 8);
        rotate(&mut f);
        assert_eq!(short_loops(&f).len(), 1);
        // A bound that is a parameter, or a counter stepped twice, is not.
        assert!(short_loops(&counting(false)).is_empty());
        let mut f = constant_count(0, 8);
        let step = f.blocks[2].stmts[0].clone();
        f.blocks[2].stmts.push(step);
        assert!(short_loops(&f).is_empty());
    }

    #[test]
    fn a_counted_loop_tests_at_the_bottom() {
        let mut f = counting(false);
        rotate(&mut f);
        // bb2 goes to bb4, a copy of the test: bb2 is the header now.
        assert_eq!(succs(&f.blocks[2]), vec![4]);
        assert_eq!(succs(&f.blocks[4]), vec![2, 3]);
        let edges = back_edges(&f);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[&4], HashSet::from([2]));
        // The step moved in front of the bottom test.
        assert!(f.blocks[2].stmts.is_empty());
        assert_eq!(f.blocks[4].stmts.len(), 2);
    }

    #[test]
    fn a_counter_read_after_its_step_keeps_it() {
        let mut f = counting(false);
        f.blocks[2]
            .stmts
            .push(op(2, PrimOp::Lt, 0, Operand::Copy(LocalId(1))));
        rotate(&mut f);
        assert_eq!(f.blocks[2].stmts.len(), 2);
        assert_eq!(f.blocks[4].stmts.len(), 1);
    }

    #[test]
    fn two_moving_bounds_stay() {
        let mut f = counting(true);
        rotate(&mut f);
        assert_eq!(f.blocks.len(), 4);
    }
}
