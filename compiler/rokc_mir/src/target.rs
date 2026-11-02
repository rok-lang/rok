//! The compilation target and the data layout derived from it. Built
//! once by the driver; `#[cfg]`, MIR lowering and codegen all read it.

use rokc_ir_common::PrimTy;

use crate::gc_abi::{AMBIGUOUS, ANY_SLOT, INTERIOR_REF, STRONG_REF};
use crate::{MirTy, UnionKind};

/// What the collector must see of a value, offsets in
/// bytes from the same base.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GcSlot {
    /// `(offset, kind)`: a `STRONG_REF`, `INTERIOR_REF`, `ANY_SLOT` or
    /// `AMBIGUOUS` word.
    Word(u32, u32),
    /// A sum (`SUM_PAYLOAD`) whose `u32` tag is at the offset: the
    /// slots of each data variant that has any, by stored tag.
    Sum(u32, Vec<(u32, Vec<GcSlot>)>),
}

pub use rokc_target::{CAbi, StackProbes, TargetSpec};

/// The target (its spec) and what layout needs of the program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub spec: std::sync::Arc<TargetSpec>,
    /// `#[repr(..)]` of the structs that have one, by `StructId` (MIR
    /// lowering fills it from the HIR module).
    pub reprs: std::sync::Arc<std::collections::BTreeMap<u32, rokc_hir::Repr>>,
}

impl Default for Target {
    fn default() -> Self {
        Self::new(TargetSpec::host().clone())
    }
}

impl std::ops::Deref for Target {
    type Target = TargetSpec;
    fn deref(&self) -> &TargetSpec {
        &self.spec
    }
}

impl Target {
    #[must_use]
    pub fn new(spec: TargetSpec) -> Self {
        Self {
            spec: std::sync::Arc::new(spec),
            reprs: std::sync::Arc::default(),
        }
    }

    /// Built-in target `name` (tests).
    #[must_use]
    pub fn builtin(name: &str) -> Self {
        Self::new(TargetSpec::builtin(name).unwrap_or_else(|| panic!("no target {name}")))
    }

    /// `#[repr(..)]` of struct `id` (the default without one).
    #[must_use]
    pub fn repr(&self, id: rokc_hir::StructId) -> rokc_hir::Repr {
        self.reprs.get(&id.0).copied().unwrap_or_default()
    }

    /// Byte offset of each field of a struct / tuple `ps`, then its
    /// size and alignment: C's layout, or `#[repr(packed)]` (no padding,
    /// alignment 1) / `#[repr(align(N))]` (at least `N`) of struct `id`.
    #[must_use]
    pub fn fields_layout(
        &self,
        id: Option<rokc_hir::StructId>,
        ps: &[MirTy],
    ) -> (Vec<u32>, u32, u32) {
        let repr = id.map(|id| self.repr(id)).unwrap_or_default();
        let mut offsets = Vec::with_capacity(ps.len());
        let (mut size, mut align) = (0u32, 1);
        for t in ps {
            let (s, a) = self.size_align(t);
            let a = if repr.packed { 1 } else { a };
            size = size.next_multiple_of(a);
            offsets.push(size);
            size += s;
            align = align.max(a);
        }
        let align = align.max(repr.align);
        (offsets, size.next_multiple_of(align), align)
    }

    /// `fields_layout` of a struct / tuple type; empty for others.
    #[must_use]
    pub fn field_offsets(&self, ty: &MirTy) -> Vec<u32> {
        match ty {
            MirTy::Tuple(ps) => self.fields_layout(None, ps).0,
            MirTy::Struct(id, ps) => self.fields_layout(Some(*id), ps).0,
            _ => Vec::new(),
        }
    }

    /// `ty`'s layout is not the one LLVM gives its natural type (a
    /// `packed` / `align(N)` struct, or an aggregate holding one): it is
    /// emitted as a packed LLVM struct with explicit padding.
    #[must_use]
    pub fn explicit_layout(&self, ty: &MirTy) -> bool {
        match ty {
            MirTy::Struct(id, ps) => {
                let r = self.repr(*id);
                r.packed || r.align != 0 || ps.iter().any(|p| self.explicit_layout(p))
            }
            MirTy::Tuple(ps) => ps.iter().any(|p| self.explicit_layout(p)),
            MirTy::Array(e, _) => self.explicit_layout(e),
            MirTy::Union(..) => self.size_align(ty).1 > self.pointer_width / 8,
            _ => false,
        }
    }

