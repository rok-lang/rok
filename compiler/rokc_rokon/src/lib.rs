//! ROKON (Rok Object Notation): data files written with
//! rok's literal syntax, extension `.rokon` (`rok.rokon`, `rok.lock`).
//!
//! A file is one value, lexed and parsed by the rok lexer / parser
//! (`parse_expr_file`) and then checked to be data only:
//!
//! ```rokon
//! Config {
//!     name: "geom",            // struct, name optional: `{ name: .. }`
//!     tags: ["a", "b"],        // list
//!     limits: { "cpu": 2 },    // dict (keys are values)
//!     at: (1, -2.5),           // tuple
//!     mode: Mode::Fast,        // enum variant, by full path
//!     dep: Dep::git("u", tag: "v1"),
//!     proxy: none,
//! }
//! ```
//!
//! - [`parse`]: text to a [`Value`] tree with spans.
//! - [`Doc`] / [`Decoder`]: typed reading for Rust consumers, with
//!   `file:line:col` errors and a caret.
//! - [`to_string`]: the canonical text (4-space indent, one line when it
//!   fits in 100 columns, else one entry per line with trailing commas).
//! - [`edit`]: layout-preserving edits (comments stay).

mod decode;
pub mod edit;
mod write;

#[cfg(test)]
mod tests;

pub use decode::{Decoder, Doc, Record, Variant};
pub use rokc_diagnostic::Span;
pub use write::{quote, to_line, to_string};

use rokc_ast::escape::{EscapeMode, unescape};
use rokc_ast::{CallArg, Expr, ExprKind, Literal};
use rokc_diagnostic::Code;
use rokc_lexer::{Lexer, Token, TokenKind};

#[derive(Clone, Debug)]
pub struct Value {
    pub kind: Kind,
    /// Where it was written (`Span::DUMMY` for a built value).
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Kind {
    None,
    Bool(bool),
    Int(i128),
    Float(f64),
    Char(char),
    Str(String),
    List(Vec<Value>),
    Tuple(Vec<Value>),
    /// `{ key: value }` with value keys (also `{}`).
    Dict(Vec<(Value, Value)>),
    /// `Name { field: v }`, or `{ field: v }` (`name: None`).
    Struct {
        name: Option<String>,
        fields: Vec<Field>,
    },
    /// `E::V`, `E::V(a, k: b)` (`braces: false`) or `E::V { f: v }`
    /// (`braces: true`, the fields in `named`). `path` is as written:
    /// `"E::V"`.
    Variant {
        path: String,
        args: Vec<Value>,
        named: Vec<Field>,
        braces: bool,
    },
}

/// A struct field or named argument.
#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    /// The name's span.
    pub span: Span,
    pub value: Value,
}

impl Value {
    pub const fn new(kind: Kind) -> Self {
        Self {
            kind,
            span: Span::DUMMY,
        }
    }

    pub fn str(s: impl Into<String>) -> Self {
        Self::new(Kind::Str(s.into()))
    }

    /// `Name { fields }` (`name: None`: `{ fields }`).
    pub fn record(name: Option<&str>, fields: Vec<(&str, Self)>) -> Self {
        Self::new(Kind::Struct {
            name: name.map(str::to_owned),
            fields: fields.into_iter().map(|(n, v)| Field::new(n, v)).collect(),
        })
    }

    /// `path(args, named..)`, or `path` alone when both are empty.
    pub fn call(path: &str, args: Vec<Self>, named: Vec<(&str, Self)>) -> Self {
        Self::new(Kind::Variant {
            path: path.to_owned(),
            args,
            named: named.into_iter().map(|(n, v)| Field::new(n, v)).collect(),
            braces: false,
        })
    }

    /// What it is, for "found .." in errors.
    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::None => "`none`".into(),
            Kind::Bool(_) => "a bool".into(),
            Kind::Int(_) => "an integer".into(),
            Kind::Float(_) => "a float".into(),
            Kind::Char(_) => "a char".into(),
            Kind::Str(_) => "a string".into(),
            Kind::List(_) => "a list".into(),
            Kind::Tuple(_) => "a tuple".into(),
            Kind::Dict(_) => "a dict".into(),
            Kind::Struct { name: Some(n), .. } => format!("a struct `{n} {{ .. }}`"),
            Kind::Struct { name: None, .. } => "a struct `{ .. }`".into(),
            Kind::Variant { path, .. } => format!("`{path}`"),
        }
    }
}

impl Field {
    pub fn new(name: &str, value: Value) -> Self {
        Self {
            name: name.to_owned(),
            span: Span::DUMMY,
            value,
        }
    }
}

/// Values are equal when their text is (spans ignored).
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        to_string(self) == to_string(other)
    }
}

