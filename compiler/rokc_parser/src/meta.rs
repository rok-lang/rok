//! User meta blocks (refs-lang 6.8 / 6.9): `#[impl_derive(Tr)]`,
//! `#[impl_derive_attribute(Tr, attr)]` and `#[impl_attribute(name)]`.
//!
//! Bodies stay tokens. A driver pre-pass collects them into a `MetaTable`
//! (`collect_meta_blocks`) before the real parse; a use then expands by
//! substituting template tokens (`@Self`, `@for_fields`, `@field.name`,
//! ...) and parsing the result, to a fixpoint. Tokens keep their origin's
//! spans; errors point at the template with a label at the use.
//!
//! Hygiene (6.9.8): template locals are renamed `x__meta` so they neither
//! capture nor shadow the item's names; enum payloads get generated names.

// Slice indexing and one flat template match; `meta_block` tells "not a
// block" from "a malformed block".
#![allow(
    clippy::many_single_char_names,
    clippy::cognitive_complexity,
    clippy::option_option,
    clippy::type_complexity,
    clippy::too_many_lines
)]

use std::collections::HashSet;
use std::sync::Arc;

use rokc_ast::{AttrArg, AttrBody, Attribute, EnumVariantKind, GenericParam, Item, StructKind};
use rokc_core::Visibility;
use rokc_diagnostic::{ErrorKind, RokDiagnostic, Span};
use rokc_lexer::{Token, TokenKind};

use super::Parser;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetaKind {
    /// `#[impl_derive]` / `#[impl_derive_attribute]` - used as `#[derive(Name)]`.
    Derive,
    /// `#[impl_attribute]` - used as `#[name]` / `#[name(args)]`.
    Attribute,
    /// `#[impl_macro]` - used as `name!(..)` at a call site (spec 6.8.12).
    Macro,
}

#[derive(Clone, Debug)]
pub struct MetaDef {
    pub kind: MetaKind,
    /// The derive (trait) or attribute name it registers.
    pub name: String,
    /// `#[impl_derive_attribute(Tr, attr)]`'s field attribute.
    pub field_attr: Option<String>,
    pub public: bool,
    /// The defining file (a private block is usable there only).
    pub file: String,
    /// The defining package (`""` for the one being built).
    pub package: String,
    /// The `#[impl_..]` attribute.
    pub span: Span,
    /// Tokens between the body's braces.
    pub body: Vec<Token>,
    /// The block's own `#[cfg(..)]` attributes: the driver registers it
    /// only when they hold (6.9.8: cfg runs before expansion).
    pub cfg: Vec<Attribute>,
}

impl MetaDef {
    /// Mount path of the block's package (`""`: the one being built); its
    /// `pkg::` paths keep naming it wherever the block expands (6.9.8).
    pub fn set_package(&mut self, package: &str) {
        package.clone_into(&mut self.package);
        for k in 0..self.body.len() {
            let after_sep = k > 0 && self.body[k - 1].kind == TokenKind::ColonColon;
            let root = matches!(&self.body[k].kind, TokenKind::Ident(n) if n == "pkg")
                && self
                    .body
                    .get(k + 1)
                    .is_some_and(|t| t.kind == TokenKind::ColonColon);
            if root && !after_sep {
                self.body[k].kind = TokenKind::Ident(format!("{PKG_ROOT}{package}"));
            }
        }
    }
}

/// Prefix of the path root a meta block's `pkg::` becomes; `@` keeps it
/// out of reach of source text.
pub const PKG_ROOT: &str = "pkg@";

/// Every meta block of the compilation, by name.
#[derive(Clone, Debug, Default)]
pub struct MetaTable {
    defs: Vec<MetaDef>,
}

/// Attributes the compiler owns (6.2 - 6.7); `#[impl_attribute]` can't
/// take their names. Every name the driver checks is here (a driver test
/// holds it).
pub const BUILTIN_ATTRS: &[&str] = &[
    "allow",
    "bench",
    "cfg",
    "cfg_attr",
    "cold",
    "deny",
    "deprecated",
    "deprecated_safe",
    "derive",
    "doc",
    "export_name",
    "impl_attribute",
    "impl_derive",
    "impl_derive_attribute",
    "impl_macro",
    "infix",
    "inline",
    "intrinsic",
    "keyword",
    "lang",
    "link",
    "link_name",
    "literal",
    "must_use",
    "niche",
    "no_implicit_rok",
    "no_mangle",
    "no_safepoint",
    "non_exhaustive",
    "path",
    "postfix",
    "prefix",
    "repr",
    "test",
    "track_caller",
    "warn",
];

/// Compiler built-in `!`-forms ([6.3](603)/[6.11](611)); a macro cannot
/// take their names (spec 6.8.12).
const BUILTIN_BANGS: &[&str] = &["cfg", "include_bytes", "include_str"];

impl MetaTable {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Whether `name` is a derive or attribute a meta block registers
    /// (what the parser expands on an item). Macros expand at call sites,
    /// so they are not counted here.
    #[must_use]
    pub fn defines(&self, name: &str) -> bool {
        self.defs
            .iter()
            .any(|d| d.name == name && d.kind != MetaKind::Macro)
    }

    /// The macro `name!` a meta block registers, if any (spec 6.8.12).
    #[must_use]
    pub fn find_macro(&self, name: &str) -> Option<&MetaDef> {
        self.find(MetaKind::Macro, name)
    }

    /// Whether some meta block defines attribute `name` (as the attribute,
    /// a derive's field attribute, or one tested by `@field.has_attr`).
    #[must_use]
    pub fn knows_attribute(&self, name: &str) -> bool {
        let tested = |d: &MetaDef| {
            d.body.windows(3).any(|w| {
                matches!(&w[0].kind, TokenKind::Ident(h) if h == "has_attr")
                    && w[1].kind == TokenKind::LParen
                    && matches!(&w[2].kind, TokenKind::Ident(n) if n == name)
            })
        };
        self.defs.iter().any(|d| {
            (d.kind == MetaKind::Attribute && d.name == name)
                || d.field_attr.as_deref() == Some(name)
                || tested(d)
        })
    }

    #[must_use]
    pub fn find(&self, kind: MetaKind, name: &str) -> Option<&MetaDef> {
        self.defs.iter().find(|d| d.kind == kind && d.name == name)
    }

    /// Every block registering `name` of `kind`, one per package.
    pub fn find_all<'a>(
        &'a self,
        kind: MetaKind,
        name: &'a str,
    ) -> impl Iterator<Item = &'a MetaDef> + 'a {
        self.defs
            .iter()
            .filter(move |d| d.kind == kind && d.name == name)
    }

    /// Register `def`; a second block for the same name in one package,
    /// or one taking a built-in derive / attribute name, is an error. Two
    /// packages may each register a name: a use then says which (6.8.11).
    pub fn add(&mut self, def: MetaDef) -> Option<RokDiagnostic> {
        let clash = |msg: String| RokDiagnostic::error(ErrorKind::Meta, def.span, msg);
        if let Some(old) = self
            .find_all(def.kind, &def.name)
            .find(|d| d.package == def.package)
        {
            if old.span == def.span && old.file == def.file {
                return None;
            }
            let what = match def.kind {
                MetaKind::Derive => format!("derive `{}`", def.name),
                MetaKind::Attribute => format!("attribute `#[{}]`", def.name),
                MetaKind::Macro => format!("macro `{}!`", def.name),
            };
            return Some(
                clash(format!(
                    "{what} is defined by two meta blocks (spec 6.8.11)"
                ))
                .with_secondary(old.span, "first defined here"),
            );
        }
        if def.kind == MetaKind::Derive && crate::derive::derivable(&def.name).is_some() {
            return Some(clash(format!(
                "`{}` is a built-in derive; a meta block cannot redefine it (spec 6.2.1)",
                def.name
            )));
        }
        if def.kind == MetaKind::Attribute && BUILTIN_ATTRS.contains(&def.name.as_str()) {
            return Some(
                clash(format!(
                    "`#[{}]` is a built-in attribute; a meta block cannot redefine it (spec 6.2)",
                    def.name
                ))
                .with_help(format!(
                    "rename it, e.g. `#[impl_attribute(my_{})]`",
                    def.name
                )),
            );
        }
        if def.kind == MetaKind::Macro && BUILTIN_BANGS.contains(&def.name.as_str()) {
            return Some(clash(format!(
                "`{}!` is a built-in form; a macro cannot take its name (spec 6.8.12)",
                def.name
            )));
        }
        self.defs.push(def);
        None
    }
}

/// The pre-pass: every meta block in `tokens` (a whole file). Malformed
/// blocks are reported by the real parse, so diagnostics are dropped.
#[must_use]
pub fn collect_meta_blocks(tokens: &[Token], file: &str) -> Vec<MetaDef> {
    let mut p = Parser::new(tokens, file);
    let mut out = Vec::new();
    // Open inline `mod`s: the index of their closing `}` and their
    // `#[cfg(..)]`s, which hold for every block inside too.
    let mut mods: Vec<(usize, Vec<Attribute>)> = Vec::new();
    while !p.at_eof() {
        mods.retain(|(end, _)| p.pos < *end);
        if let Some(m) = p.cfg_mod_open() {
            mods.push(m);
            continue;
        }
        match p.meta_block() {
            Some(Some(mut d)) => {
                d.cfg
                    .extend(mods.iter().flat_map(|(_, c)| c.iter().cloned()));
                out.push(d);
            }
            Some(None) => {}
            None => {
                p.bump();
            }
        }
    }
    out
}

