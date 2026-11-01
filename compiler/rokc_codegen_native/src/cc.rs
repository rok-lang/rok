//! Calling conventions: where each scalar of a signature goes.
//!
//! A rok function's parameters and result are LLVM first-class values
//! (spec 8.6.6: the plain platform C ABI), except aggregates over 64 bytes,
//! passed by address (`FnLower::rok_shapes`). LLVM passes an aggregate
//! leaf by leaf, so this backend does too and objects of both backends
//! call each other. C signatures are classified by
//! `rokc_codegen_mlir::abi` (8.6.3) first, then placed the same way.

use rokc_ir_common::PrimTy;
use rokc_mir::{MirTy, Target};

use crate::lir::{K, Loc, Part};

/// A scalar of a value's memory: its byte offset and kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Leaf {
    pub off: u32,
    pub k: K,
    pub signed: bool,
}

/// The LLVM type shape the MLIR emitter gives a value, as far as the
/// calling convention cares.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shape {
    pub leaves: Vec<Leaf>,
    /// A top-level array of one kind (LLVM's "consecutive registers").
    pub array: bool,
    /// ABI alignment.
    pub align: u32,
    /// Passed as a copy on the stack (`x86_64` `byval`): the leaves are
    /// its 8-byte words.
    pub byval: bool,
    /// An LLVM aggregate: on Apple arm64 its narrow stack leaves take 4-byte
    /// slots, a scalar its own size (`SelectionDAG`).
    pub aggregate: bool,
}

/// The scalar kind of a primitive (`str` excluded).
#[must_use]
pub const fn prim_k(p: PrimTy, pw: u32) -> K {
    match p {
        PrimTy::I8 | PrimTy::U8 | PrimTy::CChar | PrimTy::Bool => K::I8,
        PrimTy::I16 | PrimTy::U16 => K::I16,
        PrimTy::I32 | PrimTy::U32 | PrimTy::Char => K::I32,
        PrimTy::F32 => K::F32,
        PrimTy::F64 => K::F64,
        PrimTy::Isize | PrimTy::Usize if pw == 32 => K::I32,
        _ => K::I64,
    }
}

/// The scalar kind of `ty` when it is one (`None`: an aggregate, `str`,
/// `()`).
#[must_use]
pub fn scalar_k(ty: &MirTy, t: &Target) -> Option<K> {
    let word = if t.pointer_width == 32 {
        K::I32
    } else {
        K::I64
    };
    match ty {
        MirTy::Prim(PrimTy::Str) => None,
        MirTy::Prim(p) => Some(prim_k(*p, t.pointer_width)),
        MirTy::Any | MirTy::GcRef | MirTy::Fn(..) => Some(word),
        _ => None,
    }
}

/// Scalar leaves of `ty` (as `mlir_type` spells it) at offset `at`.
pub fn leaves(ty: &MirTy, t: &Target, at: u32, out: &mut Vec<Leaf>) {
    let word = if t.pointer_width == 32 {
        K::I32
    } else {
        K::I64
    };
    let wb = t.pointer_width / 8;
    match ty {
        MirTy::Unit => {}
        MirTy::Prim(PrimTy::Str) => {
            out.push(Leaf {
                off: at,
                k: word,
                signed: false,
            });
            out.push(Leaf {
                off: at + wb,
                k: word,
                signed: false,
            });
        }
        MirTy::Prim(p) => out.push(Leaf {
            off: at,
            k: prim_k(*p, t.pointer_width),
            signed: p.is_signed_int() || *p == PrimTy::CChar,
        }),
        MirTy::Any | MirTy::GcRef | MirTy::Fn(..) => out.push(Leaf {
            off: at,
            k: word,
            signed: false,
        }),
        MirTy::Tuple(ps) | MirTy::Struct(_, ps) => {
            let offs = t.field_offsets(ty);
            if t.explicit_layout(ty) {
                // A packed LLVM struct with `[k x i8]` padding fields:
                // the padding bytes are leaves too.
                let size = t.size_align(ty).0;
                let mut cur = 0;
                for (p, off) in ps.iter().zip(&offs) {
                    for b in cur..*off {
                        out.push(Leaf {
                            off: at + b,
                            k: K::I8,
                            signed: false,
                        });
                    }
                    leaves(p, t, at + off, out);
                    cur = off + t.size_align(p).0;
                }
                for b in cur..size {
                    out.push(Leaf {
                        off: at + b,
                        k: K::I8,
                        signed: false,
                    });
                }
            } else {
                for (p, off) in ps.iter().zip(offs) {
                    leaves(p, t, at + off, out);
                }
            }
        }
        MirTy::Array(e, n) => {
            let s = t.size_align(e).0;
            for i in 0..*n {
                leaves(e, t, at + i * s, out);
            }
        }
        MirTy::Union(_, n, _) => {
            for i in 0..*n {
                out.push(Leaf {
                    off: at + i * wb,
                    k: word,
                    signed: false,
                });
            }
        }
    }
}