/// A ROKON syntax error: a message at a span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub span: Span,
    pub message: String,
    /// What to write instead, as a `= help:` line.
    pub help: Option<String>,
}

impl Error {
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            help: None,
        }
    }

    /// The syntax error as `rokc` prints diagnostics.
    pub fn render(&self, text: &str, file: &str) -> String {
        let r = render(text, file, self.span, Code::SYNTAX, &self.message);
        match &self.help {
            Some(h) => with_help(&r, h),
            None => r,
        }
    }
}

/// 1-based `(line, column)` of byte `offset` (columns count chars).
pub fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(src.len());
    let before = &src[..offset];
    let line = before.matches('\n').count() + 1;
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[start..].chars().count() + 1)
}

/// An error at `span` of `src` in rokc's diagnostic layout:
/// `error[code]: message`, `--> file:line:col`, then the line with
/// `^^^` under the span (its first line).
pub fn render(src: &str, file: &str, span: Span, code: Code, message: &str) -> String {
    let (start, end) = (span.start as usize, span.end as usize);
    let (line, col) = line_col(src, start);
    let text = src.lines().nth(line - 1).unwrap_or("");
    let width = src
        .get(start..end.max(start))
        .map_or(1, |s| s.lines().next().unwrap_or("").chars().count().max(1));
    let pad = " ".repeat(line.to_string().len() + 1);
    format!(
        "error[{code}]: {message}\n{pad}--> {file}:{line}:{col}\n{pad} |\n {line} | {text}\n{pad} | {}{}",
        " ".repeat(col - 1),
        "^".repeat(width)
    )
}

/// A rendered error (`render`) with a `= help:` line under its caret.
#[must_use]
pub fn with_help(rendered: &str, help: &str) -> String {
    let pad = rendered
        .lines()
        .nth(1)
        .map_or(0, |l| l.len() - l.trim_start().len());
    let pad = " ".repeat(pad);
    format!("{rendered}\n{pad} |\n{pad} = help: {help}")
}

/// Lex `text` (rok tokens, comments dropped); the first lexical error.
pub(crate) fn lex(text: &str) -> Result<Vec<Token>, Error> {
    let (tokens, diags) = Lexer::new(text, "<rokon>").lex_all();
    match diags.iter().find(|d| d.is_error()) {
        Some(d) => Err(Error::new(
            d.primary.as_ref().map(|l| l.span).unwrap_or_default(),
            d.message.clone(),
        )),
        None => Ok(tokens),
    }
}

/// Parse a ROKON file: exactly one value.
pub fn parse(text: &str) -> Result<Value, Error> {
    let tokens = lex(text)?;
    parse_tokens(&tokens)
}

pub(crate) fn parse_tokens(tokens: &[Token]) -> Result<Value, Error> {
    let eof = tokens.last().map(|t| t.span).unwrap_or_default();
    if tokens.iter().all(|t| matches!(t.kind, TokenKind::Eof)) {
        return Err(Error::new(
            eof,
            "expected a value, found the end of the file",
        ));
    }
    let (expr, diags) = rokc_parser::parse_expr_file(tokens, "<rokon>");
    if let Some(d) = diags.iter().find(|d| d.is_error()) {
        let span = d.primary.as_ref().map(|l| l.span).unwrap_or_default();
        let message = if d.message.starts_with("expected the end of the file") {
            "expected the end of the file: a ROKON file holds one value".to_owned()
        } else {
            d.message.clone()
        };
        let mut e = Error::new(span, message);
        e.help = d
            .notes
            .iter()
            .find_map(|n| n.strip_prefix("help: ").map(str::to_owned));
        return Err(e);
    }
    let expr = expr.ok_or_else(|| Error::new(eof, "expected a value"))?;
    Conv { tokens }.value(&expr)
}

struct Conv<'t> {
    tokens: &'t [Token],
}

const NOT_DATA: &str = "is not allowed in ROKON (only literals, lists, dicts, tuples, structs and \
                        enum variants)";

