//! `Codec` for the AST (the parse cache, and the position-free
//! fingerprints of the incremental HIR cache, spec 9.2.16).

use rokc_core::{codec_enum, codec_struct};

use crate::{
    AccessTail, AttrArg, AttrBody, Attribute, Block, CallArg, CaptureSpec, CatchArm, ConstDef,
    ConstExpr, EnumDef, EnumVariant, EnumVariantKind, Expr, ExprKind, ExternBlock, ExternItem,
    FStringPart, FieldPattern, FnDef, FnTyBounds, GenericParam, Generics, ImplDef, ImplItem, Item,
    LitArg, LitCand, Literal, MatchArm, ModDef, NamedField, Param, Path, PathSegment, Pattern,
    PatternKind, Program, QPath, SpecPiece, StaticDef, Stmt, StmtKind, StructDef, StructKind,
    ThrowsKind, TraitDef, TraitItem, TryKind, TupleField, Ty, TyKind, TypeAliasDef, UnionDef,
    UseDef, UseTree, UseTreeKind, WherePred, WitRef,
};

codec_struct!(Program {
    attrs,
    items,
    stmts,
    expanded,
    tops
});
codec_struct!(Attribute {
    path,
    body,
    inner,
    span
});
codec_enum!(AttrBody {
    Empty,
    Args(x0),
    Value(x0)
});
codec_enum!(AttrArg {
    Lit(x0),
    Word(x0),
    KeyValue(x0, x1),
    Nested { path, body },
    Range { lo, hi, inclusive },
});
codec_enum!(Literal {
    Int(x0),
    Float(x0),
    Bool(x0),
    Str(x0),
    RawStr(x0),
    ByteStr(x0),
    Char(x0),
    CChar(x0),
    None,
});
codec_enum!(Item {
    Fn(x0),
    Struct(x0),
    Enum(x0),
    Union(x0),
    Trait(x0),
    Impl(x0),
    Const(x0),
    Static(x0),
    TypeAlias(x0),
    Mod(x0),
    Use(x0),
    Extern(x0),
});
codec_struct!(FnDef {
    vis,
    attrs,
    name,
    generics,
    params,
    ret_ty,
    body,
    is_async,
    is_generator,
    is_unsafe,
    throws,
    where_clause,
    span,
});
codec_enum!(ThrowsKind {
    None,
    Untyped,
    Typed(x0)
});
codec_struct!(Generics { params });
codec_enum!(GenericParam {
    Type {
        name,
        bounds,
        default
    },
    Const { name, ty },
});
codec_struct!(WherePred { ty, bounds });
codec_struct!(Param {
    name,
    ty,
    by_ref,
    default,
    variadic,
    span
});
codec_struct!(Block {
    stmts,
    trailing,
    span,
    label
});
codec_struct!(Stmt { kind, span });
codec_enum!(StmtKind {
    Let { name, ty, init },
    LetTuple {
        names,
        rest,
        ty,
        init
    },
    LetStruct {
        path,
        fields,
        rest,
        ty,
        init
    },
    LetPattern { pat, ty, init },
    Assign(x0, x1),
    CompoundAssign { target, op, value },
    Item(x0),
    Expr(x0),
    Trailing(x0),
    Return(x0),
    Break { label, value },
    Continue { label },
    Throw(x0),
});
codec_struct!(LitCand {
    handler,
    args,
    priority,
    reg
});
codec_enum!(LitArg {
    One(x0),
    Many(x0)
});
codec_struct!(Expr { kind, span });
codec_struct!(QPath {
    self_ty,
    trait_path,
    item,
    member
});
codec_enum!(ExprKind {
    Literal(x0),
    Path(x0),
    QPath(x0),
    SelfLower,
    SelfUpper,
    Tuple(x0),
    List(x0),
    ArrayRepeat { value, len },
    Dict(x0),
    Set(x0),
    MagicLiteral(x0),
    Block(x0),
    Unsafe(x0),
    If {
        cond,
        then_branch,
        else_branch
    },
    Match { scrutinee, arms },
    Loop { label, body },
    While { label, cond, body },
    For {
        label,
        pat,
        iter,
        body,
        is_await
    },
    Try { kind, operand },
    Catch {
        body,
        catches,
        finally
    },
    OpInfix { op, lhs, rhs },
    OpPrefix { op, operand },
    OpPostfix { op, operand },
    Cast(x0, x1),
    Is(x0, x1),
    Field(x0, x1),
    TupleField(x0, x1),
    MethodCall {
        receiver,
        method,
        type_args,
        args,
        trailing_closure,
        origin
    },
    Call {
        callee,
        args,
        trailing_closure
    },
    Index(x0, x1),
    Slice {
        target,
        start,
        end,
        step
    },
    Range {
        start,
        end,
        inclusive
    },
    QuestionDot(x0, x1),
    NullishCoalesce(x0, x1),
    TryQ(x0),
    Closure {
        captures,
        params,
        ret_ty,
        body,
        is_async,
        is_generator
    },
    Await(x0),
    InModule(x0, x1),
    Yield(x0),
    AsyncBlock(x0),
    YieldBlock(x0),
    AsyncYieldBlock(x0),
    StructInit {
        path,
        fields,
        spread
    },
    Broadcast { callee, args },
    FString(x0),
});
codec_enum!(FStringPart {
    Lit(x0),
    Hole { expr, spec }
});
codec_enum!(SpecPiece {
    Text(x0),
    Expr(x0)
});
codec_enum!(AccessTail {
    Field(x0),
    TupleField(x0),
    Method {
        method,
        type_args,
        args
    },
    Index(x0),
});
codec_enum!(CallArg {
    Positional(x0),
    Named { name, value },
    Spread(x0),
    Ref(x0),
});
codec_enum!(CaptureSpec {
    Value(x0),
    Ref(x0)
});
codec_struct!(MatchArm { pat, guard, body });
codec_enum!(TryKind {
    Plain,
    Optional,
    Force
});
codec_struct!(CatchArm { pat, body });
codec_struct!(Pattern { kind, span });
codec_enum!(PatternKind {
    Wildcard,
    Binding { name, sub },
    Literal(x0),
    Range {
        start,
        end,
        inclusive
    },
    TypeCheck { binder, ty },
    Tuple(x0),
    Rest(x0),
    Struct { path, fields, rest },
    EnumVariant { path, args },
    Or(x0),
});
codec_struct!(FieldPattern { name, pat });
codec_struct!(Path {
    segments,
    span,
    origin
});
codec_struct!(PathSegment { name, generics });
codec_struct!(Ty { kind, span });
codec_struct!(FnTyBounds {
    auto,
    unsafe_fn,
    c_abi,
    system
});
codec_enum!(TyKind {
    Path(x0),
    QPath(x0),
    Sum(x0),
    Tuple(x0),
    Fn(x0, x1, x2),
    Nest(x0),
    Binding(x0, x1),
    Throws { ok, err },
    Array(x0, x1),
    Any,
    AnyTrait(x0),
    None,
    SelfUpper,
    Never,
    Const(x0),
});
codec_enum!(ConstExpr {
    Literal(x0),
    Path(x0),
    Expr(x0)
});
codec_struct!(StructDef {
    vis,
    attrs,
    name,
    generics,
    kind,
    where_clause,
    span
});
codec_enum!(StructKind {
    Unit,
    Tuple(x0),
    Named(x0)
});
codec_struct!(NamedField {
    vis,
    attrs,
    name,
    ty,
    default,
    span
});
codec_struct!(TupleField {
    vis,
    attrs,
    ty,
    span
});
codec_struct!(EnumDef {
    vis,
    attrs,
    name,
    generics,
    variants,
    where_clause,
    span
});
codec_struct!(EnumVariant {
    attrs,
    name,
    kind,
    span
});
codec_enum!(EnumVariantKind {
    Unit,
    Tuple(x0),
    Named(x0),
    Value(x0)
});
codec_struct!(UnionDef {
    vis,
    attrs,
    name,
    generics,
    fields,
    where_clause,
    span
});
codec_struct!(TraitDef {
    vis,
    attrs,
    name,
    generics,
    supertraits,
    items,
    where_clause,
    span
});
codec_enum!(TraitItem {
    Method(x0),
    AssocType {
        name,
        bounds,
        default
    },
    Const { name, ty, default },
});
codec_struct!(ImplDef {
    vis,
    attrs,
    generics,
    trait_path,
    is_unsafe,
    negative,
    self_ty,
    where_clause,
    items,
    module,
    span,
});
codec_enum!(ImplItem {
    Method(x0),
    AssocType { name, ty },
    Const(x0),
});
codec_struct!(ConstDef {
    vis,
    attrs,
    name,
    ty,
    value,
    span
});
codec_struct!(StaticDef {
    vis,
    attrs,
    name,
    ty,
    value,
    is_mut,
    span
});
codec_struct!(TypeAliasDef {
    vis,
    attrs,
    name,
    generics,
    ty,
    where_clause,
    span
});
codec_struct!(ModDef {
    vis,
    attrs,
    name,
    items,
    span
});
codec_struct!(UseDef {
    vis,
    attrs,
    tree,
    span
});
codec_struct!(UseTree { prefix, kind });
codec_enum!(UseTreeKind {
    Simple(x0),
    Glob,
    GlobRecursive,
    Group(x0),
});
codec_struct!(ExternBlock {
    attrs,
    abi,
    items,
    span,
    wit
});
codec_struct!(WitRef { export, paths });
codec_enum!(ExternItem {
    Fn(x0),
    Static(x0)
});
