//! Shared IR primitives used by HIR and MIR.

use rokc_core::DefId;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlockId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LocalId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FunctionId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PrimTy {
    I8,
    I16,
    I32,
    I64,
    Isize,
    U8,
    U16,
    U32,
    U64,
    Usize,
    F32,
    F64,
    Bool,
    /// C `char` (FFI; `c'A'`).
    CChar,
    /// Unicode scalar value (`'가'`); 32-bit code point.
    Char,
    /// stdlib `str` (UTF-8 view, primitive).
    Str,
}

impl PrimTy {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::Isize => "isize",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::Usize => "usize",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::CChar => "c_char",
            Self::Char => "char",
            Self::Str => "str",
        }
    }

    #[must_use]
    pub const fn is_signed_int(self) -> bool {
        matches!(
            self,
            Self::I8 | Self::I16 | Self::I32 | Self::I64 | Self::Isize
        )
    }

    #[must_use]
    pub const fn is_unsigned_int(self) -> bool {
        matches!(
            self,
            Self::U8 | Self::U16 | Self::U32 | Self::U64 | Self::Usize
        )
    }

    #[must_use]
    pub const fn is_int(self) -> bool {
        self.is_signed_int() || self.is_unsigned_int() || matches!(self, Self::CChar | Self::Char)
    }

    #[must_use]
    pub const fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    /// Bit width given the target pointer width (32 or 64).
    /// Returns 0 for `Str` since it lowers to (ptr, len).
    #[must_use]
    pub const fn bit_width(self, pointer_width: u32) -> u32 {
        match self {
            Self::I8 | Self::U8 | Self::Bool | Self::CChar => 8,
            Self::I16 | Self::U16 => 16,
            Self::I32 | Self::U32 | Self::F32 | Self::Char => 32,
            Self::I64 | Self::U64 | Self::F64 => 64,
            Self::Isize | Self::Usize => pointer_width,
            Self::Str => 0,
        }
    }
}

/// Primitive op after stdlib-trait desugaring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// Floored `%%` (`rok.mod.TYPE`): the result takes the divisor's sign.
    /// MIR expands it, so backends never see it.
    Mod,
    Neg,
    Not,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LogicalAnd,
    LogicalOr,
    LogicalNot,
    /// Square root (floats; `rok.sqrt.TYPE`, `√`).
    Sqrt,
    /// Integer `+ - *` that wrap even where overflow checks are on
    /// (`rok.wrapping_add.TYPE` ..; the `wrapping_*` methods).
    WrappingAdd,
    WrappingSub,
    WrappingMul,
    /// Bit counts / byte swap on integers, same type in and out
    /// (`rok.ctpop` / `rok.ctlz` / `rok.cttz` / `rok.bswap`).
    CountOnes,
    LeadingZeros,
    TrailingZeros,
    SwapBytes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstVal {
    Unit,
    Bool(bool),
    Int {
        value: i128,
        ty: PrimTy,
    },
    UInt {
        value: u128,
        ty: PrimTy,
    },
    Float {
        bits: u64,
        ty: PrimTy,
    },
    Char(u32),
    Str(String),
    /// A `[u8; N]` (`b".."`, `include_bytes!`). MIR lowering expands it
    /// to its elements; it reaches codegen only as a static's value.
    Bytes(Vec<u8>),
    None,
}

/// Calling convention of a function.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Abi {
    /// rok's own: `str` is `(ptr, len)` by value, a `fn` value
    /// `{code, env}`. Every rok function and `extern "rok"` declaration.
    #[default]
    Rok,
    /// Platform C ABI: `str` is a NUL-terminated `char*`, `fn`
    /// a bare function pointer, aggregates classified. `system`:
    /// `stdcall` on 32-bit Windows, C elsewhere.
    C { variadic: bool, system: bool },
}

/// Where a function's code comes from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Linkage {
    /// Its body is compiled here.
    #[default]
    Defined,
    /// A declaration of an outside symbol, under its own name.
    Extern,
    /// A declaration of a prebuilt dependency's function:
    /// mangled, in the dependency's archive.
    Prebuilt,
}

/// `#[inline]` / `#[inline(always)]` / `#[inline(never)]`,
/// or `#[cold]`: never inlined, laid out away from hot code.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Inline {
    #[default]
    Auto,
    Hint,
    Always,
    Never,
    Cold,
}

/// A function the back end treats specially.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FnRole {
    #[default]
    Plain,
    /// The user's `main`: the C `main` wraps it.
    Main,
    /// An `any Tr` method dispatcher: weak in a prebuilt library, so the
    /// program's complete one replaces it.
    AnyDispatcher,
}

