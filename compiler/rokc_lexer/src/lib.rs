//! Lexer: source text to `Vec<Token>`.

pub mod lexer;
pub mod token;
pub mod unicode;
#[rustfmt::skip]
mod unicode_tables;

pub use lexer::Lexer;
pub use token::{DocLine, Origin, Token, TokenKind};
