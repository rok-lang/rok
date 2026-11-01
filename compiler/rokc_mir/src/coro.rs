//! Coroutine lowering (spec 708 / 810): a compiler-generated state
//! machine over MIR, in place of `llvm.coro.*`.
//!
//! A coroutine body (HIR `is_async` / `is_generator`) is lowered like any
//! function, with `Terminator::Suspend` at its `yield` / `await` points,
//! then split into
//!
//! - `resume(frame, resume_args..) -> step`: reloads the live locals,
//!   dispatches on the frame's state, and at each suspend point spills
//!   them back, records the next state and returns the step value;
//! - `new(args..) -> {resume, frame}`: allocates the zeroed frame
//!   (state 0) and stores the arguments.
//!
//! The frame is a GC object of `(i32 state, locals..)`, traced through
//! its `ScanTable` rather than the shadow stack (704); inside `resume`
//! the locals are ordinary roots. Only locals live into a resume point
//! (`live_in`) get a slot; a pinned local's slot holds ambiguous words
//! so its referent cannot move while the coroutine is suspended.

use std::collections::{BTreeSet, HashMap};

use rokc_ir_common::{BlockId, ConstVal, FunctionId, LocalId, PrimOp, PrimTy};

use crate::ty::{code_ty, fat_fn};
use crate::{BasicBlock, LocalDecl, MirFunction, MirTy, Operand, RValue, Statement, Terminator};

