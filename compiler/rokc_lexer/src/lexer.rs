//! Lexer driver. Lines are 1-based and end at LF, CRLF or a lone CR;
//! columns count code points (spec 1.1.3, 1.1.5).

use rokc_ast::escape::{EscapeMode, unescape};
use rokc_diagnostic::{ErrorKind, RokDiagnostic, Span};

use crate::token::{DocLine, Token, TokenKind, keyword_kind};
use crate::unicode::{
    is_bidi_control, is_ident_continue, is_ident_start, is_pictographic, mixed_script,
    normalize_ident, skeleton,
};

pub const INT_SUFFIXES: &[&str] = &[
    "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
    "c_char",
];
pub const FLOAT_SUFFIXES: &[&str] = &["f32", "f64"];

pub struct Lexer<'src> {
    src: &'src str,
    bytes: &'src [u8],
    file: String,
    pos: usize,
    line: u32,
    col: u32,
    diagnostics: Vec<RokDiagnostic>,
    /// The previous token can end an expression (`x`, `1`, `)`, ...).
    /// Also marks a following `-` as binary (see `prev_minus`).
    prev_ends_expr: bool,
    /// Previous token was a unary `-`, so `-128i8` may use magnitude
    /// `MAX + 1`.
    prev_minus: bool,
    /// First occurrence of each distinct (normalized) identifier, for
    /// the confusable check (spec 1.4.2); `any_non_ascii` gates it.
    idents: Vec<(String, Span)>,
    seen: std::collections::HashSet<String>,
    any_non_ascii: bool,
    /// Registered extension operator tokens (spec 1.10.4), longest first.
    ext_ops: Vec<String>,
    /// Doc comments seen since the last token.
    docs: Vec<DocLine>,
    /// Open f-strings, innermost last (spec 1.7.3).
    fstr: Vec<Frame>,
    /// Start of the outermost f-string a hole error (backslash,
    /// newline) was reported in: one per literal.
    hole_reported: Option<usize>,
}

/// One level of f-string nesting. `triple`: of the f-string it is in.
#[derive(Clone, Copy)]
enum Frame {
    /// Literal text of the f-string opened at `start`.
    Text { triple: bool, start: usize },
    /// Hole code; `depth` counts open brackets.
    Hole { triple: bool, depth: u32 },
    /// A hole's format spec after `:`.
    Spec { triple: bool },
}

