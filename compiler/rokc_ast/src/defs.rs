//! The AST. Operators are kept as their lexical symbol; trait resolution
//! happens later.

use rokc_core::Visibility;
use rokc_diagnostic::Span;

use crate::{AttrArg, AttrBody, Attribute, Item, Literal};

#[derive(Clone, Debug)]
pub struct FnDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub generics: Generics,
    pub params: Vec<Param>,
    pub ret_ty: Option<Ty>,
    /// Read when first used where a decode keeps its bytes (a library
    /// stage's items).
    pub body: Option<rokc_core::codec::Lazy<Block>>,
    pub is_async: bool,
    pub is_generator: bool,
    /// `unsafe fn` (spec 4.15.1): callers need an unsafe scope.
    pub is_unsafe: bool,
    pub throws: ThrowsKind,
    pub where_clause: Vec<WherePred>,
    pub span: Span,
}

impl FnDef {
    /// A copy without the body and attributes, private.
    #[must_use]
    pub fn signature(&self) -> Self {
        Self {
            vis: Visibility::Private,
            attrs: Vec::new(),
            name: self.name.clone(),
            generics: self.generics.clone(),
            params: self.params.clone(),
            ret_ty: self.ret_ty.clone(),
            body: None,
            is_async: self.is_async,
            is_generator: self.is_generator,
            is_unsafe: self.is_unsafe,
            throws: self.throws.clone(),
            where_clause: self.where_clause.clone(),
            span: self.span,
        }
    }
}

/// Marker attribute of an `async fn` rewritten into a plain fn returning
/// its coroutine (`rokc_driver::coro`): each call makes a fresh future.
pub const CORO_FN_ATTR: &str = "#coro_fn";

/// Marker attribute of what `#[cfg]` dropped, for not-found errors.
///
/// `[CFG_OFF_ATTR, kind..](name, pred, ..)`: on an `impl`, `["method"]`
/// for each of its dropped methods; on an empty `extern` block standing
/// in for a dropped item, none (an item), `["mod"]` or `["method", type]`.
pub const CFG_OFF_ATTR: &str = "#cfg_off";

/// Marker attribute of an `extern "ABI" fn f(..) { .. }` definition
/// (spec 5.9.10), its value the ABI string.
pub const EXTERN_FN_ATTR: &str = "#extern_fn";

/// The ABI of an `extern "ABI" fn` definition with attributes `attrs`.
#[must_use]
pub fn extern_fn_abi(attrs: &[crate::Attribute]) -> Option<&str> {
    attrs.iter().find_map(|a| match &a.body {
        crate::AttrBody::Value(crate::Literal::Str(abi)) if a.path == [EXTERN_FN_ATTR] => {
            Some(abi.as_str())
        }
        _ => None,
    })
}

/// `#[inline]` / `#[inline(always)]` (not `never`): a prebuilt package's
/// function so marked is copied into its users.
#[must_use]
pub fn inline_wanted(attrs: &[crate::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.name() == "inline"
            && !matches!(&a.body, crate::AttrBody::Args(xs)
                if matches!(xs.as_slice(), [crate::AttrArg::Word(w)] if w == "never"))
    })
}

/// Marks a nested `fn` statement (spec 4.10), lifted to a module item
/// named by the string (`outer/f`); a second `true` marks a sibling
/// visible from an enclosing block.
pub const NESTED_FN_ATTR: &str = "#nested_fn";

/// Marks an item lifted from a block (spec 3.9.1); the string is the
/// enclosing fn's lifted path. Uses in scope are renamed to
/// `outer/name` (`rename`); a copy stays in the block.
pub const LOCAL_ITEM_ATTR: &str = "#local_item";

/// Marker attribute of a block's `use` copied into the bodies of the fns
/// lifted from the block (spec 3.9.1).
pub const INHERITED_USE_ATTR: &str = "#inherited_use";

/// Marks a block-lifted item with `use p::*`s in scope (spec 3.9.1), one
/// prefix per glob. HIR resolves them into the item's headers; its fn
/// bodies carry the `use`s themselves.
pub const BLOCK_GLOB_ATTR: &str = "#block_glob";