/// Rewrite coroutine body `f` (whose last `n_resume` params come with
/// every resume) into its resume function, keeping `f.id`, and return its
/// constructor `ctor`. `pinning(ty)`: the type a frame slot of a pinned
/// local (`f.pins`) is allocated as, so its referent never moves while
/// the frame holds it (ambiguous words pin, 703 §7.3.1).
pub fn transform(
    f: &mut MirFunction,
    n_resume: usize,
    ctor: FunctionId,
    pinning: impl Fn(&MirTy) -> MirTy,
) -> MirFunction {
    remove_block_params(f);
    let n_init = f.params.len() - n_resume;
    let resume_params: Vec<LocalId> = f.params[n_init..].to_vec();

    // Old entry → a fresh id; the dispatcher becomes block 0.
    let mut next_block = f.blocks.iter().map(|b| b.id.0 + 1).max().unwrap_or(0);
    let old_entry = BlockId(next_block);
    next_block += 1;
    for b in &mut f.blocks {
        if b.id == BlockId(0) {
            b.id = old_entry;
        }
        for t in successors_mut(&mut b.terminator) {
            if *t == BlockId(0) {
                *t = old_entry;
            }
        }
    }

    // States: 0 = start, then one per suspend point in block order. A
    // finished coroutine re-runs the block that parks it (the final
    // value is rebuilt from the reloaded locals).
    let mut targets: Vec<BlockId> = vec![old_entry];
    let mut final_block = None;
    for b in &f.blocks {
        if let Terminator::Suspend {
            resume, is_final, ..
        } = &b.terminator
        {
            if *is_final {
                final_block = Some((targets.len() as u32, b.id));
                targets.push(b.id);
            } else {
                targets.push(*resume);
            }
        }
    }

    // Only the locals live into a resume point cross a suspension.
    let live = live_in(f);
    let skipped: BTreeSet<LocalId> = f
        .locals
        .iter()
        .filter(|l| matches!(l.ty, MirTy::Unit))
        .map(|l| l.id)
        .chain(resume_params.iter().copied())
        .collect();
    let kept = |l: &LocalId| !skipped.contains(l);
    let live_at = |b: BlockId| -> Vec<LocalId> {
        live.get(&b)
            .map(|s| s.iter().copied().filter(kept).collect())
            .unwrap_or_default()
    };
    let mut frame_set: std::collections::BTreeSet<LocalId> =
        f.params[..n_init].iter().copied().filter(kept).collect();
    for t in &targets {
        frame_set.extend(live_at(*t));
    }
    let frame_locals: Vec<LocalId> = frame_set.into_iter().collect();
    let mut fields = vec![MirTy::Prim(PrimTy::I32)];
    fields.extend(
        frame_locals
            .iter()
            .map(|l| f.locals[l.0 as usize].ty.clone()),
    );
    let frame_ty = MirTy::Tuple(fields);
    // The frame as allocated: a pinned local's slot scans as pinning words.
    let mut alloc_fields = vec![MirTy::Prim(PrimTy::I32)];
    alloc_fields.extend(frame_locals.iter().map(|l| {
        let ty = &f.locals[l.0 as usize].ty;
        if f.pins.contains(l) {
            pinning(ty)
        } else {
            ty.clone()
        }
    }));
    let alloc_ty = MirTy::Tuple(alloc_fields);
    let field_of = |l: LocalId| -> u32 {
        1 + u32::try_from(
            frame_locals
                .iter()
                .position(|x| *x == l)
                .expect("frame local"),
        )
        .expect("frame too large")
    };

    let new_local = |f: &mut MirFunction, ty: MirTy| {
        let id = LocalId(u32::try_from(f.locals.len()).expect("too many locals"));
        f.locals.push(LocalDecl { id, name: None, ty });
        id
    };
    let frame = new_local(f, MirTy::GcRef);
    f.locals[frame.0 as usize].name = Some("#frame".to_owned());
    let frame_op = Operand::Copy(frame);
    let state_const = |s: u32| {
        Operand::Const(ConstVal::Int {
            value: i128::from(s),
            ty: PrimTy::I32,
        })
    };

    let spill = |stmts: &mut Vec<Statement>, set: &[LocalId], state: u32| {
        for &l in set {
            stmts.push(Statement::HeapStore {
                ptr: frame_op.clone(),
                ty: frame_ty.clone(),
                index: field_of(l),
                value: Operand::Copy(l),
            });
        }
        stmts.push(Statement::HeapStore {
            ptr: frame_op.clone(),
            ty: frame_ty.clone(),
            index: 0,
            value: state_const(state),
        });
    };
    let (final_state, final_set) = match final_block {
        Some((s, b)) => (s, live_at(b)),
        None => (u32::MAX, Vec::new()),
    };
    let mut state = 1u32;
    for b in &mut f.blocks {
        match std::mem::replace(&mut b.terminator, Terminator::Unreachable) {
            Terminator::Suspend {
                value,
                is_final,
                resume,
            } => {
                if is_final {
                    spill(&mut b.stmts, &final_set, final_state);
                } else {
                    spill(&mut b.stmts, &live_at(resume), state);
                }
                state += 1;
                b.terminator = Terminator::Return(value);
            }
            // Only an error leaves a coroutine body by `return`.
            Terminator::Return(value) => {
                spill(&mut b.stmts, &final_set, final_state);
                b.terminator = Terminator::Return(value);
            }
            t => b.terminator = t,
        }
    }

    // Dispatcher: branch on the frame's state to a block that reloads
    // what is live there.
    let st = new_local(f, MirTy::Prim(PrimTy::I32));
    let mut chain: Vec<BasicBlock> = Vec::new();
    let mut cur = BasicBlock {
        id: BlockId(0),
        params: Vec::new(),
        stmts: vec![Statement::Assign {
            target: st,
            rvalue: RValue::HeapField {
                ptr: frame_op.clone(),
                ty: frame_ty.clone(),
                index: 0,
            },
        }],
        terminator: Terminator::Unreachable,
    };
    for (s, target) in targets.iter().enumerate() {
        let c = new_local(f, MirTy::Prim(PrimTy::Bool));
        cur.stmts.push(Statement::Assign {
            target: c,
            rvalue: RValue::BinOp {
                op: PrimOp::Eq,
                op_ty: MirTy::Prim(PrimTy::I32),
                lhs: Operand::Copy(st),
                rhs: state_const(s as u32),
            },
        });
        let reload = BlockId(next_block);
        let next = BlockId(next_block + 1);
        next_block += 2;
        chain.push(BasicBlock {
            id: reload,
            params: Vec::new(),
            stmts: live_at(*target)
                .into_iter()
                .map(|l| Statement::Assign {
                    target: l,
                    rvalue: RValue::HeapField {
                        ptr: frame_op.clone(),
                        ty: frame_ty.clone(),
                        index: field_of(l),
                    },
                })
                .collect(),
            terminator: Terminator::Goto {
                target: *target,
                args: Vec::new(),
            },
        });
        cur.terminator = Terminator::If {
            cond: Operand::Copy(c),
            then_bb: reload,
            then_args: Vec::new(),
            else_bb: next,
            else_args: Vec::new(),
        };
        let done = std::mem::replace(
            &mut cur,
            BasicBlock {
                id: next,
                params: Vec::new(),
                stmts: Vec::new(),
                terminator: Terminator::Unreachable,
            },
        );
        chain.insert(chain.len() - 1, done);
    }
    chain.push(cur);
    chain.append(&mut f.blocks);
    f.blocks = chain;
    prune_unreachable_blocks(f);

    // Constructor: a zeroed frame (state 0) holding the initial args.
    let init_tys: Vec<MirTy> = f.param_tys[..n_init].to_vec();
    let resume_tys: Vec<MirTy> = f.param_tys[n_init..].to_vec();
    let mut locals: Vec<LocalDecl> = Vec::new();
    let mut local = |ty: MirTy| {
        let id = LocalId(u32::try_from(locals.len()).expect("too many locals"));
        locals.push(LocalDecl { id, name: None, ty });
        id
    };
    let args: Vec<LocalId> = init_tys.iter().map(|t| local(t.clone())).collect();
    let new_frame = local(MirTy::GcRef);
    let code_t = code_ty(resume_tys.clone(), f.ret_ty.clone());
    let code = local(code_t.clone());
    let fat = fat_fn(resume_tys.clone(), f.ret_ty.clone());
    let result = local(fat.clone());
    let mut body = vec![Statement::Assign {
        target: new_frame,
        rvalue: RValue::HeapAlloc { ty: alloc_ty },
    }];
    for (i, a) in args.iter().enumerate() {
        let p = f.params[i];
        if !frame_locals.contains(&p) {
            continue;
        }
        body.push(Statement::HeapStore {
            ptr: Operand::Copy(new_frame),
            ty: frame_ty.clone(),
            index: field_of(p),
            value: Operand::Copy(*a),
        });
    }
    body.push(Statement::Assign {
        target: code,
        rvalue: RValue::FnAddr {
            callee: f.id,
            fn_ty: code_t,
        },
    });
    body.push(Statement::Assign {
        target: result,
        rvalue: RValue::StructInit {
            ty: fat.clone(),
            fields: vec![Operand::Copy(code), Operand::Copy(new_frame)],
        },
    });
    let ctor_fn = MirFunction {
        id: ctor,
        def_id: None,
        name: format!("{}__new", f.name),
        params: args,
        locals,
        blocks: vec![BasicBlock {
            id: BlockId(0),
            params: Vec::new(),
            stmts: body,
            terminator: Terminator::Return(Some(Operand::Copy(result))),
        }],
        param_tys: init_tys,
        ret_ty: fat,
        span: f.span,
        facts: rokc_ir_common::FnFacts::default(),
        strips: Vec::new(),
        pins: Vec::new(),
    };

    let mut params = vec![frame];
    params.extend(resume_params);
    let mut param_tys = vec![MirTy::GcRef];
    param_tys.extend(resume_tys);
    f.params = params;
    f.param_tys = param_tys;
    ctor_fn
}

