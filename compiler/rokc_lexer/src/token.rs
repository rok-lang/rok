//! Token definitions.

use rokc_diagnostic::Span;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// The `///` / `//!` lines in the trivia before this token; the
    /// parser turns them into `#[doc]` / `#![doc]` attributes.
    pub docs: Vec<DocLine>,
    /// Written `r#name` (spec 1.4.4): never a keyword, contextual or
    /// reserved.
    pub raw: bool,
}

impl Token {
    #[must_use]
    pub const fn new(kind: TokenKind, span: Span) -> Self {
        Self {
            kind,
            span,
            docs: Vec::new(),
            raw: false,
        }
    }
}

pub use rokc_core::FUTURE_KEYWORDS;

/// One doc comment line: its text after the marker and one space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocLine {
    /// `//!` (documents the enclosing module).
    pub inner: bool,
    pub text: String,
    pub span: Span,
}

/// Token kinds. Strings carry the lexeme as written; the parser owns
/// any further interpretation (suffix splitting, escape decoding, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    // Literals.
    /// Integer literal; value+suffix kept as written, e.g. `42i32`, `0xFFu8`.
    IntLit(String),
    /// Float literal; e.g. `3.14`, `1.5e-3f32`.
    FloatLit(String),
    /// `'A'` (stdlib `char`).
    CharLit(String),
    /// `c'A'` (primitive `c_char`).
    CCharLit(String),
    /// `"hello"`.
    StrLit(String),
    /// `r"raw"` or `r#"raw"#`.
    RawStrLit(String),
    /// `b"bytes"` (a `[u8; N]`).
    ByteStrLit(String),
    // `f"x = {x:>{w}}"` (spec 1.7.3): `FStrStart`, `FStrText` runs and
    // holes, `FStrEnd`. A hole: `HoleStart`, code tokens, optional
    // `HoleSpec` with `SpecText` and nested holes, `HoleEnd`.
    /// `f"` or (`true`) `f"""`.
    FStrStart(bool),
    /// Literal text: escapes kept raw, `{{` / `}}` folded.
    FStrText(String),
    /// `{` opening a hole (also `${` inside a spec).
    HoleStart,
    /// `:` starting a hole's format spec.
    HoleSpec,
    /// Format spec text, `$$` folded.
    SpecText(String),
    /// `}` closing a hole.
    HoleEnd,
    /// `"` or (`true`) `"""`.
    FStrEnd(bool),

    // Identifiers and contextual promotions.
    Ident(String),
    /// `'name`: a label; `'X'` with its closing quote is a char.
    Label(String),

    // Declaration / variable keywords.
    Let,
    Fn,
    Struct,
    Enum,
    Trait,
    Impl,
    Pub,
    Mod,
    Use,
    As,
    Ref,
    Unsafe,

    // Control flow keywords.
    If,
    Else,
    Match,
    While,
    Loop,
    For,
    Break,
    Continue,
    Return,
    Is,
    Using,
    Try,
    Throw,

    // Async / generator keywords.
    Async,
    Await,
    Yield,

    // Self keywords. `pkg` / `sup` are identifiers: they are path roots
    // only before `::`.
    SelfLower,
    SelfUpper,

    // Sentinels.
    True,
    False,
    None,

    // Arithmetic / comparison / logical.
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    /// `%%` floored modulo (spec 1.10.3).
    PercentPercent,
    EqEq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    AmpAmp,
    PipePipe,
    Bang,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Shl,
    Shr,
    /// Unicode math operator (spec 1.10.3): `× ⋅ ∘ √ ∈ ∉ ⊂ ⊆ ∪ ∩`.
    /// `÷ ≡ ≠ ≤ ≥` lex as their ASCII synonyms (`Slash`, `EqEq`, ...).
    MathOp(char),
    /// Registered extension operator token (spec 1.10.4, `like = ..`).
    ExtOp(String),

    // Broadcast (`.` prefix).
    DotPlus,
    DotMinus,
    DotStar,
    DotSlash,
    DotPercent,
    DotPercentPercent,
    DotEqEq,
    DotNotEq,
    DotLt,
    DotGt,
    DotLtEq,
    DotGtEq,
    DotAmp,
    DotPipe,
    DotAmpAmp,
    DotPipePipe,
    DotCaret,
    DotShl,
    DotShr,

    // Assignment / compound assignment.
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    PercentPercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,

    // Range / slice.
    DotDot,
    DotDotEq,
    Colon,
    ColonColon,

    // Dot / question.
    Dot,
    QuestionDot,
    QuestionQuestion,
    Question,

    // Lambda / arrow.
    Arrow,
    FatArrow,

    // Punctuation.
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Semi,
    At,

    // Attribute introducers (no space allowed between `#` and `[`).
    HashLBracket,
    HashBangLBracket,

    Eof,
}

