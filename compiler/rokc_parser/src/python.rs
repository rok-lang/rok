//! Python habits: forms a Python programmer types first, each reported
//! with the Rok form to write instead. Only token shapes that are never
//! valid Rok match.

use rokc_ast::{Expr, ExprKind, Pattern, PatternKind, Stmt, StmtKind};
use rokc_diagnostic::{Code, RokDiagnostic, Span};
use rokc_lexer::{Token, TokenKind};

use super::Parser;

/// Help for an "expected X" error: where, the help, and an optional fix.
pub(super) struct PyHelp {
    pub span: Span,
    pub help: String,
    pub fix: Option<(Span, String, String)>,
}

/// A token an operator, `if` or `for` may follow.
const fn ends_operand(k: &TokenKind) -> bool {
    use TokenKind as T;
    matches!(
        k,
        T::Ident(_)
            | T::IntLit(_)
            | T::FloatLit(_)
            | T::StrLit(_)
            | T::RawStrLit(_)
            | T::CharLit(_)
            | T::FStrEnd(_)
            | T::True
            | T::False
            | T::None
            | T::SelfLower
            | T::RParen
            | T::RBracket
    )
}

/// A token an operand starts with.
pub(super) const fn starts_operand(k: &TokenKind) -> bool {
    use TokenKind as T;
    matches!(
        k,
        T::Ident(_)
            | T::IntLit(_)
            | T::FloatLit(_)
            | T::StrLit(_)
            | T::RawStrLit(_)
            | T::CharLit(_)
            | T::FStrStart(_)
            | T::True
            | T::False
            | T::None
            | T::SelfLower
            | T::LParen
            | T::LBracket
            | T::Minus
            | T::Bang
    )
}

/// What `import name` of a Python module becomes.
pub(super) fn python_module(name: &str) -> Option<&'static str> {
    Some(match name {
        "math" => {
            "math needs no import in Rok: it is methods on numbers (`x.sqrt()`, `x.sin()`, \
             `x.floor()`) and constants such as `f64::PI`"
        }
        "os" | "sys" => {
            "write `use rok::std::{env, fs, process};`: `env::args()`, `env::var(\"HOME\")`, \
             `fs::read_to_str(p)`, `process::exit(1)`"
        }
        "subprocess" => {
            "write `use rok::std::process::Command;`: \
             `Command::new(\"git\").args([\"log\"]).output()`"
        }
        "pathlib" => "write `use rok::std::path::Path;`: `Path::new(\"a\") / \"b\"`",
        "random" => {
            "write `use rok::std::random;`: `random::range(1..7)`, `random::choice(xs)`, \
             `random::shuffle(xs)`"
        }
        "threading" => "write `use rok::std::thread;`: `thread::spawn(|| ..)`",
        "asyncio" => "`async fn` and `await` need no import in Rok",
        _ => return None,
    })
}