impl<'src> Lexer<'src> {
    #[must_use]
    pub fn new(src: &'src str, file: impl Into<String>) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            file: file.into(),
            pos: 0,
            line: 1,
            col: 1,
            diagnostics: Vec::new(),
            prev_ends_expr: false,
            prev_minus: false,
            idents: Vec::new(),
            seen: std::collections::HashSet::new(),
            any_non_ascii: false,
            ext_ops: Vec::new(),
            docs: Vec::new(),
            fstr: Vec::new(),
            hole_reported: None,
        }
    }

    /// Also lex these registered extension operator tokens (spec 1.10.4,
    /// gathered by the driver's `#[infix]` pre-pass), longest first.
    #[must_use]
    pub fn with_ext_ops(mut self, ops: Vec<String>) -> Self {
        self.ext_ops = ops;
        self
    }

    pub fn lex_all(mut self) -> (Vec<Token>, Vec<RokDiagnostic>) {
        // A leading BOM is ignored (spec 1.1.2).
        if self.src.starts_with('\u{FEFF}') {
            self.pos = '\u{FEFF}'.len_utf8();
        }
        self.advance(rokc_core::shebang_len(&self.src[self.pos..]));
        self.check_bidi();
        let mut tokens = Vec::new();
        loop {
            let from = self.pos;
            let flat = self.in_flat_hole();
            let piece = matches!(
                self.fstr.last(),
                Some(Frame::Text { .. } | Frame::Spec { .. })
            );
            if !piece {
                self.skip_trivia();
            }
            if self.pos >= self.bytes.len() {
                self.close_fstrings(&mut tokens);
                let mut eof = Token::new(TokenKind::Eof, self.span_here());
                eof.docs = std::mem::take(&mut self.docs);
                tokens.push(eof);
                break;
            }
            // `None`: invalid input was reported and skipped.
            let tok = if piece {
                self.fstr_piece()
            } else {
                self.next_token()
            };
            if let Some(nl) = self.src[from..self.pos].find(['\n', '\r']).filter(|_| flat) {
                self.hole_error(
                    from + nl,
                    "newline in f-string `{...}` code; hole code spans lines only in \
                     `f\"\"\"...\"\"\"`",
                );
            }
            if let Some(mut tok) = tok {
                tok.docs = std::mem::take(&mut self.docs);
                self.prev_minus = tok.kind == TokenKind::Minus && !self.prev_ends_expr;
                self.prev_ends_expr = matches!(
                    tok.kind,
                    TokenKind::Ident(_)
                        | TokenKind::IntLit(_)
                        | TokenKind::FloatLit(_)
                        | TokenKind::StrLit(_)
                        | TokenKind::RawStrLit(_)
                        | TokenKind::ByteStrLit(_)
                        | TokenKind::FStrEnd(_)
                        | TokenKind::CharLit(_)
                        | TokenKind::CCharLit(_)
                        | TokenKind::RParen
                        | TokenKind::RBracket
                        | TokenKind::RBrace
                        | TokenKind::SelfLower
                        | TokenKind::SelfUpper
                        | TokenKind::True
                        | TokenKind::False
                        | TokenKind::None
                        | TokenKind::Question
                );
                tokens.push(tok);
            }
        }
        self.check_confusables();
        (tokens, self.diagnostics)
    }

    /// Spec 1.1: bidirectional formatting controls are rejected
    /// anywhere - comments and strings included (`\u{..}` escapes only).
    fn check_bidi(&mut self) {
        if self.src.is_ascii() {
            return;
        }
        let hits: Vec<(usize, char)> = self
            .src
            .char_indices()
            .filter(|&(_, c)| is_bidi_control(c))
            .collect();
        for (at, c) in hits {
            let (line, col) = self.line_col(at);
            let span = Self::make_span(at, at + c.len_utf8(), line, col);
            self.error(
                span,
                format!(
                    "bidirectional control character U+{:04X} in source; write it as an \
                     escape (`\\u{{{:X}}}`) instead",
                    c as u32, c as u32
                ),
            );
        }
    }

    /// Spec 1.4.2: warn about distinct identifiers that share a
    /// confusable skeleton (`σ²` / `σ2`, Latin `a` / Cyrillic `а`).
    fn check_confusables(&mut self) {
        if !self.any_non_ascii {
            return;
        }
        let mut by_skel: std::collections::HashMap<String, &(String, Span)> =
            std::collections::HashMap::new();
        let mut warns = Vec::new();
        for id in &self.idents {
            match by_skel.entry(skeleton(&id.0)) {
                std::collections::hash_map::Entry::Occupied(o) => {
                    warns.push((
                        id.1,
                        format!(
                            "identifier `{}` looks like `{}` (confusable spelling)",
                            id.0,
                            o.get().0
                        ),
                    ));
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(id);
                }
            }
        }
        for (span, msg) in warns {
            self.warning(span, msg);
        }
    }

    fn next_token(&mut self) -> Option<Token> {
        let start = self.pos;
        let line = self.line;
        let col = self.col;

        let b = self.bytes[start];
        let next = self.bytes.get(start + 1).copied();
        if let Some(kind) = self.hole_punct(b, next) {
            return Some(Token::new(
                kind,
                Self::make_span(start, self.pos, line, col),
            ));
        }
        if b == b'\\' && matches!(self.fstr.last(), Some(Frame::Hole { .. })) {
            self.advance(if next == Some(b'"') { 2 } else { 1 });
            self.hole_error(
                start,
                "backslash in f-string `{...}` code; a hole is plain code, so write `\"` \
                 directly: `{x ?? \"a\"}`, `{d[\"k\"]}`",
            );
            return None;
        }
        let kind = match b {
            b'#' if next == Some(b'[') => {
                self.advance(2);
                TokenKind::HashLBracket
            }
            b'#' if next == Some(b'!') && self.bytes.get(start + 2) == Some(&b'[') => {
                self.advance(3);
                TokenKind::HashBangLBracket
            }
            b'(' => self.single(TokenKind::LParen),
            b')' => self.single(TokenKind::RParen),
            b'[' => self.single(TokenKind::LBracket),
            b']' => self.single(TokenKind::RBracket),
            b'{' => self.single(TokenKind::LBrace),
            b'}' => self.single(TokenKind::RBrace),
            b',' => self.single(TokenKind::Comma),
            b';' => self.single(TokenKind::Semi),
            b'@' => self.single(TokenKind::At),
            b'~' => self.single(TokenKind::Tilde),
            b'+' => self.lex_op2(b'=', TokenKind::PlusEq, TokenKind::Plus),
            b'-' => self.lex_minus(),
            b'*' => self.lex_op2(b'=', TokenKind::StarEq, TokenKind::Star),
            b'/' => self.lex_op2(b'=', TokenKind::SlashEq, TokenKind::Slash),
            b'%' => self.lex_percent(),
            b'^' => self.lex_op2(b'=', TokenKind::CaretEq, TokenKind::Caret),
            b'=' => self.lex_eq(),
            b'!' => self.lex_op2(b'=', TokenKind::NotEq, TokenKind::Bang),
            b'<' => self.lex_lt(),
            b'>' => self.lex_gt(),
            b'&' => self.lex_amp(),
            b'|' => self.lex_pipe(),
            b'.' => self.lex_dot(),
            b':' => self.lex_colon(),
            b'?' => self.lex_question(),
            b'\'' => self.lex_quote_or_label(),
            b'"' => self.lex_str_lit(),
            b'r' if next == Some(b'"') || next == Some(b'#') => self.lex_raw_str_lit(),
            b'f' if next == Some(b'"') => {
                let triple = self.src[start + 1..].starts_with("\"\"\"");
                self.advance(if triple { 4 } else { 2 });
                self.fstr.push(Frame::Text { triple, start });
                TokenKind::FStrStart(triple)
            }
            b'c' if next == Some(b'\'') => self.lex_char_lit(true),
            b'b' if next == Some(b'"') => self.lex_byte_str_lit(),
            d if d.is_ascii_digit() => self.lex_number(),
            _ if !self.ext_ops.is_empty() && self.peek_ext_op().is_some() => {
                let op = self.peek_ext_op().unwrap_or_default();
                self.advance(op.len());
                TokenKind::ExtOp(op)
            }
            _ => match self.peek_char() {
                Some(c) if is_ident_start(c) || self.at_flag() => self.lex_ident(),
                Some(c) if math_op(c).is_some() => {
                    self.advance(c.len_utf8());
                    math_op(c)?
                }
                _ => {
                    self.skip_invalid();
                    return None;
                }
            },
        };

        let span = Self::make_span(start, self.pos, line, col);
        Some(Token::new(kind, span))
    }

    /// Report and skip a run of characters that cannot start a token
    /// (one diagnostic per run, never recursion).
    fn skip_invalid(&mut self) {
        let start = self.pos;
        let span = self.span_here();
        while let Some(c) = self.peek_char() {
            let ok_start = c.is_ascii() && b"()[]{},;@~+-*/%^=!<>&|.:?'\"".contains(&(c as u8));
            if self.pos > start
                && (ok_start
                    || is_ident_start(c)
                    || math_op(c).is_some()
                    || is_whitespace(c)
                    || c.is_ascii_digit())
            {
                break;
            }
            if self.pos > start && c == '#' {
                break;
            }
            self.advance(c.len_utf8());
        }
        let text: String = self.src[start..self.pos].chars().take(16).collect();
        // `#!` past the first line: a misplaced shebang, so the rest of
        // the line is not code.
        if text == "#" && self.peek_char() == Some('!') {
            while let Some(c) = self.peek_char().filter(|&c| c != '\n') {
                self.advance(c.len_utf8());
            }
        }
        let msg = if text == "#" {
            "stray `#` (expected `#[` or `#![`)".to_owned()
        } else {
            format!("unexpected character(s) `{}`", text.escape_debug())
        };
        self.error(
            Span::new(span.start, self.pos as u32, span.line, span.col),
            msg,
        );
    }

    fn single(&mut self, kind: TokenKind) -> TokenKind {
        self.advance(1);
        kind
    }

    fn lex_minus(&mut self) -> TokenKind {
        match self.bytes.get(self.pos + 1) {
            Some(b'>') => {
                self.advance(2);
                TokenKind::Arrow
            }
            Some(b'=') => {
                self.advance(2);
                TokenKind::MinusEq
            }
            _ => self.single(TokenKind::Minus),
        }
    }

    fn lex_percent(&mut self) -> TokenKind {
        // `%%=` > `%%` > `%=` > `%` (longest match, spec 1.10.2).
        match (self.bytes.get(self.pos + 1), self.bytes.get(self.pos + 2)) {
            (Some(b'%'), Some(b'=')) => {
                self.advance(3);
                TokenKind::PercentPercentEq
            }
            (Some(b'%'), _) => {
                self.advance(2);
                TokenKind::PercentPercent
            }
            (Some(b'='), _) => {
                self.advance(2);
                TokenKind::PercentEq
            }
            _ => self.single(TokenKind::Percent),
        }
    }

    fn lex_eq(&mut self) -> TokenKind {
        match self.bytes.get(self.pos + 1) {
            Some(b'=') => {
                self.advance(2);
                TokenKind::EqEq
            }
            Some(b'>') => {
                self.advance(2);
                TokenKind::FatArrow
            }
            _ => self.single(TokenKind::Eq),
        }
    }

    fn lex_lt(&mut self) -> TokenKind {
        match (self.bytes.get(self.pos + 1), self.bytes.get(self.pos + 2)) {
            (Some(b'='), _) => {
                self.advance(2);
                TokenKind::LtEq
            }
            (Some(b'<'), Some(b'=')) => {
                self.advance(3);
                TokenKind::ShlEq
            }
            (Some(b'<'), _) => {
                self.advance(2);
                TokenKind::Shl
            }
            _ => self.single(TokenKind::Lt),
        }
    }

    fn lex_gt(&mut self) -> TokenKind {
        match (self.bytes.get(self.pos + 1), self.bytes.get(self.pos + 2)) {
            (Some(b'='), _) => {
                self.advance(2);
                TokenKind::GtEq
            }
            (Some(b'>'), Some(b'=')) => {
                self.advance(3);
                TokenKind::ShrEq
            }
            (Some(b'>'), _) => {
                self.advance(2);
                TokenKind::Shr
            }
            _ => self.single(TokenKind::Gt),
        }
    }

    fn lex_amp(&mut self) -> TokenKind {
        match self.bytes.get(self.pos + 1) {
            Some(b'&') => {
                self.advance(2);
                TokenKind::AmpAmp
            }
            Some(b'=') => {
                self.advance(2);
                TokenKind::AmpEq
            }
            _ => self.single(TokenKind::Amp),
        }
    }

    fn lex_pipe(&mut self) -> TokenKind {
        match self.bytes.get(self.pos + 1) {
            Some(b'|') => {
                self.advance(2);
                TokenKind::PipePipe
            }
            Some(b'=') => {
                self.advance(2);
                TokenKind::PipeEq
            }
            _ => self.single(TokenKind::Pipe),
        }
    }

    fn lex_dot(&mut self) -> TokenKind {
        // `..=`, `..`, `.5` float, broadcast operators, plain `.`.
        if self.bytes.get(self.pos + 1) == Some(&b'.') {
            if self.bytes.get(self.pos + 2) == Some(&b'=') {
                self.advance(3);
                return TokenKind::DotDotEq;
            }
            self.advance(2);
            return TokenKind::DotDot;
        }
        // `.5` is always `.` + tuple index; a float literal needs
        // its leading `0`. Where no expression precedes, say so.
        if !self.prev_ends_expr && self.bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit) {
            self.error_from(
                self.pos,
                "a float literal needs a leading digit (`0.5`); `.5` is a tuple index",
            );
        }
        if let Some(kind) = self.try_broadcast() {
            return kind;
        }
        self.single(TokenKind::Dot)
    }

    fn try_broadcast(&mut self) -> Option<TokenKind> {
        // `.+` `.-` `.*` `./` `.%` `.%%` `.&` `.|` `.^` `.<<` `.>>`
        // `.==` `.!=` `.<` `.>` `.<=` `.>=` `.&&` `.||`
        let p = self.pos + 1;
        let b1 = *self.bytes.get(p)?;
        let b2 = self.bytes.get(p + 1).copied();

        let (len, kind) = match (b1, b2) {
            (b'<', Some(b'<')) => (3, TokenKind::DotShl),
            (b'>', Some(b'>')) => (3, TokenKind::DotShr),
            (b'<', Some(b'=')) => (3, TokenKind::DotLtEq),
            (b'>', Some(b'=')) => (3, TokenKind::DotGtEq),
            (b'=', Some(b'=')) => (3, TokenKind::DotEqEq),
            (b'!', Some(b'=')) => (3, TokenKind::DotNotEq),
            (b'%', Some(b'%')) => (3, TokenKind::DotPercentPercent),
            (b'&', Some(b'&')) => (3, TokenKind::DotAmpAmp),
            (b'|', Some(b'|')) => (3, TokenKind::DotPipePipe),
            (b'+', _) => (2, TokenKind::DotPlus),
            (b'-', _) => (2, TokenKind::DotMinus),
            (b'*', _) => (2, TokenKind::DotStar),
            (b'/', _) => (2, TokenKind::DotSlash),
            (b'%', _) => (2, TokenKind::DotPercent),
            (b'&', _) => (2, TokenKind::DotAmp),
            (b'|', _) => (2, TokenKind::DotPipe),
            (b'^', _) => (2, TokenKind::DotCaret),
            (b'<', _) => (2, TokenKind::DotLt),
            (b'>', _) => (2, TokenKind::DotGt),
            _ => return None,
        };
        self.advance(len);
        Some(kind)
    }

    fn lex_colon(&mut self) -> TokenKind {
        self.lex_op2(b':', TokenKind::ColonColon, TokenKind::Colon)
    }

    fn lex_question(&mut self) -> TokenKind {
        match self.bytes.get(self.pos + 1) {
            Some(b'.') => {
                self.advance(2);
                TokenKind::QuestionDot
            }
            Some(b'?') => {
                self.advance(2);
                TokenKind::QuestionQuestion
            }
            _ => self.single(TokenKind::Question),
        }
    }

    fn lex_op2(&mut self, second: u8, two: TokenKind, one: TokenKind) -> TokenKind {
        if self.bytes.get(self.pos + 1) == Some(&second) {
            self.advance(2);
            two
        } else {
            self.single(one)
        }
    }

    fn peek_ext_op(&self) -> Option<String> {
        let rest = &self.src[self.pos..];
        self.ext_ops
            .iter()
            .find(|op| rest.starts_with(op.as_str()))
            .cloned()
    }

    /// A flag - a pair of regional indicators - may start an identifier
    /// (`🇰🇷`); a lone indicator may not.
    fn at_flag(&self) -> bool {
        let ri = |c: char| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c);
        let mut it = self.src[self.pos..].chars();
        it.next().is_some_and(ri) && it.next().is_some_and(ri)
    }

    fn lex_ident(&mut self) -> TokenKind {
        let start = self.pos;
        let (line, col) = (self.line, self.col);
        self.eat_ident_continue();
        let raw = &self.src[start..self.pos];
        // Spec 1.4.2: identifiers compare after NFC + variation-selector
        // removal.
        let name = normalize_ident(raw);
        if let Some(k) = keyword_kind(&name) {
            return k;
        }
        let span = Self::make_span(start, self.pos, line, col);
        if self.seen.insert(name.clone()) {
            if !raw.is_ascii() {
                self.any_non_ascii = true;
                if mixed_script(&name) {
                    self.warning(
                        span,
                        format!("identifier `{name}` mixes Cyrillic with Latin or Greek letters"),
                    );
                }
            }
            self.idents.push((name.clone(), span));
        }
        TokenKind::Ident(name)
    }

    fn eat_ident_continue(&mut self) {
        // Last character that was not an emoji modifier / selector, for
        // the ZWJ rule (a ZWJ only joins two pictographs).
        let mut last_base: Option<char> = None;
        while let Some(c) = self.peek_char() {
            if c == '\u{200D}' {
                let next = self.src[self.pos + c.len_utf8()..].chars().next();
                if !(last_base.is_some_and(is_pictographic) && next.is_some_and(is_pictographic)) {
                    break;
                }
            } else if !is_ident_continue(c) {
                break;
            }
            if !crate::unicode::is_emoji_component(c) {
                last_base = Some(c);
            }
            self.advance(c.len_utf8());
        }
    }

    /// Numeric literal per spec 1.6. Malformed input is reported but
    /// still yields a token so the parser does not cascade.
    fn lex_number(&mut self) -> TokenKind {
        let start = self.pos;
        let mut radix = 10;
        let mut is_float = false;

        let prefix = match (self.bytes[start], self.bytes.get(start + 1)) {
            (b'0', Some(b'x' | b'X')) => Some(16),
            (b'0', Some(b'b' | b'B')) => Some(2),
            (b'0', Some(b'o' | b'O')) => Some(8),
            _ => None,
        };
        if let Some(r) = prefix {
            radix = r;
            self.advance(2);
            self.eat_digit_seq(radix, start);
            let stray = self.bytes.get(self.pos).copied().filter(u8::is_ascii_digit);
            if let Some(d) = stray.filter(|_| radix != 16) {
                self.error_from(
                    start,
                    format!("invalid digit `{}` in base-{radix} literal", d as char),
                );
                while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                    self.advance(1);
                }
            }
        } else {
            self.eat_digit_seq(10, start);
            if self.bytes.get(self.pos) == Some(&b'.') {
                let after = self.src[self.pos + 1..].chars().next();
                if after.is_some_and(|c| c.is_ascii_digit()) {
                    is_float = true;
                    self.advance(1);
                    self.eat_digit_seq(10, start);
                } else if !after.is_some_and(|c| c == '.' || is_ident_start(c)) {
                    // `1.` - `.` not followed by an identifier or `.`.
                    is_float = true;
                    self.advance(1);
                }
            }
        }

        if radix == 10 && matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            is_float = true;
            self.advance(1);
            if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
                self.advance(1);
            }
            if self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                self.eat_digit_seq(10, start);
            } else {
                self.error_from(start, "exponent has no digits");
            }
        }

        let body_end = self.pos;
        self.eat_ident_continue();
        let suffix = &self.src[body_end..self.pos];
        let text = self.src[start..self.pos].to_owned();
        if !suffix.is_empty() {
            if FLOAT_SUFFIXES.contains(&suffix) {
                if radix == 10 {
                    is_float = true;
                } else {
                    self.error_from(
                        start,
                        format!("float suffix `{suffix}` on a base-{radix} literal"),
                    );
                }
            } else if INT_SUFFIXES.contains(&suffix) && is_float {
                self.error_from(
                    start,
                    format!("integer suffix `{suffix}` on a float literal"),
                );
            }
            // Any other suffix (`2i`) belongs to a `#[literal(number "i")]`
            // handler (spec 1.6.2, 6.5); the parser resolves it.
        }
        self.check_range(start, &self.src[start..body_end], radix, is_float, suffix);
        if is_float {
            TokenKind::FloatLit(text)
        } else {
            TokenKind::IntLit(text)
        }
    }

    /// Consume `digit ("_"? digit)*` in `radix`; report empty runs and
    /// misplaced `_` (spec 1.6.1, 1.6.4).
    fn eat_digit_seq(&mut self, radix: u32, lit_start: usize) {
        let start = self.pos;
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|&b| b == b'_' || (b as char).is_digit(radix))
        {
            self.advance(1);
        }
        let run = &self.src[start..self.pos];
        let msg = if run.is_empty() {
            "missing digits in numeric literal"
        } else if run.starts_with('_') {
            "`_` must sit between digits (not after a prefix or `.`)"
        } else if run.ends_with('_') {
            "numeric literal cannot end with `_`"
        } else if run.contains("__") {
            "consecutive `_` in numeric literal"
        } else {
            return;
        };
        self.error_from(lit_start, msg);
    }

    /// A suffix fixes the type, so the value must fit. Unsuffixed literals
    /// are checked by HIR against the slot (`int` / `float` hold anything).
    fn check_range(&mut self, start: usize, body: &str, radix: u32, is_float: bool, suffix: &str) {
        if suffix.is_empty() {
            return;
        }
        let clean: String = body.chars().filter(|&c| c != '_').collect();
        if is_float {
            let inf = match suffix {
                "f32" => clean.parse::<f32>().is_ok_and(f32::is_infinite),
                _ => clean.parse::<f64>().is_ok_and(f64::is_infinite),
            };
            if inf {
                self.error_from(
                    start,
                    format!(
                        "float literal out of range for `{}`",
                        if suffix.is_empty() { "f64" } else { suffix }
                    ),
                );
            }
            return;
        }
        let digits = if radix == 10 { &clean[..] } else { &clean[2..] };
        let Ok(value) = u128::from_str_radix(digits, radix) else {
            if !digits.is_empty() && digits.chars().all(|c| c.is_digit(radix)) {
                self.error_from(start, "integer literal is too large");
            }
            return;
        };
        let (bits, signed) = match suffix {
            "i8" => (8, true),
            "i16" => (16, true),
            "i32" => (32, true),
            "i64" | "isize" => (64, true),
            "i128" => (128, true),
            "u8" => (8, false),
            "u16" => (16, false),
            "u32" => (32, false),
            "u64" | "usize" => (64, false),
            _ => return,
        };
        // Only a unary `-` directly before the literal admits
        // `MAX + 1` (`-128i8`); in `a - 128i8` the literal is out of range.
        let max = if signed {
            (1u128 << (bits - 1)) - u128::from(!self.prev_minus)
        } else if bits == 128 {
            u128::MAX
        } else {
            (1u128 << bits) - 1
        };
        if value > max {
            self.error_from(
                start,
                format!("literal `{body}` out of range for `{suffix}` (max {max})"),
            );
        }
    }

    /// `'` starts a char literal (`'X'`, `'\n'`) or a label (`'name`).
    fn lex_quote_or_label(&mut self) -> TokenKind {
        let mut rest = self.src[self.pos + 1..].chars();
        let c1 = rest.next().unwrap_or('\'');
        if is_ident_start(c1) && rest.next() != Some('\'') {
            // Label unless the identifier run is closed by `'` (then it
            // is an over-long char literal, reported as such).
            let save = (self.pos, self.line, self.col);
            self.advance(1);
            let start = self.pos;
            self.eat_ident_continue();
            if self.bytes.get(self.pos) != Some(&b'\'') {
                return TokenKind::Label(normalize_ident(&self.src[start..self.pos]));
            }
            (self.pos, self.line, self.col) = save;
        }
        self.lex_char_lit(false)
    }

    fn lex_char_lit(&mut self, c_char: bool) -> TokenKind {
        let lit_start = self.pos;
        self.advance(usize::from(c_char) + 1); // optional `c`, then `'`
        let start = self.pos;
        let Some(end) = self.scan_quoted(b'\'') else {
            self.error_from(lit_start, "unterminated char literal");
            return TokenKind::CharLit(String::new());
        };
        let body = self.src[start..end].to_owned();
        let mode = if c_char {
            EscapeMode::CChar
        } else {
            EscapeMode::Char
        };
        match unescape(&body, mode) {
            Err((_, msg)) => self.error_from(lit_start, msg),
            Ok(v) => {
                let mut chars = v.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => {
                        if c_char && (c as u32 > 0xFF || (!c.is_ascii() && !body.starts_with('\\')))
                        {
                            self.error_from(
                                lit_start,
                                format!(
                                    "`c'{body}'` must be one byte; write non-ASCII bytes as `\\xHH`"
                                ),
                            );
                        }
                    }
                    _ => self.error_from(
                        lit_start,
                        "char literal must contain exactly one code point",
                    ),
                }
            }
        }
        if c_char {
            TokenKind::CCharLit(body)
        } else {
            TokenKind::CharLit(body)
        }
    }

    /// Skip to past the closing `quote`; the body's end, or `None` at EOF
    /// (for chars, at a line end).
    fn scan_quoted(&mut self, quote: u8) -> Option<usize> {
        loop {
            match self.bytes.get(self.pos) {
                None => return None,
                Some(&b) if b == quote => {
                    let end = self.pos;
                    self.advance(1);
                    return Some(end);
                }
                Some(b'\n' | b'\r') if quote == b'\'' => return None,
                Some(b'\\') => {
                    self.advance(1);
                    if let Some(c) = self.peek_char() {
                        self.advance(c.len_utf8());
                    }
                }
                Some(_) => {
                    let n = self.peek_char().map_or(1, char::len_utf8);
                    self.advance(n);
                }
            }
        }
    }

    fn lex_str_lit(&mut self) -> TokenKind {
        let lit_start = self.pos;
        self.advance(1);
        let start = self.pos;
        let Some(end) = self.scan_quoted(b'"') else {
            self.error_from(lit_start, "unterminated string literal");
            return TokenKind::StrLit(String::new());
        };
        let body = self.src[start..end].to_owned();
        if let Err((_, msg)) = unescape(&body, EscapeMode::Str) {
            self.error_from(lit_start, msg);
        }
        TokenKind::StrLit(body)
    }

    /// `b"..."`: the escapes of `c'x'` (any value up to `\xFF`), and
    /// only ASCII written directly.
    fn lex_byte_str_lit(&mut self) -> TokenKind {
        let lit_start = self.pos;
        self.advance(2);
        let start = self.pos;
        let Some(end) = self.scan_quoted(b'"') else {
            self.error_from(lit_start, "unterminated byte string literal");
            return TokenKind::ByteStrLit(String::new());
        };
        let body = self.src[start..end].to_owned();
        match unescape(&body, EscapeMode::CChar) {
            Err((_, msg)) => self.error_from(lit_start, msg),
            Ok(_) if !body.is_ascii() => self.error_from(
                lit_start,
                "a byte string holds ASCII only; write other bytes as `\\xHH`",
            ),
            Ok(v) if v.chars().any(|c| c as u32 > 0xFF) => {
                self.error_from(lit_start, "a byte string escape must be at most `\\xFF`");
            }
            Ok(_) => {}
        }
        TokenKind::ByteStrLit(body)
    }

    /// Inside a hole (spec 1.7.3): `}` / `:` at depth 0 end or split it;
    /// `None` lexes `b` as code.
    fn hole_punct(&mut self, b: u8, next: Option<u8>) -> Option<TokenKind> {
        let Some(Frame::Hole { triple, depth }) = self.fstr.last_mut() else {
            return None;
        };
        let triple = *triple;
        match b {
            b'}' if *depth == 0 => {
                self.fstr.pop();
                self.advance(1);
                Some(TokenKind::HoleEnd)
            }
            b':' if *depth == 0 && next != Some(b':') => {
                *self.fstr.last_mut()? = Frame::Spec { triple };
                self.advance(1);
                Some(TokenKind::HoleSpec)
            }
            b'(' | b'[' | b'{' => {
                *depth += 1;
                None
            }
            b')' | b']' | b'}' => {
                *depth = depth.saturating_sub(1);
                None
            }
            _ => None,
        }
    }

    /// The next f-string text run, spec run, `{` / `${` (hole start),
    /// spec-closing `}` or closing quote. `None` after a reported stray `}`.
    fn fstr_piece(&mut self) -> Option<Token> {
        let (start, line, col) = (self.pos, self.line, self.col);
        let (triple, spec) = match *self.fstr.last()? {
            Frame::Text { triple, .. } => (triple, false),
            Frame::Spec { triple } => (triple, true),
            Frame::Hole { .. } => return None,
        };
        let closes = |s: &Self| !triple || s.src[s.pos..].starts_with("\"\"\"");
        let mut text = String::new();
        while let Some(&b) = self.bytes.get(self.pos) {
            let next = self.bytes.get(self.pos + 1).copied();
            match b {
                b'"' if spec || closes(self) => break,
                b'{' | b'}' if !spec && next == Some(b) => {
                    text.push(char::from(b));
                    self.advance(2);
                }
                b'}' if !spec => {
                    let at = self.pos;
                    self.advance(1);
                    self.error_from(at, "single `}` in f-string (write `}}`)");
                }
                b'$' if spec && next == Some(b'$') => {
                    text.push('$');
                    self.advance(2);
                }
                b'{' | b'}' => break,
                b'$' if spec && next == Some(b'{') => break,
                _ => {
                    let n = usize::from(b == b'\\' && !spec);
                    let end = self.src[self.pos + n..]
                        .chars()
                        .next()
                        .map_or(self.pos + n, |c| self.pos + n + c.len_utf8());
                    text.push_str(&self.src[self.pos..end]);
                    self.advance(end - self.pos);
                }
            }
        }
        let kind = if !text.is_empty() {
            if spec {
                TokenKind::SpecText(text)
            } else {
                TokenKind::FStrText(text)
            }
        } else if self.pos > start || self.pos >= self.bytes.len() {
            return None;
        } else {
            match self.bytes[self.pos] {
                b'"' if spec => {
                    self.error_from(start, "unclosed `{` in f-string (missing `}`)");
                    self.fstr.pop();
                    TokenKind::HoleEnd
                }
                b'"' => {
                    self.advance(if triple { 3 } else { 1 });
                    self.fstr.pop();
                    TokenKind::FStrEnd(triple)
                }
                b'}' => {
                    self.advance(1);
                    self.fstr.pop();
                    TokenKind::HoleEnd
                }
                b => {
                    self.advance(if b == b'$' { 2 } else { 1 });
                    self.fstr.push(Frame::Hole { triple, depth: 0 });
                    TokenKind::HoleStart
                }
            }
        };
        Some(Token::new(
            kind,
            Self::make_span(start, self.pos, line, col),
        ))
    }

    /// Inside the code of a hole of a one-line `f"..."`, at any depth.
    fn in_flat_hole(&self) -> bool {
        self.fstr.iter().any(|f| {
            matches!(
                f,
                Frame::Hole { triple: false, .. } | Frame::Spec { triple: false }
            )
        })
    }

    /// One hole error per f-string literal: the rest is likely fallout.
    fn hole_error(&mut self, at: usize, msg: &str) {
        let outer = match self.fstr.first() {
            Some(&Frame::Text { start, .. }) => Some(start),
            _ => None,
        };
        if outer.is_none() || outer != self.hole_reported {
            self.hole_reported = outer;
            let (line, col) = self.line_col(at);
            let end = at + self.src[at..].chars().next().map_or(0, char::len_utf8);
            self.error(Self::make_span(at, end, line, col), msg);
        }
    }

    /// At EOF: report an unterminated f-string and close its open
    /// holes and literals so the parser sees balanced tokens.
    fn close_fstrings(&mut self, tokens: &mut Vec<Token>) {
        if let Some(&Frame::Text { start, .. }) = self.fstr.first() {
            self.error_from(start, "unterminated f-string literal");
        }
        while let Some(f) = self.fstr.pop() {
            let kind = match f {
                Frame::Text { triple, .. } => TokenKind::FStrEnd(triple),
                Frame::Hole { .. } | Frame::Spec { .. } => TokenKind::HoleEnd,
            };
            tokens.push(Token::new(kind, self.span_here()));
        }
    }

    fn lex_raw_str_lit(&mut self) -> TokenKind {
        // r"..." or r#"..."# / r##"..."##.
        let lit_start = self.pos;
        self.advance(1); // `r`
        let mut hash_count = 0usize;
        while self.bytes.get(self.pos) == Some(&b'#') {
            hash_count += 1;
            self.advance(1);
        }
        if self.bytes.get(self.pos) != Some(&b'"') {
            self.error_from(lit_start, "expected `\"` after raw-string prefix");
            return TokenKind::RawStrLit(String::new());
        }
        self.advance(1);
        let start = self.pos;
        let closing = format!("\"{}", "#".repeat(hash_count));
        let Some(len) = self.src[start..].find(&closing) else {
            self.advance(self.bytes.len() - self.pos);
            self.error_from(lit_start, "unterminated raw string");
            return TokenKind::RawStrLit(String::new());
        };
        self.advance(len + closing.len());
        TokenKind::RawStrLit(self.src[start..start + len].to_owned())
    }

    fn skip_trivia(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            match b {
                b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C => self.advance(1),
                b'/' if self.bytes.get(self.pos + 1) == Some(&b'/') => {
                    let len = self.src[self.pos..]
                        .find(['\n', '\r'])
                        .unwrap_or(self.bytes.len() - self.pos);
                    let line = &self.src[self.pos..self.pos + len];
                    // `////` is a plain comment, as in Rust.
                    let doc = match line.get(..3) {
                        Some("///") if !line.starts_with("////") => Some(false),
                        Some("//!") => Some(true),
                        _ => None,
                    };
                    if let Some(inner) = doc {
                        let span = Self::make_span(self.pos, self.pos + len, self.line, self.col);
                        let text = &line[3..];
                        self.docs.push(DocLine {
                            inner,
                            text: text.strip_prefix(' ').unwrap_or(text).to_owned(),
                            span,
                        });
                    }
                    self.advance(len);
                }
                b'/' if self.bytes.get(self.pos + 1) == Some(&b'*') => self.skip_block_comment(),
                _ => break,
            }
        }
    }

    /// A (nested) block comment; `/** .. */` and `/*! .. */` are doc
    /// comments, but `/**/`, `/***/` and `/*** ..` are not (as in Rust).
    fn skip_block_comment(&mut self) {
        let start = self.pos;
        let (line, col) = (self.line, self.col);
        let doc = match (self.bytes.get(start + 2), self.bytes.get(start + 3)) {
            (Some(b'!'), _) => Some(true),
            (Some(b'*'), Some(c)) if !matches!(c, b'*' | b'/') => Some(false),
            _ => None,
        };
        self.advance(2);
        let mut depth = 1usize;
        while depth > 0 {
            match (self.bytes.get(self.pos), self.bytes.get(self.pos + 1)) {
                (None, _) => {
                    self.error_from(start, "unterminated block comment");
                    return;
                }
                (Some(b'/'), Some(b'*')) => {
                    self.advance(2);
                    depth += 1;
                }
                (Some(b'*'), Some(b'/')) => {
                    self.advance(2);
                    depth -= 1;
                }
                _ => self.advance(1),
            }
        }
        if let Some(inner) = doc {
            self.docs.push(DocLine {
                inner,
                text: block_doc_text(&self.src[start + 3..self.pos - 2]),
                span: Self::make_span(start, self.pos, line, col),
            });
        }
    }

    /// Move forward `n` bytes, tracking line (LF, CRLF, CR) and
    /// code-point column.
    fn advance(&mut self, n: usize) {
        let end = (self.pos + n).min(self.bytes.len());
        for i in self.pos..end {
            match self.bytes[i] {
                b'\n' if i > 0 && self.bytes[i - 1] == b'\r' => {}
                b'\n' | b'\r' => {
                    self.line += 1;
                    self.col = 1;
                }
                b if b & 0xC0 == 0x80 => {} // UTF-8 continuation byte
                _ => self.col += 1,
            }
        }
        self.pos = end;
    }

    fn peek_char(&self) -> Option<char> {
        self.src.get(self.pos..)?.chars().next()
    }

    fn span_here(&self) -> Span {
        let start = u32::try_from(self.pos).unwrap_or(u32::MAX);
        Span::new(start, start, self.line, self.col)
    }

    fn make_span(start: usize, end: usize, line: u32, col: u32) -> Span {
        Span::new(
            u32::try_from(start).unwrap_or(u32::MAX),
            u32::try_from(end).unwrap_or(u32::MAX),
            line,
            col,
        )
    }

    /// Error spanning `start..pos`, positioned at `start`'s line/column.
    fn error_from(&mut self, start: usize, msg: impl Into<String>) {
        let (line, col) = self.line_col(start);
        let span = Self::make_span(start, self.pos, line, col);
        self.error(span, msg);
    }

    fn line_col(&self, at: usize) -> (u32, u32) {
        let before = &self.src[..at];
        let line_start = before.rfind(['\n', '\r']).map_or(0, |i| i + 1);
        let line = 1 + before.matches('\n').count() + before.matches('\r').count()
            - before.matches("\r\n").count();
        let col = 1 + before[line_start..].chars().count();
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(col).unwrap_or(u32::MAX),
        )
    }

    fn warning(&mut self, span: Span, msg: impl Into<String>) {
        self.diagnostics.push(
            RokDiagnostic::warning(ErrorKind::Lexical, span, msg).with_file(self.file.clone()),
        );
    }

    fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.diagnostics
            .push(RokDiagnostic::error(ErrorKind::Lexical, span, msg).with_file(self.file.clone()));
    }
}

