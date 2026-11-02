//! Parser entry point. Recursive descent with Pratt-style operator
//! parsing over the fixed precedence table.

use rokc_ast::{
    AccessTail, AttrArg, AttrBody, Attribute, Block, CallArg, CatchArm, ConstExpr, EnumDef,
    EnumVariant, EnumVariantKind, Expr, ExprKind, ExternBlock, ExternItem, FieldPattern, FnDef,
    Generics, ImplDef, ImplItem, Item, Literal, MatchArm, ModDef, NamedField, Param, Path,
    PathSegment, Pattern, PatternKind, Program, Stmt, StmtKind, StructDef, StructKind, ThrowsKind,
    TraitDef, TraitItem, TryKind, Ty, TyKind, UnionDef,
};
use rokc_core::Visibility;
use rokc_diagnostic::{Code, RokDiagnostic, Span};
use rokc_lexer::token::{UNUSED_KEYWORDS, keyword_str};
use rokc_lexer::{Token, TokenKind};

use crate::prec::{Assoc, CAST_PREC, COALESCE_PREC, ExtOps, Fixity, PREFIX_PREC};
use meta::{MetaDef, MetaKind, MetaUse};

#[path = "fstring.rs"]
mod fstring;
#[path = "literal.rs"]
pub mod literal;
#[path = "macros.rs"]
mod macros;
pub use macros::expand_to_text;
#[path = "meta.rs"]
pub mod meta;

/// `rest` of a `let (..)` tuple pattern: splice index + optional name.
type LetTupleRest = (usize, Option<String>);

#[allow(clippy::struct_excessive_bools)] // independent context flags, not a state machine
pub struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    file: String,
    pub(crate) diagnostics: Vec<RokDiagnostic>,
    /// Whether `Path { .. }` is a struct literal here; off in `if` /
    /// `while` / `for` heads so the `{` opens the body.
    struct_expr_allowed: bool,
    /// One struct literal allowed at the start of a block head, already
    /// reported (E0013), so the rest of the head parses as written.
    head_struct_lit: bool,
    /// `|` mid-type expands to sum-type alternation. Inside closure
    /// parameter lists the closing `|` must end the list instead.
    type_sum_allowed: bool,
    /// What is left of a `>`-prefixed token split to close a generic list
    /// (`>>`, `>=`, `>>=`); served before the next real token.
    pending: Option<TokenKind>,
    /// In `expr as T`: a `<` opens generic args only when a matching `>`
    /// follows, so `x as i64 < y` stays a comparison.
    in_cast_type: bool,
    /// Current expr/type/pattern/block nesting; capped at `MAX_DEPTH`
    /// so hostile input gets a diagnostic instead of a stack overflow.
    depth: u32,
    depth_reported: bool,
    /// Links on the operator / postfix chains enclosing this point.
    chain: u32,
    /// Extra statements from one site (nested `let ((a, b), c)`),
    /// drained by `parse_block` into the same scope.
    pending_stmts: Vec<Stmt>,
    /// Counter for unique temp names of nested tuple destructures.
    next_destructure_temp: u32,
    /// Spans of untyped `throws` in fn types not (yet) claimed by a
    /// `rethrows` signature; reported at the end of the parse.
    untyped_throws: Vec<Span>,
    /// Registered extension operators and their levels.
    ext: ExtOps,
    /// Items `#[derive(..)]` generated for the last item parsed.
    pending_items: Vec<Item>,
    /// User meta blocks, gathered by the driver's pre-pass.
    meta: std::sync::Arc<meta::MetaTable>,
    /// The build's cfg: resolves `#[cfg_attr]` and `cfg!(..)`
    /// before meta expansion. `None` outside a build (tools, pre-pass).
    cfg: Option<crate::cfg::CfgEnv>,
    /// Meta expansions in progress (cycle detection).
    meta_stack: Vec<String>,
    /// Source text of the meta expansions so far (`Program::expanded`).
    expanded: Vec<String>,
    /// `#[literal]` registrations, gathered by the pre-pass.
    literals: std::sync::Arc<literal::LiteralTable>,
    /// The last `parse_item` consumed an item that expanded to nothing.
    vanished: bool,
    /// Nested `fn`s and block-local items, lifted
    /// to module items once the enclosing item is parsed.
    lifted: Vec<Item>,
    /// Names of the `fn`s being parsed, outermost first (`Type.m` for
    /// a method).
    fn_names: Vec<String>,
    /// The impl target's or trait's name while parsing its items.
    fn_owner: Option<String>,
    /// `CFG_OFF_ATTR` markers for the methods the impl being parsed
    /// dropped.
    cfg_off: Vec<Attribute>,
    /// Lifted names given out so far.
    lifted_names: std::collections::HashSet<String>,
    /// The next primary starts a statement: if it is block-like, `(` / `[`
    /// after it begin a new statement (`while c {} (a, b)`), not a call.
    stmt_head: bool,
    /// `(token, inner)` whose doc comments became attributes.
    docs_used: std::collections::HashSet<(usize, bool)>,
    /// Doc comments (by start) on what keeps no doc (a statement, a
    /// match arm, ..): what, and whether that is an error.
    docs_dropped: std::collections::HashMap<u32, (&'static str, bool)>,
    /// Token ranges no parse looks into (meta block bodies).
    unparsed: Vec<std::ops::Range<usize>>,
    /// The next `bump` takes a name where any keyword is one (`p.move`,
    /// a field): an unused keyword is fine there.
    name_pos: bool,
    /// Per closure or fn body being parsed, innermost last: the label
    /// `return@name` may name there, and the bare `return;`s in it.
    ret_frames: Vec<RetFrame>,
    /// The bare `return;`s of each trailing closure, by its span start:
    /// one in a statement's trailing closure is an error.
    closure_returns: std::collections::HashMap<(u32, u32), Vec<Span>>,
    /// Start of the expression statement being parsed: a trailing-closure
    /// call starting it ends it at its `}`.
    stmt_at: Option<u32>,
    /// Mount path of the package being parsed (`""`: the one being
    /// built); `pkg::name!` names its meta blocks.
    package: String,
    /// Paths the file's `use`s import (a glob ends in `*`): where a
    /// bare meta name may come from.
    meta_imports: Vec<Vec<String>>,
}

/// A closure or fn body being parsed.
#[derive(Default)]
struct RetFrame {
    /// A trailing closure's callee name: what `return@name` names.
    label: Option<String>,
    /// The `return` tokens of the bare `return;`s directly in it.
    bare: Vec<Span>,
}

/// Nesting limit for expressions, types, patterns and blocks.
const MAX_DEPTH: u32 = 256;
/// Links in one left-nested operator / postfix chain.
const MAX_CHAIN: u32 = 4096;
/// The path of the type a variadic `xs: ...T` parameter has in the body
/// (`Slice<T>`).
pub const VARIADIC_SLICE: [&str; 5] = ["rok", "core", "collections", "slice", "Slice"];

