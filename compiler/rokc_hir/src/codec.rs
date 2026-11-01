//! `Codec` for HIR (the incremental body cache, spec 9.2.16).

use rokc_core::codec::{Codec, Dec, Enc, IdKind};
use rokc_core::{codec_enum, codec_struct};

use crate::{
    EnumId, FnBounds, HirBlock, HirEnumDef, HirEnumVariant, HirExpr, HirExprKind, HirField,
    HirFunction, HirLocal, HirMatchArm, HirParam, HirPattern, HirStaticDef, HirStmt, HirStructDef,
    HirTy, HirUnionKind, NicheArg, NicheVal, Repr, StructId, TraitRef, UnionStorage,
};

macro_rules! codec_id {
    ($($t:ident $k:ident),*) => {$(
        impl Codec for $t {
            fn enc(&self, e: &mut Enc) {
                e.id(IdKind::$k, self.0);
            }
            fn dec(d: &mut Dec<'_>) -> Option<Self> {
                d.id(IdKind::$k).map(Self)
            }
        }
    )*};
}
codec_id!(StructId Struct, EnumId Enum);

codec_struct!(HirEnumDef {
    id,
    repr,
    def_id,
    name,
    variants,
    span,
    generic_args
});
codec_struct!(HirEnumVariant {
    name,
    tag,
    payload_ty,
    named_fields,
    arity
});
codec_struct!(HirStructDef {
    id,
    repr,
    def_id,
    name,
    fields,
    span,
    generic_args,
    union_kind,
    niche,
});
codec_struct!(Repr {
    c,
    transparent,
    packed,
    align,
    int
});
codec_enum!(NicheVal {
    Int(x0),
    Min,
    Max,
    Nan,
    Inf,
    NegInf
});
codec_enum!(NicheArg {
    Val(x0),
    Range(x0, x1)
});
codec_enum!(UnionStorage {
    WidestPrim(x0),
    ByteArray
});
codec_struct!(HirUnionKind {
    storage,
    aliases,
    is_repr_c
});
codec_struct!(HirField { name, ty, default });
codec_struct!(HirFunction {
    id,
    def_id,
    name,
    params,
    locals,
    ret_ty,
    body,
    is_async,
    is_generator,
    span,
    facts,
});
codec_struct!(HirParam {
    name,
    local,
    ty,
    by_ref
});
codec_struct!(HirLocal {
    id,
    name,
    ty,
    boxed
});
codec_struct!(HirBlock {
    stmts,
    trailing,
    label
});
codec_enum!(HirStmt {
    Let { local, init },
    Assign { local, value },
    ArrayAssign {
        array_local,
        path,
        index,
        value,
        elem_ty
    },
    StaticAssign { name, value },
    Expr(x0),
    Return(x0),
    Break { label, value },
    Continue { label },
    Panic(x0),
});
codec_struct!(HirExpr { kind, ty, span });
codec_enum!(HirExprKind {
    Const(x0),
    Local(x0),
    Block(x0),
    StaticRef(x0),
    UnresolvedPath(x0),
    PrimBinOp { op, lhs, rhs },
    PrimUnaryOp { op, operand },
    Call { callee, args },
    FnRef(x0),
    CallIndirect {
        callee,
        args,
        ret_ty
    },
    Cast {
        operand,
        from_ty,
        to_ty
    },
    ClosureValue {
        fn_id,
        env,
        env_ty,
        arg_tys,
        ret_ty
    },
    CallClosure {
        closure,
        args,
        arg_tys,
        ret_ty,
        env_ty
    },
    If {
        cond,
        then_block,
        else_branch
    },
    While { label, cond, body },
    Loop {
        label,
        body,
        result_ty
    },
    Match { scrutinee, arms },
    StructInit { struct_id, fields },
    FieldRead {
        target,
        struct_id,
        field_index,
        field_ty
    },
    WithField {
        target,
        struct_id,
        field_index,
        value
    },
    EnumVariantInit {
        enum_id,
        variant_index,
        args
    },
    SumWrap {
        sum_ty,
        variant_index,
        value
    },
    Tuple { fields },
    TupleField {
        target,
        index,
        field_ty
    },
    ArrayInit { elem_ty, elements },
    ArrayRepeat {
        elem_ty,
        value,
        len
    },
    BufNew { elem, len },
    RawRead {
        addr,
        ty,
        volatile
    },
    RawWrite {
        addr,
        value,
        ty,
        volatile
    },
    AddrOf { place },
    SizeOf { ty, align },
    BufGet { buf, index, elem },
    BufSet {
        buf,
        index,
        value,
        elem
    },
    BufAtomic {
        op,
        buf,
        index,
        args,
        elem,
        order
    },
    ArrayIndex {
        array,
        index,
        elem_ty
    },
    UnionWrite {
        struct_id,
        from_ty,
        storage_ty,
        value
    },
    UnionRead {
        struct_id,
        target,
        field_name,
        storage_ty,
        field_ty
    },
    Suspend { value, is_final },
    CoroNew { body_fn, args },
});
codec_struct!(HirMatchArm { pat, guard, body });
codec_enum!(HirPattern {
    Wildcard,
    Binding { local, ty },
    At { local, ty, sub },
    Literal { value, prim },
    Or(x0),
    Tuple(x0),
    Struct { struct_id, fields },
    Range {
        start,
        end,
        inclusive,
        prim
    },
    EnumVariant {
        enum_id,
        variant_index,
        args
    },
    SumVariant {
        sum_ty,
        variant_index,
        binder,
        payload_ty
    },
    SumMember {
        sum_ty,
        variant_index,
        payload_ty,
        sub
    },
});
codec_struct!(TraitRef { path, args, binds });
codec_struct!(FnBounds {
    auto,
    unsafe_fn,
    c_abi,
    stdcall
});
impl Codec for HirTy {
    fn enc(&self, e: &mut Enc) {
        match self {
            Self::Unknown => e.u(0),
            Self::Unit => e.u(1),
            Self::Prim(p) => {
                e.u(2);
                p.enc(e);
            }
            Self::Any => e.u(3),
            Self::AnyTrait(b) => {
                e.u(4);
                b.enc(e);
            }
            Self::Path(p) => {
                e.u(5);
                p.enc(e);
            }
            Self::None => e.u(6),
            Self::Const(v, t) => {
                e.u(7);
                v.enc(e);
                t.enc(e);
            }
            Self::OpenConst(n) => {
                e.u(8);
                n.enc(e);
            }
            Self::NoThrow => e.u(9),
            Self::Var(v) => {
                e.u(10);
                e.id(IdKind::InferVar, *v);
            }
            Self::Sum(ts) => {
                e.u(11);
                ts.enc(e);
            }
            Self::Tuple(ts) => {
                e.u(12);
                ts.enc(e);
            }
            Self::Struct(id) => {
                e.u(13);
                id.enc(e);
            }
            Self::Enum(id) => {
                e.u(14);
                id.enc(e);
            }
            Self::Array(t, n) => {
                e.u(15);
                t.enc(e);
                n.enc(e);
            }
            Self::Fn(ps, r, b) => {
                e.u(16);
                ps.enc(e);
                r.enc(e);
                b.enc(e);
            }
            Self::Closure {
                args,
                ret,
                env,
                shares,
            } => {
                e.u(17);
                args.enc(e);
                ret.enc(e);
                env.enc(e);
                shares.enc(e);
            }
            Self::Throws { ok, err } => {
                e.u(18);
                ok.enc(e);
                err.enc(e);
            }
            Self::GcBuf(t) => {
                e.u(19);
                t.enc(e);
            }
            Self::Never => e.u(20),
            Self::Param(n) => {
                e.u(21);
                n.enc(e);
            }
        }
    }

