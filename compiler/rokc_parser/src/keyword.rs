//! `#[keyword("name")]` (refs-lang 6.6).
//!
//! A driver pre-pass collects them (`collect_keywords`) before the package
//! is lexed for real; `apply_keywords` then rewrites each `name` token to
//! the handler's path, so `with(m) { .. }` calls `pkg::with_handler(m)
//! { .. }`, and `name` in a declaration position is an error. The
//! handler's signature decides the call shape (6.6.8).
//!
//! On a `struct` (`#[keyword("int")] struct Int`) the keyword is a type
//! name, `name::item` included. Standard library type keywords apply to
//! every package (`keyword_tables`).

use rokc_ast::{AttrArg, AttrBody, Attribute, FnDef, ImplItem, Item, Literal, TraitItem};
use rokc_diagnostic::{Code, RokDiagnostic, Span};
use rokc_lexer::token::{RESERVED, keyword_kind};
use rokc_lexer::{Token, TokenKind};

#[derive(Clone, Debug)]
pub struct KeywordDef {
    pub name: String,
    /// The handler function's path in its file (`f`, `inner::f`), or the
    /// struct's name for a type keyword.
    pub handler: String,
    /// On a `struct`: a type name, not a call.
    pub ty: bool,
    pub file: String,
    /// The `#[keyword(..)]` attribute.
    pub span: Span,
    /// The item's and its enclosing modules' `#[cfg(..)]`s: the driver
    /// registers it only when they hold (6.6.13).
    pub cfg: Vec<Attribute>,
}

/// Identifiers the language already gives meaning to in fixed positions
/// (contextual keywords, path roots).
const CONTEXTUAL: &[&str] = &[
    "type", "any", "in", "where", "throws", "rethrows", "catch", "finally", "const", "static",
    "extern", "mut", "union", "pkg", "sub", "sup", "rok",
];

/// Every `#[keyword("..")]` in `items` (inline `mod`s included). One on
/// anything but a free `fn` or `struct` gets an empty handler, reported
/// by `check_keyword`.
#[must_use]
pub fn collect_keywords(items: &[Item], file: &str) -> Vec<KeywordDef> {
    let mut out = Vec::new();
    collect_in(items, &mut Vec::new(), &[], file, &mut out);
    out
}