impl Parser<'_> {
    pub(super) fn tok(&self, i: usize) -> Option<&Token> {
        self.tokens.get(i)
    }

    /// `tokens[i]` is the plain word `w`, written at the use site.
    pub(super) fn word_at(&self, i: usize, w: &str) -> bool {
        self.tokens.get(i).is_some_and(|t| {
            !t.raw && t.origin.is_none() && matches!(&t.kind, TokenKind::Ident(n) if n == w)
        })
    }

    /// Text of `tokens[a..b]`.
    pub(super) fn tokens_text(&self, a: usize, b: usize) -> String {
        match (self.tok(a), b.checked_sub(1).and_then(|l| self.tok(l))) {
            (Some(x), Some(y)) if a < b => self.text_in(Span {
                end: y.span.end,
                ..x.span
            }),
            _ => String::new(),
        }
    }

    /// End (exclusive) of the statement from `from`: past a `;` or a
    /// `{ .. }` at depth 0, before a line break at depth 0 or an
    /// unmatched closer.
    fn python_stmt_end(&self, from: usize) -> usize {
        let line = self.tok(from).map_or(0, |t| t.span.line);
        let mut depth = 0u32;
        let mut i = from;
        while let Some(t) = self.tok(i) {
            match t.kind {
                TokenKind::Eof => return i,
                TokenKind::Semi if depth == 0 => return i + 1,
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    let Some(d) = depth.checked_sub(1) else {
                        return i;
                    };
                    depth = d;
                    if depth == 0 && t.kind == TokenKind::RBrace {
                        return i + 1;
                    }
                }
                _ if depth == 0 && t.span.line != line => return i,
                _ => {}
            }
            i += 1;
        }
        i
    }

    /// `with open(p) as f { .. }` / `assert c, "m"` at a statement start:
    /// reported and skipped whole.
    pub(super) fn python_stmt(&mut self) -> Option<Stmt> {
        if self.pending.is_some() {
            return None;
        }
        let i = self.pos;
        let (first, next) = (self.tok(i)?, self.tok(i + 1)?);
        let same = next.span.line == first.span.line;
        let span = first.span;
        // `assert(..)`, `assert -x` and `assert !x` are Rok.
        let bare_assert = !matches!(
            next.kind,
            TokenKind::LParen | TokenKind::LBracket | TokenKind::Minus | TokenKind::Bang
        );
        let end = self.python_stmt_end(i + 1);
        let (msg, help) =
            if self.word_at(i, "with") && same && matches!(next.kind, TokenKind::Ident(_)) {
                ("Rok has no `with` statement", self.with_help(i))
            } else if self.word_at(i, "assert") && same && starts_operand(&next.kind) && bare_assert
            {
                let semi = self.tok(end - 1).is_some_and(|t| t.kind == TokenKind::Semi);
                let args = self.tokens_text(i + 1, end - usize::from(semi));
                (
                    "`assert` is a function in Rok",
                    format!("call it: `assert({args});`"),
                )
            } else {
                return None;
            };
        self.error_help(Code::FOREIGN_SYNTAX, span, msg, help);
        while self.pos < end {
            self.bump();
        }
        let unit = Expr {
            kind: ExprKind::Tuple(Vec::new()),
            span,
        };
        Some(Stmt {
            kind: StmtKind::Expr(unit),
            span,
        })
    }

    /// `with EXPR as NAME`: the `using` statement it becomes.
    fn with_help(&self, i: usize) -> String {
        let end = self.python_stmt_end(i + 1);
        let as_at = (i + 1..end).find(|&k| self.tok(k).is_some_and(|t| t.kind == TokenKind::As));
        let name = as_at
            .and_then(|k| self.tok(k + 1))
            .and_then(|t| match &t.kind {
                TokenKind::Ident(n) => Some(n.clone()),
                _ => None,
            });
        let (Some(k), Some(name)) = (as_at, name) else {
            return "write `using x = try ..;`: `x` is closed when its scope ends".to_owned();
        };
        let expr = self.tokens_text(i + 1, k);
        match expr.strip_prefix("open(") {
            Some(rest) => format!(
                "write `using {name} = try File::open({rest};` (after `use rok::std::fs::File;`): \
                 `{name}` is closed when its scope ends"
            ),
            None => format!(
                "write `using {name} = try {expr};`: `{name}` is closed when its scope ends"
            ),
        }
    }

    /// Help for `expected` when the tokens at the cursor are a Python
    /// form: `and` / `or`, `not x`, `lambda`, `from m import x`, `"""`,
    /// `n // 2`, `pass`.
    pub(super) fn python_expected(&self, kind: &TokenKind) -> Option<PyHelp> {
        if self.pending.is_some() {
            return None;
        }
        let i = self.pos;
        let here = self.tok(i)?;
        let p = i.checked_sub(1)?;
        let prev = self.tok(p)?;
        let same_line = prev.span.line == here.span.line;
        if let TokenKind::Ident(w) = &here.kind {
            let op = match w.as_str() {
                "and" => Some("&&"),
                "or" => Some("||"),
                _ => None,
            };
            if let Some(op) =
                op.filter(|_| self.word_at(i, w) && same_line && ends_operand(&prev.kind))
            {
                return Some(PyHelp {
                    span: here.span,
                    help: format!("Rok writes `{w}` as `{op}`: `a {op} b`"),
                    fix: Some((here.span, op.to_owned(), format!("use `{op}`"))),
                });
            }
        }
        if self.word_at(p, "not") && same_line && starts_operand(&here.kind) {
            let x = match &here.kind {
                TokenKind::Ident(_) | TokenKind::IntLit(_) | TokenKind::SelfLower => {
                    here.kind.text()
                }
                _ => "x".to_owned(),
            };
            let span = Span {
                end: here.span.start,
                ..prev.span
            };
            return Some(PyHelp {
                span: prev.span,
                help: format!("Rok writes `not` as `!`: `!{x}`"),
                fix: Some((span, "!".to_owned(), "use `!`".to_owned())),
            });
        }
        if self.word_at(p, "lambda")
            && same_line
            && matches!(here.kind, TokenKind::Ident(_) | TokenKind::Colon)
        {
            return Some(PyHelp {
                span: prev.span,
                help: self.lambda_help(i),
                fix: None,
            });
        }
        if self.word_at(p, "from") && matches!(here.kind, TokenKind::Ident(_)) {
            if let Some(help) = self.python_from_help(i) {
                return Some(PyHelp {
                    span: prev.span,
                    help,
                    fix: None,
                });
            }
        }
        if matches!(&prev.kind, TokenKind::StrLit(s) if s.is_empty())
            && matches!(here.kind, TokenKind::StrLit(_))
            && prev.span.end == here.span.start
        {
            return Some(PyHelp {
                span: Span {
                    end: here.span.end,
                    ..prev.span
                },
                help: "Rok has no `\"\"\"` strings: an ordinary `\"..\"` string may span lines"
                    .to_owned(),
                fix: None,
            });
        }
        if *kind == TokenKind::Semi && prev.span.line < here.span.line {
            if self.word_at(p, "pass")
                && p.checked_sub(1)
                    .and_then(|q| self.tok(q))
                    .is_none_or(|t| t.span.line < prev.span.line)
            {
                return Some(PyHelp {
                    span: prev.span,
                    help: "Rok needs no `pass`: an empty block `{}` is complete; delete `pass`"
                        .to_owned(),
                    fix: None,
                });
            }
            if let Some(help) = self.floor_div_help(prev) {
                return Some(PyHelp {
                    span: self.after_prev(),
                    help,
                    fix: None,
                });
            }
        }
        None
    }

    /// `lambda x, y: body` from the token after `lambda`.
    fn lambda_help(&self, i: usize) -> String {
        const PLAIN: &str = "a closure is written `|x| x + 1`";
        let mut k = i;
        let mut params = Vec::new();
        while let Some(TokenKind::Ident(n)) = self.tok(k).map(|t| &t.kind) {
            params.push(n.clone());
            k += 1;
            if self.tok(k).is_some_and(|t| t.kind == TokenKind::Comma) {
                k += 1;
            }
        }
        if !self.tok(k).is_some_and(|t| t.kind == TokenKind::Colon) {
            return PLAIN.to_owned();
        }
        // The body runs to `,` / `;` / a closer at depth 0, or the line end.
        let line = self.tok(k).map_or(0, |t| t.span.line);
        let mut depth = 0u32;
        let mut end = k + 1;
        while let Some(t) = self.tok(end) {
            match t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace if depth == 0 => {
                    break;
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => depth -= 1,
                TokenKind::Comma | TokenKind::Semi | TokenKind::Eof if depth == 0 => break,
                _ if t.span.line != line => break,
                _ => {}
            }
            end += 1;
        }
        let body = self.tokens_text(k + 1, end);
        if body.is_empty() {
            return PLAIN.to_owned();
        }
        format!("a closure is written `|{}| {body}`", params.join(", "))
    }

    /// `from a.b import c, d` from the token after `from`.
    fn python_from_help(&self, i: usize) -> Option<String> {
        let line = self.tok(i)?.span.line;
        let at_line = |k: usize| self.tok(k).filter(|t| t.span.line == line);
        let mut path = Vec::new();
        let mut k = i;
        loop {
            let TokenKind::Ident(n) = &at_line(k)?.kind else {
                return None;
            };
            if n == "import" && !path.is_empty() {
                break;
            }
            path.push(n.clone());
            k += 1;
            if at_line(k).is_some_and(|t| t.kind == TokenKind::Dot) {
                k += 1;
            }
        }
        if let Some(m) = python_module(&path[0]).filter(|_| path.len() == 1) {
            return Some(m.to_owned());
        }
        let mut names = Vec::new();
        k += 1;
        while let Some(TokenKind::Ident(n)) = at_line(k).map(|t| &t.kind) {
            names.push(n.clone());
            k += 1;
            if !at_line(k).is_some_and(|t| t.kind == TokenKind::Comma) {
                break;
            }
            k += 1;
        }
        let names = match names.as_slice() {
            [] => return None,
            [one] => one.clone(),
            many => format!("{{{}}}", many.join(", ")),
        };
        Some(format!("write `use {}::{names};`", path.join("::")))
    }

    /// `n // 2;`: `//` started a comment, so the statement lost its end.
    /// Reads the line from the file; `None` when it is not there.
    fn floor_div_help(&self, prev: &Token) -> Option<String> {
        if prev.origin.is_some() {
            return None;
        }
        let src = std::fs::read_to_string(&self.file).ok()?;
        let (start, end) = (prev.span.start as usize, prev.span.end as usize);
        if src.get(start..end)? != prev.kind.text() {
            return None;
        }
        let line = src.get(end..)?.lines().next()?;
        let comment = line.trim_start().strip_prefix("//")?;
        if comment.starts_with(['/', '!']) {
            return None;
        }
        let rhs = comment.trim().strip_suffix(';')?.trim_end();
        if rhs.is_empty()
            || !rhs
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '(' | ')'))
        {
            return None;
        }
        let lhs = match &prev.kind {
            TokenKind::Ident(n) | TokenKind::IntLit(n) => n.clone(),
            _ => "n".to_owned(),
        };
        Some(format!(
            "`//` starts a comment in Rok, so `// {rhs};` was skipped; floor division is \
             `{lhs}.div_floor({rhs})` (integer `/` rounds toward zero)"
        ))
    }

    /// `x if c else y` at the cursor, just past `x`.
    pub(super) fn python_ternary_ahead(&self) -> bool {
        if self.pending.is_some() || !matches!(self.peek(), TokenKind::If) {
            return false;
        }
        let i = self.pos;
        let Some(prev) = i.checked_sub(1).and_then(|p| self.tok(p)) else {
            return false;
        };
        let line = self.peek_span().line;
        if prev.span.line != line || !ends_operand(&prev.kind) {
            return false;
        }
        let mut depth = 0u32;
        for (k, t) in self.tokens.iter().enumerate().skip(i + 1) {
            if t.span.line != line {
                return false;
            }
            match t.kind {
                TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RParen | TokenKind::RBracket => {
                    let Some(d) = depth.checked_sub(1) else {
                        return false;
                    };
                    depth = d;
                }
                // `else {` / `else if` is Rok's `let .. else`.
                TokenKind::Else if depth == 0 => {
                    return self
                        .tok(k + 1)
                        .is_some_and(|n| !matches!(n.kind, TokenKind::LBrace | TokenKind::If));
                }
                TokenKind::LBrace | TokenKind::Semi | TokenKind::Eof => return false,
                _ => {}
            }
        }
        false
    }

    /// `a if c else b`: read it whole, report the `if` expression.
    pub(super) fn python_ternary(&mut self, a: Expr) -> Option<Expr> {
        let at = self.peek_span();
        self.bump();
        let c = self.parse_expr()?;
        self.expect(&TokenKind::Else, "`else`")?;
        let b = self.parse_expr()?;
        let (a_text, c, b) = (
            self.text_in(a.span),
            self.text_in(c.span),
            self.text_in(b.span),
        );
        self.error_help(
            Code::FOREIGN_SYNTAX,
            at,
            "Rok has no `a if c else b` expression",
            format!("`if` is an expression: `if {c} {{ {a_text} }} else {{ {b} }}`"),
        );
        Some(a)
    }

    /// The second `*` of `a ** b` at the cursor, where an operand
    /// starts: reported and skipped, so `a ** b` reads as `a * b`.
    pub(super) fn python_pow(&mut self) {
        let i = self.pos;
        let (Some(star), Some(first)) = (self.tok(i), i.checked_sub(1).and_then(|p| self.tok(p)))
        else {
            return;
        };
        if self.pending.is_some()
            || star.kind != TokenKind::Star
            || first.kind != TokenKind::Star
            || first.span.end != star.span.start
        {
            return;
        }
        let simple = |k: usize| {
            self.tok(k)
                .filter(|t| {
                    matches!(
                        t.kind,
                        TokenKind::Ident(_) | TokenKind::IntLit(_) | TokenKind::FloatLit(_)
                    )
                })
                .map(|t| t.kind.text())
        };
        let base = i
            .checked_sub(2)
            .and_then(simple)
            .unwrap_or_else(|| "x".to_owned());
        let exp = simple(i + 1).unwrap_or_else(|| "n".to_owned());
        let at = Span {
            end: star.span.end,
            ..first.span
        };
        self.error_help(
            Code::FOREIGN_SYNTAX,
            at,
            "Rok has no `**` operator",
            format!("write `{base}.pow({exp})`"),
        );
        self.bump();
    }

    /// After `x is` (not `as`): Python's `is not` (`not` at the cursor).
    /// Reports it and skips `not`; `lhs` is `x`'s span.
    pub(super) fn python_is_not(&mut self, lhs: Span, is_cast: bool) {
        let i = self.pos;
        if is_cast
            || self.pending.is_some()
            || !self.word_at(i, "not")
            || !self.tok(i + 1).is_some_and(|t| {
                t.span.line == self.tokens[i].span.line
                    && matches!(
                        t.kind,
                        TokenKind::Ident(_) | TokenKind::None | TokenKind::LParen
                    )
            })
        {
            return;
        }
        let at = self.tokens[i].span;
        let x = self.text_in(lhs);
        let ty = if self.tok(i + 1).is_some_and(|t| t.kind == TokenKind::None) {
            None
        } else {
            let end = self.python_type_end(i + 1);
            Some(self.tokens_text(i + 1, end))
        };
        let help = match ty {
            None => format!("write `!({x} is none)`"),
            Some(t) => format!("write `!({x} is {t})`"),
        };
        self.error_help(
            Code::FOREIGN_SYNTAX,
            at,
            "Rok has no `is not` operator",
            help,
        );
        self.bump();
    }

    /// End of a type path starting at `from` (`a::B<C>`).
    fn python_type_end(&self, from: usize) -> usize {
        let mut depth = 0u32;
        let mut i = from;
        while let Some(t) = self.tok(i) {
            match t.kind {
                TokenKind::Lt => depth += 1,
                TokenKind::Gt if depth > 0 => depth -= 1,
                TokenKind::Ident(_) | TokenKind::ColonColon => {}
                TokenKind::Comma if depth > 0 => {}
                _ => break,
            }
            i += 1;
        }
        i
    }

    /// `else`, or Python's `elif` (reported, read as `else if`).
    pub(super) fn eat_else(&mut self) -> bool {
        if self.eat(&TokenKind::Else) {
            return true;
        }
        let i = self.pos;
        if self.pending.is_some()
            || !self.word_at(i, "elif")
            || !self.tok(i + 1).is_some_and(|t| {
                t.span.line == self.tokens[i].span.line
                    && (matches!(t.kind, TokenKind::Let)
                        || (starts_operand(&t.kind)
                            && !matches!(
                                t.kind,
                                TokenKind::LParen | TokenKind::LBracket | TokenKind::Minus
                            )))
            })
        {
            return false;
        }
        let span = self.tokens[i].span;
        self.diagnostics.push(
            RokDiagnostic::error(Code::FOREIGN_SYNTAX, span, "Rok has no `elif`")
                .with_file(self.file.clone())
                .with_help("write `else if`")
                .with_fix(span, "else if", "use `else if`"),
        );
        self.bump();
        self.pending = Some(TokenKind::If);
        true
    }

    /// Python's `except` where a `catch` arm may start.
    pub(super) fn at_except(&self) -> bool {
        let i = self.pos;
        self.pending.is_none()
            && self.word_at(i, "except")
            && self.tok(i + 1).is_some_and(|t| {
                t.span.line == self.tokens[i].span.line
                    && matches!(t.kind, TokenKind::Ident(_) | TokenKind::LBrace)
            })
    }

    /// `except` (cursor) as a `catch` arm head: reports and skips it.
    /// True for `except T as e`, which `python_except_as` reads.
    pub(super) fn python_except(&mut self) -> bool {
        let span = self.peek_span();
        let i = self.pos;
        let as_form = matches!(self.tok(i + 1).map(|t| &t.kind), Some(TokenKind::Ident(_)))
            && self.tok(i + 2).is_some_and(|t| t.kind == TokenKind::As)
            && matches!(self.tok(i + 3).map(|t| &t.kind), Some(TokenKind::Ident(_)));
        let help = if as_form {
            let (ty, e) = (
                self.tokens_text(i + 1, i + 2),
                self.tokens_text(i + 3, i + 4),
            );
            format!("write `catch {e}: {ty} {{ .. }}`")
        } else {
            "write `catch e { .. }`, or `catch e: ValueError { .. }` for one error type".to_owned()
        };
        let d = RokDiagnostic::error(Code::FOREIGN_SYNTAX, span, "Rok has no `except`")
            .with_file(self.file.clone())
            .with_help(help);
        self.diagnostics.push(if as_form {
            d
        } else {
            d.with_fix(span, "catch", "use `catch`")
        });
        self.bump();
        as_form
    }

    /// `T as e` after `except`, as the pattern `e: T`.
    pub(super) fn python_except_as(&mut self) -> Option<Pattern> {
        let span = self.peek_span();
        let ty = self.parse_type()?;
        self.bump(); // `as`
        let TokenKind::Ident(binder) = self.bump().kind else {
            return None;
        };
        Some(Pattern {
            kind: PatternKind::TypeCheck {
                binder: Some(binder),
                ty,
            },
            span,
        })
    }

    /// A `for` head, `pat in iter`; Python's `for i, x in ..` is
    /// reported and read as `for (i, x) in ..`.
    pub(super) fn parse_for_head(&mut self) -> Option<(Pattern, Expr)> {
        let mut pat = self.parse_pattern()?;
        let bare_tuple = self.pending.is_none() && matches!(self.peek(), TokenKind::Comma);
        if bare_tuple {
            let mut pats = vec![pat];
            while self.eat(&TokenKind::Comma) {
                pats.push(self.parse_pattern()?);
            }
            let span = self.to_prev(pats[0].span);
            pat = Pattern {
                kind: PatternKind::Tuple(pats),
                span,
            };
        }
        self.expect_kw("in")?;
        let iter = self.parse_head()?;
        if bare_tuple {
            let names = self.text_in(pat.span);
            let it = self.text_in(iter.span);
            let it = match it
                .strip_prefix("enumerate(")
                .and_then(|s| s.strip_suffix(')'))
            {
                Some(xs) => format!("{xs}.enumerate()"),
                None => it.strip_suffix(".items()").unwrap_or(&it).to_owned(),
            };
            self.error_help(
                Code::FOREIGN_SYNTAX,
                pat.span,
                "a `for` over pairs needs parentheses around the names",
                format!("write `for ({names}) in {it}`"),
            );
        }
        Some((pat, iter))
    }

    /// `[x * x for x in xs if c]`, `{k: v for ..}`, `{x for ..}` or
    /// `(x for ..)` opening at `tokens[open]`: reported, with the cursor
    /// left on the closer. False when it is not one.
    pub(super) fn comprehension(&mut self, open: usize) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let Some(opener) = self.tok(open) else {
            return false;
        };
        let what = match opener.kind {
            TokenKind::LBracket => "list comprehensions",
            TokenKind::LBrace => "set comprehensions",
            TokenKind::LParen => "generator expressions",
            _ => return false,
        };
        let brace = opener.kind == TokenKind::LBrace;
        let (mut depth, mut i) = (0u32, open + 1);
        let (mut colon, mut f, mut n, mut cond, mut nested) = (None, None, None, None, false);
        let close = loop {
            let Some(t) = self.tok(i) else {
                return false;
            };
            match t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace if depth == 0 => {
                    break i;
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => depth -= 1,
                TokenKind::Semi | TokenKind::Eof if depth == 0 => return false,
                TokenKind::Colon if depth == 0 && f.is_none() => colon = Some(i),
                TokenKind::For if depth == 0 => {
                    if f.is_some() {
                        nested = true;
                    } else {
                        let prev = &self.tokens[i - 1];
                        if i == open + 1
                            || !ends_operand(&prev.kind)
                            || (brace && prev.span.line != t.span.line)
                        {
                            return false;
                        }
                        f = Some(i);
                    }
                }
                TokenKind::If if depth == 0 && n.is_some() && cond.is_none() => cond = Some(i),
                _ if depth == 0 && f.is_some() && n.is_none() && self.word_at(i, "in") => {
                    n = Some(i);
                }
                _ => {}
            }
            i += 1;
        };
        let (Some(f), Some(n)) = (f, n) else {
            return false;
        };
        let what = if brace && colon.is_some() {
            "dict comprehensions"
        } else {
            what
        };
        let help = if nested {
            "chain iterator methods (`.flat_map(..)` for the inner `for`), or write a `for` loop"
                .to_owned()
        } else {
            format!(
                "chain iterator methods: `{}`",
                self.comprehension_chain(open, colon, f, n, cond, close)
            )
        };
        let span = Span {
            end: self.tokens[close].span.end,
            ..opener.span
        };
        self.error_help(
            Code::FOREIGN_SYNTAX,
            span,
            format!("Rok has no {what}"),
            help,
        );
        while self.pos < close {
            self.bump();
        }
        true
    }

    /// The iterator chain for a comprehension with its parts at these
    /// token indices.
    fn comprehension_chain(
        &self,
        open: usize,
        colon: Option<usize>,
        f: usize,
        n: usize,
        cond: Option<usize>,
        close: usize,
    ) -> String {
        let mut p = self.tokens_text(f + 1, n);
        if p.contains(',') && !p.starts_with('(') {
            p = format!("({p})");
        }
        let src = self.tokens_text(n + 1, cond.unwrap_or(close));
        let range = src
            .strip_prefix("range(")
            .and_then(|s| s.strip_suffix(')'))
            .map(|a| match a.split_once(',') {
                Some((lo, hi)) => format!("({}..{})", lo.trim(), hi.trim()),
                None => format!("(0..{a})"),
            });
        let mut chain = range.unwrap_or_else(|| {
            if src.contains(' ') {
                format!("({src})")
            } else {
                src.clone()
            }
        });
        if let Some(c) = cond {
            chain += &format!(".filter(|{p}| {})", self.tokens_text(c + 1, close));
        }
        let elem = match colon {
            Some(c) if self.tokens[open].kind == TokenKind::LBrace => format!(
                "({}, {})",
                self.tokens_text(open + 1, c),
                self.tokens_text(c + 1, f)
            ),
            _ => self.tokens_text(open + 1, f),
        };
        if elem != p {
            chain += &format!(".map(|{p}| {elem})");
        }
        chain += match (&self.tokens[open].kind, colon) {
            (TokenKind::LBracket, _) => ".to_list()",
            (TokenKind::LBrace, Some(_)) => ".to_dict()",
            (TokenKind::LBrace, None) => ".to_set()",
            _ => "",
        };
        chain
    }

    /// `!r` / `!s` / `!a` (cursor on `!`) ending an f-string hole: the
    /// conversion letter.
    pub(super) fn python_conversion(&self) -> Option<&'static str> {
        let i = self.pos;
        let (bang, letter, end) = (self.tok(i)?, self.tok(i + 1)?, self.tok(i + 2)?);
        if self.pending.is_some()
            || bang.kind != TokenKind::Bang
            || letter.span.start != bang.span.end
            || !matches!(end.kind, TokenKind::HoleEnd | TokenKind::HoleSpec)
        {
            return None;
        }
        match &letter.kind {
            TokenKind::Ident(c) if c == "r" => Some("r"),
            TokenKind::Ident(c) if c == "s" => Some("s"),
            TokenKind::Ident(c) if c == "a" => Some("a"),
            _ => None,
        }
    }

    /// `f(end="")`: a named argument written with `=` (cursor on the
    /// name). Reports it; the caller reads it as `end: ""`.
    pub(super) fn python_named_arg(&mut self) -> bool {
        let i = self.pos;
        let (Some(name), Some(eq)) = (self.tok(i), self.tok(i + 1)) else {
            return false;
        };
        if self.pending.is_some()
            || name.raw
            || !matches!(name.kind, TokenKind::Ident(_))
            || eq.kind != TokenKind::Eq
        {
            return false;
        }
        let at = eq.span;
        let end = {
            let mut depth = 0u32;
            let mut k = i + 2;
            while let Some(t) = self.tok(k) {
                match t.kind {
                    TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                    TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace if depth == 0 => {
                        break;
                    }
                    TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => depth -= 1,
                    TokenKind::Comma | TokenKind::Semi | TokenKind::Eof if depth == 0 => break,
                    _ => {}
                }
                k += 1;
            }
            k
        };
        let n = name.kind.text();
        let v = self.tokens_text(i + 2, end);
        self.diagnostics.push(
            RokDiagnostic::error(
                Code::FOREIGN_SYNTAX,
                at,
                "a named argument is written with `:`, not `=`",
            )
            .with_file(self.file.clone())
            .with_help(format!("write `{n}: {v}`"))
            .with_fix(at, ":", "use `:`"),
        );
        true
    }
}
