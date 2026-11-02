//! Aggregates built where they go. `t = rv; .. x = t`, where the
//! temporary `t` is read only by that copy, becomes `x = rv`: a value in
//! memory (over 64 bytes) is then built in `x`'s slot, a call's
//! by-address result written straight into it, instead of in `t`'s
//! and copied.

use std::collections::HashSet;

use rokc_ir_common::LocalId;

use crate::liveness::{block_local, stmt_uses, term_uses};
use crate::{MirFunction, MirTy, Operand, RValue, Statement};

pub fn copy_into(f: &mut MirFunction) {
    let once = block_local(f);
    let addressed = crate::addressed(f);
    let mut uses = vec![0u32; f.locals.len()];
    let mut block_params: HashSet<LocalId> = HashSet::new();
    for b in &f.blocks {
        block_params.extend(&b.params);
        let stmts = b.stmts.iter().flat_map(|s| stmt_uses(s).0);
        for u in stmts.chain(term_uses(&b.terminator)) {
            uses[u.0 as usize] += 1;
        }
    }
    let temp = |t: LocalId| {
        let l = &f.locals[t.0 as usize];
        once[t.0 as usize]
            && uses[t.0 as usize] == 1
            && matches!(
                l.ty,
                MirTy::Struct(..) | MirTy::Tuple(_) | MirTy::Array(..) | MirTy::Union(..)
            )
            && l.name.as_deref().is_none_or(|n| !crate::debug_visible(n))
    };
    let mut gone: HashSet<LocalId> = HashSet::new();
    for b in &mut f.blocks {
        let mut at = 0;
        while at < b.stmts.len() {
            let Statement::Assign {
                target: dst,
                rvalue: RValue::Use(Operand::Copy(tmp)),
            } = b.stmts[at]
            else {
                at += 1;
                continue;
            };
            let fits = dst != tmp
                && temp(tmp)
                && f.locals[dst.0 as usize].ty == f.locals[tmp.0 as usize].ty
                && !addressed.contains(&dst)
                && !block_params.contains(&dst);
            let def = fits
                .then(|| {
                    b.stmts[..at].iter().position(
                        |s| matches!(s, Statement::Assign { target, .. } if *target == tmp),
                    )
                })
                .flatten()
                .filter(|&d| {
                    let mentions = |s: &Statement| {
                        let (reads, sets) = stmt_uses(s);
                        sets == Some(dst) || reads.contains(&dst)
                    };
                    !b.stmts[d + 1..at].iter().any(mentions) && builds_over(&b.stmts[d], dst)
                });
            let Some(d) = def else {
                at += 1;
                continue;
            };
            if let Statement::Assign { target, .. } = &mut b.stmts[d] {
                *target = dst;
            }
            b.stmts.remove(at);
            gone.insert(tmp);
        }
    }
    for b in &mut f.blocks {
        b.stmts
            .retain(|s| !matches!(s, Statement::StorageDead(l) if gone.contains(l)));
    }
}

/// `s` may assign `x` in place: it reads `x` only where its value is
/// taken before any of it is written (a call's arguments, which a callee
/// copies on entry; the source of a field update).
fn builds_over(s: &Statement, x: LocalId) -> bool {
    let Statement::Assign { rvalue, .. } = s else {
        return false;
    };
    let mut ops = Vec::new();
    crate::coro::rvalue_operands(rvalue, &mut ops);
    let reads = ops.iter().any(|o| matches!(o, Operand::Copy(l) if *l == x));
    !reads
        || matches!(rvalue, RValue::Call { .. } | RValue::CallIndirect { .. })
        || matches!(rvalue, RValue::WithField { source: Operand::Copy(s), value, .. }
            if *s == x && !matches!(value, Operand::Copy(v) if *v == x))
}
