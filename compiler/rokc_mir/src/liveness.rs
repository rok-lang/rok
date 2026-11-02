//! Where locals die: backward liveness over the MIR CFG, so codegen can
//! clear a GC root slot right after its last use (a slot
//! whose variable is dead may be nulled) instead of at function end.
//!
//! A block param is defined on block entry. An array write is a partial
//! definition (the old elements stay), so it only uses the array.

use std::collections::{HashMap, HashSet};

use rokc_ir_common::{BlockId, LocalId};

use crate::{MirFunction, Operand, Statement, Terminator};

/// Deaths of the tracked locals, indexed like `MirFunction::blocks`.
#[derive(Debug, Default)]
pub struct Deaths {
    /// Locals some predecessor still held at its terminator (live out or
    /// a terminator operand) that are dead on entry to this block.
    pub on_entry: Vec<Vec<LocalId>>,
    /// Per statement: the locals it mentions that are dead after it.
    pub after_stmt: Vec<Vec<Vec<LocalId>>>,
}

/// Fixed-size bit set over the tracked locals' dense indices.
#[derive(Clone, PartialEq)]
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Self {
        Self(vec![0; n.div_ceil(64)])
    }
    fn has(&self, i: usize) -> bool {
        (self.0[i / 64] >> (i % 64)) & 1 == 1
    }
    fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }
    fn unset(&mut self, i: usize) {
        self.0[i / 64] &= !(1 << (i % 64));
    }
    fn union(&mut self, o: &Self) {
        for (a, b) in self.0.iter_mut().zip(&o.0) {
            *a |= b;
        }
    }
}

/// Locals a statement reads, and the one it (fully) defines.
pub fn stmt_uses(s: &Statement) -> (Vec<LocalId>, Option<LocalId>) {
    let mut ops = Vec::new();
    let mut def = None;
    match s {
        Statement::Assign { target, rvalue } => {
            crate::coro::rvalue_operands(rvalue, &mut ops);
            def = Some(*target);
        }
        Statement::ArrayWrite {
            array,
            index,
            value,
            ..
        } => ops.extend([Operand::Copy(*array), index.clone(), value.clone()]),
        Statement::StorageDead(_) | Statement::Loc(..) => {}
        Statement::BufStore {
            buf, index, value, ..
        } => ops.extend([buf.clone(), index.clone(), value.clone()]),
        Statement::RawStore { addr, value, .. } => ops.extend([addr.clone(), value.clone()]),
        Statement::HeapStore { ptr, value, .. } => ops.extend([ptr.clone(), value.clone()]),
        Statement::StaticStore { value, .. } => ops.push(value.clone()),
    }
    (copies(&ops), def)
}

fn copies(ops: &[Operand]) -> Vec<LocalId> {
    ops.iter()
        .filter_map(|o| match o {
            Operand::Copy(l) => Some(*l),
            Operand::Const(_) => None,
        })
        .collect()
}

pub fn term_uses(t: &Terminator) -> Vec<LocalId> {
    match t {
        Terminator::Goto { args, .. } => copies(args),
        Terminator::If {
            cond,
            then_args,
            else_args,
            ..
        } => {
            let mut ops = vec![cond.clone()];
            ops.extend(then_args.iter().cloned());
            ops.extend(else_args.iter().cloned());
            copies(&ops)
        }
        Terminator::Return(v) | Terminator::Suspend { value: v, .. } => copies(v.as_slice()),
        Terminator::Unreachable => Vec::new(),
    }
}

fn successors(t: &Terminator) -> Vec<rokc_ir_common::BlockId> {
    match t {
        Terminator::Goto { target, .. } => vec![*target],
        Terminator::If {
            then_bb, else_bb, ..
        } => vec![*then_bb, *else_bb],
        Terminator::Suspend { resume, .. } => vec![*resume],
        Terminator::Return(_) | Terminator::Unreachable => Vec::new(),
    }
}

/// Locals accessed through their memory.
///
/// Their address is taken (`RValue::AddrOf`), or an array field of
/// theirs is indexed in place (`s.buf[i]`): a pointer may read or write
/// them anywhere in their scope, so they live in their slot, rooted
/// until `StorageDead`, and never die early.
#[must_use]
pub fn addressed(f: &MirFunction) -> HashSet<LocalId> {
    let not_array = |l: &LocalId| !matches!(f.locals[l.0 as usize].ty, crate::MirTy::Array(..));
    f.blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            Statement::Assign {
                rvalue: crate::RValue::AddrOf(l),
                ..
            } => Some(*l),
            Statement::Assign {
                rvalue: crate::RValue::ArrayIndex { array: l, .. },
                ..
            }
            | Statement::ArrayWrite { array: l, .. } => Some(*l).filter(not_array),
            _ => None,
        })
        .collect()
}

