//! Completion, shared by the language server and the REPL: members after
//! `recv.`, path items after `a::b::` (and in `use`), else the names in
//! scope.

use std::collections::HashSet;

use rokc_ast::Item;
use rokc_diagnostic::Span;
use rokc_hir::{HirModule, HirTy};

use crate::analysis::{Analysis, short_name, signature, ty_name};
use crate::lsp::{is_ident_char, locals, parse_doc, receiver_start};
use crate::members::{Kind, Members};

/// A completion candidate: label, LSP `CompletionItemKind`, detail.
pub type Candidate = (String, u32, String);

// LSP `CompletionItemKind`s.
const METHOD: u32 = 2;
const FUNCTION: u32 = 3;
const FIELD: u32 = 5;
const VARIABLE: u32 = 6;
const CLASS: u32 = 7;
const MODULE: u32 = 9;
const KEYWORD: u32 = 14;

/// Completion at `off`: after `recv.` the receiver type's fields and
/// methods, after `a::b::` the items and modules under it, else the
/// locals in scope, `extra` names, the buffer's items, the names in
/// scope and keywords. `compile` checks an edited `text`; `good` is the
/// last type-checked compile. Returns the word start and candidates.
pub fn complete(
    text: &str,
    key: &str,
    off: usize,
    compile: impl Fn(&str) -> Analysis,
    good: Option<&Analysis>,
    extra: &[&str],
) -> (usize, Vec<Candidate>) {
    let line_start = text[..off].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[off..].find('\n').map_or(text.len(), |i| off + i);
    let before = &text[line_start..off];
    let head = before.trim_end_matches(is_ident_char);
    let partial = &before[head.len()..];
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |label: &str, kind: u32, detail: String| {
        if label.starts_with(partial) && !label.starts_with("__") && seen.insert(label.to_owned()) {
            out.push((label.to_owned(), kind, detail));
        }
    };
    let blank = format!("{}{}", &text[..line_start], &text[line_end..]);
    let mut fresh = None;
    if head.ends_with('.') && !head.ends_with("..") {
        let dot = line_start + head.len() - 1;
        members(text, key, dot, off, &compile, good, &mut push);
    } else if let Some(prefix) = head.strip_suffix("::") {
        let start = prefix
            .rfind(|c: char| !(is_ident_char(c) || c == ':'))
            .map_or(0, |i| i + 1);
        let segs: Vec<&str> = prefix[start..].split("::").collect();
        // Item names only: the last good compile has them (the
        // buffer with a dangling `a::` does not compile).
        if let Some(a) = checked(good, &mut fresh, &compile, &blank) {
            if let Some(hir) = &a.hir {
                path_completions(hir, &visible(a), &segs, &mut push);
            }
            module_paths(a, &segs, &mut push);
        }
    } else {
        let program = parse_doc(text, key);
        // The code up to the cursor, closed: a half-typed call still
        // parses, and its locals with it.
        let head = parse_doc(&closed(text, off), key);
        for name in locals(&head, off).visible() {
            push(&name, VARIABLE, String::new());
        }
        for name in extra {
            push(name, VARIABLE, String::new());
        }
        for item in &program.items {
            if let (Some(name), Some(kind)) = (item.name(), item_kind(item)) {
                push(name, kind, String::new());
            }
        }
        if let Some(a) = checked(good, &mut fresh, &compile, &blank) {
            for name in a.scope_names() {
                push(&name, guess_kind(&name), String::new());
            }
        }
        let keywords = rokc_lexer::token::RESERVED
            .iter()
            .chain(crate::line_edit::CONTEXTUAL);
        for k in keywords {
            push(k, KEYWORD, String::new());
        }
    }
    (off - partial.len(), out)
}

/// `text` up to `off`, each open bracket closed (a block's after a `;`
/// that ends the statement being typed).
pub fn closed(text: &str, off: usize) -> String {
    use rokc_lexer::TokenKind as K;
    let mut out = text[..off].to_owned();
    let (toks, _) = rokc_lexer::Lexer::new(&out, String::new()).lex_all();
    let mut open = Vec::new();
    for t in &toks {
        match &t.kind {
            K::LParen => open.push(")"),
            K::LBracket | K::HashLBracket | K::HashBangLBracket => open.push("]"),
            K::LBrace => open.push(";}"),
            K::HoleStart => open.push("}"),
            K::FStrStart(triple) => open.push(if *triple { "\"\"\"" } else { "\"" }),
            K::RParen | K::RBracket | K::RBrace | K::HoleEnd | K::FStrEnd(_) => {
                open.pop();
            }
            _ => {}
        }
    }
    for c in open.iter().rev() {
        out.push_str(c);
    }
    out.push(';');
    out
}