impl Conv<'_> {
    /// The token index starting at byte `at`.
    fn token(&self, at: u32) -> Option<usize> {
        self.tokens.binary_search_by_key(&at, |t| t.span.start).ok()
    }

    /// The first token of `e` (for carets).
    fn head(&self, e: &Expr) -> Span {
        self.token(e.span.start)
            .map_or(e.span, |i| self.tokens[i].span)
    }

    fn err(&self, e: &Expr, msg: impl Into<String>) -> Error {
        Error::new(self.head(e), msg)
    }

    fn values(&self, es: &[Expr]) -> Result<Vec<Value>, Error> {
        es.iter().map(|e| self.value(e)).collect()
    }

    /// A `name: value` field from the AST (which keeps no name span):
    /// the name is the token before the `:` before the value.
    fn field(&self, name: &str, e: &Expr) -> Result<Field, Error> {
        let at = self.token(e.span.start);
        let colon = at.and_then(|i| i.checked_sub(1));
        let has_colon = colon.is_some_and(|i| self.tokens[i].kind == TokenKind::Colon);
        if !has_colon {
            // Shorthand `Name { x }`: the value is a made-up path.
            let span = colon.map_or(e.span, |i| self.tokens[i].span);
            return Err(Error::new(
                span,
                format!("field `{name}` needs a value: `{name}: ..`"),
            ));
        }
        let span = colon
            .and_then(|i| i.checked_sub(1))
            .map_or(e.span, |i| self.tokens[i].span);
        Ok(Field {
            name: name.to_owned(),
            span,
            value: self.value(e)?,
        })
    }

    fn path(&self, e: &Expr, p: &rokc_ast::Path) -> Result<String, Error> {
        if p.segments.iter().any(|s| !s.generics.is_empty()) {
            return Err(self.err(e, "generic arguments are not allowed in ROKON"));
        }
        Ok(p.segments
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::"))
    }

    fn value(&self, e: &Expr) -> Result<Value, Error> {
        let kind = match &e.kind {
            ExprKind::Literal(l) => self.literal(e, l, false)?,
            ExprKind::OpPrefix { op, operand } if op == "-" => match &operand.kind {
                ExprKind::Literal(l @ (Literal::Int(_) | Literal::Float(_))) => {
                    self.literal(operand, l, true)?
                }
                _ => return Err(self.err(e, "`-` goes only before a number in ROKON")),
            },
            ExprKind::List(es) => Kind::List(self.values(es)?),
            ExprKind::Tuple(es) => Kind::Tuple(self.values(es)?),
            ExprKind::Block(b) if b.stmts.is_empty() && b.trailing.is_none() => {
                Kind::Dict(Vec::new())
            }
            ExprKind::Dict(pairs) => {
                let ident = |k: &Expr| match &k.kind {
                    ExprKind::Path(p) if p.segments.len() == 1 => Some(p.segments[0].name.clone()),
                    _ => None,
                };
                if ident(&pairs[0].0).is_some() {
                    // `{ field: v }`: a struct without a name.
                    let mut fields = Vec::new();
                    for (k, v) in pairs {
                        let Some(name) = ident(k) else {
                            return Err(self.err(
                                k,
                                "a `{ .. }` holds either fields (`name: v`) or keys (`\"k\": v`), \
                                 not both",
                            ));
                        };
                        fields.push(self.field(&name, v)?);
                    }
                    Kind::Struct { name: None, fields }
                } else {
                    let mut out = Vec::new();
                    for (k, v) in pairs {
                        if ident(k).is_some() {
                            return Err(self.err(
                                k,
                                "a `{ .. }` holds either fields (`name: v`) or keys (`\"k\": v`), \
                                 not both",
                            ));
                        }
                        out.push((self.value(k)?, self.value(v)?));
                    }
                    Kind::Dict(out)
                }
            }
            ExprKind::StructInit {
                path,
                fields,
                spread,
            } => {
                if let Some(s) = spread {
                    let at = self
                        .token(s.span.start)
                        .and_then(|i| i.checked_sub(1))
                        .map_or(s.span, |i| self.tokens[i].span);
                    return Err(Error::new(at, "`..` is not allowed in ROKON"));
                }
                let name = self.path(e, path)?;
                let fields = fields
                    .iter()
                    .map(|(n, v)| self.field(n, v))
                    .collect::<Result<Vec<_>, _>>()?;
                if path.segments.len() == 1 {
                    Kind::Struct {
                        name: Some(name),
                        fields,
                    }
                } else {
                    Kind::Variant {
                        path: name,
                        args: Vec::new(),
                        named: fields,
                        braces: true,
                    }
                }
            }
            ExprKind::Path(p) if p.segments.len() >= 2 => Kind::Variant {
                path: self.path(e, p)?,
                args: Vec::new(),
                named: Vec::new(),
                braces: false,
            },
            ExprKind::Path(p) => {
                let n = &p.segments[0].name;
                return Err(self.err(
                    e,
                    format!(
                        "bare identifier `{n}` is not a ROKON value (a string is quoted: \
                         `\"{n}\"`; an enum variant is written with its type: `Type::{n}`)"
                    ),
                ));
            }
            ExprKind::Call {
                callee,
                args,
                trailing_closure,
            } => {
                let path = match &callee.kind {
                    ExprKind::Path(p) if p.segments.len() >= 2 => self.path(callee, p)?,
                    _ => {
                        return Err(self.err(
                            e,
                            "calls are not allowed in ROKON (only enum variants: `Type::V(..)`)",
                        ));
                    }
                };
                if let Some(c) = trailing_closure {
                    return Err(self.err(c, format!("a closure {NOT_DATA}")));
                }
                let (mut pos, mut named) = (Vec::new(), Vec::new());
                for a in args {
                    match a {
                        CallArg::Positional(x) => pos.push(self.value(x)?),
                        CallArg::Named { name, value } => named.push(self.field(name, value)?),
                        CallArg::Spread(x) | CallArg::Ref(x) => {
                            return Err(self.err(x, format!("this argument {NOT_DATA}")));
                        }
                    }
                }
                Kind::Variant {
                    path,
                    args: pos,
                    named,
                    braces: false,
                }
            }
            ExprKind::MethodCall { .. } => {
                return Err(self.err(e, "method calls are not allowed in ROKON"));
            }
            ExprKind::OpInfix { op, lhs, .. } => {
                // The operator: the first token after `lhs`.
                let at = self
                    .tokens
                    .iter()
                    .find(|t| t.span.start >= lhs.span.end)
                    .map_or(e.span, |t| t.span);
                return Err(Error::new(
                    at,
                    format!("operator `{op}` is not allowed in ROKON"),
                ));
            }
            ExprKind::OpPrefix { op, .. } | ExprKind::OpPostfix { op, .. } => {
                return Err(self.err(e, format!("operator `{op}` is not allowed in ROKON")));
            }
            ExprKind::FString(_) => {
                return Err(self.err(e, "f-strings are not allowed in ROKON"));
            }
            ExprKind::Range { .. } => {
                return Err(self.err(e, "ranges (`..`) are not allowed in ROKON"));
            }
            ExprKind::Block(_) => {
                return Err(self.err(e, "blocks are not allowed in ROKON"));
            }
            ExprKind::Set(_) => {
                return Err(self.err(e, "sets are not ROKON values (write a list: `[a, b]`)"));
            }
            _ => return Err(self.err(e, format!("this expression {NOT_DATA}"))),
        };
        Ok(Value { kind, span: e.span })
    }

    fn literal(&self, e: &Expr, l: &Literal, neg: bool) -> Result<Kind, Error> {
        let sign = if neg { "-" } else { "" };
        Ok(match l {
            Literal::None => Kind::None,
            Literal::Bool(b) => Kind::Bool(*b),
            Literal::Int(t) => Kind::Int(int(t, neg).map_err(|m| self.err(e, m))?),
            Literal::Float(t) => {
                let digits = t.replace('_', "");
                if digits.ends_with("f32") || digits.ends_with("f64") {
                    return Err(self.err(e, "type suffixes are not allowed in ROKON"));
                }
                let f: f64 = format!("{sign}{digits}")
                    .parse()
                    .map_err(|_| self.err(e, format!("invalid float `{t}`")))?;
                // Infinity has no ROKON form.
                if !f.is_finite() {
                    return Err(self.err(
                        e,
                        format!(
                            "float `{t}` is out of range (ROKON floats are f64, up to about \
                             1.8e308)"
                        ),
                    ));
                }
                Kind::Float(f)
            }
            Literal::Str(s) => {
                Kind::Str(unescape(s, EscapeMode::Str).map_err(|(_, m)| self.err(e, m))?)
            }
            Literal::RawStr(s) => Kind::Str(s.clone()),
            Literal::Char(s) => {
                let c = unescape(s, EscapeMode::Char).map_err(|(_, m)| self.err(e, m))?;
                let mut it = c.chars();
                match (it.next(), it.next()) {
                    (Some(c), None) => Kind::Char(c),
                    _ => return Err(self.err(e, "a char literal holds one character")),
                }
            }
            Literal::CChar(_) => {
                return Err(self.err(e, "`c'..'` literals are not allowed in ROKON (write `'a'`)"));
            }
            Literal::ByteStr(_) => {
                return Err(self.err(e, "`b\"..\"` literals are not allowed in ROKON"));
            }
        })
    }
}

/// An integer literal's text (`0x`/`0o`/`0b`, `_`), negated by `neg`.
fn int(text: &str, neg: bool) -> Result<i128, String> {
    let t = text.replace('_', "");
    let (radix, digits) = match t.get(..2) {
        Some("0x" | "0X") => (16, &t[2..]),
        Some("0o" | "0O") => (8, &t[2..]),
        Some("0b" | "0B") => (2, &t[2..]),
        _ => (10, t.as_str()),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(if digits.chars().any(|c| matches!(c, 'i' | 'u')) {
            "type suffixes are not allowed in ROKON".to_owned()
        } else {
            format!("invalid integer `{text}`")
        });
    }
    let v = u128::from_str_radix(digits, radix)
        .map_err(|_| format!("integer `{text}` is too large"))?;
    let v = i128::try_from(v).map_err(|_| format!("integer `{text}` is too large"))?;
    Ok(if neg { -v } else { v })
}
