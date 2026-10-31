//! Operator precedence is the language's (spec 3.2.3, `fixed_prec`).
//! `#[infix]` / `#[prefix]` / `#[postfix]` only bind tokens to trait
//! methods (6.7); `PrecTable` records that for HIR.

use std::collections::HashMap;

use rokc_ast::{AttrArg, AttrBody, Attribute, Item, Literal, Program, TraitItem};
use rokc_diagnostic::{ErrorKind, RokDiagnostic};
use rokc_lexer::TokenKind;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Fixity {
    Infix,
    Prefix,
    Postfix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Assoc {
    Left,
    Right,
    /// Non-associative; chaining must be parenthesized.
    None,
}

/// Binding power of the prefix unary operators and `await` (spec 3.2.3
/// level 12, right-assoc).
pub const PREFIX_PREC: u32 = 12;
/// Binding power of `as` / `is` (spec 3.2.3 level 11, left-assoc).
pub const CAST_PREC: u32 = 11;
/// Binding power of `??` (level 1, right-assoc).
pub const COALESCE_PREC: u32 = 1;

/// A built-in operator's precedence (spec 3.2.3); dotted broadcast tokens
/// share their scalar's row. `None` outside the table.
#[must_use]
pub fn fixed_prec(symbol: &str, fixity: Fixity) -> Option<(u32, Assoc)> {
    let base = match symbol {
        "./" => "/",
        s if s.len() > 1 && s.starts_with('.') => &s[1..],
        s => s,
    };
    match fixity {
        Fixity::Prefix => {
            matches!(symbol, "-" | "!" | "~" | "√").then_some((PREFIX_PREC, Assoc::Right))
        }
        // Level 13: the postfix row (`?`, member access). `?` itself is
        // language syntax; it is here only as a `like` target (spec 6.7.1).
        Fixity::Postfix => (symbol == "?").then_some((13, Assoc::Left)),
        Fixity::Infix => Some(match base {
            "<<" | ">>" => (10, Assoc::Left),
            "∘" => (9, Assoc::Left),
            "*" | "/" | "%" | "%%" | "×" | "⋅" | "÷" | "∩" => (8, Assoc::Left),
            "+" | "-" | "∪" => (7, Assoc::Left),
            "&" | "^" => (6, Assoc::Left),
            "|" => (5, Assoc::Left),
            "==" | "!=" | "<" | ">" | "<=" | ">=" | "≡" | "≠" | "≤" | "≥" | "∈" | "∉" | "⊂"
            | "⊆" => (4, Assoc::None),
            "&&" => (3, Assoc::Left),
            "||" => (2, Assoc::Left),
            _ => return None,
        }),
    }
}

/// Compound assignment tokens (spec 3.4.3): statements, so no binding
/// level; their `#[infix]` binds the `*Assign` method.
#[must_use]
pub fn is_assign_token(symbol: &str) -> bool {
    matches!(
        symbol,
        "+=" | "-=" | "*=" | "/=" | "%=" | "%%=" | "&=" | "|=" | "^=" | "<<=" | ">>="
    )
}

/// The level recorded for a compound assignment token (never parsed).
const ASSIGN_LEVEL: (u32, Assoc) = (0, Assoc::Right);

#[derive(Clone, Debug)]
pub struct OpEntry {
    pub symbol: String,
    pub fixity: Fixity,
    /// Trait method the operator dispatches to (`add` for `+`).
    pub method_name: String,
    /// Trait declaring that method, by its full path (`rok::core::ops::Add`,
    /// `m::Dot`); only its impls make a type support the operator (spec 6.4).
    pub trait_name: String,
    /// `#[infix("∈", swap)]`: `a ∈ s` calls `s.contains(a)` (spec 6.7.2).
    pub swap: bool,
    /// Binding level: the table's, or `like`'s for an extension token.
    pub level: (u32, Assoc),
    /// The package whose trait registers it (`""`: the one being built).
    pub package: String,
}

/// Registered extension operator tokens: what the lexer must recognize
/// and the level the parser gives them.
#[derive(Clone, Debug, Default)]
pub struct ExtOps {
    pub levels: HashMap<(String, Fixity), (u32, Assoc)>,
    /// Tokens that start like a fixed one (`<-`, `**`), by the package
    /// registering them: they lex only in that package's files, so
    /// `a<-1` elsewhere stays `a < -1`.
    pub owners: std::collections::BTreeMap<String, String>,
}

impl ExtOps {
    /// Distinct symbols, longest first (the lexer's longest match).
    #[must_use]
    pub fn symbols(&self) -> Vec<String> {
        let mut v: Vec<String> = self.levels.keys().map(|k| k.0.clone()).collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.len()));
        v.dedup();
        v
    }

    /// What files of `package` lex: without another package's tokens
    /// that start like a fixed one.
    #[must_use]
    pub fn in_package(&self, package: &str) -> std::borrow::Cow<'_, Self> {
        if self.owners.values().all(|p| p == package) {
            return std::borrow::Cow::Borrowed(self);
        }
        let foreign = |s: &String| self.owners.get(s).is_some_and(|p| p != package);
        std::borrow::Cow::Owned(Self {
            levels: self
                .levels
                .iter()
                .filter(|((s, _), _)| !foreign(s))
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            owners: self
                .owners
                .iter()
                .filter(|(_, p)| *p == package)
                .map(|(s, p)| (s.clone(), p.clone()))
                .collect(),
        })
    }

    /// Level of `symbol` at `fixity`: the fixed table, else a registration.
    #[must_use]
    pub fn level(&self, symbol: &str, fixity: Fixity) -> Option<(u32, Assoc)> {
        fixed_prec(symbol, fixity)
            .or_else(|| self.levels.get(&(symbol.to_owned(), fixity)).copied())
    }
}