/// Drop blocks the dispatcher no longer reaches (the continuation of
/// the final suspend point): MLIR leaves their ops unconverted.
fn prune_unreachable_blocks(f: &mut MirFunction) {
    let index: std::collections::HashMap<BlockId, usize> = f
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id, i))
        .collect();
    let mut seen = vec![false; f.blocks.len()];
    let mut work = vec![0usize];
    while let Some(i) = work.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        for t in successors_mut(&mut f.blocks[i].terminator) {
            work.push(index[t]);
        }
    }
    let mut i = 0;
    f.blocks.retain(|_| {
        i += 1;
        seen[i - 1]
    });
}

/// Locals live on entry to each block (backward dataflow).
fn live_in(f: &MirFunction) -> HashMap<BlockId, BTreeSet<LocalId>> {
    let mut gen_kill: Vec<(BTreeSet<LocalId>, BTreeSet<LocalId>)> = Vec::new();
    for b in &f.blocks {
        let (mut used, mut defd) = (BTreeSet::new(), BTreeSet::new());
        let use_op = |op: &Operand, defd: &BTreeSet<LocalId>, used: &mut BTreeSet<LocalId>| {
            if let Operand::Copy(l) = op {
                if !defd.contains(l) {
                    used.insert(*l);
                }
            }
        };
        for s in &b.stmts {
            let mut ops: Vec<Operand> = Vec::new();
            let mut def = None;
            match s {
                Statement::Assign { target, rvalue } => {
                    rvalue_operands(rvalue, &mut ops);
                    def = Some(*target);
                }
                Statement::ArrayWrite {
                    array,
                    index,
                    value,
                    ..
                } => ops.extend([Operand::Copy(*array), index.clone(), value.clone()]),
                // A pinned local stays in the frame until its scope ends.
                Statement::StorageDead(l) if f.pins.contains(l) => ops.push(Operand::Copy(*l)),
                Statement::StorageDead(_) | Statement::Loc(..) => {}
                Statement::BufStore {
                    buf, index, value, ..
                } => ops.extend([buf.clone(), index.clone(), value.clone()]),
                Statement::RawStore { addr, value, .. } => {
                    ops.extend([addr.clone(), value.clone()]);
                }
                Statement::HeapStore { ptr, value, .. } => ops.extend([ptr.clone(), value.clone()]),
                Statement::StaticStore { value, .. } => ops.push(value.clone()),
            }
            for op in &ops {
                use_op(op, &defd, &mut used);
            }
            if let Some(d) = def {
                defd.insert(d);
            }
        }
        let mut ops: Vec<Operand> = Vec::new();
        match &b.terminator {
            Terminator::Goto { args, .. } => ops.extend(args.iter().cloned()),
            Terminator::If {
                cond,
                then_args,
                else_args,
                ..
            } => {
                ops.push(cond.clone());
                ops.extend(then_args.iter().cloned());
                ops.extend(else_args.iter().cloned());
            }
            // Pinned locals of the body's own scope (no `StorageDead`)
            // live until it ends.
            Terminator::Return(v) | Terminator::Suspend { value: v, .. } => {
                ops.extend(v.clone());
                ops.extend(f.pins.iter().map(|l| Operand::Copy(*l)));
            }
            Terminator::Unreachable => {}
        }
        for op in &ops {
            use_op(op, &defd, &mut used);
        }
        gen_kill.push((used, defd));
    }
    let index: HashMap<BlockId, usize> = f
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id, i))
        .collect();
    let succs: Vec<Vec<usize>> = f
        .blocks
        .iter()
        .map(|b| {
            let mut t = b.terminator.clone();
            successors_mut(&mut t)
                .into_iter()
                .map(|x| index[x])
                .collect()
        })
        .collect();
    let mut live: Vec<BTreeSet<LocalId>> = vec![BTreeSet::new(); f.blocks.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for i in (0..f.blocks.len()).rev() {
            let mut out: BTreeSet<LocalId> = BTreeSet::new();
            for &s in &succs[i] {
                out.extend(live[s].iter().copied());
            }
            let (used, defd) = &gen_kill[i];
            let mut inn: BTreeSet<LocalId> = out.difference(defd).copied().collect();
            inn.extend(used.iter().copied());
            if inn != live[i] {
                live[i] = inn;
                changed = true;
            }
        }
    }
    f.blocks.iter().map(|b| b.id).zip(live).collect()
}