#[must_use]
pub fn shape(ty: &MirTy, t: &Target) -> Shape {
    let mut ls = Vec::new();
    leaves(ty, t, 0, &mut ls);
    let array = matches!(ty, MirTy::Array(..) | MirTy::Union(..))
        && ls.first().is_some_and(|f| ls.iter().all(|l| l.k == f.k));
    Shape {
        leaves: ls,
        array,
        align: t.size_align(ty).1,
        byval: false,
        aggregate: matches!(
            ty,
            MirTy::Tuple(_)
                | MirTy::Struct(..)
                | MirTy::Array(..)
                | MirTy::Union(..)
                | MirTy::Prim(PrimTy::Str)
        ),
    }
}

/// A coerced C type of `abi::classify` (`i64`, `iN`, `f32`,
/// `!llvm.array<N x T>`, `!llvm.struct<(A, B)>`, `vector<2xf32>`,
/// `!llvm.ptr`) as a shape.
#[must_use]
pub fn coerced(s: &str) -> Shape {
    fn one(s: &str) -> (K, u32) {
        match s.trim() {
            "f32" => (K::F32, 4),
            "f64" => (K::F64, 8),
            "!llvm.ptr" => (K::I64, 8),
            int => {
                let bits: u32 = int.trim_start_matches('i').parse().unwrap_or(64);
                match bits {
                    1..=8 => (K::I8, 1),
                    9..=16 => (K::I16, 2),
                    17..=32 => (K::I32, 4),
                    _ => (K::I64, 8),
                }
            }
        }
    }
    let leaf = |off, k| Leaf {
        off,
        k,
        signed: false,
    };
    if let Some(inner) = s
        .strip_prefix("!llvm.array<")
        .and_then(|r| r.strip_suffix('>'))
    {
        let (n, elem) = inner.split_once(" x ").unwrap_or(("1", inner));
        let n: u32 = n.trim().parse().unwrap_or(1);
        let (k, size) = one(elem);
        return Shape {
            leaves: (0..n).map(|i| leaf(i * size, k)).collect(),
            array: true,
            align: size,
            byval: false,
            aggregate: true,
        };
    }
    if let Some(inner) = s
        .strip_prefix("!llvm.struct<(")
        .and_then(|r| r.strip_suffix(")>"))
    {
        let mut ls = Vec::new();
        let mut at = 0u32;
        let mut align = 1;
        for part in inner.split(", ") {
            if part.starts_with("vector<2xf32>") {
                // Both halves in one SSE register: its 8 bytes.
                at = at.next_multiple_of(8);
                ls.push(leaf(at, K::F64));
                at += 8;
                align = align.max(8);
                continue;
            }
            let (k, size) = one(part);
            at = at.next_multiple_of(size);
            ls.push(leaf(at, k));
            at += size;
            align = align.max(size);
        }
        return Shape {
            leaves: ls,
            array: false,
            align,
            byval: false,
            aggregate: true,
        };
    }
    let (k, size) = if s == "vector<2xf32>" {
        (K::F64, 8)
    } else {
        one(s)
    };
    Shape {
        leaves: vec![leaf(0, k)],
        array: false,
        align: size,
        byval: false,
        aggregate: false,
    }
}

/// A `byval` copy of `size` bytes aligned to `align`: its 8-byte words.
#[must_use]
pub fn byval(size: u32, align: u32) -> Shape {
    Shape {
        leaves: (0..size.div_ceil(8))
            .map(|i| Leaf {
                off: 8 * i,
                k: K::I64,
                signed: false,
            })
            .collect(),
        array: false,
        align,
        byval: true,
        aggregate: true,
    }
}

/// The locations of one signature: per parameter its parts (one per
/// leaf, in leaf order); the result's parts, or `None` for a hidden
/// result pointer (`Loc::Sret`).
#[derive(Clone, Debug)]
pub struct SigLocs {
    pub params: Vec<Vec<Part>>,
    pub ret: Option<Vec<Part>>,
}

