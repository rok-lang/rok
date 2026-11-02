//! Partial element stores. `xs[i].f = v` on a `GcBuf` of structs reads
//! the element, replaces the field in the copy and stores the copy back
//! (the element is a value). When nothing writes memory
//! between that load and store, storing only field `f` is the same, and
//! skips the write barrier of the fields that hold GC references.

use std::collections::HashMap;

use rokc_ir_common::{ConstVal, LocalId};

use crate::{BasicBlock, Operand, RValue, Statement};

/// A `BufStore` that may write only field `field` of the element, as
/// `value`.
#[derive(Clone, Debug)]
pub struct PartStore {
    pub field: u32,
    pub value: Operand,
}

/// A value by how it was computed: the buffer of `self.h[0]` is read
/// anew for the load and for the store.
#[derive(Clone, PartialEq)]
enum Val {
    Local(LocalId),
    Const(ConstVal),
    Field(Box<Self>, u32),
}

impl Val {
    fn mentions(&self, l: LocalId) -> bool {
        match self {
            Self::Local(x) => *x == l,
            Self::Const(_) => false,
            Self::Field(v, _) => v.mentions(l),
        }
    }
}

/// What a local holds: element `index` of `buf` as loaded, or that with
/// field `field` replaced by `value`.
#[derive(Clone)]
enum Known {
    Loaded {
        buf: Val,
        index: Val,
    },
    Updated {
        buf: Val,
        index: Val,
        field: u32,
        value: Operand,
    },
}

impl Known {
    fn mentions(&self, l: LocalId) -> bool {
        match self {
            Self::Loaded { buf, index } => buf.mentions(l) || index.mentions(l),
            Self::Updated {
                buf, index, value, ..
            } => {
                buf.mentions(l) || index.mentions(l) || matches!(value, Operand::Copy(x) if *x == l)
            }
        }
    }
}

