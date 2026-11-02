//! Declarative macro expansion: match a `name!(..)` call's
//! token tree against a rule's matcher, bind the metavariables, and
//! transcribe the rule's template. The output tokens are re-parsed at the
//! call position by the parser. This is the call-site meta definition; it
//! shares the token-template model of `meta.rs` but matches caller input
//! rather than a type's shape.

use std::collections::HashSet;
use std::sync::Arc;

use rokc_diagnostic::{Code, RokDiagnostic, Span};
use rokc_lexer::{Origin, Token, TokenKind};

use crate::MetaDef;

const fn tok(kind: TokenKind, span: Span) -> Token {
    Token::new(kind, span)
}

fn err(code: Code, span: Span, msg: impl Into<String>) -> RokDiagnostic {
    RokDiagnostic::error(code, span, msg.into())
}

/// The matching close of a bracket opened at `open` (`()`, `[]`, `{}`).
pub(super) fn close_of(toks: &[Token], open: usize) -> Option<usize> {
    let opener = &toks[open].kind;
    let closer = match opener {
        TokenKind::LParen => TokenKind::RParen,
        TokenKind::LBracket => TokenKind::RBracket,
        TokenKind::LBrace => TokenKind::RBrace,
        _ => return None,
    };
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match &t.kind {
            // `#[` and `#![` are single opener tokens that close with `]`.
            TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::HashLBracket
            | TokenKind::HashBangLBracket => depth += 1,
            k if *k == closer
                || matches!(
                    k,
                    TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace
                ) =>
            {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// A fragment specifier in a matcher (`@name: <frag>`).
#[derive(Clone, Copy, PartialEq)]
enum Frag {
    /// One identifier.
    Ident,
    /// One literal token.
    Literal,
    /// One token tree (a token, or a balanced group).
    Tt,
    /// A run of tokens up to the next matcher boundary (expr / ty / pat /
    /// path / stmt / block / item): parsed when the expansion is re-parsed.
    Run,
}

impl Frag {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "ident" => Self::Ident,
            "literal" => Self::Literal,
            "tt" => Self::Tt,
            "expr" | "ty" | "pat" | "path" | "stmt" | "block" | "item" => Self::Run,
            _ => return None,
        })
    }
}

/// One element of a matcher.
enum MatchItem {
    /// A literal token that must appear verbatim.
    Lit(TokenKind),
    /// `@name: frag` - capture.
    Var { name: String, frag: Frag },
    /// `@rep[sep = ",", trailing] { sub }` - zero or more of `sub`.
    Rep {
        sep: Option<String>,
        sub: Vec<MatchItem>,
    },
}

/// One macro rule: a matcher and the template to transcribe.
struct Rule {
    matcher: Vec<MatchItem>,
    template: Vec<Token>,
}

/// A captured metavariable: one run, or a sequence (inside `@rep`).
#[derive(Clone)]
enum Binding {
    One(Vec<Token>),
    Many(Vec<Vec<Token>>),
}

type Bindings = std::collections::HashMap<String, Binding>;

/// Split a macro block body into its rules: `( matcher ) => { template }`.
fn parse_rules(body: &[Token], span: Span) -> Result<Vec<Rule>, RokDiagnostic> {
    let mut rules = Vec::new();
    let mut i = 0;
    while i < body.len() {
        if body[i].kind != TokenKind::LParen {
            return Err(err(
                Code::INVALID_MACRO,
                body[i].span,
                "a macro rule starts with `( matcher )`",
            ));
        }
        let mp = close_of(body, i).ok_or_else(|| {
            err(
                Code::INVALID_MACRO,
                body[i].span,
                "unbalanced `(` in macro matcher",
            )
        })?;
        let matcher = parse_matcher(&body[i + 1..mp])?;
        i = mp + 1;
        if body.get(i).map(|t| &t.kind) != Some(&TokenKind::FatArrow) {
            return Err(err(
                Code::INVALID_MACRO,
                body.get(i).map_or(span, |t| t.span),
                "a macro rule needs `=>` after its matcher",
            ));
        }
        i += 1;
        if body.get(i).map(|t| &t.kind) != Some(&TokenKind::LBrace) {
            return Err(err(
                Code::INVALID_MACRO,
                body.get(i).map_or(span, |t| t.span),
                "a macro rule needs a `{ template }` body",
            ));
        }
        let tp = close_of(body, i).ok_or_else(|| {
            err(
                Code::INVALID_MACRO,
                body[i].span,
                "unbalanced `{` in macro template",
            )
        })?;
        rules.push(Rule {
            matcher,
            template: body[i + 1..tp].to_vec(),
        });
        i = tp + 1;
    }
    if rules.is_empty() {
        return Err(err(
            Code::INVALID_MACRO,
            span,
            "a macro needs at least one `( matcher ) => { template }` rule",
        ));
    }
    Ok(rules)
}