/// Which convention lowers a signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Conv {
    /// AAPCS64 as Apple runs it: stack arguments packed at their own
    /// size, not 8-byte slots.
    Darwin64,
    /// AAPCS64 elsewhere.
    Aapcs64,
    /// AAPCS64 on Windows: a C variadic call passes floats in integer
    /// registers, fixed arguments too.
    WinArm64,
    /// System V `x86_64` (Linux, macOS).
    SysV64,
    /// Windows x64: four register positions shared by both classes (an
    /// argument takes the next one, integer or float), then 8-byte stack
    /// slots past the caller's 32-byte home area.
    Win64,
}

impl Conv {
    #[must_use]
    pub fn of(t: &Target) -> Option<Self> {
        let windows = t.os == "windows";
        match t.arch.as_str() {
            "aarch64" if matches!(t.os.as_str(), "macos" | "ios") => Some(Self::Darwin64),
            "aarch64" if windows => Some(Self::WinArm64),
            "aarch64" => Some(Self::Aapcs64),
            "x86_64" if windows => Some(Self::Win64),
            "x86_64" => Some(Self::SysV64),
            _ => None,
        }
    }

    /// Integer / float argument registers.
    const fn arg_regs(self) -> (u8, u8) {
        match self {
            Self::SysV64 => (6, 8),
            Self::Win64 => (4, 4),
            _ => (8, 8),
        }
    }

    /// Integer / float result registers (`x86_64`: `rax rdx rcx`;
    /// `xmm0 xmm1` then the x87 `st0 st1`, as LLVM returns them).
    const fn ret_regs(self) -> (usize, usize) {
        match self {
            Self::SysV64 | Self::Win64 => (3, 4),
            _ => (8, 8),
        }
    }

    const fn x86(self) -> bool {
        matches!(self, Self::SysV64 | Self::Win64)
    }
}

const fn part(loc: Loc, l: Leaf) -> Part {
    Part {
        loc,
        k: l.k,
        signed: l.signed,
    }
}

