//! `#[literal(pattern)]` (refs-lang 6.5): source matching a registered
//! pattern is a call of its handler.
//!
//! Patterns (6.5.3): `"tok"` matches a token; `expr` / `number` /
//! `string` / `ident` capture; `{ .. }` / `( .. )` group; `% "sep"`
//! separates repetitions; `*` / `+` / `?` repeat; `, priority = N` (6.5.5).
//!
//! - Built-in shapes (`[..]`, `{k: v}`, `{a, b}`, ranges) keep the
//!   language's own parse (6.5.6, 3.7); HIR dispatches them by exact
//!   pattern (`shape`).
//! - `number "suffix"` (1.6.2): a numeric literal with an unknown suffix.
//! - Any other pattern starting with a quoted token is tried at every
//!   expression start before the grammar (`Parser::try_magic_literal`);
//!   longest match wins, ties go to HIR as candidates (6.5.5).
//!
//! Captures are handler arguments in order; a `?` group's are each a value
//! or `none`; a `*` / `+` repetition is one argument of items (a value, a
//! tuple, or an array when nested), or the variadic arguments of a `...T`.

use std::sync::Arc;

use rokc_ast::{Expr, ExprKind, LitArg, LitCand, Literal, Path, PathSegment};
use rokc_diagnostic::{Code, RokDiagnostic, Span};
use rokc_lexer::lexer::{FLOAT_SUFFIXES, INT_SUFFIXES};
use rokc_lexer::{Lexer, Token, TokenKind};

use super::Parser;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    Expr,
    Number,
    Str,
    Ident,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rep {
    One,
    Opt,
    Star,
    Plus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatElem {
    /// A quoted token and its text.
    Tok(TokenKind, String),
    Cap(Cap),
    Group {
        body: Vec<PatElem>,
        sep: Option<(TokenKind, String)>,
        rep: Rep,
    },
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pub elems: Vec<PatElem>,
    pub priority: i64,
}

/// Literal syntaxes the parser owns whose value a handler builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    List,
    Dict,
    Set,
    Range,
    RangeInclusive,
    RangeFrom,
    RangeTo,
    RangeToInclusive,
    RangeFull,
    Tuple,
}

