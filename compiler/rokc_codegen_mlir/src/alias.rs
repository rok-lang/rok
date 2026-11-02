//! Alias scopes of `List`s (LLVM scoped `noalias` metadata).
//!
//! A `List`'s object and buffer belong to it alone, so
//! locals that only ever take lists fresh from calls never share memory. A
//! call touching only its first argument's list (`xs[i]`, `xs.len()`) gets
//! its local's scope and `noalias` for the others; LLVM keeps both on the
//! inlined accesses, so a store to one list doesn't clobber loads from
//! another.

use std::collections::{HashMap, HashSet};

use rokc_ir_common::{FunctionId, Linkage};
use rokc_mir::{MirFunction, MirModule, MirTy, Operand, RValue, Statement, Target, Terminator};

/// The functions the scopes rely on (`ListAlias::new`).
#[derive(Default)]
pub struct ListAlias {
    /// Every value it returns is a list allocated during the call.
    fresh: HashSet<FunctionId>,
    /// Touches only its first argument's list object and buffer, writes no
    /// reference and reaches no safepoint (which could move another list).
    confined: HashSet<FunctionId>,
}

/// How a local of a confined function derives from its first argument.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    List,
    /// The list's object (`GcBuf<ListHead<T>>`).
    Object,
    /// Its head (length, capacity, buffer), loaded.
    Head,
    /// The element buffer.
    Buf,
}

/// The definitions of each local; `opaque`: locals also set some other
/// way (parameters, block and region parameters, an address taken).
struct Defs<'a> {
    by: HashMap<u32, Vec<&'a RValue>>,
    opaque: HashSet<u32>,
}

fn defs(f: &MirFunction) -> Defs<'_> {
    let mut by: HashMap<u32, Vec<&RValue>> = HashMap::new();
    let mut opaque: HashSet<u32> = f.params.iter().map(|p| p.0).collect();
    for b in &f.blocks {
        opaque.extend(b.params.iter().map(|p| p.0));
        for s in &b.stmts {
            if let Statement::Assign { target, rvalue } = s {
                if let RValue::AddrOf(l) = rvalue {
                    opaque.insert(l.0);
                }
                by.entry(target.0).or_default().push(rvalue);
            }
        }
    }
    Defs { by, opaque }
}

fn body(f: &MirFunction) -> bool {
    !f.blocks.is_empty()
        && (f.facts.linkage == Linkage::Defined || f.facts.linkage == Linkage::Prebuilt)
}

const fn copy(op: &Operand) -> Option<u32> {
    match op {
        Operand::Copy(l) => Some(l.0),
        Operand::Const(_) => None,
    }
}

