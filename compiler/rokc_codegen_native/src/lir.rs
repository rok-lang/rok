//! Low-level IR between MIR and machine code. Every value lives in a frame
//! slot; instructions load operands into scratch registers, compute and
//! store. ISA modules only pick instructions; everything else is decided
//! here once for all targets.

use std::collections::HashMap;

/// A frame slot (a local, or a temporary).
pub type Slot = u32;

/// A jump target within a function.
pub type Label = u32;

/// A symbol of the object being built (`obj::Obj::syms`).
pub type SymId = u32;

/// Scalar kind: the size of a value in memory and its register class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum K {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl K {
    #[must_use]
    pub const fn size(self) -> u32 {
        match self {
            Self::I8 => 1,
            Self::I16 => 2,
            Self::I32 | Self::F32 => 4,
            Self::I64 | Self::F64 => 8,
        }
    }

    #[must_use]
    pub const fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    #[must_use]
    pub const fn bits(self) -> u32 {
        self.size() * 8
    }
}

/// A memory location.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mem {
    /// Byte `off` of a frame slot.
    Frame(Slot, u32),
    /// Byte `off` past the pointer stored in a frame slot.
    Ind(Slot, i32),
}

impl Mem {
    #[must_use]
    pub const fn at(self, d: u32) -> Self {
        match self {
            Self::Frame(s, o) => Self::Frame(s, o + d),
            Self::Ind(s, o) => Self::Ind(s, o + d as i32),
        }
    }
}

/// A scalar operand: memory, or an immediate (a float's bits).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum V {
    M(Mem),
    I(i64),
}

/// Where a call argument / result / incoming parameter is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Loc {
    /// The `n`th integer argument / result register.
    Int(u8),
    /// The `n`th float argument / result register.
    Fp(u8),
    /// Byte offset in the outgoing (caller) / incoming (callee) argument
    /// area.
    Stack(u32),
    /// The hidden result pointer (`x8` on arm64).
    Sret,
}