/// Place `params` and `ret` (`None`: nothing, or with `sret` a C result
/// through a hidden pointer); parameters from `variadic_from` on are C
/// variadics.
#[must_use]
pub fn assign(
    conv: Conv,
    params: &[Shape],
    ret: Option<&Shape>,
    sret: bool,
    variadic_from: Option<usize>,
) -> SigLocs {
    let (max_int, max_fp) = conv.ret_regs();
    let ret = match ret {
        None if sret => None,
        None => Some(Vec::new()),
        Some(r) => {
            let ints = r.leaves.iter().filter(|l| !l.k.is_float()).count();
            let fps = r.leaves.len() - ints;
            (ints <= max_int && fps <= max_fp).then(|| {
                let (mut gi, mut fi) = (0u8, 0u8);
                r.leaves
                    .iter()
                    .map(|l| {
                        let loc = if l.k.is_float() {
                            fi += 1;
                            Loc::Fp(fi - 1)
                        } else {
                            gi += 1;
                            Loc::Int(gi - 1)
                        };
                        part(loc, *l)
                    })
                    .collect()
            })
        }
    };
    let (gpr, fpr) = conv.arg_regs();
    // `x86_64`: the hidden result pointer is the first integer argument.
    let mut ngrn = u8::from(conv.x86() && ret.is_none());
    let win64 = conv == Conv::Win64;
    let (mut nsrn, mut stack) = (0u8, if win64 { 32u32 } else { 0 });
    let darwin = conv == Conv::Darwin64;
    let as_ints: Vec<Shape>;
    let params = if conv == Conv::WinArm64 && variadic_from.is_some() {
        as_ints = params
            .iter()
            .map(|p| Shape {
                leaves: p
                    .leaves
                    .iter()
                    .map(|l| Leaf {
                        k: match l.k {
                            K::F64 => K::I64,
                            K::F32 => K::I32,
                            k => k,
                        },
                        ..*l
                    })
                    .collect(),
                ..p.clone()
            })
            .collect();
        &as_ints
    } else {
        params
    };
    // Apple packs a stack argument at its own size, an aggregate's
    // narrow leaves at 4 bytes.
    let packed = |k: K, agg: bool| match (darwin, agg) {
        (true, false) => k.size(),
        (true, true) => k.size().max(4),
        (false, _) => 8,
    };
    let mut out = Vec::new();
    for (i, p) in params.iter().enumerate() {
        let mut parts = Vec::new();
        if p.byval {
            // A copy on the stack (System V MEMORY class).
            stack = stack.next_multiple_of(p.align.max(8));
            for l in &p.leaves {
                parts.push(part(Loc::Stack(stack + l.off), *l));
            }
            stack += p.leaves.last().map_or(0, |l| l.off + 8);
            out.push(parts);
            continue;
        }
        if darwin && variadic_from.is_some_and(|v| i >= v) {
            // Apple: every variadic argument in its own 8-byte stack
            // slot (narrow integers widened, `f32` already `f64`).
            for l in &p.leaves {
                let k = if l.k.is_float() { K::F64 } else { K::I64 };
                stack = stack.next_multiple_of(8);
                parts.push(Part {
                    loc: Loc::Stack(stack),
                    k,
                    signed: l.signed,
                });
                stack += 8;
            }
            out.push(parts);
            continue;
        }
        // AArch64: an array of one kind takes consecutive registers, or
        // the stack (and no more registers of its class).
        let block_k = p
            .leaves
            .first()
            .map(|l| l.k)
            .filter(|k| !conv.x86() && p.array && matches!(k, K::I64 | K::F32 | K::F64));
        if let Some(k) = block_k {
            let next = if k.is_float() { &mut nsrn } else { &mut ngrn };
            if usize::from(*next) + p.leaves.len() <= 8 {
                for l in &p.leaves {
                    let loc = if k.is_float() {
                        Loc::Fp(*next)
                    } else {
                        Loc::Int(*next)
                    };
                    *next += 1;
                    parts.push(part(loc, *l));
                }
            } else {
                *next = 8;
                let align = if darwin {
                    p.align.min(16)
                } else {
                    p.align.clamp(8, 16)
                };
                stack = stack.next_multiple_of(align);
                for l in &p.leaves {
                    parts.push(part(Loc::Stack(stack), *l));
                    stack += k.size();
                }
            }
            out.push(parts);
            continue;
        }
        for l in &p.leaves {
            let loc = if win64 && ngrn < gpr {
                // `ngrn` is the register position, of either class.
                ngrn += 1;
                if l.k.is_float() {
                    Loc::Fp(ngrn - 1)
                } else {
                    Loc::Int(ngrn - 1)
                }
            } else if win64 {
                stack += 8;
                Loc::Stack(stack - 8)
            } else if l.k.is_float() && nsrn < fpr {
                nsrn += 1;
                Loc::Fp(nsrn - 1)
            } else if !l.k.is_float() && ngrn < gpr {
                ngrn += 1;
                Loc::Int(ngrn - 1)
            } else {
                let s = packed(l.k, p.aggregate);
                stack = stack.next_multiple_of(s);
                stack += s;
                Loc::Stack(stack - s)
            };
            parts.push(part(loc, *l));
        }
        out.push(parts);
    }
    SigLocs { params: out, ret }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Target {
        Target::builtin("aarch64-apple-darwin")
    }

    /// Checked against `llc -global-isel=0 -mtriple=arm64-apple-macosx`
    /// (`SelectionDAG`, as the LLVM path selects) on the same first-class
    /// aggregate signatures.
    #[test]
    fn darwin_matches_llvm() {
        let t = t();
        let s = |ty: &MirTy| shape(ty, &t);
        let str_ = MirTy::Prim(PrimTy::Str);
        let i64_ = MirTy::Prim(PrimTy::I64);
        // Five `str`s: the fifth on the stack, then packed narrow ints.
        let mut ps: Vec<Shape> = (0..5).map(|_| s(&str_)).collect();
        for p in [
            PrimTy::I8,
            PrimTy::Bool,
            PrimTy::I16,
            PrimTy::I32,
            PrimTy::I64,
        ] {
            ps.push(s(&MirTy::Prim(p)));
        }
        let l = assign(Conv::Darwin64, &ps, None, false, None);
        assert_eq!(l.params[3][1].loc, Loc::Int(7));
        assert_eq!(l.params[4][0].loc, Loc::Stack(0));
        let at: Vec<Loc> = l.params[5..].iter().map(|p| p[0].loc).collect();
        assert_eq!(
            at,
            [16, 17, 18, 20, 24].map(Loc::Stack).to_vec(),
            "packed narrow stack args"
        );
        // `[2 x i64]` with one register left: wholly on the stack, and
        // the register is gone; a `{i64, i64}` splits.
        let mut ps: Vec<Shape> = (0..7).map(|_| s(&i64_)).collect();
        ps.push(s(&MirTy::Array(Box::new(i64_.clone()), 2)));
        ps.push(s(&i64_));
        let l = assign(Conv::Darwin64, &ps, None, false, None);
        assert_eq!(l.params[7][0].loc, Loc::Stack(0));
        assert_eq!(l.params[8][0].loc, Loc::Stack(16));
        ps[7] = s(&MirTy::Tuple(vec![i64_.clone(), i64_.clone()]));
        let l = assign(Conv::Darwin64, &ps, None, false, None);
        assert_eq!(l.params[7][0].loc, Loc::Int(7));
        assert_eq!(l.params[7][1].loc, Loc::Stack(0));
        // An aggregate's narrow leaves past the registers: 4-byte slots
        // (`[10 x i1]`, then `[3 x i16]`).
        let bools = s(&MirTy::Array(Box::new(MirTy::Prim(PrimTy::Bool)), 10));
        let shorts = s(&MirTy::Array(Box::new(MirTy::Prim(PrimTy::I16)), 3));
        let l = assign(Conv::Darwin64, &[bools, shorts], None, false, None);
        assert_eq!(l.params[0][7].loc, Loc::Int(7));
        assert_eq!(l.params[0][8].loc, Loc::Stack(0));
        assert_eq!(l.params[0][9].loc, Loc::Stack(4));
        assert_eq!(l.params[1][2].loc, Loc::Stack(16));
        // Results: 8 integers in registers, 9 through `x8`.
        let r8 = s(&MirTy::Tuple(vec![i64_.clone(); 8]));
        assert!(
            assign(Conv::Darwin64, &[], Some(&r8), false, None)
                .ret
                .is_some()
        );
        let r9 = s(&MirTy::Tuple(vec![i64_; 9]));
        assert!(
            assign(Conv::Darwin64, &[], Some(&r9), false, None)
                .ret
                .is_none()
        );
    }

    /// Checked against `llc -mtriple=x86_64-pc-windows-msvc`: `g({ptr,
    /// i64}, double, {ptr, i64}, i32)` takes `rcx rdx xmm2 r9`, then
    /// `[rsp + 32]`, `[rsp + 40]`; four `i64` come back through `sret`
    /// (in `rcx`, so the arguments start at the second position).
    #[test]
    fn win64_matches_llvm() {
        let t = Target::builtin("x86_64-pc-windows-msvc");
        let s = |ty: &MirTy| shape(ty, &t);
        let str_ = s(&MirTy::Prim(PrimTy::Str));
        let ps = [
            str_.clone(),
            s(&MirTy::Prim(PrimTy::F64)),
            str_,
            s(&MirTy::Prim(PrimTy::I32)),
        ];
        let l = assign(Conv::Win64, &ps, None, false, None);
        let at: Vec<Loc> = l.params.iter().flatten().map(|p| p.loc).collect();
        assert_eq!(
            at,
            [
                Loc::Int(0),
                Loc::Int(1),
                Loc::Fp(2),
                Loc::Int(3),
                Loc::Stack(32),
                Loc::Stack(40)
            ]
        );
        let r4 = s(&MirTy::Tuple(vec![MirTy::Prim(PrimTy::I64); 4]));
        let l = assign(Conv::Win64, &ps[1..2], Some(&r4), false, None);
        assert!(l.ret.is_none());
        assert_eq!(l.params[0][0].loc, Loc::Fp(1));
    }

    /// Windows arm64: a C variadic call passes every float in an integer
    /// register (`CC_AArch64_Win64_VarArg`).
    #[test]
    fn win_arm64_variadic_floats_in_integer_registers() {
        let t = Target::builtin("aarch64-pc-windows-msvc");
        let ps = [
            shape(&MirTy::Prim(PrimTy::F64), &t),
            shape(&MirTy::Prim(PrimTy::F64), &t),
        ];
        let l = assign(Conv::WinArm64, &ps, None, false, Some(1));
        let at: Vec<(Loc, K)> = l.params.iter().flatten().map(|p| (p.loc, p.k)).collect();
        assert_eq!(at, [(Loc::Int(0), K::I64), (Loc::Int(1), K::I64)]);
        let l = assign(Conv::WinArm64, &ps, None, false, None);
        assert_eq!(l.params[1][0].loc, Loc::Fp(1));
    }
}