/// Per local: it is assigned exactly once, by an `Assign`, and read
/// only after that in the same block (so its value can stay an SSA
/// value). Never a parameter or a block param.
#[must_use]
pub fn block_local(f: &MirFunction) -> Vec<bool> {
    // Per local: (defining block, statement index), or poisoned.
    let mut def: Vec<Option<(usize, usize)>> = vec![None; f.locals.len()];
    let mut ok = vec![true; f.locals.len()];
    for p in f
        .params
        .iter()
        .chain(f.blocks.iter().flat_map(|b| &b.params))
    {
        ok[p.0 as usize] = false;
    }
    for (bi, b) in f.blocks.iter().enumerate() {
        for (si, s) in b.stmts.iter().enumerate() {
            let (uses, d) = stmt_uses(s);
            for u in uses {
                let i = u.0 as usize;
                ok[i] &= def[i].is_some_and(|(db, ds)| db == bi && ds < si);
            }
            match (s, d) {
                (Statement::Assign { .. }, Some(d)) => {
                    let i = d.0 as usize;
                    ok[i] &= def[i].is_none();
                    def[i] = Some((bi, si));
                }
                (_, Some(d)) => ok[d.0 as usize] = false,
                _ => {}
            }
        }
        for u in term_uses(&b.terminator) {
            let i = u.0 as usize;
            ok[i] &= def[i].is_some_and(|(db, _)| db == bi);
        }
    }
    for (o, d) in ok.iter_mut().zip(&def) {
        *o &= d.is_some();
    }
    for l in addressed(f) {
        ok[l.0 as usize] = false;
    }
    ok
}

/// The tracked locals' dense indices: liveness of one local does not
/// depend on the others, so the rest are ignored.
struct Dense {
    ids: Vec<LocalId>,
    index: HashMap<LocalId, usize>,
}

impl Dense {
    fn get(&self, l: LocalId) -> Option<usize> {
        self.index.get(&l).copied()
    }

    fn bits<'a>(&self, ls: impl IntoIterator<Item = &'a LocalId>) -> Bits {
        let mut bits = Bits::new(self.ids.len());
        for &l in ls {
            if let Some(i) = self.get(l) {
                bits.set(i);
            }
        }
        bits
    }
}

/// `(gen, kill)` of a block: tracked locals used before any definition
/// in it, and those it defines (block params included).
fn gen_kill(
    dense: &Dense,
    b: &crate::BasicBlock,
    stmts: &[(Vec<LocalId>, Option<LocalId>)],
) -> (Bits, Bits) {
    let mut kill = dense.bits(&b.params);
    let mut gen_ = Bits::new(dense.ids.len());
    let mut uses_of = |ls: &[LocalId], kill: &Bits| {
        for &l in ls {
            if let Some(i) = dense.get(l).filter(|&i| !kill.has(i)) {
                gen_.set(i);
            }
        }
    };
    for (uses, def) in stmts {
        uses_of(uses, &kill);
        if let Some(i) = def.and_then(|d| dense.get(d)) {
            kill.set(i);
        }
    }
    uses_of(&term_uses(&b.terminator), &kill);
    (gen_, kill)
}

/// Live-in sets: the backward dataflow fixpoint.
fn solve(succs: &[Vec<usize>], gen_kill: &[(Bits, Bits)], n: usize) -> Vec<Bits> {
    let mut live_in = vec![Bits::new(n); succs.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for i in (0..succs.len()).rev() {
            let mut inn = Bits::new(n);
            for &s in &succs[i] {
                inn.union(&live_in[s]);
            }
            let (gen_, kill) = &gen_kill[i];
            for (w, (g, k)) in inn.0.iter_mut().zip(gen_.0.iter().zip(&kill.0)) {
                *w = (*w & !k) | g;
            }
            if inn != live_in[i] {
                live_in[i] = inn;
                changed = true;
            }
        }
    }
    live_in
}

/// The tracked locals, each block's successors (indices), its statements'
/// uses and definitions, and its live-in set.
struct Flow {
    dense: Dense,
    succs: Vec<Vec<usize>>,
    stmts: Vec<Vec<(Vec<LocalId>, Option<LocalId>)>>,
    live_in: Vec<Bits>,
}