/// Every operand `r` reads (an array read names its local).
pub fn rvalue_operands(r: &RValue, out: &mut Vec<Operand>) {
    match r {
        RValue::Use(o)
        | RValue::UnaryOp { operand: o, .. }
        | RValue::FieldRead { target: o, .. }
        | RValue::Cast { source: o, .. }
        | RValue::Bitcast { source: o, .. }
        | RValue::HeapBox { value: o, .. }
        | RValue::BufAlloc { len: o, .. }
        | RValue::RawLoad { addr: o, .. }
        | RValue::HeapLoad { ptr: o, .. }
        | RValue::HeapField { ptr: o, .. } => out.push(o.clone()),
        RValue::BinOp { lhs, rhs, .. } => out.extend([lhs.clone(), rhs.clone()]),
        RValue::Call { args, .. }
        | RValue::StructInit { fields: args, .. }
        | RValue::ArrayInit { elements: args, .. } => out.extend(args.iter().cloned()),
        RValue::WithField { source, value, .. } => out.extend([source.clone(), value.clone()]),
        RValue::ArrayIndex { array, index, .. } => {
            out.extend([Operand::Copy(*array), index.clone()]);
        }
        RValue::AddrOf(l) => out.push(Operand::Copy(*l)),
        RValue::BufLoad { buf, index, .. } => out.extend([buf.clone(), index.clone()]),
        RValue::BufAtomic {
            buf, index, args, ..
        } => {
            out.extend([buf.clone(), index.clone()]);
            out.extend(args.iter().cloned());
        }
        RValue::CallIndirect { callee, args, .. } => {
            out.push(callee.clone());
            out.extend(args.iter().cloned());
        }
        RValue::FnAddr { .. }
        | RValue::StaticLoad { .. }
        | RValue::StaticAddr(_)
        | RValue::SizeOf { .. }
        | RValue::HeapAlloc { .. } => {}
    }
}