    fn dec(d: &mut Dec<'_>) -> Option<Self> {
        Some(match d.u()? {
            0 => Self::Unknown,
            1 => Self::Unit,
            2 => Self::Prim(Codec::dec(d)?),
            3 => Self::Any,
            4 => Self::AnyTrait(Codec::dec(d)?),
            5 => Self::Path(Codec::dec(d)?),
            6 => Self::None,
            7 => Self::Const(Codec::dec(d)?, Codec::dec(d)?),
            8 => Self::OpenConst(Codec::dec(d)?),
            9 => Self::NoThrow,
            10 => Self::Var(d.id(IdKind::InferVar)?),
            11 => Self::Sum(Codec::dec(d)?),
            12 => Self::Tuple(Codec::dec(d)?),
            13 => Self::Struct(Codec::dec(d)?),
            14 => Self::Enum(Codec::dec(d)?),
            15 => Self::Array(Codec::dec(d)?, Codec::dec(d)?),
            16 => Self::Fn(Codec::dec(d)?, Codec::dec(d)?, Codec::dec(d)?),
            17 => Self::Closure {
                args: Codec::dec(d)?,
                ret: Codec::dec(d)?,
                env: Codec::dec(d)?,
                shares: Codec::dec(d)?,
            },
            18 => Self::Throws {
                ok: Codec::dec(d)?,
                err: Codec::dec(d)?,
            },
            19 => Self::GcBuf(Codec::dec(d)?),
            20 => Self::Never,
            21 => Self::Param(Codec::dec(d)?),
            _ => return None,
        })
    }
}
codec_struct!(HirStaticDef {
    name,
    ty,
    init,
    is_mut,
    is_extern,
    dll_import,
    init_fn,
    span
});