impl ListAlias {
    /// The fixpoints over `module`: `lists` are the `List` struct ids,
    /// `quiet` the functions that reach no safepoint.
    pub fn new(
        module: &MirModule,
        lists: &HashSet<u32>,
        quiet: &HashSet<FunctionId>,
        target: &Target,
    ) -> Self {
        let mut me = Self::default();
        let is_list = |t: &MirTy| matches!(t, MirTy::Struct(id, _) if lists.contains(&id.0));
        let all: Vec<(&MirFunction, Defs<'_>)> = module
            .functions
            .iter()
            .filter(|f| body(f))
            .map(|f| (f, defs(f)))
            .collect();
        loop {
            let mut changed = false;
            for (f, d) in &all {
                if is_list(&f.ret_ty) && !me.fresh.contains(&f.id) && me.returns_fresh(f, d, lists)
                {
                    me.fresh.insert(f.id);
                    changed = true;
                }
                if f.param_tys.first().is_some_and(is_list)
                    && quiet.contains(&f.id)
                    && !me.confined.contains(&f.id)
                    && me.touches_own(f, d, target)
                {
                    me.confined.insert(f.id);
                    changed = true;
                }
            }
            if !changed {
                return me;
            }
        }
    }

    pub fn confined(&self, f: FunctionId) -> bool {
        self.confined.contains(&f)
    }

    /// The locals of `f` with a scope: lists, each only ever set to a
    /// fresh call's result. Empty unless there are two.
    pub fn scoped(&self, f: &MirFunction, lists: &HashSet<u32>) -> Vec<u32> {
        let d = defs(f);
        let mut out: Vec<u32> = f
            .locals
            .iter()
            .filter(|l| matches!(&l.ty, MirTy::Struct(id, _) if lists.contains(&id.0)))
            .map(|l| l.id.0)
            .filter(|l| {
                !d.opaque.contains(l)
                    && d.by.get(l).is_some_and(|rs| {
                        rs.iter().all(
                        |r| matches!(r, RValue::Call { callee, .. } if self.fresh.contains(callee)),
                    )
                    })
            })
            .collect();
        if out.len() < 2 {
            out.clear();
        }
        out
    }

    fn returns_fresh(&self, f: &MirFunction, d: &Defs<'_>, lists: &HashSet<u32>) -> bool {
        let mut any = false;
        for b in &f.blocks {
            match &b.terminator {
                Terminator::Return(Some(op)) => {
                    any = true;
                    let fresh = copy(op)
                        .is_some_and(|l| self.fresh_local(d, l, lists, &mut HashSet::new()));
                    if !fresh {
                        return false;
                    }
                }
                Terminator::Return(None) | Terminator::Suspend { .. } => return false,
                _ => {}
            }
        }
        any
    }

    /// Every value local `l` takes is an object allocated in this call.
    fn fresh_local(
        &self,
        d: &Defs<'_>,
        l: u32,
        lists: &HashSet<u32>,
        seen: &mut HashSet<u32>,
    ) -> bool {
        if d.opaque.contains(&l) || !seen.insert(l) {
            return false;
        }
        let Some(rs) = d.by.get(&l) else {
            return false;
        };
        rs.iter().all(|r| match r {
            RValue::Call { callee, .. } => self.fresh.contains(callee),
            RValue::BufAlloc { .. } => true,
            RValue::Use(Operand::Copy(m)) => self.fresh_local(d, m.0, lists, seen),
            RValue::StructInit {
                ty: MirTy::Struct(id, _),
                fields,
            } if lists.contains(&id.0) => match fields.as_slice() {
                [Operand::Copy(h)] => self.fresh_local(d, h.0, lists, seen),
                _ => false,
            },
            _ => false,
        })
    }

    /// `f` reaches only its first argument's list object and buffer, and
    /// writes no GC reference there.
    fn touches_own(&self, f: &MirFunction, d: &Defs<'_>, target: &Target) -> bool {
        let mut part: HashMap<u32, Part> = HashMap::new();
        part.insert(f.params[0].0, Part::List);
        loop {
            let before = part.len();
            for (l, rs) in &d.by {
                if part.contains_key(l) || d.opaque.contains(l) {
                    continue;
                }
                let of = |op: &Operand| copy(op).and_then(|x| part.get(&x).copied());
                let mut kinds = rs.iter().map(|r| match r {
                    RValue::Use(op) => of(op),
                    RValue::FieldRead {
                        target, index: 0, ..
                    } if of(target) == Some(Part::List) => Some(Part::Object),
                    RValue::BufLoad { buf, .. } if of(buf) == Some(Part::Object) => {
                        Some(Part::Head)
                    }
                    RValue::FieldRead {
                        target,
                        field_ty: MirTy::GcRef,
                        ..
                    } if of(target) == Some(Part::Head) => Some(Part::Buf),
                    _ => None,
                });
                let first = kinds.next().flatten();
                if let Some(k) = first.filter(|k| kinds.all(|o| o == Some(*k))) {
                    part.insert(*l, k);
                }
            }
            if part.len() == before {
                break;
            }
        }
        let memory = |op: &Operand| {
            copy(op)
                .and_then(|x| part.get(&x))
                .is_some_and(|p| matches!(p, Part::Object | Part::Buf))
        };
        let list = |op: Option<&Operand>| {
            op.and_then(copy)
                .is_some_and(|x| part.get(&x) == Some(&Part::List))
        };
        f.blocks.iter().all(|b| {
            let skip = rokc_mir::noreturn_call(b);
            !matches!(b.terminator, Terminator::Suspend { .. })
                && b.stmts.iter().enumerate().all(|(i, s)| match s {
                    Statement::Assign { rvalue, .. } => match rvalue {
                        RValue::Use(_)
                        | RValue::BinOp { .. }
                        | RValue::UnaryOp { .. }
                        | RValue::Cast { .. }
                        | RValue::Bitcast { .. }
                        | RValue::FieldRead { .. }
                        | RValue::StructInit { .. }
                        | RValue::WithField { .. }
                        | RValue::SizeOf { .. }
                        | RValue::FnAddr { .. }
                        | RValue::ArrayInit { .. }
                        | RValue::ArrayIndex { .. } => true,
                        RValue::BufLoad { buf, .. } => memory(buf),
                        RValue::Call { callee, args, .. } => {
                            Some(i) == skip
                                || (self.confined.contains(callee) && list(args.first()))
                        }
                        _ => false,
                    },
                    Statement::BufStore { buf, elem_ty, .. } => {
                        memory(buf) && !target.has_gc_ref(elem_ty)
                    }
                    Statement::ArrayWrite { .. }
                    | Statement::Loc(..)
                    | Statement::StorageDead(_) => true,
                    _ => false,
                })
        })
    }
}