/// The last good compile; without one, the buffer without the line
/// being typed (`blank`), else an empty one: what has the library.
pub fn checked<'a>(
    good: Option<&'a Analysis>,
    fresh: &'a mut Option<Analysis>,
    compile: &impl Fn(&str) -> Analysis,
    blank: &str,
) -> Option<&'a Analysis> {
    if good.is_some() {
        return good;
    }
    for t in [blank, ""] {
        let a = compile(t);
        if a.library.is_some() || !a.sources.is_empty() {
            return Some(fresh.insert(a));
        }
    }
    None
}

/// Fields and methods after the `.` at `dot`.
fn members(
    text: &str,
    key: &str,
    dot: usize,
    off: usize,
    compile: &impl Fn(&str) -> Analysis,
    good: Option<&Analysis>,
    push: &mut impl FnMut(&str, u32, String),
) {
    receiver(text, key, dot, off, compile, good, |a, ty| {
        list_members(a, ty, push);
    });
}

/// `f` of the type of the receiver of the `.` at `dot`: recompile
/// without `.partial` (up to `off`) and take the outermost expression
/// starting where the receiver does, else the last good compile's.
pub fn receiver<R>(
    text: &str,
    key: &str,
    dot: usize,
    off: usize,
    compile: &impl Fn(&str) -> Analysis,
    good: Option<&Analysis>,
    f: impl FnOnce(&Analysis, &HirTy) -> R,
) -> Option<R> {
    let start = receiver_start(text, dot);
    // At a line end the receiver ends a statement: close it.
    let rest = &text[off..];
    let at_end = rest.split('\n').next().is_some_and(|l| l.trim().is_empty());
    let edited = format!("{}{}{rest}", &text[..dot], if at_end { ";" } else { "" });
    let fresh = compile(&edited);
    let starts = |s: Span| s.start as usize == start;
    let found = fresh
        .find_expr(key, starts, true)
        .map(|e| (&fresh, e.ty.clone()))
        .or_else(|| {
            let a = good?;
            Some((a, a.find_expr(key, starts, true)?.ty.clone()))
        });
    let (a, ty) = found?;
    Some(f(a, &ty))
}

/// The fields and methods of `ty`.
fn list_members(a: &Analysis, ty: &HirTy, push: &mut impl FnMut(&str, u32, String)) {
    let hir = a.hir.as_ref().expect("find_expr needs HIR");
    let index = Members::of(a);
    let mut any = false;
    for m in index.get(&ty_name(hir, ty)) {
        any = true;
        match m.kind {
            Kind::Method => {
                let detail = match &m.via_trait {
                    Some(t) => format!("{} (trait {t})", m.sig),
                    None => m.sig.clone(),
                };
                push(&m.name, METHOD, detail);
            }
            Kind::Field => push(&m.name, FIELD, m.sig.clone()),
            Kind::Assoc => {}
        }
    }
    if any {
        return;
    }
    // No source (a prebuilt dependency's type): its HIR.
    let hidden = a.hidden_spans();
    if let HirTy::Struct(id) = ty {
        if let Some(s) = hir.structs.iter().find(|s| s.id == *id) {
            for f in &s.fields {
                push(&f.name, FIELD, ty_name(hir, &f.ty));
            }
        }
    }
    for f in &hir.functions {
        if f.params
            .first()
            .is_some_and(|p| p.name == "self" && p.ty == *ty)
            && !hidden.contains(&f.span)
        {
            push(short_name(&f.name), METHOD, signature(hir, f));
        }
    }
}

/// Which HIR items a path completion offers: not `#[doc(hidden)]`; of
/// the library or another package, only what is public (an item, or a
/// shown member for `Type__method`). Items with no source are kept.
fn visible(a: &Analysis) -> impl Fn(&str, Span) -> bool + '_ {
    let hidden = a.hidden_spans();
    let mut own = HashSet::new();
    let mut foreign = HashSet::new();
    for f in &a.sources {
        if f.package.is_empty() {
            own.insert(f.file_id);
        } else {
            foreign.insert(f.file_id);
        }
    }
    for item in a.library.iter().flat_map(|p| &p.items) {
        foreign.insert(item.span().file);
    }
    let public: HashSet<String> = a
        .public_items()
        .into_iter()
        .map(|(m, n, _)| if m.is_empty() { n } else { format!("{m}::{n}") })
        .collect();
    let index = Members::of(a);
    move |name: &str, span: Span| {
        if hidden.contains(&span) {
            return false;
        }
        if own.contains(&span.file) || !foreign.contains(&span.file) {
            return true;
        }
        let base = name.split_once("__mono__").map_or(name, |(b, _)| b);
        if public.contains(base) {
            return true;
        }
        base.split_once("__")
            .is_some_and(|(ty, m)| index.get(ty).any(|x| x.name == short_name(m)))
    }
}

