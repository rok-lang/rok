//! HIR → MIR type lowering.

use rokc_hir::{HirModule, HirTy};
use rokc_ir_common::PrimTy;

use crate::{DebugTy, MirTy, Target, UnionKind};

/// Struct id marking the MIR type of an `any Tr` value, so the
/// collector's `ScanTable` gets an `ANY_SLOT` (802 §8.2.7) for it.
pub const ANY_STRUCT: rokc_hir::StructId = rokc_hir::StructId(u32::MAX);

/// `any Tr`: `(i32 tag, payload word)`. The payload is the value itself
/// when [`any_inline`], else a GC reference to a boxed copy.
#[must_use]
pub fn any_ty() -> MirTy {
    MirTy::Struct(ANY_STRUCT, vec![MirTy::Prim(PrimTy::I32), MirTy::GcRef])
}

/// `{code: fn(GcRef env, args..) -> ret, env: GcRef}`.
#[must_use]
pub fn fat_fn(args: Vec<MirTy>, ret: MirTy) -> MirTy {
    MirTy::Tuple(vec![code_ty(args, ret), MirTy::GcRef])
}

/// The code pointer of a function value: the env comes first.
#[must_use]
pub fn code_ty(args: Vec<MirTy>, ret: MirTy) -> MirTy {
    let mut all = vec![MirTy::GcRef];
    all.extend(args);
    MirTy::Fn(all, Box::new(ret))
}

/// Return value of a throwing function (spec 8.6.5): `(is_err, payload)`,
/// the payload one slot for `ok` (tag 0) and `err` (tag 1), or `err`
/// alone when `ok` is unit. HIR still names `(is_err, ok, err)`; MIR
/// lowering maps those fields onto the slot (`carrier_field`). A
/// payload that holds a GC reference is a [`MirTy::Union`] behind an
/// `i32` tag, so the collector scans it as a sum; the payload is word
/// aligned there, so the layout is the spec's `i1` one.
fn carrier(ok: MirTy, err: MirTy, target: &Target) -> MirTy {
    if ok == MirTy::Unit {
        return MirTy::Tuple(vec![MirTy::Prim(PrimTy::Bool), err]);
    }
    let payload = unify_payload(&[(0, ok), (1, err)], target).expect("two variants");
    let tag = if matches!(payload, MirTy::Union(..)) {
        PrimTy::I32
    } else {
        PrimTy::Bool
    };
    MirTy::Tuple(vec![MirTy::Prim(tag), payload])
}

/// The MIR type of `ty` on `target`.
#[must_use]
pub fn lower_ty(ty: &HirTy, module: &HirModule, target: &Target) -> MirTy {
    let lower = |t: &HirTy| lower_ty(t, module, target);
    match ty {
        HirTy::Unknown
        | HirTy::Path(_)
        | HirTy::Any
        | HirTy::None
        | HirTy::Const(..)
        | HirTy::OpenConst(_)
        | HirTy::NoThrow
        | HirTy::Var(_)
        | HirTy::Param(_) => MirTy::Any,
        // `!` has no values; a diverging call returns nothing.
        HirTy::Unit | HirTy::Never => MirTy::Unit,
        HirTy::Prim(p) => MirTy::Prim(*p),
        HirTy::AnyTrait(_) => any_ty(),
        HirTy::Struct(id) => {
            let def = &module.structs[id.0 as usize];
            let fields = match &def.union_kind {
                Some(u) if matches!(u.storage, rokc_hir::UnionStorage::ByteArray) => {
                    vec![union_storage(&u.aliases, module, target)]
                }
                _ => def.fields.iter().map(|f| lower(&f.ty)).collect(),
            };
            MirTy::Struct(*id, fields)
        }
        HirTy::Sum(parts) => match crate::lower::niche::sum_niche(ty, module, target) {
            Some((m, n)) => crate::lower::niche::sum_repr(m, &n, module, target),
            None => sum_layout(
                &parts
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| !v.is_none())
                    .map(|(i, v)| (sum_tag(ty, i as u32), lower(v)))
                    .collect::<Vec<_>>(),
                target,
            ),
        },
        HirTy::Tuple(parts) => MirTy::Tuple(parts.iter().map(lower).collect()),
        HirTy::Enum(id) => {
            let def = &module.enums[id.0 as usize];
            match sum_layout(&enum_payloads(&def.variants, module, target), target) {
                // `#[repr(u8)]` ..: a fieldless enum is its tag.
                MirTy::Prim(PrimTy::I32) => MirTy::Prim(def.repr.int.unwrap_or(PrimTy::I32)),
                l => l,
            }
        }
        HirTy::Array(elem, n) => MirTy::Array(Box::new(lower(elem)), *n),
        // Spec 4.4: every function value (named fn or closure) is
        // `{code, env}`: `code` takes the `GcRef` env first; a named fn's
        // env is null. A C function pointer is the bare code pointer.
        HirTy::Fn(args, ret, _) if ty.is_c_fn() => {
            MirTy::Fn(args.iter().map(lower).collect(), Box::new(lower(ret)))
        }
        HirTy::Fn(args, ret, _) | HirTy::Closure { args, ret, .. } => {
            fat_fn(args.iter().map(lower).collect(), lower(ret))
        }
        HirTy::Throws { ok, err } => carrier(lower(ok), lower(err), target),
        HirTy::GcBuf(_) => MirTy::GcRef,
        HirTy::Mlir(t) => MirTy::Mlir(t.clone()),
    }
}