/// Parse matcher tokens into items. `@name: frag`, `@rep[..] { .. }`, or a
/// literal token.
fn parse_matcher(toks: &[Token]) -> Result<Vec<MatchItem>, RokDiagnostic> {
    let mut items = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i].kind == TokenKind::At {
            let Some(TokenKind::Ident(head)) = toks.get(i + 1).map(|t| &t.kind) else {
                return Err(err(
                    Code::INVALID_MACRO,
                    toks[i].span,
                    "`@` in a matcher must be `@name: frag` or `@rep`",
                ));
            };
            if head == "rep" {
                // @rep [ opts ] { sub }
                let lb = i + 2;
                if toks.get(lb).map(|t| &t.kind) != Some(&TokenKind::LBracket) {
                    return Err(err(
                        Code::INVALID_MACRO,
                        toks[i].span,
                        "`@rep` needs `[ sep = \",\" ]`",
                    ));
                }
                let rb = close_of(toks, lb).ok_or_else(|| {
                    err(
                        Code::INVALID_MACRO,
                        toks[lb].span,
                        "unbalanced `[` in `@rep`",
                    )
                })?;
                let sep = rep_sep(&toks[lb + 1..rb]);
                let br = rb + 1;
                if toks.get(br).map(|t| &t.kind) != Some(&TokenKind::LBrace) {
                    return Err(err(
                        Code::INVALID_MACRO,
                        toks[i].span,
                        "`@rep[..]` needs a `{ sub-pattern }`",
                    ));
                }
                let be = close_of(toks, br).ok_or_else(|| {
                    err(
                        Code::INVALID_MACRO,
                        toks[br].span,
                        "unbalanced `{` in `@rep`",
                    )
                })?;
                let sub = parse_matcher(&toks[br + 1..be])?;
                items.push(MatchItem::Rep { sep, sub });
                i = be + 1;
            } else {
                // @name : frag
                if toks.get(i + 2).map(|t| &t.kind) != Some(&TokenKind::Colon) {
                    return Err(err(
                        Code::INVALID_MACRO,
                        toks[i].span,
                        "a metavariable is `@name: frag`",
                    ));
                }
                let Some(TokenKind::Ident(fname)) = toks.get(i + 3).map(|t| &t.kind) else {
                    return Err(err(
                        Code::INVALID_MACRO,
                        toks[i].span,
                        "`@name:` needs a fragment (expr, ty, ident, ..)",
                    ));
                };
                let frag = Frag::parse(fname).ok_or_else(|| {
                    err(
                        Code::INVALID_MACRO,
                        toks[i + 3].span,
                        format!("unknown fragment `{fname}`"),
                    )
                })?;
                items.push(MatchItem::Var {
                    name: head.clone(),
                    frag,
                });
                i += 4;
            }
        } else {
            items.push(MatchItem::Lit(toks[i].kind.clone()));
            i += 1;
        }
    }
    Ok(items)
}

/// The separator string of `@rep[sep = ","]` (`None` if absent).
fn rep_sep(opts: &[Token]) -> Option<String> {
    opts.windows(3).find_map(|w| {
        (matches!(&w[0].kind, TokenKind::Ident(n) if n == "sep") && w[1].kind == TokenKind::Eq)
            .then(|| match &w[2].kind {
                TokenKind::StrLit(s) => Some(s.clone()),
                k => Some(k.text()),
            })
            .flatten()
    })
}