/// Marker attribute of a `const fn` (spec 5.2.5): callable in const
/// contexts, evaluated at compile time there.
pub const CONST_FN_ATTR: &str = "#const_fn";

/// Marker attribute of a C variadic declaration `fn printf(fmt: str, ...)`
/// (spec 8.6.10).
pub const C_VARIADIC_ATTR: &str = "#c_variadic";

/// Method name of the `using` desugar's release call (`x.#drop()`):
/// `Drop::drop`, exempt from the direct-call ban (spec 2.13.4).
pub const USING_DROP_METHOD: &str = "#drop";

/// The error type of an untyped `throws`: `any rok::core::error::Error`
/// (spec 4.14.3).
#[must_use]
pub fn untyped_throws_ty(span: Span) -> Ty {
    let segments = UNTYPED_THROWS_TRAIT
        .iter()
        .map(|s| PathSegment {
            name: (*s).to_owned(),
            generics: Vec::new(),
        })
        .collect();
    Ty {
        kind: TyKind::AnyTrait(vec![Path { segments, span }]),
        span,
    }
}

/// Is `t` the error type of an untyped `throws` (`untyped_throws_ty`)?
#[must_use]
pub fn is_untyped_throws(t: &Ty) -> bool {
    matches!(&t.kind, TyKind::AnyTrait(b) if matches!(b.as_slice(), [p]
        if p.segments.iter().map(|s| s.name.as_str()).eq(UNTYPED_THROWS_TRAIT.iter().copied())
            && p.segments.iter().all(|s| s.generics.is_empty())))
}

const UNTYPED_THROWS_TRAIT: &[&str] = &["rok", "core", "error", "Error"];

/// `Name = Ty` if `ty` is an associated-type binding (`TyKind::Binding`).
#[must_use]
pub fn as_assoc_binding(ty: &Ty) -> Option<(&str, &Ty)> {
    match &ty.kind {
        TyKind::Binding(name, t) => Some((name, t)),
        _ => None,
    }
}

/// `throws` clause (spec 4.14). The parser desugars `rethrows` into
/// `Typed` over synthetic error parameters (`#E0`, ...), one per throwing
/// fn-typed parameter.
#[derive(Clone, Debug, Default)]
pub enum ThrowsKind {
    #[default]
    None,
    /// Bare `throws` = `throws (any Error)`; the parser rewrites it to
    /// `Typed([untyped_throws_ty(..)])`.
    Untyped,
    /// `throws (A | B)` - the listed types form the error sum.
    Typed(Vec<Ty>),
}

#[derive(Clone, Debug, Default)]
pub struct Generics {
    pub params: Vec<GenericParam>,
}

#[derive(Clone, Debug)]
pub enum GenericParam {
    /// `T`, `T: Bound`, or `T = Default` (spec 2.10.5); the default applies
    /// when nothing else fixes `T`.
    Type {
        name: String,
        bounds: Vec<Ty>,
        default: Option<Ty>,
    },
    Const {
        name: String,
        ty: Ty,
    },
}

#[derive(Clone, Debug)]
pub struct WherePred {
    pub ty: Ty,
    pub bounds: Vec<Ty>,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: String,
    pub ty: Option<Ty>,
    pub by_ref: bool,
    pub default: Option<Expr>,
    pub variadic: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub trailing: Option<Box<Expr>>,
    pub span: Span,
    /// `'b: { ... }` label; `break 'b value` exits the block with `value`.
    /// Loops carry their label on their own variant.
    pub label: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    /// `let name [: ty] = init;` - fresh binding.
    Let {
        name: String,
        ty: Option<Ty>,
        init: Expr,
    },
    /// `let (a, b, ...) [: ty] = init;`; `_` discards. `rest` is a `..` /
    /// `..name` (spec 3.15.3) spliced in before `names[index]`:
    ///   `(first, ..rest)`      → `names=["first"], rest=(1, Some("rest"))`
    ///   `(head, ..mid, tail)`  → `names=["head","tail"], rest=(1, Some("mid"))`
    ///   `(..)`                 → `names=[], rest=(0, None)`
    LetTuple {
        names: Vec<String>,
        rest: Option<(usize, Option<String>)>,
        ty: Option<Ty>,
        init: Expr,
    },
    /// `let S { f1, f2: alias, .. } [: ty] = init;`: `(field, binding)`
    /// pairs; `rest` for a trailing `..`.
    LetStruct {
        path: Path,
        fields: Vec<(String, String)>,
        rest: bool,
        ty: Option<Ty>,
        init: Expr,
    },
    /// `let pat = init;` for any irrefutable pattern (the `pat` of
    /// `let name @ pat = ..`, after `name` is bound).
    LetPattern {
        pat: Pattern,
        ty: Option<Ty>,
        init: Expr,
    },
    /// `name = expr;` - reassignment (no `let`).
    Assign(Expr, Expr),
    /// `name op= expr;` - `op` is the lexical compound symbol (`+=` etc.).
    CompoundAssign {
        target: Expr,
        op: String,
        value: Expr,
    },
    /// Item declaration appearing inside a block / module body.
    Item(Item),
    /// Bare expression statement (terminated with `;`).
    Expr(Expr),
    /// Trailing expression that yields the block's value (no `;`).
    Trailing(Expr),
    Return(Option<Expr>),
    Break {
        label: Option<String>,
        value: Option<Expr>,
    },
    Continue {
        label: Option<String>,
    },
    Throw(Expr),
}