/// The patterns (as `render` prints them) of the built-in shapes.
const SHAPES: &[(&str, Shape)] = &[
    (r#""[" {expr} % "," * "]""#, Shape::List),
    (r#""{" {expr ":" expr} % "," * "}""#, Shape::Dict),
    (r#""{" {expr ":" expr} % "," + "}""#, Shape::Dict),
    (r#""{" {expr} % "," + "}""#, Shape::Set),
    (r#""{" {expr} % "," * "}""#, Shape::Set),
    (r#""(" expr "," {expr} % "," * ")""#, Shape::Tuple),
    (r#"expr ".." expr"#, Shape::Range),
    (r#"expr "..=" expr"#, Shape::RangeInclusive),
    (r#"expr "..""#, Shape::RangeFrom),
    (r#"".." expr"#, Shape::RangeTo),
    (r#""..=" expr"#, Shape::RangeToInclusive),
    (r#""..""#, Shape::RangeFull),
];

/// How a valid pattern is dispatched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatKind {
    Shape(Shape),
    Suffix(String),
    Prefix,
}

/// Whether a handler argument is one capture or a repetition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    One,
    Many,
}

impl Pattern {
    /// The pattern in canonical text (`"[" {expr} % "," * "]"`).
    #[must_use]
    pub fn render(&self) -> String {
        render(&self.elems)
    }

    /// A pattern `render` printed (kept in the parsed attribute).
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        let (toks, _) = Lexer::new(text, String::new()).lex_all();
        let toks: Vec<Token> = toks
            .into_iter()
            .filter(|t| t.kind != TokenKind::Eof)
            .collect();
        parse_pattern(&toks, &[]).ok()
    }

    /// 6.5.7: what the pattern registers, or why it can't.
    pub fn kind(&self) -> Result<PatKind, String> {
        let text = self.render();
        if let Some((_, s)) = SHAPES.iter().find(|(p, _)| *p == text) {
            if *s == Shape::Tuple {
                return Err("the tuple literal `(a, b)` is built into the language and \
                            can't be registered (a handler can't take heterogeneous items)"
                    .to_owned());
            }
            return Ok(PatKind::Shape(*s));
        }
        if let [
            PatElem::Cap(Cap::Number),
            PatElem::Tok(TokenKind::Ident(s), _),
        ] = &self.elems[..]
        {
            if INT_SUFFIXES.contains(&s.as_str()) || FLOAT_SUFFIXES.contains(&s.as_str()) {
                return Err(format!(
                    "`{s}` is a built-in numeric suffix and can't be registered (spec 1.6.2)"
                ));
            }
            return Ok(PatKind::Suffix(s.clone()));
        }
        match self.elems.first() {
            Some(PatElem::Tok(..)) => Ok(PatKind::Prefix),
            _ => Err(format!(
                "pattern `{text}` must start with a quoted token (or be `number \"suffix\"`, \
                 or one of the built-in collection / range shapes)"
            )),
        }
    }

    /// The handler arguments the captures make, in order.
    #[must_use]
    pub fn args(&self) -> Vec<ArgKind> {
        let mut out = Vec::new();
        arg_kinds(&self.elems, &mut out);
        out
    }
}

fn arg_kinds(elems: &[PatElem], out: &mut Vec<ArgKind>) {
    for e in elems {
        match e {
            PatElem::Tok(..) => {}
            PatElem::Cap(_) => out.push(ArgKind::One),
            PatElem::Group {
                body,
                rep: Rep::One | Rep::Opt,
                ..
            } => arg_kinds(body, out),
            PatElem::Group { .. } => out.push(ArgKind::Many),
        }
    }
}

fn render(elems: &[PatElem]) -> String {
    let parts: Vec<String> = elems
        .iter()
        .map(|e| match e {
            PatElem::Tok(_, t) => format!("\"{t}\""),
            PatElem::Cap(c) => match c {
                Cap::Expr => "expr",
                Cap::Number => "number",
                Cap::Str => "string",
                Cap::Ident => "ident",
            }
            .to_owned(),
            PatElem::Group { body, sep, rep } => {
                let mut s = format!("{{{}}}", render(body));
                if let Some((_, t)) = sep {
                    s.push_str(&format!(" % \"{t}\""));
                }
                s.push_str(match rep {
                    Rep::One => "",
                    Rep::Opt => " ?",
                    Rep::Star => " *",
                    Rep::Plus => " +",
                });
                s
            }
        })
        .collect();
    parts.join(" ")
}

/// Parse the tokens between `#[literal(` and `)]`. Quoted tokens are
/// lexed with the build's extension operators (`ext`).
pub fn parse_pattern(tokens: &[Token], ext: &[String]) -> Result<Pattern, (Span, String)> {
    let mut p = PatParser {
        t: tokens,
        i: 0,
        ext,
    };
    let elems = p.seq(None)?;
    let mut priority = 0;
    if p.peek() == Some(&TokenKind::Comma) {
        p.i += 1;
        let ok = matches!(p.peek(), Some(TokenKind::Ident(n)) if n == "priority");
        p.i += 1;
        let ok = ok && p.peek() == Some(&TokenKind::Eq);
        p.i += 1;
        let neg = p.peek() == Some(&TokenKind::Minus);
        p.i += usize::from(neg);
        match p.peek() {
            Some(TokenKind::IntLit(n)) if ok => {
                let v: i64 = n
                    .parse()
                    .map_err(|_| (p.span(), "`priority` must be an integer".to_owned()))?;
                priority = if neg { -v } else { v };
                p.i += 1;
            }
            _ => {
                return Err((
                    p.span(),
                    "expected `priority = N` after the pattern (spec 6.5.5)".to_owned(),
                ));
            }
        }
    }
    if p.i < tokens.len() {
        return Err((p.span(), "unexpected token in a literal pattern".to_owned()));
    }
    if elems.is_empty() {
        return Err((p.span(), "empty literal pattern".to_owned()));
    }
    Ok(Pattern { elems, priority })
}

struct PatParser<'a> {
    t: &'a [Token],
    i: usize,
    ext: &'a [String],
}

impl PatParser<'_> {
    fn peek(&self) -> Option<&TokenKind> {
        self.t.get(self.i).map(|t| &t.kind)
    }

    fn span(&self) -> Span {
        self.t
            .get(self.i)
            .or_else(|| self.t.last())
            .map_or(Span::DUMMY, |t| t.span)
    }

    fn seq(&mut self, close: Option<&TokenKind>) -> Result<Vec<PatElem>, (Span, String)> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None if close.is_some() => {
                    return Err((self.span(), "unclosed group in literal pattern".to_owned()));
                }
                None | Some(TokenKind::Comma) if close.is_none() => return Ok(out),
                Some(k) if Some(k) == close => {
                    self.i += 1;
                    return Ok(out);
                }
                _ => {}
            }
            let atom = self.atom()?;
            out.push(self.postfix(atom)?);
        }
    }

    fn atom(&mut self) -> Result<PatElem, (Span, String)> {
        let span = self.span();
        let kind = self.peek().cloned();
        self.i += 1;
        match kind {
            Some(TokenKind::StrLit(s)) => {
                let (tok, text) = self.quoted(&s, span)?;
                Ok(PatElem::Tok(tok, text))
            }
            Some(TokenKind::Ident(w)) => match w.as_str() {
                "expr" => Ok(PatElem::Cap(Cap::Expr)),
                "number" => Ok(PatElem::Cap(Cap::Number)),
                "string" => Ok(PatElem::Cap(Cap::Str)),
                "ident" => Ok(PatElem::Cap(Cap::Ident)),
                _ => Err((
                    span,
                    format!(
                        "unknown literal pattern word `{w}` (expected `expr`, `number`, \
                         `string`, `ident` or a quoted token, spec 6.5.3)"
                    ),
                )),
            },
            Some(open @ (TokenKind::LBrace | TokenKind::LParen)) => {
                let close = if open == TokenKind::LBrace {
                    TokenKind::RBrace
                } else {
                    TokenKind::RParen
                };
                let body = self.seq(Some(&close))?;
                if body.is_empty() {
                    return Err((span, "empty group in literal pattern".to_owned()));
                }
                Ok(PatElem::Group {
                    body,
                    sep: None,
                    rep: Rep::One,
                })
            }
            _ => Err((span, "unexpected token in a literal pattern".to_owned())),
        }
    }

    /// `"tok"` → its one token.
    fn quoted(&self, s: &str, span: Span) -> Result<(TokenKind, String), (Span, String)> {
        let (toks, diags) = Lexer::new(s, String::new())
            .with_ext_ops(self.ext.to_vec())
            .lex_all();
        let toks: Vec<Token> = toks
            .into_iter()
            .filter(|t| t.kind != TokenKind::Eof)
            .collect();
        match &toks[..] {
            [t] if diags.is_empty() => Ok((t.kind.clone(), s.to_owned())),
            _ => Err((
                span,
                format!("`\"{s}\"` is not a single token - quote each token on its own"),
            )),
        }
    }

    fn postfix(&mut self, atom: PatElem) -> Result<PatElem, (Span, String)> {
        let span = self.span();
        let sep = if self.peek() == Some(&TokenKind::Percent) {
            self.i += 1;
            let s_span = self.span();
            let Some(TokenKind::StrLit(s)) = self.peek().cloned() else {
                return Err((
                    s_span,
                    "`%` must be followed by a quoted separator".to_owned(),
                ));
            };
            self.i += 1;
            Some(self.quoted(&s, s_span)?)
        } else {
            None
        };
        let rep = match self.peek() {
            Some(TokenKind::Star) => Rep::Star,
            Some(TokenKind::Plus) => Rep::Plus,
            Some(TokenKind::Question) if sep.is_none() => Rep::Opt,
            _ if sep.is_some() => {
                return Err((
                    span,
                    "a `% \"sep\"` separator must be followed by `*` or `+`".to_owned(),
                ));
            }
            _ => return Ok(atom),
        };
        self.i += 1;
        if matches!(
            self.peek(),
            Some(TokenKind::Star | TokenKind::Plus | TokenKind::Question)
        ) {
            return Err((
                self.span(),
                "repeated repetition in literal pattern".to_owned(),
            ));
        }
        let body = match atom {
            PatElem::Group {
                body,
                rep: Rep::One,
                ..
            } => body,
            other => vec![other],
        };
        Ok(PatElem::Group { body, sep, rep })
    }
}

/// A `#[literal(..)]` found by the pre-pass.
#[derive(Clone, Debug)]
pub struct LiteralDef {
    /// The tokens between the attribute's parentheses.
    pub tokens: Vec<Token>,
    /// The handler function's path in its file (`f`, `inner::f`).
    pub handler: String,
    pub file: String,
    /// The `#[literal(..)]` attribute.
    pub span: Span,
}

/// Every `#[literal(..)] .. fn handler` in `tokens` (a whole file).
#[must_use]
pub fn collect_literals(tokens: &[Token], file: &str) -> Vec<LiteralDef> {
    let mut out = Vec::new();
    for (k, w) in tokens.windows(3).enumerate() {
        let (TokenKind::HashLBracket, TokenKind::Ident(attr), TokenKind::LParen) =
            (&w[0].kind, &w[1].kind, &w[2].kind)
        else {
            continue;
        };
        if attr != "literal" {
            continue;
        }
        let Some(close) = balanced_close(tokens, k + 2) else {
            continue;
        };
        let handler = crate::keyword::handler_at(tokens, k, close);
        out.push(LiteralDef {
            tokens: tokens[k + 3..close].to_vec(),
            handler,
            file: file.to_owned(),
            span: w[0].span,
        });
    }
    out
}

/// The index of the `)` closing the `(` at `open`.
fn balanced_close(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate().skip(open) {
        match t.kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            TokenKind::Eof => return None,
            _ => {}
        }
    }
    None
}

/// A non-shape registration the parser matches.
#[derive(Clone, Debug)]
pub struct LitRule {
    pub pattern: Pattern,
    pub kind: PatKind,
    /// The handler's absolute path.
    pub handler: Vec<String>,
    pub span: Span,
}

/// The registrations a package's parse sees: the library's and its own.
#[derive(Clone, Debug, Default)]
pub struct LiteralTable {
    pub rules: Vec<LitRule>,
}

/// One captured piece while matching.
enum Got {
    Val(Expr),
    Absent,
    Rep(Vec<Vec<Got>>),
}

impl Parser<'_> {
    /// `#[literal(..)]`: the pattern is checked here (with spans) and
    /// kept as `Args[Lit(canonical text), priority = N]` for HIR.
    pub(super) fn parse_literal_attr_body(&mut self) -> rokc_ast::AttrBody {
        let start = self.pos;
        let Some(close) = balanced_close(self.tokens, start) else {
            let s = self.peek_span();
            self.error(Code::INVALID_LITERAL_HANDLER, s, "unclosed `#[literal(`");
            return rokc_ast::AttrBody::Empty;
        };
        self.pos = close + 1;
        let pat = parse_pattern(&self.tokens[start + 1..close], &self.ext.symbols()).and_then(
            |p| match p.kind() {
                Ok(_) => Ok(p),
                Err(m) => Err((self.tokens[start].span, m)),
            },
        );
        match pat {
            Ok(p) => rokc_ast::AttrBody::Args(vec![
                rokc_ast::AttrArg::Lit(Literal::Str(p.render())),
                rokc_ast::AttrArg::KeyValue(
                    "priority".to_owned(),
                    Literal::Int(p.priority.to_string()),
                ),
            ]),
            Err((span, m)) => {
                self.diagnostics.push(
                    RokDiagnostic::error(
                        Code::INVALID_LITERAL_HANDLER,
                        span,
                        format!("invalid `#[literal]`: {m}"),
                    )
                    .with_file(self.file.clone()),
                );
                rokc_ast::AttrBody::Empty
            }
        }
    }

    /// Spec 6.5.4: registered patterns, tried before the grammar. `None`
    /// consumes nothing.
    pub(super) fn try_magic_literal(&mut self) -> Option<Expr> {
        let span = self.peek_span();
        if let TokenKind::IntLit(t) | TokenKind::FloatLit(t) = self.peek().clone() {
            return self.suffix_literal(&t, span);
        }
        if self.literals.rules.is_empty() {
            return None;
        }
        let first = self.peek().clone();
        let table = Arc::clone(&self.literals);
        let start = self.save();
        let mut best: Option<(usize, Vec<LitCand>, Snapshot)> = None;
        for rule in &table.rules {
            if rule.kind != PatKind::Prefix
                || !matches!(rule.pattern.elems.first(), Some(PatElem::Tok(k, _)) if *k == first)
            {
                continue;
            }
            let mut got = Vec::new();
            let ok = self.match_seq(&rule.pattern.elems, &mut got)
                && self.diagnostics.len() == start.diags;
            if ok {
                let cand = LitCand {
                    handler: path_of(&rule.handler, span),
                    args: to_args(got, span),
                    priority: rule.pattern.priority,
                    reg: rule.span,
                };
                match &mut best {
                    Some((end, cands, _)) if *end == self.pos => cands.push(cand),
                    Some((end, ..)) if *end > self.pos => {}
                    _ => best = Some((self.pos, vec![cand], self.save())),
                }
            }
            self.restore(&start);
        }
        let (_, cands, end) = best?;
        self.restore(&end);
        Some(Expr {
            kind: ExprKind::MagicLiteral(cands),
            span,
        })
    }

    /// `2i`: a numeric literal whose suffix the language doesn't define
    /// is its `#[literal(number "i")]` handler's call (spec 1.6.2).
    fn suffix_literal(&mut self, text: &str, span: Span) -> Option<Expr> {
        let (body, suffix) = split_suffix(text);
        if suffix.is_empty() || INT_SUFFIXES.contains(&suffix) || FLOAT_SUFFIXES.contains(&suffix) {
            return None;
        }
        let float = matches!(self.peek(), TokenKind::FloatLit(_)) || body.contains(['.', 'e', 'E']);
        let arg = Expr {
            kind: ExprKind::Literal(if float {
                Literal::Float(body.to_owned())
            } else {
                Literal::Int(body.to_owned())
            }),
            span,
        };
        let cands: Vec<LitCand> = self
            .literals
            .rules
            .iter()
            .filter(|r| matches!(&r.kind, PatKind::Suffix(s) if s == suffix))
            .map(|r| LitCand {
                handler: path_of(&r.handler, span),
                args: vec![LitArg::One(arg.clone())],
                priority: r.pattern.priority,
                reg: r.span,
            })
            .collect();
        self.bump();
        if cands.is_empty() {
            self.error(
                Code::INVALID_LITERAL_HANDLER,
                span,
                format!(
                    "unknown numeric literal suffix `{suffix}` - no `#[literal(number \
                     \"{suffix}\")]` handler is registered (spec 1.6.2, 6.5)"
                ),
            );
            return Some(arg);
        }
        Some(Expr {
            kind: ExprKind::MagicLiteral(cands),
            span,
        })
    }

    fn match_seq(&mut self, elems: &[PatElem], out: &mut Vec<Got>) -> bool {
        elems.iter().all(|e| self.match_elem(e, out))
    }

    fn match_elem(&mut self, e: &PatElem, out: &mut Vec<Got>) -> bool {
        match e {
            PatElem::Tok(k, _) => {
                if self.peek() == k {
                    self.bump();
                    true
                } else {
                    false
                }
            }
            PatElem::Cap(c) => match self.capture(*c) {
                Some(x) => {
                    out.push(Got::Val(x));
                    true
                }
                None => false,
            },
            PatElem::Group {
                body,
                rep: Rep::One,
                ..
            } => self.match_seq(body, out),
            PatElem::Group {
                body,
                rep: Rep::Opt,
                ..
            } => {
                let s = self.save();
                let mut inner = Vec::new();
                if self.match_seq(body, &mut inner) && self.diagnostics.len() == s.diags {
                    out.extend(inner);
                } else {
                    self.restore(&s);
                    let mut n = Vec::new();
                    arg_kinds(body, &mut n);
                    out.extend(n.iter().map(|_| Got::Absent));
                }
                true
            }
            PatElem::Group { body, sep, rep } => {
                let mut items: Vec<Vec<Got>> = Vec::new();
                loop {
                    let before = self.save();
                    if let (Some((s, _)), false) = (sep, items.is_empty()) {
                        if self.peek() != s {
                            break;
                        }
                        self.bump();
                    }
                    // A trailing separator is allowed (`[1, 2,]`).
                    let after_sep = self.save();
                    let mut inner = Vec::new();
                    if !self.match_seq(body, &mut inner)
                        || self.diagnostics.len() != before.diags
                        || self.pos == before.pos
                    {
                        self.restore(&after_sep);
                        break;
                    }
                    items.push(inner);
                }
                if *rep == Rep::Plus && items.is_empty() {
                    return false;
                }
                out.push(Got::Rep(items));
                true
            }
        }
    }

    fn capture(&mut self, c: Cap) -> Option<Expr> {
        let span = self.peek_span();
        let lit = match (c, self.peek().clone()) {
            (Cap::Expr, _) => {
                let d = self.diagnostics.len();
                let e = self.with_struct_expr(true, Self::parse_expr)?;
                return (self.diagnostics.len() == d).then_some(e);
            }
            (Cap::Number, TokenKind::IntLit(t)) if user_suffix(&t).is_none() => Literal::Int(t),
            (Cap::Number, TokenKind::FloatLit(t)) if user_suffix(&t).is_none() => Literal::Float(t),
            (Cap::Str, TokenKind::StrLit(s)) => Literal::Str(s),
            (Cap::Str, TokenKind::RawStrLit(s)) => Literal::RawStr(s),
            (Cap::Ident, TokenKind::Ident(n)) => Literal::Str(n),
            _ => return None,
        };
        self.bump();
        Some(Expr {
            kind: ExprKind::Literal(lit),
            span,
        })
    }

    fn save(&self) -> Snapshot {
        Snapshot {
            pos: self.pos,
            pending: self.pending.clone(),
            diags: self.diagnostics.len(),
            depth: self.depth,
        }
    }

    fn restore(&mut self, s: &Snapshot) {
        self.pos = s.pos;
        self.pending.clone_from(&s.pending);
        self.diagnostics.truncate(s.diags);
        self.depth = s.depth;
    }
}

struct Snapshot {
    pos: usize,
    pending: Option<TokenKind>,
    diags: usize,
    depth: u32,
}

/// `2i` → (`2`, `i`); `0x1F` → (`0x1F`, empty).
fn split_suffix(text: &str) -> (&str, &str) {
    let hex = text.starts_with("0x") || text.starts_with("0X");
    let mut end = text.len();
    for (i, c) in text.char_indices() {
        let digit = c.is_ascii_digit()
            || c == '_'
            || c == '.'
            || (hex && c.is_ascii_hexdigit())
            || (i < 2 && matches!(c, 'x' | 'X' | 'b' | 'B' | 'o' | 'O'))
            || (!hex && matches!(c, 'e' | 'E'))
            || (!hex && matches!(c, '+' | '-') && text[..i].ends_with(['e', 'E']));
        if !digit {
            end = i;
            break;
        }
    }
    text.split_at(end)
}

/// The suffix of `text` if it's not a built-in one.
pub fn user_suffix(text: &str) -> Option<&str> {
    let (_, s) = split_suffix(text);
    (!s.is_empty() && !INT_SUFFIXES.contains(&s) && !FLOAT_SUFFIXES.contains(&s)).then_some(s)
}

/// A handler's call: written by the compiler, so seen from the handler's
/// own module, and the handler may have any visibility (spec 6.9.9).
fn path_of(segs: &[String], span: Span) -> Path {
    let module = segs.split_last().map(|(_, m)| match m.split_first() {
        Some((root, rest)) if root == "pkg" => rest.join("::"),
        _ => m.join("::"),
    });
    Path {
        segments: segs
            .iter()
            .map(|n| PathSegment {
                name: n.clone(),
                generics: Vec::new(),
            })
            .collect(),
        span,
        origin: module,
    }
}

fn to_args(got: Vec<Got>, span: Span) -> Vec<LitArg> {
    got.into_iter()
        .map(|g| match g {
            Got::Rep(items) => {
                LitArg::Many(items.into_iter().map(|i| item_expr(i, span)).collect())
            }
            g => LitArg::One(got_expr(g, span)),
        })
        .collect()
}

fn item_expr(item: Vec<Got>, span: Span) -> Expr {
    let mut es: Vec<Expr> = item.into_iter().map(|g| got_expr(g, span)).collect();
    if es.len() == 1 {
        return es.pop().expect("one item");
    }
    Expr {
        kind: ExprKind::Tuple(es),
        span,
    }
}

fn got_expr(g: Got, span: Span) -> Expr {
    match g {
        Got::Val(e) => e,
        Got::Absent => Expr {
            kind: ExprKind::Literal(Literal::None),
            span,
        },
        Got::Rep(items) => Expr {
            kind: ExprKind::List(items.into_iter().map(|i| item_expr(i, span)).collect()),
            span,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(src: &str) -> Result<Pattern, (Span, String)> {
        let (toks, _) = Lexer::new(src, String::new()).lex_all();
        let toks: Vec<Token> = toks
            .into_iter()
            .filter(|t| t.kind != TokenKind::Eof)
            .collect();
        parse_pattern(&toks, &[])
    }

    #[test]
    fn shapes_and_kinds() {
        let p = pat(r#""[" {expr} % "," * "]""#).unwrap();
        assert_eq!(p.kind(), Ok(PatKind::Shape(Shape::List)));
        assert_eq!(p.args(), vec![ArgKind::Many]);
        let p = pat(r#"expr "..=" expr"#).unwrap();
        assert_eq!(p.kind(), Ok(PatKind::Shape(Shape::RangeInclusive)));
        let p = pat(r#"number "i""#).unwrap();
        assert_eq!(p.kind(), Ok(PatKind::Suffix("i".to_owned())));
        assert!(pat(r#"number "i32""#).unwrap().kind().is_err());
        assert!(
            pat(r#""(" expr "," {expr} % "," * ")""#)
                .unwrap()
                .kind()
                .is_err()
        );
        let p = pat(r#""m" "[" {{expr} % "," +} % ";" + "]", priority = 2"#).unwrap();
        assert_eq!(p.kind(), Ok(PatKind::Prefix));
        assert_eq!(p.priority, 2);
        let p = pat(r#""s" "[" expr? ":" expr? (":" expr?)? "]""#).unwrap();
        assert_eq!(p.args(), vec![ArgKind::One; 3]);
    }

    #[test]
    fn invalid_patterns() {
        assert!(pat(r#"{expr} % ",""#).is_err());
        assert!(pat(r#""[" bogus "]""#).is_err());
        assert!(pat(r#""[" {expr"#).is_err());
        assert!(pat(r#""ab cd""#).is_err());
        assert!(pat(r#"expr "@""#).unwrap().kind().is_err());
        assert!(pat("").is_err());
    }

    #[test]
    fn suffix_split() {
        assert_eq!(split_suffix("2i"), ("2", "i"));
        assert_eq!(split_suffix("2.5i"), ("2.5", "i"));
        assert_eq!(split_suffix("1e3i"), ("1e3", "i"));
        assert_eq!(split_suffix("0xFFi"), ("0xFF", "i"));
        assert_eq!(split_suffix("42i32"), ("42", "i32"));
        assert_eq!(split_suffix("7"), ("7", ""));
    }
}

rokc_core::codec_struct!(LiteralDef {
    tokens,
    handler,
    file,
    span
});

rokc_core::codec_enum!(Shape {
    List,
    Dict,
    Set,
    Range,
    RangeInclusive,
    RangeFrom,
    RangeTo,
    RangeToInclusive,
    RangeFull,
    Tuple
});