impl TokenKind {
    /// The token as source text (`rokc build --emit=expanded`, spec
    /// 6.9.9): lexing the result gives the token back.
    #[must_use]
    pub fn text(&self) -> String {
        if let Some(k) = keyword_str(self) {
            return k.to_owned();
        }
        let p = match self {
            Self::Ident(s) if RESERVED.contains(&s.as_str()) => return format!("r#{s}"),
            Self::IntLit(s) | Self::FloatLit(s) | Self::Ident(s) | Self::ExtOp(s) => {
                return s.clone();
            }
            Self::CharLit(s) => return format!("'{s}'"),
            Self::CCharLit(s) => return format!("c'{s}'"),
            Self::StrLit(s) => return format!("\"{s}\""),
            Self::ByteStrLit(s) => return format!("b\"{s}\""),
            Self::FStrStart(t) => return if *t { "f\"\"\"" } else { "f\"" }.to_owned(),
            Self::FStrText(s) => return s.replace('{', "{{").replace('}', "}}"),
            Self::SpecText(s) => return s.replace('$', "$$"),
            Self::FStrEnd(t) => return if *t { "\"\"\"" } else { "\"" }.to_owned(),
            Self::RawStrLit(s) => {
                let mut h = String::new();
                while s.contains(&format!("\"{h}")) {
                    h.push('#');
                }
                return format!("r{h}\"{s}\"{h}");
            }
            Self::Label(s) => return format!("'{s}"),
            Self::MathOp(c) => return c.to_string(),
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Star => "*",
            Self::Slash => "/",
            Self::Percent => "%",
            Self::PercentPercent => "%%",
            Self::EqEq => "==",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::LtEq => "<=",
            Self::GtEq => ">=",
            Self::AmpAmp => "&&",
            Self::PipePipe => "||",
            Self::Bang => "!",
            Self::Amp => "&",
            Self::Pipe => "|",
            Self::Caret => "^",
            Self::Tilde => "~",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::DotPlus => ".+",
            Self::DotMinus => ".-",
            Self::DotStar => ".*",
            Self::DotSlash => "./",
            Self::DotPercent => ".%",
            Self::DotPercentPercent => ".%%",
            Self::DotEqEq => ".==",
            Self::DotNotEq => ".!=",
            Self::DotLt => ".<",
            Self::DotGt => ".>",
            Self::DotLtEq => ".<=",
            Self::DotGtEq => ".>=",
            Self::DotAmp => ".&",
            Self::DotPipe => ".|",
            Self::DotAmpAmp => ".&&",
            Self::DotPipePipe => ".||",
            Self::DotCaret => ".^",
            Self::DotShl => ".<<",
            Self::DotShr => ".>>",
            Self::Eq => "=",
            Self::PlusEq => "+=",
            Self::MinusEq => "-=",
            Self::StarEq => "*=",
            Self::SlashEq => "/=",
            Self::PercentEq => "%=",
            Self::PercentPercentEq => "%%=",
            Self::AmpEq => "&=",
            Self::PipeEq => "|=",
            Self::CaretEq => "^=",
            Self::ShlEq => "<<=",
            Self::ShrEq => ">>=",
            Self::DotDot => "..",
            Self::DotDotEq => "..=",
            Self::Colon | Self::HoleSpec => ":",
            Self::ColonColon => "::",
            Self::Dot => ".",
            Self::QuestionDot => "?.",
            Self::QuestionQuestion => "??",
            Self::Question => "?",
            Self::Arrow => "->",
            Self::FatArrow => "=>",
            Self::LParen => "(",
            Self::RParen => ")",
            Self::LBracket => "[",
            Self::RBracket => "]",
            Self::LBrace | Self::HoleStart => "{",
            Self::RBrace | Self::HoleEnd => "}",
            Self::Comma => ",",
            Self::Semi => ";",
            Self::At => "@",
            Self::HashLBracket => "#[",
            Self::HashBangLBracket => "#![",
            // Keywords were handled above; `Eof` has no text.
            _ => "",
        };
        p.to_owned()
    }

    /// The token as a diagnostic names it: ``keyword `fn` ``,
    /// ``identifier `x` ``, `` `;` ``, `end of file`.
    #[must_use]
    pub fn describe(&self) -> String {
        if let Some(k) = keyword_str(self) {
            return format!("keyword `{k}`");
        }
        match self {
            Self::Ident(s) => format!("identifier `{s}`"),
            Self::IntLit(s) => format!("integer literal `{s}`"),
            Self::FloatLit(s) => format!("float literal `{s}`"),
            Self::CharLit(_) | Self::CCharLit(_) => format!("character literal `{}`", self.text()),
            Self::StrLit(_) | Self::RawStrLit(_) => "string literal".to_owned(),
            Self::ByteStrLit(_) => "byte string literal".to_owned(),
            Self::FStrStart(_) | Self::FStrText(_) | Self::FStrEnd(_) => "f-string".to_owned(),
            Self::HoleStart | Self::HoleEnd | Self::HoleSpec | Self::SpecText(_) => {
                "f-string hole".to_owned()
            }
            Self::Label(s) => format!("label `'{s}`"),
            Self::Eof => "end of file".to_owned(),
            _ => format!("`{}`", self.text()),
        }
    }

