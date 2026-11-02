//! Niche layout: a sum of one member `T` and `none` takes
//! `T`'s own bits when some scalar inside `T` has a bit pattern no `T`
//! value uses. `none` is all zero bits, so zeroed memory still reads as
//! `none`; the member is stored with that scalar XOR the pattern, which
//! is never zero. A zero pattern (null pointers, `NonZero`) leaves the
//! value untouched, so such sums cross the C ABI unchanged.
//!
//! Every niche sum goes through here: its MIR type (`ty::lower_ty`),
//! `SumWrap` (`niche_wrap`), the tag test (`niche_tag`) and the payload
//! binding (`niche_unwrap`). Widening into a larger sum re-tags member by
//! member through those, so the larger sum keeps the tagged layout.

use rokc_hir::{HirModule, HirTy, NicheArg, NicheVal};
use rokc_ir_common::{ConstVal, PrimOp, PrimTy};

use super::FnBuilder;
use crate::ty::lower_ty;
use crate::{MirTy, Operand, RValue, Statement, Target};

/// Where a niche sum keeps `none`.
#[derive(Clone, Debug, PartialEq)]
pub struct Niche {
    /// Field indices from the member value to the scalar.
    pub path: Vec<u32>,
    /// The scalar: a primitive or a C function pointer.
    pub leaf: MirTy,
    /// The pattern the scalar never holds: `none`'s bits before the XOR.
    pub bits: u64,
}

impl Niche {
    /// A `bool` / float scalar storing a non-zero pattern is XOR-ed as an
    /// unsigned int; it only sits behind single-field wrappers, and the
    /// whole sum is that int.
    fn uint_repr(&self, target: &Target) -> Option<PrimTy> {
        match self.leaf {
            MirTy::Prim(p) if self.bits != 0 && !p.is_int() => Some(uint_of(p, target)),
            _ => None,
        }
    }
}

fn uint_of(p: PrimTy, target: &Target) -> PrimTy {
    match p.bit_width(target.pointer_width) {
        8 => PrimTy::U8,
        16 => PrimTy::U16,
        32 => PrimTy::U32,
        _ => PrimTy::U64,
    }
}

/// `T | none` (either order, nothing else) where `T` has a niche: the
/// member and the niche.
pub fn sum_niche<'t>(
    sum_ty: &'t HirTy,
    module: &HirModule,
    target: &Target,
) -> Option<(&'t HirTy, Niche)> {
    let HirTy::Sum(parts) = sum_ty else {
        return None;
    };
    let m = match parts.as_slice() {
        [m, n] | [n, m] if n.is_none() && !m.is_none() => m,
        _ => return None,
    };
    Some((m, niche_of(m, module, target)?))
}

/// The MIR type of a niche sum.
pub fn sum_repr(member: &HirTy, niche: &Niche, module: &HirModule, target: &Target) -> MirTy {
    niche
        .uint_repr(target)
        .map_or_else(|| lower_ty(member, module, target), MirTy::Prim)
}