/// The first literal token that bounds a capture after item `idx` (what a
/// `Run` fragment stops before), if the next item is a literal.
fn follow(items: &[MatchItem], idx: usize) -> Option<String> {
    match items.get(idx + 1) {
        Some(MatchItem::Lit(k)) => Some(k.text()),
        _ => None,
    }
}

/// Match `call` against `items`, filling `out`. Returns whether it matched
/// (and consumed all of `call`).
fn match_items(items: &[MatchItem], call: &[Token], out: &mut Bindings, sep: Option<&str>) -> bool {
    let mut i = 0; // call index
    for (n, item) in items.iter().enumerate() {
        match item {
            MatchItem::Lit(k) => {
                if call.get(i).map(|t| &t.kind) != Some(k) {
                    return false;
                }
                i += 1;
            }
            MatchItem::Var { name, frag } => {
                let stop = follow(items, n);
                let Some((toks, next)) = capture(call, i, *frag, stop.as_deref().or(sep)) else {
                    return false;
                };
                out.insert(name.clone(), Binding::One(toks));
                i = next;
            }
            MatchItem::Rep { sep: rsep, sub } => {
                // Repeat `sub` separated by `rsep` until call is exhausted.
                let names = sub_var_names(sub);
                let mut seqs: Vec<Vec<Vec<Token>>> = vec![Vec::new(); names.len()];
                while i < call.len() {
                    let mut one = Bindings::new();
                    // Match one `sub` over the run up to the next separator.
                    let end = rsep
                        .as_deref()
                        .and_then(|s| boundary(call, i, s))
                        .unwrap_or(call.len());
                    if !match_items(sub, &call[i..end], &mut one, rsep.as_deref()) {
                        return false;
                    }
                    for (j, nm) in names.iter().enumerate() {
                        if let Some(Binding::One(t)) = one.remove(nm) {
                            seqs[j].push(t);
                        }
                    }
                    i = end;
                    if i < call.len() {
                        // consume the separator
                        if rsep.as_deref().is_some_and(|s| call[i].kind.text() == s) {
                            i += 1;
                        } else {
                            return false;
                        }
                    }
                }
                for (j, nm) in names.iter().enumerate() {
                    out.insert(nm.clone(), Binding::Many(std::mem::take(&mut seqs[j])));
                }
            }
        }
    }
    i == call.len()
}