/// Operator bindings: `(symbol, fixity) -> trait method`.
#[derive(Clone, Debug, Default)]
pub struct PrecTable {
    by_key: HashMap<(String, Fixity), OpEntry>,
}

impl PrecTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, entry: OpEntry) {
        self.by_key
            .insert((entry.symbol.clone(), entry.fixity), entry);
    }

    #[must_use]
    pub fn lookup(&self, symbol: &str, fixity: Fixity) -> Option<&OpEntry> {
        self.by_key.get(&(symbol.to_owned(), fixity))
    }

    pub fn entries(&self) -> impl Iterator<Item = &OpEntry> {
        self.by_key.values()
    }

    /// Extension tokens (spec 1.10.4): registered symbols outside the
    /// fixed table, with the level they took from `like`.
    #[must_use]
    pub fn ext_ops(&self) -> ExtOps {
        let ext = || {
            self.by_key.values().filter(|e| {
                fixed_prec(&e.symbol, e.fixity).is_none() && !is_assign_token(&e.symbol)
            })
        };
        ExtOps {
            levels: ext()
                .map(|e| ((e.symbol.clone(), e.fixity), e.level))
                .collect(),
            owners: ext()
                .filter(|e| {
                    e.symbol
                        .bytes()
                        .next()
                        .is_some_and(rokc_lexer::lexer::fixed_lead)
                })
                .map(|e| (e.symbol.clone(), e.package.clone()))
                .collect(),
        }
    }

    /// Insert `entry`; an error if the token is already bound to a
    /// different trait method.
    pub fn merge(&mut self, entry: OpEntry) -> Option<RokDiagnostic> {
        if let Some(e) = self.lookup(&entry.symbol, entry.fixity) {
            if e.trait_name != entry.trait_name || e.method_name != entry.method_name {
                return Some(RokDiagnostic::error(
                    ErrorKind::Meta,
                    rokc_diagnostic::Span::DUMMY,
                    format!(
                        "operator `{}` is bound twice (`{}::{}` and `{}::{}`)",
                        entry.symbol,
                        e.trait_name,
                        e.method_name,
                        entry.trait_name,
                        entry.method_name
                    ),
                ));
            }
        }
        self.insert(entry);
        None
    }
}

