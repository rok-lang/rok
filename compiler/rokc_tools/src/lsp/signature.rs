//! `textDocument/signatureHelp`: the signature of the innermost call
//! around the cursor, with the parameter being typed active.

use rokc_diagnostic::Json;
use rokc_lexer::TokenKind as K;

use crate::analysis::{Analysis, short_name, signature, ty_name};
use crate::complete::{checked, receiver};
use crate::members::{Kind, Members};

/// The innermost call `(` still open at the end of `toks` (its index)
/// and how many top-level commas follow it.
fn open_call(toks: &[rokc_lexer::Token]) -> Option<(usize, u32)> {
    // (token index, commas, is `(`)
    let mut stack: Vec<(usize, u32, bool)> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match &t.kind {
            K::LParen => stack.push((i, 0, true)),
            K::LBracket | K::LBrace | K::HashLBracket | K::HashBangLBracket | K::HoleStart => {
                stack.push((i, 0, false));
            }
            K::RParen | K::RBracket | K::RBrace | K::HoleEnd => {
                stack.pop();
            }
            K::Comma => {
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
            }
            _ => {}
        }
    }
    // Inside a list argument (`f([1, 2`) the call still counts; inside
    // a block (a closure body) it does not.
    while let Some(&(i, commas, paren)) = stack.last() {
        if paren {
            return Some((i, commas));
        }
        if !matches!(toks[i].kind, K::LBracket) {
            return None;
        }
        stack.pop();
    }
    None
}

/// What the `(` at token `open` calls.
enum Callee<'t> {
    /// `name(`
    Fn(&'t str),
    /// `recv.name(`, with the byte offset of the `.`.
    Method(&'t str, usize),
    /// `Type::name(`
    Assoc(&'t str, &'t str),
}

fn callee(toks: &[rokc_lexer::Token], open: usize) -> Option<Callee<'_>> {
    let kind = |i: usize| toks.get(i).map(|t| &t.kind);
    let name_at = open.checked_sub(1)?;
    let K::Ident(name) = kind(name_at)? else {
        return None;
    };
    let before = name_at.checked_sub(1);
    match before.and_then(kind) {
        Some(K::Fn) => None,
        Some(K::Dot) => Some(Callee::Method(name, toks[name_at - 1].span.start as usize)),
        Some(K::ColonColon) => match name_at.checked_sub(2).and_then(kind) {
            Some(K::Ident(ty)) => Some(Callee::Assoc(ty, name)),
            _ => Some(Callee::Fn(name)),
        },
        _ => Some(Callee::Fn(name)),
    }
}

/// Signature help at `off`, or `None` outside a call.
pub(super) fn signature_help(
    text: &str,
    key: &str,
    off: usize,
    compile: impl Fn(&str) -> Analysis,
    good: Option<&Analysis>,
) -> Option<Json> {
    let (toks, _) = rokc_lexer::Lexer::new(&text[..off], key.to_owned()).lex_all();
    let (open, commas) = open_call(&toks)?;
    let line_end = text[off..].find('\n').map_or(text.len(), |i| off + i);
    // Without a good compile: the buffer without the line being typed.
    let line_start = text[..off].rfind('\n').map_or(0, |i| i + 1);
    let blank = format!("{}{}", &text[..line_start], &text[line_end..]);
    let mut fresh = None;
    let ((sig, doc), method) = match callee(&toks, open)? {
        Callee::Method(name, dot) => {
            let found = receiver(text, key, dot, line_end, &compile, good, |a, ty| {
                method_sig(a, ty, name)
            });
            (found.flatten()?, true)
        }
        Callee::Assoc(ty, name) => {
            let a = checked(good, &mut fresh, &compile, &blank)?;
            let index = Members::of(a);
            let m = index
                .get(ty)
                .find(|m| m.name == name && m.kind != Kind::Field)
                .map(|m| (m.sig.clone(), m.doc.clone()));
            (m.or_else(|| fn_sig(a, key, name))?, false)
        }
        Callee::Fn(name) => (
            fn_sig(checked(good, &mut fresh, &compile, &blank)?, key, name)?,
            false,
        ),
    };
    let params = param_ranges(&sig);
    let skip_self = method
        && params
            .first()
            .is_some_and(|&(s, e)| is_self(&utf16_slice(&sig, s, e)));
    let mut active = commas as usize + usize::from(skip_self);
    // A variadic last parameter takes every argument after it.
    if let Some(&(s, e)) = params.last() {
        if active >= params.len() && utf16_slice(&sig, s, e).contains("...") {
            active = params.len() - 1;
        }
    }
    let params = params
        .iter()
        .map(|&(s, e)| {
            Json::obj([(
                "label",
                Json::Arr(vec![Json::num(s as u32), Json::num(e as u32)]),
            )])
        })
        .collect();
    let info = Json::obj([
        ("label", Json::str(&sig)),
        ("parameters", Json::Arr(params)),
        (
            "documentation",
            Json::obj([("kind", Json::str("markdown")), ("value", Json::str(doc))]),
        ),
    ]);
    Some(Json::obj([
        ("signatures", Json::Arr(vec![info])),
        ("activeSignature", Json::num(0)),
        ("activeParameter", Json::num(active as u32)),
    ]))
}