/// Text of a block doc comment (spec 1.2.3): drop a blank first and last
/// line, a common `*` prefix, common indentation and the whitespace
/// before `*/`; line breaks become LF.
fn block_doc_text(body: &str) -> String {
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<&str> = body.split('\n').collect();
    if lines.len() > 1 && lines[0].trim().is_empty() {
        lines.remove(0);
    }
    if lines.len() > 1 && lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let starred = |l: &&str| l.trim_start().starts_with('*');
    let from = usize::from(!lines.first().is_some_and(starred));
    if body.contains('\n') && lines.len() > from && lines[from..].iter().all(starred) {
        for l in &mut lines[from..] {
            *l = &l.trim_start()[1..];
        }
    }
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    let text: Vec<&str> = lines
        .iter()
        .map(|l| l.get(indent..).unwrap_or(""))
        .collect();
    text.join("\n").trim_end().to_owned()
}

/// Spec 1.10.3 Unicode math operators. `÷ ≡ ≠ ≤ ≥` are synonyms of
/// `/ == != <= >=` and lex as those tokens.
const fn math_op(c: char) -> Option<TokenKind> {
    Some(match c {
        '÷' => TokenKind::Slash,
        '≡' => TokenKind::EqEq,
        '≠' => TokenKind::NotEq,
        '≤' => TokenKind::LtEq,
        '≥' => TokenKind::GtEq,
        '×' | '⋅' | '∘' | '√' | '∈' | '∉' | '⊂' | '⊆' | '∪' | '∩' => {
            TokenKind::MathOp(c)
        }
        _ => return None,
    })
}

const fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0B' | '\x0C')
}

#[cfg(test)]
mod tests {
    use super::Lexer;
    use crate::TokenKind as T;

    fn lex(src: &str) -> (Vec<T>, Vec<String>) {
        let (toks, diags) = Lexer::new(src, "t").lex_all();
        (
            toks.into_iter().map(|t| t.kind).collect(),
            diags.into_iter().map(|d| d.message).collect(),
        )
    }

    fn ok(src: &str) -> Vec<T> {
        let (t, d) = lex(src);
        assert!(d.is_empty(), "{src:?}: {d:?}");
        t
    }

    fn err(src: &str) {
        let (_, d) = lex(src);
        assert!(!d.is_empty(), "{src:?} should be rejected");
    }

    /// `(inner, text)` of the doc comments before the first token.
    fn docs(src: &str) -> Vec<(bool, String)> {
        let (toks, diags) = Lexer::new(src, "t").lex_all();
        assert!(diags.is_empty(), "{src:?}: {diags:?}");
        toks[0]
            .docs
            .iter()
            .map(|d| (d.inner, d.text.clone()))
            .collect()
    }

    #[test]
    fn shebang_line() {
        assert_eq!(ok("#!/usr/bin/env rok\nx"), [T::Ident("x".into()), T::Eof]);
        assert_eq!(ok("\u{FEFF}#!rok -q\r\nx"), [T::Ident("x".into()), T::Eof]);
        assert_eq!(ok("#!rok"), [T::Eof]);
        assert_eq!(ok("#![a]")[0], T::HashBangLBracket);
        // Only the first line.
        err("x\n#!/usr/bin/env rok\n");
        let (toks, _) = Lexer::new("#!/bin/rok\nx", "t").lex_all();
        assert_eq!((toks[0].span.line, toks[0].span.col), (2, 1));
    }

