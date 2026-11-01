//! C ABI lowering of `extern "C"` aggregate parameters and results (spec
//! 8.6.3).
//!
//! LLVM does not classify aggregates, so, like clang, the front end rewrites
//! a by-value `struct` into what the platform C ABI uses and converts
//! through memory at the call site.
//!
//! - AAPCS64 (Linux / Windows / Apple arm64): a homogeneous float
//!   aggregate (HFA, 1-4 members of one float type) goes in `v`
//! - AAPCS64 (Linux / Windows / Apple arm64): a homogeneous float
//!   aggregate (HFA, 1-4 members of one float type) goes in `v` registers
//!   as `[n x fT]`; others up to 16 bytes as `i64` / `[2 x i64]` (results
//!   ≤ 8 bytes as `iN`); bigger ones by pointer to a caller copy, returned
//!   through `x8` (`sret`). Apple differs only for variadics (all on the
//!   stack; LLVM handles it) and small-integer extension (`c_ext_attr`).
//!   aggregate is MEMORY), > 16 bytes are MEMORY (`byval` / `sret`).
//! - Windows x64: 1 / 2 / 4 / 8-byte aggregates as `iN`, others by
//!   reference / `sret`.
//! - i386 System V and Windows x86 (cdecl): aggregates are copied onto
//!   the stack (`byval`, 4-aligned); results come back through `sret`
//!   (the System V callee pops its pointer, LLVM's job), except Windows's
//!   1 / 2 / 4 / 8-byte ones, in `eax` / `edx:eax`.
//! - 32-bit ARM AAPCS-VFP: an HFA goes in `s` / `d` registers as
//!   `[n x fT]`; other aggregates up to 64 bytes as `[n x i32]` (`i64`
//!   when 8-aligned: an even register pair) through `r0`-`r3` and the
//!   stack, bigger ones `byval`; results ≤ 4 bytes in `r0`, HFAs in
//!   VFP registers, others through `sret`.
//! - WebAssembly: an aggregate of one scalar (no padding) as that scalar,
//!   others as a pointer to a caller copy (not `byval`: LLVM's -O2 then
//!   passes a stale slot) / `sret`.
//! - RISC-V LP64D: a struct of one float, two floats, or a float and an
//!   integer of at most 8 bytes (flattened) goes in `fa` / `a` registers
//!   as `f32` / `f64` / `{t1, t2}` while enough of both are left; other
//!   aggregates up to 16 bytes as `i64` / `[2 x i64]` (`i128` when
//!   16-aligned), split between the last `a` register and the stack as
//!   LLVM splits them; bigger ones by reference / `sret`.
//!
//! Other targets reject aggregates across the C boundary.

use rokc_ir_common::PrimTy;
use rokc_mir::{CAbi, MirTy, Target};

/// How one parameter crosses the C boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgAbi {
    /// As its own MLIR type (scalars).
    Direct,
    /// Bits reinterpreted as this MLIR type (through memory).
    Coerce(String),
    /// A pointer to a caller-owned copy; `byval` makes it an on-stack
    /// copy (System V MEMORY class) rather than a plain pointer argument.
    Indirect { byval: bool },
}

/// How the result comes back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetAbi {
    Direct,
    Coerce(String),
    /// Caller passes a hidden `sret` pointer first; nothing is returned.
    Sret,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FnAbi {
    pub ret: RetAbi,
    pub args: Vec<ArgAbi>,
    /// C variadic (spec 8.6.10): an `llvm.func` called with `llvm.call`;
    /// arguments past `args` are its `...`.
    pub variadic: bool,
    /// A runtime (`extern "rok"`) call: `str` crosses as its `(ptr, len)`
    /// pair, not as a `char*`.
    pub rok: bool,
    /// `extern "system"` where it is `stdcall` (32-bit x86 Windows, spec
    /// 5.9.2): an `llvm.func` / `llvm.call` with `x86_stdcallcc` (the
    /// callee pops its arguments; LLVM decorates the symbol `_f@N`).
    pub stdcall: bool,
}