fn successors_mut(t: &mut Terminator) -> Vec<&mut BlockId> {
    match t {
        Terminator::Goto { target, .. } => vec![target],
        Terminator::If {
            then_bb, else_bb, ..
        } => vec![then_bb, else_bb],
        Terminator::Suspend { resume, .. } => vec![resume],
        Terminator::Return(_) | Terminator::Unreachable => Vec::new(),
    }
}

/// Turn block params into plain locals assigned on each incoming edge,
/// so every value survives the dispatcher's jumps (a block param is an
/// SSA value only its predecessors define).
fn remove_block_params(f: &mut MirFunction) {
    let params: std::collections::HashMap<BlockId, Vec<LocalId>> = f
        .blocks
        .iter()
        .filter(|b| !b.params.is_empty())
        .map(|b| (b.id, b.params.clone()))
        .collect();
    if params.is_empty() {
        return;
    }
    let mut next_block = f.blocks.iter().map(|b| b.id.0 + 1).max().unwrap_or(0);
    let mut extra: Vec<BasicBlock> = Vec::new();
    let mut locals = std::mem::take(&mut f.locals);
    // Parallel copy: the args are read into temps before any param is
    // written.
    let mut copies = |target: BlockId, args: Vec<Operand>| -> Vec<Statement> {
        let Some(ps) = params.get(&target) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut temps = Vec::new();
        for (p, a) in ps.iter().zip(args) {
            if matches!(locals[p.0 as usize].ty, MirTy::Unit) {
                continue;
            }
            let id = LocalId(u32::try_from(locals.len()).expect("too many locals"));
            locals.push(LocalDecl {
                id,
                name: None,
                ty: locals[p.0 as usize].ty.clone(),
            });
            out.push(Statement::Assign {
                target: id,
                rvalue: RValue::Use(a),
            });
            temps.push((*p, id));
        }
        for (p, t) in temps {
            out.push(Statement::Assign {
                target: p,
                rvalue: RValue::Use(Operand::Copy(t)),
            });
        }
        out
    };
    for b in &mut f.blocks {
        b.params.clear();
        match &mut b.terminator {
            Terminator::Goto { target, args } => {
                let moves = copies(*target, std::mem::take(args));
                b.stmts.extend(moves);
            }
            Terminator::If {
                then_bb,
                then_args,
                else_bb,
                else_args,
                ..
            } => {
                for (bb, args) in [(then_bb, then_args), (else_bb, else_args)] {
                    if args.is_empty() {
                        continue;
                    }
                    let stmts = copies(*bb, std::mem::take(args));
                    let id = BlockId(next_block);
                    next_block += 1;
                    extra.push(BasicBlock {
                        id,
                        params: Vec::new(),
                        stmts,
                        terminator: Terminator::Goto {
                            target: *bb,
                            args: Vec::new(),
                        },
                    });
                    *bb = id;
                }
            }
            _ => {}
        }
    }
    f.locals = locals;
    f.blocks.extend(extra);
}