/// Per-function facts HIR decides and MIR / codegen only read.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FnFacts {
    pub abi: Abi,
    pub linkage: Linkage,
    /// `#[export_name("sym")]`.
    pub export_name: Option<String>,
    /// wasm32 core import `(module, name)`: `#[link(wasm_import_module)]`
    /// and `#[link_name]` or the own name.
    pub wasm_import: Option<(String, String)>,
    /// `#[link_name = "sym"]` of an extern function outside a
    /// `wasm_import_module` block: the symbol it links to.
    pub link_name: Option<String>,
    pub role: FnRole,
    /// A generic instance's mangled type arguments (905.2); the name ends
    /// in `MONO_MARK` + these.
    pub mono_args: Option<String>,
    /// `#[track_caller]`: the last parameter is the caller's location
    /// (`str`, `path:line:col`), which its panics report.
    pub track_caller: bool,
    /// Declared in a `#[no_safepoint] extern "rok"` block: neither
    /// allocates nor blocks, so a call to it is no safepoint.
    pub no_safepoint: bool,
    pub inline: Inline,
}

/// Separator between a generic function's path and its instance's type
/// arguments in the instance's name.
pub const MONO_MARK: &str = "__mono__";

impl FnFacts {
    /// The item path of a function named `name` (an instance's generic).
    #[must_use]
    pub fn path<'a>(&self, name: &'a str) -> &'a str {
        self.mono_args
            .as_ref()
            .and_then(|a| name.strip_suffix(a.as_str()))
            .and_then(|n| n.strip_suffix(MONO_MARK))
            .unwrap_or(name)
    }

    /// Whether the body takes the caller's `rok_tls` address as a hidden
    /// last argument. All but the `any` dispatcher and
    /// `#[export_name]` functions, which are entered from outside.
    #[must_use]
    pub fn takes_tls(&self) -> bool {
        self.linkage != Linkage::Extern
            && self.role != FnRole::AnyDispatcher
            && !(self.abi == Abi::Rok && self.export_name.is_some())
    }
}

/// Callee form after name resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedFn {
    /// Statically resolved.
    Free(DefId),
    /// any-slot or sum-type narrowing path; resolved at runtime.
    Dynamic(String),
}

macro_rules! codec_id {
    ($($t:ident),*) => {$(
        impl rokc_core::codec::Codec for $t {
            fn enc(&self, e: &mut rokc_core::codec::Enc) {
                e.u(u64::from(self.0));
            }
            fn dec(d: &mut rokc_core::codec::Dec<'_>) -> Option<Self> {
                Some(Self(u32::try_from(d.u()?).ok()?))
            }
        }
    )*};
}
codec_id!(BlockId, LocalId);

impl rokc_core::codec::Codec for FunctionId {
    fn enc(&self, e: &mut rokc_core::codec::Enc) {
        e.id(rokc_core::codec::IdKind::Function, self.0);
    }
    fn dec(d: &mut rokc_core::codec::Dec<'_>) -> Option<Self> {
        d.id(rokc_core::codec::IdKind::Function).map(Self)
    }
}

rokc_core::codec_enum!(PrimTy {
    I8,
    I16,
    I32,
    I64,
    Isize,
    U8,
    U16,
    U32,
    U64,
    Usize,
    F32,
    F64,
    Bool,
    CChar,
    Char,
    Str,
});
rokc_core::codec_enum!(PrimOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Neg,
    Not,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LogicalAnd,
    LogicalOr,
    LogicalNot,
    Sqrt,
    WrappingAdd,
    WrappingSub,
    WrappingMul,
    CountOnes,
    LeadingZeros,
    TrailingZeros,
    SwapBytes,
    Mod,
});
rokc_core::codec_enum!(ConstVal {
    Unit,
    Bool(x0),
    Int { value, ty },
    UInt { value, ty },
    Float { bits, ty },
    Char(x0),
    Str(x0),
    Bytes(x0),
    None,
});
rokc_core::codec_enum!(Abi {
    Rok,
    C { variadic, system }
});
rokc_core::codec_enum!(Linkage {
    Defined,
    Extern,
    Prebuilt
});
rokc_core::codec_enum!(FnRole {
    Plain,
    Main,
    AnyDispatcher
});
rokc_core::codec_enum!(Inline {
    Auto,
    Hint,
    Always,
    Never,
    Cold
});
rokc_core::codec_struct!(FnFacts {
    abi,
    linkage,
    export_name,
    wasm_import,
    link_name,
    role,
    mono_args,
    track_caller,
    no_safepoint,
    inline,
});