/// A by-value aggregate (as opposed to a scalar, `str` or `()`).
#[must_use]
pub const fn is_aggregate(ty: &MirTy) -> bool {
    matches!(
        ty,
        MirTy::Struct(..) | MirTy::Tuple(_) | MirTy::Array(..) | MirTy::Union(..)
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Leaf {
    Int,
    F32,
    F64,
}

/// Scalar leaves of `ty` at their byte offsets (`str` is two words).
fn leaves(ty: &MirTy, at: u32, target: &Target, out: &mut Vec<(u32, u32, Leaf)>) {
    let word = target.pointer_width / 8;
    match ty {
        MirTy::Unit => {}
        MirTy::Prim(PrimTy::F32) => out.push((at, 4, Leaf::F32)),
        MirTy::Prim(PrimTy::F64) => out.push((at, 8, Leaf::F64)),
        MirTy::Prim(PrimTy::Str) => {
            out.push((at, word, Leaf::Int));
            out.push((at + word, word, Leaf::Int));
        }
        MirTy::Prim(_) | MirTy::Any | MirTy::GcRef | MirTy::Fn(..) | MirTy::Mlir(_) => {
            out.push((at, target.size_align(ty).0, Leaf::Int));
        }
        MirTy::Tuple(ps) | MirTy::Struct(_, ps) => {
            for (p, off) in ps.iter().zip(target.field_offsets(ty)) {
                leaves(p, at + off, target, out);
            }
        }
        MirTy::Array(e, n) => {
            let s = target.size_align(e).0;
            for i in 0..*n {
                leaves(e, at + i * s, target, out);
            }
        }
        MirTy::Union(_, n, _) => {
            for i in 0..*n {
                out.push((at + i * word, word, Leaf::Int));
            }
        }
    }
}

/// The scalar an aggregate made of one scalar leaf (and no padding)
/// crosses the WebAssembly C ABI as.
fn single_scalar(ty: &MirTy, target: &Target) -> Option<String> {
    let mut ls = Vec::new();
    leaves(ty, 0, target, &mut ls);
    let [(0, size, leaf)] = ls[..] else {
        return None;
    };
    (size == target.size_align(ty).0).then(|| match leaf {
        Leaf::F32 => "f32".to_owned(),
        Leaf::F64 => "f64".to_owned(),
        Leaf::Int => format!("i{}", size * 8),
    })
}

/// Homogeneous float aggregate: `[n x f32|f64]` for 1..=4 members.
fn hfa(ty: &MirTy, target: &Target) -> Option<String> {
    let mut ls = Vec::new();
    leaves(ty, 0, target, &mut ls);
    let first = ls.first()?.2;
    if first == Leaf::Int || ls.len() > 4 || ls.iter().any(|l| l.2 != first) {
        return None;
    }
    let elem = if first == Leaf::F32 { "f32" } else { "f64" };
    Some(format!("!llvm.array<{} x {elem}>", ls.len()))
}

/// Classify a C signature on `target`; `params` are the fixed ones
/// (variadic extras are scalars). `None`: aggregates the target can't pass.
#[must_use]
pub fn classify(target: &Target, params: &[MirTy], ret: &MirTy, variadic: bool) -> Option<FnAbi> {
    // A `#[repr(transparent)]` struct passes as its field.
    let params: Vec<MirTy> = params.iter().map(|p| transparent(p, target)).collect();
    let params = params.as_slice();
    let ret = &transparent(ret, target);
    let (ret, args) = match target.c_abi {
        CAbi::Aarch64 => (
            aapcs_ret(ret, target),
            params.iter().map(|p| aapcs_arg(p, target)).collect(),
        ),
        CAbi::X86_64Win => {
            let small = |t: &MirTy| match target.size_align(t).0 {
                s @ (1 | 2 | 4 | 8) => Some(format!("i{}", s * 8)),
                _ => None,
            };
            let ret = if is_aggregate(ret) {
                small(ret).map_or(RetAbi::Sret, RetAbi::Coerce)
            } else {
                RetAbi::Direct
            };
            let args = params
                .iter()
                .map(|p| {
                    if !is_aggregate(p) {
                        return ArgAbi::Direct;
                    }
                    small(p).map_or(ArgAbi::Indirect { byval: false }, ArgAbi::Coerce)
                })
                .collect();
            (ret, args)
        }
        CAbi::X86_64SysV => sysv(params, ret, target),
        CAbi::X86SysV | CAbi::X86Win => x86(params, ret, target),
        CAbi::ArmHf => (
            aapcs32_ret(ret, target),
            params.iter().map(|p| aapcs32_arg(p, target)).collect(),
        ),
        CAbi::Wasm32 => {
            let ret = if is_aggregate(ret) {
                single_scalar(ret, target).map_or(RetAbi::Sret, RetAbi::Coerce)
            } else {
                RetAbi::Direct
            };
            let args = params
                .iter()
                .map(|p| {
                    if !is_aggregate(p) {
                        return ArgAbi::Direct;
                    }
                    single_scalar(p, target)
                        .map_or(ArgAbi::Indirect { byval: false }, ArgAbi::Coerce)
                })
                .collect();
            (ret, args)
        }
        CAbi::Riscv64 => riscv(params, ret, target),
        CAbi::Other => {
            if params.iter().chain([ret]).any(is_aggregate) {
                return None;
            }
            (RetAbi::Direct, vec![ArgAbi::Direct; params.len()])
        }
    };
    Some(FnAbi {
        ret,
        args,
        variadic,
        rok: false,
        stdcall: false,
    })
}

/// A runtime (`extern "rok"`) declaration (spec 8.5): the C ABI, with
/// `str` a by-value `(ptr, len)` C struct like `RokFn`.
///
/// `None` means a plain call already matches C: LLVM splits a two-word pair
/// (`str`, a `fn` value) into words, which agrees with C while all integer
/// words fit in registers. Otherwise the signature is classified: a pair
/// that could straddle the last register and the stack, any bigger or float
/// aggregate, any pair on Windows x64 / WebAssembly (by reference), and a
/// by-value pair result on 32-bit targets (memory, or `edx:eax` on Windows
/// x86). RISC-V passes a pair as C's 16-byte integer struct both ways,
/// which LLVM matches. No runtime runs on other targets.
#[must_use]
pub fn classify_rok(target: &Target, params: &[MirTy], ret: &MirTy) -> Option<FnAbi> {
    let by_value = |t: &MirTy| is_aggregate(t) || *t == MirTy::Prim(PrimTy::Str);
    let word = target.pointer_width / 8;
    let pair = |t: &MirTy| {
        let mut ls = Vec::new();
        leaves(t, 0, target, &mut ls);
        ls == [(0, word, Leaf::Int), (word, word, Leaf::Int)] && target.size_align(t).0 == 2 * word
    };
    let regs = match target.c_abi {
        CAbi::Aarch64 => 8,
        CAbi::X86_64SysV => 6,
        CAbi::X86_64Win | CAbi::Wasm32 => 0,
        CAbi::X86SysV | CAbi::X86Win | CAbi::ArmHf if by_value(ret) => 0,
        CAbi::X86SysV | CAbi::X86Win | CAbi::ArmHf | CAbi::Riscv64 => u32::MAX,
        CAbi::Other => return None,
    };
    let words: u32 = params
        .iter()
        .filter(|t| !matches!(t, MirTy::Prim(PrimTy::F32 | PrimTy::F64)))
        .map(|t| target.size_align(t).0.div_ceil(word))
        .sum();
    let other = params
        .iter()
        .chain([ret])
        .any(|t| is_aggregate(t) && !pair(t));
    if !other {
        if regs == 0 {
            if !params.iter().chain([ret]).any(by_value) {
                return None;
            }
        } else if words <= regs {
            return None;
        }
    }
    let pair = |t: &MirTy| {
        if *t == MirTy::Prim(PrimTy::Str) {
            MirTy::Tuple(vec![MirTy::Prim(PrimTy::Usize); 2])
        } else {
            t.clone()
        }
    };
    let params: Vec<MirTy> = params.iter().map(pair).collect();
    let mut abi = classify(target, &params, &pair(ret), false)?;
    abi.rok = true;
    Some(abi)
}

/// Aggregates larger than this go between rok functions by address.
pub const BY_REF_BYTES: u32 = 64;

/// A rok-to-rok argument or result passed by address (spec 8.6.6).
///
/// An aggregate over [`BY_REF_BYTES`], or on 32-bit Arm an array of 1- or
/// 2-byte elements (LLVM's Arm lowering writes their stack leaves in 4-byte
/// slots but reads them packed). The argument is a pointer the callee
/// copies from; the result goes through a pointer passed first and the
/// function returns nothing.
#[must_use]
pub fn rok_by_ref(ty: &MirTy, target: &Target) -> bool {
    if let (MirTy::Array(elem, _), CAbi::ArmHf) = (ty, target.c_abi) {
        if target.size_align(elem).0 < 4 {
            return true;
        }
    }
    is_aggregate(ty) && target.size_align(ty).0 > BY_REF_BYTES
}

/// The type a value of `ty` crosses the C boundary as: the field of a
/// `#[repr(transparent)]` struct (recursively), else `ty`.
#[must_use]
pub fn transparent(ty: &MirTy, target: &Target) -> MirTy {
    match ty {
        MirTy::Struct(id, ps) if target.repr(*id).transparent && ps.len() == 1 => {
            transparent(&ps[0], target)
        }
        _ => ty.clone(),
    }
}

fn aapcs_arg(ty: &MirTy, target: &Target) -> ArgAbi {
    if !is_aggregate(ty) {
        return ArgAbi::Direct;
    }
    let size = target.size_align(ty).0;
    if size == 0 {
        return ArgAbi::Direct;
    }
    if let Some(h) = hfa(ty, target) {
        return ArgAbi::Coerce(h);
    }
    match size {
        1..=8 => ArgAbi::Coerce("i64".to_owned()),
        9..=16 => ArgAbi::Coerce("!llvm.array<2 x i64>".to_owned()),
        _ => ArgAbi::Indirect { byval: false },
    }
}

fn aapcs_ret(ty: &MirTy, target: &Target) -> RetAbi {
    if !is_aggregate(ty) {
        return RetAbi::Direct;
    }
    let size = target.size_align(ty).0;
    if size == 0 {
        return RetAbi::Direct;
    }
    if let Some(h) = hfa(ty, target) {
        return RetAbi::Coerce(h);
    }
    match size {
        1..=8 => RetAbi::Coerce(format!("i{}", size * 8)),
        9..=16 => RetAbi::Coerce("!llvm.array<2 x i64>".to_owned()),
        _ => RetAbi::Sret,
    }
}

/// The alignment of a `byval` stack copy of `ty`.
#[must_use]
pub fn byval_align(ty: &MirTy, target: &Target) -> u32 {
    let a = target.size_align(ty).1;
    match target.c_abi {
        CAbi::X86SysV | CAbi::X86Win => 4,
        CAbi::ArmHf => a.clamp(4, 8),
        _ => a.max(8),
    }
}

fn x86(params: &[MirTy], ret: &MirTy, target: &Target) -> (RetAbi, Vec<ArgAbi>) {
    let size = target.size_align(ret).0;
    let ret = if !is_aggregate(ret) || size == 0 {
        RetAbi::Direct
    } else if target.c_abi == CAbi::X86Win && matches!(size, 1 | 2 | 4 | 8) {
        RetAbi::Coerce(format!("i{}", size * 8))
    } else {
        RetAbi::Sret
    };
    let args = params
        .iter()
        .map(|p| {
            if is_aggregate(p) && target.size_align(p).0 > 0 {
                ArgAbi::Indirect { byval: true }
            } else {
                ArgAbi::Direct
            }
        })
        .collect();
    (ret, args)
}

fn aapcs32_arg(ty: &MirTy, target: &Target) -> ArgAbi {
    let (size, align) = target.size_align(ty);
    if !is_aggregate(ty) || size == 0 {
        return ArgAbi::Direct;
    }
    if let Some(h) = hfa(ty, target) {
        return ArgAbi::Coerce(h);
    }
    if size > 64 {
        return ArgAbi::Indirect { byval: true };
    }
    let (n, elem) = if align <= 4 {
        (size.div_ceil(4), "i32")
    } else {
        (size.div_ceil(8), "i64")
    };
    ArgAbi::Coerce(format!("!llvm.array<{n} x {elem}>"))
}

fn aapcs32_ret(ty: &MirTy, target: &Target) -> RetAbi {
    let size = target.size_align(ty).0;
    if !is_aggregate(ty) || size == 0 {
        return RetAbi::Direct;
    }
    if let Some(h) = hfa(ty, target) {
        return RetAbi::Coerce(h);
    }
    match size {
        1 => RetAbi::Coerce("i8".to_owned()),
        2 => RetAbi::Coerce("i16".to_owned()),
        3 | 4 => RetAbi::Coerce("i32".to_owned()),
        _ => RetAbi::Sret,
    }
}

fn riscv(params: &[MirTy], ret: &MirTy, target: &Target) -> (RetAbi, Vec<ArgAbi>) {
    let mut gprs = 8;
    let mut fprs = 8;
    let ret = match riscv_arg(ret, target, &mut 2, &mut 2) {
        ArgAbi::Direct => RetAbi::Direct,
        ArgAbi::Coerce(c) => RetAbi::Coerce(c),
        ArgAbi::Indirect { .. } => {
            gprs -= 1;
            RetAbi::Sret
        }
    };
    let args = params
        .iter()
        .map(|p| riscv_arg(p, target, &mut gprs, &mut fprs))
        .collect();
    (ret, args)
}

/// One RISC-V LP64D argument (or, with two of each register, a result),
/// taking the registers it uses from `gprs` / `fprs` as clang counts them.
fn riscv_arg(ty: &MirTy, target: &Target, gprs: &mut u32, fprs: &mut u32) -> ArgAbi {
    let (size, align) = target.size_align(ty);
    if !is_aggregate(ty) {
        match ty {
            MirTy::Unit => {}
            MirTy::Prim(PrimTy::F32 | PrimTy::F64) if *fprs > 0 => *fprs -= 1,
            // C sees `str` as a `char*`.
            MirTy::Prim(PrimTy::Str) => *gprs = gprs.saturating_sub(1),
            _ => *gprs = gprs.saturating_sub(size.div_ceil(8)),
        }
        return ArgAbi::Direct;
    }
    if size == 0 {
        return ArgAbi::Direct;
    }
    if let Some((c, ints, floats)) = riscv_fp(ty, target) {
        if ints <= *gprs && floats <= *fprs {
            *gprs -= ints;
            *fprs -= floats;
            return ArgAbi::Coerce(c);
        }
    }
    match size {
        1..=8 => {
            *gprs = gprs.saturating_sub(1);
            ArgAbi::Coerce("i64".to_owned())
        }
        9..=16 => {
            *gprs = gprs.saturating_sub(2);
            let c = if align == 16 {
                "i128"
            } else {
                "!llvm.array<2 x i64>"
            };
            ArgAbi::Coerce(c.to_owned())
        }
        _ => {
            *gprs = gprs.saturating_sub(1);
            ArgAbi::Indirect { byval: false }
        }
    }
}

/// A struct the RISC-V float convention passes as fields (one float, two
/// floats, or a float and an integer ≤ 8 bytes, flattened; never a
/// union): its MLIR type and the integer and float registers it takes.
fn riscv_fp(ty: &MirTy, target: &Target) -> Option<(String, u32, u32)> {
    fn has_union(ty: &MirTy) -> bool {
        match ty {
            MirTy::Union(..) => true,
            MirTy::Tuple(ps) | MirTy::Struct(_, ps) => ps.iter().any(has_union),
            MirTy::Array(e, _) => has_union(e),
            _ => false,
        }
    }
    if has_union(ty) {
        return None;
    }
    let mut ls = Vec::new();
    leaves(ty, 0, target, &mut ls);
    let name = |&(_, size, leaf): &(u32, u32, Leaf)| match leaf {
        Leaf::F32 => "f32".to_owned(),
        Leaf::F64 => "f64".to_owned(),
        Leaf::Int => format!("i{}", size * 8),
    };
    let floats = u32::try_from(ls.iter().filter(|l| l.2 != Leaf::Int).count()).ok()?;
    match ls[..] {
        [one] if one.2 != Leaf::Int => Some((name(&one), 0, 1)),
        [a, b] if floats > 0 && a.1 <= 8 && b.1 <= 8 => {
            // Each field where the coerced struct puts it: naturally
            // aligned, or packed right after the first.
            let fields = format!("{}, {}", name(&a), name(&b));
            let c = if b.0 == (a.0 + a.1).next_multiple_of(b.1) && a.0 == 0 {
                format!("!llvm.struct<({fields})>")
            } else if b.0 == a.0 + a.1 && a.0 == 0 {
                format!("!llvm.struct<packed ({fields})>")
            } else {
                return None;
            };
            Some((c, 2 - floats, floats))
        }
        _ => None,
    }
}

/// System V `x86_64` eightbyte classes of a ≤ 16-byte aggregate: the MLIR
/// type of each eightbyte and whether it is SSE.
fn sysv_eightbytes(ty: &MirTy, target: &Target) -> Vec<(String, bool)> {
    let size = target.size_align(ty).0;
    let mut ls = Vec::new();
    leaves(ty, 0, target, &mut ls);
    let mut out = Vec::new();
    for start in [0u32, 8] {
        if start >= size {
            break;
        }
        let here: Vec<_> = ls
            .iter()
            .filter(|(o, _, _)| *o >= start && *o < start + 8)
            .collect();
        if here.is_empty() {
            // Padding-only eightbyte (NO_CLASS): still occupies the
            // slot in the coerced type to keep offsets.
            out.push(("i64".to_owned(), false));
            continue;
        }
        let sse = here.iter().all(|l| l.2 != Leaf::Int);
        let t = if sse {
            match here.as_slice() {
                [(_, _, Leaf::F64)] => "f64".to_owned(),
                [(_, _, Leaf::F32)] => "f32".to_owned(),
                _ => "vector<2xf32>".to_owned(),
            }
        } else {
            // Up to the last data byte (tail padding excluded), like clang.
            let end = here.iter().map(|l| l.0 + l.1).max().unwrap_or(start);
            format!("i{}", (end - start).min(8) * 8)
        };
        out.push((t, sse));
    }
    out
}

/// An unaligned field (`#[repr(packed)]`): System V MEMORY class.
fn unaligned(ty: &MirTy, target: &Target) -> bool {
    let mut ls = Vec::new();
    leaves(ty, 0, target, &mut ls);
    ls.iter().any(|(off, s, _)| off % s != 0)
}

fn sysv(params: &[MirTy], ret: &MirTy, target: &Target) -> (RetAbi, Vec<ArgAbi>) {
    let mut free_int = 6u32;
    let mut free_sse = 8u32;
    let ret_size = target.size_align(ret).0;
    let ret_abi = if !is_aggregate(ret) || ret_size == 0 {
        RetAbi::Direct
    } else if ret_size > 16 || unaligned(ret, target) {
        free_int -= 1;
        RetAbi::Sret
    } else {
        RetAbi::Coerce(join(&sysv_eightbytes(ret, target)))
    };
    let mut args = Vec::new();
    for t in params {
        if !is_aggregate(t) {
            match t {
                MirTy::Prim(PrimTy::F32 | PrimTy::F64) => free_sse = free_sse.saturating_sub(1),
                MirTy::Unit => {}
                _ => free_int = free_int.saturating_sub(1),
            }
            args.push(ArgAbi::Direct);
            continue;
        }
        let size = target.size_align(t).0;
        if size == 0 {
            args.push(ArgAbi::Direct);
            continue;
        }
        if size > 16 {
            args.push(ArgAbi::Indirect { byval: true });
            continue;
        }
        if unaligned(t, target) {
            args.push(ArgAbi::Indirect { byval: true });
            continue;
        }
        let ebs = sysv_eightbytes(t, target);
        let need_sse = u32::try_from(ebs.iter().filter(|e| e.1).count()).unwrap_or(2);
        let need_int = u32::try_from(ebs.len()).unwrap_or(2) - need_sse;
        if need_int > free_int || need_sse > free_sse {
            // Not enough registers for all of it: the whole aggregate
            // goes on the stack.
            args.push(ArgAbi::Indirect { byval: true });
            continue;
        }
        free_int -= need_int;
        free_sse -= need_sse;
        args.push(ArgAbi::Coerce(join(&ebs)));
    }
    (ret_abi, args)
}

fn join(ebs: &[(String, bool)]) -> String {
    match ebs {
        [(one, _)] => one.clone(),
        _ => format!(
            "!llvm.struct<({})>",
            ebs.iter()
                .map(|e| e.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(fields: &[PrimTy]) -> MirTy {
        MirTy::Tuple(fields.iter().map(|p| MirTy::Prim(*p)).collect())
    }
    use PrimTy::{F32, F64, I8, I32, I64};

    fn on(triple: &str) -> Target {
        Target::builtin(triple)
    }

    #[test]
    fn aapcs64() {
        let t = on("aarch64-unknown-linux-gnu");
        let a = |ty: &MirTy| aapcs_arg(ty, &t);
        let r = |ty: &MirTy| aapcs_ret(ty, &t);
        // div_t: one x register, both ways.
        assert_eq!(r(&st(&[I32, I32])), RetAbi::Coerce("i64".into()));
        assert_eq!(a(&st(&[I32, I32])), ArgAbi::Coerce("i64".into()));
        assert_eq!(r(&st(&[I8, I8, I8])), RetAbi::Coerce("i24".into()));
        assert_eq!(
            r(&st(&[I64, I32])),
            RetAbi::Coerce("!llvm.array<2 x i64>".into())
        );
        // HFA / HFA-like.
        assert_eq!(
            a(&st(&[F32, F32, F32])),
            ArgAbi::Coerce("!llvm.array<3 x f32>".into())
        );
        assert_eq!(
            r(&st(&[F64, F64, F64, F64])),
            RetAbi::Coerce("!llvm.array<4 x f64>".into())
        );
        // Mixed float kinds are not an HFA.
        assert_eq!(
            a(&st(&[F32, F64])),
            ArgAbi::Coerce("!llvm.array<2 x i64>".into())
        );
        // 5 floats: too many for an HFA, 20 bytes → memory.
        assert_eq!(a(&st(&[F32; 5])), ArgAbi::Indirect { byval: false });
        assert_eq!(r(&st(&[I64, I64, I64])), RetAbi::Sret);
        assert_eq!(a(&MirTy::Prim(I32)), ArgAbi::Direct);
    }

    #[test]
    fn sysv_x86_64() {
        let t = on("x86_64-unknown-linux-gnu");
        let f = |ps: &[MirTy], r: &MirTy| sysv(ps, r, &t);
        assert_eq!(f(&[], &st(&[I32, I32])).0, RetAbi::Coerce("i64".into()));
        assert_eq!(
            f(&[], &st(&[I64, F64])).0,
            RetAbi::Coerce("!llvm.struct<(i64, f64)>".into())
        );
        assert_eq!(
            f(&[], &st(&[F32, F32, F32])).0,
            RetAbi::Coerce("!llvm.struct<(vector<2xf32>, f32)>".into())
        );
        assert_eq!(
            f(&[], &st(&[I64, I32])).0,
            RetAbi::Coerce("!llvm.struct<(i64, i32)>".into())
        );
        assert_eq!(f(&[], &st(&[F32, I32])).0, RetAbi::Coerce("i64".into()));
        assert_eq!(f(&[], &st(&[I64, I64, I64])).0, RetAbi::Sret);
        assert_eq!(
            f(&[st(&[I64, I64, I64])], &MirTy::Unit).1,
            vec![ArgAbi::Indirect { byval: true }]
        );
        // Five ints use five GPRs; a two-INTEGER struct no longer fits
        // and goes to memory whole, while a one-eightbyte one still fits.
        let ints = vec![MirTy::Prim(I64); 5];
        let mut ps = ints.clone();
        ps.push(st(&[I64, I64]));
        assert_eq!(f(&ps, &MirTy::Unit).1[5], ArgAbi::Indirect { byval: true });
        let mut ps = ints;
        ps.push(st(&[I32, I32]));
        assert_eq!(f(&ps, &MirTy::Unit).1[5], ArgAbi::Coerce("i64".into()));
        // sret takes rdi.
        let mut ps = vec![MirTy::Prim(I64); 4];
        ps.push(st(&[I64, I64]));
        assert_eq!(
            f(&ps, &st(&[I64, I64, I64])).1[4],
            ArgAbi::Indirect { byval: true }
        );
    }

    #[test]
    fn win64_and_targets() {
        let w = classify(
            &on("x86_64-pc-windows-msvc"),
            &[st(&[I32, I32]), st(&[I32, I32, I32])],
            &st(&[I8; 3]),
            false,
        )
        .unwrap();
        assert_eq!(w.args[0], ArgAbi::Coerce("i64".into()));
        assert_eq!(w.args[1], ArgAbi::Indirect { byval: false });
        assert_eq!(w.ret, RetAbi::Sret);
        // riscv32 has no aggregate classification: aggregates are
        // refused, scalars pass as themselves.
        let rv = on("riscv32imac-unknown-none-elf");
        assert!(classify(&rv, &[st(&[I32, I32])], &MirTy::Unit, false).is_none());
        let scalars = classify(&rv, &[MirTy::Prim(I32)], &MirTy::Unit, true).unwrap();
        assert!(scalars.variadic);
    }

    /// i386: aggregates on the stack, results in memory but for Windows's
    /// register-sized ones (`div_t` in `edx:eax`, a 3-byte one not).
    #[test]
    fn x86_cdecl() {
        let lin = on("i686-unknown-linux-gnu");
        let win = on("i686-pc-windows-msvc");
        let ps = [st(&[I32, I32]), MirTy::Prim(I32)];
        for t in [&lin, &win] {
            let a = classify(t, &ps, &MirTy::Unit, false).unwrap();
            assert_eq!(a.args, [ArgAbi::Indirect { byval: true }, ArgAbi::Direct]);
            assert_eq!(byval_align(&st(&[I64, F64]), t), 4);
        }
        let r = |t: &Target, ty: &MirTy| classify(t, &[], ty, false).unwrap().ret;
        assert_eq!(r(&lin, &st(&[I32, I32])), RetAbi::Sret);
        assert_eq!(r(&win, &st(&[I32, I32])), RetAbi::Coerce("i64".into()));
        assert_eq!(r(&win, &st(&[F32])), RetAbi::Coerce("i32".into()));
        assert_eq!(r(&win, &st(&[I8; 3])), RetAbi::Sret);
        assert_eq!(r(&lin, &MirTy::Prim(I64)), RetAbi::Direct);
    }

    #[test]
    fn aapcs_vfp() {
        let t = on("armv7-unknown-linux-gnueabihf");
        let a = |ty: &MirTy| aapcs32_arg(ty, &t);
        let r = |ty: &MirTy| aapcs32_ret(ty, &t);
        assert_eq!(
            a(&st(&[F32, F32, F32])),
            ArgAbi::Coerce("!llvm.array<3 x f32>".into())
        );
        assert_eq!(
            r(&st(&[F64, F64])),
            RetAbi::Coerce("!llvm.array<2 x f64>".into())
        );
        assert_eq!(
            a(&st(&[I32, I8])),
            ArgAbi::Coerce("!llvm.array<2 x i32>".into())
        );
        // 8-aligned: whole even register pairs.
        assert_eq!(
            a(&st(&[I64, I32])),
            ArgAbi::Coerce("!llvm.array<2 x i64>".into())
        );
        assert_eq!(a(&st(&[I32; 17])), ArgAbi::Indirect { byval: true });
        assert_eq!(byval_align(&st(&[I64; 9]), &t), 8);
        assert_eq!(r(&st(&[I8, I8, I8])), RetAbi::Coerce("i32".into()));
        assert_eq!(r(&st(&[I32, I32])), RetAbi::Sret);
        let s = MirTy::Prim(PrimTy::Str);
        let rok = classify_rok(&t, &[s.clone()], &s).unwrap();
        assert_eq!(rok.args[0], ArgAbi::Coerce("!llvm.array<2 x i32>".into()));
        assert_eq!(rok.ret, RetAbi::Sret);
        // Arguments alone split as C lays them out.
        assert!(classify_rok(&t, &[s.clone(), s], &MirTy::Unit).is_none());
    }

    /// RISC-V LP64D: float structs in `fa` / `a` registers while both
    /// are left, integer ones as `i64` / `[2 x i64]`, bigger ones by
    /// reference; `sret` takes `a0`.
    #[test]
    fn riscv_lp64d() {
        let t = on("riscv64gc-unknown-linux-gnu");
        let c = |ps: &[MirTy], r: &MirTy| classify(&t, ps, r, false).unwrap();
        let co = |s: &str| ArgAbi::Coerce(s.into());
        let a = c(
            &[
                st(&[F32, F64]),
                st(&[I32, F32]),
                st(&[F64]),
                st(&[F32, F32, F32]),
                st(&[I64, I64, I64]),
            ],
            &st(&[F64, I64]),
        );
        assert_eq!(a.ret, RetAbi::Coerce("!llvm.struct<(f64, i64)>".into()));
        assert_eq!(
            a.args,
            [
                co("!llvm.struct<(f32, f64)>"),
                co("!llvm.struct<(i32, f32)>"),
                co("f64"),
                co("!llvm.array<2 x i64>"),
                ArgAbi::Indirect { byval: false },
            ]
        );
        // Two integers, or a union, are not a float struct.
        assert_eq!(c(&[st(&[I32, I32])], &MirTy::Unit).args[0], co("i64"));
        assert_eq!(
            c(
                &[MirTy::Union(Vec::new(), 1, rokc_mir::UnionKind::Plain)],
                &MirTy::Unit
            )
            .args[0],
            co("i64")
        );
        // Eight floats take every `fa`: the struct goes in `a` registers.
        let mut ps = vec![MirTy::Prim(F64); 8];
        ps.push(st(&[F32, F32]));
        assert_eq!(c(&ps, &MirTy::Unit).args[8], co("i64"));
        // An `sret` result takes `a0`: seven integers then leave one
        // register, too few for the float struct's integer half.
        let mut ps = vec![MirTy::Prim(I64); 7];
        ps.push(st(&[F32, I32]));
        assert_eq!(c(&ps, &MirTy::Unit).args[7], co("!llvm.struct<(f32, i32)>"));
        assert_eq!(c(&ps, &st(&[I64; 3])).args[7], co("i64"));
        let s = MirTy::Prim(PrimTy::Str);
        assert!(classify_rok(&t, &[s.clone(), s.clone()], &s).is_none());
    }

    /// A runtime `str` on Windows x64 is a 16-byte C struct: by
    /// reference, returned through `sret`; scalars alone stay plain.
    #[test]
    fn win64_runtime_strs_go_by_reference() {
        let t = on("x86_64-pc-windows-msvc");
        let s = MirTy::Prim(PrimTy::Str);
        let r = classify_rok(&t, &[MirTy::Prim(I32), s.clone()], &s).unwrap();
        assert_eq!(r.args[1], ArgAbi::Indirect { byval: false });
        assert_eq!(r.ret, RetAbi::Sret);
        assert!(r.rok);
        assert!(classify_rok(&t, &[MirTy::Prim(I64)], &MirTy::Prim(I32)).is_none());
        let arm = on("aarch64-pc-windows-msvc");
        assert!(classify_rok(&arm, &[MirTy::Prim(I32), s.clone()], &s).is_none());
    }

    /// A runtime aggregate other than a two-word pair follows C wherever
    /// the registers stand: 24 bytes go by reference on AAPCS64,
    /// `byval` on System V; a `fn` value pair splits like a `str`.
    #[test]
    fn runtime_aggregates_follow_c() {
        let big = MirTy::Tuple(vec![MirTy::Prim(I64); 3]);
        let a = classify_rok(&on("aarch64-apple-darwin"), &[big.clone()], &big).unwrap();
        assert_eq!(a.args[0], ArgAbi::Indirect { byval: false });
        assert_eq!(a.ret, RetAbi::Sret);
        let x = on("x86_64-unknown-linux-gnu");
        let r = classify_rok(&x, &[big], &MirTy::Unit).unwrap();
        assert_eq!(r.args[0], ArgAbi::Indirect { byval: true });
        let mixed = MirTy::Tuple(vec![MirTy::Prim(I32), MirTy::Prim(PrimTy::F32)]);
        let arm = on("armv7-unknown-linux-gnueabihf");
        let r = classify_rok(&arm, &[mixed], &MirTy::Unit).unwrap();
        assert_eq!(r.args[0], ArgAbi::Coerce("!llvm.array<2 x i32>".into()));
        let f = MirTy::Tuple(vec![MirTy::GcRef, MirTy::GcRef]);
        assert!(classify_rok(&x, &[f], &MirTy::Unit).is_none());
    }
}