/// Collect `#[infix]` / `#[prefix]` / `#[postfix]` on trait methods.
///
/// Errors: precedence arguments (6.7.2), `like` on a table token, a
/// non-table token without `like`, aliases on different levels (6.7.12),
/// one token bound to two methods.
pub fn collect_prec_table(program: &Program, module: &[String]) -> (PrecTable, Vec<RokDiagnostic>) {
    let mut table = PrecTable::new();
    let mut diags = Vec::new();
    collect_items(&program.items, module, &mut table, &mut diags);
    (table, diags)
}

fn collect_items(
    items: &[Item],
    module: &[String],
    table: &mut PrecTable,
    diags: &mut Vec<RokDiagnostic>,
) {
    for item in items {
        if let Item::Mod(m) = item {
            if let Some(inner) = &m.items {
                let mut sub = module.to_vec();
                sub.push(m.name.clone());
                collect_items(inner, &sub, table, diags);
            }
            continue;
        }
        let stray = match item {
            Item::Fn(f) => f.attrs.iter().collect(),
            Item::Impl(i) => i
                .items
                .iter()
                .filter_map(|it| match it {
                    rokc_ast::ImplItem::Method(m) => Some(&m.attrs),
                    _ => None,
                })
                .flatten()
                .collect(),
            _ => Vec::new(),
        };
        for attr in stray {
            if let [p] = attr.path.as_slice() {
                if matches!(p.as_str(), "infix" | "prefix" | "postfix") {
                    diags.push(meta_err(
                        attr,
                        format!(
                            "`#[{p}]` can be attached only to a trait method signature (spec \
                             6.7.13)"
                        ),
                    ));
                }
            }
        }
        let Item::Trait(t) = item else { continue };
        let trait_name = module
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(t.name.as_str()))
            .collect::<Vec<_>>()
            .join("::");
        for ti in &t.items {
            let TraitItem::Method(m) = ti else { continue };
            let mut level: Option<(u32, Assoc)> = None;
            for attr in &m.attrs {
                let Some((fixity, symbol, lv, swap)) = collect_attr(attr, diags) else {
                    continue;
                };
                if level.is_some_and(|l| l != lv) {
                    diags.push(meta_err(
                        attr,
                        format!(
                            "operator aliases of `{}` must share one precedence level (spec 6.7.12)",
                            m.name
                        ),
                    ));
                    continue;
                }
                level = Some(lv);
                let entry = OpEntry {
                    symbol,
                    fixity,
                    swap,
                    level: lv,
                    method_name: m.name.clone(),
                    trait_name: trait_name.clone(),
                    package: String::new(),
                };
                if let Some(e) = table.merge(entry) {
                    diags.push(e.with_span(attr.span, ""));
                }
            }
        }
    }
}

/// Spec 1.10.4: a registered token is `Pattern_Syntax` code points only
/// (no identifier character, no space) and no fixed token.
fn bad_new_token(symbol: &str) -> Option<&'static str> {
    if symbol.is_empty() {
        return Some("it is empty");
    }
    if symbol
        .chars()
        .any(|c| !is_pattern_syntax(c) || rokc_lexer::unicode::is_ident_continue(c))
    {
        return Some("every character must be a Unicode `Pattern_Syntax` code point");
    }
    let (tokens, diags) = rokc_lexer::Lexer::new(symbol, "").lex_all();
    let fixed = diags.is_empty()
        && matches!(tokens.as_slice(), [t, end] if end.kind == TokenKind::Eof
            && !matches!(t.kind, TokenKind::ExtOp(_)));
    fixed.then_some("it is a fixed token of the language")
}

