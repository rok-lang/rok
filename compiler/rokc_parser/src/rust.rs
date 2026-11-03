//! Rust habits: forms a Rust programmer types first, each reported with
//! the Rok form to write instead. Only token shapes that are never valid
//! Rok match.

use rokc_ast::{Expr, ExprKind, Param, Path, Pattern, Stmt, StmtKind, Ty, TyKind};
use rokc_core::Visibility;
use rokc_diagnostic::{Code, Span};
use rokc_lexer::TokenKind;

use super::python::starts_operand;
use super::{Parser, macros};
use crate::prec::PREFIX_PREC;

/// Rust's standard macros that are a function, an f-string or a literal
/// in Rok.
const STD_MACROS: &[&str] = &[
    "println",
    "print",
    "eprintln",
    "eprint",
    "format",
    "panic",
    "assert",
    "assert_eq",
    "assert_ne",
    "vec",
    "todo",
    "unimplemented",
    "unreachable",
    "dbg",
    "write",
    "writeln",
];

const COPY_OR_HANDLE: &str = "a struct is passed as a copy, a `List` or `Dict` as the same object";

fn fn_trait(name: &str) -> bool {
    matches!(name, "Fn" | "FnMut" | "FnOnce")
}

/// What `impl Trait` becomes, in argument and return position alike.
fn impl_trait_help(bound: &str) -> String {
    format!("write `some {bound}`: one type, picked by the compiler, that implements it")
}

/// A hole's expression as it may stand inside an f-string.
fn hole_ok(e: &str) -> bool {
    !e.is_empty() && !e.contains(['"', '{', '}', '\n'])
}

/// Rust's `"text {} {x:?}"` with `args` (positional and `name = value`)
/// as a Rok string: an f-string when it has holes.
fn rust_format(lit: &str, args: &[String]) -> Option<String> {
    let mut positional = Vec::new();
    let mut named = Vec::new();
    for a in args {
        match a.split_once(" = ") {
            Some((n, v)) if n.chars().all(|c| c.is_alphanumeric() || c == '_') => {
                named.push((n, v));
            }
            _ => positional.push(a.as_str()),
        }
    }
    let mut out = String::new();
    let mut next = 0;
    let mut holes = false;
    let mut chars = lit.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push_str("{{");
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push_str("}}");
            }
            '{' => {
                let mut inner = String::new();
                loop {
                    match chars.next()? {
                        '}' => break,
                        c => inner.push(c),
                    }
                }
                let (key, spec) = inner.split_once(':').unwrap_or((&inner, ""));
                let val = if key.is_empty() {
                    next += 1;
                    *positional.get(next - 1)?
                } else if let Ok(k) = key.parse::<usize>() {
                    *positional.get(k)?
                } else {
                    named
                        .iter()
                        .find(|(n, _)| *n == key)
                        .map_or(key, |(_, v)| *v)
                };
                if !hole_ok(val) {
                    return None;
                }
                holes = true;
                out.push('{');
                out.push_str(val);
                if !spec.is_empty() {
                    out.push(':');
                    out.push_str(spec);
                }
                out.push('}');
            }
            c => out.push(c),
        }
    }
    if holes {
        Some(format!("f\"{out}\""))
    } else if positional.is_empty() && named.is_empty() {
        Some(format!("\"{lit}\""))
    } else {
        None
    }
}