/// What `-g` shows of a `ty` value beyond its `MirTy`, or `None` when
/// that says it all: a tagged sum's variants, a `GcBuf`'s elements, an
/// `any`, a closure's environment.
#[must_use]
pub fn debug_ty(ty: &HirTy, module: &HirModule, target: &Target) -> Option<DebugTy> {
    let of = |t: &HirTy| {
        debug_ty(t, module, target).unwrap_or_else(|| DebugTy::Mir(lower_ty(t, module, target)))
    };
    // Every variant, those without data too (their payload `()`), so a
    // debugger names each value's case.
    let variants: Vec<(u32, String, Option<&HirTy>)> = match ty {
        HirTy::GcBuf(t) => {
            return Some(DebugTy::Buf(debug_name(ty, module), Box::new(of(t))));
        }
        HirTy::AnyTrait(_) => return Some(DebugTy::Any),
        // A carrier: `ok` / `err` over the payload, by `is_err`.
        HirTy::Throws { ok, err } if **ok != HirTy::Unit && **ok != HirTy::Never => {
            return Some(DebugTy::Sum(
                debug_name(ty, module),
                lower_ty(ty, module, target),
                vec![(0, "ok".to_owned(), of(ok)), (1, "err".to_owned(), of(err))],
            ));
        }
        HirTy::Closure { env, .. } => {
            let name = format!("env<{}>", debug_name(env, module));
            return Some(DebugTy::Closure(
                format!("{} {name}", debug_name(ty, module)),
                lower_ty(ty, module, target),
                Box::new(DebugTy::Boxed(name, Box::new(of(env)))),
            ));
        }
        // The environment's type is unknown here.
        HirTy::Fn(..) if !ty.is_c_fn() => {
            return Some(DebugTy::Closure(
                debug_name(ty, module),
                lower_ty(ty, module, target),
                Box::new(DebugTy::Mir(MirTy::GcRef)),
            ));
        }
        HirTy::Sum(parts) => (0u32..)
            .zip(parts)
            .map(|(i, v)| (sum_tag(ty, i), debug_name(v, module), Some(v)))
            .collect(),
        HirTy::Enum(id) => (0u32..)
            .zip(&module.enums[id.0 as usize].variants)
            .map(|(i, v)| (i, v.name.clone(), v.payload_ty.as_ref()))
            .collect(),
        _ => return None,
    };
    // A niche sum or a bare tag has no payload slot.
    let mir = lower_ty(ty, module, target);
    if !matches!(&mir, MirTy::Tuple(p) if p.len() == 2 && p[0] == MirTy::Prim(PrimTy::I32)) {
        return None;
    }
    let variants = variants
        .into_iter()
        .map(|(tag, name, t)| (tag, name, t.map_or(DebugTy::Mir(MirTy::Unit), of)))
        .collect();
    Some(DebugTy::Sum(debug_name(ty, module), mir, variants))
}