    /// `(size, align)` in bytes of `ty` under the C struct layout LLVM
    /// uses on this target.
    #[must_use]
    pub fn size_align(&self, ty: &MirTy) -> (u32, u32) {
        let word = (self.pointer_width / 8, self.pointer_width / 8);
        match ty {
            MirTy::Unit => (0, 1),
            MirTy::Prim(p) => match p {
                PrimTy::I8 | PrimTy::U8 | PrimTy::CChar | PrimTy::Bool => (1, 1),
                PrimTy::I16 | PrimTy::U16 => (2, 2),
                PrimTy::I32 | PrimTy::U32 | PrimTy::Char | PrimTy::F32 => (4, 4),
                PrimTy::I64 | PrimTy::U64 | PrimTy::F64 => (8, self.wide_align),
                PrimTy::Isize | PrimTy::Usize => word,
                PrimTy::Str => (2 * word.0, word.1),
            },
            MirTy::Any | MirTy::GcRef | MirTy::Fn(_, _) => word,
            MirTy::Union(vs, n, _) => (
                word.0 * n,
                vs.iter()
                    .map(|(_, t)| self.size_align(t).1)
                    .fold(word.1, u32::max),
            ),
            MirTy::Tuple(ps) => {
                let (_, size, align) = self.fields_layout(None, ps);
                (size, align)
            }
            MirTy::Struct(id, ps) => {
                let (_, size, align) = self.fields_layout(Some(*id), ps);
                (size, align)
            }
            MirTy::Array(t, n) => {
                let (s, a) = self.size_align(t);
                (s * n, a)
            }
        }
    }

    /// The words of a `ty` at `at` that may hold a GC reference. The one
    /// walk behind scan tables, write barriers and stack / static roots.
    pub fn gc_offsets(&self, ty: &MirTy, at: u32, out: &mut Vec<GcSlot>) {
        match ty {
            MirTy::GcRef => out.push(GcSlot::Word(at, STRONG_REF)),
            MirTy::Prim(PrimTy::Str) => out.push(GcSlot::Word(at, INTERIOR_REF)),
            MirTy::Struct(crate::ANY_STRUCT, _) => out.push(GcSlot::Word(at, ANY_SLOT)),
            // A sum (`sum_layout`): `(i32 tag, union)`.
            MirTy::Tuple(ps)
                if matches!(
                    ps.as_slice(),
                    [MirTy::Prim(PrimTy::I32), MirTy::Union(_, _, UnionKind::Sum)]
                ) =>
            {
                let MirTy::Union(vs, _, _) = &ps[1] else {
                    unreachable!()
                };
                let payload = at
                    + self
                        .size_align(&ps[0])
                        .0
                        .next_multiple_of(self.size_align(&ps[1]).1);
                let variants = vs
                    .iter()
                    .filter_map(|(tag, t)| {
                        let mut slots = Vec::new();
                        self.gc_offsets(t, payload, &mut slots);
                        (!slots.is_empty()).then_some((*tag, slots))
                    })
                    .collect::<Vec<_>>();
                if !variants.is_empty() {
                    out.push(GcSlot::Sum(at, variants));
                }
            }
            // A union away from its tag (a payload temporary): any word
            // a variant keeps a reference in, kept alive and never moved.
            MirTy::Union(vs, _, _) => {
                let mut slots = Vec::new();
                for (_, t) in vs {
                    self.gc_offsets(t, at, &mut slots);
                }
                out.extend(
                    self.flat_words(&slots)
                        .into_iter()
                        .map(|off| GcSlot::Word(off, AMBIGUOUS)),
                );
            }
            MirTy::Tuple(ps) | MirTy::Struct(_, ps) => {
                for (t, off) in ps.iter().zip(self.field_offsets(ty)) {
                    self.gc_offsets(t, at + off, out);
                }
            }
            MirTy::Array(t, n) => {
                let s = self.size_align(t).0;
                for i in 0..*n {
                    self.gc_offsets(t, at + i * s, out);
                }
            }
            _ => {}
        }
    }