    #[test]
    fn block_doc_comments() {
        let d = |s: &str| (false, s.to_owned());
        assert_eq!(docs("/** One. */x"), [d("One.")]);
        assert_eq!(docs("/*! Mod. */x"), [(true, "Mod.".to_owned())]);
        assert_eq!(
            docs("/**\n * A.\n *\n *     code\n */x"),
            [d("A.\n\n    code")]
        );
        assert_eq!(docs("/**\r\n    A.\r\n      B.\r\n*/x"), [d("A.\n  B.")]);
        assert_eq!(docs("/** a /* b */ c */x"), [d("a /* b */ c")]);
        assert_eq!(docs("/**/x"), []);
        assert_eq!(docs("/***/x"), []);
        assert_eq!(docs("/*** no */x"), []);
        assert_eq!(docs("/**/ /*!*/x"), [(true, String::new())]);
        assert_eq!(docs("//// no\n/// yes\nx"), [d("yes")]);
        assert_eq!(docs("r\"/// no\"")[..], []);
        err("/** open");
    }

    #[test]
    fn numeric_literals() {
        for good in [
            "1_000",
            "0xFF_FF",
            "0b1010_0101",
            "3.141_592",
            "1.",
            "2e10",
            "1.5e-3f32",
            "0xFFu8",
            "1f32",
            "-128i8",
            "255u8",
            "0123",
            "2i", // a `#[literal]` suffix: the parser resolves it
        ] {
            ok(good);
        }
        for bad in [
            "0x", "0b", "0o", "1__0", "1_", "0x_FF", "1e", "0b102", "0o8", "1.5i32", "300i8",
            "256u8", "128i8", "1e999f32", "0b1f32",
        ] {
            err(bad);
        }
        assert_eq!(ok("1f32")[0], T::FloatLit("1f32".into()));
        assert_eq!(
            ok("1.foo"),
            vec![
                T::IntLit("1".into()),
                T::Dot,
                T::Ident("foo".into()),
                T::Eof
            ]
        );
        assert_eq!(
            ok("1..2"),
            vec![
                T::IntLit("1".into()),
                T::DotDot,
                T::IntLit("2".into()),
                T::Eof
            ]
        );
        // `.5` after an expression is `.` + tuple index.
        assert_eq!(
            ok("t.5"),
            vec![T::Ident("t".into()), T::Dot, T::IntLit("5".into()), T::Eof]
        );
        err("x = .5");
        err("5 - 128i8");
        err("0xFFi8");
        ok("x = -128i8");
    }