/// One `#[literal]` handler a literal matched, with its captures.
#[derive(Clone, Debug)]
pub struct LitCand {
    /// The handler's absolute path.
    pub handler: Path,
    pub args: Vec<LitArg>,
    pub priority: i64,
    /// The `#[literal]` attribute.
    pub reg: Span,
}

/// A handler argument: one capture, or a repetition's items (a
/// variadic `...T` parameter's arguments, else one array argument).
#[derive(Clone, Debug)]
pub enum LitArg {
    One(Expr),
    Many(Vec<Expr>),
}

impl LitArg {
    #[must_use]
    pub fn exprs(&self) -> &[Expr] {
        match self {
            Self::One(e) => std::slice::from_ref(e),
            Self::Many(es) => es,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

/// A qualified path `<T as Tr>::item` (spec 2.13.4): `item` of the impl
/// of `Tr` for `T` (an associated type, method or const).
#[derive(Clone, Debug)]
pub struct QPath {
    pub self_ty: Ty,
    pub trait_path: Path,
    pub item: String,
    /// Expressions only: `<T as Tr>::X::m` names the associated function
    /// `m` of the associated type `X`.
    pub member: Option<String>,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Literal(Literal),
    /// Bare identifier reference (variable or item).
    Path(Path),
    /// `<T as Tr>::m` / `<T as Tr>::N` (spec 2.13.4).
    QPath(Box<QPath>),
    SelfLower,
    SelfUpper,
    Tuple(Vec<Expr>),
    List(Vec<Expr>),
    /// `[v; N]` - `v` repeated `N` times, a `[T; N]` (spec 2.11).
    ArrayRepeat {
        value: Box<Expr>,
        len: ConstExpr,
    },
    Dict(Vec<(Expr, Expr)>),
    Set(Vec<Expr>),
    /// Source matched by `#[literal(..)]` patterns (spec 6.5.4), one
    /// candidate per handler; HIR picks by slot type, then priority.
    MagicLiteral(Vec<LitCand>),
    Block(Block),
    /// `unsafe { ... }` (spec 4.15.2).
    Unsafe(Block),
    If {
        cond: Box<Expr>,
        then_branch: Block,
        else_branch: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Loop {
        label: Option<String>,
        body: Block,
    },
    While {
        label: Option<String>,
        cond: Box<Expr>,
        body: Block,
    },
    For {
        label: Option<String>,
        pat: Pattern,
        iter: Box<Expr>,
        body: Block,
        /// `for await` over a `Stream` (spec 4.13.5).
        is_await: bool,
    },
    /// Prefix `try e` / `try? e` / `try! e`: marks the throwing calls in
    /// `e` (everything to its right).
    Try {
        kind: TryKind,
        operand: Box<Expr>,
    },
    /// `e catch pat { .. } ... finally { .. }` (at least one part).
    /// `try { .. } catch ..` is a `Catch` over a `Try` of a block.
    Catch {
        body: Box<Expr>,
        catches: Vec<CatchArm>,
        finally: Option<Block>,
    },
    /// Infix operator. `op` is the lexeme (e.g. `"+"`, `"<="`, `".+"`,
    /// `"≤"`); resolved to a stdlib trait method later.
    OpInfix {
        op: String,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Prefix operator (`!`, `-`, `~`, ...).
    OpPrefix {
        op: String,
        operand: Box<Expr>,
    },
    /// Postfix operator (registered via `#[postfix]`).
    OpPostfix {
        op: String,
        operand: Box<Expr>,
    },
    Cast(Box<Expr>, Ty),
    Is(Box<Expr>, Pattern),
    /// `expr.field`.
    Field(Box<Expr>, String),
    /// Tuple field access: `expr.0`, `expr.1` ...
    TupleField(Box<Expr>, u32),
    /// `expr.method(args...)`
    MethodCall {
        receiver: Box<Expr>,
        method: String,
        type_args: Vec<Ty>,
        args: Vec<CallArg>,
        trailing_closure: Option<Box<Expr>>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<CallArg>,
        trailing_closure: Option<Box<Expr>>,
    },
    Index(Box<Expr>, Box<Expr>),
    Slice {
        target: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },
    Range {
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        inclusive: bool,
    },
    /// `x?.a.b(..)[i]`: the links after `?.` are all skipped when `x` is
    /// `none` (spec 3.8.8).
    QuestionDot(Box<Expr>, Vec<AccessTail>),
    /// `lhs ?? rhs` - language-level sum-type-aware coalesce.
    NullishCoalesce(Box<Expr>, Box<Expr>),
    /// `expr?` - postfix early-return for sum types. Yields the
    /// non-none value or `return none` from the enclosing function.
    TryQ(Box<Expr>),
    Closure {
        captures: Vec<CaptureSpec>,
        params: Vec<Param>,
        ret_ty: Option<Ty>,
        body: Box<Expr>,
        is_async: bool,
        is_generator: bool,
    },
    Await(Box<Expr>),
    /// `expr` resolved in `module`'s scope with no locals: a parameter /
    /// field default spliced into a use in another module (spec 4.2.3).
    InModule(String, Box<Expr>),
    Yield(Option<Box<Expr>>),
    AsyncBlock(Block),
    YieldBlock(Block),
    AsyncYieldBlock(Block),
    StructInit {
        path: Path,
        fields: Vec<(String, Expr)>,
        spread: Option<Box<Expr>>,
    },
    /// `f.(a, b)` (spec 3.3.3): `f` applied to each element of the
    /// `List` / `Slice` / array arguments (others are passed as is).
    Broadcast {
        callee: Box<Expr>,
        args: Vec<CallArg>,
    },
    /// `f"..."` (spec 1.7.3): literal runs and `{expr:spec}` holes,
    /// formatted by type in HIR (stdlib 2.10).
    FString(Vec<FStringPart>),
}

#[derive(Clone, Debug)]
pub enum FStringPart {
    /// Literal text as written (escapes not yet decoded, `{{` / `}}`
    /// folded).
    Lit(String),
    Hole {
        expr: Expr,
        /// The format spec after `:`; empty when there is none.
        spec: Vec<SpecPiece>,
    },
}

/// A piece of a format spec: text (`$$` folded to `$`) or a dynamic
/// `${expr}`.
#[derive(Clone, Debug)]
pub enum SpecPiece {
    Text(String),
    Expr(Expr),
}

#[derive(Clone, Debug)]
pub enum AccessTail {
    Field(String),
    TupleField(u32),
    Method {
        method: String,
        type_args: Vec<Ty>,
        args: Vec<CallArg>,
    },
    Index(Box<Expr>),
}

#[derive(Clone, Debug)]
pub enum CallArg {
    Positional(Expr),
    Named {
        name: String,
        value: Expr,
    },
    Spread(Expr),
    /// `f(ref x)`: pass the binding `x` by reference (spec 4.7.3).
    Ref(Expr),
}

#[derive(Clone, Debug)]
pub enum CaptureSpec {
    Value(String),
    Ref(String),
}

#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pat: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TryKind {
    /// `try e` - errors propagate.
    Plain,
    /// `try? e` - errors become `none`.
    Optional,
    /// `try! e` - errors panic.
    Force,
}

/// `catch pat { body }`; an omitted pattern is `_`.
#[derive(Clone, Debug)]
pub struct CatchArm {
    pub pat: Pattern,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PatternKind {
    Wildcard,
    Binding {
        name: String,
        sub: Option<Box<Pattern>>,
    },
    Literal(Literal),
    Range {
        start: Option<Literal>,
        end: Option<Literal>,
        inclusive: bool,
    },
    /// `name: T` - type-narrowing pattern.
    TypeCheck {
        binder: Option<String>,
        ty: Ty,
    },
    Tuple(Vec<Pattern>),
    /// `..rest` (rest pattern).
    Rest(Option<String>),
    Struct {
        path: Path,
        fields: Vec<FieldPattern>,
        rest: bool,
    },
    EnumVariant {
        path: Path,
        args: Vec<Pattern>,
    },
    Or(Vec<Pattern>),
}

#[derive(Clone, Debug)]
pub struct FieldPattern {
    pub name: String,
    pub pat: Option<Pattern>,
}

#[derive(Clone, Debug)]
pub struct Path {
    pub segments: Vec<PathSegment>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct PathSegment {
    pub name: String,
    pub generics: Vec<Ty>,
}

#[derive(Clone, Debug)]
pub struct Ty {
    pub kind: TyKind,
    pub span: Span,
}

impl Ty {
    /// `X` of a nested sum member (`TyKind::Nest`).
    #[must_use]
    pub const fn nest_inner(&self) -> Option<&Self> {
        match &self.kind {
            TyKind::Nest(x) => Some(x),
            _ => None,
        }
    }
}

/// What a `fn` type says beyond its signature.
#[derive(Clone, Debug, Default)]
pub struct FnTyBounds {
    /// `+ Send + Sync`: auto-trait bounds.
    pub auto: Vec<Path>,
    /// `unsafe fn(..)`.
    pub unsafe_fn: bool,
    /// `extern "C" fn(..)`.
    pub c_abi: bool,
    /// `extern "system" fn(..)` (with `c_abi`).
    pub system: bool,
}

#[derive(Clone, Debug)]
pub enum TyKind {
    /// Path type (`i32`, `List<T>`, `std::time::Duration`).
    Path(Path),
    /// `<T as Tr>::X` (spec 2.13.4).
    QPath(Box<QPath>),
    /// Sum type `A | B | C`.
    Sum(Vec<Ty>),
    /// Tuple `(A, B)`.
    Tuple(Vec<Ty>),
    /// Function type `fn(A, B) -> R [+ Send + Sync]`. A throwing one has
    /// a `Throws` return type.
    Fn(Vec<Ty>, Box<Ty>, FnTyBounds),
    /// A sum member kept as one layer (`T` of `T?`, a substituted
    /// associated type): `HirTy::sum` keeps it nested when it overlaps
    /// (spec 2.6.4).
    Nest(Box<Ty>),
    /// An associated-type binding `Name = Ty` among a trait's generic
    /// arguments (`Iterator<Item = i32>`, spec 2.1 `trait_arg`).
    Binding(String, Box<Ty>),
    /// Return slot of `fn(..) throws (err) -> ok`; only appears as the
    /// return type of `Fn`.
    Throws { ok: Box<Ty>, err: Box<Ty> },
    /// Fixed-size array `[T; N]`.
    Array(Box<Ty>, ConstExpr),
    /// `any` slot.
    Any,
    /// `any Tr` / `any (A + B)` - a dynamic slot bounded by traits.
    AnyTrait(Vec<Path>),
    /// `none` (used inside sum types).
    None,
    /// Self type.
    SelfUpper,
    /// `!` - the never type of a diverging function (`panic`, `exit`).
    Never,
    /// A const generic argument in a type-argument list (`Matrix<f64, 3, 4>`,
    /// `replicate::<i32, 8>`; spec 2.11.4).
    Const(ConstExpr),
}

/// Const-expression placeholder for array length / const generics.
#[derive(Clone, Debug)]
pub enum ConstExpr {
    Literal(Literal),
    Path(Path),
    Expr(Box<Expr>),
}

#[derive(Clone, Debug)]
pub struct StructDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub generics: Generics,
    pub kind: StructKind,
    /// `where` predicates after the header (spec 2.10.4).
    pub where_clause: Vec<WherePred>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum StructKind {
    Unit,
    Tuple(Vec<TupleField>),
    Named(Vec<NamedField>),
}

#[derive(Clone, Debug)]
pub struct NamedField {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub ty: Ty,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TupleField {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub generics: Generics,
    pub variants: Vec<EnumVariant>,
    /// `where` predicates after the header (spec 2.10.4).
    pub where_clause: Vec<WherePred>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumVariant {
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub kind: EnumVariantKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum EnumVariantKind {
    Unit,
    Tuple(Vec<Ty>),
    Named(Vec<NamedField>),
    Value(Expr),
}

#[derive(Clone, Debug)]
pub struct UnionDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub generics: Generics,
    pub fields: Vec<NamedField>,
    /// `where` predicates after the header (spec 2.10.4).
    pub where_clause: Vec<WherePred>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TraitDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub generics: Generics,
    pub supertraits: Vec<Ty>,
    pub items: Vec<TraitItem>,
    pub where_clause: Vec<WherePred>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TraitItem {
    Method(FnDef),
    AssocType {
        name: String,
        bounds: Vec<Ty>,
        default: Option<Ty>,
    },
    /// `const N: T;` / `const N: T = v;` (spec 2.8.3a).
    Const {
        name: String,
        ty: Ty,
        default: Option<Expr>,
    },
}

#[derive(Clone, Debug)]
pub struct ImplDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub generics: Generics,
    pub trait_path: Option<Path>,
    /// `unsafe impl` (manual `Send` / `Sync` opt-in).
    pub is_unsafe: bool,
    /// `impl !Tr for T` (auto-trait opt-out).
    pub negative: bool,
    pub self_ty: Ty,
    pub where_clause: Vec<WherePred>,
    pub items: Vec<ImplItem>,
    /// The module the impl is declared in (`a::b`; empty at a package
    /// root), set by the driver when it flattens modules.
    pub module: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ImplItem {
    Method(FnDef),
    AssocType { name: String, ty: Ty },
    Const(ConstDef),
}

#[derive(Clone, Debug)]
pub struct ConstDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub ty: Ty,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct StaticDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub ty: Ty,
    pub value: Option<Expr>,
    pub is_mut: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeAliasDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub generics: Generics,
    pub ty: Ty,
    /// `where` predicates after the header (spec 2.10.4).
    pub where_clause: Vec<WherePred>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ModDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub items: Option<Vec<Item>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct UseDef {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub tree: UseTree,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct UseTree {
    pub prefix: Path,
    pub kind: UseTreeKind,
}

#[derive(Clone, Debug)]
pub enum UseTreeKind {
    Simple(Option<String>),
    Glob,
    GlobRecursive,
    Group(Vec<UseTree>),
}

#[derive(Clone, Debug)]
pub struct ExternBlock {
    pub attrs: Vec<Attribute>,
    pub abi: String,
    pub items: Vec<ExternItem>,
    pub span: Span,
    /// `ffi::wit::import { "a.wit", .. }` / `ffi::wit::export { .. }`
    /// (stdlib 1.8): no items; the driver adds the bindings.
    pub wit: Option<WitRef>,
}

/// The `.wit` paths of an `ffi::wit::import` / `ffi::wit::export`.
#[derive(Clone, Debug)]
pub struct WitRef {
    pub export: bool,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum ExternItem {
    Fn(FnDef),
    Static(StaticDef),
}

/// `(lifted name, inherited)` of a nested fn marker (`NESTED_FN_ATTR`).
#[must_use]
pub fn nested_marker_of(f: &FnDef) -> Option<(&str, bool)> {
    let a = f
        .attrs
        .iter()
        .find(|a| a.path.first().map(String::as_str) == Some(NESTED_FN_ATTR))?;
    let AttrBody::Args(args) = &a.body else {
        return None;
    };
    let AttrArg::Lit(Literal::Str(name)) = args.first()? else {
        return None;
    };
    Some((name.as_str(), args.len() > 1))
}
