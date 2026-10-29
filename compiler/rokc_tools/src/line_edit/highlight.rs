//! Input line colours from `rokc_lexer` tokens, in the classes of the
//! docs and playground highlighter (`rok-syntax`): keyword, type, string,
//! number, comment, attribute.

use rokc_lexer::TokenKind as T;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Keyword,
    Type,
    Str,
    Number,
    Comment,
    Attribute,
}

impl Class {
    /// The ANSI SGR colour: terminal palette entries, so themes apply.
    pub const fn sgr(self) -> &'static str {
        match self {
            Self::Keyword => "\x1b[35m",
            Self::Type => "\x1b[36m",
            Self::Str => "\x1b[32m",
            Self::Number => "\x1b[33m",
            Self::Comment => "\x1b[90m",
            Self::Attribute => "\x1b[95m",
        }
    }
}

/// Keywords in place only; identifiers to the lexer.
pub const CONTEXTUAL: &[&str] = &[
    "type", "any", "in", "where", "throws", "rethrows", "catch", "finally", "const", "static",
    "extern", "mut", "union", "pkg",
];

const PRIMITIVES: &[&str] = &[
    "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize", "f32",
    "f64", "bool", "char", "str", "int", "float",
];

/// The coloured byte ranges of `src`, in order and not overlapping.
pub fn spans(src: &str) -> Vec<(Range<usize>, Class)> {
    let (tokens, _) = rokc_lexer::Lexer::new(src, "<repl>").lex_all();
    let mut out: Vec<(Range<usize>, Class)> = Vec::new();
    let mut push = |r: Range<usize>, c: Class| {
        if r.is_empty() {
            return;
        }
        // Adjacent tokens of one class join (an f-string's parts).
        let from = r.start;
        let joins = out.last().is_some_and(|(p, pc)| *pc == c && p.end == from);
        match out.last_mut() {
            Some(last) if joins => last.0.end = r.end,
            _ => out.push((r, c)),
        }
    };
    let mut at = 0;
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        let (start, end) = (t.span.start as usize, t.span.end as usize);
        trivia(src, at..start.max(at), &mut push);
        let class = match &t.kind {
            T::HashLBracket | T::HashBangLBracket => {
                // The whole attribute, to its closing bracket.
                let mut depth = 0;
                let mut last = end;
                while i < tokens.len() {
                    let k = &tokens[i];
                    if k.kind == T::Eof {
                        break;
                    }
                    last = k.span.end as usize;
                    match k.kind {
                        T::HashLBracket | T::HashBangLBracket | T::LBracket => depth += 1,
                        T::RBracket => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                push(start..last, Class::Attribute);
                at = last;
                continue;
            }
            T::IntLit(_) | T::FloatLit(_) => Some(Class::Number),
            T::StrLit(_)
            | T::RawStrLit(_)
            | T::ByteStrLit(_)
            | T::CharLit(_)
            | T::CCharLit(_)
            | T::FStrStart(_)
            | T::FStrText(_)
            | T::FStrEnd(_)
            | T::HoleStart
            | T::HoleSpec
            | T::SpecText(_)
            | T::HoleEnd => Some(Class::Str),
            T::Ident(w) if CONTEXTUAL.contains(&w.as_str()) => Some(Class::Keyword),
            T::Ident(w)
                if PRIMITIVES.contains(&w.as_str()) || w.starts_with(char::is_uppercase) =>
            {
                Some(Class::Type)
            }
            k if rokc_lexer::token::keyword_str(k).is_some() => Some(Class::Keyword),
            _ => None,
        };
        if let Some(c) = class {
            push(start..end, c);
        }
        at = at.max(end);
        i += 1;
    }
    trivia(src, at..src.len(), &mut push);
    out
}

/// Comments in the text between tokens; an opening quote there starts a
/// string the lexer gave up on.
fn trivia(src: &str, r: Range<usize>, push: &mut impl FnMut(Range<usize>, Class)) {
    let gap = &src[r.clone()];
    let mut i = 0;
    while i < gap.len() {
        let rest = &gap[i..];
        if rest.starts_with("//") {
            let end = rest.find('\n').map_or(gap.len(), |n| i + n);
            push(r.start + i..r.start + end, Class::Comment);
            i = end;
        } else if rest.starts_with("/*") {
            let mut depth = 0;
            let mut k = i;
            while k < gap.len() {
                if gap[k..].starts_with("/*") {
                    depth += 1;
                    k += 2;
                } else if gap[k..].starts_with("*/") {
                    depth -= 1;
                    k += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    k += gap[k..].chars().next().map_or(1, char::len_utf8);
                }
            }
            push(r.start + i..r.start + k, Class::Comment);
            i = k;
        } else if rest.starts_with('"') {
            push(r.start + i..r.end, Class::Str);
            return;
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(src: &str) -> Vec<(&str, Class)> {
        spans(src).into_iter().map(|(r, c)| (&src[r], c)).collect()
    }

    #[test]
    fn classes_follow_the_docs() {
        use Class::*;
        assert_eq!(
            classes("let x: List<i32> = [1, 2]; // two"),
            [
                ("let", Keyword),
                ("List", Type),
                ("i32", Type),
                ("1", Number),
                ("2", Number),
                ("// two", Comment),
            ]
        );
        assert_eq!(
            classes("#[derive(Debug)] struct P { a: f64 }"),
            [
                ("#[derive(Debug)]", Attribute),
                ("struct", Keyword),
                ("P", Type),
                ("f64", Type),
            ]
        );
        assert_eq!(
            classes("fn f() throws { println(f\"a{x + 1:>4}b\") }"),
            [
                ("fn", Keyword),
                ("throws", Keyword),
                ("f\"a{", Str),
                ("1", Number),
                (":>4}b\"", Str),
            ]
        );
        assert_eq!(
            classes("'c' /* a /* b */ c */ 2.5"),
            [
                ("'c'", Str),
                ("/* a /* b */ c */", Comment),
                ("2.5", Number)
            ]
        );
    }

    #[test]
    fn open_input_still_colours() {
        use Class::*;
        assert_eq!(classes("let s = \"abc"), [("let", Keyword), ("\"abc", Str)]);
        assert_eq!(classes("/* open"), [("/* open", Comment)]);
        assert_eq!(classes("#[test"), [("#[test", Attribute)]);
        assert_eq!(classes("x.한글 + 1"), [("1", Number)]);
    }
}