/// `Slice<elem>`: a variadic `...elem` parameter's type.
#[must_use]
pub fn variadic_slice_ty(elem: Ty) -> Ty {
    let span = elem.span;
    let n = VARIADIC_SLICE.len();
    Ty {
        kind: TyKind::Path(Path {
            segments: VARIADIC_SLICE
                .iter()
                .enumerate()
                .map(|(k, name)| PathSegment {
                    name: (*name).to_owned(),
                    generics: if k + 1 == n {
                        vec![elem.clone()]
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
            span,
            origin: None,
        }),
        span,
    }
}

/// The file an `include_str!` / `include_bytes!` in `file` names by
/// the string literal body `lit`: relative to `file`'s directory.
#[must_use]
pub fn include_path(file: &str, lit: &str) -> std::path::PathBuf {
    let rel = rokc_ast::escape::unescape(lit, rokc_ast::escape::EscapeMode::Str)
        .unwrap_or_else(|_| lit.to_owned());
    std::path::Path::new(file)
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""))
        .join(rel)
}

/// The literal `include_bytes!` (`bytes`) or `include_str!` gives for
/// the path literal `lit` in `file`, or why not.
pub fn include_file(file: &str, lit: &str, bytes: bool) -> Result<Literal, String> {
    let data = std::fs::read(include_path(file, lit)).map_err(|e| {
        let why = if e.kind() == std::io::ErrorKind::NotFound {
            "no such file".to_owned()
        } else {
            e.to_string()
        };
        format!("cannot read \"{lit}\": {why}")
    })?;
    if bytes {
        return Ok(Literal::ByteStr(rokc_ast::escape::escape_bytes(&data)));
    }
    match String::from_utf8(data) {
        Ok(s) => Ok(Literal::Str(rokc_ast::escape::escape_str(&s))),
        Err(e) => Err(format!(
            "\"{lit}\" is not UTF-8 (invalid byte at offset {}); use `include_bytes!`",
            e.utf8_error().valid_up_to()
        )),
    }
}

/// `MAX_DEPTH` levels need ~5 MiB (the dev profile builds the parser at
/// opt-level 1; ~4 MiB at opt-level 3: `Expr` values are large).
const PARSER_STACK: usize = 16 << 20;

pub fn parse_program(tokens: &[Token], file: impl Into<String>) -> (Program, Vec<RokDiagnostic>) {
    parse_program_ext(tokens, file, &ExtOps::default())
}

/// `parse_program` that also knows registered extension operators.
pub fn parse_program_ext(
    tokens: &[Token],
    file: impl Into<String>,
    ext: &ExtOps,
) -> (Program, Vec<RokDiagnostic>) {
    parse_program_meta(
        tokens,
        file,
        "",
        ext,
        &std::sync::Arc::default(),
        &std::sync::Arc::default(),
        None,
    )
}

/// `parse_program_ext` that also expands user meta blocks
/// and matches `#[literal]` patterns.
pub fn parse_program_meta(
    tokens: &[Token],
    file: impl Into<String>,
    package: &str,
    ext: &ExtOps,
    metas: &std::sync::Arc<meta::MetaTable>,
    literals: &std::sync::Arc<literal::LiteralTable>,
    cfg: Option<&crate::cfg::CfgEnv>,
) -> (Program, Vec<RokDiagnostic>) {
    let file = file.into();
    let ext = ext.clone();
    let metas = std::sync::Arc::clone(metas);
    let literals = std::sync::Arc::clone(literals);
    let cfg = cfg.cloned();
    let package = package.to_owned();
    let run = move || {
        let mut p = Parser::new(tokens, file);
        p.ext = ext;
        p.package = package;
        p.meta = metas;
        p.literals = literals;
        p.cfg = cfg;
        let program = p.parse_program();
        (program, p.diagnostics)
    };
    on_parser_stack(run)
}

/// A file that is one expression and nothing else (a `.rokon` file: the package
/// manifest): parse it without top-level statement rules (no `;`).
pub fn parse_expr_file(
    tokens: &[Token],
    file: impl Into<String>,
) -> (Option<Expr>, Vec<RokDiagnostic>) {
    let file = file.into();
    on_parser_stack(move || {
        let mut p = Parser::new(tokens, file);
        let e = p.parse_expr();
        if e.is_some() && !matches!(p.peek(), TokenKind::Eof) {
            let span = p.peek_span();
            p.error(
                Code::SYNTAX,
                span,
                "expected the end of the file after the expression",
            );
        }
        (e, p.diagnostics)
    })
}

/// `MAX_DEPTH` recursion needs more than a default 1-2 MiB stack, so
/// parse on a thread sized for the cap (only address space until used).
fn on_parser_stack<T: Send>(run: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(PARSER_STACK)
            .spawn_scoped(s, run)
            .expect("spawn parser thread")
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}

impl<'a> Parser<'a> {
    pub fn new(tokens: &'a [Token], file: impl Into<String>) -> Self {
        Self {
            tokens,
            pos: 0,
            file: file.into(),
            diagnostics: Vec::new(),
            struct_expr_allowed: true,
            head_struct_lit: false,
            stmt_head: false,
            type_sum_allowed: true,
            pending: None,
            in_cast_type: false,
            depth: 0,
            depth_reported: false,
            chain: 0,
            pending_stmts: Vec::new(),
            next_destructure_temp: 0,
            untyped_throws: Vec::new(),
            ext: ExtOps::default(),
            pending_items: Vec::new(),
            meta: std::sync::Arc::default(),
            cfg: None,
            meta_stack: Vec::new(),
            expanded: Vec::new(),
            literals: std::sync::Arc::default(),
            vanished: false,
            lifted: Vec::new(),
            fn_names: Vec::new(),
            fn_owner: None,
            cfg_off: Vec::new(),
            lifted_names: std::collections::HashSet::new(),
            docs_used: std::collections::HashSet::new(),
            docs_dropped: std::collections::HashMap::new(),
            unparsed: Vec::new(),
            name_pos: false,
            stmt_at: None,
            ret_frames: Vec::new(),
            closure_returns: std::collections::HashMap::new(),
            package: String::new(),
            meta_imports: Vec::new(),
        }
    }

    /// Does an `else` sit at this statement's top level, before its `;`
    /// (`let (a, b) = e else { .. };`)? An `if .. else` in the
    /// initializer also says yes; the general `let` form handles that.
    fn stmt_has_else(&self) -> bool {
        let mut depth = 0usize;
        for t in &self.tokens[self.pos..] {
            match &t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                }
                TokenKind::Else if depth == 0 => return true,
                TokenKind::Semi | TokenKind::Eof if depth == 0 => return false,
                _ => {}
            }
        }
        false
    }

    fn backtracks<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> bool {
        let (pos, n_diags, n_pending, n_lifted) = (
            self.pos,
            self.diagnostics.len(),
            self.pending_stmts.len(),
            self.lifted.len(),
        );
        let failed = f(self).is_none();
        self.pos = pos;
        self.diagnostics.truncate(n_diags);
        self.pending_stmts.truncate(n_pending);
        self.lifted.truncate(n_lifted);
        failed
    }

    pub(crate) fn at_eof(&self) -> bool {
        matches!(self.peek(), TokenKind::Eof)
    }

    pub(crate) const fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn parse_item_pub(&mut self) -> Option<Item> {
        self.parse_item()
    }

    /// Run `f` with `struct_expr_allowed` = `allowed`, restoring it even
    /// when `f` bails out with `None`.
    fn with_struct_expr<T>(&mut self, allowed: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let prev = std::mem::replace(&mut self.struct_expr_allowed, allowed);
        let r = f(self);
        self.struct_expr_allowed = prev;
        r
    }

    /// Same as `with_struct_expr` for `type_sum_allowed`.
    fn with_type_sum<T>(&mut self, allowed: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let prev = std::mem::replace(&mut self.type_sum_allowed, allowed);
        let r = f(self);
        self.type_sum_allowed = prev;
        r
    }

    /// Rebuild an assignment target so each side-effecting sub-expression
    /// runs once, into `let` temps; variables, `self`,
    /// literals and field chains on them stay.
    fn hoist_place(&mut self, e: Expr, temps: &mut Vec<Stmt>) -> Expr {
        let span = e.span;
        let kind = match e.kind {
            ExprKind::Path(_)
            | ExprKind::SelfLower
            | ExprKind::SelfUpper
            | ExprKind::Literal(_) => e.kind,
            ExprKind::Field(base, name) => {
                ExprKind::Field(Box::new(self.hoist_place(*base, temps)), name)
            }
            ExprKind::TupleField(base, i) => {
                ExprKind::TupleField(Box::new(self.hoist_place(*base, temps)), i)
            }
            ExprKind::Index(base, idx) => {
                let base = self.hoist_place(*base, temps);
                let idx = self.hoist_place(*idx, temps);
                ExprKind::Index(Box::new(base), Box::new(idx))
            }
            other => {
                let name = self.fresh_destructure_temp();
                temps.push(Stmt {
                    kind: StmtKind::Let {
                        name: name.clone(),
                        ty: None,
                        init: Expr { kind: other, span },
                    },
                    span,
                });
                ExprKind::Path(Path {
                    segments: vec![PathSegment {
                        name,
                        generics: Vec::new(),
                    }],
                    span,
                    origin: None,
                })
            }
        };
        Expr { kind, span }
    }

    /// Recover from a failed item / statement that began at `from`: one
    /// diagnostic per mistake. Close braces opened since `from` (not
    /// parens / brackets: an unclosed one is often the error), then stop
    /// after `;`, before an item or statement keyword, or at the enclosing
    /// `}`, consumed only at top level or when it closes the failed item.
    fn recover(&mut self, from: usize, ctx: Recover) {
        self.pending = None;
        let opened: i32 = self.tokens[from.min(self.pos)..self.pos]
            .iter()
            .map(|t| match t.kind {
                TokenKind::LBrace => 1,
                TokenKind::RBrace => -1,
                _ => 0,
            })
            .sum();
        let mut depth = opened.max(0);
        if self.pos == from {
            // Failed on its first token: make progress.
            match self.bump().kind {
                TokenKind::LBrace => depth += 1,
                TokenKind::Semi | TokenKind::RBrace if depth == 0 => return,
                _ => {}
            }
        }
        loop {
            let kind = self.peek();
            match kind {
                TokenKind::Eof => return,
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace if depth == 0 => {
                    if ctx == Recover::TopItem {
                        self.bump();
                    }
                    return;
                }
                TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 && ctx != Recover::Stmt {
                        self.bump();
                        return;
                    }
                }
                TokenKind::Semi if depth == 0 => {
                    self.bump();
                    return;
                }
                _ if depth == 0 && ctx != Recover::Stmt && starts_item(kind) => return,
                _ if depth == 0 && ctx == Recover::Stmt && starts_stmt(kind) => return,
                _ => {}
            }
            self.bump();
        }
    }

    /// Run a recursive production one nesting level deeper, or report
    /// (once) and fail when `MAX_DEPTH` is reached.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        if self.depth >= MAX_DEPTH {
            if !self.depth_reported {
                self.depth_reported = true;
                let span = self.peek_span();
                self.error(
                    Code::NESTING_LIMIT,
                    span,
                    format!("nesting too deep (limit {MAX_DEPTH})"),
                );
            }
            return None;
        }
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// Count one more link of a left-nested chain (`1 + 1 + ..`,
    /// `a.f().g()`), which deepens the tree without `nested`.
    fn chain_link(&mut self) -> Option<()> {
        self.chain += 1;
        if self.chain <= MAX_CHAIN {
            return Some(());
        }
        if !self.depth_reported {
            self.depth_reported = true;
            let span = self.peek_span();
            self.error(
                Code::NESTING_LIMIT,
                span,
                format!("expression chain too long (limit {MAX_CHAIN})"),
            );
        }
        None
    }

    /// From a path (`a::E`) at the cursor: is its `{ .. }` group followed
    /// by another `{`?
    fn braces_then_block(&self) -> bool {
        self.braces_then(true)
    }

    /// `braces_then_block`; with `strict` off, a continued head
    /// (`S { a: 1 }.ok() {`) counts too.
    fn braces_then(&self, strict: bool) -> bool {
        let mut i = self.pos;
        let kind = |i: usize| self.tokens.get(i).map(|t| &t.kind);
        while matches!(kind(i), Some(TokenKind::Ident(_) | TokenKind::ColonColon)) {
            i += 1;
        }
        if i == self.pos || !matches!(kind(i), Some(TokenKind::LBrace)) {
            return false;
        }
        let mut depth = 0u32;
        while let Some(k) = kind(i) {
            match k {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(kind(i + 1), Some(TokenKind::LBrace))
                            || !strict && self.continued_head(i + 1);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// At token `i`: a member access or an operator, then (at bracket
    /// depth 0) the `{` of the block.
    fn continued_head(&self, i: usize) -> bool {
        let kind = |i: usize| self.tokens.get(i).map(|t| &t.kind);
        if !matches!(
            kind(i),
            Some(
                TokenKind::Dot
                    | TokenKind::Gt
                    | TokenKind::Lt
                    | TokenKind::GtEq
                    | TokenKind::LtEq
                    | TokenKind::EqEq
                    | TokenKind::NotEq
            )
        ) {
            return false;
        }
        let mut depth = 0i32;
        for j in i.. {
            match kind(j) {
                Some(TokenKind::LParen | TokenKind::LBracket) => depth += 1,
                Some(TokenKind::RParen | TokenKind::RBracket) => depth -= 1,
                Some(TokenKind::LBrace) if depth == 0 => return true,
                None | Some(TokenKind::Semi | TokenKind::LBrace | TokenKind::RBrace) => {
                    return false;
                }
                _ => {}
            }
        }
        false
    }

    /// A `fn(..)` type's parameter types through `)`. It names no
    /// parameters and is never `rethrows`.
    fn fn_type_params(&mut self) -> Option<Vec<Ty>> {
        if matches!(self.peek(), TokenKind::Ident(_))
            && matches!(self.peek_next(), Some(TokenKind::Colon))
        {
            let s = self.peek_span();
            self.error_help(
                Code::SYNTAX,
                s,
                "a function type has no parameter names",
                "write the types only: `fn(i32, str) -> bool`",
            );
            return None;
        }
        let args = self.nested_type_list(TokenKind::RParen)?;
        self.expect(&TokenKind::RParen, "`)`")?;
        if self.at_kw("rethrows") {
            let s = self.peek_span();
            self.error_help(
                Code::SYNTAX,
                s,
                "a function type cannot be `rethrows`",
                "write the error it throws: `fn(i32) throws (E) -> i32`",
            );
            return None;
        }
        Some(args)
    }

    /// `S { a: 1 } {` at the cursor: a struct literal at the top of a
    /// block head, where its `{` reads as the body.
    fn struct_lit_head(&self) -> bool {
        if !self.braces_then(false) {
            return false;
        }
        let kind = |i: usize| self.tokens.get(i).map(|t| &t.kind);
        let mut i = self.pos;
        while matches!(kind(i), Some(TokenKind::Ident(_) | TokenKind::ColonColon)) {
            i += 1;
        }
        if !matches!(
            (kind(i + 1), kind(i + 2)),
            (Some(TokenKind::Ident(_)), Some(TokenKind::Colon))
        ) {
            return false;
        }
        // A match arm `x: T => ..` is a body, not fields.
        let mut depth = 0u32;
        while let Some(k) = kind(i) {
            match k {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        // `S { a: 1 } {` or `S { a: 1 }.ok() {`.
                        return matches!(kind(i + 1), Some(TokenKind::LBrace))
                            || self.continued_head(i + 1);
                    }
                }
                TokenKind::FatArrow if depth == 1 => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// The expression heading a block (`if` / `while` / `match` / `for` /
    /// `using`), where a struct literal needs parentheses.
    fn parse_head(&mut self) -> Option<Expr> {
        if !self.struct_lit_head() {
            return self.with_struct_expr(false, Self::parse_expr);
        }
        // The literal's closing `}`.
        let close = (self.pos..self.tokens.len())
            .find(|&i| matches!(self.tokens[i].kind, TokenKind::LBrace))
            .and_then(|open| {
                let mut depth = 0u32;
                (open..self.tokens.len()).find(|&i| {
                    match self.tokens[i].kind {
                        TokenKind::LBrace => depth += 1,
                        TokenKind::RBrace => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                })
            });
        if let Some(close) = close.filter(|&c| {
            !matches!(
                self.tokens.get(c + 1).map(|t| &t.kind),
                Some(TokenKind::LBrace)
            )
        }) {
            // `S { a: 1 }.ok() {`: the literal, then the rest of the head.
            let lit_span = Span {
                end: self.tokens[close].span.end,
                ..self.peek_span()
            };
            self.head_struct_lit = true;
            let head = self.with_struct_expr(false, Self::parse_expr);
            self.head_struct_lit = false;
            let text = self.text_in(lit_span);
            self.error_help(
                Code::STRUCT_LITERAL_IN_HEAD,
                lit_span,
                "a struct literal can't start a block's head: its `{` would open the body \
                ",
                format!("wrap the struct literal in parentheses: `({text})`"),
            );
            return head;
        }
        let lit = self.with_struct_expr(true, Self::parse_primary)?;
        let text = self.text_in(lit.span);
        self.error_help(
            Code::STRUCT_LITERAL_IN_HEAD,
            lit.span,
            "a struct literal can't start a block's head: its `{` would open the body \
            ",
            format!("wrap the struct literal in parentheses: `({text})`"),
        );
        Some(lit)
    }

    /// The code of the tokens inside `span`, spaced as written.
    fn text_in(&self, span: Span) -> String {
        let mut out = String::new();
        let mut end = None;
        for t in self.tokens.iter().filter(|t| {
            t.span.file == span.file && t.span.start >= span.start && t.span.end <= span.end
        }) {
            if end.is_some_and(|e| e < t.span.start) {
                out.push(' ');
            }
            out.push_str(&t.kind.text());
            end = Some(t.span.end);
        }
        out
    }

    fn error_help(
        &mut self,
        code: Code,
        span: Span,
        msg: impl Into<String>,
        help: impl Into<String>,
    ) {
        self.diagnostics.push(
            RokDiagnostic::error(code, span, msg)
                .with_file(self.file.clone())
                .with_help(help),
        );
    }

    /// `c ? a : b` (C, JS) at the cursor, just past `c`'s `?`.
    fn ternary_ahead(&self) -> bool {
        if self.prev_kind() != Some(&TokenKind::Question) {
            return false;
        }
        match self.peek() {
            TokenKind::Ident(w) if !matches!(w.as_str(), "catch" | "finally" | "in" | "where") => {}
            TokenKind::IntLit(_)
            | TokenKind::FloatLit(_)
            | TokenKind::StrLit(_)
            | TokenKind::CharLit(_)
            | TokenKind::FStrStart(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::None
            | TokenKind::LParen
            | TokenKind::LBracket => {}
            _ => return false,
        }
        let line = self.peek_span().line;
        let mut depth = 0u32;
        for t in &self.tokens[self.pos..] {
            if t.span.line != line {
                return false;
            }
            match t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    let Some(d) = depth.checked_sub(1) else {
                        return false;
                    };
                    depth = d;
                }
                TokenKind::Semi | TokenKind::Eof => return false,
                TokenKind::Colon if depth == 0 => return true,
                _ => {}
            }
        }
        false
    }

    /// `c ? a : b`: there is no such operator; read it whole for the
    /// message and keep `c`.
    fn ternary(&mut self, cond: Expr) -> Option<Expr> {
        let q = self.prev_span()?;
        let c = self.text_in(cond.span);
        let c = c.strip_suffix('?').unwrap_or(&c).trim_end().to_owned();
        let a = self.parse_expr()?;
        self.expect(&TokenKind::Colon, "`:`")?;
        let b = self.parse_expr()?;
        let (a, b) = (self.text_in(a.span), self.text_in(b.span));
        self.error_help(
            Code::FOREIGN_SYNTAX,
            q,
            "rok has no `? :` operator",
            format!("`if` is an expression: `if {c} {{ {a} }} else {{ {b} }}`"),
        );
        Some(cond)
    }

    /// `i++` / `i--` (C, JS) at the cursor, after `lhs`: there are no
    /// such operators. Reports and skips them.
    fn incr_decr(&mut self, lhs: &Expr) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let tok = |i: usize| self.tokens.get(self.pos + i);
        let (Some(a), Some(b), Some(c)) = (tok(0), tok(1), tok(2)) else {
            return false;
        };
        let op = match (&a.kind, &b.kind) {
            (TokenKind::Plus, TokenKind::Plus) => "+",
            (TokenKind::Minus, TokenKind::Minus) => "-",
            _ => return false,
        };
        let ends = matches!(
            c.kind,
            TokenKind::Semi | TokenKind::RParen | TokenKind::RBrace | TokenKind::Eof
        );
        if b.span.start != a.span.end || !ends {
            return false;
        }
        let at = Span {
            end: b.span.end,
            ..a.span
        };
        let x = self.text_in(lhs.span);
        self.error_help(
            Code::FOREIGN_SYNTAX,
            at,
            format!("rok has no `{op}{op}` operator"),
            format!("write `{x} {op}= 1`"),
        );
        self.bump();
        self.bump();
        true
    }

    fn fresh_destructure_temp(&mut self) -> String {
        let n = self.next_destructure_temp;
        self.next_destructure_temp = self.next_destructure_temp.wrapping_add(1);
        format!("__destruct{n}")
    }

    /// `if let` / `while let` pattern with an optional `: T`;
    /// `(a, b): T` matches a `__destructN: T` and returns the statement that
    /// destructures it.
    fn parse_let_cond_pattern(&mut self) -> Option<(Pattern, Option<Stmt>)> {
        let pat = self.parse_pattern()?;
        if !self.eat(&TokenKind::Colon) {
            return Some((pat, None));
        }
        let ty = self.parse_type()?;
        let span = pat.span;
        let typed = |binder| Pattern {
            kind: PatternKind::TypeCheck { binder, ty },
            span,
        };
        if matches!(pat.kind, PatternKind::Wildcard) {
            return Some((typed(None), None));
        }
        let tmp = self.fresh_destructure_temp();
        let init = Expr {
            span,
            kind: ExprKind::Path(Path {
                segments: vec![PathSegment {
                    name: tmp.clone(),
                    generics: Vec::new(),
                }],
                span,
                origin: None,
            }),
        };
        let stmt = Stmt {
            kind: StmtKind::LetPattern {
                pat,
                ty: None,
                init,
            },
            span,
        };
        Some((typed(Some(tmp)), Some(stmt)))
    }

    /// Consume a closing `>`, splitting `>>`, `>=`, `>>=` and leaving the
    /// rest pending; false (with an error) when there is none.
    fn expect_closing_gt(&mut self) -> bool {
        let rest = match self.peek() {
            TokenKind::Gt => None,
            TokenKind::Shr => Some(TokenKind::Gt),
            TokenKind::GtEq => Some(TokenKind::Eq),
            TokenKind::ShrEq => Some(TokenKind::GtEq),
            other => {
                let msg = format!(
                    "expected `>` to close generic arguments, found {}",
                    other.describe()
                );
                let span = self.peek_span();
                // `Buf<2 * 8>`: arithmetic in an argument needs braces.
                let after_value = matches!(
                    self.prev_kind(),
                    Some(TokenKind::IntLit(_) | TokenKind::Ident(_) | TokenKind::RParen)
                );
                if after_value
                    && matches!(
                        other,
                        TokenKind::Star
                            | TokenKind::Plus
                            | TokenKind::Minus
                            | TokenKind::Slash
                            | TokenKind::Percent
                    )
                {
                    self.error_help(
                        Code::SYNTAX,
                        span,
                        msg,
                        "a const argument that is an expression goes in braces, as in \
                         `<{ 2 * 8 }>`",
                    );
                } else {
                    self.error(Code::SYNTAX, span, msg);
                }
                return false;
            }
        };
        self.bump();
        self.pending = rest;
        true
    }

    /// Open a generic list or `<T as Tr>`: consume a `<`, splitting `<<`
    /// (`Tag<<A as P>::X>`) and leaving the second pending.
    fn eat_opening_lt(&mut self) -> bool {
        match self.peek() {
            TokenKind::Lt => {
                self.bump();
            }
            TokenKind::Shl => {
                self.bump();
                self.pending = Some(TokenKind::Lt);
            }
            _ => return false,
        }
        true
    }

    fn peek(&self) -> &TokenKind {
        if let Some(k) = &self.pending {
            return k;
        }
        self.tokens
            .get(self.pos)
            .map_or(&TokenKind::Eof, |t| &t.kind)
    }

    fn peek_span(&self) -> Span {
        self.tokens.get(self.pos).map_or(Span::DUMMY, |t| t.span)
    }

    fn bump(&mut self) -> Token {
        if let Some(kind) = self.pending.take() {
            // Remainder of a split token; span of the token it came from.
            let span = self
                .tokens
                .get(self.pos.saturating_sub(1))
                .map_or(Span::DUMMY, |t| t.span);
            return Token::new(kind, span);
        }
        // Recovery can bump past the last token: hand back a synthetic Eof.
        let Some(t) = self.tokens.get(self.pos).cloned() else {
            return Token::new(TokenKind::Eof, Span::DUMMY);
        };
        if let TokenKind::Ident(n) = &t.kind {
            let name_pos = std::mem::take(&mut self.name_pos);
            // `@macro { .. }` is meta syntax, not a name.
            if !t.raw
                && !name_pos
                && UNUSED_KEYWORDS.contains(&n.as_str())
                && self.prev_kind() != Some(&TokenKind::At)
            {
                self.unused_keyword(n, t.span);
            }
        }
        self.pos += 1;
        t
    }

    /// `bump` a name where any keyword is one (after `.`, a field).
    fn bump_name(&mut self) -> Token {
        self.name_pos = true;
        let t = self.bump();
        self.name_pos = false;
        t
    }

    /// `name` is a reserved keyword with no syntax yet: not a name.
    /// Backtracking drops what a failed parse reported, and the token is
    /// bumped again: report once per kept parse.
    fn unused_keyword(&mut self, name: &str, span: Span) {
        let msg = format!("`{name}` is a reserved keyword, not a name");
        if span.is_dummy()
            || self
                .diagnostics
                .iter()
                .any(|d| d.message == msg && d.primary.as_ref().is_some_and(|l| l.span == span))
        {
            return;
        }
        self.diagnostics.push(
            RokDiagnostic::error(Code::RESERVED_NAME, span, msg)
                .with_file(self.file.clone())
                .with_help(format!("write `r#{name}` to keep this name, or rename it"))
                .with_fix(span, format!("r#{name}"), format!("write `r#{name}`")),
        );
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Option<Token> {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(kind) {
            Some(self.bump())
        } else {
            let d = self.expected(kind, what);
            self.diagnostics.push(d);
            None
        }
    }

    /// "expected X, found Y", with hints for a missing `;` (at the line's
    /// end), an unclosed delimiter (its opener) and `->` / `=>` mixups.
    fn expected(&self, kind: &TokenKind, what: &str) -> RokDiagnostic {
        let found = self.peek();
        let here = self.peek_span();
        let d = RokDiagnostic::error(
            Code::SYNTAX,
            Span::DUMMY,
            format!("expected {what}, found {}", found.describe()),
        )
        .with_file(self.file.clone());
        let prev = self.prev_span();
        if let Some(help) = self.bang_help() {
            return d
                .with_span(here, "")
                .with_help(help)
                .with_fix(here, "", "delete `!`");
        }
        match kind {
            TokenKind::Semi if prev.is_some_and(|p| p.line < here.line || here.is_dummy()) => {
                let at = self.after_prev();
                return d
                    .with_span(at, "expected `;`")
                    .with_help("add `;` at the end of the statement");
            }
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                if let Some(open) = self.unclosed_opener(kind) {
                    return d
                        .with_span(here, format!("expected {what}"))
                        .with_secondary(open, "unclosed delimiter");
                }
            }
            // `if x = 3 {` (C): a comparison is `==`.
            TokenKind::LBrace if matches!(found, TokenKind::Eq) && self.in_condition() => {
                return d
                    .with_span(here, "")
                    .with_help("did you mean `==`? `=` assigns; `==` compares")
                    .with_fix(here, "==", "use `==`");
            }
            TokenKind::FatArrow if matches!(found, TokenKind::Arrow) => {
                return d
                    .with_span(here, "")
                    .with_help("match arms are written `pattern => value`");
            }
            _ if matches!(found, TokenKind::FatArrow)
                && self.prev_kind() == Some(&TokenKind::RParen) =>
            {
                return d
                    .with_span(here, "")
                    .with_help("a return type is written `-> Type`");
            }
            _ => {}
        }
        // `fucn main`, `retrun x`: an identifier that reads as a
        // misspelled keyword, followed by another word.
        if let Some(TokenKind::Ident(word)) = self.prev_kind() {
            if matches!(found, TokenKind::Ident(_)) || keyword_alias(word).is_some() {
                let import = match found {
                    TokenKind::Ident(name)
                        if matches!(word.as_str(), "import" | "include" | "require") =>
                    {
                        import_help(&self.file, self.tokens, name)
                    }
                    _ => None,
                };
                if let Some(k) =
                    import.or_else(|| keyword_hint(word).map(|k| format!("did you mean `{k}`?")))
                {
                    let at = self.prev_span().unwrap_or(here);
                    return d
                        .with_span(here, "")
                        .with_secondary(at, "not a keyword")
                        .with_help(k);
                }
            }
        }
        d.with_span(here, "")
    }

    /// The cursor is in an `if` or `while` condition.
    fn in_condition(&self) -> bool {
        self.tokens[..self.pos.min(self.tokens.len())]
            .iter()
            .rev()
            .map(|t| &t.kind)
            .take_while(|k| !matches!(k, TokenKind::Semi | TokenKind::LBrace | TokenKind::RBrace))
            .any(|k| matches!(k, TokenKind::If | TokenKind::While))
    }

    /// `v!` (Swift, TypeScript): a postfix `!` to unwrap, which rok does
    /// not have.
    pub(crate) fn bang_help(&self) -> Option<String> {
        if !matches!(self.peek(), TokenKind::Bang)
            || matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace)
            )
        {
            return None;
        }
        let v = match self.prev_kind()? {
            TokenKind::Ident(v) => v.as_str(),
            TokenKind::RParen | TokenKind::RBracket => "v",
            _ => return None,
        };
        Some(format!(
            "rok has no `!` to unwrap; for a value that may be none, write `{v} ?? default`, \
             `if let x: T = {v} {{ .. }}`, or `match {v} {{ x: T => .., none => .. }}`"
        ))
    }

    fn prev_span(&self) -> Option<Span> {
        self.tokens.get(self.pos.checked_sub(1)?).map(|t| t.span)
    }

    /// `span` widened to the end of the previous token: a compound
    /// expression covers all its tokens.
    fn to_prev(&self, mut span: Span) -> Span {
        if let Some(p) = self.prev_span() {
            if p.file == span.file && p.end > span.end {
                span.end = p.end;
            }
        }
        span
    }

    fn prev_kind(&self) -> Option<&TokenKind> {
        self.tokens.get(self.pos.checked_sub(1)?).map(|t| &t.kind)
    }

    /// Zero-width span just after the previous token.
    fn after_prev(&self) -> Span {
        let Some(t) = self.pos.checked_sub(1).and_then(|i| self.tokens.get(i)) else {
            return self.peek_span();
        };
        let len = u32::try_from(t.kind.text().chars().count()).unwrap_or(1);
        Span {
            start: t.span.end,
            end: t.span.end,
            line: t.span.line,
            col: t.span.col + len.max(1),
            file: t.span.file,
        }
    }

    /// The innermost delimiter still open before the current token
    /// that `close` would close.
    fn unclosed_opener(&self, close: &TokenKind) -> Option<Span> {
        let mut stack: Vec<(&TokenKind, Span)> = Vec::new();
        for t in self.tokens.get(..self.pos)? {
            match &t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                    stack.push((&t.kind, t.span));
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    stack.pop();
                }
                _ => {}
            }
        }
        let open = match close {
            TokenKind::RParen => TokenKind::LParen,
            TokenKind::RBracket => TokenKind::LBracket,
            _ => TokenKind::LBrace,
        };
        stack
            .iter()
            .rev()
            .find(|(k, _)| **k == open)
            .map(|(_, s)| *s)
    }

    fn error(&mut self, code: Code, span: Span, msg: impl Into<String>) {
        self.diagnostics
            .push(RokDiagnostic::error(code, span, msg).with_file(self.file.clone()));
    }

    pub fn parse_program(&mut self) -> Program {
        let mut items = Vec::new();
        let mut stmts = Vec::new();
        let mut tops = Vec::new();
        let attrs = self.parse_inner_attrs();
        if !self.meta.is_empty() && self.meta_stack.is_empty() {
            self.scan_meta_imports();
        }
        while !matches!(self.peek(), TokenKind::Eof) {
            let from = self.pos;
            if self.meta_block().is_some() || self.eat(&TokenKind::Semi) {
                continue;
            }
            let at_item =
                (self.at_item_start() || self.at_item_macro_call()) && !self.at_attributed_stmt();
            tops.push((self.peek_span().start, at_item));
            if at_item {
                if let Some(item) = self.parse_item() {
                    items.push(item);
                    items.append(&mut self.pending_items);
                } else if !std::mem::take(&mut self.vanished) {
                    self.recover(from, Recover::TopItem);
                }
                continue;
            }
            let attrs = self.cfg_only_attrs("a statement");
            let marks = (self.pending_stmts.len(), self.lifted.len());
            if let Some(stmt) = self.parse_top_stmt() {
                if self.cfg_drops(&attrs, marks) {
                    continue;
                }
                stmts.push(stmt);
                while let Some(extra) = self.pending_stmts.pop() {
                    stmts.push(extra);
                }
                items.append(&mut self.lifted);
            } else {
                self.pending_stmts.clear();
                self.recover(from, Recover::Stmt);
            }
        }
        self.statement_closure_returns(&stmts, None);
        // Untyped `throws` in fn types that no `rethrows` claimed keep the
        // marker; HIR reads it as `any Error`.
        self.untyped_throws.clear();
        self.misplaced_docs();
        Program {
            attrs,
            items,
            stmts,
            expanded: std::mem::take(&mut self.expanded),
            tops,
        }
    }

    /// An item rather than a top-level statement? `unsafe` / `async` start
    /// one only before `fn`.
    /// Positioned at `name!( .. )` for a registered macro:
    /// at module level it is an item (it may expand to items).
    fn at_item_macro_call(&self) -> bool {
        !self.meta.is_empty()
            && self
                .macro_head()
                .is_some_and(|(q, n, _)| !q.is_empty() || self.meta.find_macro(&n).is_some())
    }

    fn at_item_start(&self) -> bool {
        match self.peek() {
            TokenKind::Unsafe | TokenKind::Async | TokenKind::Yield => matches!(
                self.peek_next(),
                Some(
                    TokenKind::Fn
                        | TokenKind::Impl
                        | TokenKind::Trait
                        | TokenKind::Unsafe
                        | TokenKind::Async
                        | TokenKind::Yield
                )
            ),
            _ if self.at_union_item() || self.at_ctx_item() || self.at_wit_item() => true,
            k => starts_item(k),
        }
    }

    /// `extern "C" {` / `extern {`, `const NAME` / `const fn`,
    /// `static [mut] NAME`, `type Name`: contextual at item start.
    fn at_ctx_item(&self) -> bool {
        let next = self.peek_next();
        let TokenKind::Ident(k) = self.peek() else {
            return false;
        };
        if self.peek_raw() {
            return false;
        }
        match k.as_str() {
            "extern" => matches!(next, Some(TokenKind::StrLit(_) | TokenKind::LBrace)),
            "const" => matches!(
                next,
                Some(TokenKind::Ident(_) | TokenKind::Fn | TokenKind::Unsafe)
            ),
            "static" | "type" => matches!(next, Some(TokenKind::Ident(_))),
            _ => false,
        }
    }

    /// `union` is an item keyword only as `union Name` at item
    /// start (`Set::union`, `a.union(b)` are identifiers).
    fn at_union_item(&self) -> bool {
        self.at_kw("union") && matches!(self.peek_next(), Some(TokenKind::Ident(_)))
    }

    /// Top-level statement of the entry file. Block-like
    /// expressions need no `;`; no tail expression, no `return`.
    fn parse_top_stmt(&mut self) -> Option<Stmt> {
        let span = self.peek_span();
        let (stmt, _) = self.parse_stmt()?;
        match stmt.kind {
            StmtKind::Return(_) => {
                self.error(
                    Code::TOP_LEVEL_RETURN,
                    span,
                    "top-level `return` is not allowed; use `process::exit(code)` \
                     or an explicit `fn main() -> i32`",
                );
                None
            }
            StmtKind::Trailing(expr) if !ends_stmt(&expr) => {
                let d = self.expected(&TokenKind::Semi, "`;` after top-level expression");
                self.diagnostics.push(d);
                None
            }
            // Kept `Trailing`: a block-like statement must still be `()`
            // (E0067), as in a function body.
            _ => Some(stmt),
        }
    }

    fn parse_inner_attrs(&mut self) -> Vec<Attribute> {
        let mut attrs = Vec::new();
        loop {
            self.doc_attrs(true, &mut attrs);
            if !matches!(self.peek(), TokenKind::HashBangLBracket) {
                return attrs;
            }
            if let Some(attr) = self.parse_attr(true) {
                match &self.cfg {
                    Some(env) => crate::cfg::expand_cfg_attr(env, attr, &mut attrs),
                    None => attrs.push(attr),
                }
            }
        }
    }

    /// Whether the build's cfg rules out what `attrs` are on.
    fn cfg_off(&self, attrs: &[Attribute]) -> bool {
        self.cfg.as_ref().is_some_and(|env| !env.attrs_pass(attrs))
    }

    /// Does `attrs`' cfg keep this member or statement? A
    /// dropped one's bad predicate is reported here, a kept one's with it.
    fn cfg_keeps(&mut self, attrs: &[Attribute]) -> bool {
        if !self.cfg_off(attrs) {
            return true;
        }
        for a in attrs.iter().filter(|a| a.path == ["cfg"]) {
            if let Some(m) = crate::cfg::attr_problem(a) {
                let d = RokDiagnostic::error(Code::INVALID_CFG, a.span, m);
                self.diagnostics.push(d.with_file(self.file.clone()));
            }
        }
        false
    }

    /// Whether outer attributes start here and a statement, not an
    /// item, follows them.
    fn at_attributed_stmt(&mut self) -> bool {
        let save = self.pos;
        self.pos = self.past_attrs(save);
        let stmt = self.pos != save && !self.at_item_start();
        self.pos = save;
        stmt
    }

    /// The index after the `#[..]` attributes starting at token `i`.
    fn past_attrs(&self, mut i: usize) -> usize {
        while matches!(
            self.tokens.get(i).map(|t| &t.kind),
            Some(TokenKind::HashLBracket)
        ) {
            let mut depth = 0usize;
            while let Some(t) = self.tokens.get(i) {
                i += 1;
                match t.kind {
                    TokenKind::HashLBracket | TokenKind::LBracket => depth += 1,
                    TokenKind::RBracket => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
            }
        }
        i
    }

    /// Outer attributes where only `#[cfg]` may be: a
    /// statement, arm, literal field, parameter or tuple field (`what`).
    fn cfg_only_attrs(&mut self, what: &'static str) -> Vec<Attribute> {
        let attrs = self.parse_outer_attrs();
        // A nested `fn` is a statement here; like a local item, it may
        // have a doc comment (not part of the package's API).
        let item = what == "a statement" && self.at_item_start();
        for a in attrs.iter().filter(|a| a.name() == "doc" && !item) {
            // As in Rust, a parameter's is an error.
            self.docs_dropped
                .insert(a.span.start, (what, what == "a parameter"));
        }
        for a in &attrs {
            let why = match a.name() {
                "doc" => None,
                "cfg" => crate::cfg::attr_problem(a).map(|m| (Code::INVALID_CFG, m)),
                _ => Some((
                    Code::MISPLACED_ATTRIBUTE,
                    format!(
                        "`#[{}]` cannot be attached to {what}; only `#[cfg]` can",
                        a.path.join("::")
                    ),
                )),
            };
            if let Some((code, m)) = why {
                let d = RokDiagnostic::error(code, a.span, m);
                self.diagnostics.push(d.with_file(self.file.clone()));
            }
        }
        attrs
    }

    /// Do `attrs` drop what was just parsed? Statements and
    /// nested fns queued since `marks` go with it.
    fn cfg_drops(&mut self, attrs: &[Attribute], (pending, lifted): (usize, usize)) -> bool {
        if !self.cfg_off(attrs) {
            return false;
        }
        self.pending_stmts.truncate(pending);
        for it in self.lifted.drain(lifted..) {
            if let Some(n) = it.name() {
                self.lifted_names.remove(n);
            }
        }
        true
    }

    fn parse_outer_attrs(&mut self) -> Vec<Attribute> {
        let mut attrs = Vec::new();
        loop {
            self.doc_attrs(false, &mut attrs);
            if !matches!(self.peek(), TokenKind::HashLBracket) {
                return attrs;
            }
            if let Some(attr) = self.parse_attr(false) {
                match &self.cfg {
                    Some(env) => crate::cfg::expand_cfg_attr(env, attr, &mut attrs),
                    None => attrs.push(attr),
                }
            }
        }
    }

    /// Report doc comments that document nothing, as Rust does: errors for
    /// a misplaced inner one, an outer one before `}` / EOF or on a
    /// parameter; the `unused_doc_comments` warning for one on a non-item.
    /// Skipped after a syntax error.
    fn misplaced_docs(&mut self) {
        if self.diagnostics.iter().any(RokDiagnostic::is_error) {
            return;
        }
        let plain = "use `//` for a plain comment";
        let mut out = Vec::new();
        for (i, t) in self.tokens.iter().enumerate() {
            if t.docs.is_empty() || self.unparsed.iter().any(|r| r.contains(&i)) {
                continue;
            }
            let closing = matches!(
                t.kind,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket | TokenKind::Eof
            );
            for d in &t.docs {
                let used = self.docs_used.contains(&(i, d.inner));
                let dropped = self.docs_dropped.get(&d.span.start);
                let (msg, error) = match (d.inner, used, dropped) {
                    (true, true, _) | (false, true, None) => continue,
                    (true, false, _) => (
                        "expected outer doc comment: an inner doc comment (`//!`, `/*!`) \
                         documents the file, module or function body it starts; use `///` for \
                         the next item, or `//` for a plain comment"
                            .to_owned(),
                        true,
                    ),
                    (false, _, Some(&(what, true))) => (
                        format!("a doc comment cannot document {what}; {plain}"),
                        true,
                    ),
                    (false, _, Some(&(what, false))) => (
                        format!("unused doc comment: {what} is not documented; {plain}"),
                        false,
                    ),
                    (false, false, None) if closing => (
                        format!(
                            "doc comment documents nothing: it must come before an item; {plain}"
                        ),
                        true,
                    ),
                    (false, false, None) => (
                        format!("unused doc comment: only items are documented; {plain}"),
                        false,
                    ),
                };
                let span = Span {
                    file: t.span.file,
                    ..d.span
                };
                out.push(if error {
                    RokDiagnostic::error(Code::MISPLACED_DOC_COMMENT, span, msg)
                } else {
                    RokDiagnostic::warning(Code::MISPLACED_DOC_COMMENT, span, msg)
                        .with_lint("unused_doc_comments")
                });
            }
        }
        let file = self.file.clone();
        self.diagnostics
            .extend(out.into_iter().map(|d| d.with_file(file.clone())));
    }

    /// The next token's doc comments as `#[doc = ".."]` attributes.
    fn doc_attrs(&mut self, inner: bool, out: &mut Vec<Attribute>) {
        self.doc_attrs_at(self.pos, inner, out);
    }

    fn doc_attrs_at(&mut self, at: usize, inner: bool, out: &mut Vec<Attribute>) {
        if self.pending.is_some() {
            return;
        }
        let Some(t) = self.tokens.get(at) else {
            return;
        };
        if t.docs.iter().any(|d| d.inner == inner) {
            self.docs_used.insert((at, inner));
        }
        out.extend(t.docs.iter().filter(|d| d.inner == inner).map(|d| {
            Attribute {
                path: vec!["doc".to_owned()],
                body: AttrBody::Value(Literal::Str(
                    d.text
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"")
                        .replace('\n', "\\n"),
                )),
                inner,
                span: Span {
                    file: t.span.file,
                    ..d.span
                },
            }
        }));
    }

    fn parse_attr(&mut self, inner: bool) -> Option<Attribute> {
        let start = self.peek_span();
        self.bump(); // `#[` or `#![`
        let path = self.parse_attr_path()?;
        let errors = self.diagnostics.len();
        let body = if path == ["literal"] && matches!(self.peek(), TokenKind::LParen) {
            self.parse_literal_attr_body()
        } else {
            self.parse_attr_body()
        };
        if self.diagnostics.len() == errors {
            self.expect(&TokenKind::RBracket, "`]`");
        } else {
            // A malformed body: on past its `]`, reporting nothing more.
            let mut depth = 0usize;
            loop {
                match self.peek() {
                    TokenKind::Eof => break,
                    TokenKind::RBracket if depth == 0 => {
                        self.bump();
                        break;
                    }
                    TokenKind::LBracket
                    | TokenKind::HashLBracket
                    | TokenKind::LParen
                    | TokenKind::LBrace => depth += 1,
                    TokenKind::RBracket | TokenKind::RParen | TokenKind::RBrace => {
                        depth = depth.saturating_sub(1);
                    }
                    _ => {}
                }
                self.bump();
            }
        }
        Some(Attribute {
            path,
            body,
            inner,
            span: start,
        })
    }

    /// Path inside an attribute body: `name` or `a::b::c`.
    fn parse_attr_path(&mut self) -> Option<Vec<String>> {
        let mut segments = Vec::new();
        loop {
            let name = match self.peek().clone() {
                TokenKind::Ident(n) => n,
                other => {
                    let s = self.peek_span();
                    self.error(
                        Code::INVALID_ATTRIBUTE,
                        s,
                        format!("expected attribute name, found {}", other.describe()),
                    );
                    return None;
                }
            };
            self.bump();
            segments.push(name);
            if !self.eat(&TokenKind::ColonColon) {
                break;
            }
        }
        Some(segments)
    }

    fn parse_attr_body(&mut self) -> AttrBody {
        match self.peek() {
            TokenKind::LParen => {
                self.bump();
                let mut args = Vec::new();
                while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
                    if let Some(arg) = self.parse_attr_arg() {
                        args.push(arg);
                    } else {
                        // recover until `,` or `)`
                        while !matches!(
                            self.peek(),
                            TokenKind::Comma | TokenKind::RParen | TokenKind::Eof
                        ) {
                            self.bump();
                        }
                    }
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RParen, "`)`");
                AttrBody::Args(args)
            }
            TokenKind::Eq => {
                self.bump();
                self.parse_attr_literal()
                    .map_or(AttrBody::Empty, AttrBody::Value)
            }
            _ => AttrBody::Empty,
        }
    }

    /// One argument, or a range of two (`a..=b`, `..b`, `a..`).
    fn parse_attr_arg(&mut self) -> Option<AttrArg> {
        let is_range = |k: &TokenKind| matches!(k, TokenKind::DotDot | TokenKind::DotDotEq);
        let lo = if is_range(self.peek()) {
            None
        } else {
            Some(self.parse_attr_atom()?)
        };
        if !is_range(self.peek()) {
            return lo;
        }
        let inclusive = matches!(self.peek(), TokenKind::DotDotEq);
        self.bump();
        let hi = if matches!(self.peek(), TokenKind::Comma | TokenKind::RParen) {
            None
        } else {
            Some(Box::new(self.parse_attr_atom()?))
        };
        Some(AttrArg::Range {
            lo: lo.map(Box::new),
            hi,
            inclusive,
        })
    }

    fn parse_attr_atom(&mut self) -> Option<AttrArg> {
        let ident_name = match self.peek().clone() {
            TokenKind::Ident(n) => Some(n),
            _ => None,
        };
        if let Some(name) = ident_name {
            // `key = value` form when an identifier is followed by `=`.
            if matches!(self.peek_next(), Some(TokenKind::Eq)) {
                self.bump(); // ident
                self.bump(); // `=`
                let lit = self.parse_attr_literal()?;
                return Some(AttrArg::KeyValue(name, lit));
            }
            if matches!(
                self.peek_next(),
                Some(TokenKind::LParen | TokenKind::ColonColon)
            ) {
                let path = self.parse_attr_path()?;
                let body = self.parse_attr_body();
                return Some(AttrArg::Nested {
                    path,
                    body: Box::new(body),
                });
            }
            // Plain identifier (no `=`, no `(`): treat as a Word arg.
            self.bump();
            return Some(AttrArg::Word(name));
        }
        if let Some(lit) = self.try_parse_attr_literal() {
            return Some(AttrArg::Lit(lit));
        }
        let s = self.peek_span();
        self.error(
            Code::INVALID_ATTRIBUTE,
            s,
            format!(
                "unexpected {} in attribute arguments",
                self.peek().describe()
            ),
        );
        None
    }

    fn parse_attr_literal(&mut self) -> Option<Literal> {
        self.try_parse_attr_literal().or_else(|| {
            let s = self.peek_span();
            self.error(
                Code::INVALID_ATTRIBUTE,
                s,
                format!("expected literal, found {}", self.peek().describe()),
            );
            None
        })
    }

    fn try_parse_attr_literal(&mut self) -> Option<Literal> {
        // `-1` style negative numeric literals are accepted as a single
        // literal token-pair (Rust attribute compatibility).
        let neg = self.eat(&TokenKind::Minus);
        let lit = match self.peek().clone() {
            TokenKind::IntLit(s) => {
                self.bump();
                Literal::Int(if neg { format!("-{s}") } else { s })
            }
            TokenKind::FloatLit(s) => {
                self.bump();
                Literal::Float(if neg { format!("-{s}") } else { s })
            }
            TokenKind::StrLit(s) => {
                self.bump();
                Literal::Str(s)
            }
            TokenKind::RawStrLit(s) => {
                self.bump();
                Literal::RawStr(s)
            }
            TokenKind::CharLit(s) => {
                self.bump();
                Literal::Char(s)
            }
            TokenKind::CCharLit(s) => {
                self.bump();
                Literal::CChar(s)
            }
            TokenKind::True => {
                self.bump();
                Literal::Bool(true)
            }
            TokenKind::False => {
                self.bump();
                Literal::Bool(false)
            }
            TokenKind::None => {
                self.bump();
                Literal::None
            }
            _ => return None,
        };
        Some(lit)
    }

    /// A `fn` type behind `unsafe` and / or `extern "C"` /
    /// `extern "system"` (a C function pointer).
    fn parse_marked_fn_type(&mut self, start: Span) -> Option<TyKind> {
        let unsafe_fn = self.eat(&TokenKind::Unsafe);
        let mut system = false;
        if !unsafe_fn {
            self.bump(); // `extern`
            if let TokenKind::StrLit(abi) = self.peek().clone() {
                system = abi == "system";
                if abi != "C" && !system {
                    self.error(
                        Code::INVALID_ABI,
                        start,
                        format!("unsupported ABI `\"{abi}\"` (only \"C\" or \"system\")"),
                    );
                }
            }
            self.bump(); // ABI string
        }
        let mut t = self.parse_type_atom_inner()?;
        if let TyKind::Fn(_, _, bounds) = &mut t.kind {
            if unsafe_fn {
                bounds.unsafe_fn = true;
            } else {
                bounds.c_abi = true;
                bounds.system = system;
            }
        }
        Some(t.kind)
    }

    /// `extern "<abi>" fn` starting `k` tokens ahead (a C fn pointer type).
    fn at_c_fn_type(&self, k: usize) -> bool {
        let at = |i: usize| self.tokens.get(self.pos + k + i).map(|t| &t.kind);
        matches!(at(0), Some(TokenKind::Ident(n)) if n == "extern")
            && matches!(at(1), Some(TokenKind::StrLit(_)))
            && matches!(at(2), Some(TokenKind::Fn))
    }

    /// The current token is the contextual keyword `w` (an identifier
    /// token).
    fn at_kw(&self, w: &str) -> bool {
        matches!(self.peek(), TokenKind::Ident(n) if n == w) && !self.peek_raw()
    }

    /// The current token is an identifier written `r#name`.
    fn peek_raw(&self) -> bool {
        self.pending.is_none() && self.tokens.get(self.pos).is_some_and(|t| t.raw)
    }

    fn eat_kw(&mut self, w: &str) -> bool {
        let hit = self.at_kw(w);
        if hit {
            self.bump();
        }
        hit
    }

    fn expect_kw(&mut self, w: &str) -> Option<()> {
        if self.eat_kw(w) {
            return Some(());
        }
        let s = self.peek_span();
        self.error(
            Code::SYNTAX,
            s,
            format!("expected `{w}`, found {}", self.peek().describe()),
        );
        None
    }

    /// A member / field / argument-label name: an identifier or any
    /// reserved keyword but `self` (`p.type`, `f(match: 1)`).
    fn peek_name(&self) -> Option<String> {
        match self.peek() {
            TokenKind::Ident(n) => Some(n.clone()),
            k => keyword_str(k).filter(|w| *w != "self").map(str::to_owned),
        }
    }

    fn peek_next(&self) -> Option<TokenKind> {
        self.tokens.get(self.pos + 1).map(|t| t.kind.clone())
    }

    fn parse_item(&mut self) -> Option<Item> {
        let lifted = self.lifted.len();
        let item = self.parse_item_lifting();
        let nested: Vec<Item> = self.lifted.drain(lifted..).collect();
        self.pending_items.extend(nested);
        item
    }

    fn parse_item_lifting(&mut self) -> Option<Item> {
        let span = self.peek_span();
        let from = self.pos;
        // A `name!(..)` macro at item position expands to items.
        if let Some(items) = self.macro_call_items() {
            let mut it = items.into_iter();
            let first = it.next();
            self.pending_items.extend(it);
            self.vanished = first.is_none();
            return first;
        }
        let item = self.parse_item_inner()?;
        // `pkg` / `sup` / `rok` are path roots and `any` is the any
        // type, so no module or type may take those names.
        let name = match &item {
            Item::Struct(d) => Some(&d.name),
            Item::Enum(d) => Some(&d.name),
            Item::Union(d) => Some(&d.name),
            Item::Trait(d) => Some(&d.name),
            Item::TypeAlias(d) => Some(&d.name),
            Item::Mod(d) => Some(&d.name),
            _ => None,
        };
        if let Some(n) = name.filter(|n| matches!(n.as_str(), "pkg" | "sup" | "rok" | "any")) {
            let what = if matches!(item, Item::Mod(_)) {
                "module"
            } else {
                "type"
            };
            self.error(
                Code::RESERVED_NAME,
                span,
                format!("`{n}` cannot name a {what}: it is reserved as a path root / type keyword"),
            );
        }
        // Cfg runs before meta expansion; a cfg-false item
        // expands nothing and its cfg-false members are already gone.
        if self.cfg_off(item.attrs()) {
            return Some(item);
        }
        if !self.meta.is_empty() {
            // A user attribute replaces the item with its
            // expansion (which expands again).
            if let Some(items) = self.expand_user_attribute(&item, from) {
                let mut it = items.into_iter();
                let first = it.next();
                self.pending_items.extend(it);
                self.vanished = first.is_none();
                return first;
            }
        }
        let (item, user_derived) = if self.meta.is_empty() {
            (item, Vec::new())
        } else {
            self.expand_user_derives(item, from)
        };
        let (derived, diags) = crate::derive::derive_items(&item, &self.file);
        self.pending_items.extend(derived);
        self.pending_items.extend(user_derived);
        self.diagnostics.extend(diags);
        Some(item)
    }

    fn parse_item_inner(&mut self) -> Option<Item> {
        let (attrs, vis) = self.parse_attrs_vis();
        if self.at_union_item() {
            return self.parse_union(vis, attrs).map(Item::Union);
        }
        // Contextual item keywords: `extern "C"`, `const NAME` /
        // `const fn`, `static [mut] NAME`, `type Name`.
        if self.at_wit_item() {
            return self.parse_wit_item(attrs).map(Item::Extern);
        }
        if self.at_kw("extern") {
            // `extern "C" fn f(..) { .. }`: a rok function
            // with an external ABI, marked with it for HIR.
            let after_abi = self.tokens.get(self.pos + 2).map(|t| &t.kind);
            if let (Some(TokenKind::StrLit(abi)), Some(TokenKind::Fn | TokenKind::Unsafe)) =
                (self.peek_next(), after_abi)
            {
                let span = self.peek_span();
                self.bump(); // `extern`
                self.bump(); // ABI string
                let mut attrs = attrs;
                attrs.push(Attribute {
                    path: vec![rokc_ast::EXTERN_FN_ATTR.to_owned()],
                    body: rokc_ast::AttrBody::Value(rokc_ast::Literal::Str(abi)),
                    inner: false,
                    span,
                });
                return self.parse_fn(vis, attrs).map(Item::Fn);
            }
            return self.parse_extern_block(attrs).map(Item::Extern);
        }
        if self.at_kw("const") {
            if self.at_const_fn() {
                return self.parse_const_fn(vis, attrs).map(Item::Fn);
            }
            return self.parse_const(vis, attrs).map(Item::Const);
        }
        if self.at_kw("static") {
            return self.parse_static(vis, attrs).map(Item::Static);
        }
        if self.at_kw("type") {
            return self.parse_type_alias(vis, attrs).map(Item::TypeAlias);
        }
        match self.peek() {
            TokenKind::Unsafe if self.peek_next() == Some(TokenKind::Trait) => {
                self.bump();
                self.parse_trait(vis, attrs).map(|t| {
                    Item::Trait(TraitDef {
                        is_unsafe: true,
                        ..t
                    })
                })
            }
            TokenKind::Unsafe if self.peek_next() == Some(TokenKind::Impl) => {
                self.bump();
                self.parse_impl(vis, attrs).map(|i| {
                    Item::Impl(ImplDef {
                        is_unsafe: true,
                        ..i
                    })
                })
            }
            TokenKind::Fn | TokenKind::Unsafe | TokenKind::Async | TokenKind::Yield => {
                self.parse_fn(vis, attrs).map(Item::Fn)
            }
            TokenKind::Trait => self.parse_trait(vis, attrs).map(Item::Trait),
            TokenKind::Struct => self.parse_struct(vis, attrs).map(Item::Struct),
            TokenKind::Enum => self.parse_enum(vis, attrs).map(Item::Enum),
            TokenKind::Impl => self.parse_impl(vis, attrs).map(Item::Impl),
            TokenKind::Mod => self.parse_mod(vis, attrs).map(Item::Mod),
            TokenKind::Use => self.parse_use(vis, attrs).map(Item::Use),
            _ => {
                let span = self.peek_span();
                self.error(Code::SYNTAX, span, "expected an item");
                None
            }
        }
    }

    /// `type NAME[<G>] = ty;`
    fn parse_type_alias(
        &mut self,
        vis: Visibility,
        attrs: Vec<Attribute>,
    ) -> Option<rokc_ast::TypeAliasDef> {
        let start = self.peek_span();
        self.bump(); // contextual `type`
        let name = if let TokenKind::Ident(n) = self.peek().clone() {
            self.bump();
            n
        } else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected type alias name, found {}", self.peek().describe()),
            );
            return None;
        };
        let generics = self.parse_generic_params();
        let mut where_clause = self.parse_where_clause();
        self.expect(&TokenKind::Eq, "`=`")?;
        let ty = self.parse_type()?;
        // `type A<T> = B<T> where T: X;` reads the same.
        where_clause.extend(self.parse_where_clause());
        self.expect(&TokenKind::Semi, "`;`")?;
        Some(rokc_ast::TypeAliasDef {
            vis,
            attrs,
            name,
            generics,
            ty,
            where_clause,
            span: start,
        })
    }

    /// `-> T throws` (Swift, Java order): `throws` after the return type
    /// `ret` of a signature from `start` to its `)` at `rparen`.
    fn throws_after_ret(
        &mut self,
        throws: ThrowsKind,
        ret: Option<&Ty>,
        start: Span,
        rparen: Option<Span>,
    ) -> ThrowsKind {
        let Some(ret) = ret.filter(|_| matches!(throws, ThrowsKind::None) && self.at_kw("throws"))
        else {
            return throws;
        };
        let at = self.peek_span();
        let ty = self.text_in(ret.span);
        let head = self.text_in(Span {
            end: rparen.map_or(start.end, |p| p.end),
            ..start
        });
        let throws = self.parse_throws();
        let clause = self.text_in(Span {
            end: self.prev_span().map_or(at.end, |p| p.end),
            ..at
        });
        self.error_help(
            Code::MODIFIER_ORDER,
            at,
            "`throws` goes before the return type",
            format!("write `{head} {clause} -> {ty}`"),
        );
        throws
    }

    /// `const NAME: T = expr;`
    fn parse_const(
        &mut self,
        vis: Visibility,
        attrs: Vec<Attribute>,
    ) -> Option<rokc_ast::ConstDef> {
        let start = self.peek_span();
        self.bump(); // contextual `const`
        let name = if let TokenKind::Ident(n) = self.peek().clone() {
            self.bump();
            n
        } else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected const name, found {}", self.peek().describe()),
            );
            return None;
        };
        self.expect(&TokenKind::Colon, "`:`")?;
        let ty = self.parse_type()?;
        self.expect(&TokenKind::Eq, "`=`")?;
        let value = self.parse_expr()?;
        self.expect(&TokenKind::Semi, "`;`")?;
        Some(rokc_ast::ConstDef {
            vis,
            attrs,
            name,
            ty,
            value,
            span: start,
        })
    }

    /// `static [mut] NAME: T = expr;` - a module-level global.
    fn parse_static(
        &mut self,
        vis: Visibility,
        attrs: Vec<Attribute>,
    ) -> Option<rokc_ast::StaticDef> {
        let start = self.peek_span();
        self.bump(); // contextual `static`
        let is_mut = matches!(self.peek_next(), Some(TokenKind::Ident(_))) && self.eat_kw("mut");
        // `static mut` only inside `extern "C" { .. }` (FFI),
        // which doesn't come through here.
        if is_mut {
            let mut_span = self.peek_span();
            self.error_help(
                Code::MISPLACED_ITEM,
                mut_span,
                "`static mut` is only allowed inside `extern \"C\" { ... }` (FFI)",
                "a static is shared by every thread: hold the value in an `Atomic` or a \
                 `Mutex`, as in `static N: Atomic<i32> = Atomic::new(0);`",
            );
        }
        let name = if let TokenKind::Ident(n) = self.peek().clone() {
            self.bump();
            n
        } else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected static name, found {}", self.peek().describe()),
            );
            return None;
        };
        self.expect(&TokenKind::Colon, "`:`")?;
        let ty = self.parse_type()?;
        let value = if self.eat(&TokenKind::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(&TokenKind::Semi, "`;`")?;
        Some(rokc_ast::StaticDef {
            vis,
            attrs,
            name,
            ty,
            value,
            is_mut,
            span: start,
        })
    }

    fn parse_struct(&mut self, vis: Visibility, attrs: Vec<Attribute>) -> Option<StructDef> {
        let start = self.peek_span();
        self.expect(&TokenKind::Struct, "`struct`")?;
        let name = if let TokenKind::Ident(n) = self.peek().clone() {
            self.bump();
            n
        } else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected struct name, found {}", self.peek().describe()),
            );
            return None;
        };
        let generics = if matches!(self.peek(), TokenKind::Lt) {
            self.parse_generic_params()
        } else {
            Generics::default()
        };
        let mut where_clause = self.parse_where_clause();

        let kind = match self.peek().clone() {
            TokenKind::Semi => {
                self.bump();
                StructKind::Unit
            }
            TokenKind::LParen => {
                self.bump();
                let mut fields = Vec::new();
                while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
                    let (f_attrs, f_vis) = self.parse_attrs_vis();
                    let span = self.peek_span();
                    let ty = self.parse_type()?;
                    if self.cfg_keeps(&f_attrs) {
                        fields.push(rokc_ast::TupleField {
                            vis: f_vis,
                            attrs: f_attrs,
                            ty,
                            span,
                        });
                    }
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RParen, "`)`")?;
                // `struct P<T>(T) where T: X;` reads the same.
                where_clause.extend(self.parse_where_clause());
                self.expect(&TokenKind::Semi, "`;`")?;
                StructKind::Tuple(fields)
            }
            _ => {
                let fields = self.parse_named_fields(true)?;
                StructKind::Named(fields)
            }
        };

        Some(StructDef {
            vis,
            attrs,
            name,
            generics,
            kind,
            where_clause,
            span: start,
        })
    }

    /// `union Name<G> { f: T; g: U; }` - fields share storage.
    fn parse_union(&mut self, vis: Visibility, attrs: Vec<Attribute>) -> Option<UnionDef> {
        let start = self.peek_span();
        self.bump(); // contextual `union`
        let name = if let TokenKind::Ident(n) = self.peek().clone() {
            self.bump();
            n
        } else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected union name, found {}", self.peek().describe()),
            );
            return None;
        };
        let generics = if matches!(self.peek(), TokenKind::Lt) {
            self.parse_generic_params()
        } else {
            Generics::default()
        };
        let where_clause = self.parse_where_clause();
        let fields = self.parse_named_fields(false)?;
        Some(UnionDef {
            vis,
            attrs,
            name,
            generics,
            fields,
            where_clause,
            span: start,
        })
    }

    /// `{ [attrs] [vis] name: T [= default]; ... }` - struct, union and
    /// enum-variant bodies. Defaults only where `allow_default`.
    fn parse_named_fields(&mut self, allow_default: bool) -> Option<Vec<NamedField>> {
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut fields = Vec::new();
        let mut comma_reported = false;
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            let (attrs, vis) = self.parse_attrs_vis();
            let span = self.peek_span();
            let Some(name) = self.peek_name() else {
                self.error(
                    Code::SYNTAX,
                    span,
                    format!("expected field name, found {}", self.peek().describe()),
                );
                return None;
            };
            self.bump_name();
            self.expect(&TokenKind::Colon, "`:`")?;
            let ty = self.parse_type()?;
            let default = if allow_default && self.eat(&TokenKind::Eq) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            if self.cfg_keeps(&attrs) {
                fields.push(NamedField {
                    vis,
                    attrs,
                    name,
                    ty,
                    default,
                    span,
                });
            }
            if self.eat(&TokenKind::Semi) {
                continue;
            }
            // `x: i32, y: i32` (Rust, Go): fields end with `;`.
            if matches!(self.peek(), TokenKind::Comma) {
                let at = self.peek_span();
                if !comma_reported {
                    let field = fields.last().map_or_else(String::new, |f: &NamedField| {
                        format!("{}: {};", f.name, self.text_in(f.ty.span))
                    });
                    self.error_help(
                        Code::SYNTAX,
                        at,
                        "expected `;` after a struct field, found `,`",
                        format!("struct fields end with `;`: `{field}`"),
                    );
                    comma_reported = true;
                }
                self.bump();
                continue;
            }
            break;
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some(fields)
    }

    /// `impl [Trait for] Type { .. }`: an inherent or trait impl.
    fn parse_impl(&mut self, vis: Visibility, attrs: Vec<Attribute>) -> Option<ImplDef> {
        let start = self.peek_span();
        self.expect(&TokenKind::Impl, "`impl`")?;
        // `<` opens generics only before a parameter (`<>`, `<T>`, `<T:`,
        // `<T,`, `<T =`, `<const N`); else it starts a projection
        // (`impl <A as P>::Out { .. }`).
        let tok = |i: usize| self.tokens.get(self.pos + i).map(|t| &t.kind);
        let params = match (tok(1), tok(2)) {
            (Some(TokenKind::Gt), _)
            | (
                Some(TokenKind::Ident(_)),
                Some(TokenKind::Colon | TokenKind::Comma | TokenKind::Gt | TokenKind::Eq),
            ) => true,
            (Some(TokenKind::Ident(k)), Some(TokenKind::Ident(_))) => k == "const",
            _ => false,
        };
        let generics = if matches!(self.peek(), TokenKind::Lt) && !params {
            Generics::default()
        } else {
            self.parse_generic_params()
        };
        // `impl !Send for T {}` - auto-trait opt-out.
        let negative = self.eat(&TokenKind::Bang);
        let first_ty = self.parse_type()?;
        if negative && !matches!(self.peek(), TokenKind::For) {
            let s = self.peek_span();
            self.error(Code::SYNTAX, s, "`impl !Trait` needs `for Type`");
        }
        let (trait_path, self_ty) = if self.eat(&TokenKind::For) {
            // `impl TRAIT for SELF` - the first parsed type was the trait.
            let trait_path = match &first_ty.kind {
                TyKind::Path(p) => Some(p.clone()),
                _ => None,
            };
            let self_ty = self.parse_type()?;
            (trait_path, self_ty)
        } else {
            (None, first_ty)
        };
        let where_clause = self.parse_where_clause();
        self.expect(&TokenKind::LBrace, "`{`")?;
        let owner = match &self_ty.kind {
            TyKind::Path(p) => p.segments.last().map(|s| s.name.clone()),
            _ => None,
        };
        let outer_owner = std::mem::replace(&mut self.fn_owner, owner);
        let items = self.parse_impl_items();
        self.fn_owner = outer_owner;
        if trait_path.is_some() {
            for it in items.iter().flatten() {
                let (vis, at) = match it {
                    ImplItem::Method(m) => (m.vis, m.span),
                    ImplItem::Const(c) => (c.vis, c.span),
                    ImplItem::AssocType { .. } => continue,
                };
                if vis != Visibility::Private {
                    self.error_help(
                        Code::MISPLACED_VISIBILITY,
                        at,
                        "a trait impl's item has no visibility of its own: it is as visible \
                         as the trait",
                        "remove `pub`",
                    );
                }
            }
        }
        let mut attrs = attrs;
        attrs.append(&mut self.cfg_off);
        let items = items?;
        Some(ImplDef {
            vis,
            attrs,
            generics,
            trait_path,
            is_unsafe: false,
            negative,
            self_ty,
            where_clause,
            items,
            module: String::new(),
            span: start,
        })
    }

    /// An impl's items through its `}`.
    fn parse_impl_items(&mut self) -> Option<Vec<ImplItem>> {
        let mut items = Vec::new();
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            let attrs_from = self.pos;
            let (item_attrs, item_vis) = self.parse_attrs_vis();
            let (on, before) = (self.cfg_keeps(&item_attrs), items.len());
            match self.peek() {
                TokenKind::Fn | TokenKind::Unsafe | TokenKind::Async | TokenKind::Yield => {
                    let from = self.pos;
                    if let Some(f) = self.parse_fn(item_vis, item_attrs) {
                        let fs = if on {
                            self.expand_method(f, attrs_from)
                        } else {
                            self.cfg_off.push(cfg_off_marker(&f));
                            Vec::new()
                        };
                        items.extend(fs.into_iter().map(ImplItem::Method));
                    } else {
                        self.recover(from, Recover::NestedItem);
                    }
                }
                // `impl_item ::= fn_decl | "type" ident "=" ty ";" | const_decl`.
                // Associated-type concrete binding: `type Output = V;`.
                TokenKind::Ident(k) if k == "type" => {
                    let _ = (item_vis, item_attrs);
                    self.bump();
                    let assoc_name = if let TokenKind::Ident(n) = self.peek().clone() {
                        self.bump();
                        n
                    } else {
                        let s = self.peek_span();
                        self.error(Code::SYNTAX, s, "associated-type name expected");
                        String::new()
                    };
                    self.expect(&TokenKind::Eq, "`=`")?;
                    let aty = self.parse_type()?;
                    self.expect(&TokenKind::Semi, "`;`")?;
                    items.push(ImplItem::AssocType {
                        name: assoc_name,
                        ty: aty,
                    });
                }
                TokenKind::Ident(k) if k == "const" && self.at_const_fn() => {
                    let from = self.pos;
                    if let Some(f) = self.parse_const_fn(item_vis, item_attrs) {
                        let fs = if on {
                            self.expand_method(f, attrs_from)
                        } else {
                            Vec::new()
                        };
                        items.extend(fs.into_iter().map(ImplItem::Method));
                    } else {
                        self.recover(from, Recover::NestedItem);
                    }
                }
                // `impl_item ::= .. | const_decl`.
                TokenKind::Ident(k) if k == "const" => {
                    let from = self.pos;
                    match self.parse_const(item_vis, item_attrs) {
                        Some(c) => items.push(ImplItem::Const(c)),
                        None => self.recover(from, Recover::NestedItem),
                    }
                }
                _ => {
                    let s = self.peek_span();
                    let (code, msg) = if self.at_kw("static") && self.at_ctx_item() {
                        (
                            Code::MISPLACED_ITEM,
                            "a `static` is declared only at module level, not in an `impl`; \
                             an associated `const` goes there"
                                .to_owned(),
                        )
                    } else {
                        (
                            Code::SYNTAX,
                            format!("expected impl item, found {}", self.peek().describe()),
                        )
                    };
                    self.error(code, s, msg);
                    let from = self.pos;
                    self.recover(from, Recover::NestedItem);
                }
            }
            if !on {
                items.truncate(before);
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some(items)
    }

    /// A method with a user attribute becomes its expansion,
    /// which must be methods.
    fn expand_method(&mut self, f: FnDef, from: usize) -> Vec<FnDef> {
        if self.meta.is_empty() {
            return vec![f];
        }
        let span = f.span;
        let item = Item::Fn(f);
        let Some(items) = self.expand_user_attribute(&item, from) else {
            let Item::Fn(f) = item else { unreachable!() };
            return vec![f];
        };
        let mut out = Vec::new();
        for i in items {
            match i {
                Item::Fn(f) => out.push(f),
                _ => self.error(
                    Code::INVALID_META_BLOCK,
                    span,
                    "an attribute on a method must expand to methods only",
                ),
            }
        }
        out
    }

    /// `enum Name { V, V(T), V { x: T }, .. }`.
    fn parse_enum(&mut self, vis: Visibility, attrs: Vec<Attribute>) -> Option<EnumDef> {
        let start = self.peek_span();
        self.expect(&TokenKind::Enum, "`enum`")?;
        let name = if let TokenKind::Ident(n) = self.peek().clone() {
            self.bump();
            n
        } else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected enum name, found {}", self.peek().describe()),
            );
            return None;
        };
        let generics = if matches!(self.peek(), TokenKind::Lt) {
            self.parse_generic_params()
        } else {
            Generics::default()
        };
        let where_clause = self.parse_where_clause();
        self.expect(&TokenKind::LBrace, "`{`")?;

        let mut variants = Vec::new();
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            let v_attrs = self.parse_outer_attrs();
            // Variants take the enum's visibility.
            if matches!(self.peek(), TokenKind::Pub) {
                let s = self.peek_span();
                self.error(
                    Code::MISPLACED_VISIBILITY,
                    s,
                    "enum variants take the enum's visibility; remove the visibility \
                     keyword",
                );
                self.parse_visibility();
            }
            let v_span = self.peek_span();
            let v_name = if let TokenKind::Ident(n) = self.peek().clone() {
                self.bump();
                n
            } else {
                let s = self.peek_span();
                self.error(
                    Code::SYNTAX,
                    s,
                    format!("expected variant name, found {}", self.peek().describe()),
                );
                return None;
            };
            let kind = match self.peek() {
                TokenKind::LParen => {
                    self.bump();
                    let mut tys = Vec::new();
                    while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
                        let f_attrs = self.cfg_only_attrs("a tuple variant field");
                        if let Some(t) = self.parse_type().filter(|_| !self.cfg_off(&f_attrs)) {
                            tys.push(t);
                        }
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RParen, "`)`")?;
                    EnumVariantKind::Tuple(tys)
                }
                TokenKind::LBrace => {
                    let fields = self.parse_named_fields(false)?;
                    EnumVariantKind::Named(fields)
                }
                TokenKind::Eq => {
                    self.bump();
                    let value = self.parse_expr()?;
                    EnumVariantKind::Value(value)
                }
                _ => EnumVariantKind::Unit,
            };
            if self.cfg_keeps(&v_attrs) {
                variants.push(EnumVariant {
                    attrs: v_attrs,
                    name: v_name,
                    kind,
                    span: v_span,
                });
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;

        Some(EnumDef {
            vis,
            attrs,
            name,
            generics,
            variants,
            where_clause,
            span: start,
        })
    }

    // `extern "ABI" { fn name(args) -> ret; ... }`; a missing ABI is
    // reported and read as "C".
    fn parse_extern_block(&mut self, attrs: Vec<Attribute>) -> Option<ExternBlock> {
        let start = self.peek_span();
        self.bump(); // contextual `extern`
        let abi = if let TokenKind::StrLit(s) = self.peek().clone() {
            self.bump();
            s
        } else {
            self.error(
                Code::INVALID_ABI,
                start,
                "an `extern` block names its ABI: `extern \"C\" { .. }`",
            );
            "C".to_owned()
        };
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut items = Vec::new();
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            let (item_attrs, item_vis) = self.parse_attrs_vis();
            let (on, before) = (self.cfg_keeps(&item_attrs), items.len());
            match self.peek() {
                TokenKind::Fn | TokenKind::Unsafe => {
                    let from = self.pos;
                    if let Some(f) = self.parse_fn(item_vis, item_attrs) {
                        items.push(ExternItem::Fn(f));
                    } else {
                        self.recover(from, Recover::NestedItem);
                    }
                }
                // `static mut? name: T;` defined outside.
                TokenKind::Ident(k) if k == "static" => {
                    let from = self.pos;
                    let span = self.peek_span();
                    self.bump();
                    let is_mut =
                        matches!(self.peek_next(), Some(TokenKind::Ident(_))) && self.eat_kw("mut");
                    let name = match self.peek().clone() {
                        TokenKind::Ident(n) => {
                            self.bump();
                            Some(n)
                        }
                        _ => None,
                    };
                    let ty = name.as_ref().and_then(|_| {
                        self.expect(&TokenKind::Colon, "`:`")?;
                        let ty = self.parse_type()?;
                        self.expect(&TokenKind::Semi, "`;`")?;
                        Some(ty)
                    });
                    if let (Some(name), Some(ty)) = (name, ty) {
                        items.push(ExternItem::Static(rokc_ast::StaticDef {
                            vis: item_vis,
                            attrs: item_attrs,
                            name,
                            ty,
                            value: None,
                            is_mut,
                            span,
                        }));
                    } else {
                        self.error(
                            Code::SYNTAX,
                            span,
                            "expected `static name: Type;` in `extern` block",
                        );
                        self.recover(from, Recover::NestedItem);
                    }
                }
                _ => {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!("expected extern item, found {}", self.peek().describe()),
                    );
                    let from = self.pos;
                    self.recover(from, Recover::NestedItem);
                }
            }
            if !on {
                items.truncate(before);
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some(ExternBlock {
            attrs,
            abi,
            items,
            span: start,
            wit: None,
        })
    }

    /// `ffi::wit::import` / `ffi::wit::export` then `{`.
    fn at_wit_item(&self) -> bool {
        let k = |n: usize| self.tokens.get(self.pos + n).map(|t| &t.kind);
        let word = |n: usize, w: &str| matches!(k(n), Some(TokenKind::Ident(x)) if x == w);
        word(0, "ffi")
            && k(1) == Some(&TokenKind::ColonColon)
            && word(2, "wit")
            && k(3) == Some(&TokenKind::ColonColon)
            && (word(4, "import") || word(4, "export"))
            && k(5) == Some(&TokenKind::LBrace)
    }

    /// `ffi::wit::import { "a.wit", "dir" }`: string paths, commas
    /// between.
    fn parse_wit_item(&mut self, attrs: Vec<Attribute>) -> Option<ExternBlock> {
        let start = self.peek_span();
        for _ in 0..4 {
            self.bump();
        }
        let export = matches!(self.peek(), TokenKind::Ident(w) if w == "export");
        self.bump();
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut paths = Vec::new();
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            let TokenKind::StrLit(p) = self.peek().clone() else {
                let s = self.peek_span();
                self.error(
                    Code::SYNTAX,
                    s,
                    format!(
                        "expected a `.wit` path string, found {}",
                        self.peek().describe()
                    ),
                );
                return None;
            };
            self.bump();
            paths.push(p);
            if !matches!(self.peek(), TokenKind::RBrace) {
                self.expect(&TokenKind::Comma, "`,`")?;
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some(ExternBlock {
            attrs,
            abi: "C".to_owned(),
            items: Vec::new(),
            span: start,
            wit: Some(rokc_ast::WitRef { export, paths }),
        })
    }

    fn parse_mod(&mut self, vis: Visibility, mut attrs: Vec<Attribute>) -> Option<ModDef> {
        let start = self.peek_span();
        self.expect(&TokenKind::Mod, "`mod`")?;
        let name = if let TokenKind::Ident(n) = self.peek().clone() {
            self.bump();
            n
        } else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected module name, found {}", self.peek().describe()),
            );
            return None;
        };
        let items = if matches!(self.peek(), TokenKind::LBrace) {
            self.bump();
            // `#![..]` / `//!` at the body's head are the module's.
            attrs.extend(self.parse_inner_attrs());
            let mut items = Vec::new();
            while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
                let from = self.pos;
                if self.meta_block().is_some() {
                    continue;
                }
                if !self.at_item_start() {
                    let span = self.peek_span();
                    self.error(
                        Code::MISPLACED_ITEM,
                        span,
                        format!(
                            "expected an item in `mod {name}`: module bodies are \
                             definitions-only; statements belong in `main.rok`"
                        ),
                    );
                    self.recover(from, Recover::NestedItem);
                } else if let Some(item) = self.parse_item() {
                    items.push(item);
                    items.append(&mut self.pending_items);
                } else if !std::mem::take(&mut self.vanished) {
                    self.recover(from, Recover::NestedItem);
                }
            }
            self.expect(&TokenKind::RBrace, "`}`")?;
            Some(items)
        } else {
            self.expect(&TokenKind::Semi, "`;` or `{` after module name")?;
            None
        };
        Some(ModDef {
            vis,
            attrs,
            name,
            items,
            span: start,
        })
    }

    /// `use path[::name | ::* | ::** | ::{ ... } | as alias];`
    fn parse_use(&mut self, vis: Visibility, attrs: Vec<Attribute>) -> Option<rokc_ast::UseDef> {
        let start = self.peek_span();
        self.expect(&TokenKind::Use, "`use`")?;
        let tree = self.parse_use_tree()?;
        self.expect(&TokenKind::Semi, "`;`")?;
        let Some(tree) = self.strip_meta_imports(tree, &[], vis != Visibility::Private, start)
        else {
            self.vanished = true;
            return None;
        };
        Some(rokc_ast::UseDef {
            vis,
            attrs,
            tree,
            span: start,
        })
    }

    fn parse_use_tree(&mut self) -> Option<rokc_ast::UseTree> {
        // Path prefix - every form starts with at least one segment.
        let prefix_start = self.peek_span();
        let mut segments: Vec<PathSegment> = Vec::new();
        loop {
            // `use` roots: an identifier (absolute), `pkg::` or `sup::`. No
            // `self::`: children resolve through the ancestor walk.
            let seg_name = if let TokenKind::Ident(n) = self.peek().clone() {
                self.bump();
                n
            } else {
                let s = self.peek_span();
                let help = match self.peek() {
                    TokenKind::SelfLower => Some(
                        "a `use` path has no `self`: name a child module directly \
                         (`use c::x;`), a module itself as `use a::b;`, this package's root \
                         as `pkg::`",
                    ),
                    TokenKind::ColonColon => Some(
                        "a `use` path starts with a name: `pkg::` for this package's root, \
                         a dependency's or `rok`'s name otherwise",
                    ),
                    _ => None,
                };
                let msg = format!("expected an import path, found {}", self.peek().describe());
                match help {
                    Some(h) => self.error_help(Code::SYNTAX, s, msg, h),
                    None => self.error(Code::SYNTAX, s, msg),
                }
                return None;
            };
            segments.push(PathSegment {
                name: seg_name,
                generics: Vec::new(),
            });
            if !matches!(self.peek(), TokenKind::ColonColon) {
                break;
            }
            // Commit only if `*`, `{` or a segment follows; else the `::`
            // is the caller's.
            let after_colons = self.peek_next();
            match after_colons {
                Some(TokenKind::Star | TokenKind::LBrace | TokenKind::Ident(_)) => {
                    self.bump(); // consume `::`
                    if matches!(self.peek(), TokenKind::Star | TokenKind::LBrace) {
                        break;
                    }
                }
                _ => break,
            }
        }
        let prefix = Path {
            segments,
            span: prefix_start,
            origin: None,
        };
        // Disambiguate the trailing form.
        let kind = match self.peek().clone() {
            TokenKind::Star => {
                self.bump();
                // `**` is two consecutive Stars - the second `*` follows
                // immediately when there's no space.
                let kind = if matches!(self.peek(), TokenKind::Star) {
                    self.bump();
                    rokc_ast::UseTreeKind::GlobRecursive
                } else {
                    rokc_ast::UseTreeKind::Glob
                };
                if matches!(self.peek(), TokenKind::As) {
                    let s = self.peek_span();
                    self.error_help(
                        Code::SYNTAX,
                        s,
                        "a glob import takes no alias",
                        "import the module under another name instead: `use a as x;`",
                    );
                    return None;
                }
                kind
            }
            TokenKind::LBrace => {
                self.bump();
                let mut children = Vec::new();
                while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
                    let child = self.parse_use_tree()?;
                    children.push(child);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RBrace, "`}`")?;
                rokc_ast::UseTreeKind::Group(children)
            }
            TokenKind::As => {
                self.bump();
                let alias = if let TokenKind::Ident(n) = self.peek().clone() {
                    self.bump();
                    n
                } else {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!(
                            "expected alias name after `as`, found {}",
                            self.peek().describe()
                        ),
                    );
                    return None;
                };
                rokc_ast::UseTreeKind::Simple(Some(alias))
            }
            // `use dep::name!;`: a macro, marked by its `!`
            // until `strip_meta_imports` takes it out.
            TokenKind::Bang => {
                self.bump();
                let mut prefix = prefix;
                if let Some(last) = prefix.segments.last_mut() {
                    last.name.push('!');
                }
                if self.peek() == &TokenKind::As {
                    let s = self.peek_span();
                    let path: Vec<&str> = prefix.segments.iter().map(|s| s.name.as_str()).collect();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!(
                            "a macro import cannot be renamed: write `use {};`",
                            path.join("::")
                        ),
                    );
                    return None;
                }
                return Some(rokc_ast::UseTree {
                    prefix,
                    kind: rokc_ast::UseTreeKind::Simple(None),
                });
            }
            _ => rokc_ast::UseTreeKind::Simple(None),
        };
        Some(rokc_ast::UseTree { prefix, kind })
    }

    fn parse_trait(&mut self, vis: Visibility, attrs: Vec<Attribute>) -> Option<TraitDef> {
        let start = self.peek_span();
        self.expect(&TokenKind::Trait, "`trait`")?;
        let name = if let TokenKind::Ident(n) = self.peek().clone() {
            self.bump();
            n
        } else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected trait name, found {}", self.peek().describe()),
            );
            return None;
        };
        let generics = self.parse_generic_params();
        let supertraits = if self.eat(&TokenKind::Colon) {
            self.parse_bounds()
        } else {
            Vec::new()
        };
        let where_clause = self.parse_where_clause();
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut items = Vec::new();
        let outer_owner = self.fn_owner.replace(name.clone());
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            let from = self.pos;
            if let Some((item, on)) = self.parse_trait_item() {
                match item {
                    _ if !on => {}
                    TraitItem::Method(f) => {
                        let fs = self.expand_method(f, from);
                        items.extend(fs.into_iter().map(TraitItem::Method));
                    }
                    item => items.push(item),
                }
            } else {
                self.recover(from, Recover::NestedItem);
            }
        }
        self.fn_owner = outer_owner;
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some(TraitDef {
            vis,
            attrs,
            is_unsafe: false,
            name,
            generics,
            supertraits,
            items,
            where_clause,
            span: start,
        })
    }

    /// A trait item and whether its cfg keeps it.
    fn parse_trait_item(&mut self) -> Option<(TraitItem, bool)> {
        let at = self.peek_span();
        let (attrs, vis) = self.parse_attrs_vis();
        if vis != Visibility::Private {
            self.error_help(
                Code::MISPLACED_VISIBILITY,
                at,
                "a trait item has no visibility of its own: it is as visible as the trait \
                ",
                "remove `pub`; write `pub trait` to export the trait with its items",
            );
        }
        let on = self.cfg_keeps(&attrs);
        self.parse_trait_member(attrs, vis).map(|i| (i, on))
    }

    fn parse_trait_member(&mut self, attrs: Vec<Attribute>, vis: Visibility) -> Option<TraitItem> {
        match self.peek() {
            TokenKind::Fn | TokenKind::Unsafe | TokenKind::Async | TokenKind::Yield => {
                let f = self.parse_fn(vis, attrs)?;
                Some(TraitItem::Method(f))
            }
            TokenKind::Ident(k) if k == "const" && self.at_const_fn() => {
                self.parse_const_fn(vis, attrs).map(TraitItem::Method)
            }
            TokenKind::Ident(k) if k == "const" => {
                self.bump();
                let TokenKind::Ident(name) = self.peek().clone() else {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!(
                            "expected associated const name, found {}",
                            self.peek().describe()
                        ),
                    );
                    return None;
                };
                self.bump();
                self.expect(&TokenKind::Colon, "`:`")?;
                let ty = self.parse_type()?;
                let default = if self.eat(&TokenKind::Eq) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                self.expect(&TokenKind::Semi, "`;`")?;
                Some(TraitItem::Const { name, ty, default })
            }
            TokenKind::Ident(k) if k == "type" => {
                self.bump();
                let name = if let TokenKind::Ident(n) = self.peek().clone() {
                    self.bump();
                    n
                } else {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!(
                            "expected associated type name, found {}",
                            self.peek().describe()
                        ),
                    );
                    return None;
                };
                // Optional bounds (`: A + B`) and default (`= T`).
                let mut bounds = if self.eat(&TokenKind::Colon) {
                    self.parse_bounds()
                } else {
                    Vec::new()
                };
                // `type Item where Item: Clone;`: the
                // predicates bound the associated type itself.
                for pred in self.parse_where_clause() {
                    let own = matches!(&pred.ty.kind, TyKind::Path(p)
                        if matches!(p.segments.as_slice(), [s] if s.name == name)
                            || matches!(p.segments.as_slice(),
                                [a, s] if a.name == "Self" && s.name == name));
                    if own {
                        bounds.extend(pred.bounds);
                    } else {
                        self.error(
                            Code::INVALID_GENERIC_PARAMS,
                            pred.ty.span,
                            format!(
                                "a `where` clause on associated type `{name}` can only \
                                 bound `{name}` itself"
                            ),
                        );
                    }
                }
                let default = if self.eat(&TokenKind::Eq) {
                    self.parse_type()
                } else {
                    None
                };
                self.expect(&TokenKind::Semi, "`;`")?;
                let _ = attrs;
                Some(TraitItem::AssocType {
                    name,
                    bounds,
                    default,
                })
            }
            _ => {
                let s = self.peek_span();
                self.error(
                    Code::SYNTAX,
                    s,
                    format!("expected trait item, found {}", self.peek().describe()),
                );
                None
            }
        }
    }

    /// `(elem, ...)` of a `let` destructure: names, `..` / `..name`, or
    /// nested groups, which become a temp plus a queued
    /// `LetTuple` in `pending_stmts`. The flag: ended in `,` (`(a)` groups,
    /// `(a,)` is a 1-tuple).
    fn parse_let_tuple_pattern(
        &mut self,
        outer_span: Span,
    ) -> Option<(Vec<String>, Option<LetTupleRest>, bool)> {
        self.expect(&TokenKind::LParen, "`(`")?;
        let mut names: Vec<String> = Vec::new();
        let mut rest: Option<LetTupleRest> = None;
        let mut trailing_comma = false;
        while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
            if matches!(self.peek(), TokenKind::DotDot) {
                self.bump();
                if rest.is_some() {
                    let s = self.peek_span();
                    self.error(
                        Code::REST_PATTERN,
                        s,
                        "tuple destructure: only one `..` rest pattern allowed",
                    );
                    return None;
                }
                let name = match self.peek().clone() {
                    TokenKind::Ident(n) => {
                        self.bump();
                        Some(n)
                    }
                    _ => None,
                };
                rest = Some((names.len(), name));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                continue;
            }
            // Nested `((a, b), c)`: bind a temp and queue
            // `let (a, b) = __destructN;` so the names land in this scope.
            if matches!(self.peek(), TokenKind::LParen) {
                let inner_span = self.peek_span();
                let (mut inner_names, inner_rest, inner_comma) =
                    self.parse_let_tuple_pattern(inner_span)?;
                // `((a), b)`: the inner parens only group.
                if inner_names.len() == 1 && inner_rest.is_none() && !inner_comma {
                    names.extend(inner_names.pop());
                    trailing_comma = self.eat(&TokenKind::Comma);
                    if !trailing_comma {
                        break;
                    }
                    continue;
                }
                let temp_name = self.fresh_destructure_temp();
                names.push(temp_name.clone());
                let init = Expr {
                    span: inner_span,
                    kind: ExprKind::Path(Path {
                        segments: vec![PathSegment {
                            name: temp_name,
                            generics: Vec::new(),
                        }],
                        span: inner_span,
                        origin: None,
                    }),
                };
                self.pending_stmts.push(Stmt {
                    kind: StmtKind::LetTuple {
                        names: inner_names,
                        rest: inner_rest,
                        ty: None,
                        init,
                    },
                    span: outer_span,
                });
            } else {
                let n = if let TokenKind::Ident(n) = self.peek().clone() {
                    self.bump();
                    n
                } else {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!("expected binding name, found {}", self.peek().describe()),
                    );
                    return None;
                };
                // `n @ (x, y)`: bind `n`, then destructure it.
                if self.eat(&TokenKind::At) {
                    let inner_span = self.peek_span();
                    let sub = self.parse_let_sub_pattern(&n, inner_span)?;
                    self.pending_stmts.push(sub);
                }
                names.push(n);
            }
            trailing_comma = self.eat(&TokenKind::Comma);
            if !trailing_comma {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)`")?;
        Some((names, rest, trailing_comma))
    }

    /// The pattern after `let name @`: an irrefutable destructure of
    /// `name`, checked in HIR.
    fn parse_let_sub_pattern(&mut self, name: &str, span: Span) -> Option<Stmt> {
        let init = Expr {
            span,
            kind: ExprKind::Path(Path {
                segments: vec![PathSegment {
                    name: name.to_owned(),
                    generics: Vec::new(),
                }],
                span,
                origin: None,
            }),
        };
        let pat = self.with_struct_expr(true, Self::parse_pattern_atom)?;
        Some(Stmt {
            kind: StmtKind::LetPattern {
                pat,
                ty: None,
                init,
            },
            span,
        })
    }

    /// `Path { f, g: alias, .. }` of a `let` struct destructure:
    /// the path, `(field, binding)` pairs, and whether `..` ends it.
    #[allow(clippy::type_complexity)]
    fn parse_let_struct_fields(&mut self) -> Option<(Path, Vec<(String, String)>, bool)> {
        let path = self.parse_path()?;
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut rest = false;
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            if matches!(self.peek(), TokenKind::DotDot) {
                self.bump();
                rest = true;
                break;
            }
            let field_name = if let Some(n) = self.peek_name() {
                self.bump_name();
                n
            } else {
                let s = self.peek_span();
                self.error(Code::SYNTAX, s, "expected field name");
                return None;
            };
            let bind_name = if self.eat(&TokenKind::Colon) {
                if let TokenKind::Ident(n) = self.peek().clone() {
                    self.bump();
                    n
                } else {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!("expected rebind name, found {}", self.peek().describe()),
                    );
                    return None;
                }
            } else {
                field_name.clone()
            };
            fields.push((field_name, bind_name));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some((path, fields, rest))
    }

    /// Attributes and visibility in either order, or attributes on both
    /// sides (`pub #[inline] fn`).
    fn parse_attrs_vis(&mut self) -> (Vec<Attribute>, Visibility) {
        let mut attrs = self.parse_outer_attrs();
        let vis = self.parse_visibility();
        if vis != Visibility::Private {
            attrs.extend(self.parse_outer_attrs());
        }
        (attrs, vis)
    }

    fn parse_visibility(&mut self) -> Visibility {
        if !self.eat(&TokenKind::Pub) {
            return Visibility::Private;
        }
        if !self.eat(&TokenKind::LParen) {
            return Visibility::Pub;
        }
        // `pub(reach)`. Contextual identifier; only valid inside `pub(...)`.
        let v = if let TokenKind::Ident(name) = self.peek().clone() {
            match name.as_str() {
                "pkg" => Visibility::Pkg,
                "sub" => Visibility::Sub,
                _ => {
                    let s = self.peek_span();
                    self.error(Code::SYNTAX, s, "expected `pkg` or `sub`");
                    Visibility::Pub
                }
            }
        } else {
            let s = self.peek_span();
            self.error(Code::SYNTAX, s, "expected `pkg` or `sub`");
            Visibility::Pub
        };
        self.bump();
        self.expect(&TokenKind::RParen, "`)`");
        v
    }

    /// A function's name. Contextual keywords lex as identifiers,
    /// so `fn union` (`Set::union`) and `fn any` (`Iterator::any`) work.
    fn fn_name(&mut self) -> Option<String> {
        let TokenKind::Ident(name) = self.peek().clone() else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected function name, found {}", self.peek().describe()),
            );
            return None;
        };
        self.bump();
        Some(name)
    }

    /// `const fn`: a plain fn carrying `CONST_FN_ATTR`.
    /// `const fn` / `const unsafe fn` (`"const" fn_decl`).
    fn at_const_fn(&self) -> bool {
        matches!(self.peek_next(), Some(TokenKind::Fn | TokenKind::Unsafe))
    }

    fn parse_const_fn(&mut self, vis: Visibility, mut attrs: Vec<Attribute>) -> Option<FnDef> {
        let span = self.peek_span();
        self.bump(); // `const`
        attrs.push(Attribute {
            path: vec![rokc_ast::CONST_FN_ATTR.to_owned()],
            body: rokc_ast::AttrBody::Empty,
            inner: false,
            span,
        });
        let def = self.parse_fn(vis, attrs)?;
        // No type arguments; introspection is meta blocks'.
        for p in &def.params {
            if let Some(rokc_ast::Ty {
                kind: rokc_ast::TyKind::Path(path),
                span,
            }) = &p.ty
            {
                if matches!(path.segments.as_slice(), [s] if s.name == "type") {
                    self.error(
                        Code::INVALID_SIGNATURE,
                        *span,
                        "a `const fn` cannot take a type as an argument: use a \
                         generic parameter, or a meta block for type introspection",
                    );
                }
            }
        }
        Some(def)
    }

    fn parse_fn(&mut self, vis: Visibility, attrs: Vec<Attribute>) -> Option<FnDef> {
        // Modifiers in fixed order `unsafe async yield`; a
        // misordered or repeated one is reported and kept.
        const MODS: [&str; 3] = ["unsafe", "async", "yield"];
        let start = self.peek_span();
        let mut seen: Vec<(usize, Span)> = Vec::new();
        loop {
            let rank = match self.peek() {
                TokenKind::Unsafe => 0,
                TokenKind::Async => 1,
                TokenKind::Yield => 2,
                _ => break,
            };
            seen.push((rank, self.peek_span()));
            self.bump();
        }
        if let Some(w) = seen.windows(2).find(|w| w[1].0 <= w[0].0) {
            let (this, prev) = (MODS[w[1].0], MODS[w[0].0]);
            let mut ranks: Vec<usize> = seen.iter().map(|m| m.0).collect();
            ranks.sort_unstable();
            ranks.dedup();
            let fixed: Vec<&str> = ranks.iter().map(|r| MODS[*r]).collect();
            let msg = if w[1].0 == w[0].0 {
                format!("fn modifier `{this}` written twice")
            } else {
                format!(
                    "fn modifier `{this}` after `{prev}`: modifiers go in the order `unsafe \
                     async yield`"
                )
            };
            self.error_help(
                Code::MODIFIER_ORDER,
                w[1].1,
                msg,
                format!("write `{} fn`", fixed.join(" ")),
            );
        }
        let is_unsafe = seen.iter().any(|m| m.0 == 0);
        let is_async = seen.iter().any(|m| m.0 == 1);
        let is_generator = seen.iter().any(|m| m.0 == 2);
        self.expect(&TokenKind::Fn, "`fn`")?;
        let name = self.fn_name()?;
        // Optional generic parameters.
        let generics = if matches!(self.peek(), TokenKind::Lt) {
            self.parse_generic_params()
        } else {
            Generics::default()
        };
        self.expect(&TokenKind::LParen, "`(`")?;
        let untyped_mark = self.untyped_throws.len();
        let mut params = Vec::new();
        let mut attrs = attrs;
        while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
            if self.eat_c_variadic(&mut attrs) {
                break;
            }
            let p_attrs = self.cfg_only_attrs("a parameter");
            let marks = (self.pending_stmts.len(), self.lifted.len());
            let param = self.parse_param()?;
            if !self.cfg_drops(&p_attrs, marks) {
                params.push(param);
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)`")?;
        let rparen = self.prev_span();

        let mut generics = generics;
        self.desugar_variadic(&mut params);
        let throws = if matches!(self.peek(), TokenKind::Ident(n) if n == "rethrows") {
            let span = self.peek_span();
            self.bump();
            self.untyped_throws.truncate(untyped_mark);
            let errs = desugar_rethrows(&mut params, &mut generics);
            if errs.is_empty() {
                self.error(
                    Code::INVALID_SIGNATURE,
                    span,
                    "`rethrows` needs a parameter of throwing function type \
                     (`f: fn(T) throws -> U`)",
                );
            }
            ThrowsKind::Typed(errs)
        } else {
            match self.parse_throws() {
                ThrowsKind::Untyped => ThrowsKind::Typed(vec![rokc_ast::untyped_throws_ty(start)]),
                t => t,
            }
        };
        let ret_ty = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let throws = self.throws_after_ret(throws, ret_ty.as_ref(), start, rparen);
        let where_clause = self.parse_where_clause();

        let body = if matches!(self.peek(), TokenKind::LBrace) {
            let owned = match (&self.fn_owner, self.fn_names.is_empty()) {
                (Some(o), true) => format!("{o}.{name}"),
                _ => name.clone(),
            };
            self.fn_names.push(owned);
            // `//!` at the start of the body documents the function.
            self.doc_attrs_at(self.pos + 1, true, &mut attrs);
            let (b, _) = self.ret_frame(None, Self::parse_block);
            self.fn_names.pop();
            Some(b?)
        } else {
            self.expect(&TokenKind::Semi, "`;` or function body")?;
            None
        };

        Some(FnDef {
            vis,
            attrs,
            name,
            generics,
            params,
            ret_ty,
            body: body.map(Into::into),
            is_async,
            is_generator,
            is_unsafe,
            throws,
            where_clause,
            span: start,
        })
    }

    /// `fn printf(fmt: str, ...)`: C variadic marker, kept as
    /// an attribute; HIR checks it is last and in an `extern` block.
    fn eat_c_variadic(&mut self, attrs: &mut Vec<Attribute>) -> bool {
        if !(matches!(self.peek(), TokenKind::DotDot)
            && matches!(self.peek_next(), Some(TokenKind::Dot)))
        {
            return false;
        }
        let span = self.peek_span();
        self.bump();
        self.bump();
        attrs.push(Attribute {
            path: vec![rokc_ast::C_VARIADIC_ATTR.to_owned()],
            body: rokc_ast::AttrBody::Empty,
            inner: false,
            span,
        });
        self.eat(&TokenKind::Comma);
        if !matches!(self.peek(), TokenKind::RParen) {
            self.error(
                Code::INVALID_SIGNATURE,
                span,
                "`...` must be the last parameter",
            );
        }
        true
    }

    /// `xs: ...T` is `xs: Slice<T>` over the trailing
    /// arguments, so there is no instance per argument count. Last only.
    fn desugar_variadic(&mut self, params: &mut [Param]) {
        // Defaults are trailing. A variadic may follow them,
        // and so may a last `fn` parameter, which a trailing closure
        // fills.
        if let Some(first) = params.iter().position(|p| p.default.is_some()) {
            let last = params.len() - 1;
            let late = params[first + 1..].iter().enumerate().find_map(|(k, p)| {
                let closure_slot = first + 1 + k == last
                    && p.ty
                        .as_ref()
                        .is_some_and(|t| matches!(t.kind, TyKind::Fn(..)));
                (p.default.is_none() && !p.variadic && !closure_slot).then_some(p)
            });
            if let Some(p) = late {
                let msg = format!(
                    "parameter `{}` has no default but follows one that does; defaults are \
                     trailing",
                    p.name
                );
                let help = format!(
                    "give `{}` a default, or move it before `{}`",
                    p.name, params[first].name
                );
                self.error_help(Code::INVALID_SIGNATURE, p.span, msg, help);
            }
        }
        let last = params.len().saturating_sub(1);
        for (i, p) in params.iter_mut().enumerate() {
            if !p.variadic {
                continue;
            }
            if i != last {
                self.error(
                    Code::INVALID_SIGNATURE,
                    p.span,
                    "only the last parameter can be variadic `...T`",
                );
                continue;
            }
            let Some(elem) = p.ty.take() else { continue };
            p.ty = Some(variadic_slice_ty(elem));
        }
    }

    /// Parse an optional `<T: Bound = Default, const N: usize>` generic
    /// parameter list. Returns empty generics when not at `<`.
    fn parse_generic_params(&mut self) -> Generics {
        let mut params: Vec<rokc_ast::GenericParam> = Vec::new();
        if !self.eat(&TokenKind::Lt) {
            return Generics { params };
        }
        let mut defaulted = false;
        while !matches!(self.peek(), TokenKind::Gt | TokenKind::Eof) {
            let start = self.peek_span();
            if matches!(self.peek_next(), Some(TokenKind::Ident(_))) && self.eat_kw("const") {
                let TokenKind::Ident(name) = self.peek().clone() else {
                    let s = self.peek_span();
                    self.error(Code::SYNTAX, s, "const-generic name expected");
                    break;
                };
                self.bump();
                self.expect(&TokenKind::Colon, "`:`");
                if let Some(ty) = self.parse_type() {
                    if !const_param_ty(&ty) {
                        self.error_help(
                            Code::INVALID_GENERIC_PARAMS,
                            ty.span,
                            format!(
                                "const parameter `{name}` has type `{}`; only integers, `bool`, \
                                 `char` and `c_char` may be const parameters",
                                rokc_ast::print::print_ty(&ty)
                            ),
                            "use an integer type, such as `usize`",
                        );
                    }
                    params.push(rokc_ast::GenericParam::Const { name, ty });
                }
                // Optional `= const_expr` default (not modelled).
                if self.eat(&TokenKind::Eq) {
                    let _ = self.parse_expr_bp(0);
                }
            } else {
                let TokenKind::Ident(name) = self.peek().clone() else {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!(
                            "expected generic param name, found {}",
                            self.peek().describe()
                        ),
                    );
                    break;
                };
                self.bump();
                let bounds = if self.eat(&TokenKind::Colon) {
                    self.parse_bounds()
                } else {
                    Vec::new()
                };
                // Optional default `= T2`.
                let default = if self.eat(&TokenKind::Eq) {
                    self.parse_type()
                } else {
                    None
                };
                params.push(rokc_ast::GenericParam::Type {
                    name,
                    bounds,
                    default,
                });
            }
            // Defaults are trailing.
            match params.last() {
                Some(rokc_ast::GenericParam::Type {
                    default: Some(_), ..
                }) => defaulted = true,
                Some(
                    rokc_ast::GenericParam::Type { name, .. }
                    | rokc_ast::GenericParam::Const { name, .. },
                ) if defaulted => {
                    let msg = format!(
                        "generic parameter `{name}` has no default but follows one that does; \
                         defaults must be trailing"
                    );
                    self.error(Code::INVALID_GENERIC_PARAMS, start, msg);
                }
                _ => {}
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_closing_gt();
        Generics { params }
    }

    /// `Bound (+ Bound)*`.
    fn parse_bounds(&mut self) -> Vec<Ty> {
        let mut bounds = Vec::new();
        loop {
            if let Some(t) = self.parse_type() {
                bounds.push(t);
            }
            if !self.eat(&TokenKind::Plus) {
                return bounds;
            }
        }
    }

    /// The `N` of `[T; N]` / `[v; N]`: a const expression (`3`, `N`,
    /// `M * N`).
    fn parse_const_len(&mut self) -> Option<ConstExpr> {
        Some(match (self.peek().clone(), self.peek_next()) {
            (TokenKind::IntLit(s), Some(TokenKind::RBracket)) => {
                self.bump();
                ConstExpr::Literal(Literal::Int(s))
            }
            _ => ConstExpr::Expr(Box::new(self.with_struct_expr(false, Self::parse_expr)?)),
        })
    }

    /// Optional `where T: A + B, U::Out: C`.
    fn parse_where_clause(&mut self) -> Vec<rokc_ast::WherePred> {
        let mut preds = Vec::new();
        if !self.eat_kw("where") {
            return preds;
        }
        while !matches!(
            self.peek(),
            TokenKind::LBrace | TokenKind::Semi | TokenKind::Eof
        ) {
            let Some(ty) = self.parse_type() else { break };
            if self
                .expect(&TokenKind::Colon, "`:` in where clause")
                .is_none()
            {
                break;
            }
            let bounds = self.parse_bounds();
            preds.push(rokc_ast::WherePred { ty, bounds });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        preds
    }

    fn parse_throws(&mut self) -> ThrowsKind {
        if !self.eat_kw("throws") {
            return ThrowsKind::None;
        }
        if !self.eat(&TokenKind::LParen) {
            return ThrowsKind::Untyped;
        }
        let mut tys = Vec::new();
        while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
            if let Some(ty) = self.parse_type() {
                tys.push(ty);
            }
            if !self.eat(&TokenKind::Pipe) {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)`");
        ThrowsKind::Typed(tys)
    }

    fn parse_param(&mut self) -> Option<Param> {
        let start = self.peek_span();
        let by_ref = self.eat(&TokenKind::Ref);

        // `self` / `ref self`; its type is filled in with the trait or impl.
        if matches!(self.peek(), TokenKind::SelfLower) {
            self.bump();
            return Some(Param {
                name: "self".to_owned(),
                ty: None,
                by_ref,
                default: None,
                variadic: false,
                span: start,
            });
        }

        let name = if let TokenKind::Ident(n) = self.peek().clone() {
            self.bump();
            n
        } else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!("expected parameter name, found {}", self.peek().describe()),
            );
            return None;
        };
        // `xs: ...T` - `...` precedes the element type; the
        // param is marked `variadic` (its type becomes `Slice<T>`).
        let (ty, variadic) = if self.eat(&TokenKind::Colon) {
            if self.eat(&TokenKind::DotDot) && self.eat(&TokenKind::Dot) {
                // `...T` - three dots. Lexed as `..` then `.`.
                let elem = self.parse_type()?;
                (Some(elem), true)
            } else {
                (Some(self.parse_type()?), false)
            }
        } else {
            (None, false)
        };
        let default = if self.eat(&TokenKind::Eq) {
            let value = self.with_struct_expr(true, Self::parse_expr)?;
            Some(value)
        } else {
            None
        };
        Some(Param {
            name,
            ty,
            by_ref,
            default,
            variadic,
            span: start,
        })
    }

    /// A type. `|` (sum) binds weakest and is off in closure
    /// parameters and cast targets, where it ends the type.
    fn parse_type(&mut self) -> Option<Ty> {
        let start = self.peek_span();
        let first = self.parse_type_atom()?;
        if !(self.type_sum_allowed && matches!(self.peek(), TokenKind::Pipe)) {
            return Some(first);
        }
        let mut variants = vec![first];
        while self.eat(&TokenKind::Pipe) {
            variants.push(self.parse_type_atom()?);
        }
        Some(Ty {
            kind: TyKind::Sum(variants),
            span: start,
        })
    }

    /// Everything but `|`: paths, `fn(..) -> R`, tuples / grouping,
    /// arrays, `any`, `none`, `Self`, and their `T?`.
    fn parse_type_atom(&mut self) -> Option<Ty> {
        let mut t = self.nested(Self::parse_type_atom_inner)?;
        // `T?` binds tighter than `|`. After `x as T` / `x is
        // T` a `?` is the type's (write `(x as T)?`), but `??` coalesces.
        loop {
            let layers = match self.peek() {
                TokenKind::Question => 1,
                TokenKind::QuestionQuestion if !self.in_cast_type => 2,
                _ => break,
            };
            self.bump();
            for _ in 0..layers {
                t = optional_ty(t);
            }
        }
        Some(t)
    }

    /// `name!( .. )` where a type is expected: expand and
    /// re-parse the result as a type.
    // `Parser::parse_*` is not general over the expansion's lifetime.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn macro_call_type(&mut self) -> Option<Ty> {
        let (def, name, start, call) = self.take_macro_call(false)?;
        let poison = || Ty {
            kind: TyKind::Tuple(Vec::new()),
            span: start,
        };
        let t =
            def.and_then(|d| self.expand_call(&d, &name, start, &call, false, |p| p.parse_type()));
        Some(t.flatten().unwrap_or_else(poison))
    }

    fn parse_type_atom_inner(&mut self) -> Option<Ty> {
        if let Some(ty) = self.macro_call_type() {
            return Some(ty);
        }
        let start = self.peek_span();
        let kind = match self.peek().clone() {
            // `any` | `any Tr` | `any (A + B)`. The bound binds
            // tighter than `|`: `any Error | none` is `(any Error) | none`.
            TokenKind::Ident(_)
                if self.at_kw("any")
                    && !matches!(self.peek_next(), Some(TokenKind::ColonColon)) =>
            {
                self.bump();
                match self.peek() {
                    TokenKind::Ident(_) => TyKind::AnyTrait(vec![self.parse_type_path()?]),
                    TokenKind::LParen => {
                        self.bump();
                        let mut bounds = vec![self.parse_type_path()?];
                        while self.eat(&TokenKind::Plus) {
                            bounds.push(self.parse_type_path()?);
                        }
                        self.expect(&TokenKind::RParen, "`)`")?;
                        TyKind::AnyTrait(bounds)
                    }
                    _ => TyKind::Any,
                }
            }
            TokenKind::None => {
                self.bump();
                TyKind::None
            }
            // `Self::Item`: an associated type of the impl / trait.
            TokenKind::SelfUpper if self.peek_next() == Some(TokenKind::ColonColon) => {
                self.bump();
                self.bump();
                let mut p = self.parse_type_path()?;
                p.segments.insert(
                    0,
                    PathSegment {
                        name: "Self".to_owned(),
                        generics: Vec::new(),
                    },
                );
                p.span = start;
                TyKind::Path(p)
            }
            TokenKind::SelfUpper => {
                self.bump();
                TyKind::SelfUpper
            }
            TokenKind::Bang => {
                self.bump();
                TyKind::Never
            }
            TokenKind::Unsafe
                if self.peek_next() == Some(TokenKind::Fn) || self.at_c_fn_type(1) =>
            {
                self.parse_marked_fn_type(start)?
            }
            TokenKind::Ident(_) if self.at_c_fn_type(0) => self.parse_marked_fn_type(start)?,
            TokenKind::Ident(_) => tuple_or_path(self.parse_type_path()?),
            // `<T as Tr>::X`.
            TokenKind::Lt | TokenKind::Shl => TyKind::QPath(Box::new(self.parse_qpath()?)),
            TokenKind::Fn => {
                self.bump();
                self.expect(&TokenKind::LParen, "`(`")?;
                let args = self.fn_type_params()?;
                let throws_span = self.peek_span();
                let throws = self.parse_throws();
                // `fn(..) -> R` binds tighter than `|`:
                // `fn() -> A | B` is `(fn() -> A) | B`.
                let mut ret = if self.eat(&TokenKind::Arrow) {
                    self.parse_type_atom()?
                } else {
                    Ty {
                        kind: TyKind::Tuple(Vec::new()),
                        span: start,
                    }
                };
                let err = match throws {
                    ThrowsKind::None => None,
                    // Claimed by an enclosing `rethrows` signature, else
                    // reported at the end of the parse.
                    ThrowsKind::Untyped => {
                        self.untyped_throws.push(throws_span);
                        Some(rokc_ast::untyped_throws_ty(throws_span))
                    }
                    ThrowsKind::Typed(mut tys) if tys.len() == 1 => tys.pop(),
                    ThrowsKind::Typed(tys) => Some(Ty {
                        kind: TyKind::Sum(tys),
                        span: throws_span,
                    }),
                };
                if let Some(err) = err {
                    let span = ret.span;
                    ret = Ty {
                        kind: TyKind::Throws {
                            ok: Box::new(ret),
                            err: Box::new(err),
                        },
                        span,
                    };
                }
                // `fn(..) -> R + Send + Sync` - auto-trait bounds.
                let mut bounds = rokc_ast::FnTyBounds::default();
                while self.eat(&TokenKind::Plus) {
                    bounds.auto.push(self.parse_type_path()?);
                }
                TyKind::Fn(args, Box::new(ret), bounds)
            }
            TokenKind::LParen => {
                // `()` unit, `(T)` grouping, `(T,)` / `(A, B)` tuple.
                self.bump();
                let mut tys = Vec::new();
                let mut trailing_comma = false;
                let ok = self.with_type_sum(true, |p| {
                    let prev_cast = std::mem::replace(&mut p.in_cast_type, false);
                    let mut ok = true;
                    while !matches!(p.peek(), TokenKind::RParen | TokenKind::Eof) {
                        let Some(t) = p.parse_type() else {
                            ok = false;
                            break;
                        };
                        tys.push(t);
                        trailing_comma = p.eat(&TokenKind::Comma);
                        if !trailing_comma {
                            break;
                        }
                    }
                    p.in_cast_type = prev_cast;
                    ok
                });
                if !ok {
                    return None;
                }
                self.expect(&TokenKind::RParen, "`)`")?;
                if tys.len() == 1 && !trailing_comma {
                    return tys.pop();
                }
                TyKind::Tuple(tys)
            }
            // `[T; N]` fixed-size array.
            TokenKind::LBracket => {
                self.bump();
                let elem = self.with_type_sum(true, Self::parse_type)?;
                self.expect(&TokenKind::Semi, "`;`")?;
                let len = self.parse_const_len()?;
                self.expect(&TokenKind::RBracket, "`]`")?;
                TyKind::Array(Box::new(elem), len)
            }
            other => {
                self.error(
                    Code::SYNTAX,
                    start,
                    format!("expected type, found {}", other.describe()),
                );
                return None;
            }
        };
        Some(Ty { kind, span: start })
    }

    /// Comma-separated types up to (not including) `close`. Sum types are
    /// allowed inside the brackets even where a bare `|` would end a type.
    fn nested_type_list(&mut self, close: TokenKind) -> Option<Vec<Ty>> {
        self.with_type_sum(true, |p| {
            let prev_cast = std::mem::replace(&mut p.in_cast_type, false);
            let mut tys = Vec::new();
            let mut ok = true;
            while std::mem::discriminant(p.peek()) != std::mem::discriminant(&close)
                && !matches!(
                    p.peek(),
                    TokenKind::Eof | TokenKind::Shr | TokenKind::GtEq | TokenKind::ShrEq
                )
            {
                // Const argument: a literal, `-lit`, or
                // `{ expr }`.
                if matches!(
                    p.peek(),
                    TokenKind::IntLit(_)
                        | TokenKind::CharLit(_)
                        | TokenKind::Minus
                        | TokenKind::True
                        | TokenKind::False
                        | TokenKind::LBrace
                ) {
                    let span = p.peek_span();
                    let e = if p.eat(&TokenKind::LBrace) {
                        let e = p.with_struct_expr(true, Self::parse_expr);
                        p.expect(&TokenKind::RBrace, "`}`")?;
                        e
                    } else {
                        p.parse_expr_bp(PREFIX_PREC)
                    };
                    let Some(e) = e else {
                        ok = false;
                        break;
                    };
                    tys.push(Ty {
                        kind: TyKind::Const(ConstExpr::Expr(Box::new(e))),
                        span,
                    });
                    if !p.eat(&TokenKind::Comma) {
                        break;
                    }
                    continue;
                }
                // `Name = Ty`: an associated-type binding.
                let binding = match (p.peek().clone(), p.tokens.get(p.pos + 1).map(|t| &t.kind)) {
                    (TokenKind::Ident(name), Some(TokenKind::Eq)) => {
                        p.bump();
                        p.bump();
                        Some(name)
                    }
                    _ => None,
                };
                let Some(t) = p.parse_type() else {
                    ok = false;
                    break;
                };
                tys.push(match binding {
                    Some(name) => Ty {
                        span: t.span,
                        kind: TyKind::Binding(name, Box::new(t)),
                    },
                    None => t,
                });
                if !p.eat(&TokenKind::Comma) {
                    break;
                }
            }
            p.in_cast_type = prev_cast;
            ok.then_some(tys)
        })
    }

    /// Target type of `expr as T`: no bare `|` sum, and `<` only opens
    /// generic args when a matching `>` follows.
    fn parse_cast_type(&mut self) -> Option<Ty> {
        let prev = std::mem::replace(&mut self.in_cast_type, true);
        let ty = self.with_type_sum(false, Self::parse_type);
        self.in_cast_type = prev;
        ty
    }

    /// Lookahead from a `<`: does a matching `>` follow, with only
    /// type-shaped tokens in between?
    fn generic_close_ahead(&self) -> bool {
        let mut angle = 0i32;
        let mut nest = 0i32;
        for t in self.tokens.iter().skip(self.pos) {
            match &t.kind {
                TokenKind::Lt => angle += 1,
                TokenKind::Gt => angle -= 1,
                TokenKind::Shr => angle -= 2,
                TokenKind::LParen | TokenKind::LBracket => nest += 1,
                TokenKind::RParen | TokenKind::RBracket => {
                    nest -= 1;
                    if nest < 0 {
                        return false;
                    }
                }
                TokenKind::Semi if nest > 0 => {}
                TokenKind::Ident(_)
                | TokenKind::ColonColon
                | TokenKind::Comma
                | TokenKind::IntLit(_)
                | TokenKind::CharLit(_)
                | TokenKind::Fn
                | TokenKind::Arrow
                | TokenKind::None
                | TokenKind::SelfUpper
                | TokenKind::Pipe
                | TokenKind::Question
                | TokenKind::QuestionQuestion => {}
                _ => return false,
            }
            if angle <= 0 {
                return nest == 0;
            }
        }
        false
    }

    fn parse_path(&mut self) -> Option<Path> {
        self.parse_path_inner(false)
    }

    /// `<T as Tr>::item`.
    fn parse_qpath(&mut self) -> Option<rokc_ast::QPath> {
        if !self.eat_opening_lt() {
            self.expect(&TokenKind::Lt, "`<`")?;
        }
        let self_ty = self.with_type_sum(false, Self::parse_type_atom)?;
        self.expect(&TokenKind::As, "`as`")?;
        let tr = self.parse_type_path()?;
        if !self.expect_closing_gt() {
            return None;
        }
        self.expect(&TokenKind::ColonColon, "`::`")?;
        let TokenKind::Ident(name) = self.peek().clone() else {
            let s = self.peek_span();
            self.error(
                Code::SYNTAX,
                s,
                format!(
                    "expected associated item name after `<.. as ..>::`, found {}",
                    self.peek().describe()
                ),
            );
            return None;
        };
        self.bump();
        Some(rokc_ast::QPath {
            self_ty,
            trait_path: tr,
            item: name,
            member: None,
        })
    }

    fn parse_type_path(&mut self) -> Option<Path> {
        self.parse_path_inner(true)
    }

    fn parse_path_inner(&mut self, allow_generics: bool) -> Option<Path> {
        let start = self.peek_span();
        let origin = self.tokens.get(self.pos).and_then(Token::origin_module);
        let mut segments = Vec::new();
        loop {
            let TokenKind::Ident(name) = self.peek().clone() else {
                let s = self.peek_span();
                self.error(
                    Code::SYNTAX,
                    s,
                    format!("expected path segment, found {}", self.peek().describe()),
                );
                return None;
            };
            self.bump();
            // Generic args only in type position; in expressions `<` could
            // be a comparison.
            let mut generics: Vec<Ty> = Vec::new();
            if allow_generics
                && (matches!(self.peek(), TokenKind::Shl) && !self.in_cast_type
                    || matches!(self.peek(), TokenKind::Lt)
                        && (!self.in_cast_type || self.generic_close_ahead()))
            {
                self.eat_opening_lt();
                generics = self.nested_type_list(TokenKind::Gt)?;
                if !self.expect_closing_gt() {
                    return None;
                }
            }
            segments.push(PathSegment { name, generics });
            if !self.eat(&TokenKind::ColonColon) {
                break;
            }
            // `f::<T, 3>(..)`; type paths too.
            if matches!(self.peek(), TokenKind::Lt) {
                self.bump();
                let args = self.nested_type_list(TokenKind::Gt)?;
                if !self.expect_closing_gt() {
                    return None;
                }
                if let Some(last) = segments.last_mut() {
                    last.generics = args;
                }
                if !self.eat(&TokenKind::ColonColon) {
                    break;
                }
            }
        }
        Some(Path {
            segments,
            span: start,
            origin,
        })
    }

    /// `target = value` in expression position, from the `=`: a block
    /// holding the assignment, of unit type.
    fn assign_tail(&mut self, target: Expr) -> Option<Expr> {
        let span = target.span;
        self.expect(&TokenKind::Eq, "`=`")?;
        let value = self.parse_expr()?;
        Some(Expr {
            kind: ExprKind::Block(Block {
                stmts: vec![Stmt {
                    kind: StmtKind::Assign(target, value),
                    span,
                }],
                trailing: None,
                span,
                label: None,
            }),
            span,
        })
    }

    /// Arm body. `return` / `break` / `continue` / `throw` are statements,
    /// so `none => return 0,` wraps one in a block.
    fn parse_arm_body(&mut self) -> Option<Expr> {
        let span = self.peek_span();
        let label = |p: &mut Self| {
            if let TokenKind::Label(name) = p.peek().clone() {
                p.bump();
                Some(name)
            } else {
                None
            }
        };
        let value = |p: &mut Self| {
            if matches!(
                p.peek(),
                TokenKind::Comma
                    | TokenKind::RBrace
                    | TokenKind::RParen
                    | TokenKind::RBracket
                    | TokenKind::Semi
            ) || (!p.struct_expr_allowed && matches!(p.peek(), TokenKind::LBrace))
            {
                Some(None)
            } else {
                p.parse_expr().map(Some)
            }
        };
        let kind = match self.peek() {
            TokenKind::Return => {
                let at = self.bump().span;
                let labeled = self.return_label(at);
                let v = value(self)?;
                if v.is_none() && !labeled {
                    self.bare_return(at);
                }
                StmtKind::Return(v)
            }
            TokenKind::Break => {
                self.bump();
                let label = label(self);
                StmtKind::Break {
                    label,
                    value: value(self)?,
                }
            }
            TokenKind::Continue => {
                self.bump();
                StmtKind::Continue { label: label(self) }
            }
            TokenKind::Throw => {
                self.bump();
                StmtKind::Throw(self.parse_expr()?)
            }
            _ => {
                let e = self.parse_expr()?;
                return if matches!(self.peek(), TokenKind::Eq) {
                    self.assign_tail(e)
                } else {
                    Some(e)
                };
            }
        };
        Some(Expr {
            kind: ExprKind::Block(Block {
                stmts: vec![Stmt { kind, span }],
                trailing: None,
                span,
                label: None,
            }),
            span,
        })
    }

    /// `let PAT [: T] = init else { .. };` →
    /// `let (b1, .., bn,) = match init { PAT => (b1, .., bn,), _ => else };`
    /// (one binder: a plain `let`; none: an expression statement).
    fn let_else_pattern(
        &mut self,
        pat: Pattern,
        ty: Option<Ty>,
        init: Expr,
        else_block: Block,
        start: Span,
    ) -> Stmt {
        fn binders(p: &Pattern, out: &mut Vec<String>) {
            match &p.kind {
                PatternKind::Binding { name, sub } => {
                    out.push(name.clone());
                    if let Some(s) = sub {
                        binders(s, out);
                    }
                }
                PatternKind::TypeCheck {
                    binder: Some(n), ..
                }
                | PatternKind::Rest(Some(n)) => out.push(n.clone()),
                PatternKind::Tuple(ps) | PatternKind::EnumVariant { args: ps, .. } => {
                    ps.iter().for_each(|p| binders(p, out));
                }
                PatternKind::Or(ps) => {
                    if let Some(p) = ps.first() {
                        binders(p, out);
                    }
                }
                PatternKind::Struct { fields, .. } => {
                    for f in fields {
                        match &f.pat {
                            Some(p) => binders(p, out),
                            None => out.push(f.name.clone()),
                        }
                    }
                }
                _ => {}
            }
        }
        let span = init.span;
        let path = |name: &str| Expr {
            kind: ExprKind::Path(Path {
                segments: vec![PathSegment {
                    name: name.to_string(),
                    generics: vec![],
                }],
                span,
                origin: None,
            }),
            span,
        };
        let mut names = Vec::new();
        binders(&pat, &mut names);
        let body = match names.as_slice() {
            [one] => path(one),
            _ => Expr {
                kind: ExprKind::Tuple(names.iter().map(|n| path(n)).collect()),
                span,
            },
        };
        let scrutinee = match ty {
            Some(ty) => Expr {
                kind: ExprKind::Block(Block {
                    stmts: vec![Stmt {
                        kind: StmtKind::Let {
                            name: "__letelse_s".to_string(),
                            ty: Some(ty),
                            init,
                        },
                        span,
                    }],
                    trailing: Some(Box::new(path("__letelse_s"))),
                    span,
                    label: None,
                }),
                span,
            },
            None => init,
        };
        let else_span = else_block.span;
        let m = Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![
                    MatchArm {
                        pat,
                        guard: None,
                        body,
                    },
                    MatchArm {
                        pat: Pattern {
                            kind: PatternKind::Wildcard,
                            span: else_span,
                        },
                        guard: None,
                        body: Expr {
                            kind: ExprKind::Block(else_block),
                            span: else_span,
                        },
                    },
                ],
            },
            span,
        };
        let kind = match names.len() {
            0 => StmtKind::Expr(m),
            1 => StmtKind::Let {
                name: names.pop().unwrap_or_default(),
                ty: None,
                init: m,
            },
            _ => {
                self.pending_stmts.push(Stmt {
                    kind: StmtKind::LetTuple {
                        names,
                        rest: None,
                        ty: None,
                        init: path("__letelse_t"),
                    },
                    span: start,
                });
                StmtKind::Let {
                    name: "__letelse_t".to_string(),
                    ty: None,
                    init: m,
                }
            }
        };
        Stmt { kind, span: start }
    }

    fn parse_block(&mut self) -> Option<Block> {
        self.nested(|p| p.with_struct_expr(true, Self::parse_block_inner))
    }

    fn parse_block_inner(&mut self) -> Option<Block> {
        let lifted = self.lifted.len();
        let mut block = self.parse_block_stmts()?;
        self.statement_closure_returns(&block.stmts, block.trailing.as_deref());
        self.rename_local_items(&mut block, lifted);
        // A nested fn and a `use` hold in their whole
        // block - in the bodies of the fns lifted from inside it too.
        let markers: Vec<Stmt> = block
            .stmts
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Item(Item::Fn(f)) => {
                    rokc_ast::nested_marker_of(f).map(|(lifted, _)| Stmt {
                        kind: StmtKind::Item(Item::Fn(nested_marker(f, lifted, true))),
                        span: s.span,
                    })
                }
                StmtKind::Item(Item::Use(u)) => {
                    let mut u = u.clone();
                    u.attrs.push(Attribute {
                        path: vec![rokc_ast::INHERITED_USE_ATTR.to_owned()],
                        body: AttrBody::Empty,
                        inner: false,
                        span: u.span,
                    });
                    Some(Stmt {
                        kind: StmtKind::Item(Item::Use(u)),
                        span: s.span,
                    })
                }
                _ => None,
            })
            .collect();
        if !markers.is_empty() {
            for item in &mut self.lifted[lifted..] {
                let bodies: Vec<&mut FnDef> = match item {
                    Item::Fn(f) => vec![f],
                    Item::Impl(i) => (i.items.iter_mut())
                        .filter_map(|ii| match ii {
                            ImplItem::Method(m) => Some(m),
                            _ => None,
                        })
                        .collect(),
                    Item::Trait(t) => (t.items.iter_mut())
                        .filter_map(|ti| match ti {
                            TraitItem::Method(m) => Some(m),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                for b in bodies.into_iter().filter_map(|f| f.body.as_mut()) {
                    b.stmts.splice(0..0, markers.iter().cloned());
                }
            }
        }
        Some(block)
    }

    /// A `fn` statement: lifted to item `outer/name`
    /// (`outer/name/2` on repeats), leaving a marker statement.
    fn parse_nested_fn(&mut self) -> Option<Stmt> {
        let start = self.peek_span();
        if self.parse_visibility() != Visibility::Private {
            self.error(
                Code::MISPLACED_VISIBILITY,
                start,
                "`pub` is meaningless on a nested function: it is visible only in its block \
                ",
            );
        }
        let mut def = self.parse_fn(Visibility::Private, Vec::new())?;
        let lifted = self.lift_name(&def.name);
        let marker = nested_marker(&def, &lifted, false);
        def.name = lifted;
        self.lifted.push(Item::Fn(def));
        Some(Stmt {
            kind: StmtKind::Item(Item::Fn(marker)),
            span: start,
        })
    }

    /// The lifted path of the fn being parsed (`f`, `f/g`, `Type.m`).
    fn lift_outer(&self) -> String {
        if self.fn_names.is_empty() {
            "main".to_owned()
        } else {
            self.fn_names.join("/")
        }
    }

    /// The unique module item name `outer/name` for `name` declared in
    /// the body of the fn being parsed (`rokc_core::local`).
    fn lift_name(&mut self, name: &str) -> String {
        let base = format!("{}/{name}", self.lift_outer());
        let mut lifted = base.clone();
        let mut n = 1;
        while !self.lifted_names.insert(lifted.clone()) {
            n += 1;
            lifted = format!("{base}/{n}");
        }
        lifted
    }

    /// Does a block-local item start here?
    fn at_local_item(&mut self) -> bool {
        let save = self.pos;
        self.pos = self.past_attrs(save);
        if self.eat(&TokenKind::Pub) && matches!(self.peek(), TokenKind::LParen) {
            self.pos += 3;
        }
        if matches!(self.peek(), TokenKind::Unsafe)
            && matches!(self.peek_next(), Some(TokenKind::Impl | TokenKind::Trait))
        {
            self.bump();
        }
        let at = matches!(
            self.peek(),
            TokenKind::Struct | TokenKind::Enum | TokenKind::Trait | TokenKind::Impl
        ) || self.at_union_item()
            || (self.at_kw("type") || self.at_kw("static")) && self.at_ctx_item()
            || self.at_kw("const") && matches!(self.peek_next(), Some(TokenKind::Ident(_)));
        self.pos = save;
        at
    }

    /// A block-local item and its expansions, each lifted to a
    /// private item named by `lift_name` with a copy left as a statement
    /// (renamed at the block's end; HIR checks it captures nothing).
    /// `None` after an error; empty when cfg drops it.
    fn parse_local_item(&mut self) -> Option<Vec<Stmt>> {
        let start = self.peek_span();
        let (pending, lifted) = (self.pending_items.len(), self.lifted.len());
        let first = self.parse_item_lifting();
        if first.is_none() && !std::mem::take(&mut self.vanished) {
            return None;
        }
        let items: Vec<Item> = first
            .into_iter()
            .chain(self.pending_items.drain(pending..))
            .collect();
        let attrs = items.first().map_or(&[][..], |i| i.attrs().as_slice());
        if self.cfg_drops(attrs, (self.pending_stmts.len(), lifted)) {
            return Some(Vec::new());
        }
        let mut stmts = Vec::new();
        for mut item in items {
            let span = item.span();
            let (vis, name) = match &mut item {
                Item::Struct(x) => (x.vis, Some(&mut x.name)),
                Item::Enum(x) => (x.vis, Some(&mut x.name)),
                Item::Union(x) => (x.vis, Some(&mut x.name)),
                Item::Trait(x) => (x.vis, Some(&mut x.name)),
                Item::TypeAlias(x) => (x.vis, Some(&mut x.name)),
                Item::Static(x) => (x.vis, Some(&mut x.name)),
                Item::Fn(x) => (x.vis, Some(&mut x.name)),
                Item::Const(x) => (x.vis, Some(&mut x.name)),
                Item::Impl(x) => (x.vis, None),
                Item::Mod(_) | Item::Use(_) | Item::Extern(_) => {
                    self.error(
                        Code::INVALID_META_BLOCK,
                        span,
                        "a meta expansion in a function body gives only `struct`, `enum`, \
                         `union`, `trait`, `impl`, `type`, `static`, `fn` and `const` items \
                        ",
                    );
                    continue;
                }
            };
            if vis != Visibility::Private {
                self.error(
                    Code::MISPLACED_VISIBILITY,
                    start,
                    "`pub` is meaningless on an item in a function body: it is visible only \
                     in its block",
                );
            }
            if let Some(name) = name {
                *name = self.lift_name(name);
            }
            item.attrs_mut().push(Attribute {
                path: vec![rokc_ast::LOCAL_ITEM_ATTR.to_owned()],
                body: AttrBody::Args(vec![AttrArg::Lit(Literal::Str(self.lift_outer()))]),
                inner: false,
                span,
            });
            stmts.push(Stmt {
                kind: StmtKind::Item(item.clone()),
                span,
            });
            self.lifted.push(item);
        }
        Some(stmts)
    }

    /// A block's local items shadow outer names of their
    /// namespace in the whole block and in items lifted from it.
    fn rename_local_items(&mut self, block: &mut Block, lifted: usize) {
        let mut r = rokc_ast::rename::Renames::default();
        // Per namespace (types, values): where each name is first.
        let mut first: [std::collections::HashMap<String, Span>; 2] = Default::default();
        for s in &block.stmts {
            let StmtKind::Item(item) = &s.kind else {
                continue;
            };
            let Some(name) = item.name().filter(|_| item.is_local()) else {
                continue;
            };
            let (ty, value) = match item {
                Item::Struct(x) => (true, !matches!(x.kind, StructKind::Named(_))),
                Item::Enum(_) | Item::Union(_) | Item::Trait(_) | Item::TypeAlias(_) => {
                    (true, false)
                }
                _ => (false, true),
            };
            let source = rokc_core::local::leaf(name).into_owned();
            let twice = ty && r.types.insert(source.clone(), name.to_owned()).is_some()
                || value && r.values.insert(source.clone(), name.to_owned()).is_some();
            let ns: [bool; 2] = (ty, value).into();
            if twice {
                let at = ns
                    .iter()
                    .zip(&first)
                    .find_map(|(in_ns, m)| m.get(&source).filter(|_| *in_ns));
                self.diagnostics.push(
                    rokc_diagnostic::duplicate(
                        Code::DUPLICATE_DEFINITION,
                        format!("`{source}` is defined twice in this block"),
                        &source,
                        s.span,
                        at.copied(),
                        matches!(item, Item::Fn(_)),
                    )
                    .with_file(self.file.clone()),
                );
            }
            for (in_ns, m) in ns.into_iter().zip(&mut first) {
                if in_ns {
                    m.entry(source.clone()).or_insert(s.span);
                }
            }
        }
        // An item and a `use` of its name in one block.
        let nested: Vec<&str> = block
            .stmts
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Item(Item::Fn(f)) if rokc_ast::nested_marker_of(f).is_some() => {
                    Some(f.name.as_str())
                }
                _ => None,
            })
            .collect();
        for s in &block.stmts {
            let StmtKind::Item(Item::Use(u)) = &s.kind else {
                continue;
            };
            let mut used = Vec::new();
            rokc_ast::rename::use_names(&u.tree, &mut used);
            for n in used {
                if r.types.contains_key(n) || r.values.contains_key(n) || nested.contains(&n) {
                    self.error_help(
                        Code::IMPORT_CONFLICT,
                        s.span,
                        format!("`{n}` is imported here and also defined in this block"),
                        format!("rename the import: `use ... as {n}2;`, or remove one of them"),
                    );
                }
            }
        }
        if !r.is_empty() {
            r.block(block);
            for item in &mut self.lifted[lifted..] {
                r.item(item);
            }
        }
        // The block's `use`s hold in its items too: fn bodies get copies
        // (`parse_block_inner`), the rest the paths.
        let mut uses = rokc_ast::rename::Renames {
            headers_only: true,
            ..Default::default()
        };
        // Other globs: their names are known only to HIR.
        let mut module_globs = Vec::new();
        for s in &block.stmts {
            let StmtKind::Item(Item::Use(u)) = &s.kind else {
                continue;
            };
            let (mut named, mut globs) = (Vec::new(), Vec::new());
            rokc_ast::rename::use_paths(&u.tree, &[], &mut named, &mut globs);
            // `use E::*` of a local enum: its variants.
            for mut prefix in globs {
                let variants = block.stmts.iter().find_map(|s| match &s.kind {
                    StmtKind::Item(Item::Enum(e)) if prefix == [e.name.clone()] => Some(
                        e.variants
                            .iter()
                            .map(|v| v.name.clone())
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                });
                if let Some(vs) = variants {
                    named.extend(
                        vs.into_iter()
                            .map(|v| (v.clone(), vec![prefix[0].clone(), v])),
                    );
                    continue;
                }
                if let Some(head) = prefix.first().and_then(|h| uses.paths.get(h)) {
                    prefix.splice(0..1, head.clone());
                }
                module_globs.push(prefix.join("::"));
            }
            for (name, mut path) in named {
                // `use a::b; use b::c;`
                if let Some(head) = uses.paths.get(&path[0]).cloned() {
                    path.splice(0..1, head);
                }
                uses.paths.insert(name, path);
            }
        }
        if !uses.is_empty() {
            for item in &mut self.lifted[lifted..] {
                uses.item(item);
            }
        }
        if !module_globs.is_empty() {
            let span = block.span;
            for item in &mut self.lifted[lifted..] {
                item.attrs_mut().push(Attribute {
                    path: vec![rokc_ast::BLOCK_GLOB_ATTR.to_owned()],
                    body: AttrBody::Args(
                        (module_globs.iter())
                            .map(|g| AttrArg::Lit(Literal::Str(g.clone())))
                            .collect(),
                    ),
                    inner: false,
                    span,
                });
            }
        }
    }

    /// Does a nested `fn` (`pub`, `unsafe` / `async` / `yield`) start here?
    fn at_nested_fn(&self) -> bool {
        let mut i = self.pos;
        let kind = |i: usize| self.tokens.get(i).map(|t| &t.kind);
        if matches!(kind(i), Some(TokenKind::Pub)) {
            i += 1;
            if matches!(kind(i), Some(TokenKind::LParen)) {
                i += 3;
            }
        }
        while matches!(
            kind(i),
            Some(TokenKind::Unsafe | TokenKind::Async | TokenKind::Yield)
        ) {
            i += 1;
        }
        matches!(kind(i), Some(TokenKind::Fn)) && matches!(kind(i + 1), Some(TokenKind::Ident(_)))
    }

    fn parse_block_stmts(&mut self) -> Option<Block> {
        let start = self.peek_span();
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut stmts = Vec::new();
        let mut trailing: Option<Box<Expr>> = None;
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            let from = self.pos;
            // An empty statement (`;;`).
            if self.eat(&TokenKind::Semi) {
                continue;
            }
            // `name!(..);` at statement position expands to
            // statements / local items spliced here.
            if let Some(sts) = self.macro_call_stmts() {
                stmts.extend(sts);
                continue;
            }
            if self.at_local_item() {
                match self.parse_local_item() {
                    Some(items) => stmts.extend(items),
                    None => self.recover(from, Recover::Stmt),
                }
                continue;
            }
            let attrs = self.cfg_only_attrs("a statement");
            let marks = (self.pending_stmts.len(), self.lifted.len());
            if let Some((stmt, is_trailing)) = self.parse_stmt() {
                if self.cfg_drops(&attrs, marks) {
                    continue;
                }
                if is_trailing && matches!(self.peek(), TokenKind::RBrace) {
                    if let StmtKind::Trailing(expr) = stmt.kind {
                        trailing = Some(Box::new(expr));
                    } else {
                        stmts.push(stmt);
                    }
                } else {
                    let stmt = match stmt.kind {
                        StmtKind::Trailing(e) if !ends_stmt(&e) => {
                            let d =
                                self.expected(&TokenKind::Semi, "`;` after expression statement");
                            self.diagnostics.push(d);
                            Stmt {
                                kind: StmtKind::Trailing(e),
                                span: stmt.span,
                            }
                        }
                        // A trailing-closure call ending at its `}` is an
                        // expression statement, as if `;` were written:
                        // its value is dropped, not checked.
                        StmtKind::Trailing(e) if !is_block_like(&e) => Stmt {
                            kind: StmtKind::Expr(e),
                            span: stmt.span,
                        },
                        kind => Stmt {
                            kind,
                            span: stmt.span,
                        },
                    };
                    stmts.push(stmt);
                }
                // Follow-ups queued by the statement (nested tuple
                // destructure).
                while let Some(extra) = self.pending_stmts.pop() {
                    stmts.push(extra);
                }
            } else {
                self.pending_stmts.clear();
                self.recover(from, Recover::Stmt);
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some(Block {
            stmts,
            trailing,
            span: start,
            label: None,
        })
    }

    /// `(stmt, may be the tail)`; the block builder decides.
    #[allow(clippy::cognitive_complexity)] // one arm per statement form
    fn parse_stmt(&mut self) -> Option<(Stmt, bool)> {
        let start = self.peek_span();
        if self.at_nested_fn() {
            return Some((self.parse_nested_fn()?, false));
        }
        // Module-level only (other items lift out of blocks).
        let module_only = match self.peek() {
            TokenKind::Mod => Some("a `mod`"),
            _ if self.at_kw("extern") && self.at_ctx_item() => Some("an `extern` block"),
            _ => None,
        };
        if let Some(what) = module_only {
            self.error(
                Code::MISPLACED_ITEM,
                start,
                format!("{what} is declared only at module level, not in a function body"),
            );
            return None;
        }
        match self.peek() {
            TokenKind::Use => {
                let vis = Visibility::Private;
                let attrs = Vec::new();
                let Some(use_def) = self.parse_use(vis, attrs) else {
                    if std::mem::take(&mut self.vanished) {
                        self.error(
                            Code::MISPLACED_ITEM,
                            start,
                            "a macro import goes at module level: move this `use dep::name!;` \
                             to the top of the file",
                        );
                    }
                    return None;
                };
                Some((
                    Stmt {
                        kind: StmtKind::Item(Item::Use(use_def)),
                        span: start,
                    },
                    false,
                ))
            }
            TokenKind::Let => {
                self.bump();
                // Flat forms (`Point { x, y: b }`, `(a, ..rest)`, `name`)
                // have their own statements; any other irrefutable pattern
                // is a `LetPattern`.
                let general = match self.peek() {
                    TokenKind::Ident(_) => {
                        matches!(
                            self.peek_next(),
                            Some(TokenKind::LParen | TokenKind::ColonColon)
                        ) && !self.peek_path_then_lbrace()
                    }
                    TokenKind::LParen => {
                        self.backtracks(|p| p.parse_let_tuple_pattern(start))
                            || self.stmt_has_else()
                    }
                    _ => false,
                } || (self.peek_path_then_lbrace()
                    && self.backtracks(|p| {
                        p.parse_let_struct_fields()
                            .filter(|_| matches!(p.peek(), TokenKind::Colon | TokenKind::Eq))
                    }));
                if general {
                    let pat = self.with_struct_expr(true, Self::parse_pattern_atom)?;
                    let ty = if self.eat(&TokenKind::Colon) {
                        Some(self.parse_type()?)
                    } else {
                        None
                    };
                    self.expect(&TokenKind::Eq, "`=`")?;
                    let init = self.parse_expr()?;
                    if self.eat(&TokenKind::Else) {
                        let else_block = self.parse_block()?;
                        self.stmt_semi()?;
                        return Some((
                            self.let_else_pattern(pat, ty, init, else_block, start),
                            false,
                        ));
                    }
                    self.stmt_semi()?;
                    return Some((
                        Stmt {
                            kind: StmtKind::LetPattern { pat, ty, init },
                            span: start,
                        },
                        false,
                    ));
                }
                // `let Point { x, y, .. } [: ty] = init;`: `let Path {`
                // can't start a plain binding, so it is a struct pattern.
                if self.peek_path_then_lbrace() {
                    let (path, fields, rest) = self.parse_let_struct_fields()?;
                    let ty = if self.eat(&TokenKind::Colon) {
                        Some(self.parse_type()?)
                    } else {
                        None
                    };
                    self.expect(&TokenKind::Eq, "`=`")?;
                    let init = self.parse_expr()?;
                    self.stmt_semi()?;
                    return Some((
                        Stmt {
                            kind: StmtKind::LetStruct {
                                path,
                                fields,
                                rest,
                                ty,
                                init,
                            },
                            span: start,
                        },
                        false,
                    ));
                }
                // `let (a, b) = ..;`. Nested groups become
                // a chain of `LetTuple`s queued on `pending_stmts`.
                if matches!(self.peek(), TokenKind::LParen) {
                    let (mut names, rest, trailing_comma) = self.parse_let_tuple_pattern(start)?;
                    let ty = if self.eat(&TokenKind::Colon) {
                        Some(self.parse_type()?)
                    } else {
                        None
                    };
                    self.expect(&TokenKind::Eq, "`=`")?;
                    let init = self.parse_expr()?;
                    self.stmt_semi()?;
                    // `let (a) = ..` only groups; `let (a,) = ..` is a 1-tuple.
                    let kind = if names.len() == 1 && rest.is_none() && !trailing_comma {
                        StmtKind::Let {
                            name: names.pop()?,
                            ty,
                            init,
                        }
                    } else {
                        StmtKind::LetTuple {
                            names,
                            rest,
                            ty,
                            init,
                        }
                    };
                    return Some((Stmt { kind, span: start }, false));
                }
                let mut name = if let TokenKind::Ident(n) = self.peek() {
                    n.clone()
                } else {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!("expected binding name, found {}", self.peek().describe()),
                    );
                    return None;
                };
                self.bump();
                // `let mut x` (Rust): every binding is mutable already.
                if let (true, TokenKind::Ident(n)) = (name == "mut", self.peek().clone()) {
                    let at = self.prev_span().unwrap_or(start);
                    self.error_help(
                        Code::FOREIGN_SYNTAX,
                        at,
                        "rok has no `let mut`: variables are mutable by default",
                        format!("drop `mut`: `let {n} = ..;`"),
                    );
                    self.bump();
                    name = n;
                }
                // `let whole @ (a, b) = init;` → `let whole = init;` then
                // a destructure of `whole`.
                let sub = if self.eat(&TokenKind::At) {
                    let s = self.peek_span();
                    Some(self.parse_let_sub_pattern(&name, s)?)
                } else {
                    None
                };
                let ty = if self.eat(&TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                // `let x: i32;` (C, Java, TypeScript): no value yet.
                if let (TokenKind::Semi | TokenKind::RBrace, Some(t)) = (self.peek(), &ty) {
                    let t = self.text_in(t.span);
                    let at = self.peek_span();
                    self.error_help(
                        Code::LET_WITHOUT_VALUE,
                        at,
                        format!("`let {name}` needs a value"),
                        format!(
                            "give it one, `let {name}: {t} = ..;`, or start it empty: `let \
                             {name}: {t} | none = none;`"
                        ),
                    );
                    return None;
                }
                self.expect(&TokenKind::Eq, "`=`")?;
                let init = self.parse_expr()?;
                if let Some(sub) = sub {
                    self.stmt_semi()?;
                    self.pending_stmts.push(sub);
                    return Some((
                        Stmt {
                            kind: StmtKind::Let { name, ty, init },
                            span: start,
                        },
                        false,
                    ));
                }
                // `let name [: T] = expr else { .. };` →
                // `let name = match expr { v: T => v, _ => { else } };`
                if matches!(self.peek(), TokenKind::Else) {
                    self.bump();
                    let else_block = self.parse_block()?;
                    self.stmt_semi()?;
                    let init_span = init.span;
                    let bk_span = else_block.span;
                    let binder = "__letelse_v".to_string();
                    let success_pat = if let Some(t) = &ty {
                        Pattern {
                            kind: PatternKind::TypeCheck {
                                binder: Some(binder.clone()),
                                ty: t.clone(),
                            },
                            span: init_span,
                        }
                    } else {
                        Pattern {
                            kind: PatternKind::Binding {
                                name: binder.clone(),
                                sub: None,
                            },
                            span: init_span,
                        }
                    };
                    let success_body = Expr {
                        kind: ExprKind::Path(Path {
                            segments: vec![PathSegment {
                                name: binder,
                                generics: vec![],
                            }],
                            span: init_span,
                            origin: None,
                        }),
                        span: init_span,
                    };
                    let else_expr = Expr {
                        kind: ExprKind::Block(else_block),
                        span: bk_span,
                    };
                    let arms = vec![
                        MatchArm {
                            pat: success_pat,
                            guard: None,
                            body: success_body,
                        },
                        MatchArm {
                            pat: Pattern {
                                kind: PatternKind::Wildcard,
                                span: bk_span,
                            },
                            guard: None,
                            body: else_expr,
                        },
                    ];
                    let match_expr = Expr {
                        kind: ExprKind::Match {
                            scrutinee: Box::new(init),
                            arms,
                        },
                        span: init_span,
                    };
                    return Some((
                        Stmt {
                            kind: StmtKind::Let {
                                name,
                                ty,
                                init: match_expr,
                            },
                            span: start,
                        },
                        false,
                    ));
                }
                self.stmt_semi()?;
                Some((
                    Stmt {
                        kind: StmtKind::Let { name, ty, init },
                        span: start,
                    },
                    false,
                ))
            }
            TokenKind::Return => {
                let at = self.bump().span;
                let labeled = self.return_label(at);
                let value = if matches!(self.peek(), TokenKind::Semi | TokenKind::RBrace) {
                    if !labeled {
                        self.bare_return(at);
                    }
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.stmt_semi()?;
                Some((
                    Stmt {
                        kind: StmtKind::Return(value),
                        span: start,
                    },
                    false,
                ))
            }
            TokenKind::Break => {
                self.bump();
                // Optional `'label` after `break`.
                let label = if let TokenKind::Label(name) = self.peek().clone() {
                    self.bump();
                    Some(name)
                } else {
                    None
                };
                let value = if matches!(self.peek(), TokenKind::Semi | TokenKind::RBrace) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.stmt_semi()?;
                Some((
                    Stmt {
                        kind: StmtKind::Break { label, value },
                        span: start,
                    },
                    false,
                ))
            }
            TokenKind::Continue => {
                self.bump();
                let label = if let TokenKind::Label(name) = self.peek().clone() {
                    self.bump();
                    Some(name)
                } else {
                    None
                };
                self.stmt_semi()?;
                Some((
                    Stmt {
                        kind: StmtKind::Continue { label },
                        span: start,
                    },
                    false,
                ))
            }
            TokenKind::Throw => {
                self.bump();
                let value = self.parse_expr()?;
                self.stmt_semi()?;
                Some((
                    Stmt {
                        kind: StmtKind::Throw(value),
                        span: start,
                    },
                    false,
                ))
            }
            // A block-like expression ends its statement; no `;`,
            // and a following operator starts a new one (`if c {} -1` is
            // two). Only postfix links (`.m()`, `?`) continue it.
            TokenKind::If
            | TokenKind::Match
            | TokenKind::Loop
            | TokenKind::While
            | TokenKind::For
            | TokenKind::LBrace
            | TokenKind::Label(_) => {
                self.stmt_head = true;
                let expr = self.parse_expr_bp(u32::MAX)?;
                let ends = is_block_like(&expr) || matches!(self.peek(), TokenKind::RBrace);
                if !ends || self.eat(&TokenKind::Semi) {
                    if !ends {
                        self.expect(&TokenKind::Semi, "`;`")?;
                    }
                    return Some((
                        Stmt {
                            kind: StmtKind::Expr(expr),
                            span: start,
                        },
                        false,
                    ));
                }
                Some((
                    Stmt {
                        kind: StmtKind::Trailing(expr),
                        span: start,
                    },
                    true,
                ))
            }
            _ => {
                let outer = self.stmt_at.replace(start.start);
                let expr = self.parse_expr();
                self.stmt_at = outer;
                let expr = expr?;
                if matches!(self.peek(), TokenKind::Eq) {
                    self.bump();
                    let value = self.parse_expr()?;
                    self.stmt_semi()?;
                    return Some((
                        Stmt {
                            kind: StmtKind::Assign(expr, value),
                            span: start,
                        },
                        false,
                    ));
                }
                // `target op= value;`: HIR uses the `*Assign` method, else
                // `target = target op value`. The target is evaluated once:
                // `a[f()] += 1` → `{ let t = f(); a[t] += 1; }`.
                if let Some(op) = compound_assign_op(self.peek()) {
                    self.bump();
                    let value = self.parse_expr()?;
                    self.stmt_semi()?;
                    let mut stmts = Vec::new();
                    let target = self.hoist_place(expr, &mut stmts);
                    let assign = Stmt {
                        kind: StmtKind::CompoundAssign {
                            target,
                            op: format!("{op}="),
                            value,
                        },
                        span: start,
                    };
                    if stmts.is_empty() {
                        return Some((assign, false));
                    }
                    stmts.push(assign);
                    let block = Block {
                        stmts,
                        trailing: None,
                        span: start,
                        label: None,
                    };
                    return Some((
                        Stmt {
                            kind: StmtKind::Expr(Expr {
                                kind: ExprKind::Block(block),
                                span: start,
                            }),
                            span: start,
                        },
                        false,
                    ));
                }
                if self.eat(&TokenKind::Semi) {
                    Some((
                        Stmt {
                            kind: StmtKind::Expr(expr),
                            span: start,
                        },
                        false,
                    ))
                } else {
                    Some((
                        Stmt {
                            kind: StmtKind::Trailing(expr),
                            span: start,
                        },
                        true,
                    ))
                }
            }
        }
    }

    /// The `;` ending a statement; optional before `}`,
    /// where leaving it out means the same as writing it.
    fn stmt_semi(&mut self) -> Option<()> {
        if !matches!(self.peek(), TokenKind::RBrace) {
            self.expect(&TokenKind::Semi, "`;`")?;
        }
        Some(())
    }

    /// Expression including ranges: `a..b`, `a..=b`, `..b`,
    /// `a..`, `..`. Ranges bind weakest (prec 0) and do not chain.
    fn parse_expr(&mut self) -> Option<Expr> {
        let span = self.peek_span();
        let start = if matches!(self.peek(), TokenKind::DotDot | TokenKind::DotDotEq) {
            None
        } else {
            let e = self.parse_expr_bp(0)?;
            Some(Box::new(self.parse_catch_tail(e)?))
        };
        let inclusive = match self.peek() {
            TokenKind::DotDot => false,
            TokenKind::DotDotEq => true,
            _ => return start.map(|e| *e),
        };
        self.bump();
        let end = if !inclusive && !self.can_start_range_end() {
            None
        } else {
            Some(Box::new(self.parse_expr_bp(0)?))
        };
        if matches!(self.peek(), TokenKind::DotDot | TokenKind::DotDotEq) {
            let s = self.peek_span();
            self.error(
                Code::NEEDS_PARENTHESES,
                s,
                "range operators cannot be chained",
            );
            // Skip the rest of the chain, so it is not a second error.
            while matches!(self.peek(), TokenKind::DotDot | TokenKind::DotDotEq) {
                self.bump();
                if self.can_start_range_end() {
                    self.parse_expr_bp(0)?;
                }
            }
        }
        Some(Expr {
            span: self.to_prev(span),
            kind: ExprKind::Range {
                start,
                end,
                inclusive,
            },
        })
    }

    /// `catch pat? { .. }` arms and `finally { .. }` around `body`;
    /// loosest binding, so `try a() + b() catch _ { 0 }` covers both.
    fn parse_catch_tail(&mut self, body: Expr) -> Option<Expr> {
        if !(self.at_kw("catch") || self.at_kw("finally")) {
            return Some(body);
        }
        let mut catches: Vec<CatchArm> = Vec::new();
        while self.at_kw("catch") {
            let span = self.peek_span();
            self.bump();
            let pat = if matches!(self.peek(), TokenKind::LBrace) {
                Pattern {
                    kind: PatternKind::Wildcard,
                    span,
                }
            } else {
                // `catch E { code, .. } { body }`: a struct pattern when a
                // second block follows the first.
                let struct_pat = self.braces_then_block();
                self.with_struct_expr(struct_pat, Self::parse_pattern)?
            };
            let arm_body = self.parse_block()?;
            catches.push(CatchArm {
                pat,
                body: arm_body,
            });
        }
        let finally = if self.eat_kw("finally") {
            Some(self.parse_block()?)
        } else {
            None
        };
        Some(Expr {
            span: self.to_prev(body.span),
            kind: ExprKind::Catch {
                body: Box::new(body),
                catches,
                finally,
            },
        })
    }

    /// After `..`: does an end operand follow (vs. `a..` / `..`)?
    fn can_start_range_end(&self) -> bool {
        match self.peek() {
            TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::RBrace
            | TokenKind::Comma
            | TokenKind::Semi
            | TokenKind::FatArrow
            | TokenKind::Eq
            | TokenKind::Eof => false,
            TokenKind::LBrace => self.struct_expr_allowed,
            _ => true,
        }
    }

    /// Pratt loop. `min_bp` is the minimum binding power that the next
    /// infix/postfix operator must exceed for the loop to consume it.
    fn parse_expr_bp(&mut self, min_bp: u32) -> Option<Expr> {
        let chain = self.chain;
        let r = self.nested(|p| p.parse_expr_bp_inner(min_bp));
        self.chain = chain;
        r
    }

    fn parse_expr_bp_inner(&mut self, min_bp: u32) -> Option<Expr> {
        let mut lhs = if let Some((sym, span)) = self.peek_op_symbol(Fixity::Prefix) {
            let (prec, _) = self.ext.level(&sym, Fixity::Prefix)?;
            self.bump();
            let rhs = self.parse_expr_bp(prec)?;
            Expr {
                kind: ExprKind::OpPrefix {
                    op: sym,
                    operand: Box::new(rhs),
                },
                span: self.to_prev(span),
            }
        } else {
            self.parse_primary()?
        };

        // Precedence of the last non-associative op, to reject `a < b < c`.
        let mut last_none_prec: Option<u32> = None;

        loop {
            self.chain_link()?;
            // `as` / `is` sit at level 11, left-assoc - below
            // prefix unary (`-x as i64` is `(-x) as i64`), above arithmetic.
            if matches!(self.peek(), TokenKind::As | TokenKind::Is) {
                if CAST_PREC < min_bp {
                    break;
                }
                let is_cast = matches!(self.peek(), TokenKind::As);
                self.bump();
                let ty_span = self.peek_span();
                let ty = self.parse_cast_type()?;
                let span = lhs.span;
                let kind = if is_cast {
                    ExprKind::Cast(Box::new(lhs), ty)
                } else {
                    // `v is T` - runtime type test.
                    ExprKind::Is(
                        Box::new(lhs),
                        rokc_ast::Pattern {
                            kind: rokc_ast::PatternKind::TypeCheck { binder: None, ty },
                            span: ty_span,
                        },
                    )
                };
                lhs = Expr {
                    kind,
                    span: self.to_prev(span),
                };
                continue;
            }

            if self.incr_decr(&lhs) {
                break;
            }
            let Some((sym, _)) = self.peek_op_symbol(Fixity::Infix) else {
                break;
            };
            // `??` is a language-level desugar, level 1,
            // right-assoc.
            if sym == "??" {
                if COALESCE_PREC < min_bp || self.closure_stmt_ends(&lhs) {
                    break;
                }
                self.bump();
                let rhs = self.parse_expr_bp(COALESCE_PREC)?;
                let span = self.to_prev(lhs.span);
                lhs = Expr {
                    kind: ExprKind::NullishCoalesce(Box::new(lhs), Box::new(rhs)),
                    span,
                };
                continue;
            }
            let Some((prec, assoc)) = self.ext.level(&sym, Fixity::Infix) else {
                break;
            };
            let (left_bp, right_bp) = match assoc {
                Assoc::Left | Assoc::None => (prec, prec + 1),
                Assoc::Right => (prec + 1, prec),
            };
            if left_bp < min_bp || self.closure_stmt_ends(&lhs) {
                break;
            }
            // Non-associative ops don't chain. Without this
            // the loop would build `(a < b) < c`.
            if last_none_prec == Some(prec) {
                let span = self.peek_span();
                self.error(
                    Code::NEEDS_PARENTHESES,
                    span,
                    format!(
                        "non-associative operator `{sym}` cannot be chained \
                         (parenthesise to disambiguate)"
                    ),
                );
            }
            self.bump();
            let rhs = self.parse_expr_bp(right_bp)?;
            let span = self.to_prev(lhs.span);
            lhs = Expr {
                kind: ExprKind::OpInfix {
                    op: sym,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
            last_none_prec = matches!(assoc, Assoc::None).then_some(prec);
        }
        if min_bp == 0 && self.ternary_ahead() {
            return self.ternary(lhs);
        }
        Some(lhs)
    }

    /// `try? f() ?? d` (operand from token `from`) is an error: it would
    /// be `try? (f() ?? d)`, where an error gives `none`, not `d`.
    fn try_coalesce(&mut self, at: Span, from: usize, operand: &Expr) {
        if !matches!(operand.kind, ExprKind::NullishCoalesce(..)) || self.pending.is_some() {
            return;
        }
        let toks = &self.tokens[from..self.pos];
        let mut depth = 0i32;
        let mut split = None;
        let mut closed = None;
        for (i, t) in toks.iter().enumerate() {
            match t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 && closed.is_none() {
                        closed = Some(i);
                    }
                }
                TokenKind::QuestionQuestion if depth == 0 && split.is_none() => split = Some(i),
                _ => {}
            }
        }
        // `try? (f() ?? d)`: already grouped.
        if toks.first().is_some_and(|t| t.kind == TokenKind::LParen)
            && closed == Some(toks.len() - 1)
        {
            return;
        }
        let Some(split) = split else { return };
        let Some(last) = toks.last() else { return };
        let (l, r) = (tokens_text(&toks[..split]), tokens_text(&toks[split + 1..]));
        let whole = Span {
            end: last.span.end,
            ..at
        };
        // Long operands read better as placeholders.
        let (sl, sr) = if l.len() + r.len() > 60 {
            ("x", "d")
        } else {
            (l.as_str(), r.as_str())
        };
        self.diagnostics.push(
            RokDiagnostic::error(
                Code::NEEDS_PARENTHESES,
                at,
                format!("`try? {sl} ?? {sr}` needs parentheses"),
            )
            .with_file(self.file.clone())
            .with_help(format!(
                "write `(try? {sl}) ?? {sr}` to get `{sr}` on an error, or \
                 `try? ({sl} ?? {sr})` to get `none` on an error"
            ))
            .with_fix(
                whole,
                format!("(try? {l}) ?? {r}"),
                format!("write `(try? {sl}) ?? {sr}`"),
            )
            .with_fix(
                whole,
                format!("try? ({l} ?? {r})"),
                format!("write `try? ({sl} ?? {sr})`"),
            ),
        );
    }

    /// `f` parsing a closure or fn body whose `return@name` may name
    /// `label`; with the bare `return;`s directly in it.
    fn ret_frame<T>(
        &mut self,
        label: Option<String>,
        f: impl FnOnce(&mut Self) -> Option<T>,
    ) -> (Option<T>, Vec<Span>) {
        self.ret_frames.push(RetFrame {
            label,
            bare: Vec::new(),
        });
        let r = f(self);
        let frame = self.ret_frames.pop().unwrap_or_default();
        (r, frame.bare)
    }

    /// The bare `return;`s of the closure starting at `span`.
    fn note_closure_returns(&mut self, span: Span, bare: Vec<Span>) {
        let key = (span.file, span.start);
        if bare.is_empty() {
            self.closure_returns.remove(&key);
        } else {
            self.closure_returns.insert(key, bare);
        }
    }

    /// A bare `return;` (its `return` at `at`).
    fn bare_return(&mut self, at: Span) {
        if let Some(f) = self.ret_frames.last_mut() {
            f.bare.push(at);
        }
    }

    /// `return@name` (`@name` glued to the `return` at `at`): leaves the
    /// innermost closure, which must be the trailing closure of a call
    /// to `name`. Whether there is one.
    fn return_label(&mut self, at: Span) -> bool {
        let glued = self.peek_span().start == at.end;
        if !glued || self.peek() != &TokenKind::At {
            return false;
        }
        let Some(TokenKind::Ident(name)) = self.tokens.get(self.pos + 1).map(|t| t.kind.clone())
        else {
            return false;
        };
        self.bump(); // `@`
        let span = self.bump_name().span;
        let inner = self.ret_frames.last().and_then(|f| f.label.as_deref());
        if inner == Some(name.as_str()) {
            return true;
        }
        let help = match inner {
            Some(l) => format!(
                "the innermost closure here is the trailing closure of `{l}`: write `return@{l}`"
            ),
            None => "`return@name` leaves the trailing closure of a call to `name`; \
                     here, write `return`"
                .to_owned(),
        };
        self.diagnostics.push(
            RokDiagnostic::error(
                Code::TRAILING_CLOSURE_RETURN,
                span,
                format!("`return@{name}`: no enclosing trailing closure of a call to `{name}`"),
            )
            .with_file(self.file.clone())
            .with_help(help),
        );
        true
    }

    /// A bare `return;` in a statement's trailing closure is an error: it
    /// would leave only the closure, not the function.
    fn statement_closure_returns(&mut self, stmts: &[Stmt], tail: Option<&Expr>) {
        let exprs = stmts.iter().filter_map(|s| match &s.kind {
            StmtKind::Expr(e) | StmtKind::Trailing(e) => Some(e),
            _ => None,
        });
        let mut found = Vec::new();
        for e in exprs.chain(tail) {
            let (ExprKind::Call {
                trailing_closure: Some(c),
                ..
            }
            | ExprKind::MethodCall {
                trailing_closure: Some(c),
                ..
            }) = &e.kind
            else {
                continue;
            };
            if let Some(bare) = self.closure_returns.get(&(c.span.file, c.span.start)) {
                found.extend(bare.iter().map(|&at| (at, callee_name(e))));
            }
        }
        for (at, name) in found {
            let mut d = RokDiagnostic::error(
                Code::TRAILING_CLOSURE_RETURN,
                at,
                "a bare `return` in a statement's trailing closure: it would leave only \
                 the closure, not the function",
            )
            .with_file(self.file.clone());
            d = match name {
                Some(n) => d
                    .with_help(format!(
                        "write `return@{n}` to end this call of the closure; to leave the \
                         function, write a `for` loop instead of the call"
                    ))
                    .with_fix(at, format!("return@{n}"), format!("write `return@{n}`")),
                None => d.with_help(
                    "to leave the function, write a `for` loop instead of the call; to end \
                     this call of the closure, pass it in the parentheses",
                ),
            };
            self.diagnostics.push(d);
        }
    }

    /// `lhs` is a trailing-closure call that starts a statement: like a
    /// block-like statement, it ends at its `}`, so a binary operator
    /// after it starts a new statement (`run { .. }` then `-x;`).
    fn closure_stmt_ends(&self, lhs: &Expr) -> bool {
        self.stmt_at == Some(lhs.span.start) && !is_block_like(lhs) && ends_stmt(lhs)
    }

    /// The current token as a `fixity` operator; tokens that are also
    /// syntax (`|`, `<`) are the caller's to filter.
    fn peek_op_symbol(&self, fixity: Fixity) -> Option<(String, Span)> {
        let tok = self.tokens.get(self.pos)?;
        let sym = match (&tok.kind, fixity) {
            // Infix-only tokens.
            (TokenKind::Plus, Fixity::Infix) => "+".into(),
            (TokenKind::Minus, Fixity::Infix | Fixity::Prefix) => "-".into(),
            (TokenKind::Star, Fixity::Infix) => "*".into(),
            (TokenKind::Slash, Fixity::Infix) => "/".into(),
            (TokenKind::Percent, Fixity::Infix) => "%".into(),
            (TokenKind::PercentPercent, Fixity::Infix) => "%%".into(),
            (TokenKind::DotPercentPercent, Fixity::Infix) => ".%%".into(),
            (TokenKind::DotAmpAmp, Fixity::Infix) => ".&&".into(),
            (TokenKind::DotPipePipe, Fixity::Infix) => ".||".into(),
            (TokenKind::EqEq, Fixity::Infix) => "==".into(),
            (TokenKind::NotEq, Fixity::Infix) => "!=".into(),
            (TokenKind::Lt, Fixity::Infix) => "<".into(),
            (TokenKind::Gt, Fixity::Infix) => ">".into(),
            (TokenKind::LtEq, Fixity::Infix) => "<=".into(),
            (TokenKind::GtEq, Fixity::Infix) => ">=".into(),
            (TokenKind::AmpAmp, Fixity::Infix) => "&&".into(),
            (TokenKind::PipePipe, Fixity::Infix) => "||".into(),
            (TokenKind::QuestionQuestion, Fixity::Infix) => "??".into(),
            (TokenKind::Amp, Fixity::Infix) => "&".into(),
            (TokenKind::Pipe, Fixity::Infix) => "|".into(),
            (TokenKind::Caret, Fixity::Infix) => "^".into(),
            (TokenKind::Shl, Fixity::Infix) => "<<".into(),
            (TokenKind::Shr, Fixity::Infix) => ">>".into(),
            (TokenKind::DotPlus, Fixity::Infix) => ".+".into(),
            (TokenKind::DotMinus, Fixity::Infix) => ".-".into(),
            (TokenKind::DotStar, Fixity::Infix) => ".*".into(),
            (TokenKind::DotSlash, Fixity::Infix) => "./".into(),
            (TokenKind::DotPercent, Fixity::Infix) => ".%".into(),
            (TokenKind::DotEqEq, Fixity::Infix) => ".==".into(),
            (TokenKind::DotNotEq, Fixity::Infix) => ".!=".into(),
            (TokenKind::DotLt, Fixity::Infix) => ".<".into(),
            (TokenKind::DotGt, Fixity::Infix) => ".>".into(),
            (TokenKind::DotLtEq, Fixity::Infix) => ".<=".into(),
            (TokenKind::DotGtEq, Fixity::Infix) => ".>=".into(),
            (TokenKind::DotAmp, Fixity::Infix) => ".&".into(),
            (TokenKind::DotPipe, Fixity::Infix) => ".|".into(),
            (TokenKind::DotCaret, Fixity::Infix) => ".^".into(),
            (TokenKind::DotShl, Fixity::Infix) => ".<<".into(),
            (TokenKind::DotShr, Fixity::Infix) => ".>>".into(),
            // Prefix-only tokens.
            (TokenKind::Bang, Fixity::Prefix) => "!".into(),
            (TokenKind::Tilde, Fixity::Prefix) => "~".into(),
            (TokenKind::MathOp('√'), Fixity::Prefix) => "√".into(),
            (TokenKind::MathOp(c), Fixity::Infix) if *c != '√' => c.to_string(),
            (TokenKind::ExtOp(s), f) if self.ext.levels.contains_key(&(s.clone(), f)) => s.clone(),
            _ => return None,
        };
        Some((sym, tok.span))
    }

    /// `cfg!(pred)`: a `bool` literal, folded here. Without
    /// a build's cfg (tools) it reads as the empty environment.
    fn cfg_macro(&mut self) -> Option<Expr> {
        let start = self.peek_span();
        let at = |k: usize| self.tokens.get(self.pos + k).map(|t| &t.kind);
        if self.pending.is_some()
            || !matches!(at(0), Some(TokenKind::Ident(n)) if n == "cfg")
            || at(1) != Some(&TokenKind::Bang)
            || at(2) != Some(&TokenKind::LParen)
        {
            return None;
        }
        self.bump();
        self.bump();
        let body = self.parse_attr_body();
        let value = match &body {
            AttrBody::Args(args) if args.len() == 1 => {
                if let Some(why) = crate::cfg::check_pred(&args[0]) {
                    self.error(Code::INVALID_CFG, start, why);
                }
                self.cfg.as_ref().is_some_and(|env| env.eval(&args[0]))
            }
            _ => {
                self.error(
                    Code::INVALID_CFG,
                    start,
                    "`cfg!` takes exactly one predicate".to_owned(),
                );
                false
            }
        };
        Some(Expr {
            kind: ExprKind::Literal(Literal::Bool(value)),
            span: start,
        })
    }

    /// `include_str!` / `include_bytes!`: the file (relative to this one),
    /// read now, as a literal.
    fn include_macro(&mut self) -> Option<Expr> {
        let start = self.peek_span();
        let at = |k: usize| self.tokens.get(self.pos + k).map(|t| &t.kind);
        let (name, bytes) = match at(0) {
            Some(TokenKind::Ident(n)) if n == "include_str" => ("include_str", false),
            Some(TokenKind::Ident(n)) if n == "include_bytes" => ("include_bytes", true),
            _ => return None,
        };
        if self.pending.is_some()
            || at(1) != Some(&TokenKind::Bang)
            || at(2) != Some(&TokenKind::LParen)
        {
            return None;
        }
        let lit = match (at(3), at(4)) {
            (Some(TokenKind::StrLit(s)), Some(TokenKind::RParen)) => Some(s.clone()),
            _ => None,
        };
        self.bump();
        self.bump();
        let read = if let Some(lit) = lit {
            for _ in 0..3 {
                self.bump();
            }
            include_file(&self.file, &lit, bytes)
        } else {
            let _ = self.parse_attr_body();
            Err("takes one string literal path".to_owned())
        };
        let lit = read.unwrap_or_else(|msg| {
            self.error(Code::INCLUDE_FAILED, start, format!("`{name}!` {msg}"));
            Literal::Str(String::new())
        });
        Some(Expr {
            kind: ExprKind::Literal(lit),
            span: start,
        })
    }

    /// `name!( .. )` / `name![ .. ]` / `name!{ .. }` where `name` is a user
    /// macro: capture the token tree, expand, and re-parse the
    /// result as an expression.
    // `Parser::parse_*` is not general over the expansion's lifetime.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn macro_call_expr(&mut self) -> Option<Expr> {
        let (def, name, start, call) = self.take_macro_call(false)?;
        let poison = || Expr {
            kind: ExprKind::Tuple(Vec::new()),
            span: start,
        };
        let e =
            def.and_then(|d| self.expand_call(&d, &name, start, &call, false, |p| p.parse_expr()));
        Some(e.flatten().unwrap_or_else(poison))
    }

    /// The user macro call `self.pos` starts, `name!(..)` or
    /// `dep::name!(..)`: its qualifier, name and the index of
    /// its opening bracket. Nothing is consumed.
    fn macro_head(&self) -> Option<(Vec<String>, String, usize)> {
        if self.pending.is_some() {
            return None;
        }
        let mut k = self.pos;
        let mut path = Vec::new();
        loop {
            let TokenKind::Ident(n) = &self.tokens.get(k)?.kind else {
                return None;
            };
            path.push(n.clone());
            match self.tokens.get(k + 1)?.kind {
                TokenKind::ColonColon => k += 2,
                TokenKind::Bang => break,
                _ => return None,
            }
        }
        let open = k + 2;
        if !matches!(
            self.tokens.get(open)?.kind,
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace
        ) {
            return None;
        }
        let name = path.pop()?;
        Some((path, name, open))
    }

    /// Take the macro call at `self.pos` (only one ending in `;`, with
    /// the `;`, when `semi`): its block (`None`: a qualified name that
    /// names none, reported), name, span and argument tokens. `None`, with
    /// nothing consumed, when no macro call is there.
    fn take_macro_call(
        &mut self,
        semi: bool,
    ) -> Option<(Option<MetaDef>, String, Span, Vec<Token>)> {
        if self.meta.is_empty() {
            return None;
        }
        let (qual, name, open) = self.macro_head()?;
        if qual.is_empty() && self.meta.find_macro(&name).is_none() {
            return None;
        }
        let close = macros::close_of(self.tokens, open)?;
        if semi && self.tokens.get(close + 1).map(|t| &t.kind) != Some(&TokenKind::Semi) {
            return None;
        }
        let start = self.peek_span();
        let at = self.tokens[open - 2].span;
        let def = match self.find_meta(MetaKind::Macro, &qual, &name, at)? {
            MetaUse::Def(d) => Some(d),
            MetaUse::Bad => None,
        };
        let call = self.tokens[open + 1..close].to_vec();
        self.pos = close + 1 + usize::from(semi);
        Some((def, name, start, call))
    }

    /// Expand the call of macro `def` and parse the result with `parse`
    /// (in braces when `braced`). `None`: it failed (reported).
    fn expand_call<R>(
        &mut self,
        def: &MetaDef,
        name: &str,
        start: Span,
        call: &[Token],
        braced: bool,
        parse: impl FnOnce(&mut Parser<'_>) -> R,
    ) -> Option<R> {
        let key = format!("macro:{name}:{}", def.package);
        if self.meta_stack.iter().any(|k| *k == key) || self.meta_stack.len() >= 256 {
            self.error(
                Code::EXPANSION_LIMIT,
                start,
                format!("`{name}!` expands too deeply or cyclically"),
            );
            return None;
        }
        let (out, diags) = macros::expand_macro(def, call, start);
        self.diagnostics.extend(diags);
        let out = out?;
        self.expanded
            .push(format!("// expanded from {name}!\n{}", meta::render(&out)));
        let toks = if braced {
            let mut toks = vec![Token::new(TokenKind::LBrace, start)];
            toks.extend(out);
            toks.push(Token::new(TokenKind::RBrace, start));
            toks
        } else {
            out
        };
        let mut p = Parser::new(&toks, self.file.clone());
        p.ext = self.ext.clone();
        p.meta = std::sync::Arc::clone(&self.meta);
        p.literals = std::sync::Arc::clone(&self.literals);
        p.cfg.clone_from(&self.cfg);
        p.package.clone_from(&self.package);
        p.meta_imports.clone_from(&self.meta_imports);
        p.meta_stack.clone_from(&self.meta_stack);
        p.meta_stack.push(key);
        let r = parse(&mut p);
        self.diagnostics.extend(p.diagnostics);
        self.expanded.append(&mut p.expanded);
        Some(r)
    }

    /// `name!( .. );` at item position: expand and re-parse
    /// the result as items. Returns the generated items (possibly empty),
    /// or `None` if not positioned at a user-macro call.
    fn macro_call_items(&mut self) -> Option<Vec<Item>> {
        let (def, name, start, call) = self.take_macro_call(false)?;
        if self.peek() == &TokenKind::Semi {
            self.bump();
        }
        let items = def.and_then(|d| {
            self.expand_call(&d, &name, start, &call, false, |p| {
                let mut items = Vec::new();
                while !p.at_eof() {
                    let before = p.pos;
                    if p.meta_block().is_some() {
                        continue;
                    }
                    match p.parse_item() {
                        Some(i) => {
                            items.push(i);
                            items.append(&mut p.pending_items);
                        }
                        None if std::mem::take(&mut p.vanished) => {}
                        None if p.pos == before => break,
                        None => {}
                    }
                }
                items
            })
        });
        Some(items.unwrap_or_default())
    }

    /// `name!( .. );` at statement position: expand and
    /// splice the result into the block as statements (local items, `let`s,
    /// or an expression statement). Only a call terminated by `;` is taken
    /// here; a block-tail `name!(..)` stays an expression.
    // `Parser::parse_*` is not general over the expansion's lifetime.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn macro_call_stmts(&mut self) -> Option<Vec<Stmt>> {
        let (def, name, start, call) = self.take_macro_call(true)?;
        let block = def
            .and_then(|d| {
                self.expand_call(&d, &name, start, &call, true, |p| p.parse_block_stmts())
            })
            .flatten();
        let Some(block) = block else {
            return Some(Vec::new());
        };
        let mut stmts = block.stmts;
        if let Some(e) = block.trailing {
            stmts.push(Stmt {
                kind: StmtKind::Expr(*e),
                span: start,
            });
        }
        Some(stmts)
    }

    #[allow(clippy::cognitive_complexity)] // one arm per primary-expression token
    fn parse_primary(&mut self) -> Option<Expr> {
        let stmt_head = std::mem::take(&mut self.stmt_head);
        if let Some(e) = self
            .try_magic_literal()
            .or_else(|| self.cfg_macro())
            .or_else(|| self.include_macro())
            .or_else(|| self.macro_call_expr())
        {
            return Some(e);
        }
        let start = self.peek_span();
        let kind = match self.peek().clone() {
            // `<Type as Trait>::method(...)`. At the start of
            // a primary, `<` cannot begin a comparison.
            TokenKind::Lt => {
                let mut q = self.parse_qpath()?;
                // `<T as Tr>::X::m`: an associated function of the
                // associated type `X`.
                if self.peek() == &TokenKind::ColonColon
                    && matches!(self.peek_next(), Some(TokenKind::Ident(_)))
                {
                    self.bump();
                    if let TokenKind::Ident(m) = self.peek().clone() {
                        self.bump();
                        q.member = Some(m);
                    }
                }
                ExprKind::QPath(Box::new(q))
            }
            TokenKind::IntLit(s) => {
                self.bump();
                ExprKind::Literal(Literal::Int(s))
            }
            TokenKind::FloatLit(s) => {
                self.bump();
                ExprKind::Literal(Literal::Float(s))
            }
            TokenKind::StrLit(s) => {
                self.bump();
                ExprKind::Literal(Literal::Str(s))
            }
            TokenKind::RawStrLit(s) => {
                self.bump();
                ExprKind::Literal(Literal::RawStr(s))
            }
            TokenKind::ByteStrLit(s) => {
                self.bump();
                ExprKind::Literal(Literal::ByteStr(s))
            }
            TokenKind::FStrStart(_) => self.parse_fstring(),
            TokenKind::Unsafe => {
                // `unsafe { ... }`.
                self.bump();
                if !matches!(self.peek(), TokenKind::LBrace) {
                    self.error(Code::SYNTAX, start, "expected `{` after `unsafe`");
                    return None;
                }
                ExprKind::Unsafe(self.parse_block()?)
            }
            // `using a = e, b = f { body }` is
            // `{ let a = e; { let b = f; { body } finally { b.drop(); } }
            // finally { a.drop(); } }`: reverse order, every exit.
            TokenKind::Using => {
                self.bump();
                // `using_binding ::= pattern "=" expr`: a name,
                // or any irrefutable pattern over a temp that is dropped.
                let mut bindings: Vec<(String, Span, Expr, Option<Pattern>)> = Vec::new();
                loop {
                    let span = self.peek_span();
                    let (name, pat) = match self.peek().clone() {
                        TokenKind::Ident(name) if self.peek_next() == Some(TokenKind::Eq) => {
                            self.bump();
                            (name, None)
                        }
                        _ => {
                            let pat = self.with_struct_expr(true, Self::parse_pattern_atom)?;
                            (format!("#using{}", self.pos), Some(pat))
                        }
                    };
                    self.expect(&TokenKind::Eq, "`=`")?;
                    let init = self.parse_head()?;
                    bindings.push((name, span, init, pat));
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                let mut body = self.parse_block()?;
                // The patterns bind in source order, before the body.
                let lets: Vec<Stmt> = bindings
                    .iter()
                    .filter_map(|(name, span, _, pat)| {
                        Some(Stmt {
                            kind: StmtKind::LetPattern {
                                pat: pat.clone()?,
                                ty: None,
                                init: Expr {
                                    span: *span,
                                    kind: ExprKind::Path(Path {
                                        segments: vec![PathSegment {
                                            name: name.clone(),
                                            generics: Vec::new(),
                                        }],
                                        span: *span,
                                        origin: None,
                                    }),
                                },
                            },
                            span: *span,
                        })
                    })
                    .collect();
                body.stmts.splice(0..0, lets);
                let mut inner = Expr {
                    span: start,
                    kind: ExprKind::Block(body),
                };
                for (name, span, init, _) in bindings.into_iter().rev() {
                    let path = |name: &str| Expr {
                        span,
                        kind: ExprKind::Path(Path {
                            segments: vec![PathSegment {
                                name: name.to_owned(),
                                generics: Vec::new(),
                            }],
                            span,
                            origin: None,
                        }),
                    };
                    let drop_call = Stmt {
                        kind: StmtKind::Expr(Expr {
                            span,
                            kind: ExprKind::MethodCall {
                                receiver: Box::new(path(&name)),
                                method: rokc_ast::USING_DROP_METHOD.to_owned(),
                                type_args: Vec::new(),
                                args: Vec::new(),
                                trailing_closure: None,
                                origin: None,
                            },
                        }),
                        span,
                    };
                    let guarded = Expr {
                        span: start,
                        kind: ExprKind::Catch {
                            body: Box::new(inner),
                            catches: Vec::new(),
                            finally: Some(Block {
                                stmts: vec![drop_call],
                                trailing: None,
                                span,
                                label: None,
                            }),
                        },
                    };
                    inner = Expr {
                        span: start,
                        kind: ExprKind::Block(Block {
                            stmts: vec![Stmt {
                                kind: StmtKind::Let {
                                    name,
                                    ty: None,
                                    init,
                                },
                                span,
                            }],
                            trailing: Some(Box::new(guarded)),
                            span: start,
                            label: None,
                        }),
                    };
                }
                inner.kind
            }
            TokenKind::True => {
                self.bump();
                ExprKind::Literal(Literal::Bool(true))
            }
            TokenKind::False => {
                self.bump();
                ExprKind::Literal(Literal::Bool(false))
            }
            TokenKind::None => {
                self.bump();
                ExprKind::Literal(Literal::None)
            }
            TokenKind::CharLit(s) => {
                self.bump();
                ExprKind::Literal(Literal::Char(s))
            }
            TokenKind::CCharLit(s) => {
                self.bump();
                ExprKind::Literal(Literal::CChar(s))
            }
            TokenKind::LParen => {
                self.bump();
                // `()` unit, `(e)` grouping, `(e,)` / `(a, b)` tuple.
                let (mut items, trailing_comma) =
                    self.with_struct_expr(true, |p| p.expr_list(&TokenKind::RParen))?;
                if items.len() == 1 && !trailing_comma && matches!(self.peek(), TokenKind::Eq) {
                    let target = items.pop()?;
                    let assign = self.with_struct_expr(true, |p| p.assign_tail(target))?;
                    self.expect(&TokenKind::RParen, "`)`")?;
                    return Some(assign);
                }
                self.expect(&TokenKind::RParen, "`)`")?;
                if items.len() == 1 && !trailing_comma {
                    items.pop()?.kind
                } else {
                    ExprKind::Tuple(items)
                }
            }
            TokenKind::Pipe | TokenKind::PipePipe => self.parse_closure_expr()?,
            TokenKind::LBrace => {
                if let Some(lit) = self.try_set_or_dict() {
                    lit
                } else {
                    ExprKind::Block(self.parse_block()?)
                }
            }
            TokenKind::Label(name) => {
                // `'label:` before `loop`, `while`, `for` or a block.
                let label_name = name;
                self.bump();
                self.expect(&TokenKind::Colon, "`:` after label")?;
                match self.peek().clone() {
                    TokenKind::Loop => {
                        self.bump();
                        let body = self.parse_block()?;
                        ExprKind::Loop {
                            label: Some(label_name),
                            body,
                        }
                    }
                    TokenKind::While => {
                        self.bump();
                        let cond = self.parse_head()?;
                        let body = self.parse_block()?;
                        ExprKind::While {
                            label: Some(label_name),
                            cond: Box::new(cond),
                            body,
                        }
                    }
                    TokenKind::For => {
                        self.bump();
                        let is_await = self.eat(&TokenKind::Await);
                        let pat = self.parse_pattern()?;
                        self.expect_kw("in")?;
                        let iter = self.parse_head()?;
                        let body = self.parse_block()?;
                        ExprKind::For {
                            label: Some(label_name),
                            pat,
                            iter: Box::new(iter),
                            body,
                            is_await,
                        }
                    }
                    TokenKind::LBrace => {
                        let mut block = self.parse_block()?;
                        block.label = Some(label_name);
                        ExprKind::Block(block)
                    }
                    _ => {
                        let s = self.peek_span();
                        self.error(
                            Code::SYNTAX,
                            s,
                            "expected `loop`, `while`, `for`, or `{` after label",
                        );
                        return None;
                    }
                }
            }
            TokenKind::Loop => {
                self.bump();
                let body = self.parse_block()?;
                ExprKind::Loop { label: None, body }
            }
            TokenKind::For => {
                self.bump();
                // `for await pat in stream`.
                let is_await = self.eat(&TokenKind::Await);
                let pat = self.parse_pattern()?;
                self.expect_kw("in")?;
                let iter = self.parse_head()?;
                let body = self.parse_block()?;
                ExprKind::For {
                    label: None,
                    pat,
                    iter: Box::new(iter),
                    body,
                    is_await,
                }
            }
            TokenKind::Match => {
                self.bump();
                let scrutinee = self.parse_head()?;
                self.expect(&TokenKind::LBrace, "`{`")?;
                let arms = self.with_struct_expr(true, |p| {
                    let mut arms = Vec::new();
                    while !matches!(p.peek(), TokenKind::RBrace | TokenKind::Eof) {
                        let attrs = p.cfg_only_attrs("a match arm");
                        let marks = (p.pending_stmts.len(), p.lifted.len());
                        let pat = p.parse_pattern()?;
                        let guard = if p.eat(&TokenKind::If) {
                            Some(p.parse_expr()?)
                        } else {
                            None
                        };
                        p.expect(&TokenKind::FatArrow, "`=>`")?;
                        let body = p.parse_arm_body()?;
                        p.eat(&TokenKind::Comma);
                        if !p.cfg_drops(&attrs, marks) {
                            arms.push(MatchArm { pat, guard, body });
                        }
                    }
                    Some(arms)
                })?;
                self.expect(&TokenKind::RBrace, "`}`")?;
                ExprKind::Match {
                    scrutinee: Box::new(scrutinee),
                    arms,
                }
            }
            TokenKind::Try => {
                // `try e` / `try? e` / `try! e` covers everything to its right
                // up to `catch` / `finally` (`parse_catch_tail`).
                let try_span = self.peek_span();
                let try_end = try_span.end;
                self.bump();
                // `try!` is one word: `try !x` tries the negation.
                let glued = self.peek_span().start == try_end;
                let kind = if matches!(self.peek(), TokenKind::Question) {
                    if !glued {
                        let at = self.peek_span();
                        self.error_help(
                            Code::SYNTAX,
                            at,
                            "space between `try` and `?`",
                            "write `try?` as one word",
                        );
                    }
                    self.bump();
                    TryKind::Optional
                } else if glued && self.eat(&TokenKind::Bang) {
                    TryKind::Force
                } else {
                    TryKind::Plain
                };
                let from = self.pos;
                // `try for x in g { .. } (a, b)`: the loop ends there, the
                // `(` starts the next expression.
                if matches!(self.peek(), TokenKind::For) {
                    self.stmt_head = true;
                }
                let operand = self.parse_expr_bp(0)?;
                if kind == TryKind::Optional {
                    self.try_coalesce(try_span, from, &operand);
                }
                ExprKind::Try {
                    kind,
                    operand: Box::new(operand),
                }
            }
            TokenKind::While => {
                self.bump();
                // `while let pat = expr { body }` desugars to
                // `loop { match expr { pat => body, _ => break } }`.
                if self.eat(&TokenKind::Let) {
                    let (pat, destructure) = self.parse_let_cond_pattern()?;
                    self.expect(&TokenKind::Eq, "`=`")?;
                    let scrutinee = self.parse_head()?;
                    let mut body_block = self.parse_block()?;
                    body_block.stmts.splice(0..0, destructure);
                    let body_span = body_block.span;
                    let then_body = Expr {
                        span: body_span,
                        kind: ExprKind::Block(body_block),
                    };
                    let break_expr = Expr {
                        span: start,
                        kind: ExprKind::Tuple(Vec::new()),
                    };
                    let break_block = Block {
                        stmts: vec![Stmt {
                            kind: StmtKind::Break {
                                label: None,
                                value: None,
                            },
                            span: start,
                        }],
                        trailing: None,
                        span: start,
                        label: None,
                    };
                    let break_arm_body = Expr {
                        span: start,
                        kind: ExprKind::Block(break_block),
                    };
                    let _ = break_expr;
                    let wildcard = Pattern {
                        kind: PatternKind::Wildcard,
                        span: start,
                    };
                    let match_expr = Expr {
                        span: start,
                        kind: ExprKind::Match {
                            scrutinee: Box::new(scrutinee),
                            arms: vec![
                                MatchArm {
                                    pat,
                                    guard: None,
                                    body: then_body,
                                },
                                MatchArm {
                                    pat: wildcard,
                                    guard: None,
                                    body: break_arm_body,
                                },
                            ],
                        },
                    };
                    let loop_body = Block {
                        stmts: vec![Stmt {
                            kind: StmtKind::Expr(match_expr),
                            span: start,
                        }],
                        trailing: None,
                        span: start,
                        label: None,
                    };
                    ExprKind::Loop {
                        label: None,
                        body: loop_body,
                    }
                } else {
                    let cond = self.parse_head()?;
                    let body = self.parse_block()?;
                    ExprKind::While {
                        label: None,
                        cond: Box::new(cond),
                        body,
                    }
                }
            }
            TokenKind::If => {
                self.bump();
                // `if let pat = expr block else?` desugars to a `match`
                // with the pattern's arm and a wildcard fallback.
                if self.eat(&TokenKind::Let) {
                    let (pat, destructure) = self.parse_let_cond_pattern()?;
                    self.expect(&TokenKind::Eq, "`=`")?;
                    let scrutinee = self.parse_head()?;
                    let mut then_block = self.parse_block()?;
                    then_block.stmts.splice(0..0, destructure);
                    let then_span = then_block.span;
                    let then_body = Expr {
                        span: then_span,
                        kind: ExprKind::Block(then_block),
                    };
                    let else_body = if self.eat(&TokenKind::Else) {
                        if matches!(self.peek(), TokenKind::If) {
                            self.parse_primary()?
                        } else {
                            let block = self.parse_block()?;
                            let s = block.span;
                            Expr {
                                span: s,
                                kind: ExprKind::Block(block),
                            }
                        }
                    } else {
                        Expr {
                            span: start,
                            kind: ExprKind::Tuple(Vec::new()),
                        }
                    };
                    let wildcard = Pattern {
                        kind: PatternKind::Wildcard,
                        span: start,
                    };
                    ExprKind::Match {
                        scrutinee: Box::new(scrutinee),
                        arms: vec![
                            MatchArm {
                                pat,
                                guard: None,
                                body: then_body,
                            },
                            MatchArm {
                                pat: wildcard,
                                guard: None,
                                body: else_body,
                            },
                        ],
                    }
                } else {
                    let cond = self.parse_head()?;
                    let then_branch = self.parse_block()?;
                    let else_branch = if self.eat(&TokenKind::Else) {
                        if matches!(self.peek(), TokenKind::If) {
                            // Chained `else if`: recurse into another primary.
                            Some(Box::new(self.parse_primary()?))
                        } else {
                            let block = self.parse_block()?;
                            let span = block.span;
                            Some(Box::new(Expr {
                                span,
                                kind: ExprKind::Block(block),
                            }))
                        }
                    } else {
                        None
                    };
                    ExprKind::If {
                        cond: Box::new(cond),
                        then_branch,
                        else_branch,
                    }
                }
            }
            TokenKind::Ident(_) => {
                let mut path = self.parse_path()?;
                // `Pair<i32, i32> { .. }`: `<` compares unless a
                // balanced `>` is followed by `{` or `(`.
                if matches!(self.peek(), TokenKind::Lt) && self.peek_struct_or_call_after_generics()
                {
                    self.bump(); // `<`
                    let generics = self.nested_type_list(TokenKind::Gt)?;
                    if !self.expect_closing_gt() {
                        return None;
                    }
                    if let Some(last) = path.segments.last_mut() {
                        last.generics = generics;
                    }
                }
                if std::mem::take(&mut self.head_struct_lit)
                    && matches!(self.peek(), TokenKind::LBrace)
                {
                    self.parse_struct_init_body(path)?
                } else if self.struct_expr_allowed && matches!(self.peek(), TokenKind::LBrace) {
                    // `Path { .. }`: struct init or 0-arg trailing closure
                    // by the body's first tokens:
                    //   `}`, `..rest`, `ident :|,|}`  → struct init
                    //   anything else               → trailing closure
                    if self.peek_struct_init_body() {
                        self.parse_struct_init_body(path)?
                    } else {
                        // `path { body }` → call with 0-arg trailing closure.
                        let body_span = self.peek_span();
                        let label = path.segments.last().map(|s| s.name.clone());
                        let (block, bare) = self.ret_frame(label, Self::parse_block);
                        let block = block?;
                        self.note_closure_returns(body_span, bare);
                        let body = Expr {
                            span: body_span,
                            kind: ExprKind::Block(block),
                        };
                        let closure = Expr {
                            span: body_span,
                            kind: ExprKind::Closure {
                                captures: Vec::new(),
                                params: Vec::new(),
                                ret_ty: None,
                                body: Box::new(body),
                                is_async: false,
                                is_generator: false,
                            },
                        };
                        let path_span = start;
                        ExprKind::Call {
                            callee: Box::new(Expr {
                                span: path_span,
                                kind: ExprKind::Path(path),
                            }),
                            args: Vec::new(),
                            trailing_closure: Some(Box::new(closure)),
                        }
                    }
                } else if let Some(closure) =
                    self.paren_less_trailing_closure(path.segments.last().map(|s| s.name.clone()))
                {
                    // `f |x| { .. }` → `f(|x| { .. })`.
                    ExprKind::Call {
                        callee: Box::new(Expr {
                            span: start,
                            kind: ExprKind::Path(path),
                        }),
                        args: Vec::new(),
                        trailing_closure: Some(closure),
                    }
                } else {
                    ExprKind::Path(path)
                }
            }
            TokenKind::SelfLower => {
                self.bump();
                ExprKind::SelfLower
            }
            // `Self::NAME` / `Self::f(..)` inside an impl.
            TokenKind::SelfUpper if matches!(self.peek_next(), Some(TokenKind::ColonColon)) => {
                self.bump();
                self.bump();
                let mut rest = self.parse_path()?;
                rest.segments.insert(
                    0,
                    PathSegment {
                        name: "Self".to_owned(),
                        generics: Vec::new(),
                    },
                );
                rest.span = start;
                ExprKind::Path(rest)
            }
            // `Self { x, y }` inside an impl.
            TokenKind::SelfUpper
                if self.struct_expr_allowed
                    && matches!(self.peek_next(), Some(TokenKind::LBrace))
                    && self.struct_init_body_at(self.pos + 1) =>
            {
                self.bump();
                let path = Path {
                    segments: vec![PathSegment {
                        name: "Self".to_owned(),
                        generics: Vec::new(),
                    }],
                    span: start,
                    origin: None,
                };
                self.parse_struct_init_body(path)?
            }
            TokenKind::SelfUpper => {
                self.bump();
                ExprKind::SelfUpper
            }
            // `[a, b]` array, or capture list `[ref x] || ..`:
            // `ref` inside, or names then `]` and `|` / `||`, decide.
            TokenKind::LBracket if self.at_capture_list() => self.parse_capture_closure()?,
            TokenKind::LBracket => {
                self.bump();
                let (mut items, trailing_comma) =
                    self.with_struct_expr(true, |p| p.expr_list(&TokenKind::RBracket))?;
                // `[v; N]` repeat literal.
                if items.len() == 1 && !trailing_comma && self.eat(&TokenKind::Semi) {
                    let len = self.parse_const_len()?;
                    self.expect(&TokenKind::RBracket, "`]`")?;
                    ExprKind::ArrayRepeat {
                        value: Box::new(items.pop()?),
                        len,
                    }
                } else {
                    self.expect(&TokenKind::RBracket, "`]`")?;
                    ExprKind::List(items)
                }
            }
            // `async { }`, `async yield { }`, and
            // the closure modifiers `async ||` / `async yield ||`.
            TokenKind::Async => {
                self.bump();
                let is_generator = self.eat(&TokenKind::Yield);
                match self.peek() {
                    TokenKind::LBrace => {
                        let b = self.ret_frame(None, Self::parse_block).0?;
                        if is_generator {
                            ExprKind::AsyncYieldBlock(b)
                        } else {
                            ExprKind::AsyncBlock(b)
                        }
                    }
                    TokenKind::Pipe | TokenKind::PipePipe | TokenKind::LBracket => {
                        self.coro_closure(true, is_generator)?
                    }
                    _ => {
                        self.error(
                            Code::SYNTAX,
                            start,
                            "expected `{`, `|` or `fn` after `async`",
                        );
                        return None;
                    }
                }
            }
            TokenKind::Await => {
                // `await` is a prefix unary operator
                // (`await f() == x` is `(await f()) == x`).
                self.bump();
                let inner = self.parse_expr_bp(PREFIX_PREC)?;
                ExprKind::Await(Box::new(inner))
            }
            // `yield {` is a generator block, `yield |` a
            // generator closure, anything else the `yield` statement.
            TokenKind::Yield if matches!(self.peek_next(), Some(TokenKind::LBrace)) => {
                self.bump();
                ExprKind::YieldBlock(self.ret_frame(None, Self::parse_block).0?)
            }
            TokenKind::Yield
                if matches!(
                    self.peek_next(),
                    Some(TokenKind::Pipe | TokenKind::PipePipe)
                ) || (matches!(self.peek_next(), Some(TokenKind::LBracket))
                    && matches!(
                        self.tokens.get(self.pos + 2).map(|t| &t.kind),
                        Some(TokenKind::Ref)
                    )) =>
            {
                self.bump();
                self.coro_closure(false, true)?
            }
            // A divergent jump in expression position: `match return 0 {}`,
            // `f(return 1)`.
            TokenKind::Return | TokenKind::Break | TokenKind::Continue | TokenKind::Throw => {
                return self.parse_arm_body();
            }
            TokenKind::Yield => {
                self.bump();
                let value = if matches!(
                    self.peek(),
                    TokenKind::Semi | TokenKind::RBrace | TokenKind::Comma | TokenKind::RParen
                ) {
                    None
                } else {
                    Some(Box::new(self.parse_expr_bp(0)?))
                };
                ExprKind::Yield(value)
            }
            other => {
                let msg = format!("expected expression, found {}", other.describe());
                let help = match other {
                    TokenKind::HashBangLBracket => Some(
                        "an inner attribute `#![..]` goes at the top of a file; put an outer \
                         `#[..]` on the function instead",
                    ),
                    TokenKind::Pub => Some(
                        "an item or `use` in a function body is visible only in its block; \
                         remove `pub`",
                    ),
                    TokenKind::ColonColon => Some(
                        "a path starts with a name: write `pkg::a` for an item of this \
                         package's root, `sup::a` for the parent module's",
                    ),
                    TokenKind::At => Some(
                        "`@` tokens belong to meta templates; the shape tokens (`@Self`, \
                         `@field`, `@variant`) only to `@derive` / `@attribute` blocks \
                        ",
                    ),
                    _ => None,
                };
                match help {
                    Some(h) => self.error_help(Code::SYNTAX, start, msg, h),
                    None => self.error(Code::SYNTAX, start, msg),
                }
                return None;
            }
        };
        let mut expr = Expr {
            kind,
            span: self.to_prev(start),
        };

        // Postfix chain: calls, members, indexing, `?`, `?.`. After a
        // `?.`, member / index links join its tail.
        let mut chain_open = false;
        loop {
            expr.span = self.to_prev(expr.span);
            if chain_open {
                match fold_chain(expr) {
                    Ok(e) => expr = e,
                    Err(e) => {
                        expr = e;
                        chain_open = false;
                    }
                }
            }
            self.chain_link()?;
            match self.peek() {
                TokenKind::LParen | TokenKind::LBracket
                    if (stmt_head || matches!(expr.kind, ExprKind::Try { .. }))
                        && is_block_like(&expr) =>
                {
                    break;
                }
                TokenKind::LParen => {
                    self.bump();
                    let args = self.parse_call_args()?;
                    self.expect(&TokenKind::RParen, "`)`")?;
                    // Trailing closure: a `{` or `|..|` right
                    // after `)` is the last argument.
                    let trailing_closure = self.try_parse_trailing_closure(path_name(&expr))?;
                    expr = Expr {
                        span: expr.span,
                        kind: ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                            trailing_closure,
                        },
                    };
                }
                TokenKind::Dot if self.peek_next() == Some(TokenKind::LParen) => {
                    // `f.(xs)`: function broadcast.
                    self.bump();
                    self.bump();
                    let args = self.parse_call_args()?;
                    self.expect(&TokenKind::RParen, "`)`")?;
                    expr = Expr {
                        span: expr.span,
                        kind: ExprKind::Broadcast {
                            callee: Box::new(expr),
                            args,
                        },
                    };
                }
                TokenKind::Dot => {
                    self.bump();
                    let origin = self.tokens.get(self.pos).and_then(Token::origin_module);
                    let name = match self.peek().clone() {
                        _ if self.peek_name().is_some() => {
                            let n = self.peek_name().unwrap_or_default();
                            self.bump_name();
                            n
                        }
                        TokenKind::IntLit(s) | TokenKind::FloatLit(s) => {
                            // Tuple field: `expr.0`, `expr.1`, ... A float
                            // token `0.1` in `t.0.1` is two accesses.
                            let span = self.peek_span();
                            self.bump();
                            for part in s.split('.') {
                                let idx = self.tuple_index(part, span)?;
                                expr = Expr {
                                    span: expr.span,
                                    kind: ExprKind::TupleField(Box::new(expr), idx),
                                };
                            }
                            continue;
                        }
                        _ => {
                            let s = self.peek_span();
                            self.error(
                                Code::SYNTAX,
                                s,
                                format!(
                                    "expected field or method name after `.`, found {}",
                                    self.peek().describe()
                                ),
                            );
                            return None;
                        }
                    };
                    // `x.m::<T>(..)`: explicit generic arguments.
                    let mut type_args = Vec::new();
                    if matches!(self.peek(), TokenKind::ColonColon)
                        && self.peek_next() == Some(TokenKind::Lt)
                    {
                        self.bump();
                        self.bump();
                        type_args = self.nested_type_list(TokenKind::Gt)?;
                        if !self.expect_closing_gt() {
                            return None;
                        }
                        if !matches!(self.peek(), TokenKind::LParen) {
                            let s = self.peek_span();
                            self.error(Code::SYNTAX, s, "expected `(` after a method's `::<..>`");
                            return None;
                        }
                    }
                    if matches!(self.peek(), TokenKind::LParen) {
                        self.bump();
                        let args = self.parse_call_args()?;
                        self.expect(&TokenKind::RParen, "`)`")?;
                        // Trailing closure on a method call, as
                        // for calls.
                        let trailing_closure =
                            self.try_parse_trailing_closure(Some(name.clone()))?;
                        expr = Expr {
                            span: expr.span,
                            kind: ExprKind::MethodCall {
                                receiver: Box::new(expr),
                                method: name,
                                type_args,
                                args,
                                trailing_closure,
                                origin,
                            },
                        };
                    } else if let Some(closure) = self
                        .paren_less_trailing_closure(Some(name.clone()))
                        .or_else(|| self.paren_less_block_closure(Some(name.clone())))
                    {
                        // `xs.map |x| { .. }`, `s.spawn { .. }`.
                        expr = Expr {
                            span: expr.span,
                            kind: ExprKind::MethodCall {
                                receiver: Box::new(expr),
                                method: name,
                                type_args,
                                args: Vec::new(),
                                trailing_closure: Some(closure),
                                origin,
                            },
                        };
                    } else {
                        expr = Expr {
                            span: expr.span,
                            kind: ExprKind::Field(Box::new(expr), name),
                        };
                    }
                }
                TokenKind::LBracket => {
                    self.bump();
                    // Slicing: `a[start? : end? (: step?)?]`.
                    if let Some(slice) = self.with_struct_expr(true, |p| p.parse_slice(&expr)) {
                        expr = slice?;
                        continue;
                    }
                    let (indices, _) =
                        self.with_struct_expr(true, |p| p.expr_list(&TokenKind::RBracket))?;
                    if matches!(self.peek(), TokenKind::Colon | TokenKind::ColonColon) {
                        self.mixed_index_error();
                        return None;
                    }
                    if indices.is_empty() {
                        let s = self.peek_span();
                        self.error(Code::SYNTAX, s, "expected index expression");
                    }
                    self.expect(&TokenKind::RBracket, "`]`")?;
                    // `m[i, j]` is `m[(i, j)]` (`Index<(I, J)>`).
                    let span = expr.span;
                    let idx = if indices.len() == 1 {
                        indices.into_iter().next()?
                    } else {
                        let at = indices.first().map_or(span, |e| e.span);
                        Expr {
                            span: at,
                            kind: ExprKind::Tuple(indices),
                        }
                    };
                    expr = Expr {
                        span,
                        kind: ExprKind::Index(Box::new(expr), Box::new(idx)),
                    };
                }
                TokenKind::Question => {
                    self.bump();
                    expr = Expr {
                        span: expr.span,
                        kind: ExprKind::TryQ(Box::new(expr)),
                    };
                }
                // A registered postfix extension token (`x°`): the postfix
                // row (level 13) with `?` and member access.
                TokenKind::ExtOp(sym)
                    if self
                        .ext
                        .levels
                        .contains_key(&(sym.clone(), Fixity::Postfix)) =>
                {
                    let op = sym.clone();
                    self.bump();
                    expr = Expr {
                        span: expr.span,
                        kind: ExprKind::OpPostfix {
                            op,
                            operand: Box::new(expr),
                        },
                    };
                }
                TokenKind::QuestionDot => {
                    self.bump();
                    let name = match self.peek().clone() {
                        _ if self.peek_name().is_some() => {
                            let n = self.peek_name().unwrap_or_default();
                            self.bump_name();
                            n
                        }
                        TokenKind::IntLit(s) => {
                            let span = self.peek_span();
                            self.bump();
                            let idx = self.tuple_index(&s, span)?;
                            expr = Expr {
                                span: expr.span,
                                kind: ExprKind::QuestionDot(
                                    Box::new(expr),
                                    vec![AccessTail::TupleField(idx)],
                                ),
                            };
                            chain_open = true;
                            continue;
                        }
                        // No `x?.[i]`.
                        TokenKind::LBracket => {
                            let s = self.peek_span();
                            self.error_help(
                                Code::SYNTAX,
                                s,
                                "`?.` is not followed by `[`: there is no `x?.[i]`",
                                "write `x?.get(i)`",
                            );
                            return None;
                        }
                        _ => {
                            let s = self.peek_span();
                            self.error(
                                Code::SYNTAX,
                                s,
                                format!(
                                    "expected field or method name after `?.`, found {}",
                                    self.peek().describe()
                                ),
                            );
                            return None;
                        }
                    };
                    let tail = if matches!(self.peek(), TokenKind::LParen) {
                        self.bump();
                        let args = self.parse_call_args()?;
                        self.expect(&TokenKind::RParen, "`)`")?;
                        AccessTail::Method {
                            method: name,
                            type_args: Vec::new(),
                            args,
                        }
                    } else {
                        AccessTail::Field(name)
                    };
                    expr = Expr {
                        span: expr.span,
                        kind: ExprKind::QuestionDot(Box::new(expr), vec![tail]),
                    };
                    chain_open = true;
                }
                _ => break,
            }
        }
        if chain_open {
            expr = fold_chain(expr).unwrap_or_else(|e| e);
        }
        expr.span = self.to_prev(expr.span);
        Some(expr)
    }

    /// Tuple index after `.`: plain decimal only (`t.1u8`, `t.0x1` are
    /// errors).
    fn tuple_index(&mut self, text: &str, span: Span) -> Option<u32> {
        let plain = !text.is_empty()
            && text.bytes().all(|b| b.is_ascii_digit())
            && (text == "0" || !text.starts_with('0'));
        match text.parse::<u32>() {
            Ok(i) if plain => Some(i),
            _ => {
                self.error(
                    Code::SYNTAX,
                    span,
                    format!("invalid tuple field index `{text}`"),
                );
                None
            }
        }
    }

    /// `{ k: v, .. }` is a dict, `{ a, b, .. }` a set (two
    /// items or a trailing comma), anything else a block. Decided by a
    /// speculative parse of the first expression.
    fn try_set_or_dict(&mut self) -> Option<ExprKind> {
        let (pos, pending, n_diags) = (self.pos, self.pending.clone(), self.diagnostics.len());
        let n_lifted = self.lifted.len();
        let rewind = |p: &mut Self| {
            p.pos = pos;
            p.pending.clone_from(&pending);
            p.diagnostics.truncate(n_diags);
            p.lifted.truncate(n_lifted);
        };
        self.bump(); // `{`
        // Try only a simple first item, so nested blocks aren't re-parsed
        // exponentially; one starting with a keyword is a block.
        let first = if matches!(
            self.peek(),
            TokenKind::IntLit(_)
                | TokenKind::FloatLit(_)
                | TokenKind::CharLit(_)
                | TokenKind::StrLit(_)
                | TokenKind::RawStrLit(_)
                | TokenKind::Ident(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Minus
                | TokenKind::LParen
                | TokenKind::LBracket
        ) {
            self.with_struct_expr(true, Self::parse_expr)
        } else {
            None
        };
        let kind = match (first, self.peek()) {
            (Some(k), TokenKind::Colon) => {
                self.bump();
                let mut pairs = Vec::new();
                let mut key = k;
                loop {
                    let v = self.with_struct_expr(true, Self::parse_expr)?;
                    pairs.push((key, v));
                    if !self.eat(&TokenKind::Comma) || matches!(self.peek(), TokenKind::RBrace) {
                        break;
                    }
                    key = self.with_struct_expr(true, Self::parse_expr)?;
                    self.expect(&TokenKind::Colon, "`:` in a dict literal")?;
                }
                ExprKind::Dict(pairs)
            }
            (Some(first), TokenKind::Comma) => {
                self.bump();
                let (mut rest, _) =
                    self.with_struct_expr(true, |p| p.expr_list(&TokenKind::RBrace))?;
                rest.insert(0, first);
                ExprKind::Set(rest)
            }
            _ => {
                rewind(self);
                return None;
            }
        };
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some(kind)
    }

    /// After `[`: a slice `start? : end? (: step?)? ]` on `target`, or
    /// `None` (nothing consumed) when the brackets hold a plain index.
    #[allow(clippy::option_option)] // outer: is a slice; inner: parsed ok
    fn parse_slice(&mut self, target: &Expr) -> Option<Option<Expr>> {
        let save = self.pos;
        let colon = |p: &Self| matches!(p.peek(), TokenKind::Colon | TokenKind::ColonColon);
        let start = if colon(self) {
            None
        } else {
            let (saved_diags, n_lifted) = (self.diagnostics.len(), self.lifted.len());
            let e = self.parse_expr();
            if e.is_none() || !colon(self) {
                self.pos = save;
                self.diagnostics.truncate(saved_diags);
                self.lifted.truncate(n_lifted);
                return None;
            }
            e.map(Box::new)
        };
        let opt = |p: &mut Self| {
            if matches!(p.peek(), TokenKind::Colon | TokenKind::RBracket) {
                Some(None)
            } else {
                p.parse_expr().map(|e| Some(Box::new(e)))
            }
        };
        // `a::k` lexes `::` as one token: no end, then the step.
        let (end, has_step) = if self.eat(&TokenKind::ColonColon) {
            (None, true)
        } else {
            self.bump(); // `:`
            let Some(end) = opt(self) else {
                return Some(None);
            };
            (end, self.eat(&TokenKind::Colon))
        };
        let step = if has_step {
            let Some(step) = opt(self) else {
                return Some(None);
            };
            step
        } else {
            None
        };
        if matches!(self.peek(), TokenKind::Comma) {
            self.mixed_index_error();
            return Some(None);
        }
        if self.expect(&TokenKind::RBracket, "`]`").is_none() {
            return Some(None);
        }
        Some(Some(Expr {
            span: target.span,
            kind: ExprKind::Slice {
                target: Box::new(target.clone()),
                start,
                end,
                step,
            },
        }))
    }

    /// One bracket either indexes (`,`) or slices (`:`).
    fn mixed_index_error(&mut self) {
        let s = self.peek_span();
        self.error(
            Code::NEEDS_PARENTHESES,
            s,
            "one `[..]` cannot both index with `,` and slice with `:`; \
             slice and index in separate brackets (`m[a:b][c]`)",
        );
    }

    /// `e, e, ...` up to (not consuming) `close`. Returns the items and
    /// whether the list ended with a trailing comma.
    fn expr_list(&mut self, close: &TokenKind) -> Option<(Vec<Expr>, bool)> {
        let mut items = Vec::new();
        let mut trailing_comma = false;
        while std::mem::discriminant(self.peek()) != std::mem::discriminant(close)
            && !matches!(self.peek(), TokenKind::Eof)
        {
            if *close == TokenKind::RBracket
                && matches!(self.peek(), TokenKind::Colon | TokenKind::ColonColon)
            {
                break;
            }
            items.push(self.parse_expr()?);
            trailing_comma = self.eat(&TokenKind::Comma);
            if !trailing_comma {
                break;
            }
        }
        Some((items, trailing_comma))
    }

    /// Trailing closure: `|..|` or `{ .. }` right after a
    /// call's `)`. `||` there is logical-or (0-arg is `{ .. }`), and `{`
    /// is off in a control-flow head (`struct_expr_allowed`).
    #[allow(clippy::option_option)] // outer None = parse error (for `?`), inner None = no closure
    fn try_parse_trailing_closure(&mut self, label: Option<String>) -> Option<Option<Box<Expr>>> {
        match self.peek() {
            TokenKind::LBrace if self.struct_expr_allowed => {
                let span = self.peek_span();
                let (block, bare) = self.ret_frame(label, Self::parse_block);
                let block = block?;
                self.note_closure_returns(span, bare);
                let body = Expr {
                    span,
                    kind: ExprKind::Block(block),
                };
                let closure = Expr {
                    span,
                    kind: ExprKind::Closure {
                        captures: Vec::new(),
                        params: Vec::new(),
                        ret_ty: None,
                        body: Box::new(body),
                        is_async: false,
                        is_generator: false,
                    },
                };
                Some(Some(Box::new(closure)))
            }
            TokenKind::Pipe => {
                let span = self.peek_span();
                let kind = self.closure_expr(label, span)?;
                Some(Some(Box::new(Expr { span, kind })))
            }
            _ => Some(None),
        }
    }

    /// At `[`: a capture list, `[ref x ..` or `[x, y] |..|`.
    fn at_capture_list(&self) -> bool {
        let tok = |i: usize| self.tokens.get(self.pos + i).map(|t| &t.kind);
        if matches!(tok(1), Some(TokenKind::Ref)) {
            return true;
        }
        let mut i = 1;
        loop {
            if !matches!(tok(i), Some(TokenKind::Ident(_) | TokenKind::SelfLower)) {
                return false;
            }
            i += 1;
            match tok(i) {
                Some(TokenKind::Comma) if matches!(tok(i + 1), Some(TokenKind::RBracket)) => {
                    i += 1;
                    break;
                }
                Some(TokenKind::Comma) => i += 1,
                Some(TokenKind::RBracket) => break,
                _ => return false,
            }
            if matches!(tok(i), Some(TokenKind::Ref)) {
                return true;
            }
        }
        matches!(tok(i + 1), Some(TokenKind::Pipe | TokenKind::PipePipe))
    }

    /// `[ref x, y] |..| body` - a closure with a capture list.
    fn parse_capture_closure(&mut self) -> Option<ExprKind> {
        self.bump(); // `[`
        let mut captures: Vec<rokc_ast::CaptureSpec> = Vec::new();
        while !matches!(self.peek(), TokenKind::RBracket | TokenKind::Eof) {
            if self.eat(&TokenKind::Ref) {
                let Some(name) = self.capture_name() else {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!(
                            "expected capture name after `ref`, found {}",
                            self.peek().describe()
                        ),
                    );
                    return None;
                };
                captures.push(rokc_ast::CaptureSpec::Ref(name));
            } else if let Some(n) = self.capture_name() {
                captures.push(rokc_ast::CaptureSpec::Value(n));
            } else {
                let s = self.peek_span();
                self.error(Code::SYNTAX, s, "capture-list item expected");
                return None;
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RBracket, "`]`")?;
        if !matches!(self.peek(), TokenKind::Pipe | TokenKind::PipePipe) {
            let s = self.peek_span();
            self.error(Code::SYNTAX, s, "`|` or `||` expected after capture list");
            return None;
        }
        let mut closure_kind = self.parse_closure_expr()?;
        if let ExprKind::Closure { captures: c, .. } = &mut closure_kind {
            *c = captures;
        }
        Some(closure_kind)
    }

    /// A capture-list name: a variable, or a method's `self`.
    fn capture_name(&mut self) -> Option<String> {
        let name = match self.peek() {
            TokenKind::Ident(n) => n.clone(),
            TokenKind::SelfLower => "self".to_owned(),
            _ => return None,
        };
        self.bump();
        Some(name)
    }

    /// `async ||` / `yield ||` / `async yield ||` closure.
    fn coro_closure(&mut self, is_async: bool, is_generator: bool) -> Option<ExprKind> {
        let mut k = if matches!(self.peek(), TokenKind::LBracket) {
            self.parse_capture_closure()?
        } else {
            self.parse_closure_expr()?
        };
        if let ExprKind::Closure {
            is_async: a,
            is_generator: g,
            ..
        } = &mut k
        {
            *a = is_async;
            *g = is_generator;
        }
        Some(k)
    }

    /// `|args| body`, `|args| -> R body` or `|| body`.
    fn parse_closure_expr(&mut self) -> Option<ExprKind> {
        let span = self.peek_span();
        self.closure_expr(None, span)
    }

    /// `parse_closure_expr` for a trailing closure of a call to `label`
    /// (`span`: the closure's).
    fn closure_expr(&mut self, label: Option<String>, span: Span) -> Option<ExprKind> {
        let (params, ret_ty) = self.parse_closure_head()?;
        let body_at = self.peek_span();
        let (body, bare) = self.ret_frame(label, Self::parse_expr);
        let body = body?;
        // `-> T` only before a block body.
        if ret_ty.is_some() && !matches!(&body.kind, ExprKind::Block(b) if b.label.is_none()) {
            self.error_help(
                Code::SYNTAX,
                body_at,
                "a closure with `-> T` needs a block body",
                "wrap the body in braces: `|x: i32| -> i32 { x * 2 }`",
            );
        }
        self.note_closure_returns(span, bare);
        Some(ExprKind::Closure {
            captures: Vec::new(),
            params,
            ret_ty,
            body: Box::new(body),
            is_async: false,
            is_generator: false,
        })
    }

    /// Without the `( )`: `xs.map |x| { .. }`. A `|` after a
    /// callee is a closure only when `|params| (-> R)? {` follows; `||`
    /// stays logical-or. Off in a control-flow head.
    /// `s.spawn { body }`: a `{` after a method name, outside
    /// a condition head.
    fn paren_less_block_closure(&mut self, label: Option<String>) -> Option<Box<Expr>> {
        if !self.struct_expr_allowed || !matches!(self.peek(), TokenKind::LBrace) {
            return None;
        }
        self.try_parse_trailing_closure(label).flatten()
    }

    fn paren_less_trailing_closure(&mut self, label: Option<String>) -> Option<Box<Expr>> {
        if !self.struct_expr_allowed || !matches!(self.peek(), TokenKind::Pipe) {
            return None;
        }
        let not_closure = self.backtracks(|p| {
            p.parse_closure_head()?;
            matches!(p.peek(), TokenKind::LBrace).then_some(())
        });
        if not_closure {
            return None;
        }
        let span = self.peek_span();
        let (params, ret_ty) = self.parse_closure_head()?;
        let body_span = self.peek_span();
        let (block, bare) = self.ret_frame(label, Self::parse_block);
        let block = block?;
        self.note_closure_returns(span, bare);
        Some(Box::new(Expr {
            span,
            kind: ExprKind::Closure {
                captures: Vec::new(),
                params,
                ret_ty,
                body: Box::new(Expr {
                    span: body_span,
                    kind: ExprKind::Block(block),
                }),
                is_async: false,
                is_generator: false,
            },
        }))
    }

    /// `|params|` / `||`, then an optional `-> R`.
    fn parse_closure_head(&mut self) -> Option<(Vec<Param>, Option<Ty>)> {
        let mut params: Vec<Param> = Vec::new();
        if self.eat(&TokenKind::PipePipe) {
            // Empty param list `||`.
        } else {
            self.expect(&TokenKind::Pipe, "`|`")?;
            params = self.with_type_sum(false, |p| {
                let mut params = Vec::new();
                while !matches!(p.peek(), TokenKind::Pipe | TokenKind::Eof) {
                    params.push(p.parse_param()?);
                    if !p.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                Some(params)
            })?;
            self.expect(&TokenKind::Pipe, "`|`")?;
        }
        let ret_ty = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Some((params, ret_ty))
    }

    /// `Path<...>` in an expression: a balanced `>` followed by `{` or `(`.
    /// Stops at tokens no generic list holds, so comparisons fail fast.
    fn peek_struct_or_call_after_generics(&self) -> bool {
        // Caller has already confirmed `peek() == Lt`.
        let mut depth: i32 = 0;
        let mut i = self.pos;
        let max = i.saturating_add(64).min(self.tokens.len());
        while i < max {
            let kind = match self.tokens.get(i) {
                Some(t) => &t.kind,
                None => return false,
            };
            match kind {
                TokenKind::Lt => depth += 1,
                TokenKind::Shl => depth += 2,
                TokenKind::Gt => {
                    depth -= 1;
                    if depth == 0 {
                        let next = self.tokens.get(i + 1).map(|t| &t.kind);
                        return matches!(next, Some(TokenKind::LBrace | TokenKind::LParen));
                    }
                }
                TokenKind::Shr => {
                    depth -= 2;
                    if depth <= 0 {
                        let next = self.tokens.get(i + 1).map(|t| &t.kind);
                        return matches!(next, Some(TokenKind::LBrace | TokenKind::LParen));
                    }
                }
                // Tokens that may appear inside a generic-arg type list.
                TokenKind::Ident(_)
                | TokenKind::Comma
                | TokenKind::ColonColon
                | TokenKind::Amp
                | TokenKind::Star
                | TokenKind::LBracket
                | TokenKind::RBracket
                | TokenKind::LParen
                | TokenKind::RParen
                | TokenKind::Semi
                | TokenKind::IntLit(_) => {}
                _ => return false,
            }
            i += 1;
        }
        false
    }

    /// `Ident (:: Ident)* {` lookahead (no tokens consumed).
    fn peek_path_then_lbrace(&self) -> bool {
        let mut i = self.pos;
        loop {
            if !matches!(
                self.tokens.get(i).map(|t| &t.kind),
                Some(TokenKind::Ident(_))
            ) {
                return false;
            }
            match self.tokens.get(i + 1).map(|t| &t.kind) {
                Some(TokenKind::ColonColon) => i += 2,
                Some(TokenKind::LBrace) => return true,
                _ => return false,
            }
        }
    }

    /// `Path { .. }`: struct literal (`{}`, `{ .. }`, `{ x: `, `{ x, `,
    /// `{ x }`) or 0-arg trailing closure? Peeks two tokens.
    fn peek_struct_init_body(&self) -> bool {
        // Caller has just confirmed `peek() == LBrace`.
        self.struct_init_body_at(self.pos)
    }

    /// `peek_struct_init_body` for the `{` at token `brace`.
    fn struct_init_body_at(&self, brace: usize) -> bool {
        // A first field's `#[cfg]` is skipped.
        let brace = self.past_attrs(brace + 1) - 1;
        let after_brace = self.tokens.get(brace + 1).map(|t| &t.kind);
        match after_brace {
            Some(TokenKind::RBrace | TokenKind::DotDot) => true,
            Some(TokenKind::Ident(_)) => {
                let third = self.tokens.get(brace + 2).map(|t| &t.kind);
                matches!(
                    third,
                    Some(TokenKind::Colon | TokenKind::Comma | TokenKind::RBrace)
                )
            }
            // A keyword field name, `Ev { match: 1 }`.
            Some(k) if keyword_str(k).is_some_and(|w| w != "self") => matches!(
                self.tokens.get(brace + 2).map(|t| &t.kind),
                Some(TokenKind::Colon)
            ),
            _ => false,
        }
    }

    /// `{ field: expr, .., ..base? }` after a path, from the `{`.
    fn parse_struct_init_body(&mut self, path: Path) -> Option<ExprKind> {
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut fields = Vec::new();
        let mut spread: Option<Box<Expr>> = None;
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            if self.eat(&TokenKind::DotDot) {
                let base = self.with_struct_expr(true, Self::parse_expr)?;
                spread = Some(Box::new(base));
                break;
            }
            let attrs = self.cfg_only_attrs("a struct literal field");
            let marks = (self.pending_stmts.len(), self.lifted.len());
            let name = if let Some(n) = self.peek_name() {
                self.bump_name();
                n
            } else {
                let s = self.peek_span();
                self.error(Code::SYNTAX, s, "expected field name");
                return None;
            };
            // Shorthand `Foo { x }` (using a local named `x`) is allowed
            // when the next token is not `:`.
            let value = if self.eat(&TokenKind::Colon) {
                self.with_struct_expr(true, Self::parse_expr)?
            } else {
                Expr {
                    span: self.peek_span(),
                    kind: ExprKind::Path(Path {
                        segments: vec![PathSegment {
                            name: name.clone(),
                            generics: Vec::new(),
                        }],
                        span: self.peek_span(),
                        origin: None,
                    }),
                }
            };
            if !self.cfg_drops(&attrs, marks) {
                fields.push((name, value));
            }
            if !self.eat(&TokenKind::Comma) {
                // `S { a: 1 b: 2 }`: a field follows without its `,`.
                if let (TokenKind::Ident(next), Some(TokenKind::Colon)) =
                    (self.peek().clone(), self.peek_next())
                {
                    let at = self.peek_span();
                    let last = fields
                        .last()
                        .map_or_else(String::new, |(n, v): &(String, Expr)| {
                            format!("{n}: {}, ", self.text_in(v.span))
                        });
                    self.error_help(
                        Code::SYNTAX,
                        at,
                        format!("expected `,` or `}}`, found field `{next}`"),
                        format!("separate the fields with `,`: `{last}{next}: ..`"),
                    );
                    return None;
                }
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some(ExprKind::StructInit {
            path,
            fields,
            spread,
        })
    }

    /// If a literal pattern is followed by `..` or `..=`, extend it into
    /// a range pattern. The end is optional: `0..` is open-ended.
    fn maybe_extend_range(&mut self, span: Span, start: Literal) -> Pattern {
        let inclusive = match self.peek() {
            TokenKind::DotDotEq => true,
            TokenKind::DotDot => false,
            _ => {
                return Pattern {
                    kind: PatternKind::Literal(start),
                    span,
                };
            }
        };
        self.bump();
        let end = match self.peek().clone() {
            TokenKind::IntLit(s) => {
                self.bump();
                Some(Literal::Int(s))
            }
            TokenKind::FloatLit(s) => {
                self.bump();
                Some(Literal::Float(s))
            }
            TokenKind::CharLit(s) => {
                self.bump();
                Some(Literal::Char(s))
            }
            TokenKind::CCharLit(s) => {
                self.bump();
                Some(Literal::CChar(s))
            }
            TokenKind::Minus => {
                self.bump();
                match self.peek().clone() {
                    TokenKind::IntLit(s) => {
                        self.bump();
                        Some(Literal::Int(format!("-{s}")))
                    }
                    TokenKind::FloatLit(s) => {
                        self.bump();
                        Some(Literal::Float(format!("-{s}")))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        Pattern {
            kind: PatternKind::Range {
                start: Some(start),
                end,
                inclusive,
            },
            span,
        }
    }

    /// `(p, .., q)` after a variant path (empty without parentheses).
    /// `..` stands for the fields not written.
    fn parse_variant_pattern_args(&mut self) -> Option<Vec<Pattern>> {
        Some(if self.eat(&TokenKind::LParen) {
            let mut sub = Vec::new();
            while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
                // `V(a, ..)`: the remaining fields, any value.
                if matches!(self.peek(), TokenKind::DotDot) {
                    let span = self.peek_span();
                    self.bump();
                    sub.push(Pattern {
                        kind: PatternKind::Rest(None),
                        span,
                    });
                } else {
                    sub.push(self.parse_pattern()?);
                }
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen, "`)`")?;
            sub
        } else {
            Vec::new()
        })
    }

    fn parse_struct_pattern_body(&mut self, path: Path) -> Option<PatternKind> {
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut fields = Vec::new();
        let mut rest = false;
        while !matches!(self.peek(), TokenKind::RBrace | TokenKind::Eof) {
            if self.eat(&TokenKind::DotDot) {
                rest = true;
                break;
            }
            let name = if let Some(n) = self.peek_name() {
                self.bump_name();
                n
            } else {
                let s = self.peek_span();
                self.error(Code::SYNTAX, s, "expected field name");
                return None;
            };
            // `Foo { x: pat }` or shorthand `Foo { x }` (binds `x`).
            let pat = if self.eat(&TokenKind::Colon) {
                Some(self.parse_pattern()?)
            } else {
                None
            };
            fields.push(FieldPattern { name, pat });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Some(PatternKind::Struct { path, fields, rest })
    }

    fn parse_pattern(&mut self) -> Option<Pattern> {
        let first = self.parse_pattern_atom()?;
        if !matches!(self.peek(), TokenKind::Pipe) {
            return Some(first);
        }
        let span = first.span;
        let mut alts = vec![first];
        while self.eat(&TokenKind::Pipe) {
            alts.push(self.parse_pattern_atom()?);
        }
        Some(Pattern {
            kind: PatternKind::Or(alts),
            span,
        })
    }

    /// After a binding's name: `name` or `name @ pat` (binds
    /// the whole value matched by `pat`).
    fn parse_binding_rest(&mut self, name: String) -> Option<PatternKind> {
        let sub = if self.eat(&TokenKind::At) {
            Some(Box::new(self.parse_pattern_atom()?))
        } else {
            None
        };
        Some(PatternKind::Binding { name, sub })
    }

    fn parse_pattern_atom(&mut self) -> Option<Pattern> {
        self.nested(Self::parse_pattern_atom_inner)
    }

    /// `name!( .. )` where a pattern is expected: expand and
    /// re-parse the result as a pattern.
    // `Parser::parse_*` is not general over the expansion's lifetime.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn macro_call_pattern(&mut self) -> Option<Pattern> {
        let (def, name, span, call) = self.take_macro_call(false)?;
        let poison = || Pattern {
            kind: PatternKind::Wildcard,
            span,
        };
        let pat = def
            .and_then(|d| self.expand_call(&d, &name, span, &call, false, |p| p.parse_pattern()));
        Some(pat.flatten().unwrap_or_else(poison))
    }

    #[allow(clippy::cognitive_complexity)] // one arm per pattern form
    fn parse_pattern_atom_inner(&mut self) -> Option<Pattern> {
        if let Some(pat) = self.macro_call_pattern() {
            return Some(pat);
        }
        let span = self.peek_span();
        let num = match self.peek() {
            TokenKind::Minus => self.tokens.get(self.pos + 1).map(|t| &t.kind),
            k => Some(k),
        };
        if let Some(TokenKind::IntLit(t) | TokenKind::FloatLit(t)) = num {
            if let Some(sfx) = literal::user_suffix(t) {
                self.error(
                    Code::PATTERN_TYPE,
                    span,
                    format!(
                        "a `{sfx}` suffix literal is a `#[literal]` handler call and can't be a \
                         pattern"
                    ),
                );
                return None;
            }
        }
        // Negative numeric literal: `-NUM`.
        if matches!(self.peek(), TokenKind::Minus) {
            self.bump();
            let lit = match self.peek().clone() {
                TokenKind::IntLit(s) => {
                    self.bump();
                    Literal::Int(format!("-{s}"))
                }
                TokenKind::FloatLit(s) => {
                    self.bump();
                    Literal::Float(format!("-{s}"))
                }
                _ => {
                    let s = self.peek_span();
                    self.error(
                        Code::SYNTAX,
                        s,
                        format!(
                            "expected literal after `-`, found {}",
                            self.peek().describe()
                        ),
                    );
                    return None;
                }
            };
            return Some(self.maybe_extend_range(span, lit));
        }
        let kind = match self.peek().clone() {
            TokenKind::IntLit(s) => {
                self.bump();
                return Some(self.maybe_extend_range(span, Literal::Int(s)));
            }
            TokenKind::FloatLit(s) => {
                self.bump();
                return Some(self.maybe_extend_range(span, Literal::Float(s)));
            }
            TokenKind::StrLit(s) => {
                self.bump();
                PatternKind::Literal(Literal::Str(s))
            }
            TokenKind::RawStrLit(s) => {
                self.bump();
                PatternKind::Literal(Literal::RawStr(s))
            }
            TokenKind::CharLit(s) => {
                self.bump();
                return Some(self.maybe_extend_range(span, Literal::Char(s)));
            }
            TokenKind::CCharLit(s) => {
                self.bump();
                return Some(self.maybe_extend_range(span, Literal::CChar(s)));
            }
            TokenKind::True => {
                self.bump();
                PatternKind::Literal(Literal::Bool(true))
            }
            TokenKind::False => {
                self.bump();
                PatternKind::Literal(Literal::Bool(false))
            }
            TokenKind::None => {
                self.bump();
                PatternKind::Literal(Literal::None)
            }
            TokenKind::Ident(name) if name == "_" => {
                self.bump();
                // `_: T` - type-check pattern without a binding.
                if self.eat(&TokenKind::Colon) {
                    let ty = self.parse_type()?;
                    PatternKind::TypeCheck { binder: None, ty }
                } else {
                    PatternKind::Wildcard
                }
            }
            TokenKind::LParen => {
                self.bump();
                let mut subs: Vec<Pattern> = Vec::new();
                let mut trailing_comma = false;
                if !matches!(self.peek(), TokenKind::RParen) {
                    loop {
                        subs.push(self.parse_pattern()?);
                        trailing_comma = self.eat(&TokenKind::Comma);
                        if !trailing_comma || matches!(self.peek(), TokenKind::RParen) {
                            break;
                        }
                    }
                }
                self.expect(&TokenKind::RParen, "`)`")?;
                // `(p)` groups; a 1-tuple pattern is `(p,)`.
                if subs.len() == 1 && !trailing_comma {
                    return subs.pop();
                }
                PatternKind::Tuple(subs)
            }
            TokenKind::Ident(_) => {
                // Multi-segment path, or `Name(..)` (an imported variant
                // such as `Ok(v)`) → enum variant pattern.
                if matches!(
                    self.peek_next(),
                    Some(TokenKind::ColonColon | TokenKind::LParen)
                ) {
                    let path = self.parse_path()?;
                    // Not where a block follows (`catch E::V { body }`).
                    if self.struct_expr_allowed && matches!(self.peek(), TokenKind::LBrace) {
                        self.parse_struct_pattern_body(path)?
                    } else {
                        let args = self.parse_variant_pattern_args()?;
                        PatternKind::EnumVariant { path, args }
                    }
                } else if self.struct_expr_allowed
                    && matches!(self.peek_next(), Some(TokenKind::LBrace))
                {
                    // `Name { .. }` struct pattern. Disabled where a block
                    // follows the pattern (`catch e { body }`).
                    let path = self.parse_path()?;
                    self.parse_struct_pattern_body(path)?
                } else if matches!(self.peek_next(), Some(TokenKind::Colon)) {
                    // `name: T` - type-narrow pattern (sum-type slot).
                    let TokenKind::Ident(name) = self.peek().clone() else {
                        unreachable!()
                    };
                    self.bump(); // ident
                    self.bump(); // ':'
                    let ty = self.parse_type()?;
                    PatternKind::TypeCheck {
                        binder: Some(name),
                        ty,
                    }
                } else {
                    let TokenKind::Ident(name) = self.peek().clone() else {
                        unreachable!()
                    };
                    self.bump();
                    self.parse_binding_rest(name)?
                }
            }
            TokenKind::SelfUpper
                if self.struct_expr_allowed
                    && matches!(self.peek_next(), Some(TokenKind::LBrace)) =>
            {
                self.bump();
                let path = Path {
                    segments: vec![PathSegment {
                        name: "Self".to_owned(),
                        generics: Vec::new(),
                    }],
                    span,
                    origin: None,
                };
                self.parse_struct_pattern_body(path)?
            }
            other => {
                self.error(
                    Code::SYNTAX,
                    span,
                    format!("expected pattern, found {}", other.describe()),
                );
                return None;
            }
        };
        Some(Pattern { kind, span })
    }

    /// Arguments are inside `( )`, so a condition's struct-literal
    /// restriction does not reach them (`if f(S { a: 1 }) { .. }`).
    fn parse_call_args(&mut self) -> Option<Vec<CallArg>> {
        self.with_struct_expr(true, Self::parse_call_args_inner)
    }

    fn parse_call_args_inner(&mut self) -> Option<Vec<CallArg>> {
        let mut args = Vec::new();
        while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
            // `name: expr` is a named argument.
            if let Some(name) = self.peek_name() {
                if matches!(self.peek_next(), Some(TokenKind::Colon)) {
                    self.bump_name(); // ident
                    self.bump(); // `:`
                    let value = self.parse_expr()?;
                    args.push(CallArg::Named { name, value });
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                    continue;
                }
            }
            // `ref x` passes the binding by reference.
            if matches!(self.peek(), TokenKind::Ref) {
                self.bump();
                let value = self.parse_expr()?;
                args.push(CallArg::Ref(value));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                continue;
            }
            // `..expr` is a spread argument.
            if matches!(self.peek(), TokenKind::DotDot) {
                self.bump();
                let value = self.parse_expr()?;
                args.push(CallArg::Spread(value));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                continue;
            }
            let value = self.parse_expr()?;
            args.push(CallArg::Positional(value));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        // Positional arguments come first.
        if let Some(first) = args.iter().position(|a| matches!(a, CallArg::Named { .. })) {
            let late = args[first + 1..].iter().find_map(|a| match a {
                CallArg::Positional(e) | CallArg::Ref(e) => Some(e),
                _ => None,
            });
            if let Some(e) = late {
                self.error_help(
                    Code::NAMED_ARGUMENT,
                    e.span,
                    "positional argument after a named argument",
                    "write the positional arguments first, or name this one too",
                );
            }
        }
        Some(args)
    }
}

/// The types a const parameter may have.
fn const_param_ty(ty: &Ty) -> bool {
    let TyKind::Path(p) = &ty.kind else {
        return false;
    };
    match p.segments.as_slice() {
        [s] if s.generics.is_empty() => matches!(
            s.name.as_str(),
            "i8" | "i16"
                | "i32"
                | "i64"
                | "isize"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "usize"
                | "bool"
                | "char"
                | "c_char"
        ),
        // A path (`ffi::c_char`) or an alias is left to the type check.
        [..] => p.segments.len() > 1,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recover {
    /// Program level: a stray `}` is consumed.
    TopItem,
    /// Inside `impl` / `trait` / `mod` / `extern` braces.
    NestedItem,
    /// Inside a block.
    Stmt,
}

const fn starts_item(k: &TokenKind) -> bool {
    use TokenKind as T;
    matches!(
        k,
        T::Fn
            | T::Struct
            | T::Enum
            | T::Trait
            | T::Impl
            | T::Mod
            | T::Use
            | T::Pub
            | T::HashLBracket
            | T::Unsafe
            | T::Async
    )
}

/// May omit its `;`: a block-like expression or a call with a
/// block-bodied trailing closure (`with_lock(m) { .. }`); the
/// value is discarded.
const fn ends_stmt(e: &Expr) -> bool {
    let (ExprKind::Call {
        trailing_closure: Some(c),
        ..
    }
    | ExprKind::MethodCall {
        trailing_closure: Some(c),
        ..
    }) = &e.kind
    else {
        return is_block_like(e);
    };
    matches!(&c.kind, ExprKind::Closure { body, .. } if matches!(body.kind, ExprKind::Block(_)))
}

/// The name `return@name` gives the trailing closure of call `e`: the
/// method's, or a path callee's last segment.
fn callee_name(e: &Expr) -> Option<String> {
    match &e.kind {
        ExprKind::Call { callee, .. } => path_name(callee),
        ExprKind::MethodCall { method, .. } => Some(method.clone()),
        _ => None,
    }
}

/// A path expression's last segment.
fn path_name(e: &Expr) -> Option<String> {
    match &e.kind {
        ExprKind::Path(p) => p.segments.last().map(|s| s.name.clone()),
        _ => None,
    }
}

/// Tokens as source, one space where the source had any.
fn tokens_text(toks: &[Token]) -> String {
    let mut out = String::new();
    let mut end = None;
    for t in toks {
        if end.is_some_and(|e| e != t.span.start) {
            out.push(' ');
        }
        out.push_str(&t.kind.text());
        end = Some(t.span.end);
    }
    out
}

const fn is_block_like(e: &Expr) -> bool {
    // `try for x in g { .. }` (a throwing iterator) ends like its loop.
    if let ExprKind::Try { operand, .. } = &e.kind {
        return matches!(operand.kind, ExprKind::For { .. });
    }
    matches!(
        e.kind,
        ExprKind::Block(_)
            | ExprKind::Unsafe(_)
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Loop { .. }
            | ExprKind::While { .. }
            | ExprKind::For { .. }
            | ExprKind::Catch { .. }
    )
}

/// `T?` is `Nest(T) | none`, so `(i32 | none)?` keeps two layers while
/// `i32?` is `i32 | none`. `Tuple<A, B>` is `(A, B)`.
fn tuple_or_path(mut p: Path) -> TyKind {
    match p.segments.as_mut_slice() {
        [s] if s.name == "Tuple" && !s.generics.is_empty() => {
            TyKind::Tuple(std::mem::take(&mut s.generics))
        }
        _ => TyKind::Path(p),
    }
}

fn optional_ty(t: Ty) -> Ty {
    let span = t.span;
    let nest = Ty {
        kind: TyKind::Nest(Box::new(t)),
        span,
    };
    let none = Ty {
        kind: TyKind::None,
        span,
    };
    Ty {
        kind: TyKind::Sum(vec![nest, none]),
        span,
    }
}

fn path_ty(name: &str, span: Span) -> Ty {
    Ty {
        kind: TyKind::Path(Path {
            segments: vec![PathSegment {
                name: name.to_owned(),
                generics: Vec::new(),
            }],
            span,
            origin: None,
        }),
        span,
    }
}

/// `fn f(g: fn(T) throws -> U) rethrows -> R` → `fn f<#E0>(g: fn(T)
/// throws (#E0) -> U) throws (#E0) -> R`; monomorphization binds `#E0` to
/// the argument's error, or none. Returns the error types.
fn desugar_rethrows(params: &mut [Param], generics: &mut Generics) -> Vec<Ty> {
    fn walk(ty: &mut Ty, generics: &mut Generics, errs: &mut Vec<Ty>) {
        if rokc_ast::is_untyped_throws(ty) {
            let name = format!("#E{}", errs.len());
            *ty = path_ty(&name, ty.span);
            generics.params.push(rokc_ast::GenericParam::Type {
                name,
                bounds: Vec::new(),
                default: None,
            });
            errs.push(ty.clone());
            return;
        }
        match &mut ty.kind {
            TyKind::Path(p) => {
                for seg in &mut p.segments {
                    for g in &mut seg.generics {
                        walk(g, generics, errs);
                    }
                }
            }
            TyKind::QPath(q) => walk(&mut q.self_ty, generics, errs),
            TyKind::Sum(ts) | TyKind::Tuple(ts) => {
                for t in ts {
                    walk(t, generics, errs);
                }
            }
            TyKind::Fn(args, ret, _) => {
                for t in args {
                    walk(t, generics, errs);
                }
                walk(ret, generics, errs);
            }
            TyKind::Throws { ok, err } => {
                walk(ok, generics, errs);
                walk(err, generics, errs);
            }
            TyKind::Array(t, _) | TyKind::Nest(t) | TyKind::Binding(_, t) => {
                walk(t, generics, errs);
            }
            TyKind::Const(_)
            | TyKind::AnyTrait(_)
            | TyKind::Any
            | TyKind::None
            | TyKind::SelfUpper
            | TyKind::Never => {}
        }
    }
    let mut errs = Vec::new();
    for p in params {
        if let Some(t) = &mut p.ty {
            walk(t, generics, &mut errs);
        }
    }
    errs
}

const fn starts_stmt(k: &TokenKind) -> bool {
    use TokenKind as T;
    matches!(
        k,
        T::Let | T::Return | T::Break | T::Continue | T::Throw | T::Use
    )
}

/// The base operator symbol of a compound-assign token.
const fn compound_assign_op(tok: &rokc_lexer::TokenKind) -> Option<&'static str> {
    use rokc_lexer::TokenKind as T;
    Some(match tok {
        T::PlusEq => "+",
        T::MinusEq => "-",
        T::StarEq => "*",
        T::SlashEq => "/",
        T::PercentEq => "%",
        T::PercentPercentEq => "%%",
        T::AmpEq => "&",
        T::PipeEq => "|",
        T::CaretEq => "^",
        T::ShlEq => "<<",
        T::ShrEq => ">>",
        _ => return None,
    })
}

/// Keywords of other languages and their rok spelling.
fn keyword_alias(word: &str) -> Option<&'static str> {
    Some(match word {
        "func" | "fun" | "function" | "def" | "fucn" | "fnc" => "fn",
        "elif" | "elsif" | "elseif" => "else if",
        "switch" => "match",
        "var" | "val" => "let",
        "null" | "nil" | "None" | "NULL" => "none",
        "class" => "struct",
        "interface" | "protocol" => "trait",
        "import" | "include" | "require" => "use",
        "module" | "namespace" | "package" => "mod",
        "raise" => "throw",
        "except" => "catch",
        _ => return None,
    })
}

/// Help for `import name` (or `include` / `require`): `mod` for a sibling
/// `name.rok`; `None` (the `use` hint) for a name the file knows; else
/// `rok add`.
fn import_help(file: &str, tokens: &[Token], name: &str) -> Option<String> {
    use std::path::Path;
    let path = Path::new(file);
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if dir.join(format!("{name}.rok")).is_file() {
        return Some(format!(
            "to load {name}.rok, write `mod {name};` then call `{name}::f()` or `use {name}::f;`"
        ));
    }
    let declared = tokens.windows(2).any(|w| {
        w[0].kind == TokenKind::Mod && matches!(&w[1].kind, TokenKind::Ident(n) if n == name)
    });
    if declared || matches!(name, "rok" | "crate" | "self" | "super" | "pkg" | "sup") {
        return None;
    }
    // A package's file (under its `src/`) adds to rok.rokon.
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let package = abs
        .ancestors()
        .skip(1)
        .find(|d| d.join("src/main.rok").is_file() || d.join("src/lib.rok").is_file())
        .is_some_and(|d| abs.starts_with(d.join("src")));
    let add = if package {
        format!("rok add {name}")
    } else {
        format!("rok add {name} {file}")
    };
    Some(format!(
        "to use library `{name}`, run `{add}`, then write `use {name}::f;`"
    ))
}

/// The keyword `word` most likely meant: a known alias, else the
/// closest reserved or contextual keyword.
pub(crate) fn keyword_hint(word: &str) -> Option<&'static str> {
    const CONTEXTUAL: &[&str] = &[
        "type", "any", "in", "where", "throws", "rethrows", "catch", "finally", "const", "static",
        "extern", "mut", "union",
    ];
    keyword_alias(word).or_else(|| {
        rokc_diagnostic::suggest(
            word,
            rokc_lexer::token::RESERVED
                .iter()
                .chain(CONTEXTUAL)
                .copied(),
        )
    })
}

/// The marker statement's fn of a nested fn `f` lifted as `lifted`.
fn nested_marker(f: &FnDef, lifted: &str, inherited: bool) -> FnDef {
    let mut args = vec![rokc_ast::AttrArg::Lit(Literal::Str(lifted.to_owned()))];
    if inherited {
        args.push(rokc_ast::AttrArg::Lit(Literal::Bool(true)));
    }
    FnDef {
        vis: Visibility::Private,
        attrs: vec![Attribute {
            path: vec![rokc_ast::NESTED_FN_ATTR.to_owned()],
            body: rokc_ast::AttrBody::Args(args),
            inner: false,
            span: f.span,
        }],
        name: f.name.clone(),
        generics: Generics::default(),
        params: Vec::new(),
        ret_ty: None,
        body: None,
        is_async: false,
        is_generator: false,
        is_unsafe: false,
        throws: f.throws.clone(),
        where_clause: Vec::new(),
        span: f.span,
    }
}

/// `x?.a` followed by a member / index link: the link joins the `?.`
/// tail; anything else (`Err`) ends the optional chain.
fn fold_chain(e: Expr) -> Result<Expr, Expr> {
    let span = e.span;
    let (recv, tail) = match e.kind {
        ExprKind::QuestionDot(..) => return Ok(e),
        ExprKind::Field(r, name) if matches!(r.kind, ExprKind::QuestionDot(..)) => {
            (r, AccessTail::Field(name))
        }
        ExprKind::TupleField(r, i) if matches!(r.kind, ExprKind::QuestionDot(..)) => {
            (r, AccessTail::TupleField(i))
        }
        ExprKind::Index(r, i) if matches!(r.kind, ExprKind::QuestionDot(..)) => {
            (r, AccessTail::Index(i))
        }
        ExprKind::MethodCall {
            receiver,
            method,
            type_args,
            mut args,
            trailing_closure,
            ..
        } if matches!(receiver.kind, ExprKind::QuestionDot(..)) => {
            args.extend(trailing_closure.map(|c| CallArg::Positional(*c)));
            (
                receiver,
                AccessTail::Method {
                    method,
                    type_args,
                    args,
                },
            )
        }
        kind => return Err(Expr { kind, span }),
    };
    let ExprKind::QuestionDot(x, mut tails) = recv.kind else {
        unreachable!("checked above")
    };
    tails.push(tail);
    Ok(Expr {
        kind: ExprKind::QuestionDot(x, tails),
        span,
    })
}

/// The `CFG_OFF_ATTR` marker for method `f`, which its `#[cfg]` dropped.
fn cfg_off_marker(f: &rokc_ast::FnDef) -> Attribute {
    let mut args = vec![rokc_ast::AttrArg::Word(f.name.clone())];
    args.extend(
        f.attrs
            .iter()
            .filter(|a| a.path == ["cfg"])
            .filter_map(|a| match &a.body {
                rokc_ast::AttrBody::Args(xs) if xs.len() == 1 => Some(xs[0].clone()),
                _ => None,
            }),
    );
    Attribute {
        path: vec![rokc_ast::CFG_OFF_ATTR.to_owned(), "method".to_owned()],
        body: rokc_ast::AttrBody::Args(args),
        inner: false,
        span: f.span,
    }
}