impl Flow {
    fn new(f: &MirFunction, tracked: impl Fn(LocalId) -> bool) -> Self {
        let ids: Vec<LocalId> = f
            .locals
            .iter()
            .map(|l| l.id)
            .filter(|&l| tracked(l))
            .collect();
        let dense = Dense {
            index: ids.iter().enumerate().map(|(i, &l)| (l, i)).collect(),
            ids,
        };
        let pos: HashMap<_, usize> = f
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| (b.id, i))
            .collect();
        let succs: Vec<Vec<usize>> = f
            .blocks
            .iter()
            .map(|b| {
                successors(&b.terminator)
                    .iter()
                    .filter_map(|s| pos.get(s).copied())
                    .collect()
            })
            .collect();
        let stmts: Vec<Vec<(Vec<LocalId>, Option<LocalId>)>> = f
            .blocks
            .iter()
            .map(|b| b.stmts.iter().map(stmt_uses).collect())
            .collect();
        let gk: Vec<(Bits, Bits)> = f
            .blocks
            .iter()
            .zip(&stmts)
            .map(|(b, st)| gen_kill(&dense, b, st))
            .collect();
        let live_in = solve(&succs, &gk, dense.ids.len());
        Self {
            dense,
            succs,
            stmts,
            live_in,
        }
    }

    /// What block `bi` still holds at its terminator.
    fn live_out(&self, bi: usize, b: &crate::BasicBlock) -> Bits {
        let mut live = self.dense.bits(&term_uses(&b.terminator));
        for &s in &self.succs[bi] {
            live.union(&self.live_in[s]);
        }
        live
    }
}

/// The locals `tracked` accepts that hold a value while a collection can
/// run; only these need a GC root.
///
/// Collections run at entry when `entry`
/// (an entry poll), at the end of a block `poll` accepts (a back-edge
/// poll), and during a statement `safepoint` takes for one: its live-out
/// locals, plus its operands when it reads them after allocating (a
/// runtime function; a rok callee roots what it needs itself).
pub fn live_across(
    f: &MirFunction,
    tracked: impl Fn(LocalId) -> bool,
    safepoint: impl Fn(&Statement) -> Option<bool>,
    poll: impl Fn(BlockId) -> bool,
    entry: bool,
) -> HashSet<LocalId> {
    let mut always: HashSet<LocalId> = addressed(f);
    always.extend(&f.pins);
    always.retain(|&l| tracked(l));
    let flow = Flow::new(f, tracked);
    let n = flow.dense.ids.len();
    let mut out = Bits::new(n);
    if n == 0 || f.blocks.is_empty() {
        return always;
    }
    if entry {
        out.union(&flow.live_in[0]);
    }
    for (bi, b) in f.blocks.iter().enumerate() {
        let mut live = flow.live_out(bi, b);
        if poll(b.id) {
            out.union(&live);
        }
        for (s, (uses, def)) in b.stmts.iter().zip(&flow.stmts[bi]).rev() {
            let def = def.and_then(|d| flow.dense.get(d));
            if let Some(reads) = safepoint(s) {
                let mut during = live.clone();
                if let Some(i) = def {
                    during.unset(i);
                }
                if reads {
                    during.union(&flow.dense.bits(uses));
                }
                out.union(&during);
            }
            if let Some(i) = def {
                live.unset(i);
            }
            live.union(&flow.dense.bits(uses));
        }
    }
    (0..n)
        .filter(|&i| out.has(i))
        .map(|i| flow.dense.ids[i])
        .chain(always)
        .collect()
}

/// The locals `tracked` accepts that are live at the end of each block
/// `at` accepts (a back-edge poll there), by block id.
pub fn live_out_of(
    f: &MirFunction,
    tracked: impl Fn(LocalId) -> bool,
    at: impl Fn(BlockId) -> bool,
) -> HashMap<u32, Vec<LocalId>> {
    let flow = Flow::new(f, tracked);
    let n = flow.dense.ids.len();
    let mut out = HashMap::new();
    if n == 0 {
        return out;
    }
    for (bi, b) in f.blocks.iter().enumerate().filter(|(_, b)| at(b.id)) {
        let live = flow.live_out(bi, b);
        let ls: Vec<LocalId> = (0..n)
            .filter(|&i| live.has(i))
            .map(|i| flow.dense.ids[i])
            .collect();
        out.insert(b.id.0, ls);
    }
    out
}