fn is_meta_attr(a: &Attribute) -> bool {
    matches!(
        a.name(),
        "impl_derive" | "impl_attribute" | "impl_derive_attribute" | "impl_macro"
    )
}

fn attr_words(a: &Attribute) -> Vec<String> {
    match &a.body {
        AttrBody::Args(args) => args
            .iter()
            .filter_map(|x| match x {
                AttrArg::Word(w) => Some(w.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn meta_err(span: Span, msg: impl Into<String>) -> RokDiagnostic {
    RokDiagnostic::error(ErrorKind::Meta, span, msg)
}

/// Index of the token closing the group opened at `open`.
fn close_of(toks: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (k, t) in toks.iter().enumerate().skip(open) {
        match t.kind {
            TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::HashLBracket
            | TokenKind::HashBangLBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// Index of the token closing the innermost group around `at` (a `{..}`
/// only, with `braces`); the end of `toks` when there is none.
fn enclosing_close(toks: &[Token], at: usize, braces: bool) -> usize {
    let mut depth = 0u32;
    for k in (0..at).rev() {
        match toks[k].kind {
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => depth += 1,
            TokenKind::LBrace if depth == 0 => return close_of(toks, k).unwrap_or(toks.len()),
            TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::HashLBracket
            | TokenKind::HashBangLBracket
                if depth == 0 =>
            {
                if !braces {
                    return close_of(toks, k).unwrap_or(toks.len());
                }
            }
            TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::HashLBracket
            | TokenKind::HashBangLBracket => depth -= 1,
            _ => {}
        }
    }
    toks.len()
}

/// Index of the token closing the first `{..}` at or after `from` outside
/// any other group (a `for` loop's body after its `in`).
fn block_after(toks: &[Token], from: usize) -> Option<usize> {
    let mut depth = 0u32;
    for (k, t) in toks.iter().enumerate().skip(from) {
        match t.kind {
            TokenKind::LBrace if depth == 0 => return close_of(toks, k),
            TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::HashLBracket
            | TokenKind::HashBangLBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                depth = depth.checked_sub(1)?;
            }
            _ => {}
        }
    }
    None
}

/// First token at or after `from` written at `span`.
fn find_span(toks: &[Token], from: usize, span: Span) -> Option<usize> {
    (from..toks.len()).find(|&k| toks[k].span == span)
}

/// The tokens inside the parentheses of the attribute at `toks[at]`.
fn attr_arg_tokens(toks: &[Token], at: usize) -> Vec<Token> {
    let Some(end) = close_of(toks, at) else {
        return Vec::new();
    };
    match (at..end).find(|&k| toks[k].kind == TokenKind::LParen) {
        Some(lp) => close_of(toks, lp).map_or_else(Vec::new, |rp| toks[lp + 1..rp].to_vec()),
        None => Vec::new(),
    }
}

const fn tok(kind: TokenKind, span: Span) -> Token {
    Token::new(kind, span)
}

fn ident(name: &str, span: Span) -> Token {
    // A tuple field's "name" is its index (`self.0`).
    if name.bytes().all(|b| b.is_ascii_digit()) {
        return tok(TokenKind::IntLit(name.to_owned()), span);
    }
    tok(TokenKind::Ident(name.to_owned()), span)
}

#[derive(Clone, Debug)]
struct FieldInfo {
    /// Field name, or its index in a tuple struct / variant.
    name: String,
    ty: Vec<Token>,
    is_pub: bool,
    /// `(name, argument tokens)` of each attribute on the field.
    attrs: Vec<(String, Vec<Token>)>,
    /// The `= expr` default of a named struct field (empty: none).
    default: Vec<Token>,
    span: Span,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VariantShape {
    Unit,
    Tuple,
    Named,
}

#[derive(Clone, Debug)]
struct VariantInfo {
    name: String,
    shape: VariantShape,
    fields: Vec<FieldInfo>,
}

/// What a meta block is applied to.
struct Target {
    /// `struct` / `enum` / `union` / `fn` / `impl` / `trait` / other.
    kind: &'static str,
    name: String,
    generics: Vec<GenericParam>,
    /// A struct's shape (`@Self.is_unit` / `.is_tuple` / `.is_named`).
    shape: VariantShape,
    fields: Vec<FieldInfo>,
    variants: Vec<VariantInfo>,
    /// The item's tokens without the applied attribute.
    item: Vec<Token>,
    /// `#[name(args)]`'s arguments (`@attribute`).
    attr_args: Vec<Token>,
    /// The `#[derive(..)]` / `#[name]` use.
    at: Span,
}

#[derive(Clone, Copy, Default)]
struct Ctx {
    variant: Option<usize>,
    /// (variant or `usize::MAX` for the item's own fields, field index).
    field: Option<(usize, usize)>,
}

const TOP: usize = usize::MAX;

struct Expander<'a> {
    def: &'a MetaDef,
    t: &'a Target,
    out: Vec<Token>,
    diags: Vec<RokDiagnostic>,
    /// `@where_all(Tr)` bounds for the type parameters of later impls.
    where_all: Vec<Vec<Token>>,
    /// Hygiene: names the template binds (with the token range each is
    /// in scope over), and token indices that are binding positions.
    bound: Vec<(String, usize, usize)>,
    bind_pos: HashSet<usize>,
}

/// Hygienic name of a template-introduced local.
fn hygienic(name: &str) -> String {
    format!("{name}__meta")
}

impl Expander<'_> {
    fn body(&self) -> &[Token] {
        &self.def.body
    }

    fn err(&mut self, at: usize, msg: impl Into<String>) {
        let span = self.body().get(at).map_or(self.def.span, |t| t.span);
        let what = match self.def.kind {
            MetaKind::Derive => format!("in this `#[derive({})]`", self.def.name),
            MetaKind::Attribute => format!("in this `#[{}]`", self.def.name),
            MetaKind::Macro => format!("in this `{}!` macro", self.def.name),
        };
        self.diags
            .push(meta_err(span, msg).with_secondary(self.t.at, what));
    }

    fn field(&self, ctx: Ctx) -> Option<&FieldInfo> {
        let (v, f) = ctx.field?;
        if v == TOP {
            self.t.fields.get(f)
        } else {
            self.t.variants.get(v)?.fields.get(f)
        }
    }

    /// Name a variant payload field is bound to by `@variant.pattern`.
    fn binding(&self, ctx: Ctx) -> Option<String> {
        let (v, f) = ctx.field?;
        (v != TOP).then(|| format!("{}__meta{f}", self.t.variants[v].name))
    }

    /// `Name` or `Name<T, N>`.
    fn self_tokens(&self, span: Span) -> Vec<Token> {
        let mut v = vec![tok(TokenKind::Ident(self.t.name.clone()), span)];
        if !self.t.generics.is_empty() {
            v.push(tok(TokenKind::Lt, span));
            for (i, g) in self.t.generics.iter().enumerate() {
                if i > 0 {
                    v.push(tok(TokenKind::Comma, span));
                }
                let (GenericParam::Type { name, .. } | GenericParam::Const { name, .. }) = g;
                v.push(tok(TokenKind::Ident(name.clone()), span));
            }
            v.push(tok(TokenKind::Gt, span));
        }
        v
    }

    /// `<T: B1 + B2, const N: usize>` after a template `impl`.
    fn impl_params(&self, span: Span) -> Vec<Token> {
        let mut v = vec![tok(TokenKind::Lt, span)];
        for (i, g) in self.t.generics.iter().enumerate() {
            if i > 0 {
                v.push(tok(TokenKind::Comma, span));
            }
            match g {
                GenericParam::Type { name, .. } => {
                    v.push(tok(TokenKind::Ident(name.clone()), span));
                    for (j, b) in self.where_all.iter().enumerate() {
                        v.push(tok(
                            if j == 0 {
                                TokenKind::Colon
                            } else {
                                TokenKind::Plus
                            },
                            span,
                        ));
                        v.extend(b.iter().cloned());
                    }
                }
                GenericParam::Const { name, ty } => {
                    v.push(tok(TokenKind::Ident("const".to_owned()), span));
                    v.push(tok(TokenKind::Ident(name.clone()), span));
                    v.push(tok(TokenKind::Colon, span));
                    v.push(tok(TokenKind::Ident(crate::derive::ty_text(ty)), span));
                }
            }
        }
        v.push(tok(TokenKind::Gt, span));
        v
    }

    /// `Name::V`, `Name::V(a, b)` or `Name::V { x: a }` binding payload
    /// fields to hygienic names.
    fn pattern(&self, v: usize, span: Span) -> Vec<Token> {
        let var = &self.t.variants[v];
        let mut out = vec![
            tok(TokenKind::Ident(self.t.name.clone()), span),
            tok(TokenKind::ColonColon, span),
            tok(TokenKind::Ident(var.name.clone()), span),
        ];
        let bind = |f: usize| {
            self.binding(Ctx {
                variant: Some(v),
                field: Some((v, f)),
            })
            .unwrap_or_default()
        };
        match var.shape {
            VariantShape::Unit => {}
            VariantShape::Tuple => {
                out.push(tok(TokenKind::LParen, span));
                for f in 0..var.fields.len() {
                    if f > 0 {
                        out.push(tok(TokenKind::Comma, span));
                    }
                    out.push(tok(TokenKind::Ident(bind(f)), span));
                }
                out.push(tok(TokenKind::RParen, span));
            }
            VariantShape::Named => {
                out.push(tok(TokenKind::LBrace, span));
                for (f, fi) in var.fields.iter().enumerate() {
                    if f > 0 {
                        out.push(tok(TokenKind::Comma, span));
                    }
                    out.push(tok(TokenKind::Ident(fi.name.clone()), fi.span));
                    out.push(tok(TokenKind::Colon, span));
                    out.push(tok(TokenKind::Ident(bind(f)), span));
                }
                out.push(tok(TokenKind::RBrace, span));
            }
        }
        out
    }

    /// Substitute `@..` names inside string / f-string text.
    fn string_text(&self, s: &str, ctx: Ctx) -> String {
        let mut s = s.replace("@Self", &self.t.name);
        s = s.replace("@item.name", &self.t.name);
        if let Some(f) = self.field(ctx) {
            s = s.replace("@field.name", &f.name);
        }
        if let Some(v) = ctx.variant {
            s = s.replace("@variant.name", &self.t.variants[v].name);
        }
        s
    }

    /// Hygiene pre-scan: names bound by `let` / `for` / closure params
    /// written in the template itself (not those spliced in by `@..`),
    /// each with the token range it is in scope over.
    fn scan_binders(&mut self) {
        let body = &self.def.body;
        let mut names = Vec::new();
        let mut pos = HashSet::new();
        let mut mark = |k: usize, end: usize, names: &mut Vec<(String, usize, usize)>| {
            if let TokenKind::Ident(n) = &body[k].kind {
                if n != "mut" && n != "in" && n != "_" {
                    names.push((n.clone(), k, end));
                    pos.insert(k);
                }
            }
        };
        for k in 0..body.len() {
            let prev_at = k > 0 && body[k - 1].kind == TokenKind::At;
            match body[k].kind {
                TokenKind::Let | TokenKind::For if !prev_at => {
                    let mut j = k + 1;
                    if matches!(&body.get(j).map(|t| &t.kind), Some(TokenKind::Ident(m)) if m == "mut")
                    {
                        j += 1;
                    }
                    // `for x in` binds; `impl Tr for Ty` doesn't.
                    let after = match body.get(j).map(|t| &t.kind) {
                        Some(TokenKind::LParen) => close_of(body, j).map(|e| e + 1),
                        _ => Some(j + 1),
                    };
                    let is_in = matches!(
                        after.and_then(|a| body.get(a)).map(|t| &t.kind),
                        Some(TokenKind::Ident(w)) if w == "in"
                    );
                    if body[k].kind == TokenKind::For && !is_in {
                        continue;
                    }
                    // A `let` holds to the end of its block, a `for`
                    // binding over the loop body.
                    let scope = if body[k].kind == TokenKind::For {
                        after.and_then(|a| block_after(body, a))
                    } else {
                        None
                    }
                    .unwrap_or_else(|| enclosing_close(body, k, true));
                    match body.get(j).map(|t| &t.kind) {
                        Some(TokenKind::Ident(_)) => mark(j, scope, &mut names),
                        Some(TokenKind::LParen) => {
                            let end = close_of(body, j).unwrap_or(j);
                            for m in j + 1..end {
                                if matches!(
                                    body.get(m + 1).map(|t| &t.kind),
                                    Some(TokenKind::Comma | TokenKind::RParen)
                                ) && body[m - 1].kind != TokenKind::At
                                    && body[m - 1].kind != TokenKind::Dot
                                {
                                    mark(m, scope, &mut names);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                // A closure's `|a, b: T|` (a `|` where an expression starts).
                TokenKind::Pipe
                    if k > 0
                        && matches!(
                            body[k - 1].kind,
                            TokenKind::LParen
                                | TokenKind::Comma
                                | TokenKind::Eq
                                | TokenKind::LBrace
                                | TokenKind::Semi
                                | TokenKind::FatArrow
                                | TokenKind::Return
                        ) =>
                {
                    let mut params = Vec::new();
                    let mut m = k + 1;
                    let mut expect_name = true;
                    while m < body.len() && body[m].kind != TokenKind::Pipe {
                        match body[m].kind {
                            TokenKind::Ident(_) if expect_name => {
                                params.push(m);
                                expect_name = false;
                            }
                            TokenKind::Comma => expect_name = true,
                            _ => {}
                        }
                        m += 1;
                    }
                    // The closure body: a block, else up to the end of the
                    // group the closure is written in.
                    let scope = match body.get(m + 1).map(|t| &t.kind) {
                        Some(TokenKind::LBrace) => close_of(body, m + 1),
                        _ => None,
                    }
                    .unwrap_or_else(|| enclosing_close(body, k, false));
                    for p in params {
                        mark(p, scope, &mut names);
                    }
                }
                _ => {}
            }
        }
        self.bound = names;
        self.bind_pos = pos;
    }

    /// Emit template token `k` (renamed if it is a hygienic local).
    fn emit_plain(&mut self, k: usize, ctx: Ctx) {
        let t = self.body()[k].clone();
        let kind = match &t.kind {
            TokenKind::Ident(n)
                if self
                    .bound
                    .iter()
                    .any(|(b, from, to)| b == n && (*from..=*to).contains(&k))
                    && self.renames(k) =>
            {
                TokenKind::Ident(hygienic(n))
            }
            TokenKind::StrLit(s) => TokenKind::StrLit(self.string_text(s, ctx)),
            TokenKind::FStrText(s) => TokenKind::FStrText(self.string_text(s, ctx)),
            k => k.clone(),
        };
        self.out.push(tok(kind, t.span));
    }

    /// Whether the bound name at `k` is a variable use / binding (not a
    /// member, path segment or `label:`).
    fn renames(&self, k: usize) -> bool {
        if self.bind_pos.contains(&k) {
            return true;
        }
        let b = self.body();
        let prev = k.checked_sub(1).map(|p| &b[p].kind);
        let next = b.get(k + 1).map(|t| &t.kind);
        if matches!(
            prev,
            Some(TokenKind::Dot | TokenKind::ColonColon | TokenKind::At)
        ) || matches!(next, Some(TokenKind::ColonColon))
        {
            return false;
        }
        let label = matches!(
            prev,
            Some(TokenKind::LBrace | TokenKind::Comma | TokenKind::LParen)
        ) && matches!(next, Some(TokenKind::Colon));
        !label
    }

    /// Expand `body[lo..hi]`.
    fn expand(&mut self, lo: usize, hi: usize, ctx: Ctx) {
        let mut k = lo;
        while k < hi {
            if self.body()[k].kind != TokenKind::At {
                if self.body()[k].kind == TokenKind::Impl
                    && !self.t.generics.is_empty()
                    && self.body().get(k + 1).map(|t| &t.kind) != Some(&TokenKind::Lt)
                {
                    // `impl Tr for @Self` on a generic item: the item's
                    // parameters (bounded by `@where_all`).
                    self.emit_plain(k, ctx);
                    let span = self.body()[k].span;
                    let ps = self.impl_params(span);
                    self.out.extend(ps);
                } else {
                    self.emit_plain(k, ctx);
                }
                k += 1;
                continue;
            }
            k = self.template_token(k, hi, ctx);
        }
    }

    /// `@head[.member][(args)][{block}]` at `k`; returns the index after it.
    fn template_token(&mut self, k: usize, hi: usize, ctx: Ctx) -> usize {
        let b = &self.def.body;
        let span = b[k].span;
        let head = match b.get(k + 1).map(|t| &t.kind) {
            Some(TokenKind::SelfUpper) => "Self".to_owned(),
            Some(TokenKind::Ident(n)) => n.clone(),
            _ => {
                self.err(k, "expected a template token name after `@` (spec 6.9.1)");
                return k + 1;
            }
        };
        let mut j = k + 2;
        let members: &[&str] = match head.as_str() {
            "field" => &[
                "name",
                "type",
                "is_pub",
                "has_attr",
                "has_default",
                "default",
            ],
            "Self" => &["name", "is_unit", "is_tuple", "is_named"],
            "variant" => &["name", "pattern", "is_unit", "is_tuple", "is_named"],
            "item" => &["name", "params", "return_type", "body"],
            _ => &[],
        };
        let mut member = String::new();
        if b.get(j).map(|t| &t.kind) == Some(&TokenKind::Dot) {
            if let Some(TokenKind::Ident(m)) = b.get(j + 1).map(|t| &t.kind) {
                if members.contains(&m.as_str()) {
                    member.clone_from(m);
                    j += 2;
                }
            }
        }
        let dot = if member.is_empty() { "" } else { "." };
        let full = format!("{head}{dot}{member}");
        let takes_args = matches!(
            full.as_str(),
            "field.has_attr"
                | "has_derive_attr"
                | "derive_attr"
                | "apply_if"
                | "accept"
                | "where_all"
        );
        let takes_block = matches!(
            full.as_str(),
            "for_fields"
                | "for_variants"
                | "for_variant_fields"
                | "if_struct"
                | "if_enum"
                | "if_union"
                | "apply_if"
        );
        let mut args = (j, j);
        if takes_args {
            match (b.get(j).map(|t| &t.kind), close_of(b, j)) {
                (Some(TokenKind::LParen), Some(end)) if end < hi => {
                    args = (j + 1, end);
                    j = end + 1;
                }
                _ => {
                    self.err(k, format!("`@{full}` needs `(..)` arguments (spec 6.9)"));
                    return j;
                }
            }
        }
        let mut block = (j, j);
        if takes_block {
            match (b.get(j).map(|t| &t.kind), close_of(b, j)) {
                (Some(TokenKind::LBrace), Some(end)) if end < hi => {
                    block = (j + 1, end);
                    j = end + 1;
                }
                _ => {
                    self.err(k, format!("`@{full}` needs a `{{ .. }}` block (spec 6.9)"));
                    return j;
                }
            }
        }
        // `@x.name.as_str()` - the name as a string literal.
        let as_str = matches!(
            full.as_str(),
            "field.name" | "variant.name" | "item.name" | "Self" | "Self.name"
        ) && matches!(
            (
                b.get(j).map(|t| &t.kind),
                b.get(j + 1).map(|t| &t.kind),
                b.get(j + 2).map(|t| &t.kind),
                b.get(j + 3).map(|t| &t.kind),
            ),
            (
                Some(TokenKind::Dot),
                Some(TokenKind::Ident(m)),
                Some(TokenKind::LParen),
                Some(TokenKind::RParen)
            ) if m == "as_str"
        );
        let attr_only = head == "item" || head == "attribute" || head == "accept";
        if attr_only && self.def.kind != MetaKind::Attribute {
            self.err(
                k,
                format!("`@{full}` is only meaningful in an `@attribute` block (spec 6.8.7)"),
            );
            return j;
        }
        if matches!(head.as_str(), "has_derive_attr" | "derive_attr")
            && self.def.field_attr.is_none()
        {
            self.err(
                k,
                format!("`@{head}` needs `#[impl_derive_attribute(Trait, attr)]` (spec 6.9.6)"),
            );
            return j;
        }
        let toks: Vec<Token> = match full.as_str() {
            "Self" => self.self_tokens(span),
            "for_fields" => {
                let (v, n) = match ctx.variant {
                    Some(v) => (v, self.t.variants[v].fields.len()),
                    None if self.t.kind == "enum" => {
                        self.err(
                            k,
                            "`@for_fields` on an enum needs `@for_variants { .. }` around it",
                        );
                        return j;
                    }
                    None => (TOP, self.t.fields.len()),
                };
                for f in 0..n {
                    let c = Ctx {
                        field: Some((v, f)),
                        ..ctx
                    };
                    self.expand(block.0, block.1, c);
                }
                Vec::new()
            }
            "for_variant_fields" => {
                let Some(v) = ctx.variant else {
                    self.err(k, "`@for_variant_fields` outside `@for_variants`");
                    return j;
                };
                for f in 0..self.t.variants[v].fields.len() {
                    let c = Ctx {
                        field: Some((v, f)),
                        ..ctx
                    };
                    self.expand(block.0, block.1, c);
                }
                Vec::new()
            }
            "for_variants" => {
                if self.t.kind != "enum" {
                    self.err(
                        k,
                        format!("`@for_variants` on a {} (needs an enum)", self.t.kind),
                    );
                    return j;
                }
                for v in 0..self.t.variants.len() {
                    let c = Ctx {
                        variant: Some(v),
                        field: None,
                    };
                    self.expand(block.0, block.1, c);
                }
                Vec::new()
            }
            "if_struct" | "if_enum" | "if_union" => {
                if self.t.kind == &head[3..] {
                    self.expand(block.0, block.1, ctx);
                }
                Vec::new()
            }
            "apply_if" => {
                let mut c = Cond {
                    pos: args.0,
                    end: args.1,
                };
                let val = self.cond_or(&mut c, ctx);
                if c.pos != c.end {
                    self.err(c.pos, "unexpected token in `@apply_if` condition");
                }
                if val == Some(true) {
                    self.expand(block.0, block.1, ctx);
                }
                Vec::new()
            }
            "accept" => {
                let ok = b[args.0..args.1].iter().any(|t| {
                    let w = match &t.kind {
                        TokenKind::Ident(w) => w.as_str(),
                        k => rokc_lexer::token::keyword_str(k).unwrap_or(""),
                    };
                    w == self.t.kind
                });
                if !ok {
                    self.err(
                        k,
                        format!(
                            "`#[{}]` cannot be applied to a {} (`@accept` at the template; \
                             spec 6.9.5)",
                            self.def.name, self.t.kind
                        ),
                    );
                }
                Vec::new()
            }
            "where_all" => {
                self.where_all.push(b[args.0..args.1].to_vec());
                Vec::new()
            }
            "field" | "field.name" | "field.type" | "field.default" => {
                let binding = self.binding(ctx);
                let Some(f) = self.field(ctx) else {
                    self.err(k, format!("`@{full}` outside `@for_fields`"));
                    return j;
                };
                match (full.as_str(), binding) {
                    ("field.type", _) => f.ty.clone(),
                    ("field.default", _) if f.default.is_empty() => {
                        let name = f.name.clone();
                        self.err(
                            k,
                            format!(
                                "field `{name}` has no default (guard with \
                                 `@apply_if(@field.has_default)`)"
                            ),
                        );
                        Vec::new()
                    }
                    ("field.default", _) => f.default.clone(),
                    // A variant's payload is reached through its binding.
                    ("field", Some(bn)) => vec![tok(TokenKind::Ident(bn), span)],
                    _ => vec![ident(&f.name, f.span)],
                }
            }
            "field.is_pub" | "field.has_attr" | "field.has_default" | "has_derive_attr"
            | "variant.is_unit" | "variant.is_tuple" | "variant.is_named" | "Self.is_unit"
            | "Self.is_tuple" | "Self.is_named" => {
                let v = self.bool_token(k, &full, args, ctx).unwrap_or(false);
                vec![tok(
                    if v { TokenKind::True } else { TokenKind::False },
                    span,
                )]
            }
            "derive_attr" => {
                let name = self.word(args);
                let Some(f) = self.field(ctx) else {
                    self.err(k, "`@derive_attr` outside `@for_fields`");
                    return j;
                };
                if let Some((_, a)) = f.attrs.iter().find(|(n, _)| *n == name) {
                    a.clone()
                } else {
                    let fname = f.name.clone();
                    self.err(
                        k,
                        format!(
                            "field `{fname}` has no `#[{name}(..)]` (guard with \
                             `@apply_if(@has_derive_attr({name}))`)"
                        ),
                    );
                    Vec::new()
                }
            }
            "variant" | "variant.name" | "variant.pattern" => {
                let Some(v) = ctx.variant else {
                    self.err(k, format!("`@{full}` outside `@for_variants`"));
                    return j;
                };
                if full == "variant.pattern" {
                    self.pattern(v, span)
                } else {
                    vec![tok(TokenKind::Ident(self.t.variants[v].name.clone()), span)]
                }
            }
            "item" => self.t.item.clone(),
            // `@Self.name`: the bare name (no `<T>`), `@Self.name::V { .. }`.
            "item.name" | "Self.name" => vec![tok(TokenKind::Ident(self.t.name.clone()), span)],
            "item.params" | "item.return_type" | "item.body" => {
                let Some((p, r, bd)) = fn_parts(&self.t.item).filter(|_| self.t.kind == "fn")
                else {
                    let kind = self.t.kind;
                    self.err(k, format!("`@{full}` needs a `fn` item, not a {kind}"));
                    return j;
                };
                match full.as_str() {
                    "item.params" => p,
                    "item.return_type" if r.is_empty() => {
                        vec![tok(TokenKind::LParen, span), tok(TokenKind::RParen, span)]
                    }
                    "item.return_type" => r,
                    _ => bd,
                }
            }
            "attribute" => self.t.attr_args.clone(),
            _ => {
                self.err(k, format!("unknown template token `@{full}` (spec 6.9)"));
                return j;
            }
        };
        if as_str {
            // `@Self.as_str()` is the bare name (no `<T>`).
            let text: String = toks
                .iter()
                .take(1)
                .map(|t| match &t.kind {
                    TokenKind::Ident(n) | TokenKind::IntLit(n) => n.as_str(),
                    _ => "",
                })
                .collect();
            self.out.push(tok(TokenKind::StrLit(text), span));
            return j + 4;
        }
        self.out.extend(toks);
        j
    }

    /// The single word argument of `@x(word)`.
    fn word(&self, args: (usize, usize)) -> String {
        self.body()[args.0..args.1]
            .iter()
            .find_map(|t| match &t.kind {
                TokenKind::Ident(w) => Some(w.clone()),
                k => rokc_lexer::token::keyword_str(k).map(str::to_owned),
            })
            .unwrap_or_default()
    }

    fn bool_token(&mut self, k: usize, full: &str, args: (usize, usize), ctx: Ctx) -> Option<bool> {
        if let Some(rest) = full
            .strip_prefix("variant.is_")
            .or_else(|| full.strip_prefix("Self.is_"))
        {
            let shape = if full.starts_with("Self") {
                self.t.shape
            } else if let Some(v) = ctx.variant {
                self.t.variants[v].shape
            } else {
                self.err(k, format!("`@{full}` outside `@for_variants`"));
                return None;
            };
            return Some(
                (rest == "unit" && shape == VariantShape::Unit)
                    || (rest == "tuple" && shape == VariantShape::Tuple)
                    || (rest == "named" && shape == VariantShape::Named),
            );
        }
        let name = self.word(args);
        let Some(f) = self.field(ctx) else {
            self.err(k, format!("`@{full}` outside `@for_fields`"));
            return None;
        };
        Some(match full {
            "field.is_pub" => f.is_pub,
            "field.has_default" => !f.default.is_empty(),
            _ => f.attrs.iter().any(|(n, _)| *n == name),
        })
    }

    // `@apply_if` condition: `||` / `&&` / `!` / `( )` over template
    // booleans and `true` / `false`, evaluated now.
    fn cond_or(&mut self, c: &mut Cond, ctx: Ctx) -> Option<bool> {
        let mut v = self.cond_and(c, ctx)?;
        while c.pos < c.end && self.body()[c.pos].kind == TokenKind::PipePipe {
            c.pos += 1;
            let r = self.cond_and(c, ctx)?;
            v = v || r;
        }
        Some(v)
    }

    fn cond_and(&mut self, c: &mut Cond, ctx: Ctx) -> Option<bool> {
        let mut v = self.cond_not(c, ctx)?;
        while c.pos < c.end && self.body()[c.pos].kind == TokenKind::AmpAmp {
            c.pos += 1;
            let r = self.cond_not(c, ctx)?;
            v = v && r;
        }
        Some(v)
    }

    fn cond_not(&mut self, c: &mut Cond, ctx: Ctx) -> Option<bool> {
        if c.pos >= c.end {
            self.err(c.end, "`@apply_if` condition is incomplete");
            return None;
        }
        match self.body()[c.pos].kind.clone() {
            TokenKind::Bang => {
                c.pos += 1;
                self.cond_not(c, ctx).map(|v| !v)
            }
            TokenKind::LParen => {
                let end = close_of(self.body(), c.pos).filter(|e| *e < c.end)?;
                let mut inner = Cond {
                    pos: c.pos + 1,
                    end,
                };
                let v = self.cond_or(&mut inner, ctx);
                c.pos = end + 1;
                v
            }
            TokenKind::True => {
                c.pos += 1;
                Some(true)
            }
            TokenKind::False => {
                c.pos += 1;
                Some(false)
            }
            TokenKind::At => {
                // Expand the one template token and read its boolean.
                let start = self.out.len();
                let at = c.pos;
                c.pos = self.template_token(at, c.end, ctx);
                let got: Vec<Token> = self.out.drain(start..).collect();
                match got.as_slice() {
                    [t] if t.kind == TokenKind::True => Some(true),
                    [t] if t.kind == TokenKind::False => Some(false),
                    _ => {
                        self.err(at, "`@apply_if` takes template booleans (`@field.is_pub`, `@field.has_attr(x)`, ...)");
                        None
                    }
                }
            }
            _ => {
                self.err(c.pos, "unexpected token in `@apply_if` condition");
                None
            }
        }
    }
}

struct Cond {
    pos: usize,
    end: usize,
}

/// A `fn` item's parameter, return-type and body tokens.
fn fn_parts(item: &[Token]) -> Option<(Vec<Token>, Vec<Token>, Vec<Token>)> {
    // Skip attributes, then find `fn name`.
    let mut k = 0;
    while k < item.len() && item[k].kind != TokenKind::Fn {
        if item[k].kind == TokenKind::HashLBracket {
            k = close_of(item, k)?;
        }
        k += 1;
    }
    k += 2; // `fn name`
    if item.get(k).map(|t| &t.kind) == Some(&TokenKind::Lt) {
        let mut depth = 0i32;
        while k < item.len() {
            match item[k].kind {
                TokenKind::Lt => depth += 1,
                TokenKind::Gt => depth -= 1,
                TokenKind::Shr => depth -= 2,
                _ => {}
            }
            k += 1;
            if depth <= 0 {
                break;
            }
        }
    }
    if item.get(k).map(|t| &t.kind) != Some(&TokenKind::LParen) {
        return None;
    }
    let rp = close_of(item, k)?;
    let params = item[k + 1..rp].to_vec();
    let lb = (rp + 1..item.len()).find(|&m| item[m].kind == TokenKind::LBrace)?;
    let ret = if item.get(rp + 1).map(|t| &t.kind) == Some(&TokenKind::Arrow) {
        item[rp + 2..lb]
            .iter()
            .take_while(
                |t| !matches!(&t.kind, TokenKind::Ident(w) if w == "throws" || w == "where"),
            )
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let rb = close_of(item, lb)?;
    Some((params, ret, item[lb + 1..rb].to_vec()))
}

/// A meta name `find_meta` resolved.
pub(crate) enum MetaUse {
    Def(MetaDef),
    /// No one meta block: a wrong qualifier or an ambiguous bare name
    /// (reported).
    Bad,
}

/// The standard library's meta names stay bare (`#[derive(Inspect)]`).
fn is_std(package: &str) -> bool {
    package == "rok" || package.starts_with("rok::")
}

/// How a use of meta name `name` of `kind` reads, qualified by `qual`.
fn meta_use(kind: MetaKind, qual: &str, name: &str) -> String {
    let path = if qual.is_empty() {
        name.to_owned()
    } else {
        format!("{qual}::{name}")
    };
    match kind {
        MetaKind::Derive => format!("`#[derive({path})]`"),
        MetaKind::Attribute => format!("`#[{path}]`"),
        MetaKind::Macro => format!("`{path}!`"),
    }
}

const fn kind_word(kind: MetaKind) -> &'static str {
    match kind {
        MetaKind::Derive => "derive",
        MetaKind::Attribute => "attribute",
        MetaKind::Macro => "macro",
    }
}

/// Every path `t` imports, a glob ending in `*` / `**`; a macro
/// import's `!` is dropped.
pub(crate) fn use_leaves(t: &rokc_ast::UseTree, outer: &[String], out: &mut Vec<Vec<String>>) {
    let mut path = outer.to_vec();
    path.extend(
        t.prefix
            .segments
            .iter()
            .map(|s| s.name.trim_end_matches('!').to_owned()),
    );
    match &t.kind {
        rokc_ast::UseTreeKind::Simple(_) => out.push(path),
        rokc_ast::UseTreeKind::Glob => {
            path.push("*".to_owned());
            out.push(path);
        }
        rokc_ast::UseTreeKind::GlobRecursive => {
            path.push("**".to_owned());
            out.push(path);
        }
        rokc_ast::UseTreeKind::Group(ts) => {
            for c in ts {
                use_leaves(c, &path, out);
            }
        }
    }
}

impl Parser<'_> {
    /// The package a meta qualifier names; `pkg` is this one.
    fn meta_package(&self, qual: &[String]) -> String {
        match qual {
            [p] if p == "pkg" => self.package.clone(),
            [p] if p.starts_with(PKG_ROOT) => p[PKG_ROOT.len()..].to_owned(),
            _ => qual.join("::"),
        }
    }

    /// How this package writes `package` as a qualifier; `None` when it
    /// cannot (the package being built, seen from a dependency).
    fn meta_qualifier(&self, package: &str) -> Option<String> {
        if package == self.package {
            Some("pkg".to_owned())
        } else if package.is_empty() {
            None
        } else {
            Some(package.to_owned())
        }
    }

    /// The meta block of `kind` that `qual::name` (bare `name` when `qual`
    /// is empty) at `at` uses. `None`: not a meta use, i.e. a bare name no
    /// block registers, or a qualified derive or attribute no block of
    /// that name registers (built in, or unknown).
    pub(crate) fn find_meta(
        &mut self,
        kind: MetaKind,
        qual: &[String],
        name: &str,
        at: Span,
    ) -> Option<MetaUse> {
        let meta = std::sync::Arc::clone(&self.meta);
        let all: Vec<&MetaDef> = meta.find_all(kind, name).collect();
        if qual.is_empty() {
            return match all.as_slice() {
                [] => None,
                [d] => {
                    self.unqualified_meta(d, at);
                    Some(MetaUse::Def((*d).clone()))
                }
                _ => Some(self.ambiguous_meta(kind, name, &all, at)),
            };
        }
        let package = self.meta_package(qual);
        if let Some(d) = all.iter().find(|d| d.package == package) {
            return Some(MetaUse::Def((*d).clone()));
        }
        let def = all
            .iter()
            .find(|d| d.package == self.package)
            .or_else(|| all.first())
            .copied();
        if def.is_none() && kind == MetaKind::Attribute {
            return None;
        }
        let help = match &def {
            Some(d) => {
                let q = self.meta_qualifier(&d.package).unwrap_or_default();
                format!("write {}", meta_use(kind, &q, name))
            }
            None if kind == MetaKind::Macro => format!(
                "no package defines `{name}!`; a package defines one with \
                 `#[impl_macro({name})] pub @macro {{ .. }}` (spec 6.8.12)"
            ),
            None if crate::derive::derivable(name).is_some() => {
                format!("write `#[derive({name})]`: it is built in")
            }
            None => "remove it, or name the package whose meta block defines it".to_owned(),
        };
        let at = self.name_span(at, name);
        let shown = if package.is_empty() { "pkg" } else { &package };
        let msg = format!(
            "package `{shown}` defines no {} `{name}`, so {} names nothing",
            kind_word(kind),
            meta_use(kind, &qual.join("::"), name)
        );
        if kind == MetaKind::Derive {
            // Ignored before 0.1.6, so only a warning.
            self.diagnostics.push(
                RokDiagnostic::warning(ErrorKind::Meta, at, format!("{msg} and does nothing"))
                    .with_file(self.file.clone())
                    .with_help(help),
            );
            return None;
        }
        self.diagnostics.push(
            meta_err(at, msg)
                .with_file(self.file.clone())
                .with_help(help),
        );
        Some(MetaUse::Bad)
    }

    /// A bare `name` several packages register: this package's, else the
    /// one a `use` imports; else an error saying which to write.
    fn ambiguous_meta(
        &mut self,
        kind: MetaKind,
        name: &str,
        all: &[&MetaDef],
        at: Span,
    ) -> MetaUse {
        let pick = all
            .iter()
            .find(|d| d.package == self.package)
            .or_else(|| all.iter().find(|d| self.meta_imported(d)));
        if let Some(d) = pick {
            return MetaUse::Def((*d).clone());
        }
        let forms: Vec<String> = all
            .iter()
            .filter_map(|d| self.meta_qualifier(&d.package))
            .map(|q| meta_use(kind, &q, name))
            .collect();
        let mut d = meta_err(
            at,
            format!(
                "{} is defined by several packages, so the bare name is ambiguous (spec 6.8.11)",
                meta_use(kind, "", name)
            ),
        )
        .with_file(self.file.clone())
        .with_help(format!("write one of {}", forms.join(", ")));
        for def in all {
            d = d.with_secondary(def.span, "defined here");
        }
        self.diagnostics.push(d);
        MetaUse::Bad
    }

    /// `unqualified_meta`: `def`, another package's, used by its bare name
    /// and found only through the build-wide scope that 0.2.0 drops.
    fn unqualified_meta(&mut self, def: &MetaDef, at: Span) {
        if !self.meta_stack.is_empty()
            || at.is_dummy()
            || def.package == self.package
            || is_std(&def.package)
            || self.meta_imported(def)
        {
            return;
        }
        let Some(q) = self.meta_qualifier(&def.package) else {
            return;
        };
        if !self
            .meta_warned
            .insert(format!("{:?}:{}", def.kind, def.name))
        {
            return;
        }
        let at = self.name_span(at, &def.name);
        let name = &def.name;
        let mut help = format!("write {}", meta_use(def.kind, &q, name));
        if def.kind == MetaKind::Macro {
            help.push_str(&format!(" or add `use {q}::{name}!;`"));
        }
        let d = RokDiagnostic::warning(
            ErrorKind::Meta,
            at,
            format!(
                "{} is package `{q}`'s {}, found only through the build-wide meta scope, \
                 which Rok 0.2.0 removes",
                meta_use(def.kind, "", name),
                kind_word(def.kind)
            ),
        )
        .with_file(self.file.clone())
        .with_help(help)
        .with_fix(
            at,
            format!("{q}::{name}"),
            format!("write {}", meta_use(def.kind, &q, name)),
        )
        .with_lint("unqualified_meta");
        self.meta_lints.push(d);
    }

    /// Where `name` is written in the attribute at `at` (`#[name]`,
    /// `#[derive(dep::name)]`); `at` itself for a macro call.
    fn name_span(&self, at: Span, name: &str) -> Span {
        let Some(k) = self
            .tokens
            .iter()
            .position(|t| t.span == at && t.kind == TokenKind::HashLBracket)
        else {
            return at;
        };
        self.tokens[k..]
            .iter()
            .take_while(|t| t.kind != TokenKind::RBracket)
            .find(|t| matches!(&t.kind, TokenKind::Ident(n) if n == name))
            .map_or(at, |t| t.span)
    }

    /// Whether a `use` of the file imports `def`: its path (`use
    /// json::log!;`, or `use mdep::Tag;` for a derive named like its
    /// trait) or a glob of its package.
    fn meta_imported(&self, def: &MetaDef) -> bool {
        let pkg: Vec<&str> = def.package.split("::").collect();
        self.meta_imports.iter().any(|p| {
            p.len() > pkg.len()
                && p.iter().zip(&pkg).all(|(a, b)| a == b)
                && p.last().is_some_and(|l| {
                    *l == def.name || (p.len() == pkg.len() + 1 && (l == "*" || l == "**"))
                })
        })
    }

    /// Note every path the file's `use`s import (`meta_imports`) before
    /// the parse meets a meta name.
    pub(crate) fn scan_meta_imports(&mut self) {
        let mut out = Vec::new();
        for (k, t) in self.tokens.iter().enumerate() {
            if t.kind != TokenKind::Use {
                continue;
            }
            let mut p = Parser::new(self.tokens, self.file.clone());
            p.pos = k + 1;
            if let Some(tree) = p.parse_use_tree() {
                use_leaves(&tree, &[], &mut out);
            }
        }
        self.meta_imports = out;
    }

    /// Drop the macro imports (`use dep::name!;`) from `t`, checking
    /// each; `None` when nothing is left. They import no item, only say
    /// where a bare `name!` comes from.
    pub(crate) fn strip_meta_imports(
        &mut self,
        t: rokc_ast::UseTree,
        outer: &[String],
        public: bool,
        at: Span,
    ) -> Option<rokc_ast::UseTree> {
        let mut path = outer.to_vec();
        path.extend(t.prefix.segments.iter().map(|s| s.name.clone()));
        match t.kind {
            rokc_ast::UseTreeKind::Simple(_) if path.last().is_some_and(|n| n.ends_with('!')) => {
                let last = path.pop().unwrap_or_default();
                let name = last.trim_end_matches('!');
                let shown = if path.is_empty() {
                    "dep".to_owned()
                } else {
                    path.join("::")
                };
                if public {
                    self.diagnostics.push(
                        meta_err(
                            at,
                            format!("`pub use` cannot re-export the macro `{name}!`"),
                        )
                        .with_file(self.file.clone())
                        .with_help(format!("write `use {shown}::{name}!;`")),
                    );
                } else if path.is_empty() {
                    self.diagnostics.push(
                        meta_err(
                            at,
                            format!("`use {name}!;` does not name the package of `{name}!`"),
                        )
                        .with_file(self.file.clone())
                        .with_help(format!(
                            "write `use dep::{name}!;`, where `dep` is the package that defines it"
                        )),
                    );
                } else if !self.meta.is_empty() {
                    if let Some(MetaUse::Def(d)) = self.find_meta(MetaKind::Macro, &path, name, at)
                    {
                        self.meta_visible(&d, at);
                    }
                }
                None
            }
            rokc_ast::UseTreeKind::Group(ts) if !ts.is_empty() => {
                let kept: Vec<_> = ts
                    .into_iter()
                    .filter_map(|c| self.strip_meta_imports(c, &path, public, at))
                    .collect();
                (!kept.is_empty()).then(|| rokc_ast::UseTree {
                    prefix: t.prefix,
                    kind: rokc_ast::UseTreeKind::Group(kept),
                })
            }
            kind => Some(rokc_ast::UseTree {
                prefix: t.prefix,
                kind,
            }),
        }
    }

    /// `#[cfg(..)] mod m {`: enter it; its closing `}` index and `cfg`s.
    /// `None` consumes nothing.
    fn cfg_mod_open(&mut self) -> Option<(usize, Vec<Attribute>)> {
        if self.peek() != &TokenKind::HashLBracket {
            return None;
        }
        let (start, n_diags) = (self.pos, self.diagnostics.len());
        let attrs = self.parse_outer_attrs();
        self.parse_visibility();
        let open = self.pos + 2;
        let is_mod = self.peek() == &TokenKind::Mod
            && matches!(
                self.tokens.get(open).map(|t| &t.kind),
                Some(TokenKind::LBrace)
            );
        let cfg: Vec<Attribute> = attrs.into_iter().filter(|a| a.name() == "cfg").collect();
        let Some(end) = (is_mod && !cfg.is_empty())
            .then(|| close_of(self.tokens, open))
            .flatten()
        else {
            self.pos = start;
            self.diagnostics.truncate(n_diags);
            return None;
        };
        self.pos = open + 1;
        Some((end, cfg))
    }

    /// Consume a meta block at `#[..] pub? @derive { .. }`. `None`: not one
    /// (nothing consumed); `Some(None)`: malformed (reported).
    pub(crate) fn meta_block(&mut self) -> Option<Option<MetaDef>> {
        if self.peek() != &TokenKind::HashLBracket {
            return None;
        }
        let (start, n_diags) = (self.pos, self.diagnostics.len());
        let attrs = self.parse_outer_attrs();
        let Some(ma) = attrs.iter().find(|a| is_meta_attr(a)).cloned() else {
            self.pos = start;
            self.diagnostics.truncate(n_diags);
            return None;
        };
        let vis = self.parse_visibility();
        let starter = if self.eat(&TokenKind::At) {
            match self.peek().clone() {
                TokenKind::Ident(n) => {
                    self.bump();
                    n
                }
                _ => String::new(),
            }
        } else {
            String::new()
        };
        let want = match ma.name() {
            "impl_attribute" => "attribute",
            "impl_macro" => "macro",
            _ => "derive",
        };
        let body_start = self.pos;
        let Some(end) = (self.peek() == &TokenKind::LBrace)
            .then(|| close_of(self.tokens, self.pos))
            .flatten()
        else {
            self.diagnostics.push(meta_err(
                ma.span,
                format!(
                    "`#[{}(..)]` needs a `{}@{want} {{ .. }}` body (spec 6.8)",
                    ma.name(),
                    if vis == Visibility::Private {
                        ""
                    } else {
                        "pub "
                    }
                ),
            ));
            return Some(None);
        };
        self.pos = end + 1;
        self.unparsed.push(body_start..self.pos);
        if starter != want {
            self.diagnostics.push(meta_err(
                ma.span,
                format!(
                    "`#[{}(..)]` takes an `@{want} {{ .. }}` body, not `@{starter}` (spec 6.8.7)",
                    ma.name()
                ),
            ));
            return Some(None);
        }
        let words = attr_words(&ma);
        let arity = if ma.name() == "impl_derive_attribute" {
            2
        } else {
            1
        };
        if words.len() != arity {
            self.diagnostics.push(meta_err(
                ma.span,
                format!(
                    "`#[{}]` takes {arity} name argument(s) (spec 6.8.1)",
                    ma.name()
                ),
            ));
            return Some(None);
        }
        Some(Some(MetaDef {
            kind: match want {
                "derive" => MetaKind::Derive,
                "macro" => MetaKind::Macro,
                _ => MetaKind::Attribute,
            },
            name: words[0].clone(),
            field_attr: words.get(1).cloned(),
            public: vis != Visibility::Private,
            file: self.file.clone(),
            package: String::new(),
            span: ma.span,
            body: self.tokens[body_start + 1..end].to_vec(),
            cfg: attrs.into_iter().filter(|a| a.name() == "cfg").collect(),
        }))
    }

    /// Whether `def` may be used from this file (6.8.6: a private block
    /// only in its own module).
    fn meta_visible(&mut self, def: &MetaDef, at: Span) -> bool {
        if def.public || def.file == self.file {
            return true;
        }
        self.diagnostics.push(
            meta_err(
                at,
                format!(
                    "meta block `{}` is private to `{}` (mark it `pub @..`; spec 6.8.6)",
                    def.name, def.file
                ),
            )
            .with_secondary(def.span, "defined here"),
        );
        false
    }

    /// Expand the first user attribute (`#[impl_attribute]`) on the item
    /// parsed from `self.tokens[from..self.pos]`; `None` if it has none.
    pub(crate) fn expand_user_attribute(&mut self, item: &Item, from: usize) -> Option<Vec<Item>> {
        let mut found = None;
        for a in item.attrs() {
            let Some((name, qual)) = a.path.split_last() else {
                continue;
            };
            if a.path == ["wit", "export"] {
                continue;
            }
            if let Some(MetaUse::Def(d)) = self.find_meta(MetaKind::Attribute, qual, name, a.span) {
                found = Some((d, a.clone()));
                break;
            }
        }
        let (def, a) = found?;
        if !self.meta_visible(&def, a.span) {
            return Some(Vec::new());
        }
        let toks = &self.tokens[from..self.pos];
        let at = find_span(toks, 0, a.span).filter(|k| toks[*k].kind == TokenKind::HashLBracket);
        let (item_toks, attr_args) = match at.and_then(|k| Some((k, close_of(toks, k)?))) {
            Some((k, end)) => {
                let mut v = toks[..k].to_vec();
                v.extend_from_slice(&toks[end + 1..]);
                (v, attr_arg_tokens(toks, k))
            }
            None => (toks.to_vec(), Vec::new()),
        };
        let mut target = self.target(item, from, a.span);
        target.item = item_toks;
        target.attr_args = attr_args;
        Some(self.run_meta(&def, &target, item))
    }

    /// Expand the user derives of a struct / enum / union and drop them
    /// from its `#[derive(..)]` (the rest are built-in).
    pub(crate) fn expand_user_derives(&mut self, mut item: Item, from: usize) -> (Item, Vec<Item>) {
        let mut out = Vec::new();
        if !matches!(item, Item::Struct(_) | Item::Enum(_) | Item::Union(_)) {
            return (item, out);
        }
        // What expands, and what leaves `#[derive(..)]`: those and the
        // ones reported.
        let mut users: Vec<(MetaDef, Span)> = Vec::new();
        let mut taken: Vec<Vec<String>> = Vec::new();
        for (path, at) in crate::derive::derive_paths(item.attrs()) {
            let Some((name, qual)) = path.split_last() else {
                continue;
            };
            match self.find_meta(MetaKind::Derive, qual, name, at) {
                Some(MetaUse::Def(d)) => users.push((d, at)),
                Some(MetaUse::Bad) => {}
                None => continue,
            }
            taken.push(path);
        }
        if taken.is_empty() {
            return (item, out);
        }
        for (def, at) in &users {
            if self.meta_visible(def, *at) {
                let target = self.target(&item, from, *at);
                out.extend(self.run_meta(def, &target, &item));
            }
        }
        {
            let attrs = item.attrs_mut();
            for a in attrs.iter_mut().filter(|a| a.path == ["derive"]) {
                if let AttrBody::Args(args) = &mut a.body {
                    args.retain(|x| {
                        let path = match x {
                            AttrArg::Word(w) => std::slice::from_ref(w),
                            AttrArg::Nested { path, body } if matches!(**body, AttrBody::Empty) => {
                                path.as_slice()
                            }
                            _ => return true,
                        };
                        !taken.iter().any(|p| p == path)
                    });
                }
            }
            attrs.retain(|a| {
                !(a.path == ["derive"] && matches!(&a.body, AttrBody::Args(v) if v.is_empty()))
            });
        }
        (item, out)
    }

    /// The item parsed from `self.tokens[from..self.pos]` as a target.
    fn target(&self, item: &Item, from: usize, at: Span) -> Target {
        let toks = &self.tokens[from..self.pos];
        let mut cursor = 0;
        let mut field =
            |name: String, vis: Visibility, attrs: &[Attribute], span: Span, named: bool| {
                let mut fa = Vec::new();
                for a in attrs {
                    if let Some(k) = find_span(toks, cursor, a.span) {
                        fa.push((a.name().to_owned(), attr_arg_tokens(toks, k)));
                        cursor = k + 1;
                    }
                }
                let mut ty = Vec::new();
                let mut default = Vec::new();
                if let Some(k) = find_span(toks, cursor, span) {
                    // A named field's type follows `name :`.
                    let start = if named { k + 2 } else { k };
                    let mut p = Parser::new(toks, self.file.clone());
                    p.pos = start;
                    if p.parse_type().is_some() {
                        ty = toks[start..p.pos].to_vec();
                        cursor = p.pos;
                        // `= expr` up to the field's `;` / `,` / `}`.
                        if named && toks.get(cursor).map(|t| &t.kind) == Some(&TokenKind::Eq) {
                            let mut end = cursor + 1;
                            while let Some(t) = toks.get(end) {
                                match t.kind {
                                    TokenKind::Semi | TokenKind::Comma | TokenKind::RBrace => break,
                                    TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                                        end = close_of(toks, end).unwrap_or(toks.len());
                                    }
                                    _ => {}
                                }
                                end += 1;
                            }
                            default = toks[cursor + 1..end.min(toks.len())].to_vec();
                            cursor = end;
                        }
                    }
                }
                FieldInfo {
                    name,
                    ty,
                    is_pub: vis != Visibility::Private,
                    attrs: fa,
                    default,
                    span,
                }
            };
        let named = |fs: &[rokc_ast::NamedField],
                     field: &mut dyn FnMut(
            String,
            Visibility,
            &[Attribute],
            Span,
            bool,
        ) -> FieldInfo| {
            fs.iter()
                .map(|f| field(f.name.clone(), f.vis, &f.attrs, f.span, true))
                .collect::<Vec<_>>()
        };
        let shape = match item {
            Item::Struct(s) => match &s.kind {
                StructKind::Named(_) => VariantShape::Named,
                StructKind::Tuple(_) => VariantShape::Tuple,
                StructKind::Unit => VariantShape::Unit,
            },
            _ => VariantShape::Named,
        };
        let (kind, name, generics, fields, variants) = match item {
            Item::Struct(s) => {
                let fields = match &s.kind {
                    StructKind::Named(fs) => named(fs, &mut field),
                    StructKind::Tuple(fs) => fs
                        .iter()
                        .enumerate()
                        .map(|(i, f)| field(i.to_string(), f.vis, &f.attrs, f.ty.span, false))
                        .collect(),
                    StructKind::Unit => Vec::new(),
                };
                (
                    "struct",
                    s.name.clone(),
                    s.generics.params.clone(),
                    fields,
                    Vec::new(),
                )
            }
            Item::Union(u) => (
                "union",
                u.name.clone(),
                u.generics.params.clone(),
                named(&u.fields, &mut field),
                Vec::new(),
            ),
            Item::Enum(e) => {
                let variants = e
                    .variants
                    .iter()
                    .map(|v| {
                        let (shape, fields) = match &v.kind {
                            EnumVariantKind::Unit | EnumVariantKind::Value(_) => {
                                (VariantShape::Unit, Vec::new())
                            }
                            EnumVariantKind::Tuple(tys) => (
                                VariantShape::Tuple,
                                tys.iter()
                                    .enumerate()
                                    .map(|(i, t)| {
                                        field(i.to_string(), Visibility::Pub, &[], t.span, false)
                                    })
                                    .collect(),
                            ),
                            EnumVariantKind::Named(fs) => {
                                (VariantShape::Named, named(fs, &mut field))
                            }
                        };
                        VariantInfo {
                            name: v.name.clone(),
                            shape,
                            fields,
                        }
                    })
                    .collect();
                (
                    "enum",
                    e.name.clone(),
                    e.generics.params.clone(),
                    Vec::new(),
                    variants,
                )
            }
            Item::Fn(f) => ("fn", f.name.clone(), Vec::new(), Vec::new(), Vec::new()),
            Item::Trait(t) => ("trait", t.name.clone(), Vec::new(), Vec::new(), Vec::new()),
            Item::Impl(_) => ("impl", String::new(), Vec::new(), Vec::new(), Vec::new()),
            _ => ("item", String::new(), Vec::new(), Vec::new(), Vec::new()),
        };
        Target {
            kind,
            name,
            generics,
            shape,
            fields,
            variants,
            item: toks.to_vec(),
            attr_args: Vec::new(),
            at,
        }
    }

    /// Expand `def` over `target` and parse the result (expanding again
    /// what it produces - 6.8.8).
    fn run_meta(&mut self, def: &MetaDef, target: &Target, item: &Item) -> Vec<Item> {
        let key = format!("{:?}:{}:{}", def.kind, def.name, def.package);
        if self.meta_stack.contains(&key) || self.meta_stack.len() >= 32 {
            let chain: Vec<&str> = self
                .meta_stack
                .iter()
                .map(|k| k.split(':').nth(1).unwrap_or(""))
                .collect();
            self.diagnostics.push(
                meta_err(
                    target.at,
                    format!(
                        "cyclic meta expansion: {} → {} (spec 6.8.8)",
                        chain.join(" → "),
                        def.name
                    ),
                )
                .with_secondary(def.span, "this meta block"),
            );
            return Vec::new();
        }
        let mut ex = Expander {
            def,
            t: target,
            out: Vec::new(),
            diags: Vec::new(),
            where_all: Vec::new(),
            bound: Vec::new(),
            bind_pos: HashSet::new(),
        };
        ex.scan_binders();
        ex.expand(0, def.body.len(), Ctx::default());
        let (tokens, diags) = (ex.out, ex.diags);
        let failed = !diags.is_empty();
        self.diagnostics.extend(diags);
        if failed {
            return Vec::new();
        }
        let what = match def.kind {
            MetaKind::Derive => format!("expanded from this `#[derive({})]`", def.name),
            MetaKind::Attribute => format!("expanded from this `#[{}]`", def.name),
            MetaKind::Macro => format!("expanded from this `{}!` macro", def.name),
        };
        let (file, at) = (&self.file, target.at);
        self.expanded.push(format!(
            "// {what} ({file}:{}:{})\n{}",
            at.line,
            at.col,
            render(&tokens)
        ));
        let mut p = Parser::new(&tokens, self.file.clone());
        p.ext = self.ext.clone();
        p.meta = Arc::clone(&self.meta);
        p.literals = Arc::clone(&self.literals);
        p.cfg.clone_from(&self.cfg);
        p.package.clone_from(&def.package);
        p.meta_stack.clone_from(&self.meta_stack);
        p.meta_stack.push(key);
        let mut items = Vec::new();
        while !p.at_eof() {
            let from = p.pos;
            if p.meta_block().is_some() {
                continue;
            }
            match p.parse_item() {
                Some(i) => {
                    items.push(i);
                    items.append(&mut p.pending_items);
                }
                None if std::mem::take(&mut p.vanished) => {}
                None if p.pos == from => break,
                None => {}
            }
        }
        if p.pos < tokens.len() && p.diagnostics.is_empty() {
            let s = tokens[p.pos].span;
            p.diagnostics.push(meta_err(
                s,
                "expected an item in the meta block's expansion",
            ));
        }
        self.expanded.append(&mut p.expanded);
        // Errors in generated code point at the template and at the use.
        for mut d in p.diagnostics {
            if d.primary.as_ref().is_some_and(|l| l.span.file != 0) {
                d.file = None;
            }
            if !d.secondary.iter().any(|l| l.span == target.at) {
                d = d.with_secondary(target.at, what.clone());
            }
            self.diagnostics.push(d);
        }
        // 6.9.8: `#[cfg]` runs before expansion - what the item's cfg
        // removes, its expansion goes with it.
        let cfgs: Vec<&Attribute> = item.attrs().iter().filter(|a| a.name() == "cfg").collect();
        for i in &mut items {
            i.attrs_mut().extend(cfgs.iter().map(|a| (*a).clone()));
        }
        items
    }
}

/// Tokens as source text: one statement / brace per line, indented.
/// Long expressions are not line-broken.
pub(super) fn render(tokens: &[Token]) -> String {
    use TokenKind as K;
    let mut out = String::new();
    let mut depth = 0usize;
    let mut prev: Option<&K> = None;
    // `},` ends a match arm / struct field: break after the comma too.
    let mut brace_comma = false;
    // Open `[` / `#[` (an attribute's `]` ends its line).
    let mut brackets: Vec<bool> = Vec::new();
    let mut attr_end = false;
    let mut fstr = 0u32;
    for t in tokens {
        let k = &t.kind;
        if *k == K::RBrace {
            depth = depth.saturating_sub(1);
        }
        // A new line after `{` / `;`, and after `}` unless what follows
        // continues it (`},` `})` `} else` `}.m()` ..).
        let continues = matches!(
            k,
            K::Comma
                | K::Semi
                | K::RParen
                | K::RBracket
                | K::Dot
                | K::Question
                | K::Else
                | K::FatArrow
        ) || matches!(k, K::Ident(w) if w == "catch" || w == "finally");
        // F-string pieces stay on one line, glued to their neighbours.
        let in_fstr = fstr > 0;
        match k {
            K::FStrStart(_) => fstr += 1,
            K::FStrEnd(_) => fstr = fstr.saturating_sub(1),
            _ => {}
        }
        let new_line = match prev {
            _ if in_fstr => false,
            None | Some(K::LBrace | K::Semi) => true,
            Some(K::RBracket) if attr_end => true,
            Some(K::Comma) if brace_comma => *k != K::RParen,
            Some(K::RBrace) => !continues,
            _ => *k == K::RBrace,
        };
        if new_line {
            if prev.is_some() {
                out.push('\n');
            }
            out.push_str(&"    ".repeat(depth));
        } else {
            let glued_after = matches!(
                prev,
                Some(
                    K::LParen
                        | K::LBracket
                        | K::Dot
                        | K::ColonColon
                        | K::HashLBracket
                        | K::Bang
                        | K::FStrStart(_)
                        | K::FStrText(_)
                        | K::HoleStart
                        | K::HoleSpec
                        | K::SpecText(_)
                        | K::HoleEnd
                )
            );
            let glued_before = matches!(
                k,
                K::FStrText(_)
                    | K::HoleStart
                    | K::HoleSpec
                    | K::SpecText(_)
                    | K::HoleEnd
                    | K::FStrEnd(_)
                    | K::RParen
                    | K::RBracket
                    | K::Comma
                    | K::Semi
                    | K::Dot
                    | K::ColonColon
                    | K::Colon
                    | K::Question
            ) || (matches!(k, K::LParen | K::LBracket)
                && matches!(
                    prev,
                    Some(
                        K::Ident(_) | K::RParen | K::RBracket | K::Gt | K::SelfLower | K::SelfUpper
                    )
                ));
            if !glued_after && !glued_before {
                out.push(' ');
            }
        }
        match k {
            // A block's `pkg::` as the package it names.
            K::Ident(w) if w.starts_with(PKG_ROOT) => {
                let package = &w[PKG_ROOT.len()..];
                out.push_str(if package.is_empty() { "pkg" } else { package });
            }
            _ => out.push_str(&k.text()),
        }
        if *k == K::LBrace {
            depth += 1;
        }
        brace_comma = *k == K::Comma && prev == Some(&K::RBrace);
        attr_end = false;
        match k {
            K::LBracket => brackets.push(false),
            K::HashLBracket | K::HashBangLBracket => brackets.push(true),
            K::RBracket => attr_end = brackets.pop().unwrap_or(false),
            _ => {}
        }
        prev = Some(k);
    }
    out.push('\n');
    out
}

rokc_core::codec_enum!(MetaKind {
    Derive,
    Attribute,
    Macro
});
rokc_core::codec_struct!(MetaDef {
    kind,
    name,
    field_attr,
    public,
    file,
    package,
    span,
    body,
    cfg
});