/// The metavariable names a (flat) sub-pattern captures.
fn sub_var_names(sub: &[MatchItem]) -> Vec<String> {
    sub.iter()
        .filter_map(|it| match it {
            MatchItem::Var { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Index in `call` (at depth 0, from `start`) of the next token whose text
/// is `sep`.
fn boundary(call: &[Token], start: usize, sep: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in call.iter().enumerate().skip(start) {
        // Check the separator before adjusting depth, so a closing-bracket
        // separator (e.g. `)` ending a `( @t, @v )` sub-pattern) is found at
        // the current level rather than swallowed as a depth decrement.
        if depth == 0 && t.kind.text() == sep {
            return Some(i);
        }
        match &t.kind {
            TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::HashLBracket
            | TokenKind::HashBangLBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Capture a fragment from `call` at `i`; the tokens and the next index.
fn capture(
    call: &[Token],
    i: usize,
    frag: Frag,
    stop: Option<&str>,
) -> Option<(Vec<Token>, usize)> {
    match frag {
        Frag::Ident => matches!(call.get(i).map(|t| &t.kind), Some(TokenKind::Ident(_)))
            .then(|| (vec![call[i].clone()], i + 1)),
        Frag::Literal => call
            .get(i)
            .filter(|t| is_literal(&t.kind))
            .map(|t| (vec![t.clone()], i + 1)),
        Frag::Tt => {
            let t = call.get(i)?;
            match t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                    let e = close_of(call, i)?;
                    Some((call[i..=e].to_vec(), e + 1))
                }
                _ => Some((vec![t.clone()], i + 1)),
            }
        }
        Frag::Run => {
            let end = stop
                .and_then(|s| boundary(call, i, s))
                .unwrap_or(call.len());
            (end > i).then(|| (call[i..end].to_vec(), end))
        }
    }
}

const fn is_literal(k: &TokenKind) -> bool {
    matches!(
        k,
        TokenKind::IntLit(_)
            | TokenKind::FloatLit(_)
            | TokenKind::StrLit(_)
            | TokenKind::RawStrLit(_)
            | TokenKind::ByteStrLit(_)
            | TokenKind::CharLit(_)
    )
}

/// Expand `def` on the `call` token tree and render the result as rok
/// source, for tooling (e.g. the language server's "expand in place");
/// `None` if no rule matches.
pub fn expand_to_text(def: &MetaDef, call: &[Token], at: Span) -> Option<String> {
    let (out, _) = expand_macro(def, call, at);
    out.map(|t| super::meta::render(&t))
}

/// Expand `def` on the `call` token tree; the output tokens (to re-parse)
/// or diagnostics.
pub(super) fn expand_macro(
    def: &MetaDef,
    call: &[Token],
    at: Span,
) -> (Option<Vec<Token>>, Vec<RokDiagnostic>) {
    let rules = match parse_rules(&def.body, def.span) {
        Ok(r) => r,
        Err(d) => return (None, vec![d]),
    };
    for rule in &rules {
        let mut binds = Bindings::new();
        if match_items(&rule.matcher, call, &mut binds, None) {
            let (mut out, errs) = transcribe(&rule.template, &binds, at, &def.origin());
            super::meta::place_declared(&mut out);
            return (Some(out), errs);
        }
    }
    (
        None,
        vec![err(
            Code::NO_MACRO_RULE,
            at,
            format!("no rule of macro `{}!` matches this call", def.name),
        )],
    )
}

/// Transcribe a template with the bindings, remapping spans to the call
/// and stamping the template's own tokens with `origin` (captures keep
/// theirs); the tokens and any diagnostics (an unknown `@token` in the
/// template).
fn transcribe(
    template: &[Token],
    binds: &Bindings,
    at: Span,
    origin: &Arc<Origin>,
) -> (Vec<Token>, Vec<RokDiagnostic>) {
    let bound = template_binders(template, binds);
    let mut out = Vec::new();
    let mut errs = Vec::new();
    let site = Site { at, origin };
    emit(template, binds, &bound, None, &site, &mut out, &mut errs);
    (out, errs)
}

/// Names the template binds itself (let / for / closure), to make hygienic.
fn template_binders(template: &[Token], binds: &Bindings) -> HashSet<String> {
    let mut names = HashSet::new();
    for k in 0..template.len() {
        let prev_at = k > 0 && template[k - 1].kind == TokenKind::At;
        if prev_at {
            continue;
        }
        let binds_here = matches!(template[k].kind, TokenKind::Let | TokenKind::For)
            || matches!(&template[k].kind, TokenKind::Ident(n) if n == "let");
        if binds_here {
            let mut j = k + 1;
            if matches!(&template.get(j).map(|t| &t.kind), Some(TokenKind::Ident(m)) if m == "mut")
            {
                j += 1;
            }
            if let Some(TokenKind::Ident(n)) = template.get(j).map(|t| &t.kind) {
                if !binds.contains_key(n) && n != "mut" && n != "_" {
                    names.insert(n.clone());
                }
            }
        }
    }
    names
}

fn hygienic(name: &str) -> String {
    format!("{name}__mac")
}

/// Replace `@name` in a string / f-string with the bound metavariable's
/// text (the current `@for` element first, then the outer bindings).
fn interp(s: &str, binds: &Bindings, rep: Option<&Bindings>) -> String {
    let mut out = s.to_owned();
    let mut sub = |name: &str, b: &Binding| {
        if let Binding::One(ts) = b {
            let text: String = ts.iter().map(|t| t.kind.text()).collect();
            out = out.replace(&format!("@{name}"), &text);
        }
    };
    if let Some(r) = rep {
        for (name, b) in r {
            sub(name, b);
        }
    }
    for (name, b) in binds {
        if rep.is_none_or(|r| !r.contains_key(name)) {
            sub(name, b);
        }
    }
    out
}

/// Emit `template`, substituting metavariables and `@for` / `@concat`,
/// renaming template-bound locals. `rep` is the current `@for` element's
/// bindings, if inside one.
/// Where a transcription lands: the call, and the definition its
/// template tokens come from.
struct Site<'a> {
    at: Span,
    origin: &'a Arc<Origin>,
}

impl Site<'_> {
    /// A token the template writes.
    fn template(&self, kind: TokenKind, had: Option<&Arc<Origin>>) -> Token {
        let mut t = tok(kind, self.at);
        t.origin = Some(Arc::clone(had.unwrap_or(self.origin)));
        t
    }

    /// A token the call passed in.
    fn passed(&self, s: Token) -> Token {
        let mut t = tok(s.kind, self.at);
        t.origin = s.origin;
        t
    }
}

fn emit(
    template: &[Token],
    binds: &Bindings,
    bound: &HashSet<String>,
    rep: Option<&Bindings>,
    site: &Site<'_>,
    out: &mut Vec<Token>,
    errs: &mut Vec<RokDiagnostic>,
) {
    let at = site.at;
    let get = |name: &str| -> Option<Vec<Token>> {
        if let Some(r) = rep {
            if let Some(Binding::One(t)) = r.get(name) {
                return Some(t.clone());
            }
        }
        match binds.get(name) {
            Some(Binding::One(t)) => Some(t.clone()),
            _ => None,
        }
    };
    let mut k = 0;
    while k < template.len() {
        let t = &template[k];
        if t.kind == TokenKind::At {
            // `for` is a keyword token, not an identifier.
            if template.get(k + 1).map(|t| &t.kind) == Some(&TokenKind::For) {
                k = emit_for(template, k, binds, bound, site, out, errs);
                continue;
            }
            if let Some(TokenKind::Ident(head)) = template.get(k + 1).map(|t| &t.kind) {
                if head == "concat" {
                    k = emit_concat(template, k, binds, rep, site, out);
                    continue;
                }
                if head == "use_site" {
                    k = emit_use_site(template, k, site, out, errs);
                    continue;
                }
                if let Some(toks) = get(head) {
                    for s in toks {
                        out.push(site.passed(s));
                    }
                    k += 2;
                    continue;
                }
                // Not a metavariable, `@for` or `@concat`: the shape tokens
                // (`@Self`, `@field`, `@apply_if`, ..) belong to the
                // type-driven forms, not a macro.
                errs.push(err(
                    Code::INVALID_TEMPLATE,
                    at,
                    format!(
                        "`@{head}` is not valid in a macro template; use a metavariable, \
                         `@for`, `@concat` or `@use_site`"
                    ),
                ));
                k += 2;
                continue;
            }
        }
        // A plain token: rename a template-bound local, else emit as is.
        let kind = match &t.kind {
            TokenKind::Ident(n) if bound.contains(n) && renames(template, k) => {
                TokenKind::Ident(hygienic(n))
            }
            // `@name` inside a string / f-string interpolates the binding.
            TokenKind::StrLit(s) => TokenKind::StrLit(interp(s, binds, rep)),
            TokenKind::FStrText(s) => TokenKind::FStrText(interp(s, binds, rep)),
            other => other.clone(),
        };
        out.push(site.template(kind, t.origin.as_ref()));
        k += 1;
    }
}

/// `@use_site(path)`: the path, looked up where the macro is called.
fn emit_use_site(
    template: &[Token],
    at_tok: usize,
    site: &Site<'_>,
    out: &mut Vec<Token>,
    errs: &mut Vec<RokDiagnostic>,
) -> usize {
    let lp = at_tok + 2;
    let path = (template.get(lp).map(|t| &t.kind) == Some(&TokenKind::LParen))
        .then(|| close_of(template, lp))
        .flatten()
        .and_then(|rp| Some((rp, super::meta::use_site_path(&template[lp + 1..rp])?)));
    let Some((rp, toks)) = path else {
        errs.push(err(
            Code::INVALID_TEMPLATE,
            site.at,
            "`@use_site(..)` takes a path, e.g. `@use_site(validate)`",
        ));
        return lp;
    };
    out.extend(toks.into_iter().map(|t| tok(t.kind, site.at)));
    rp + 1
}

/// `@for @name { body }`: emit `body` once per element of `@name`'s
/// sequence, binding each repeated metavariable in lockstep.
fn emit_for(
    template: &[Token],
    at_tok: usize,
    binds: &Bindings,
    bound: &HashSet<String>,
    site: &Site<'_>,
    out: &mut Vec<Token>,
    errs: &mut Vec<RokDiagnostic>,
) -> usize {
    // @ for @ name { body }
    let name_at = at_tok + 3; // At for At name
    let name = match template.get(at_tok + 2).map(|t| &t.kind) {
        Some(TokenKind::At) => match template.get(at_tok + 3).map(|t| &t.kind) {
            Some(TokenKind::Ident(n)) => n.clone(),
            _ => return at_tok + 2,
        },
        _ => return at_tok + 2,
    };
    let _ = name_at;
    let brace = at_tok + 4;
    let Some(TokenKind::LBrace) = template.get(brace).map(|t| &t.kind) else {
        return brace;
    };
    let end = close_of(template, brace).unwrap_or(template.len() - 1);
    let body = &template[brace + 1..end];
    // The lockstep sequences: every Many binding used in the body.
    let len = match binds.get(&name) {
        Some(Binding::Many(v)) => v.len(),
        _ => 0,
    };
    let many: Vec<&String> = binds
        .iter()
        .filter_map(|(k, v)| matches!(v, Binding::Many(_)).then_some(k))
        .collect();
    for idx in 0..len {
        let mut elem = Bindings::new();
        for nm in &many {
            if let Some(Binding::Many(v)) = binds.get(*nm) {
                if let Some(t) = v.get(idx) {
                    elem.insert((*nm).clone(), Binding::One(t.clone()));
                }
            }
        }
        emit(body, binds, bound, Some(&elem), site, out, errs);
    }
    end + 1
}

/// `@concat(a, @x, "s")`: join name tokens and string literals into one
/// identifier.
fn emit_concat(
    template: &[Token],
    at_tok: usize,
    binds: &Bindings,
    rep: Option<&Bindings>,
    site: &Site<'_>,
    out: &mut Vec<Token>,
) -> usize {
    // The template's name only if every piece is the template's.
    let mut passed = false;
    let lp = at_tok + 2;
    let Some(TokenKind::LParen) = template.get(lp).map(|t| &t.kind) else {
        return lp;
    };
    let rp = close_of(template, lp).unwrap_or(template.len() - 1);
    let inner = &template[lp + 1..rp];
    let mut name = String::new();
    let mut k = 0;
    while k < inner.len() {
        match &inner[k].kind {
            TokenKind::Comma => {}
            TokenKind::At => {
                if let Some(TokenKind::Ident(n)) = inner.get(k + 1).map(|t| &t.kind) {
                    let toks = rep.and_then(|r| r.get(n)).or_else(|| binds.get(n)).cloned();
                    if let Some(Binding::One(ts)) = toks {
                        passed |= ts.iter().any(|s| s.origin.is_none());
                        for s in &ts {
                            match &s.kind {
                                // `r#match` pastes as `match`.
                                TokenKind::Ident(n) => name.push_str(n),
                                k => name.push_str(&k.text()),
                            }
                        }
                    }
                    k += 2;
                    continue;
                }
            }
            TokenKind::StrLit(s) => name.push_str(s),
            TokenKind::Ident(n) => name.push_str(n),
            other => name.push_str(&other.text()),
        }
        k += 1;
    }
    out.push(if passed {
        tok(TokenKind::Ident(name), site.at)
    } else {
        site.template(TokenKind::Ident(name), None)
    });
    rp + 1
}

/// Whether the template-bound ident at `k` is a variable use / binding
/// (not a member `.x`, path `::x`, or metavar `@x`).
fn renames(template: &[Token], k: usize) -> bool {
    let prev = k.checked_sub(1).map(|p| &template[p].kind);
    !matches!(
        prev,
        Some(TokenKind::Dot | TokenKind::ColonColon | TokenKind::At)
    )
}