/// The automatic niches and `#[niche(..)]`, carried
/// up through struct and tuple fields.
fn niche_of(ty: &HirTy, module: &HirModule, target: &Target) -> Option<Niche> {
    let leaf = |leaf: MirTy, bits: u64| {
        Some(Niche {
            path: Vec::new(),
            leaf,
            bits,
        })
    };
    match ty {
        HirTy::Fn(..) if ty.is_c_fn() => leaf(lower_ty(ty, module, target), 0),
        HirTy::Prim(PrimTy::Bool) => leaf(MirTy::Prim(PrimTy::Bool), 2),
        // The first surrogate.
        HirTy::Prim(PrimTy::Char) => leaf(MirTy::Prim(PrimTy::Char), 0xD800),
        // The first unused discriminant. Not when the payload holds GC
        // references: the collector reads the tag to scan it.
        HirTy::Enum(id) => {
            let vs = &module.enums[id.0 as usize].variants;
            let n = vs.len();
            // One past the largest tag (value variants may skip some).
            let next = vs.iter().map(|v| v.tag).max().unwrap_or(-1) + 1;
            let layout = lower_ty(ty, module, target);
            if n == 0 || target.has_gc_ref(&layout) {
                return None;
            }
            let (path, leaf) = match layout {
                MirTy::Tuple(_) => (vec![0], MirTy::Prim(PrimTy::I32)),
                // A fieldless enum: its `#[repr]` tag, which may be full.
                l => (Vec::new(), l),
            };
            let max: u64 = match leaf {
                MirTy::Prim(PrimTy::U8) => 0xFF,
                MirTy::Prim(PrimTy::I8) => 0x7F,
                MirTy::Prim(PrimTy::U16) => 0xFFFF,
                MirTy::Prim(PrimTy::I16) => 0x7FFF,
                MirTy::Prim(PrimTy::I32) => 0x7FFF_FFFF,
                MirTy::Prim(PrimTy::U32) => 0xFFFF_FFFF,
                _ => u64::MAX,
            };
            let bits = u64::try_from(next).ok().filter(|b| *b <= max)?;
            Some(Niche { path, leaf, bits })
        }
        HirTy::Struct(id) => {
            let def = &module.structs[id.0 as usize];
            if def.union_kind.is_some() {
                return None;
            }
            let declared = match def.fields.as_slice() {
                [f] => match f.ty {
                    HirTy::Prim(p) if p != PrimTy::Str => {
                        pick(&def.niche, p, target).map(|bits| (p, bits))
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some((p, bits)) = declared {
                return Some(Niche {
                    path: vec![0],
                    leaf: MirTy::Prim(p),
                    bits,
                });
            }
            in_fields(def.fields.iter().map(|f| &f.ty), module, target)
        }
        HirTy::Tuple(parts) => in_fields(parts.iter(), module, target),
        _ => None,
    }
}

/// The first field niche an aggregate can carry: an int / pointer
/// scalar anywhere, an int-encoded one only through single fields.
fn in_fields<'t>(
    fields: impl ExactSizeIterator<Item = &'t HirTy>,
    module: &HirModule,
    target: &Target,
) -> Option<Niche> {
    let single = fields.len() == 1;
    fields.enumerate().find_map(|(i, t)| {
        let mut n = niche_of(t, module, target)?;
        if !single && n.uint_repr(target).is_some() {
            return None;
        }
        n.path.insert(0, u32::try_from(i).ok()?);
        Some(n)
    })
}

/// One pattern of `#[niche(..)]` for a field of type `p`: zero when the
/// set holds it (no XOR, C-compatible), else the first one.
fn pick(args: &[NicheArg], p: PrimTy, target: &Target) -> Option<u64> {
    let w = p.bit_width(target.pointer_width);
    let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
    let (min, max) = if p.is_signed_int() {
        (-(1i128 << (w - 1)), (1i128 << (w - 1)) - 1)
    } else {
        (0, (1i128 << w) - 1)
    };
    let float = |v: f64| {
        Some(if p == PrimTy::F32 {
            u64::from((v as f32).to_bits())
        } else {
            v.to_bits()
        })
    };
    let int = |v: NicheVal| match v {
        NicheVal::Int(i) => Some(i),
        NicheVal::Min => Some(min),
        NicheVal::Max => Some(max),
        _ => None,
    };
    let val = |v: NicheVal| -> Option<u64> {
        if !p.is_float() {
            return int(v).map(|i| (i as u64) & mask);
        }
        match v {
            NicheVal::Int(i) => float(i as f64),
            NicheVal::Nan if p == PrimTy::F32 => Some(0x7FC0_0000),
            NicheVal::Nan => Some(0x7FF8_0000_0000_0000),
            NicheVal::Inf => float(f64::INFINITY),
            NicheVal::NegInf => float(f64::NEG_INFINITY),
            NicheVal::Min | NicheVal::Max => None,
        }
    };
    let mut first = None;
    for a in args {
        let bits = match *a {
            NicheArg::Val(v) => val(v),
            NicheArg::Range(lo, hi) if !p.is_float() => match (int(lo), int(hi)) {
                (Some(lo), Some(hi)) if lo <= 0 && 0 <= hi => Some(0),
                (Some(lo), Some(hi)) if lo <= hi => Some((lo as u64) & mask),
                _ => None,
            },
            NicheArg::Range(..) => None,
        };
        if bits == Some(0) {
            return bits;
        }
        first = first.or(bits);
    }
    first
}

fn field_tys(ty: &MirTy) -> &[MirTy] {
    match ty {
        MirTy::Struct(_, fs) | MirTy::Tuple(fs) => fs,
        _ => &[],
    }
}

impl FnBuilder<'_> {
    /// `sum_ty`'s niche and its member's MIR type.
    pub(super) fn niche(&self, sum_ty: &HirTy) -> Option<(MirTy, Niche)> {
        let (m, n) = sum_niche(sum_ty, self.module, self.target)?;
        Some((self.ty(m), n))
    }

    /// The member value `v` as the sum's bits.
    pub(super) fn niche_wrap(&mut self, v: Operand, member: &MirTy, n: &Niche) -> Operand {
        if n.bits == 0 {
            return v;
        }
        let Some(u) = n.uint_repr(self.target) else {
            return self.niche_flip(v, member, &n.path, n);
        };
        let leaf = self.niche_leaf(v, member, &n.path);
        let x = self.payload_cast(leaf, &n.leaf, &MirTy::Prim(u));
        self.niche_xor(x, u, n.bits)
    }

    /// The sum's bits `s` (not `none`) as the member value.
    pub(super) fn niche_unwrap(&mut self, s: Operand, member: &MirTy, n: &Niche) -> Operand {
        if n.bits == 0 {
            return s;
        }
        let Some(u) = n.uint_repr(self.target) else {
            return self.niche_flip(s, member, &n.path, n);
        };
        let x = self.niche_xor(s, u, n.bits);
        let leaf = self.payload_cast(x, &MirTy::Prim(u), &n.leaf);
        self.niche_rebuild(member, &n.path, leaf)
    }

    /// The stored tag (`ty::sum_tag`): 1 for the member, 0 for `none`.
    pub(super) fn niche_tag(&mut self, s: Operand, sum: &MirTy, n: &Niche) -> Operand {
        let bool_ = MirTy::Prim(PrimTy::Bool);
        let set = match (n.uint_repr(self.target), &n.leaf) {
            (Some(u), _) => self.niche_nonzero(s, u),
            (None, MirTy::Prim(p)) => {
                let leaf = self.niche_leaf(s, sum, &n.path);
                let u = if p.is_int() {
                    *p
                } else {
                    uint_of(*p, self.target)
                };
                let bits = self.payload_cast(leaf, &n.leaf, &MirTy::Prim(u));
                self.niche_nonzero(bits, u)
            }
            (None, fn_ptr) => {
                let fn_ptr = fn_ptr.clone();
                let leaf = self.niche_leaf(s, sum, &n.path);
                self.cast_to(leaf, &fn_ptr, &bool_)
            }
        };
        self.cast_to(set, &bool_, &MirTy::Prim(PrimTy::I32))
    }

    fn niche_leaf(&mut self, v: Operand, ty: &MirTy, path: &[u32]) -> Operand {
        let Some((&i, rest)) = path.split_first() else {
            return v;
        };
        let field_ty = field_tys(ty)[i as usize].clone();
        let f = self.niche_field(v, i, &field_ty);
        self.niche_leaf(f, &field_ty, rest)
    }

    /// `v` with the scalar at `path` XOR the pattern (an int scalar).
    fn niche_flip(&mut self, value: Operand, ty: &MirTy, path: &[u32], niche: &Niche) -> Operand {
        let Some((&index, rest)) = path.split_first() else {
            let MirTy::Prim(p) = niche.leaf else {
                return value;
            };
            return self.niche_xor(value, p, niche.bits);
        };
        let field_ty = field_tys(ty)[index as usize].clone();
        let field = self.niche_field(value.clone(), index, &field_ty);
        let field = self.niche_flip(field, &field_ty, rest, niche);
        self.niche_assign(
            ty.clone(),
            RValue::WithField {
                source: value,
                struct_ty: ty.clone(),
                index,
                value: field,
            },
        )
    }

    /// The single-field wrappers along `path` around `leaf`.
    fn niche_rebuild(&mut self, ty: &MirTy, path: &[u32], leaf: Operand) -> Operand {
        let Some((_, rest)) = path.split_first() else {
            return leaf;
        };
        let field_ty = field_tys(ty)[0].clone();
        let inner = self.niche_rebuild(&field_ty, rest, leaf);
        self.niche_assign(
            ty.clone(),
            RValue::StructInit {
                ty: ty.clone(),
                fields: vec![inner],
            },
        )
    }

    fn niche_field(&mut self, v: Operand, index: u32, field_ty: &MirTy) -> Operand {
        self.niche_assign(
            field_ty.clone(),
            RValue::FieldRead {
                target: v,
                index,
                field_ty: field_ty.clone(),
            },
        )
    }

    fn niche_xor(&mut self, v: Operand, p: PrimTy, bits: u64) -> Operand {
        let rhs = Operand::Const(self.niche_const(p, bits));
        self.niche_assign(
            MirTy::Prim(p),
            RValue::BinOp {
                op: PrimOp::BitXor,
                op_ty: MirTy::Prim(p),
                lhs: v,
                rhs,
            },
        )
    }

    fn niche_nonzero(&mut self, v: Operand, p: PrimTy) -> Operand {
        let rhs = Operand::Const(self.niche_const(p, 0));
        self.niche_assign(
            MirTy::Prim(PrimTy::Bool),
            RValue::BinOp {
                op: PrimOp::Ne,
                op_ty: MirTy::Prim(p),
                lhs: v,
                rhs,
            },
        )
    }

    /// `bits` as an int constant of `p`, sign-extended for signed `p`.
    fn niche_const(&self, p: PrimTy, bits: u64) -> ConstVal {
        let w = p.bit_width(self.target.pointer_width);
        let mut value = i128::from(bits);
        if p.is_signed_int() && w < 128 && value >= 1i128 << (w - 1) {
            value -= 1i128 << w;
        }
        ConstVal::Int { value, ty: p }
    }

    fn niche_assign(&mut self, ty: MirTy, rvalue: RValue) -> Operand {
        let dst = self.fresh_local(ty);
        self.push_stmt(Statement::Assign {
            target: dst,
            rvalue,
        });
        Operand::Copy(dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x64() -> Target {
        Target::builtin("x86_64-unknown-linux-gnu")
    }

    #[test]
    fn picks_zero_else_the_first_pattern() {
        let t = x64();
        let v = |v| NicheArg::Val(v);
        assert_eq!(pick(&[v(NicheVal::Int(0))], PrimTy::I32, &t), Some(0));
        assert_eq!(
            pick(
                &[v(NicheVal::Int(-1)), v(NicheVal::Int(0))],
                PrimTy::I16,
                &t
            ),
            Some(0)
        );
        assert_eq!(pick(&[v(NicheVal::Int(-1))], PrimTy::I16, &t), Some(0xFFFF));
        assert_eq!(
            pick(
                &[NicheArg::Range(NicheVal::Min, NicheVal::Int(-1))],
                PrimTy::I32,
                &t
            ),
            Some(0x8000_0000)
        );
        assert_eq!(
            pick(
                &[NicheArg::Range(NicheVal::Int(-5), NicheVal::Int(5))],
                PrimTy::I8,
                &t
            ),
            Some(0)
        );
        assert_eq!(
            pick(&[v(NicheVal::Nan), v(NicheVal::Inf)], PrimTy::F64, &t),
            Some(0x7FF8_0000_0000_0000)
        );
        assert_eq!(
            pick(&[v(NicheVal::Nan)], PrimTy::F32, &t),
            Some(0x7FC0_0000)
        );
        assert_eq!(pick(&[v(NicheVal::Nan)], PrimTy::I32, &t), None);
        assert_eq!(pick(&[v(NicheVal::Int(0))], PrimTy::F64, &t), Some(0));
    }

    #[test]
    fn auto_niches() {
        let (t, m) = (x64(), HirModule::default());
        let none = HirTy::None;
        let sum = |a: HirTy| HirTy::Sum(vec![a, none.clone()]);
        let of = |p: PrimTy| {
            let (_, niche) = sum_niche(&sum(HirTy::Prim(p)), &m, &t).unwrap();
            (niche.bits, niche.uint_repr(&t))
        };
        assert_eq!(of(PrimTy::Bool), (2, Some(PrimTy::U8)));
        assert_eq!(of(PrimTy::Char), (0xD800, None));
        assert!(sum_niche(&sum(HirTy::Prim(PrimTy::I32)), &m, &t).is_none());
        // A bool in a pair needs the int encoding: no niche; a char is fine.
        let pair = sum(HirTy::Tuple(vec![
            HirTy::Prim(PrimTy::I64),
            HirTy::Prim(PrimTy::Bool),
        ]));
        assert!(sum_niche(&pair, &m, &t).is_none());
        let pair = sum(HirTy::Tuple(vec![
            HirTy::Prim(PrimTy::I64),
            HirTy::Prim(PrimTy::Char),
        ]));
        assert_eq!(sum_niche(&pair, &m, &t).unwrap().1.path, vec![1]);
        // Only `T | none`.
        let wide = HirTy::Sum(vec![
            HirTy::Prim(PrimTy::Bool),
            HirTy::Prim(PrimTy::I32),
            none,
        ]);
        assert!(sum_niche(&wide, &m, &t).is_none());
    }
}