    /// Sorted offsets of every word `slots` may hold a reference in (any
    /// variant of a sum; an `any`'s payload word, not its tag).
    #[must_use]
    pub fn flat_words(&self, slots: &[GcSlot]) -> Vec<u32> {
        fn walk(slots: &[GcSlot], w: u32, out: &mut Vec<u32>) {
            for s in slots {
                match s {
                    GcSlot::Word(off, ANY_SLOT) => out.push(off + w),
                    GcSlot::Word(off, _) => out.push(*off),
                    GcSlot::Sum(_, vs) => vs.iter().for_each(|(_, v)| walk(v, w, out)),
                }
            }
        }
        let mut out = Vec::new();
        walk(slots, self.pointer_width / 8, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Byte offsets of the words of a `ty` a root slot must cover: every
    /// word any variant of it may hold a reference in.
    #[must_use]
    pub fn root_offsets(&self, ty: &MirTy) -> Vec<u32> {
        let mut slots = Vec::new();
        self.gc_offsets(ty, 0, &mut slots);
        self.flat_words(&slots)
    }

    /// `ty` has a word the collector must see.
    #[must_use]
    pub fn has_gc_ref(&self, ty: &MirTy) -> bool {
        let mut slots = Vec::new();
        self.gc_offsets(ty, 0, &mut slots);
        !slots.is_empty()
    }

    /// An `any` payload of type `ty` is the payload word
    /// itself: a number / bool / char no wider than a pointer.
    #[must_use]
    pub fn any_inline(&self, ty: &MirTy) -> bool {
        matches!(ty, MirTy::Prim(p)
            if *p != PrimTy::Str && p.bit_width(self.pointer_width) <= self.pointer_width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn p(t: PrimTy) -> MirTy {
        MirTy::Prim(t)
    }

    /// `struct { int; long long; double; }` as clang lays it out:
    /// 20 / 4 on i686 Linux, 24 / 8 on i686 Windows, armv7 and wasm32.
    #[test]
    fn wide_alignment_follows_the_target() {
        let t = MirTy::Tuple(vec![p(PrimTy::I32), p(PrimTy::I64), p(PrimTy::F64)]);
        let at = |triple| Target::builtin(triple).size_align(&t);
        assert_eq!(at("i686-unknown-linux-gnu"), (20, 4));
        assert_eq!(at("i686-pc-windows-msvc"), (24, 8));
        assert_eq!(at("armv7-unknown-linux-gnueabihf"), (24, 8));
        assert_eq!(at("wasm32-unknown-unknown"), (24, 8));
    }

    #[test]
    fn c_layout_sizes() {
        let t = Target::builtin("x86_64-unknown-linux-gnu");
        assert_eq!(t.size_align(&MirTy::Unit), (0, 1));
        assert_eq!(t.size_align(&p(PrimTy::Bool)), (1, 1));
        assert_eq!(t.size_align(&p(PrimTy::Str)), (16, 8));
        // (i8, i64, i16): padding after i8 and at the tail.
        let tup = MirTy::Tuple(vec![p(PrimTy::I8), p(PrimTy::I64), p(PrimTy::I16)]);
        assert_eq!(t.size_align(&tup), (24, 8));
        assert_eq!(
            t.size_align(&MirTy::Array(Box::new(p(PrimTy::U16)), 3)),
            (6, 2)
        );
        let w32 = Target::builtin("wasm32-unknown-unknown");
        assert_eq!(w32.size_align(&p(PrimTy::Str)), (8, 4));
    }

    #[test]
    fn any_inline_follows_the_pointer_width() {
        let i64_ = p(PrimTy::I64);
        assert!(Target::builtin("x86_64-unknown-linux-gnu").any_inline(&i64_));
        assert!(!Target::builtin("i686-unknown-linux-gnu").any_inline(&i64_));
        assert!(Target::builtin("i686-unknown-linux-gnu").any_inline(&p(PrimTy::I32)));
    }

    #[test]
    fn roots_cover_the_any_payload_word() {
        let t = Target::builtin("x86_64-unknown-linux-gnu");
        let ty = MirTy::Tuple(vec![p(PrimTy::Str), crate::any_ty()]);
        let mut words = Vec::new();
        t.gc_offsets(&ty, 0, &mut words);
        assert_eq!(
            words,
            [GcSlot::Word(0, INTERIOR_REF), GcSlot::Word(16, ANY_SLOT)]
        );
        assert_eq!(t.root_offsets(&ty), [0, 24]);
    }

    /// `S(str) | P(i64, GcRef) | N(i64)` at offset 8: one `SUM_PAYLOAD`
    /// with the reference words of each variant; roots cover their
    /// union; a bare union is ambiguous words.
    #[test]
    fn sums_scan_by_variant() {
        let t = Target::builtin("x86_64-unknown-linux-gnu");
        let pair = MirTy::Tuple(vec![p(PrimTy::I64), MirTy::GcRef]);
        let vs = vec![(0, p(PrimTy::Str)), (1, pair), (2, p(PrimTy::I64))];
        let union = MirTy::Union(vs, 2, UnionKind::Sum);
        let sum = MirTy::Tuple(vec![p(PrimTy::I32), union.clone()]);
        assert_eq!(t.size_align(&sum), (24, 8));
        let mut slots = Vec::new();
        t.gc_offsets(
            &MirTy::Tuple(vec![p(PrimTy::I64), sum.clone()]),
            0,
            &mut slots,
        );
        assert_eq!(
            slots,
            [GcSlot::Sum(
                8,
                vec![
                    (0, vec![GcSlot::Word(16, INTERIOR_REF)]),
                    (1, vec![GcSlot::Word(24, STRONG_REF)])
                ]
            )]
        );
        assert_eq!(t.root_offsets(&sum), [8, 16]);
        let mut slots = Vec::new();
        t.gc_offsets(&union, 0, &mut slots);
        assert_eq!(
            slots,
            [GcSlot::Word(0, AMBIGUOUS), GcSlot::Word(8, AMBIGUOUS)]
        );
    }

    /// An `(i32, union)` that is no sum (a coroutine frame with one
    /// pinned slot): the `i32` is no tag, every word is scanned.
    #[test]
    fn only_a_sum_payload_makes_a_sum() {
        let t = Target::builtin("x86_64-unknown-linux-gnu");
        let slot = MirTy::Union(vec![(0, MirTy::GcRef)], 1, UnionKind::Plain);
        let frame = MirTy::Tuple(vec![p(PrimTy::I32), slot]);
        let mut slots = Vec::new();
        t.gc_offsets(&frame, 0, &mut slots);
        assert_eq!(slots, [GcSlot::Word(8, AMBIGUOUS)]);
    }
}