    #[test]
    fn strings_and_chars() {
        ok(r##""a\n\u{AC00}\x41" 'x' '\'' '\u{1F600}' c'\xFF' r"\q" r#"a"b"# '세'"##);
        for bad in [
            r#""\q""#,
            r#""\x80""#,
            r#""\u{110000}""#,
            "'ab'",
            "''",
            "c'세'",
            "c'é'",
            "\"abc",
            "'\\n",
        ] {
            err(bad);
        }
        assert_eq!(ok("\"a\\\n   b\"")[0], T::StrLit("a\\\n   b".into()));
    }

    #[test]
    fn byte_strings() {
        assert_eq!(
            ok(r#"b"a\xFF\n" b"" b"#),
            [
                T::ByteStrLit(r"a\xFF\n".into()),
                T::ByteStrLit(String::new()),
                T::Ident("b".into()),
                T::Eof
            ]
        );
        for bad in [r#"b"é""#, r#"b"\u{100}""#, r#"b"\q""#, "b\"ab"] {
            err(bad);
        }
    }

    #[test]
    fn fstrings() {
        let id = |s: &str| T::Ident(s.into());
        let text = |s: &str| T::FStrText(s.into());
        assert_eq!(
            ok(r#"f"a {x} {{b}}""#),
            [
                T::FStrStart(false),
                text("a "),
                T::HoleStart,
                id("x"),
                T::HoleEnd,
                text(" {b}"),
                T::FStrEnd(false),
                T::Eof
            ]
        );
        // Hole code is ordinary tokens: the enclosing quote is reused.
        assert_eq!(
            ok(r#"f"{a + "}"}""#)[2..5],
            [id("a"), T::Plus, T::StrLit("}".into())]
        );
        assert_eq!(
            ok(r#"f"{d["k"]}""#)[2..6],
            [id("d"), T::LBracket, T::StrLit("k".into()), T::RBracket]
        );
        assert_eq!(ok(r#"f"{"a\tb"}""#)[2], T::StrLit("a\\tb".into()));
        // Specs: text runs (`$$` folded), `${..}` / `{..}` nested holes,
        // `::` stays a path separator.
        assert_eq!(
            ok(r#"f"{f"{a::b:>{w}.${p}$$}"}""#),
            [
                T::FStrStart(false),
                T::HoleStart,
                T::FStrStart(false),
                T::HoleStart,
                id("a"),
                T::ColonColon,
                id("b"),
                T::HoleSpec,
                T::SpecText(">".into()),
                T::HoleStart,
                id("w"),
                T::HoleEnd,
                T::SpecText(".".into()),
                T::HoleStart,
                id("p"),
                T::HoleEnd,
                T::SpecText("$".into()),
                T::HoleEnd,
                T::FStrEnd(false),
                T::HoleEnd,
                T::FStrEnd(false),
                T::Eof
            ]
        );
        // `f"""`: raw `"` in text, hole code across lines.
        assert_eq!(
            ok("f\"\"\"say \"{\n  x\n}\" \"\"\"")[..5],
            [
                T::FStrStart(true),
                text("say \""),
                T::HoleStart,
                id("x"),
                T::HoleEnd
            ]
        );
        assert_eq!(ok("f\"a\nb\"")[1], text("a\nb"));
        for bad in [
            "f\"a } b\"",
            "f\"{x\n}\"",
            "f\"{f\"\n\"}\"",
            "f\"{x",
            "f\"{x:5\"",
            "f\"{(}\"",
        ] {
            err(bad);
        }
        // A backslash in hole code: one error at the first one.
        let (toks, d) = Lexer::new("\n  f\"{n ?? \\\"a\\\"}\"", "t").lex_all();
        assert_eq!(toks[toks.len() - 2].kind, T::FStrEnd(false));
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].message.contains("write `\"` directly"), "{d:?}");
        let at = d[0].primary.as_ref().unwrap().span;
        assert_eq!((at.line, at.col), (2, 11));
    }

    #[test]
    fn labels_and_idents() {
        assert_eq!(ok("'바깥: loop")[0], T::Label("바깥".into()));
        assert_eq!(ok("'a'")[0], T::CharLit("a".into()));
        for id in ["α", "Δt", "σ²", "변수1", "データ", "_internal", "e\u{301}"] {
            assert_eq!(ok(id), vec![T::Ident(crate::unicode::nfc(id)), T::Eof]);
        }
        assert_eq!(ok("unsafe using")[..2], [T::Unsafe, T::Using]);
        assert_eq!(ok("a %% b %%= c .%% d .&& e .|| f")[1], T::PercentPercent);
    }

    #[test]
    fn positions_bom_whitespace_and_garbage() {
        let (toks, _) = Lexer::new("\u{FEFF}가나 x\ry\r\nz\x0B\x0Cw", "t").lex_all();
        let pos: Vec<_> = toks.iter().map(|t| (t.span.line, t.span.col)).collect();
        assert_eq!(pos[..5], [(1, 1), (1, 4), (2, 1), (3, 1), (3, 4)]);
        // A long run of invalid bytes: one diagnostic, no recursion.
        let (toks, d) = lex(&format!("a {} b", "$".repeat(200_000)));
        assert_eq!(d.len(), 1);
        assert_eq!(
            toks,
            vec![T::Ident("a".into()), T::Ident("b".into()), T::Eof]
        );
        err("/* unterminated");
        err("a # b");
    }

    #[test]
    fn token_text_lexes_back() {
        let src = r##"#[derive(X)] fn f<T>(x: ...T) -> i32 throws { 'l: loop { a .+= b; } }
            let s = "q\"\n" + r#"a"b"# + 'c' + c'\x01';
            x %%= 1e-3f32 ?? y?.z .<= 0xFFu8 << 2 ∈ ws; @x; #![a] .. ..= => ::"##;
        let toks = ok(src);
        let back: Vec<String> = toks.iter().map(T::text).collect();
        assert_eq!(ok(&back.join(" ")), toks);
        // F-string pieces carry their text exactly (no spaces between).
        let toks = ok(r#"f"""{{a}} "{f"{x:>{w}$$}"}" """"#);
        let back: String = toks.iter().map(T::text).collect();
        assert_eq!(ok(&back), toks);
    }
}
