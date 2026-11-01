//! Abstract syntax tree. Operators are just token symbols here; trait
//! dispatch (`#[infix]` / `#[prefix]` / `#[postfix]`) happens later.

mod codec;
pub mod defs;
pub mod escape;
pub mod print;
pub mod rename;

pub use defs::*;

use rokc_diagnostic::Span;

#[derive(Clone, Debug, Default)]
pub struct Program {
    pub attrs: Vec<Attribute>,
    pub items: Vec<Item>,
    /// Top-level statements in source order (spec 4.16.1). Only the entry
    /// file `main.rok` may have them; they become the implicit `main`.
    pub stmts: Vec<Stmt>,
    /// Source text of each meta expansion (spec 6.9.9, `--emit=expansions`).
    pub expanded: Vec<String>,
    /// Start of each top-level item / statement (first token, attributes
    /// included): (byte offset, is an item). The REPL splits programs by
    /// it.
    pub tops: Vec<(u32, bool)>,
}

/// Parsed attribute. Spec 6.1 grammar:
/// `attr_body ::= path attr_args?`,
/// `attr_args ::= "(" arg ("," arg)* ","? ")" | "=" literal`,
/// `arg ::= literal | identifier | attr_body | identifier "=" literal`.
#[derive(Clone, Debug)]
pub struct Attribute {
    pub path: Vec<String>,
    pub body: AttrBody,
    pub inner: bool,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub enum AttrBody {
    /// `#[name]` - flag only.
    #[default]
    Empty,
    /// `#[name(arg, arg, ...)]`.
    Args(Vec<AttrArg>),
    /// `#[name = literal]`.
    Value(Literal),
}

#[derive(Clone, Debug)]
pub enum AttrArg {
    Lit(Literal),
    Word(String),
    KeyValue(String, Literal),
    /// Nested `name(args)` or `name = literal` inside an arg list.
    Nested {
        path: Vec<String>,
        body: Box<AttrBody>,
    },
    /// `lo..hi` / `lo..=hi`, either end optional (`#[niche(I32_MIN..=-1)]`).
    Range {
        lo: Option<Box<AttrArg>>,
        hi: Option<Box<AttrArg>>,
        inclusive: bool,
    },
}

impl Attribute {
    /// Convenience: `name` of a single-segment attribute path. Returns
    /// `""` for multi-segment paths so callers can fall back to `path`.
    #[must_use]
    pub fn name(&self) -> &str {
        if self.path.len() == 1 {
            &self.path[0]
        } else {
            ""
        }
    }

    /// `#[doc(hidden)]`: left out of the API docs and completion.
    #[must_use]
    pub fn is_doc_hidden(&self) -> bool {
        self.name() == "doc"
            && matches!(&self.body, AttrBody::Args(xs) if matches!(xs.as_slice(), [AttrArg::Word(w)] if w == "hidden"))
    }
}

/// Whether `attrs` hide their item from docs and completion:
/// `#[doc(hidden)]`, or `#[internal]` (no API to show).
#[must_use]
pub fn doc_hidden(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.is_doc_hidden() || a.path == ["internal"])
}

#[derive(Clone, Debug)]
pub enum Literal {
    /// Integer text + optional suffix as written.
    Int(String),
    /// Float text + optional suffix.
    Float(String),
    Bool(bool),
    Str(String),
    RawStr(String),
    /// `b"..."`: the body as written.
    ByteStr(String),
    Char(String),
    CChar(String),
    None,
}

#[derive(Clone, Debug)]
pub enum Item {
    Fn(FnDef),
    Struct(StructDef),
    Enum(EnumDef),
    Union(UnionDef),
    Trait(TraitDef),
    Impl(ImplDef),
    Const(ConstDef),
    Static(StaticDef),
    TypeAlias(TypeAliasDef),
    Mod(ModDef),
    Use(UseDef),
    Extern(ExternBlock),
}

impl Item {
    #[must_use]
    pub const fn span(&self) -> rokc_diagnostic::Span {
        match self {
            Self::Fn(x) => x.span,
            Self::Struct(x) => x.span,
            Self::Enum(x) => x.span,
            Self::Union(x) => x.span,
            Self::Trait(x) => x.span,
            Self::Impl(x) => x.span,
            Self::Const(x) => x.span,
            Self::Static(x) => x.span,
            Self::TypeAlias(x) => x.span,
            Self::Mod(x) => x.span,
            Self::Use(x) => x.span,
            Self::Extern(x) => x.span,
        }
    }

    #[must_use]
    pub const fn attrs(&self) -> &Vec<Attribute> {
        match self {
            Self::Fn(x) => &x.attrs,
            Self::Struct(x) => &x.attrs,
            Self::Enum(x) => &x.attrs,
            Self::Union(x) => &x.attrs,
            Self::Trait(x) => &x.attrs,
            Self::Impl(x) => &x.attrs,
            Self::Const(x) => &x.attrs,
            Self::Static(x) => &x.attrs,
            Self::TypeAlias(x) => &x.attrs,
            Self::Mod(x) => &x.attrs,
            Self::Use(x) => &x.attrs,
            Self::Extern(x) => &x.attrs,
        }
    }

    pub const fn attrs_mut(&mut self) -> &mut Vec<Attribute> {
        match self {
            Self::Fn(x) => &mut x.attrs,
            Self::Struct(x) => &mut x.attrs,
            Self::Enum(x) => &mut x.attrs,
            Self::Union(x) => &mut x.attrs,
            Self::Trait(x) => &mut x.attrs,
            Self::Impl(x) => &mut x.attrs,
            Self::Const(x) => &mut x.attrs,
            Self::Static(x) => &mut x.attrs,
            Self::TypeAlias(x) => &mut x.attrs,
            Self::Mod(x) => &mut x.attrs,
            Self::Use(x) => &mut x.attrs,
            Self::Extern(x) => &mut x.attrs,
        }
    }

    /// The name it declares (`None` for an `impl`, `use` or `extern`).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Fn(x) => Some(&x.name),
            Self::Struct(x) => Some(&x.name),
            Self::Enum(x) => Some(&x.name),
            Self::Union(x) => Some(&x.name),
            Self::Trait(x) => Some(&x.name),
            Self::Const(x) => Some(&x.name),
            Self::Static(x) => Some(&x.name),
            Self::TypeAlias(x) => Some(&x.name),
            Self::Mod(x) => Some(&x.name),
            Self::Impl(_) | Self::Use(_) | Self::Extern(_) => None,
        }
    }

    /// Lifted from a block (spec 3.9.1): see [`LOCAL_ITEM_ATTR`].
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.local_outer().is_some()
    }

    /// The lifted path of the fn a local item is declared in (`f`,
    /// `Type.m`).
    #[must_use]
    pub fn local_outer(&self) -> Option<&str> {
        self.attrs().iter().find_map(|a| match &a.body {
            AttrBody::Args(args) if a.path == [LOCAL_ITEM_ATTR] => match args.as_slice() {
                [AttrArg::Lit(Literal::Str(s))] => Some(s.as_str()),
                _ => None,
            },
            _ => None,
        })
    }
}