/// The variants of [`DebugTy::Any`]: each type stored into an `any` by
/// its stored tag.
#[must_use]
pub fn any_variants(module: &HirModule, target: &Target) -> Vec<(u32, String, DebugTy)> {
    module
        .any_tags
        .iter()
        .filter(|(t, _)| !matches!(t, HirTy::AnyTrait(_)))
        .map(|(t, tag)| {
            let mir = lower_ty(t, module, target);
            let d = debug_ty(t, module, target).unwrap_or_else(|| DebugTy::Mir(mir.clone()));
            let name = debug_name(t, module);
            if target.any_inline(&mir) {
                (*tag, name, d)
            } else {
                let obj = format!("box<{name}>");
                (
                    tag | crate::gc_abi::ANY_BOXED,
                    name,
                    DebugTy::Boxed(obj, Box::new(d)),
                )
            }
        })
        .collect()
}

/// A short name of `ty` for `-g` (`List`, `i64 | f64`, `GcBuf<u8>`).
fn debug_name(ty: &HirTy, module: &HirModule) -> String {
    let short = |n: &str| {
        let n = n.rsplit("::").next().unwrap_or(n);
        rokc_core::local::leaf(n.split("__").next().unwrap_or(n)).into_owned()
    };
    let list = |ts: &[HirTy]| {
        ts.iter()
            .map(|t| debug_name(t, module))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match ty {
        HirTy::Prim(p) => p.name().to_owned(),
        HirTy::Unit => "()".to_owned(),
        HirTy::None => "none".to_owned(),
        HirTy::Struct(id) => short(&module.structs[id.0 as usize].name),
        HirTy::Enum(id) => short(&module.enums[id.0 as usize].name),
        HirTy::GcBuf(t) => format!("GcBuf<{}>", debug_name(t, module)),
        HirTy::Sum(_) => ty.sum_name(|t| debug_name(t, module)),
        HirTy::Tuple(ts) => format!("({})", list(ts)),
        HirTy::Array(t, n) => format!("[{}; {n}]", debug_name(t, module)),
        HirTy::Fn(args, ret, _) | HirTy::Closure { args, ret, .. } => match &**ret {
            HirTy::Unit => format!("fn({})", list(args)),
            r => format!("fn({}) -> {}", list(args), debug_name(r, module)),
        },
        HirTy::Throws { ok, err } => format!(
            "throws ({}) -> {}",
            debug_name(err, module),
            debug_name(ok, module)
        ),
        _ => "any".to_owned(),
    }
}

/// Stored tag of member `index` of `sum_ty`. A sum with `none` stores
/// `none` as tag 0 and shifts the members before it up by one, so
/// zeroed memory (a fresh `GcBuf` / `List` slot) reads back as `none`.
/// HIR keeps the canonical member index (`none` last); only the bits
/// differ. A niche sum (`lower::niche`) has no tag field but tests as
/// these tags too. Other types (enums, `any`) store the index as is.
#[must_use]
pub fn sum_tag(sum_ty: &HirTy, index: u32) -> u32 {
    let HirTy::Sum(parts) = sum_ty else {
        return index;
    };
    match parts.iter().position(HirTy::is_none).map(|k| k as u32) {
        Some(k) if index == k => 0,
        Some(k) if index < k => index + 1,
        _ => index,
    }
}

/// The single tagged-union layout shared by enums and sums:
/// `(i32 tag, payload)`, or a bare `i32` tag when nothing carries data.
/// `payloads` are `(stored tag, payload)`, without `none` / unit
/// variants.
#[must_use]
pub fn sum_layout(payloads: &[(u32, MirTy)], target: &Target) -> MirTy {
    match unify_payload(payloads, target) {
        Some(p) => MirTy::Tuple(vec![MirTy::Prim(PrimTy::I32), p]),
        None => MirTy::Prim(PrimTy::I32),
    }
}

/// Payload slot able to hold every variant. Identical payloads share
/// their type; differing scalar payloads share an unsigned integer as
/// wide as the widest one; any other mix (aggregates, `str`) shares a
/// byte-array union `[uN; k]` with the max size and max align (N bits
/// = max align), or a [`MirTy::Union`] when a payload holds a GC
/// reference or needs more than 8-byte alignment. All are written /
/// read through `RValue::Bitcast`.
#[must_use]
pub fn unify_payload(variants: &[(u32, MirTy)], target: &Target) -> Option<MirTy> {
    let payloads: Vec<&MirTy> = variants.iter().map(|(_, p)| p).collect();
    let first = *payloads.first()?;
    if payloads.iter().all(|p| *p == first) {
        return Some(first.clone());
    }
    let uint = |bytes: u32| {
        MirTy::Prim(match bytes {
            1 => PrimTy::U8,
            2 => PrimTy::U16,
            4 => PrimTy::U32,
            _ => PrimTy::U64,
        })
    };
    if payloads
        .iter()
        .all(|p| matches!(p, MirTy::Prim(q) if *q != PrimTy::Str))
    {
        let bytes = payloads.iter().map(|p| target.size_align(p).0).max()?;
        return Some(uint(bytes.max(1)));
    }
    let (size, align) = payloads
        .iter()
        .map(|p| target.size_align(p))
        .fold((0, 1), |(s, a), (ps, pa)| (s.max(ps), a.max(pa)));
    // A union that can hold a GC reference is pointer-width words (a
    // `GcRef` is word-aligned, so it always lands on one) that keep the
    // variants: the collector decodes the tag and scans only the active
    // variant's reference words.
    if align > 8 || payloads.iter().any(|p| target.has_gc_ref(p)) {
        let w = target.pointer_width / 8;
        let words = size.next_multiple_of(align).div_ceil(w);
        return Some(MirTy::Union(variants.to_vec(), words, UnionKind::Sum));
    }
    Some(MirTy::Array(Box::new(uint(align)), size.div_ceil(align)))
}

/// Storage of a byte-array `union`: every field overlaid at offset 0,
/// sized and aligned for the largest. Words a field may keep a
/// reference in are a bare [`MirTy::Union`], scanned as ambiguous.
fn union_storage(fields: &[(String, HirTy)], module: &HirModule, target: &Target) -> MirTy {
    let tys: Vec<(u32, MirTy)> = (0u32..)
        .zip(fields.iter().map(|(_, t)| lower_ty(t, module, target)))
        .collect();
    let (size, align) = tys
        .iter()
        .map(|(_, t)| target.size_align(t))
        .fold((1, 1), |(s, a), (ps, pa)| (s.max(ps), a.max(pa)));
    if tys.iter().any(|(_, t)| target.has_gc_ref(t)) {
        let w = target.pointer_width / 8;
        return MirTy::Union(
            tys,
            size.next_multiple_of(align).div_ceil(w),
            UnionKind::Plain,
        );
    }
    // Words of the alignment, up to 8; a more aligned member raises
    // the union struct's own alignment (`union_align`).
    let (word, w) = match align {
        1 => (PrimTy::U8, 1),
        2 => (PrimTy::U16, 2),
        4 => (PrimTy::U32, 4),
        _ => (PrimTy::U64, 8),
    };
    MirTy::Array(Box::new(MirTy::Prim(word)), size.div_ceil(w))
}

/// The alignment of a byte-array union's most aligned member, when past
/// what its storage words give (8): the union struct is aligned so.
#[must_use]
pub fn union_align(def: &rokc_hir::HirStructDef, module: &HirModule, target: &Target) -> u32 {
    let Some(u) = def.union_kind.as_ref() else {
        return 0;
    };
    if !matches!(u.storage, rokc_hir::UnionStorage::ByteArray) {
        return 0;
    }
    let align = u
        .aliases
        .iter()
        .map(|(_, t)| target.size_align(&lower_ty(t, module, target)).1)
        .max()
        .unwrap_or(1);
    if align > 8 { align } else { 0 }
}

/// `(tag, payload)` of the variants that carry data (the tag is the
/// variant index).
fn enum_payloads(
    variants: &[rokc_hir::HirEnumVariant],
    module: &HirModule,
    target: &Target,
) -> Vec<(u32, MirTy)> {
    variants
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            v.payload_ty
                .as_ref()
                .map(|p| (i as u32, lower_ty(p, module, target)))
        })
        // A unit payload (`Poll<()>::Ready(())`) takes no space.
        .filter(|(_, t)| *t != MirTy::Unit)
        .collect()
}