impl Parser<'_> {
    fn rust_error(&mut self, span: Span, msg: impl Into<String>, help: impl Into<String>) {
        self.error_help(Code::FOREIGN_SYNTAX, span, msg, help);
    }

    fn kind_at(&self, i: usize) -> Option<&TokenKind> {
        self.tok(i).map(|t| &t.kind)
    }

    /// Span of `tokens[a..=b]`.
    fn tokens_span(&self, a: usize, b: usize) -> Span {
        match (self.tok(a), self.tok(b)) {
            (Some(x), Some(y)) => Span {
                end: y.span.end,
                ..x.span
            },
            _ => self.peek_span(),
        }
    }

    fn bump_to(&mut self, end: usize) {
        while self.pos < end && !self.at_eof() {
            self.bump();
        }
    }

    /// A lifetime `'a` where a generic parameter, argument or bound goes:
    /// reported and skipped, in a `list` with its bounds and `,`.
    pub(super) fn rust_lifetime(&mut self, list: bool) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let Some(TokenKind::Label(name)) = self.kind_at(self.pos).cloned() else {
            return false;
        };
        let span = self.peek_span();
        self.rust_error(
            span,
            "Rok has no lifetimes",
            format!("delete `'{name}`: the GC keeps a value alive while anything refers to it"),
        );
        self.bump();
        if list {
            if self.eat(&TokenKind::Colon) {
                while matches!(self.peek(), TokenKind::Label(_)) {
                    self.bump();
                    if !self.eat(&TokenKind::Plus) {
                        break;
                    }
                }
            }
            self.eat(&TokenKind::Comma);
        }
        true
    }

    /// `&T`, `dyn Tr`, `impl Tr`, `Box<dyn Tr>` and `Fn(A) -> R` where a
    /// type goes: reported, and read as the Rok type each one becomes.
    #[allow(clippy::option_option)] // outer: a Rust form; inner: it parsed
    pub(super) fn rust_type(&mut self) -> Option<Option<Ty>> {
        if self.pending.is_some() {
            return None;
        }
        let i = self.pos;
        let start = self.peek_span();
        let next = self.kind_at(i + 1);
        let bound_next = matches!(next, Some(TokenKind::Ident(_) | TokenKind::LParen));
        match self.kind_at(i)? {
            TokenKind::Amp | TokenKind::AmpAmp => Some(self.rust_ref_type(None).map(|(t, _)| t)),
            TokenKind::Impl if bound_next => {
                self.bump();
                Some(self.rust_trait_type(start, "impl ", ""))
            }
            _ if self.word_at(i, "dyn") && bound_next => {
                self.bump_name();
                Some(self.rust_trait_type(start, "dyn ", ""))
            }
            _ if self.word_at(i, "Box")
                && next == Some(&TokenKind::Lt)
                && self.word_at(i + 2, "dyn") =>
            {
                self.bump_to(i + 2);
                self.bump_name();
                let t = self.rust_trait_type(start, "Box<dyn ", ">");
                if t.is_some() && !self.expect_closing_gt() {
                    return Some(None);
                }
                Some(t)
            }
            TokenKind::Ident(n)
                if fn_trait(n) && self.word_at(i, n) && next == Some(&TokenKind::LParen) =>
            {
                let n = n.clone();
                Some(self.rust_fn_trait(start, &format!("`{n}` traits"), ""))
            }
            _ => None,
        }
    }

    /// `Fn(A) -> R` at the cursor as `fn(A) -> R`, reported as `what`
    /// with `tail` (`>` of a `Box<..>`) left out of the spelling.
    fn rust_fn_trait(&mut self, start: Span, what: &str, tail: &str) -> Option<Ty> {
        let at = self.peek_span();
        let name = self.bump().kind.text();
        let kind = self.fn_type_rest(at)?;
        let written = self.text_in(self.to_prev(at));
        let f = format!(
            "fn{}",
            written.strip_prefix(name.as_str()).unwrap_or(&written)
        );
        let span = self.to_prev(start);
        let span = Span {
            end: span.end + u32::try_from(tail.len()).unwrap_or(0),
            ..span
        };
        self.rust_error(
            span,
            format!("Rok has no {what}"),
            format!("write `{f}`: one function type holds any function or closure"),
        );
        Some(Ty { kind, span: start })
    }

    /// The bounds after `impl` / `dyn` (`pre`), up to `post`.
    fn rust_trait_type(&mut self, start: Span, pre: &str, post: &str) -> Option<Ty> {
        if let Some(TokenKind::Ident(n)) = self.kind_at(self.pos) {
            if fn_trait(n) && self.kind_at(self.pos + 1) == Some(&TokenKind::LParen) {
                let what = format!("`{pre}{n}{post}`");
                return self.rust_fn_trait(start, &what, post);
            }
        }
        let mut bounds: Vec<Path> = Vec::new();
        loop {
            // `dyn Tr + 'a`: the lifetime goes with the rest.
            if matches!(self.peek(), TokenKind::Label(_)) {
                self.bump();
            } else {
                bounds.push(self.parse_type_path()?);
            }
            if !self.eat(&TokenKind::Plus) {
                break;
            }
        }
        let names: Vec<String> = bounds
            .iter()
            .map(|b| {
                rokc_ast::print::print_ty(&Ty {
                    kind: TyKind::Path(b.clone()),
                    span: b.span,
                })
            })
            .collect();
        let bound = match names.as_slice() {
            [one] => one.clone(),
            many => format!("({})", many.join(" + ")),
        };
        let span = self.to_prev(start);
        let help = if pre == "impl " {
            impl_trait_help(&bound)
        } else {
            format!("write `any {bound}`: it holds a value of any type that implements it")
        };
        let span = Span {
            end: span.end + u32::try_from(post.len()).unwrap_or(0),
            ..span
        };
        self.rust_error(span, format!("Rok has no `{pre}Trait{post}`"), help);
        Some(Ty {
            kind: TyKind::AnyTrait(bounds),
            span: start,
        })
    }

    /// `&T` / `&'a mut T` at the cursor, the type of parameter `param` if
    /// given: reported, read as `T`. Also says whether it was `&mut`.
    pub(super) fn rust_ref_type(&mut self, param: Option<&str>) -> Option<(Ty, bool)> {
        let start = self.peek_span();
        self.bump();
        let lifetime = matches!(self.peek(), TokenKind::Label(_));
        if lifetime {
            self.bump();
        }
        let is_mut = self.word_at(self.pos, "mut")
            && !matches!(
                self.peek_next(),
                Some(TokenKind::Comma | TokenKind::RParen | TokenKind::Gt | TokenKind::Eq)
            );
        if is_mut {
            self.bump();
        }
        if self.word_at(self.pos, "dyn") && !is_mut {
            self.bump_name();
            return self.rust_trait_type(start, "&dyn ", "").map(|t| (t, false));
        }
        let inner = self.peek_span();
        let ty = if self.eat(&TokenKind::LBracket) {
            let elem = self.with_type_sum(true, Self::parse_type)?;
            if self.eat(&TokenKind::Semi) {
                let len = self.parse_const_len()?;
                self.expect(&TokenKind::RBracket, "`]`")?;
                Ty {
                    kind: TyKind::Array(Box::new(elem), len),
                    span: inner,
                }
            } else {
                self.expect(&TokenKind::RBracket, "`]`")?;
                let list = format!("List<{}>", rokc_ast::print::print_ty(&elem));
                let span = self.to_prev(start);
                self.rust_error(
                    span,
                    "Rok has no slice references",
                    format!("write `{list}`: a `List` is passed as the same object"),
                );
                return Some((
                    Ty {
                        kind: TyKind::Path(Path {
                            segments: vec![rokc_ast::PathSegment {
                                name: "List".to_owned(),
                                generics: vec![elem],
                            }],
                            span: inner,
                            origin: None,
                        }),
                        span: inner,
                    },
                    false,
                ));
            }
        } else {
            self.parse_type_atom()?
        };
        let t = self.text_in(self.to_prev(inner));
        let mut help = match param {
            Some(p) if is_mut => format!(
                "write `ref {p}: {t}`: the function changes the caller's value, and each call \
                 writes `ref`"
            ),
            None if is_mut => {
                format!("write `{t}`; to change a caller's value, take `ref p: {t}`")
            }
            _ if t == "str" => "write `str`: Rok has one string type".to_owned(),
            Some(p) => format!("write `{p}: {t}`: {COPY_OR_HANDLE}"),
            None => format!("write `{t}`: {COPY_OR_HANDLE}"),
        };
        if lifetime {
            help.push_str("; there are no lifetimes");
        }
        let msg = if is_mut {
            "Rok has no `&mut` references"
        } else {
            "Rok has no `&` references"
        };
        let span = self.to_prev(start);
        self.rust_error(span, msg, help);
        Some((ty, is_mut))
    }

    /// `&self`, `&mut self` and `mut self` as a receiver; `mut x` and `&x`
    /// before a parameter's name are reported and skipped.
    pub(super) fn rust_param(&mut self) -> Option<Param> {
        if self.pending.is_some() {
            return None;
        }
        let (i, start) = (self.pos, self.peek_span());
        let amp = matches!(self.kind_at(i), Some(TokenKind::Amp | TokenKind::AmpAmp));
        let mut k = i + usize::from(amp);
        if amp && matches!(self.kind_at(k), Some(TokenKind::Label(_))) {
            k += 1;
        }
        let is_mut = self.word_at(k, "mut")
            && matches!(
                self.kind_at(k + 1),
                Some(TokenKind::Ident(_) | TokenKind::SelfLower | TokenKind::LParen)
            );
        k += usize::from(is_mut);
        if !(amp || is_mut)
            || !matches!(
                self.kind_at(k),
                Some(TokenKind::Ident(_) | TokenKind::SelfLower | TokenKind::LParen)
            )
        {
            return None;
        }
        let written = self.tokens_text(i, k);
        let span = self.tokens_span(i, k.saturating_sub(1));
        if self.kind_at(k) == Some(&TokenKind::SelfLower) {
            let (msg, help) = match (amp, is_mut) {
                (true, true) => (
                    "Rok has no `&mut self`",
                    "write `ref self`: the method changes its receiver",
                ),
                (true, false) => (
                    "Rok has no `&self`",
                    "write `self`: a method gets its receiver as a copy (`ref self` to change it)",
                ),
                _ => (
                    "Rok has no `mut self`",
                    "write `self`: a parameter can be reassigned already (`ref self` to change \
                     the caller's value)",
                ),
            };
            let span = self.tokens_span(i, k);
            self.rust_error(span, msg, help);
            self.bump_to(k + 1);
            return Some(Param {
                name: "self".to_owned(),
                ty: None,
                by_ref: amp && is_mut,
                default: None,
                variadic: false,
                span: start,
            });
        }
        let name = match self.kind_at(k) {
            Some(TokenKind::Ident(n)) => Some(n.clone()),
            _ => None,
        };
        let help = match (&name, amp) {
            (Some(n), false) => {
                format!("drop `mut`: write `{n}`; a parameter can be reassigned already")
            }
            (Some(n), true) => format!("write `{n}`: the parameter holds the value itself"),
            (None, _) => format!("drop `{written}`: the parameter holds the value itself"),
        };
        let msg = if amp {
            format!("Rok has no `{written}` patterns")
        } else {
            "Rok has no `mut` parameters".to_owned()
        };
        self.rust_error(span, msg, help);
        self.bump_to(k);
        None
    }

    /// `&x`, `&mut x` and `*x` at an operand: reported, and the operand
    /// kept. A `move` before a closure is reported and skipped.
    #[allow(clippy::option_option)] // outer: a Rust form; inner: it parsed
    pub(super) fn rust_prefix(&mut self) -> Option<Option<Expr>> {
        if self.pending.is_some() {
            return None;
        }
        let i = self.pos;
        let here = self.tok(i)?;
        if self.word_at(i, "move")
            && matches!(
                self.kind_at(i + 1),
                Some(TokenKind::Pipe | TokenKind::PipePipe)
            )
        {
            let help = self.move_help(i + 1);
            let span = here.span;
            self.rust_error(span, "Rok has no `move` closures", help);
            self.bump_name();
            return None;
        }
        let next = self.kind_at(i + 1)?;
        let operand = starts_operand(next)
            || matches!(
                next,
                TokenKind::Amp | TokenKind::AmpAmp | TokenKind::Star | TokenKind::SelfUpper
            );
        let star = match here.kind {
            TokenKind::Amp | TokenKind::AmpAmp if operand => false,
            // `a ** b` is Python's power, reported where it is read.
            TokenKind::Star
                if operand
                    && *next != TokenKind::Star
                    && !i
                        .checked_sub(1)
                        .and_then(|p| self.tok(p))
                        .is_some_and(|p| p.kind == TokenKind::Star) =>
            {
                true
            }
            _ => return None,
        };
        let start = here.span;
        let prev = i.checked_sub(1).and_then(|p| self.kind_at(p)).cloned();
        let in_loop = i.checked_sub(1).is_some_and(|p| self.word_at(p, "in"));
        self.bump();
        let is_mut = !star && self.word_at(self.pos, "mut") && {
            let n = self.kind_at(self.pos + 1);
            n.is_some_and(starts_operand)
        };
        if is_mut {
            self.bump();
        }
        let lead = self.to_prev(start);
        let Some(e) = self.parse_expr_bp(PREFIX_PREC) else {
            return Some(None);
        };
        let x = self.text_in(e.span);
        let arg = matches!(prev, Some(TokenKind::LParen | TokenKind::Comma));
        let pat = in_loop.then(|| self.loop_pattern(i - 1)).flatten();
        let (msg, help) = if star {
            (
                "Rok has no `*` dereference",
                if arg {
                    format!(
                        "write `{x}`: a value needs no dereference (a list spreads into \
                         arguments as `..{x}`)"
                    )
                } else {
                    format!("write `{x}`: a value needs no dereference")
                },
            )
        } else if is_mut {
            let help = match pat {
                Some(p) => format!("write `for ref {p} in {x}`: each `{p}` is then the element"),
                None if arg => format!(
                    "write `ref {x}`, for a parameter declared `ref p: T`: the function changes \
                     the caller's value"
                ),
                None => format!(
                    "write `{x}`; to let a function change it, pass `ref {x}` to a `ref` parameter"
                ),
            };
            ("Rok has no `&mut` borrows", help)
        } else {
            let help = match pat {
                Some(p) => format!("write `for {p} in {x}`: the loop reads the list itself"),
                None => format!("write `{x}`: {COPY_OR_HANDLE}"),
            };
            ("Rok has no `&` borrows", help)
        };
        self.rust_error(lead, msg, help);
        Some(Some(e))
    }

    /// The pattern of the `for` whose `in` is `tokens[in_at]`.
    fn loop_pattern(&self, in_at: usize) -> Option<String> {
        let line = self.tok(in_at)?.span.line;
        let mut k = in_at;
        while let Some(p) = k.checked_sub(1) {
            let t = self.tok(p)?;
            if t.span.line != line {
                return None;
            }
            if t.kind == TokenKind::For {
                let from = p + 1 + usize::from(self.kind_at(p + 1) == Some(&TokenKind::Ref));
                return Some(self.tokens_text(from, in_at)).filter(|s| !s.is_empty());
            }
            k = p;
        }
        None
    }

    /// Help for `move |..| body` from the `|` / `||`: the names the
    /// closure takes, in a capture list.
    fn move_help(&self, at: usize) -> String {
        let mut k = at;
        let mut params = Vec::new();
        if self.kind_at(k) == Some(&TokenKind::Pipe) {
            k += 1;
            while let Some(t) = self.tok(k) {
                match &t.kind {
                    TokenKind::Pipe | TokenKind::Eof => break,
                    TokenKind::Ident(n) => params.push(n.clone()),
                    _ => {}
                }
                k += 1;
            }
        }
        let head = self.tokens_text(at, k + 1);
        k += 1;
        let line = self.tok(k).map_or(0, |t| t.span.line);
        let braced = self.kind_at(k) == Some(&TokenKind::LBrace);
        let mut depth = 0u32;
        let mut lets = Vec::new();
        let mut names: Vec<String> = Vec::new();
        while let Some(t) = self.tok(k) {
            match &t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    let Some(d) = depth.checked_sub(1) else { break };
                    depth = d;
                    if braced && depth == 0 {
                        break;
                    }
                }
                TokenKind::Comma | TokenKind::Semi | TokenKind::Eof if !braced && depth == 0 => {
                    break;
                }
                _ if !braced && depth == 0 && t.span.line != line => break,
                TokenKind::Ident(n) => {
                    let after_dot = k
                        .checked_sub(1)
                        .is_some_and(|p| self.kind_at(p) == Some(&TokenKind::Dot));
                    let called = matches!(
                        self.kind_at(k + 1),
                        Some(TokenKind::LParen | TokenKind::ColonColon | TokenKind::Bang)
                    );
                    if k.checked_sub(1)
                        .is_some_and(|p| self.kind_at(p) == Some(&TokenKind::Let))
                    {
                        lets.push(n.clone());
                    } else if !after_dot
                        && !called
                        && !t.raw
                        && n.starts_with(|c: char| c.is_lowercase())
                        && !params.contains(n)
                        && !names.contains(n)
                    {
                        names.push(n.clone());
                    }
                }
                _ => {}
            }
            k += 1;
        }
        names.retain(|n| !lets.contains(n));
        if names.is_empty() {
            return format!(
                "a closure copies what it captures; to hand a value over, name it: \
                 `[move x] {head} ..`"
            );
        }
        let list: Vec<String> = names.iter().map(|n| format!("move {n}")).collect();
        format!(
            "write `[{}] {head} ..`: a closure copies what it captures, and `[move x]` hands \
             `x` over",
            list.join(", ")
        )
    }

    /// `&p` and `mut x` where a pattern goes: reported; `&p` is read as
    /// `p`, and `mut` is skipped.
    #[allow(clippy::option_option)] // outer: a Rust form; inner: it parsed
    pub(super) fn rust_pattern(&mut self) -> Option<Option<Pattern>> {
        if self.pending.is_some() {
            return None;
        }
        let i = self.pos;
        let start = self.peek_span();
        if self.word_at(i, "mut") && matches!(self.kind_at(i + 1), Some(TokenKind::Ident(_))) {
            let x = self.tokens_text(i + 1, i + 2);
            self.rust_error(
                start,
                "Rok has no `mut` bindings",
                format!("drop `mut`: write `{x}`; every variable can be reassigned"),
            );
            self.bump();
            return None;
        }
        if !matches!(self.kind_at(i), Some(TokenKind::Amp | TokenKind::AmpAmp)) {
            return None;
        }
        self.bump();
        if self.word_at(self.pos, "mut") {
            self.bump();
        }
        let lead = self.to_prev(start);
        let Some(p) = self.parse_pattern_atom() else {
            return Some(None);
        };
        let x = self.text_in(p.span);
        self.rust_error(
            lead,
            "Rok has no `&` patterns",
            format!("write `{x}`: the pattern matches the value itself"),
        );
        Some(Some(p))
    }

    /// `pub(crate)`, `pub(super)`, `pub(self)` and `pub(in path)` after
    /// `pub(`: reported, read through `)`.
    pub(super) fn rust_visibility(&mut self) -> Option<Visibility> {
        let i = self.pos;
        let which = match self.kind_at(i)? {
            TokenKind::SelfLower => "self",
            TokenKind::Ident(n) if matches!(n.as_str(), "crate" | "super" | "in") => n.as_str(),
            _ => return None,
        };
        let close = (i..self.tokens.len()).find(|&k| {
            matches!(
                self.kind_at(k),
                Some(TokenKind::RParen | TokenKind::LBrace | TokenKind::Semi | TokenKind::Eof)
            )
        })?;
        if self.kind_at(close) != Some(&TokenKind::RParen) {
            return None;
        }
        let (help, vis) = match which {
            "crate" => ("write `pub(pkg)`: visible in this package", Visibility::Pkg),
            "super" => (
                "write `pub(pkg)`: Rok has no reach for the parent module alone (`pub(sub)` \
                 opens an item to its own module's submodules)",
                Visibility::Pkg,
            ),
            "self" => (
                "delete it: an item is private to its module already",
                Visibility::Private,
            ),
            _ => (
                "write `pub(pkg)` (this package) or `pub(sub)` (this module's submodules)",
                Visibility::Pkg,
            ),
        };
        let written = format!("pub({})", self.tokens_text(i, close));
        let span = self.tokens_span(i.saturating_sub(2), close);
        self.rust_error(span, format!("Rok has no `{written}`"), help);
        self.bump_to(close + 1);
        Some(vis)
    }

    /// `macro_rules!` at the cursor.
    pub(super) fn at_macro_rules(&self) -> bool {
        self.pending.is_none()
            && self.word_at(self.pos, "macro_rules")
            && self.kind_at(self.pos + 1) == Some(&TokenKind::Bang)
    }

    /// `macro_rules! name { .. }`: reported and skipped whole.
    pub(super) fn rust_macro_rules(&mut self) -> bool {
        let i = self.pos;
        if !self.at_macro_rules() {
            return false;
        }
        let Some(TokenKind::Ident(name)) = self.kind_at(i + 2).cloned() else {
            return false;
        };
        if !matches!(
            self.kind_at(i + 3),
            Some(TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace)
        ) {
            return false;
        }
        let Some(close) = macros::close_of(self.tokens, i + 3) else {
            return false;
        };
        let span = self.tokens_span(i, i + 1);
        self.rust_error(
            span,
            "Rok has no `macro_rules!`",
            format!(
                "write a meta definition, called as `{name}!(..)`: \
                 `#[impl_macro({name})] @macro {{ ( @x: expr ) => {{ .. }} }}`"
            ),
        );
        self.bump_to(close + 1);
        self.eat(&TokenKind::Semi);
        true
    }

    /// `macro_rules!` at a statement: reported, a unit statement.
    pub(super) fn rust_stmt(&mut self) -> Option<Stmt> {
        let span = self.peek_span();
        self.rust_macro_rules().then(|| Stmt {
            kind: StmtKind::Expr(Expr {
                kind: ExprKind::Tuple(Vec::new()),
                span,
            }),
            span,
        })
    }

    /// `println!(..)`, `vec![..]`, `format!(..)` and the other standard
    /// macros, with no meta macro of that name: reported and skipped.
    pub(super) fn rust_macro(&mut self) -> Option<Expr> {
        let i = self.pos;
        let Some(TokenKind::Ident(name)) = self.kind_at(i).cloned() else {
            return None;
        };
        if self.pending.is_some()
            || !STD_MACROS.contains(&name.as_str())
            || !self.word_at(i, &name)
            || self.kind_at(i + 1) != Some(&TokenKind::Bang)
            || self.meta.find_macro(&name).is_some()
        {
            return None;
        }
        let open = i + 2;
        if !matches!(
            self.kind_at(open),
            Some(TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace)
        ) {
            return None;
        }
        let close = macros::close_of(self.tokens, open)?;
        // Arguments split at `,` and `;` at depth 0.
        let mut args = Vec::new();
        let mut repeat = false;
        let mut depth = 0u32;
        let mut from = open + 1;
        for k in open + 1..=close {
            match self.kind_at(k)? {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace if k < close => {
                    depth = depth.saturating_sub(1);
                }
                TokenKind::Comma | TokenKind::Semi if depth == 0 => {
                    repeat |= self.kind_at(k) == Some(&TokenKind::Semi);
                    args.push((from, k));
                    from = k + 1;
                }
                _ => {}
            }
        }
        if from < close {
            args.push((from, close));
        }
        let texts: Vec<String> = args.iter().map(|&(a, b)| self.tokens_text(a, b)).collect();
        let lit = args.first().and_then(|&(a, b)| match self.kind_at(a) {
            Some(TokenKind::StrLit(s)) if b == a + 1 => Some(s.clone()),
            _ => None,
        });
        let help = Self::std_macro_help(&name, &texts, lit.as_deref(), repeat);
        let msg = match name.as_str() {
            "vec" | "format" | "write" | "writeln" | "todo" | "unimplemented" => {
                format!("Rok has no `{name}!`")
            }
            _ => format!("`{name}` is a function in Rok, not a macro"),
        };
        let start = self.peek_span();
        let span = self.tokens_span(i, i + 1);
        self.rust_error(span, msg, help);
        self.bump_to(close + 1);
        Some(Expr {
            kind: ExprKind::Tuple(Vec::new()),
            span: self.to_prev(start),
        })
    }

    fn std_macro_help(name: &str, args: &[String], lit: Option<&str>, repeat: bool) -> String {
        // The format string and its arguments from `args[at..]`.
        let fmt = || rust_format(lit?, args.get(1..).unwrap_or_default());
        let fmt_at = |at: usize| -> Option<String> {
            let first = args.get(at)?;
            let s = first.strip_prefix('"')?.strip_suffix('"')?;
            rust_format(s, args.get(at + 1..).unwrap_or_default())
        };
        #[allow(clippy::literal_string_with_formatting_args)] // an f-string, shown
        let any_fmt = "an f-string formats its holes, as in `f\"{x} {y:?}\"`";
        match name {
            "vec" if repeat && args.len() == 2 => {
                format!("write `List::filled({}, {})`", args[1], args[0])
            }
            "vec" => format!("write `[{}]`: a list literal is a `List`", args.join(", ")),
            "format" => match fmt() {
                Some(s) => format!("write `{s}`: an f-string formats its holes"),
                None => format!("write an f-string: {any_fmt}"),
            },
            "println" | "print" | "eprintln" | "eprint" | "panic" | "dbg" if args.is_empty() => {
                if name == "panic" {
                    "call it: `panic(\"..\")`".to_owned()
                } else {
                    format!("call it: `{name}()`")
                }
            }
            "dbg" => format!("call it: `dbg({})`", args.join(", ")),
            "println" | "print" | "eprintln" | "eprint" | "panic" => match fmt() {
                Some(s) => format!("call it: `{name}({s})`"),
                None => format!("call it with an f-string: `{name}(f\"..\")`; {any_fmt}"),
            },
            "assert" | "assert_eq" | "assert_ne" => {
                let n = if name == "assert" { 1 } else { 2 };
                let mut call: Vec<String> = args.iter().take(n).cloned().collect();
                if args.len() > n {
                    match fmt_at(n) {
                        Some(m) => call.push(m),
                        None => {
                            return format!(
                                "call it: `{name}({}, f\"..\")`; {any_fmt}",
                                call.join(", ")
                            );
                        }
                    }
                }
                format!("call it: `{name}({})`", call.join(", "))
            }
            "todo" => "write `panic(\"not yet implemented\")`".to_owned(),
            "unimplemented" => "write `panic(\"not implemented\")`".to_owned(),
            "unreachable" if args.is_empty() => "call it: `unreachable()`".to_owned(),
            "unreachable" => match fmt() {
                Some(s) => format!("write `panic({s})`"),
                None => format!("write `panic(f\"..\")`; {any_fmt}"),
            },
            _ => {
                let f = args.first().map_or("f", String::as_str);
                let s = fmt_at(1).map(|s| {
                    if name == "writeln" {
                        match s.strip_suffix('"') {
                            Some(b) => format!("{b}\\n\""),
                            None => s,
                        }
                    } else {
                        s
                    }
                });
                match s {
                    Some(s) => format!("write `try {f}.write_str({s});`"),
                    None => format!("write `try {f}.write_str(f\"..\");`; {any_fmt}"),
                }
            }
        }
    }
}