    /// Statement terminator token.
    #[must_use]
    pub const fn is_semi(&self) -> bool {
        matches!(self, Self::Semi)
    }
}

/// Reserved keywords (spec 1.5).
///
/// `type`, `any`, `in`, `where`, `throws`, `rethrows`, `catch`, `finally`,
/// `const`, `static`, `extern`, `mut`, `union`, `pkg`, `sub`, `sup` and
/// `rok` are contextual: identifiers to the lexer, keywords in place.
pub const RESERVED: &[&str] = rokc_core::KEYWORDS;

/// A keyword's spelling, for places where any keyword is a name (after
/// `.`, field declarations, argument labels).
#[must_use]
pub fn keyword_str(kind: &TokenKind) -> Option<&'static str> {
    RESERVED
        .iter()
        .copied()
        .find(|w| keyword_kind(w).as_ref() == Some(kind))
}

/// Match a keyword string against the reserved table.
#[must_use]
pub fn keyword_kind(ident: &str) -> Option<TokenKind> {
    Some(match ident {
        "let" => TokenKind::Let,
        "fn" => TokenKind::Fn,
        "struct" => TokenKind::Struct,
        "enum" => TokenKind::Enum,
        "trait" => TokenKind::Trait,
        "impl" => TokenKind::Impl,
        "pub" => TokenKind::Pub,
        "mod" => TokenKind::Mod,
        "use" => TokenKind::Use,
        "as" => TokenKind::As,
        "ref" => TokenKind::Ref,
        "unsafe" => TokenKind::Unsafe,
        "if" => TokenKind::If,
        "else" => TokenKind::Else,
        "match" => TokenKind::Match,
        "while" => TokenKind::While,
        "loop" => TokenKind::Loop,
        "for" => TokenKind::For,
        "break" => TokenKind::Break,
        "continue" => TokenKind::Continue,
        "return" => TokenKind::Return,
        "is" => TokenKind::Is,
        "using" => TokenKind::Using,
        "try" => TokenKind::Try,
        "throw" => TokenKind::Throw,
        "async" => TokenKind::Async,
        "await" => TokenKind::Await,
        "yield" => TokenKind::Yield,
        "self" => TokenKind::SelfLower,
        "Self" => TokenKind::SelfUpper,
        "true" => TokenKind::True,
        "false" => TokenKind::False,
        "none" => TokenKind::None,
        _ => return None,
    })
}
rokc_core::codec_struct!(Token {
    kind,
    span,
    docs,
    raw
});
rokc_core::codec_struct!(DocLine { inner, text, span });
rokc_core::codec_enum!(TokenKind {
    IntLit(x0),
    FloatLit(x0),
    CharLit(x0),
    CCharLit(x0),
    StrLit(x0),
    RawStrLit(x0),
    ByteStrLit(x0),
    FStrStart(x0),
    FStrText(x0),
    HoleStart,
    HoleSpec,
    SpecText(x0),
    HoleEnd,
    FStrEnd(x0),
    Ident(x0),
    Label(x0),
    Let,
    Fn,
    Struct,
    Enum,
    Trait,
    Impl,
    Pub,
    Mod,
    Use,
    As,
    Ref,
    Unsafe,
    If,
    Else,
    Match,
    While,
    Loop,
    For,
    Break,
    Continue,
    Return,
    Is,
    Using,
    Try,
    Throw,
    Async,
    Await,
    Yield,
    SelfLower,
    SelfUpper,
    True,
    False,
    None,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PercentPercent,
    EqEq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    AmpAmp,
    PipePipe,
    Bang,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Shl,
    Shr,
    MathOp(x0),
    ExtOp(x0),
    DotPlus,
    DotMinus,
    DotStar,
    DotSlash,
    DotPercent,
    DotPercentPercent,
    DotEqEq,
    DotNotEq,
    DotLt,
    DotGt,
    DotLtEq,
    DotGtEq,
    DotAmp,
    DotPipe,
    DotAmpAmp,
    DotPipePipe,
    DotCaret,
    DotShl,
    DotShr,
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    PercentPercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,
    DotDot,
    DotDotEq,
    Colon,
    ColonColon,
    Dot,
    QuestionDot,
    QuestionQuestion,
    Question,
    Arrow,
    FatArrow,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Semi,
    At,
    HashLBracket,
    HashBangLBracket,
    Eof,
});