/// The locals `tracked` accepts that some path reaches the end of a block
/// `at` accepts without assigning (parameters count as assigned).
pub fn maybe_unassigned(
    f: &MirFunction,
    tracked: impl Fn(LocalId) -> bool,
    at: impl Fn(BlockId) -> bool,
) -> HashSet<LocalId> {
    let flow = Flow::new(f, &tracked);
    let n = flow.dense.ids.len();
    let mut out = HashSet::new();
    if n == 0 || f.blocks.is_empty() {
        return out;
    }
    // Forward must-analysis: assigned on every path into the block.
    let defs: Vec<Bits> = f
        .blocks
        .iter()
        .zip(&flow.stmts)
        .map(|(b, st)| {
            let mut d = flow.dense.bits(&b.params);
            for (_, def) in st {
                if let Some(i) = def.and_then(|l| flow.dense.get(l)) {
                    d.set(i);
                }
            }
            d
        })
        .collect();
    let mut preds = vec![Vec::new(); f.blocks.len()];
    for (i, ss) in flow.succs.iter().enumerate() {
        for &s in ss {
            preds[s].push(i);
        }
    }
    let mut full = Bits::new(n);
    for i in 0..n {
        full.set(i);
    }
    let mut entry = flow.dense.bits(&f.params);
    entry.union(&defs[0]);
    let mut assigned: Vec<Bits> = vec![full; f.blocks.len()];
    assigned[0] = entry;
    let mut changed = true;
    while changed {
        changed = false;
        for b in 1..f.blocks.len() {
            let mut inn: Option<Bits> = None;
            for &p in &preds[b] {
                inn = Some(match inn {
                    None => assigned[p].clone(),
                    Some(mut x) => {
                        for (w, o) in x.0.iter_mut().zip(&assigned[p].0) {
                            *w &= o;
                        }
                        x
                    }
                });
            }
            let mut o = inn.unwrap_or_else(|| Bits::new(n));
            o.union(&defs[b]);
            if o != assigned[b] {
                assigned[b] = o;
                changed = true;
            }
        }
    }
    for (bi, _) in f.blocks.iter().enumerate().filter(|(_, b)| at(b.id)) {
        for i in (0..n).filter(|&i| !assigned[bi].has(i)) {
            out.insert(flow.dense.ids[i]);
        }
    }
    out
}