/// A scalar crossing a call boundary: its location, kind, and whether a
/// narrow integer sign-extends (else zero-extends) into its register.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Part {
    pub loc: Loc,
    pub k: K,
    pub signed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bin {
    Add,
    Sub,
    Mul,
    /// Division / remainder; the divisor is known non-zero (and not
    /// `MIN / -1`).
    Div,
    Rem,
    And,
    Or,
    Xor,
    /// Shift amount already checked below the width.
    Shl,
    Shr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Un {
    Neg,
    Not,
    Sqrt,
    Popcnt,
    Clz,
    Ctz,
    Bswap,
}

/// Comparison predicate (integer signedness in `Cmp::signed`; floats are
/// the ordered predicates, `Ne` unordered).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cc {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A conversion between scalar kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Conv {
    /// Integer to integer: extend by `signed` from the source width, or
    /// truncate.
    Int { signed: bool },
    /// Integer to bool: `!= 0`.
    ToBool,
    /// Integer to float.
    IToF { signed: bool },
    /// Float to integer, saturating, NaN to 0; `max_bits` bits of the
    /// destination are the integer's (1 for `bool`).
    FToI { signed: bool, bits: u32 },
    /// Float to float.
    FToF,
    /// Bits reinterpreted (same size), int ↔ float.
    Bits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rmw {
    Xchg,
    Add,
    Sub,
    And,
    Or,
    Xor,
}

#[derive(Clone, Debug)]
pub enum Callee {
    Sym(SymId),
    /// A code pointer in memory.
    Ptr(Mem),
}

#[derive(Clone, Debug)]
pub enum Inst {
    Label(Label),
    Jmp(Label),
    /// To `t` when the `I8` value is non-zero, else to `f`.
    Br {
        c: V,
        t: Label,
        f: Label,
    },
    Mov {
        k: K,
        dst: Mem,
        src: V,
    },
    Copy {
        dst: Mem,
        src: Mem,
        size: u32,
    },
    Zero {
        dst: Mem,
        size: u32,
    },
    /// The address of `src` (an `I64`).
    Addr {
        dst: Mem,
        src: Mem,
    },
    /// The address of a symbol; `got`: through the GOT (may be in
    /// another image).
    Sym {
        dst: Mem,
        sym: SymId,
        got: bool,
    },
    /// The address of this thread's instance of a thread-local symbol.
    Tls {
        dst: Mem,
        sym: SymId,
    },
    Bin {
        op: Bin,
        k: K,
        signed: bool,
        dst: Mem,
        a: V,
        b: V,
    },
    /// `+ - *` that jumps to `fail` on overflow (of `k`, by `signed`).
    Ovf {
        op: Bin,
        k: K,
        signed: bool,
        dst: Mem,
        a: V,
        b: V,
        fail: Label,
    },
    Un {
        op: Un,
        k: K,
        dst: Mem,
        a: V,
    },
    /// `dst: I8 = a <cc> b`.
    Cmp {
        cc: Cc,
        k: K,
        signed: bool,
        dst: Mem,
        a: V,
        b: V,
    },
    Conv {
        conv: Conv,
        from: K,
        to: K,
        dst: Mem,
        a: V,
    },
    /// `ptr` holds the address. All atomics are `seq_cst`, which satisfies
    /// any ordering asked for.
    AtomicLoad {
        k: K,
        dst: Mem,
        ptr: Mem,
    },
    AtomicStore {
        k: K,
        ptr: Mem,
        src: V,
    },
    AtomicRmw {
        op: Rmw,
        k: K,
        dst: Mem,
        ptr: Mem,
        val: V,
    },
    /// `dst` = the old value; stores `new` when it was `expected`.
    Cas {
        k: K,
        dst: Mem,
        ptr: Mem,
        expected: V,
        new: V,
    },
    /// Load the arguments into place, call, store the results.
    /// `sret`: the hidden result pointer's memory (passed by address).
    Call {
        callee: Callee,
        args: Vec<(Part, V)>,
        rets: Vec<(Part, Mem)>,
        sret: Option<Mem>,
        /// A C variadic call (`x86_64`: `al` = the vector registers used).
        variadic: bool,
    },
    /// An incoming parameter (first, before anything else).
    Param {
        part: Part,
        dst: Mem,
    },
    /// Return, the results loaded into place.
    Ret {
        vals: Vec<(Part, V)>,
    },
    Trap,
    /// Source line, column and lexical scope of what follows (debug
    /// line info).
    Line(u32, u32, u32),
}

/// One function of LIR.
#[derive(Clone, Debug, Default)]
pub struct Func {
    pub sym: SymId,
    /// `(size, align)` of each slot.
    pub slots: Vec<(u32, u32)>,
    pub insts: Vec<Inst>,
    pub labels: u32,
    /// `-g`: what the debug info says about it.
    pub debug: Option<FnDebug>,
}

/// `-g` facts of a function.
#[derive(Clone, Debug)]
pub struct FnDebug {
    /// The rok path.
    pub name: String,
    /// `Span::file` (1-based; 0: the unit's file) and line of its
    /// declaration.
    pub file: u32,
    pub line: u32,
    /// Lexical scopes other than the body (0): `(id, parent)`.
    pub scopes: Vec<(u32, u32)>,
    pub vars: Vec<DebugVar>,
    /// `MirFunction::debug_ret`.
    pub ret: Option<rokc_ir_common::PrimTy>,
}

/// A named local in its stack slot.
#[derive(Clone, Debug)]
pub struct DebugVar {
    pub name: String,
    pub slot: Slot,
    pub ty: rokc_mir::DebugTy,
    pub line: u32,
    pub scope: u32,
    /// 1-based parameter position.
    pub arg: Option<usize>,
}

impl Func {
    pub fn slot(&mut self, size: u32, align: u32) -> Slot {
        self.slots.push((size.max(1), align.max(1)));
        self.slots.len() as Slot - 1
    }

    pub const fn label(&mut self) -> Label {
        self.labels += 1;
        self.labels - 1
    }

    /// Append `i`, folding arithmetic on two immediates or with an identity
    /// (`x + 0`, `x * 1`) into a move.
    pub fn push(&mut self, i: Inst) {
        if let Inst::Bin {
            op, k, dst, a, b, ..
        } = i
        {
            if !k.is_float() {
                let src = match (op, a, b) {
                    (Bin::Add, V::I(x), V::I(y)) => Some(V::I(x.wrapping_add(y))),
                    (Bin::Sub, V::I(x), V::I(y)) => Some(V::I(x.wrapping_sub(y))),
                    (Bin::Mul, V::I(x), V::I(y)) => Some(V::I(x.wrapping_mul(y))),
                    (Bin::And, V::I(x), V::I(y)) => Some(V::I(x & y)),
                    (Bin::Or, V::I(x), V::I(y)) => Some(V::I(x | y)),
                    (Bin::Xor, V::I(x), V::I(y)) => Some(V::I(x ^ y)),
                    (Bin::Add | Bin::Or | Bin::Xor, V::I(0), v)
                    | (
                        Bin::Add | Bin::Sub | Bin::Or | Bin::Xor | Bin::Shl | Bin::Shr,
                        v,
                        V::I(0),
                    )
                    | (Bin::Mul, V::I(1), v)
                    | (Bin::Mul, v, V::I(1)) => Some(v),
                    _ => None,
                };
                if let Some(src) = src {
                    self.insts.push(Inst::Mov { k, dst, src });
                    return;
                }
            }
        }
        self.insts.push(i);
    }
}

/// The integer kind `size` bytes make, if one does.
pub const fn scalar(size: u32) -> Option<K> {
    match size {
        1 => Some(K::I8),
        2 => Some(K::I16),
        4 => Some(K::I32),
        8 => Some(K::I64),
        _ => None,
    }
}

/// `(slot, offset)` → `(size, reads)` (`waitable`).
pub type Waits = HashMap<(u32, u32), (u32, u32)>;

/// How a slot is accessed.
#[derive(Default)]
struct Use {
    /// Reached by address, or accessed whole (a copy).
    opaque: bool,
    /// `(offset, size, reads, writes)` of its scalar parts, by offset.
    parts: std::collections::BTreeMap<u32, (u32, u32, u32, u32)>,
}

impl Use {
    /// The part at `off`, accessed as `size` bytes: one size per part,
    /// else the slot is opaque.
    fn part(&mut self, off: u32, size: u32) -> &mut (u32, u32, u32, u32) {
        let p = self.parts.entry(off).or_insert((off, size, 0, 0));
        self.opaque |= p.1 != size;
        p
    }
}

/// Scalar slot parts whose store may wait (`Pend`), `(slot, offset)` →
/// `(size, reads)`: written once, read at most once as the same bytes, in a
/// slot with non-overlapping parts, never addressed, copied whole or
/// described to the debugger. Also: which slots are addressed.
pub fn waitable(f: &Func) -> (Waits, Vec<bool>) {
    fn rd(u: &mut [Use], m: Mem, size: u32) {
        match m {
            Mem::Frame(s, o) => u[s as usize].part(o, size).2 += 1,
            // The pointer, at offset 0.
            Mem::Ind(s, _) => u[s as usize].part(0, 8).2 += 1,
        }
    }
    fn rv(u: &mut [Use], v: V, size: u32) {
        if let V::M(m) = v {
            rd(u, m, size);
        }
    }
    fn wr(u: &mut [Use], m: Mem, size: u32) {
        match m {
            Mem::Frame(s, o) => u[s as usize].part(o, size).3 += 1,
            Mem::Ind(..) => rd(u, m, 8),
        }
    }
    fn whole(u: &mut [Use], e: &mut [bool], m: Mem, escapes: bool) {
        match m {
            Mem::Frame(s, _) => {
                u[s as usize].opaque = true;
                e[s as usize] |= escapes;
            }
            Mem::Ind(..) => rd(u, m, 8),
        }
    }
    let n = f.slots.len();
    let mut u: Vec<Use> = (0..n).map(|_| Use::default()).collect();
    let mut e = vec![false; n];
    for i in &f.insts {
        match i {
            Inst::Label(_) | Inst::Jmp(_) | Inst::Trap | Inst::Line(..) => {}
            Inst::Br { c, .. } => rv(&mut u, *c, 1),
            Inst::Mov { k, dst, src } => {
                rv(&mut u, *src, k.size());
                wr(&mut u, *dst, k.size());
            }
            Inst::Copy { dst, src, size } => {
                if scalar(*size).is_some() {
                    rd(&mut u, *src, *size);
                    wr(&mut u, *dst, *size);
                } else {
                    whole(&mut u, &mut e, *src, false);
                    whole(&mut u, &mut e, *dst, false);
                }
            }
            Inst::Zero { dst, .. } => whole(&mut u, &mut e, *dst, false),
            Inst::Sym { dst, .. } | Inst::Tls { dst, .. } => wr(&mut u, *dst, 8),
            Inst::Param { part, dst } => wr(&mut u, *dst, part.k.size()),
            Inst::Addr { dst, src } => {
                whole(&mut u, &mut e, *src, true);
                wr(&mut u, *dst, 8);
            }
            Inst::Bin { k, dst, a, b, .. } | Inst::Ovf { k, dst, a, b, .. } => {
                rv(&mut u, *a, k.size());
                rv(&mut u, *b, k.size());
                wr(&mut u, *dst, k.size());
            }
            Inst::Cmp { k, dst, a, b, .. } => {
                rv(&mut u, *a, k.size());
                rv(&mut u, *b, k.size());
                wr(&mut u, *dst, 1);
            }
            Inst::Un { k, dst, a, .. } => {
                rv(&mut u, *a, k.size());
                wr(&mut u, *dst, k.size());
            }
            Inst::Conv {
                from, to, dst, a, ..
            } => {
                rv(&mut u, *a, from.size());
                wr(&mut u, *dst, to.size());
            }
            Inst::AtomicLoad { k, dst, ptr } => {
                whole(&mut u, &mut e, *ptr, true);
                wr(&mut u, *dst, k.size());
            }
            Inst::AtomicStore { k, ptr, src } => {
                whole(&mut u, &mut e, *ptr, true);
                rv(&mut u, *src, k.size());
            }
            Inst::AtomicRmw {
                k, dst, ptr, val, ..
            } => {
                whole(&mut u, &mut e, *ptr, true);
                rv(&mut u, *val, k.size());
                wr(&mut u, *dst, k.size());
            }
            Inst::Cas {
                k,
                dst,
                ptr,
                expected,
                new,
            } => {
                whole(&mut u, &mut e, *ptr, true);
                rv(&mut u, *expected, k.size());
                rv(&mut u, *new, k.size());
                wr(&mut u, *dst, k.size());
            }
            Inst::Call {
                callee,
                args,
                rets,
                sret,
                ..
            } => {
                if let Callee::Ptr(m) = callee {
                    rd(&mut u, *m, 8);
                }
                for (p, v) in args {
                    rv(&mut u, *v, p.k.size());
                }
                for (p, m) in rets {
                    wr(&mut u, *m, p.k.size());
                }
                if let Some(m) = sret {
                    whole(&mut u, &mut e, *m, true);
                }
            }
            Inst::Ret { vals } => {
                for (p, v) in vals {
                    rv(&mut u, *v, p.k.size());
                }
            }
        }
    }
    for v in f.debug.iter().flat_map(|d| &d.vars) {
        u[v.slot as usize].opaque = true;
    }
    let mut out = HashMap::new();
    for (s, use_) in u.iter().enumerate() {
        let ps: Vec<_> = use_.parts.values().collect();
        // Every part written; no two overlapping (by offset: no two
        // neighbours).
        let clean = !use_.opaque
            && ps.iter().all(|p| p.3 > 0)
            && ps.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0);
        if clean {
            for p in ps.iter().filter(|p| p.3 == 1 && p.2 <= 1) {
                out.insert((s as u32, p.0), (p.1, p.2));
            }
        }
    }
    (out, e)
}