/// `a::b::` → the next segments of every item path under it, plus the
/// associated functions / variants of a type named by the path: those
/// `shown` takes (by HIR name and span).
fn path_completions(
    hir: &HirModule,
    shown: &dyn Fn(&str, Span) -> bool,
    segs: &[&str],
    push: &mut impl FnMut(&str, u32, String),
) {
    let n = segs.len();
    let mut names: Vec<(&str, u32)> = Vec::new();
    names.extend(
        hir.functions
            .iter()
            .filter(|f| !f.name.contains("__") && !f.name.contains('.') && shown(&f.name, f.span))
            .map(|f| (f.name.as_str(), FUNCTION)),
    );
    names.extend(
        hir.structs
            .iter()
            .filter(|s| shown(&s.name, s.span))
            .map(|s| (s.name.as_str(), 22)),
    );
    names.extend(
        hir.enums
            .iter()
            .filter(|e| shown(&e.name, e.span))
            .map(|e| (e.name.as_str(), 13)),
    );
    for (name, kind) in names {
        let name = name.split("__").next().unwrap_or(name);
        let parts: Vec<&str> = name.split("::").collect();
        if parts.len() > n && parts[..n] == *segs {
            let more = parts.len() > n + 1;
            push(parts[n], if more { MODULE } else { kind }, name.to_owned());
        }
    }
    // `Type::` - its impl functions are named `<key>__<method>`.
    let last = segs[n - 1];
    let mut keys = vec![last.to_owned()];
    for s in &hir.structs {
        if short_name(&s.name) == last || s.name.rsplit("::").next() == Some(last) {
            keys.push(format!("Struct{}", s.id.0));
        }
    }
    for e in &hir.enums {
        if e.name.rsplit("::").next() == Some(last) && shown(&e.name, e.span) {
            keys.push(format!("Enum{}", e.id.0));
            for v in &e.variants {
                push(&v.name, 20, e.name.clone());
            }
        }
    }
    for f in hir.functions.iter().filter(|f| shown(&f.name, f.span)) {
        if let Some((key, method)) = f.name.split_once("__") {
            if keys
                .iter()
                .any(|k| k == key || Some(k.as_str()) == key.rsplit("::").next())
            {
                push(short_name(method), FUNCTION, signature(hir, f));
            }
        }
        // A dependency's method: `<module>::<Type>.<method>`.
        if let Some((ty, _)) = f.name.rsplit("::").next().and_then(|l| l.split_once('.')) {
            if ty == last {
                push(short_name(&f.name), FUNCTION, signature(hir, f));
            }
        }
    }
}

/// `a::b::` → the public modules and items below `a::b` (what `use`
/// can name), from the ASTs.
fn module_paths(a: &Analysis, segs: &[&str], push: &mut impl FnMut(&str, u32, String)) {
    let n = segs.len();
    for (module, name, is_mod) in a.public_items() {
        let full = if module.is_empty() {
            name
        } else {
            format!("{module}::{name}")
        };
        let parts: Vec<&str> = full.split("::").collect();
        if parts.len() > n && parts[..n] == *segs {
            let kind = if is_mod || parts.len() > n + 1 {
                MODULE
            } else {
                guess_kind(parts[n])
            };
            push(parts[n], kind, full.clone());
        }
    }
}

/// A name's kind by its case: types are capitalised.
fn guess_kind(name: &str) -> u32 {
    if name.starts_with(char::is_uppercase) {
        CLASS
    } else {
        FUNCTION
    }
}

const fn item_kind(item: &Item) -> Option<u32> {
    Some(match item {
        Item::Fn(_) => FUNCTION,
        Item::Struct(_) | Item::Union(_) => 22,
        Item::Enum(_) => 13,
        Item::Trait(_) => 8,
        Item::Const(_) => 21,
        Item::Static(_) => VARIABLE,
        Item::TypeAlias(_) => CLASS,
        Item::Mod(_) => MODULE,
        Item::Impl(_) | Item::Use(_) | Item::Extern(_) => return None,
    })
}