/// Method `name` of type `ty`: from the member index, else the HIR.
fn method_sig(a: &Analysis, ty: &rokc_hir::HirTy, name: &str) -> Option<(String, String)> {
    let hir = a.hir.as_ref()?;
    let index = Members::of(a);
    if let Some(m) = index
        .get(&ty_name(hir, ty))
        .find(|m| m.name == name && m.kind == Kind::Method)
    {
        return Some((m.sig.clone(), m.doc.clone()));
    }
    hir.functions
        .iter()
        .find(|f| {
            short_name(&f.name) == name
                && f.params
                    .first()
                    .is_some_and(|p| p.name == "self" && p.ty == *ty)
        })
        .map(|f| (signature(hir, f), a.fn_doc(f.span)))
}

/// Function `name` of the HIR: the buffer's first, a generic's
/// declaration before its instances.
fn fn_sig(a: &Analysis, key: &str, name: &str) -> Option<(String, String)> {
    let hir = a.hir.as_ref()?;
    let hidden = a.hidden_spans();
    hir.functions
        .iter()
        .filter(|f| short_name(&f.name) == name && !hidden.contains(&f.span))
        .filter(|f| f.params.first().is_none_or(|p| p.name != "self"))
        .min_by_key(|f| {
            (
                a.file(f.span).is_none_or(|(p, _)| p != key),
                f.name.contains("__mono__"),
            )
        })
        .map(|f| (signature(hir, f), a.fn_doc(f.span)))
}

fn is_self(param: &str) -> bool {
    let p = param.trim_start_matches("ref ").trim_start_matches("mut ");
    p == "self" || p.starts_with("self:")
}

/// `sig[s..e]` for UTF-16 offsets `s` and `e`.
fn utf16_slice(sig: &str, s: usize, e: usize) -> String {
    let units: Vec<u16> = sig.encode_utf16().collect();
    String::from_utf16_lossy(&units[s.min(units.len())..e.min(units.len())])
}

/// The parameters of `fn name<..>(a: T, b: U) -> R` as UTF-16 offsets
/// into it.
fn param_ranges(sig: &str) -> Vec<(usize, usize)> {
    let u16_at = |b: usize| sig[..b].encode_utf16().count();
    let Some(fn_at) = sig.find("fn ") else {
        return Vec::new();
    };
    let mut angle = 0u32;
    let mut open = None;
    for (i, c) in sig[fn_at..].char_indices() {
        match c {
            '<' => angle += 1,
            '>' if !sig[..fn_at + i].ends_with('-') => angle = angle.saturating_sub(1),
            '(' if angle == 0 => {
                open = Some(fn_at + i);
                break;
            }
            _ => {}
        }
    }
    let Some(open) = open else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut start = open + 1;
    let mut push = |s: usize, e: usize| {
        let part = &sig[s..e];
        let lead = part.len() - part.trim_start().len();
        let trail = part.len() - part.trim_end().len();
        if part.trim().is_empty() {
            return;
        }
        out.push((u16_at(s + lead), u16_at(e - trail)));
    };
    for (i, c) in sig[open..].char_indices().skip(1) {
        let at = open + i;
        match c {
            // `->` of a function type.
            '>' if sig[..at].ends_with('-') => {}
            '(' | '[' | '<' | '{' => depth += 1,
            ')' if depth == 0 => {
                push(start, at);
                break;
            }
            ')' | ']' | '>' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                push(start, at);
                start = at + 1;
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_of_a_signature() {
        let sig = "pub fn f<T: Eq>(self, x: Map<i32, fn() -> T>, ys: ...str) -> T";
        let got: Vec<String> = param_ranges(sig)
            .into_iter()
            .map(|(s, e)| utf16_slice(sig, s, e))
            .collect();
        assert_eq!(got, ["self", "x: Map<i32, fn() -> T>", "ys: ...str"]);
        assert!(param_ranges("fn g() -> i32").is_empty());
    }
}