/// The `BufStore`s of `b` (by statement index) that are partial stores.
#[must_use]
pub fn part_stores(b: &BasicBlock) -> HashMap<usize, PartStore> {
    let mut vals: HashMap<LocalId, Val> = HashMap::new();
    let mut known: HashMap<LocalId, Known> = HashMap::new();
    let val = |vals: &HashMap<LocalId, Val>, o: &Operand| match o {
        Operand::Copy(x) => vals.get(x).cloned().unwrap_or(Val::Local(*x)),
        Operand::Const(c) => Val::Const(c.clone()),
    };
    let mut out = HashMap::new();
    for (at, s) in b.stmts.iter().enumerate() {
        match s {
            Statement::Assign { target, rvalue } => {
                let v = match rvalue {
                    RValue::Use(o) => Some(val(&vals, o)),
                    RValue::FieldRead { target, index, .. } => {
                        Some(Val::Field(Box::new(val(&vals, target)), *index))
                    }
                    _ => None,
                };
                let k = match rvalue {
                    RValue::BufLoad { buf, index, .. } => Some(Known::Loaded {
                        buf: val(&vals, buf),
                        index: val(&vals, index),
                    }),
                    RValue::Use(Operand::Copy(x)) => known.get(x).cloned(),
                    RValue::WithField {
                        source: Operand::Copy(x),
                        index: field,
                        value,
                        ..
                    } => match known.get(x) {
                        Some(Known::Loaded { buf, index }) => Some(Known::Updated {
                            buf: buf.clone(),
                            index: index.clone(),
                            field: *field,
                            value: value.clone(),
                        }),
                        _ => None,
                    },
                    // Pure: no memory written.
                    RValue::BinOp { .. }
                    | RValue::UnaryOp { .. }
                    | RValue::Use(_)
                    | RValue::StructInit { .. }
                    | RValue::FieldRead { .. }
                    | RValue::Cast { .. }
                    | RValue::Bitcast { .. }
                    | RValue::WithField { .. }
                    | RValue::FnAddr { .. }
                    | RValue::SizeOf { .. }
                    | RValue::StaticLoad { .. }
                    | RValue::RawLoad { .. }
                    | RValue::HeapLoad { .. }
                    | RValue::HeapField { .. }
                    | RValue::ArrayIndex { .. } => None,
                    _ => {
                        known.clear();
                        None
                    }
                };
                vals.retain(|_, v| !v.mentions(*target));
                known.retain(|_, v| !v.mentions(*target));
                if let Some(v) = v.filter(|v| !v.mentions(*target)) {
                    vals.insert(*target, v);
                }
                if let Some(k) = k.filter(|k| !k.mentions(*target)) {
                    known.insert(*target, k);
                }
            }
            Statement::BufStore {
                buf,
                index,
                value: Operand::Copy(v),
                ..
            } => {
                if let Some(Known::Updated {
                    buf: b,
                    index: i,
                    field,
                    value,
                }) = known.get(v)
                {
                    if *b == val(&vals, buf) && *i == val(&vals, index) {
                        out.insert(
                            at,
                            PartStore {
                                field: *field,
                                value: value.clone(),
                            },
                        );
                    }
                }
                known.clear();
            }
            Statement::Loc(..) | Statement::StorageDead(_) => {}
            _ => known.clear(),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use rokc_ir_common::{BlockId, ConstVal, PrimTy};

    use super::*;
    use crate::{MirTy, Terminator};

    const fn block(stmts: Vec<Statement>) -> BasicBlock {
        BasicBlock {
            id: BlockId(0),
            params: vec![],
            stmts,
            terminator: Terminator::Return(None),
        }
    }

    /// `_0 = _9.0; _1 = _0[0]; _2 = _1; _3 = _2 with .0 = _4; (between);
    /// _6 = _9.0; _6[0] = _3`: `self.h[0].n = v`.
    fn update(between: Vec<Statement>) -> BasicBlock {
        let elem = MirTy::Struct(
            rokc_hir::StructId(0),
            vec![MirTy::Prim(PrimTy::Usize), MirTy::GcRef],
        );
        let zero = Operand::Const(ConstVal::UInt {
            value: 0,
            ty: PrimTy::Usize,
        });
        let assign = |t: u32, rvalue| Statement::Assign {
            target: LocalId(t),
            rvalue,
        };
        let buf = |t: u32| {
            assign(
                t,
                RValue::FieldRead {
                    target: Operand::Copy(LocalId(9)),
                    index: 0,
                    field_ty: MirTy::GcRef,
                },
            )
        };
        let mut stmts = vec![
            buf(0),
            assign(
                1,
                RValue::BufLoad {
                    buf: Operand::Copy(LocalId(0)),
                    index: zero.clone(),
                    elem_ty: elem.clone(),
                },
            ),
            assign(2, RValue::Use(Operand::Copy(LocalId(1)))),
            assign(
                3,
                RValue::WithField {
                    source: Operand::Copy(LocalId(2)),
                    struct_ty: elem.clone(),
                    index: 0,
                    value: Operand::Copy(LocalId(4)),
                },
            ),
        ];
        stmts.extend(between);
        stmts.push(buf(6));
        stmts.push(Statement::BufStore {
            buf: Operand::Copy(LocalId(6)),
            index: zero,
            value: Operand::Copy(LocalId(3)),
            elem_ty: elem,
        });
        block(stmts)
    }

    #[test]
    fn field_update_is_a_part_store() {
        let parts = part_stores(&update(vec![]));
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[&5].field, 0);
    }

    #[test]
    fn a_write_or_a_changed_operand_in_between_keeps_the_whole_store() {
        let call = Statement::Assign {
            target: LocalId(5),
            rvalue: RValue::Call {
                callee: rokc_ir_common::FunctionId(0),
                args: vec![],
                ret_ty: MirTy::Unit,
            },
        };
        assert!(part_stores(&update(vec![call])).is_empty());
        let rebind = Statement::Assign {
            target: LocalId(4),
            rvalue: RValue::Use(Operand::Const(ConstVal::Unit)),
        };
        assert!(part_stores(&update(vec![rebind])).is_empty());
    }
}