/// Where each local `tracked` accepts dies in `f`.
pub fn deaths(f: &MirFunction, tracked: impl Fn(LocalId) -> bool) -> Deaths {
    let mut out = Deaths {
        on_entry: vec![Vec::new(); f.blocks.len()],
        after_stmt: f
            .blocks
            .iter()
            .map(|b| vec![Vec::new(); b.stmts.len()])
            .collect(),
    };
    // Their roots hold until the scope ends (`StorageDead`).
    let mut held = addressed(f);
    held.extend(&f.pins);
    let flow = Flow::new(f, |l| tracked(l) && !held.contains(&l));
    let Flow {
        dense,
        succs,
        stmts,
        live_in,
    } = &flow;
    if dense.ids.is_empty() {
        return out;
    }
    for (bi, b) in f.blocks.iter().enumerate() {
        // What this block still holds at its terminator.
        let mut live = flow.live_out(bi, b);
        for &s in &succs[bi] {
            for (i, &l) in dense.ids.iter().enumerate() {
                let dies = live.has(i) && !live_in[s].has(i);
                if dies && !f.blocks[s].params.contains(&l) && !out.on_entry[s].contains(&l) {
                    out.on_entry[s].push(l);
                }
            }
        }
        for (si, (uses, def)) in stmts[bi].iter().enumerate().rev() {
            let dead = &mut out.after_stmt[bi][si];
            for &l in uses.iter().chain(def) {
                if dense.get(l).is_some_and(|i| !live.has(i)) && !dead.contains(&l) {
                    dead.push(l);
                }
            }
            if let Some(i) = def.and_then(|d| dense.get(d)) {
                live.unset(i);
            }
            live.union(&dense.bits(uses));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BasicBlock, LocalDecl, MirTy, RValue};
    use rokc_diagnostic::Span;
    use rokc_ir_common::{BlockId, FunctionId};

    const fn local(i: u32) -> LocalDecl {
        LocalDecl {
            id: LocalId(i),
            name: None,
            ty: MirTy::GcRef,
        }
    }

    const fn assign(t: u32, from: u32) -> Statement {
        Statement::Assign {
            target: LocalId(t),
            rvalue: RValue::Use(Operand::Copy(LocalId(from))),
        }
    }

    /// bb0: x0 = x1; x2 = x0; if x3 -> bb1 / bb2
    /// bb1: x4 = x2; return
    /// bb2: return
    fn sample() -> MirFunction {
        MirFunction {
            id: FunctionId(0),
            def_id: None,
            name: "f".into(),
            facts: rokc_ir_common::FnFacts::default(),
            strips: Vec::new(),
            pins: Vec::new(),
            params: vec![LocalId(1), LocalId(3)],
            locals: (0..5).map(local).collect(),
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    params: vec![],
                    stmts: vec![assign(0, 1), assign(2, 0)],
                    terminator: Terminator::If {
                        cond: Operand::Copy(LocalId(3)),
                        then_bb: BlockId(1),
                        then_args: vec![],
                        else_bb: BlockId(2),
                        else_args: vec![],
                    },
                },
                BasicBlock {
                    id: BlockId(1),
                    params: vec![],
                    stmts: vec![assign(4, 2)],
                    terminator: Terminator::Return(None),
                },
                BasicBlock {
                    id: BlockId(2),
                    params: vec![],
                    stmts: vec![],
                    terminator: Terminator::Return(None),
                },
            ],
            param_tys: vec![],
            ret_ty: MirTy::Unit,
            span: Span::default(),
        }
    }

    #[test]
    fn last_use_and_edge_deaths() {
        let f = sample();
        let d = deaths(&f, |_| true);
        // x1 dies at its only use, x0 at its last use.
        assert_eq!(d.after_stmt[0][0], vec![LocalId(1)]);
        assert_eq!(d.after_stmt[0][1], vec![LocalId(0)]);
        // x2 dies at its use in bb1 and on the edge into bb2; the
        // condition dies at the terminator (cleared on both entries).
        assert_eq!(d.after_stmt[1][0], vec![LocalId(2), LocalId(4)]);
        assert_eq!(d.on_entry[2], vec![LocalId(2), LocalId(3)]);
        assert_eq!(d.on_entry[1], vec![LocalId(3)]);
    }

    /// With `x2 = x0` a safepoint: its operand x0 and x3 (read by the
    /// terminator) are live across it, and the params x1 and x3 at the
    /// entry; x2 gets its value only after it, x4 never crosses one.
    #[test]
    fn live_across_a_safepoint() {
        let f = sample();
        let s = live_across(
            &f,
            |_| true,
            |s| {
                matches!(
                    s,
                    Statement::Assign {
                        target: LocalId(2),
                        ..
                    }
                )
                .then_some(true)
            },
            |_| false,
            true,
        );
        let mut got: Vec<u32> = s.iter().map(|l| l.0).collect();
        got.sort_unstable();
        assert_eq!(got, vec![0, 1, 3]);
    }

    /// bb0: x0 = x1; goto bb1
    /// bb1: x2 = x0; if x3 -> bb1 / bb2   (a loop)
    /// bb2: x4 = x0; return
    fn looping() -> MirFunction {
        let mut f = sample();
        f.blocks[0].stmts = vec![assign(0, 1)];
        f.blocks[0].terminator = Terminator::Goto {
            target: BlockId(1),
            args: vec![],
        };
        f.blocks[1].stmts = vec![assign(2, 0)];
        f.blocks[1].terminator = Terminator::If {
            cond: Operand::Copy(LocalId(3)),
            then_bb: BlockId(1),
            then_args: vec![],
            else_bb: BlockId(2),
            else_args: vec![],
        };
        f.blocks[2].stmts = vec![assign(4, 0)];
        f
    }

    /// A value read in a loop and after it stays live around the back
    /// edge: it dies at its use after the loop, not in the body.
    #[test]
    fn loop_keeps_values_live() {
        let f = looping();
        let d = deaths(&f, |_| true);
        assert!(
            !d.after_stmt[1][0].contains(&LocalId(0)),
            "{:?}",
            d.after_stmt
        );
        assert!(
            d.after_stmt[2][0].contains(&LocalId(0)),
            "{:?}",
            d.after_stmt
        );
        // x1 dies at its one use; the condition on the loop's exit.
        assert_eq!(d.after_stmt[0][0], vec![LocalId(1)]);
        assert!(d.on_entry[2].contains(&LocalId(3)), "{:?}", d.on_entry);
        assert!(!d.on_entry[1].contains(&LocalId(3)), "{:?}", d.on_entry);
    }

    /// A back-edge poll at the end of the loop body roots what the next
    /// iteration and the exit read (x0, the condition x3), not x2.
    #[test]
    fn back_edge_poll_roots_loop_values() {
        let f = looping();
        let s = live_across(&f, |_| true, |_| None, |b| b == BlockId(1), false);
        let mut got: Vec<u32> = s.iter().map(|l| l.0).collect();
        got.sort_unstable();
        assert_eq!(got, vec![0, 3]);
        // Untracked locals are never roots.
        let s = live_across(&f, |l| l != LocalId(0), |_| None, |_| true, true);
        assert!(!s.contains(&LocalId(0)), "{s:?}");
        assert!(s.contains(&LocalId(1)), "{s:?}");
    }
}
