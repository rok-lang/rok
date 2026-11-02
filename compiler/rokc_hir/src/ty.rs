//! Type lowering, AST `Ty` → `HirTy`. Primitive names are recognised by
//! spelling, not `#[lang]` slots.

use rokc_ast::{Path, Ty, TyKind};
use rokc_ir_common::PrimTy;

use crate::HirTy;

/// A written sum: written-sum members (or aliases of one)
/// spread, so `(i32 | none) | none` is `i32 | none`. A generic member
/// (`param`, `Self`, projections, `Nest`) is a generic term for
/// `HirTy::layered`: `T | none` with `T = i32 | none` is `(i32 | none)?`,
/// while `T | U` with `T = i32 | none`, `U = i32` is `i32 | none`.
pub fn written_sum(
    parts: &[Ty],
    param: impl Fn(&str) -> bool,
    rec: impl Fn(&Ty) -> HirTy,
) -> HirTy {
    let generic = |t: &Ty| match &t.kind {
        TyKind::SelfUpper | TyKind::QPath(_) => true,
        TyKind::Path(p) => p
            .segments
            .first()
            .is_some_and(|s| s.name == "Self" || param(&s.name)),
        _ => false,
    };
    let mut written = Vec::new();
    let mut subst = Vec::new();
    for p in parts {
        if let Some(inner) = p.nest_inner() {
            subst.push(rec(inner));
        } else if generic(p) {
            subst.push(rec(p));
        } else {
            rec(p).spread(&mut written);
        }
    }
    HirTy::layered(written, subst)
}

/// Resolve an AST type to HIR. Unknown paths are kept as `HirTy::Path`
/// for later resolution.
#[must_use]
pub fn resolve_ty(ty: &Ty) -> HirTy {
    match &ty.kind {
        TyKind::Path(path) => resolve_path_ty(path),
        // Resolved where impls are known (`LowerCtx::resolve_ast_ty`).
        TyKind::QPath(q) => HirTy::Path(vec!["<..>".to_owned(), q.item.clone()]),
        TyKind::Never => HirTy::Never,
        TyKind::Nest(x) => resolve_ty(x),
        // Only a trait argument; as a type it names nothing.
        TyKind::Binding(name, _) => HirTy::Path(vec![format!("{name} = ..")]),
        TyKind::Sum(parts) => written_sum(parts, |_| false, resolve_ty),
        TyKind::Tuple(parts) => {
            if parts.is_empty() {
                HirTy::Unit
            } else {
                HirTy::Tuple(parts.iter().map(resolve_ty).collect())
            }
        }
        // Plain `any` = `any ()`: tag + payload, no bound.
        TyKind::Any => HirTy::AnyTrait(Vec::new()),
        TyKind::AnyTrait(bounds) => HirTy::any(
            bounds
                .iter()
                .map(|b| {
                    let segs: Vec<&str> = b.segments.iter().map(|s| s.name.as_str()).collect();
                    crate::TraitRef::plain(&segs.join("::"))
                })
                .collect(),
        ),
        TyKind::None => HirTy::None,
        TyKind::SelfUpper => HirTy::Path(vec!["Self".to_owned()]),
        TyKind::Array(elem, len) => {
            let n = const_expr_to_u32(len).unwrap_or(0);
            HirTy::Array(Box::new(resolve_ty(elem)), n)
        }
        // Name-free resolution (prim probes) drops `+ Send` bounds.
        TyKind::Fn(args, ret, _) => HirTy::Fn(
            args.iter().map(resolve_ty).collect(),
            Box::new(resolve_ty(ret)),
            crate::FnBounds::default(),
        ),
        TyKind::Throws { ok, err } => throws_ty(resolve_ty(ok), resolve_ty(err)),
        TyKind::Const(c) => {
            const_expr_to_u32(c).map_or(HirTy::Unknown, |v| HirTy::Const(v.into(), None))
        }
    }
}

/// `throws (err) -> ok` normalized: error sums flattened, `NoThrow` parts
/// dropped; no error left is plain `ok`.
pub fn throws_ty(ok: HirTy, err: HirTy) -> HirTy {
    let parts: Vec<HirTy> = match HirTy::sum([err]) {
        HirTy::Sum(ps) => ps,
        t => vec![t],
    };
    let parts: Vec<HirTy> = parts.into_iter().filter(|p| *p != HirTy::NoThrow).collect();
    if parts.is_empty() {
        return ok;
    }
    let err = HirTy::sum(parts);
    HirTy::Throws {
        ok: Box::new(ok),
        err: Box::new(err),
    }
}

// Name-free: literals and arithmetic over them.
fn const_expr_to_u32(c: &rokc_ast::ConstExpr) -> Option<u32> {
    crate::lower::const_eval::syntactic_int(c, &|_| None).and_then(|v| u32::try_from(v).ok())
}

/// The primitive type `name` spells, if any.
pub fn prim_named(name: &str) -> Option<PrimTy> {
    match name {
        "i8" => Some(PrimTy::I8),
        "i16" => Some(PrimTy::I16),
        "i32" => Some(PrimTy::I32),
        "i64" => Some(PrimTy::I64),
        "isize" => Some(PrimTy::Isize),
        "u8" => Some(PrimTy::U8),
        "u16" => Some(PrimTy::U16),
        "u32" => Some(PrimTy::U32),
        "u64" => Some(PrimTy::U64),
        "usize" => Some(PrimTy::Usize),
        "f32" => Some(PrimTy::F32),
        "f64" => Some(PrimTy::F64),
        "bool" => Some(PrimTy::Bool),
        "c_char" => Some(PrimTy::CChar),
        "char" => Some(PrimTy::Char),
        "str" => Some(PrimTy::Str),
        _ => None,
    }
}

fn resolve_path_ty(path: &Path) -> HirTy {
    if let [seg] = path.segments.as_slice() {
        if let Some(prim) = prim_named(&seg.name) {
            return HirTy::Prim(prim);
        }
    }
    HirTy::Path(path.segments.iter().map(|s| s.name.clone()).collect())
}