#[cfg(test)]
mod tests {
    use super::*;

    const I64: MirTy = MirTy::Prim(PrimTy::I64);

    const fn add(target: u32, lhs: u32, rhs: u32) -> Statement {
        Statement::Assign {
            target: LocalId(target),
            rvalue: RValue::BinOp {
                op: PrimOp::Add,
                op_ty: I64,
                lhs: Operand::Copy(LocalId(lhs)),
                rhs: Operand::Copy(LocalId(rhs)),
            },
        }
    }

    const fn block(id: u32, stmts: Vec<Statement>, terminator: Terminator) -> BasicBlock {
        BasicBlock {
            id: BlockId(id),
            params: Vec::new(),
            stmts,
            terminator,
        }
    }

    /// `yield fn g(n) { let a = n + n; yield a; let b = a + n; yield b (final) }`
    fn generator() -> MirFunction {
        MirFunction {
            id: FunctionId(0),
            def_id: None,
            name: "g".to_owned(),
            params: vec![LocalId(0)],
            locals: (0..3)
                .map(|i| LocalDecl {
                    id: LocalId(i),
                    name: None,
                    ty: I64,
                })
                .collect(),
            blocks: vec![
                block(
                    0,
                    vec![add(1, 0, 0)],
                    Terminator::Suspend {
                        value: Some(Operand::Copy(LocalId(1))),
                        resume: BlockId(1),
                        is_final: false,
                    },
                ),
                block(
                    1,
                    vec![add(2, 1, 0)],
                    Terminator::Suspend {
                        value: Some(Operand::Copy(LocalId(2))),
                        resume: BlockId(2),
                        is_final: true,
                    },
                ),
                block(2, Vec::new(), Terminator::Unreachable),
            ],
            param_tys: vec![I64],
            ret_ty: I64,
            span: rokc_diagnostic::Span::default(),
            facts: rokc_ir_common::FnFacts::default(),
            strips: Vec::new(),
            pins: Vec::new(),
        }
    }

    #[test]
    fn transform_builds_state_machine() {
        let mut f = generator();
        let ctor = transform(&mut f, 0, FunctionId(1), MirTy::clone);

        // resume(frame) -> step; the constructor takes the initial args.
        assert_eq!(f.params.len(), 1);
        assert_eq!(f.param_tys, vec![MirTy::GcRef]);
        assert_eq!(ctor.name, "g__new");
        assert_eq!(ctor.param_tys, vec![I64]);

        // Frame = (state, n, a): `b` never crosses a suspension.
        let Statement::Assign {
            rvalue: RValue::HeapAlloc { ty: frame_ty },
            ..
        } = &ctor.blocks[0].stmts[0]
        else {
            panic!("ctor must allocate the frame first");
        };
        assert_eq!(
            *frame_ty,
            MirTy::Tuple(vec![MirTy::Prim(PrimTy::I32), I64, I64])
        );

        // No suspend survives, the dispatcher (block 0) reads the state,
        // and the dead continuation of the final suspend is pruned.
        assert!(
            f.blocks
                .iter()
                .all(|b| !matches!(b.terminator, Terminator::Suspend { .. }))
        );
        assert!(matches!(
            &f.blocks[0].stmts[0],
            Statement::Assign {
                rvalue: RValue::HeapField { index: 0, .. },
                ..
            }
        ));
        assert!(f.blocks.iter().all(|b| b.id != BlockId(2)));
    }