/// Unicode `Pattern_Syntax` (a stable property).
const fn is_pattern_syntax(c: char) -> bool {
    matches!(c,
        '!'..='/' | ':'..='@' | '['..='^' | '`' | '{'..='~'
        | '\u{A1}'..='\u{A7}' | '\u{A9}' | '\u{AB}' | '\u{AC}' | '\u{AE}' | '\u{B0}' | '\u{B1}'
        | '\u{B6}' | '\u{BB}' | '\u{BF}' | '\u{D7}' | '\u{F7}'
        | '\u{2010}'..='\u{2027}' | '\u{2030}'..='\u{203E}' | '\u{2041}'..='\u{2053}'
        | '\u{2055}'..='\u{205E}' | '\u{2190}'..='\u{245F}' | '\u{2500}'..='\u{2775}'
        | '\u{2794}'..='\u{2BFF}' | '\u{2E00}'..='\u{2E7F}' | '\u{3001}'..='\u{3003}'
        | '\u{3008}'..='\u{3020}' | '\u{3030}' | '\u{FD3E}' | '\u{FD3F}' | '\u{FE45}' | '\u{FE46}')
}

fn meta_err(attr: &Attribute, msg: String) -> RokDiagnostic {
    RokDiagnostic::error(ErrorKind::Meta, attr.span, msg)
}

fn collect_attr(
    attr: &Attribute,
    diags: &mut Vec<RokDiagnostic>,
) -> Option<(Fixity, String, (u32, Assoc), bool)> {
    let fixity = match attr.name() {
        "infix" => Fixity::Infix,
        "prefix" => Fixity::Prefix,
        "postfix" => Fixity::Postfix,
        _ => return None,
    };
    let name = attr.name();
    let AttrBody::Args(args) = &attr.body else {
        diags.push(meta_err(
            attr,
            format!("`#[{name}]` needs an operator token"),
        ));
        return None;
    };
    let mut symbol: Option<String> = None;
    let mut like: Option<String> = None;
    let mut swap = false;
    for a in args {
        match a {
            AttrArg::Lit(Literal::Str(s)) if symbol.is_none() => symbol = Some(s.clone()),
            AttrArg::Word(w) if w == "swap" && fixity == Fixity::Infix && !swap => swap = true,
            AttrArg::KeyValue(k, Literal::Str(v)) if k == "like" && like.is_none() => {
                like = Some(v.clone());
            }
            _ => {
                diags.push(meta_err(
                    attr,
                    format!(
                        "`#[{name}]` takes only an operator token, `like = \"..\"` and `swap`: \
                         precedence and associativity are fixed by the language (spec 3.2.3)"
                    ),
                ));
                return None;
            }
        }
    }
    let Some(symbol) = symbol else {
        diags.push(meta_err(
            attr,
            format!("`#[{name}]` needs an operator token"),
        ));
        return None;
    };
    if symbol == "?" {
        diags.push(meta_err(
            attr,
            "`?` is language syntax (error propagation), not an operator token".to_owned(),
        ));
        return None;
    }
    if is_assign_token(&symbol) {
        if fixity != Fixity::Infix || like.is_some() || swap {
            diags.push(meta_err(
                attr,
                format!("`{symbol}` is a compound assignment: only `#[infix(\"{symbol}\")]`"),
            ));
            return None;
        }
        return Some((fixity, symbol, ASSIGN_LEVEL, false));
    }
    let level = match (fixed_prec(&symbol, fixity), like) {
        (Some(lv), None) => lv,
        (Some(_), Some(_)) => {
            diags.push(meta_err(
                attr,
                format!(
                    "`{symbol}` has a fixed precedence (spec 3.2.3); `like` is only for new tokens"
                ),
            ));
            return None;
        }
        (None, Some(l)) => {
            if let Some(why) = bad_new_token(&symbol) {
                diags.push(meta_err(
                    attr,
                    format!("`{symbol}` can't be a new operator token: {why} (spec 1.10.4)"),
                ));
                return None;
            }
            let Some(lv) = fixed_prec(&l, fixity) else {
                diags.push(meta_err(
                    attr,
                    format!("`like = \"{l}\"` must name a built-in {name} operator"),
                ));
                return None;
            };
            lv
        }
        (None, None) => {
            diags.push(meta_err(
                attr,
                format!(
                    "`{symbol}` is not a built-in operator token; for `#[{name}]`, a new \
                     token needs `like = \"<existing token>\"`"
                ),
            ));
            return None;
        }
    };
    Some((fixity, symbol, level, swap))
}