fn collect_in(
    items: &[Item],
    path: &mut Vec<String>,
    cfg: &[Attribute],
    file: &str,
    out: &mut Vec<KeywordDef>,
) {
    let named = |path: &[String], name: &str| {
        path.iter()
            .map(String::as_str)
            .chain([name])
            .collect::<Vec<_>>()
            .join("::")
    };
    for item in items {
        let attrs = item.attrs();
        let mut cfg = cfg.to_vec();
        cfg.extend(attrs.iter().filter(|a| a.name() == "cfg").cloned());
        let (handler, ty) = match item {
            Item::Fn(f) => (named(path, &f.name), false),
            Item::Struct(s) => (named(path, &s.name), true),
            _ => (String::new(), false),
        };
        push_keywords(attrs, &handler, ty, &cfg, file, out);
        // A method or trait item can't be a handler (6.6.2).
        let methods: Vec<&FnDef> = match item {
            Item::Impl(i) => i
                .items
                .iter()
                .filter_map(|m| match m {
                    ImplItem::Method(f) => Some(f),
                    _ => None,
                })
                .collect(),
            Item::Trait(t) => t
                .items
                .iter()
                .filter_map(|m| match m {
                    TraitItem::Method(f) => Some(f),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        for f in methods {
            push_keywords(&f.attrs, "", false, &cfg, file, out);
        }
        if let Item::Mod(rokc_ast::ModDef {
            name,
            items: Some(inner),
            ..
        }) = item
        {
            path.push(name.clone());
            collect_in(inner, path, &cfg, file, out);
            path.pop();
        }
    }
}
fn push_keywords(
    attrs: &[Attribute],
    handler: &str,
    ty: bool,
    cfg: &[Attribute],
    file: &str,
    out: &mut Vec<KeywordDef>,
) {
    for a in attrs.iter().filter(|a| a.name() == "keyword") {
        let AttrBody::Args(args) = &a.body else {
            continue;
        };
        let [AttrArg::Lit(Literal::Str(name))] = args.as_slice() else {
            continue;
        };
        out.push(KeywordDef {
            name: name.clone(),
            handler: handler.to_owned(),
            ty,
            file: file.to_owned(),
            span: a.span,
            cfg: cfg.to_vec(),
        });
    }
}

/// Path (`f`, `a::b::f`) of the first `fn` after an attribute; empty if
/// none.
pub(crate) fn handler_at(tokens: &[Token], attr: usize, after: usize) -> String {
    let Some(name) = tokens[after.min(tokens.len())..]
        .windows(2)
        .find(|p| p[0].kind == TokenKind::Fn)
        .and_then(|p| match &p[1].kind {
            TokenKind::Ident(f) => Some(f.clone()),
            _ => None,
        })
    else {
        return String::new();
    };
    // Inline `mod m {` still open at the attribute (brace depth).
    let mut mods: Vec<(String, usize)> = Vec::new();
    let mut depth = 0usize;
    for (i, t) in tokens[..attr].iter().enumerate() {
        match &t.kind {
            TokenKind::LBrace => {
                depth += 1;
                if let (Some(TokenKind::Mod), Some(TokenKind::Ident(m))) = (
                    i.checked_sub(2).map(|j| &tokens[j].kind),
                    i.checked_sub(1).map(|j| &tokens[j].kind),
                ) {
                    mods.push((m.clone(), depth));
                }
            }
            TokenKind::RBrace => {
                if mods.last().is_some_and(|(_, d)| *d == depth) {
                    mods.pop();
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    let mut path: Vec<String> = mods.into_iter().map(|(m, _)| m).collect();
    path.push(name);
    path.join("::")
}

/// 6.6.4: a reserved / contextual keyword can't be registered, nor can
/// a name that is not an identifier.
#[must_use]
pub fn check_keyword(def: &KeywordDef) -> Option<RokDiagnostic> {
    let err = |m: String| {
        Some(RokDiagnostic::error(
            Code::INVALID_KEYWORD_DECL,
            def.span,
            m,
        ))
    };
    let n = def.name.as_str();
    if RESERVED.contains(&n) || keyword_kind(n).is_some() || CONTEXTUAL.contains(&n) {
        return err(format!(
            "`#[keyword(\"{n}\")]`: `{n}` is already a keyword (spec 6.6.4)"
        ));
    }
    let ident = n
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && n.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !ident {
        return err(format!(
            "`#[keyword(\"{n}\")]`: a keyword must be an identifier (spec 6.6.2)"
        ));
    }
    if def.handler.is_empty() {
        return err(format!(
            "`#[keyword(\"{n}\")]` must be on a free function - its handler (spec 6.6.2)"
        ));
    }
    None
}

/// A package's keywords: name, the handler's (or type's) absolute
/// path, and whether it is a type keyword.
pub type KeywordTable = Vec<(String, Vec<String>, bool)>;

/// Rewrite the package's registered keywords (`name` → handler path).
pub fn apply_keywords(tokens: &mut Vec<Token>, kws: &KeywordTable, diags: &mut Vec<RokDiagnostic>) {
    if kws.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(tokens.len());
    for (k, t) in tokens.iter().enumerate() {
        let TokenKind::Ident(n) = &t.kind else {
            out.push(t.clone());
            continue;
        };
        let Some((_, path, ty)) = kws.iter().find(|(kw, ..)| kw == n) else {
            out.push(t.clone());
            continue;
        };
        let prev = k.checked_sub(1).map(|p| &tokens[p].kind);
        let next = tokens.get(k + 1).map(|t| &t.kind);
        // A keyword can't name anything (6.6.3).
        let names = matches!(
            prev,
            Some(
                TokenKind::Let
                    | TokenKind::Fn
                    | TokenKind::Struct
                    | TokenKind::Enum
                    | TokenKind::Trait
                    | TokenKind::Mod
                    | TokenKind::Dot
                    | TokenKind::ColonColon
                    | TokenKind::For
            )
        ) || matches!(next, Some(TokenKind::Colon))
            || (!ty && matches!(next, Some(TokenKind::ColonColon)));
        if names {
            diags.push(
                RokDiagnostic::error(
                    Code::RESERVED_NAME,
                    t.span,
                    format!(
                        "`{n}` is a keyword registered with `#[keyword(\"{n}\")]` and can't be used as a name (spec 6.6.3)"
                    ),
                ),
            );
            out.push(t.clone());
            continue;
        }
        for (i, seg) in path.iter().enumerate() {
            if i > 0 {
                out.push(Token::new(TokenKind::ColonColon, t.span));
            }
            out.push(Token::new(TokenKind::Ident(seg.clone()), t.span));
        }
    }
    *tokens = out;
}

rokc_core::codec_struct!(KeywordDef {
    name,
    handler,
    ty,
    file,
    span,
    cfg
});