    #[test]
    fn suspend_spills_live_locals_and_next_state() {
        let mut f = generator();
        let _ = transform(&mut f, 0, FunctionId(1), MirTy::clone);
        // The first suspend point (old block 0, now renumbered) stores
        // `a` (field 2) and `n` (field 1), then state 1, and returns `a`.
        let first = f
            .blocks
            .iter()
            .find(|b| {
                matches!(&b.terminator,
                    Terminator::Return(Some(Operand::Copy(l))) if *l == LocalId(1))
            })
            .expect("first suspend point");
        let stores: Vec<(u32, bool)> = first
            .stmts
            .iter()
            .filter_map(|s| match s {
                Statement::HeapStore { index, value, .. } => {
                    Some((*index, matches!(value, Operand::Const(_))))
                }
                _ => None,
            })
            .collect();
        assert_eq!(stores, vec![(1, false), (2, false), (0, true)]);
    }

    /// `async fn h(n) { let a = n + n; r = await a; return r + a }`: `r`
    /// (local 1) comes with every resume.
    fn awaiting() -> MirFunction {
        let mut f = generator();
        f.params = vec![LocalId(0), LocalId(1)];
        f.param_tys = vec![I64, I64];
        f.locals = (0..4)
            .map(|i| LocalDecl {
                id: LocalId(i),
                name: None,
                ty: I64,
            })
            .collect();
        f.blocks = vec![
            block(
                0,
                vec![add(2, 0, 0)],
                Terminator::Suspend {
                    value: Some(Operand::Copy(LocalId(2))),
                    resume: BlockId(1),
                    is_final: false,
                },
            ),
            block(
                1,
                vec![add(3, 1, 2)],
                Terminator::Return(Some(Operand::Copy(LocalId(3)))),
            ),
        ];
        f
    }

    #[test]
    fn resume_args_come_with_every_resume() {
        let mut f = awaiting();
        let ctor = transform(&mut f, 1, FunctionId(1), MirTy::clone);
        // The constructor takes `n` only; `resume(frame, r)`.
        assert_eq!(ctor.param_tys, vec![I64]);
        assert_eq!(f.param_tys, vec![MirTy::GcRef, I64]);
        // The frame keeps `n` and `a`, not the resume argument or `r + a`.
        let Statement::Assign {
            rvalue: RValue::HeapAlloc { ty: frame_ty },
            ..
        } = &ctor.blocks[0].stmts[0]
        else {
            panic!("ctor must allocate the frame first");
        };
        assert_eq!(
            *frame_ty,
            MirTy::Tuple(vec![MirTy::Prim(PrimTy::I32), I64, I64])
        );
        // The return reads the reloaded `a` and the new `r`.
        assert!(
            f.blocks
                .iter()
                .all(|b| !matches!(b.terminator, Terminator::Suspend { .. }))
        );
        let live = live_in(&awaiting());
        assert_eq!(live[&BlockId(1)], BTreeSet::from([LocalId(1), LocalId(2)]));
    }

    #[test]
    fn live_in_is_backward_dataflow() {
        let f = generator();
        let live = live_in(&f);
        assert_eq!(live[&BlockId(0)], BTreeSet::from([LocalId(0)]));
        assert_eq!(live[&BlockId(1)], BTreeSet::from([LocalId(0), LocalId(1)]));
    }
}
