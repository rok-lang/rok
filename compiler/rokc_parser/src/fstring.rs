//! `f"..."` (refs-lang 1.7.3) from the lexer's f-string tokens: text
//! runs and holes (`ExprKind::FString`). Hole code is ordinary tokens;
//! HIR reads the spec and formats by the hole's type (stdlib 2.10).

use rokc_ast::{Expr, ExprKind, FStringPart, SpecPiece};
use rokc_diagnostic::{ErrorKind, RokDiagnostic};
use rokc_lexer::TokenKind;

use super::Parser;

impl Parser<'_> {
    /// From `FStrStart` (current token) through `FStrEnd`.
    pub(super) fn parse_fstring(&mut self) -> ExprKind {
        self.bump();
        let mut parts = Vec::new();
        loop {
            match self.peek().clone() {
                TokenKind::FStrText(t) => {
                    self.bump();
                    parts.push(FStringPart::Lit(t));
                }
                TokenKind::HoleStart => {
                    if let Some((expr, spec)) = self.parse_hole() {
                        parts.push(FStringPart::Hole { expr, spec });
                    }
                }
                TokenKind::FStrEnd(_) => {
                    self.bump();
                    break;
                }
                // The lexer closes every f-string, even at EOF.
                _ => break,
            }
        }
        ExprKind::FString(parts)
    }

    /// `HoleStart expr (HoleSpec (SpecText | hole)*)? HoleEnd`; on an
    /// error, skips to the hole's end.
    fn parse_hole(&mut self) -> Option<(Expr, Vec<SpecPiece>)> {
        let span = self.peek_span();
        self.bump();
        if self.eat(&TokenKind::HoleEnd) {
            self.error(span, "empty `{}` in f-string");
            return None;
        }
        let expr = self.with_struct_expr(true, Self::parse_expr);
        let mut spec = Vec::new();
        if expr.is_some() && self.eat(&TokenKind::HoleSpec) {
            loop {
                match self.peek().clone() {
                    TokenKind::SpecText(t) => {
                        self.bump();
                        spec.push(SpecPiece::Text(t));
                    }
                    TokenKind::HoleStart => {
                        let at = self.peek_span();
                        match self.parse_hole() {
                            Some((e, s)) if s.is_empty() => spec.push(SpecPiece::Expr(e)),
                            Some(_) => self.error(at, "a format spec can't have its own spec"),
                            None => {}
                        }
                    }
                    _ => break,
                }
            }
        }
        if self.eat(&TokenKind::HoleEnd) {
            return Some((expr?, spec));
        }
        if expr.is_some() {
            let at = self.peek_span();
            let d = RokDiagnostic::error(
                ErrorKind::Parse,
                at,
                "unexpected tokens in f-string hole (expected `}`)",
            )
            .with_file(self.file.clone());
            self.diagnostics.push(match self.bang_help() {
                Some(help) => d.with_help(help).with_fix(at, "", "delete `!`"),
                None => d,
            });
        }
        let mut depth = 0u32;
        loop {
            match self.peek() {
                TokenKind::HoleStart => depth += 1,
                TokenKind::HoleEnd if depth == 0 => {
                    self.bump();
                    return None;
                }
                TokenKind::HoleEnd => depth -= 1,
                TokenKind::Eof => return None,
                _ => {}
            }
            self.bump();
        }
    }
}