/// The shared payload slot of enum `id`; `None` without payloads.
#[must_use]
pub fn enum_payload_ty(id: rokc_hir::EnumId, module: &HirModule, target: &Target) -> Option<MirTy> {
    match sum_layout(
        &enum_payloads(&module.enums[id.0 as usize].variants, module, target),
        target,
    ) {
        MirTy::Tuple(mut parts) => parts.pop(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc_abi::INTERIOR_REF;
    use crate::target::GcSlot;

    const fn p(t: PrimTy) -> MirTy {
        MirTy::Prim(t)
    }

    /// Payloads tagged 0, 1, ..
    fn tagged(ps: &[MirTy]) -> Vec<(u32, MirTy)> {
        ps.iter()
            .cloned()
            .enumerate()
            .map(|(i, p)| (i as u32, p))
            .collect()
    }

    fn x64() -> Target {
        Target::builtin("x86_64-unknown-linux-gnu")
    }

    #[test]
    fn sum_layouts() {
        let t = x64();
        // No payload: a bare tag.
        assert_eq!(sum_layout(&tagged(&[]), &t), p(PrimTy::I32));
        let i32_ = p(PrimTy::I32);
        // Differing scalars share an unsigned int as wide as the widest.
        assert_eq!(
            sum_layout(&tagged(&[p(PrimTy::I8), p(PrimTy::F64)]), &t),
            MirTy::Tuple(vec![i32_.clone(), p(PrimTy::U64)])
        );
        // Identical payloads keep their type.
        assert_eq!(
            sum_layout(&tagged(&[p(PrimTy::Str), p(PrimTy::Str)]), &t),
            MirTy::Tuple(vec![i32_, p(PrimTy::Str)])
        );
        // Aggregates without references: a byte-array union.
        let pair = MirTy::Tuple(vec![p(PrimTy::I32), p(PrimTy::I32)]);
        assert_eq!(
            unify_payload(&tagged(&[pair, p(PrimTy::I16)]), &t),
            Some(MirTy::Array(Box::new(p(PrimTy::U32)), 2))
        );
    }

    /// Spec 8.6.5: `{i1, union(T, E)}`. An `i32` tag (a payload with GC
    /// references) keeps the `i1` layout: same offsets and size.
    #[test]
    fn carriers_overlay_ok_and_err() {
        let m = HirModule::default();
        let throws = |ok: PrimTy, err: HirTy| HirTy::Throws {
            ok: Box::new(HirTy::Prim(ok)),
            err: Box::new(err),
        };
        let (str_, i64_) = (HirTy::Prim(PrimTy::Str), HirTy::Prim(PrimTy::I64));
        for triple in [
            "x86_64-unknown-linux-gnu",
            "wasm32-wasip2",
            "i686-unknown-linux-gnu",
        ] {
            let t = Target::builtin(triple);
            let c = lower_ty(&throws(PrimTy::Str, i64_.clone()), &m, &t);
            let MirTy::Tuple(ps) = &c else {
                panic!("{c:?}")
            };
            assert_eq!(ps[0], p(PrimTy::I32));
            assert!(matches!(ps[1], MirTy::Union(..)));
            let spec = MirTy::Tuple(vec![p(PrimTy::Bool), ps[1].clone()]);
            assert_eq!(t.field_offsets(&c), t.field_offsets(&spec), "{triple}");
            assert_eq!(t.size_align(&c), t.size_align(&spec), "{triple}");
            // Only the ok side's `str` is a reference, under tag 0.
            let mut slots = Vec::new();
            t.gc_offsets(&c, 0, &mut slots);
            let at = t.field_offsets(&c)[1];
            assert_eq!(
                slots,
                [GcSlot::Sum(
                    0,
                    vec![(0, vec![GcSlot::Word(at, INTERIOR_REF)])]
                )]
            );
        }
        let t = x64();
        // `-g`: `ok` / `err` variants over the payload, by the tag.
        let c = throws(PrimTy::Str, i64_);
        let Some(DebugTy::Sum(name, mir, vs)) = debug_ty(&c, &m, &t) else {
            panic!("no variant part")
        };
        assert_eq!(name, "throws (i64) -> str");
        assert_eq!(mir.debug_tag(), PrimTy::I32);
        assert_eq!(
            vs,
            [
                (0, "ok".to_owned(), DebugTy::Mir(p(PrimTy::Str))),
                (1, "err".to_owned(), DebugTy::Mir(p(PrimTy::I64)))
            ]
        );
        let narrow = debug_ty(&throws(PrimTy::I64, HirTy::Prim(PrimTy::U32)), &m, &t);
        assert!(matches!(narrow, Some(DebugTy::Sum(_, mir, _)) if mir.debug_tag() == PrimTy::U8));
        assert_eq!(
            lower_ty(&throws(PrimTy::I64, HirTy::Prim(PrimTy::U32)), &m, &t),
            MirTy::Tuple(vec![p(PrimTy::Bool), p(PrimTy::U64)])
        );
        let unit_ok = HirTy::Throws {
            ok: Box::new(HirTy::Unit),
            err: Box::new(str_),
        };
        assert_eq!(
            lower_ty(&unit_ok, &m, &t),
            MirTy::Tuple(vec![p(PrimTy::Bool), p(PrimTy::Str)])
        );
    }

    #[test]
    fn c_fn_or_none_is_a_bare_pointer() {
        let (t, m) = (x64(), HirModule::default());
        let i64_ = HirTy::Prim(PrimTy::I64);
        let c_fn = HirTy::Fn(
            vec![i64_.clone()],
            Box::new(i64_.clone()),
            rokc_hir::FnBounds {
                c_abi: true,
                ..rokc_hir::FnBounds::default()
            },
        );
        let none = HirTy::None;
        let thin = MirTy::Fn(vec![p(PrimTy::I64)], Box::new(p(PrimTy::I64)));
        let niche = |s: &HirTy| crate::lower::niche::sum_niche(s, &m, &t).map(|(_, n)| n);
        for parts in [
            vec![c_fn.clone(), none.clone()],
            vec![none.clone(), c_fn.clone()],
        ] {
            let sum = HirTy::Sum(parts);
            assert_eq!(niche(&sum).map(|n| n.bits), Some(0));
            assert_eq!(lower_ty(&sum, &m, &t), thin);
            // Non-null tests as the fn member's stored tag.
            assert_eq!(sum_tag(&sum, sum_tag_index(&sum, &c_fn)), 1);
        }
        assert!(!t.has_gc_ref(&thin));
        // A wider sum keeps the tagged layout.
        let wide = HirTy::Sum(vec![c_fn, i64_, none.clone()]);
        assert!(niche(&wide).is_none());
        assert!(matches!(lower_ty(&wide, &m, &t), MirTy::Tuple(_)));
        // A rok `fn` value or none is no niche.
        let rok_fn = HirTy::Fn(vec![], Box::new(HirTy::Unit), rokc_hir::FnBounds::default());
        assert!(niche(&HirTy::Sum(vec![rok_fn, none])).is_none());
    }

    fn sum_tag_index(sum: &HirTy, member: &HirTy) -> u32 {
        let HirTy::Sum(parts) = sum else {
            unreachable!()
        };
        parts.iter().position(|m| m == member).unwrap() as u32
    }

    #[test]
    fn str_in_a_union_stays_gc_visible() {
        // `enum E { S(str), P(i64, i64) }` once unified to `[u64; 2]`, so a
        // collection freed the runtime string.
        let t = x64();
        assert!(t.has_gc_ref(&p(PrimTy::Str)));
        let pair = MirTy::Tuple(vec![p(PrimTy::I64), p(PrimTy::I64)]);
        let vs = tagged(&[p(PrimTy::Str), pair]);
        assert_eq!(
            unify_payload(&vs, &t),
            Some(MirTy::Union(vs, 2, UnionKind::Sum))
        );
        assert!(!t.has_gc_ref(&p(PrimTy::I64)));
    }

    #[test]
    fn none_is_stored_as_tag_zero() {
        let (i32_, str_) = (HirTy::Prim(PrimTy::I32), HirTy::Prim(PrimTy::Str));
        let sum = HirTy::Sum(vec![i32_.clone(), str_.clone(), HirTy::None]);
        assert_eq!(
            (0..3).map(|i| sum_tag(&sum, i)).collect::<Vec<_>>(),
            [1, 2, 0]
        );
        // Members after `none` keep their index; no `none`, no shift.
        let mid = HirTy::Sum(vec![i32_.clone(), HirTy::None, str_.clone()]);
        assert_eq!(
            (0..3).map(|i| sum_tag(&mid, i)).collect::<Vec<_>>(),
            [1, 0, 2]
        );
        let plain = HirTy::Sum(vec![i32_, str_]);
        assert_eq!(
            (0..2).map(|i| sum_tag(&plain, i)).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(sum_tag(&HirTy::Prim(PrimTy::I64), 3), 3);
    }

    #[test]
    fn payload_unions_take_max_size_and_align() {
        let t = x64();
        assert_eq!(unify_payload(&[], &t), None);
        // Scalars: an unsigned int as wide as the widest.
        let scalar = |ps: &[MirTy]| unify_payload(&tagged(ps), &t);
        assert_eq!(
            scalar(&[p(PrimTy::Bool), p(PrimTy::I16)]),
            Some(p(PrimTy::U16))
        );
        assert_eq!(
            scalar(&[p(PrimTy::F32), p(PrimTy::I8)]),
            Some(p(PrimTy::U32))
        );
        // A 12-byte aggregate beside an `i64`: 16 bytes of 8-byte words.
        let three = MirTy::Tuple(vec![p(PrimTy::I32), p(PrimTy::I32), p(PrimTy::I32)]);
        assert_eq!(
            scalar(&[three.clone(), p(PrimTy::I64)]),
            Some(MirTy::Array(Box::new(p(PrimTy::U64)), 2))
        );
        // Three bytes beside a `u16`: 2-byte words.
        let bytes = MirTy::Array(Box::new(p(PrimTy::U8)), 3);
        assert_eq!(
            scalar(&[bytes.clone(), p(PrimTy::U16)]),
            Some(MirTy::Array(Box::new(p(PrimTy::U16)), 2))
        );
        // The union is as large and aligned as its largest member.
        let u = scalar(&[three.clone(), bytes, p(PrimTy::I64)]).unwrap();
        assert_eq!(t.size_align(&u), (16, 8));
        // Identical aggregates keep their type.
        assert_eq!(scalar(&[three.clone(), three.clone()]), Some(three));
        // The tag comes first.
        assert_eq!(
            sum_layout(&tagged(&[p(PrimTy::I64)]), &t),
            MirTy::Tuple(vec![p(PrimTy::I32), p(PrimTy::I64)])
        );
    }

    #[test]
    fn unions_follow_the_pointer_width() {
        // `str | i64` on a 32-bit target: an 8-byte `str`, 4-aligned.
        let t = Target::builtin("i686-unknown-linux-gnu");
        let vs = tagged(&[p(PrimTy::Str), p(PrimTy::I64)]);
        assert_eq!(
            unify_payload(&vs, &t),
            Some(MirTy::Union(vs, 2, UnionKind::Sum))
        );
    }
}
