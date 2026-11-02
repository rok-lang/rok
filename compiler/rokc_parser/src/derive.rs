//! `#[derive(...)]`.
//!
//! Each trait becomes an `impl` written as source and parsed, every token
//! carrying the attribute's span so diagnostics point there. Field bounds
//! are checked in HIR (`check_derives`).

// Impl source is built by `format!`; generators take generator closures.
#![allow(clippy::format_collect, clippy::type_complexity)]

use rokc_ast::{
    AttrArg, AttrBody, EnumDef, EnumVariantKind, GenericParam, Generics, Item, StructDef,
    StructKind, Ty, TyKind,
};
use rokc_diagnostic::{Code, RokDiagnostic, Span};

use crate::parser::Parser;

/// Traits `#[derive]` can implement, with the path the generated impl
/// names.
const DERIVABLE: &[(&str, &str)] = &[
    ("Clone", "rok::core::marker::Clone"),
    ("Default", "rok::core::marker::Default"),
    ("Copy", "rok::core::marker::Copy"),
    ("Debug", "rok::core::fmt::Debug"),
    ("PartialEq", "rok::core::ops::PartialEq"),
    ("Eq", "rok::core::cmp::Eq"),
    ("PartialOrd", "rok::core::ops::PartialOrd"),
    ("Ord", "rok::core::cmp::Ord"),
    ("Hash", "rok::core::cmp::Hash"),
    ("Error", "rok::core::error::Error"),
];

const DISPLAY: &str = "rok::core::fmt::Display";

const ORDERING: &str = "rok::core::cmp::Ordering";

/// The trait names of an item's `#[derive(..)]` attributes, with the
/// attribute's span.
#[must_use]
pub fn derive_names(attrs: &[rokc_ast::Attribute]) -> Vec<(String, Span)> {
    let mut out = Vec::new();
    for a in attrs.iter().filter(|a| a.path == ["derive"]) {
        if let AttrBody::Args(args) = &a.body {
            for arg in args {
                if let AttrArg::Word(w) = arg {
                    out.push((w.clone(), a.span));
                }
            }
        }
    }
    out
}

/// Like `derive_names`, with the paths too (`#[derive(dep::Name)]`).
#[must_use]
pub fn derive_paths(attrs: &[rokc_ast::Attribute]) -> Vec<(Vec<String>, Span)> {
    let mut out = Vec::new();
    for a in attrs.iter().filter(|a| a.path == ["derive"]) {
        if let AttrBody::Args(args) = &a.body {
            for arg in args {
                match arg {
                    AttrArg::Word(w) => out.push((vec![w.clone()], a.span)),
                    AttrArg::Nested { path, body } if matches!(**body, AttrBody::Empty) => {
                        out.push((path.clone(), a.span));
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// The full path of a derivable trait.
#[must_use]
pub fn derivable(name: &str) -> Option<&'static str> {
    DERIVABLE.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

/// The derived impls of `item` (a struct or enum).
pub(crate) fn derive_items(item: &Item, file: &str) -> (Vec<Item>, Vec<RokDiagnostic>) {
    let (attrs, name, generics) = match item {
        Item::Struct(s) => (&s.attrs, &s.name, &s.generics),
        Item::Enum(e) => (&e.attrs, &e.name, &e.generics),
        _ => return (Vec::new(), Vec::new()),
    };
    let mut src = String::new();
    let mut diags = Vec::new();
    let mut span = None;
    let derived = derive_names(attrs);
    let has = |n: &str| derived.iter().any(|(d, _)| d == n);
    for (tr, at) in derived.iter().cloned() {
        span = Some(at);
        // `Eq: PartialEq`, `Ord: PartialOrd + Eq`.
        let needs: &[&str] = match tr.as_str() {
            "Eq" => &["PartialEq"],
            "Ord" => &["PartialOrd", "Eq"],
            "Copy" => &["Clone"],
            _ => &[],
        };
        if let Some(missing) = needs.iter().find(|n| !has(n)) {
            diags.push(RokDiagnostic::error(
                Code::CANNOT_DERIVE,
                at,
                format!(
                    "`#[derive({tr})]` on `{name}` needs `{missing}` too - derive both \
                         (`#[derive({missing}, {tr})]`)"
                ),
            ));
            continue;
        }
        let Some(path) = derivable(&tr) else {
            let why = if tr == "Display" {
                "its text is the type's own choice: write it with \
                 `#[display(\"..\")]` or an `impl Display`"
                    .to_owned()
            } else {
                format!(
                    "`{tr}` is not a derivable trait (derivable: {})",
                    DERIVABLE
                        .iter()
                        .map(|(n, _)| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            diags.push(RokDiagnostic::error(
                Code::CANNOT_DERIVE,
                at,
                format!("cannot derive `{tr}` for `{name}`: {why}"),
            ));
            continue;
        };
        let body = match item {
            _ if tr == "Error" => Some(String::new()),
            Item::Struct(s) => Some(struct_body(&tr, s)),
            Item::Enum(e) => enum_body(&tr, e),
            _ => None,
        };
        let Some(body) = body else {
            diags.push(RokDiagnostic::error(
                Code::CANNOT_DERIVE,
                at,
                format!(
                    "cannot derive `Default` for enum `{name}`: its first variant must \
                         be a unit variant (the default)"
                ),
            ));
            continue;
        };
        src.push_str(&format!(
            "impl{} {path} for {name}{} {{\n{body}}}\n",
            impl_params(generics, path),
            self_args(generics)
        ));
    }
    if let Some((at, body)) = display_body(item, &mut diags) {
        span = Some(at);
        src.push_str(&format!(
            "impl{} {DISPLAY} for {name}{} {{\n{body}}}\n",
            impl_params(generics, DISPLAY),
            self_args(generics)
        ));
    }
    let Some(span) = span.filter(|_| !src.is_empty()) else {
        return (Vec::new(), diags);
    };
    let (mut tokens, lex_diags) = rokc_lexer::Lexer::new(&src, file).lex_all();
    diags.extend(lex_diags);
    for t in &mut tokens {
        t.span = span;
    }
    compiler_written(&mut tokens);
    let mut p = Parser::new(&tokens, file);
    let mut items = Vec::new();
    while !p.at_eof() {
        let from = p.position();
        match p.parse_item_pub() {
            Some(i) => items.push(i),
            None if p.position() == from => break,
            None => {}
        }
    }
    diags.extend(p.diagnostics);
    (items, diags)
}

/// Items the derives name, beside the traits; written by the compiler,
/// so seen from core, while the item's own names stay the
/// user's.
const COMPILER_PATHS: &[&str] = &[
    ORDERING,
    DISPLAY,
    "rok::core::fmt::Formatter",
    "rok::core::fmt::FmtError",
    "rok::core::marker::default_of",
];

/// Stamp the compiler's paths in derive output with their core module.
fn compiler_written(tokens: &mut [rokc_lexer::Token]) {
    use rokc_lexer::TokenKind as K;
    let mut k = 0;
    while k < tokens.len() {
        let starts = matches!(&tokens[k].kind, K::Ident(n) if n == "rok")
            && (k == 0 || tokens[k - 1].kind != K::ColonColon);
        if !starts {
            k += 1;
            continue;
        }
        let mut end = k + 1;
        let mut path = "rok".to_owned();
        while end + 1 < tokens.len() && tokens[end].kind == K::ColonColon {
            let K::Ident(n) = &tokens[end + 1].kind else {
                break;
            };
            path.push_str("::");
            path.push_str(n);
            end += 2;
        }
        let known = DERIVABLE
            .iter()
            .map(|(_, p)| *p)
            .chain(COMPILER_PATHS.iter().copied())
            .any(|p| path == p || path.starts_with(&format!("{p}::")));
        if known {
            let module = path.split("::").take(3).collect::<Vec<_>>().join("::");
            let origin = std::sync::Arc::new(rokc_lexer::Origin {
                package: "rok::core".to_owned(),
                module,
                imports: Vec::new(),
            });
            for t in &mut tokens[k..end] {
                t.origin = Some(std::sync::Arc::clone(&origin));
            }
        }
        k = end;
    }
}

/// The text of a `#[display("..")]` attribute, with its span.
fn display_text(attrs: &[rokc_ast::Attribute]) -> Option<(String, Span)> {
    let a = attrs.iter().find(|a| a.path == ["display"])?;
    let text = match &a.body {
        AttrBody::Args(xs) => match xs.as_slice() {
            [AttrArg::Lit(rokc_ast::Literal::Str(t) | rokc_ast::Literal::RawStr(t))] => t.clone(),
            _ => return None,
        },
        _ => return None,
    };
    Some((text, a.span))
}

/// The text as an f-string literal.
fn fstring_of(text: &str) -> String {
    let mut out = String::from("f\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `fn fmt` of the `Display` impl that `#[display("..")]` asks for: on a
/// struct, or on an enum (every variant) or each of its variants. A
/// variant's text sees its named fields by name and tuple fields as `_0`,
/// `_1`, ..; every text sees `self`.
fn display_body(item: &Item, diags: &mut Vec<RokDiagnostic>) -> Option<(Span, String)> {
    let write = |text: &str| format!("try f.write_str({})", fstring_of(text));
    let body = |inner: String| {
        format!(
            "    fn fmt(self, ref f: rok::core::fmt::Formatter) throws (rok::core::fmt::FmtError) {{\n{inner}    }}\n"
        )
    };
    match item {
        Item::Struct(s) => {
            let (text, at) = display_text(&s.attrs)?;
            Some((at, body(format!("        {};\n", write(&text)))))
        }
        Item::Enum(e) => {
            if let Some((text, at)) = display_text(&e.attrs) {
                return Some((at, body(format!("        {};\n", write(&text)))));
            }
            let at = e.variants.iter().find_map(|v| display_text(&v.attrs))?.1;
            let mut arms = String::new();
            for v in &e.variants {
                let Some((text, _)) = display_text(&v.attrs) else {
                    diags.push(RokDiagnostic::error(
                        Code::CANNOT_DERIVE,
                        v.span,
                        format!(
                            "variant `{}::{}` has no `#[display(\"..\")]`; with one on any \
                             variant, every variant needs its own (or put one on the enum)",
                            e.name, v.name
                        ),
                    ));
                    return None;
                };
                let path = format!("{}::{}", e.name, v.name);
                let pat = match &v.kind {
                    EnumVariantKind::Unit | EnumVariantKind::Value(_) => path,
                    EnumVariantKind::Tuple(tys) => {
                        let names: Vec<String> = (0..tys.len()).map(|i| format!("_{i}")).collect();
                        format!("{path}({})", names.join(", "))
                    }
                    EnumVariantKind::Named(fs) => {
                        let names: Vec<&str> = fs.iter().map(|f| f.name.as_str()).collect();
                        format!("{path} {{ {} }}", names.join(", "))
                    }
                };
                arms.push_str(&format!("            {pat} => {{ {}; }}\n", write(&text)));
            }
            Some((
                at,
                body(format!("        match self {{\n{arms}        }}\n")),
            ))
        }
        _ => None,
    }
}

/// `<T: Tr, const N: usize>` - every type parameter bound by the trait.
fn impl_params(g: &Generics, path: &str) -> String {
    if g.params.is_empty() {
        return String::new();
    }
    let ps: Vec<String> = g
        .params
        .iter()
        .map(|gp| match gp {
            GenericParam::Type { name, .. } => format!("{name}: {path}"),
            GenericParam::Const { name, ty } => format!("const {name}: {}", ty_text(ty)),
        })
        .collect();
    format!("<{}>", ps.join(", "))
}

/// `<T, N>` - the item's own parameters as its arguments.
fn self_args(g: &Generics) -> String {
    if g.params.is_empty() {
        return String::new();
    }
    let ps: Vec<&str> = g
        .params
        .iter()
        .map(|gp| match gp {
            GenericParam::Type { name, .. } | GenericParam::Const { name, .. } => name.as_str(),
        })
        .collect();
    format!("<{}>", ps.join(", "))
}

/// A const parameter's type (a primitive name in practice).
pub(crate) fn ty_text(t: &Ty) -> String {
    match &t.kind {
        TyKind::Path(p) => p
            .segments
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::"),
        _ => "usize".to_owned(),
    }
}

/// `fn hash` writing `body` to the hasher `h` (`__H`: no clash with the
/// type's own parameters).
fn hash_fn(body: &str) -> String {
    format!("    fn hash<__H: rok::core::hash::Hasher>(self, ref h: __H) {{\n{body}    }}\n")
}

/// Lexicographic comparison over field pairs, ending in `last`.
fn lex_cmp(pairs: &[(String, String)], method: &str, last: &str) -> String {
    let rev = match method {
        "lt" | "le" => "gt",
        _ => "lt",
    };
    let strict = if method.starts_with('l') { "lt" } else { "gt" };
    let mut s = String::new();
    for (a, b) in pairs {
        s.push_str(&format!(
            "        if {a}.{strict}({b}) {{ return true; }}\n        if {a}.{rev}({b}) {{ return false; }}\n"
        ));
    }
    s.push_str(&format!("        return {last};\n"));
    s
}

fn ord_cmp(pairs: &[(String, String)]) -> String {
    let mut s = String::new();
    for (i, (a, b)) in pairs.iter().enumerate() {
        s.push_str(&format!(
            "        let c{i} = {a}.cmp({b});\n        if !c{i}.is_eq() {{ return c{i}; }}\n"
        ));
    }
    s.push_str(&format!("        return {ORDERING}::Equal;\n"));
    s
}

/// `Name { a: va, .. }`, `Name(v0, ..)` or `Name` from per-field values.
fn struct_value(s: &StructDef, fields: &[String], value: &dyn Fn(&str) -> String) -> String {
    let name = &s.name;
    match &s.kind {
        StructKind::Named(_) => {
            let inits: Vec<String> = fields
                .iter()
                .map(|f| format!("{f}: {}", value(f)))
                .collect();
            format!("{name} {{ {} }}", inits.join(", "))
        }
        StructKind::Tuple(_) => {
            let vs: Vec<String> = fields.iter().map(|f| value(f)).collect();
            format!("{name}({})", vs.join(", "))
        }
        StructKind::Unit => name.clone(),
    }
}

fn struct_body(tr: &str, s: &StructDef) -> String {
    // A tuple struct's fields are `0`, `1`, ... (`self.0`).
    let fields: Vec<String> = match &s.kind {
        StructKind::Named(fs) => fs.iter().map(|f| f.name.clone()).collect(),
        StructKind::Tuple(fs) => (0..fs.len()).map(|i| i.to_string()).collect(),
        StructKind::Unit => Vec::new(),
    };
    let name = &s.name;
    let pairs: Vec<(String, String)> = fields
        .iter()
        .map(|f| (format!("self.{f}"), format!("rhs.{f}")))
        .collect();
    match tr {
        "Clone" => format!(
            "    fn clone(self) -> Self {{ return {}; }}\n",
            struct_value(s, &fields, &|f| format!("self.{f}.clone()"))
        ),
        "Default" => format!(
            "    fn default() -> Self {{ return {}; }}\n",
            struct_value(s, &fields, &|_| "rok::core::marker::default_of()"
                .to_owned())
        ),
        "Debug" => {
            let named = matches!(s.kind, StructKind::Named(_));
            let shown: Vec<(Option<&str>, String)> = fields
                .iter()
                .map(|f| (named.then_some(f.as_str()), format!("self.{f}")))
                .collect();
            debug_fn(&debug_writes(name, named, &shown, "        "))
        }
        "PartialEq" => {
            let conj: Vec<String> = pairs.iter().map(|(a, b)| format!("{a}.eq({b})")).collect();
            let all = if conj.is_empty() {
                "true".to_owned()
            } else {
                conj.join(" && ")
            };
            format!(
                "    fn eq(self, rhs: Self) -> bool {{ return {all}; }}\n    fn ne(self, rhs: Self) -> bool {{ return !self.eq(rhs); }}\n"
            )
        }
        "PartialOrd" => ["lt", "gt", "le", "ge"]
            .iter()
            .map(|m| {
                let last = if m.len() == 2 && m.ends_with('e') {
                    "true"
                } else {
                    "false"
                };
                format!(
                    "    fn {m}(self, rhs: Self) -> bool {{\n{}    }}\n",
                    lex_cmp(&pairs, m, last)
                )
            })
            .collect(),
        "Ord" => format!(
            "    fn cmp(self, rhs: Self) -> {ORDERING} {{\n{}    }}\n",
            ord_cmp(&pairs)
        ),
        // The fields in declaration order.
        "Hash" => hash_fn(
            &fields
                .iter()
                .map(|f| format!("        self.{f}.hash(ref h);\n"))
                .collect::<String>(),
        ),
        // `Copy` / `Eq`: markers.
        _ => String::new(),
    }
}

/// A variant's pattern binding its fields to `{prefix}0..` and the
/// bound names.
fn variant_pat(e: &EnumDef, v: &rokc_ast::EnumVariant, prefix: &str) -> (String, Vec<String>) {
    let path = format!("{}::{}", e.name, v.name);
    match &v.kind {
        EnumVariantKind::Unit | EnumVariantKind::Value(_) => (path, Vec::new()),
        EnumVariantKind::Tuple(tys) => {
            let names: Vec<String> = (0..tys.len()).map(|i| format!("{prefix}{i}")).collect();
            (format!("{path}({})", names.join(", ")), names)
        }
        EnumVariantKind::Named(fs) => {
            let names: Vec<String> = (0..fs.len()).map(|i| format!("{prefix}{i}")).collect();
            let binds: Vec<String> = fs
                .iter()
                .zip(&names)
                .map(|(f, n)| format!("{}: {n}", f.name))
                .collect();
            (format!("{path} {{ {} }}", binds.join(", ")), names)
        }
    }
}

/// `E::V`, `E::V(_, _)` or `E::V { .. }`.
fn variant_wild(e: &EnumDef, v: &rokc_ast::EnumVariant) -> String {
    let path = format!("{}::{}", e.name, v.name);
    match &v.kind {
        EnumVariantKind::Unit | EnumVariantKind::Value(_) => path,
        EnumVariantKind::Tuple(tys) => format!("{path}({})", vec!["_"; tys.len()].join(", ")),
        EnumVariantKind::Named(_) => format!("{path} {{ .. }}"),
    }
}

fn enum_body(tr: &str, e: &EnumDef) -> Option<String> {
    let arms = |f: &dyn Fn(&rokc_ast::EnumVariant, &str, &[String]) -> String| -> String {
        e.variants
            .iter()
            .map(|v| {
                let (pat, names) = variant_pat(e, v, "a");
                format!("            {pat} => {},\n", f(v, &pat, &names))
            })
            .collect()
    };
    // `let ta = match self { .. }` - the variant's index.
    let tag = |x: &str, var: &str| -> String {
        let a: String = e
            .variants
            .iter()
            .enumerate()
            .map(|(i, v)| format!("            {} => {i},\n", variant_wild(e, v)))
            .collect();
        format!("        let {var}: i32 = match {x} {{\n{a}        }};\n")
    };
    // Same-variant pairs: `(E::V(a0, ..), E::V(b0, ..)) => { body }`.
    let pair_arms = |body: &dyn Fn(&[(String, String)]) -> String, other: &str| -> String {
        let mut s = String::new();
        for v in &e.variants {
            let (pa, na) = variant_pat(e, v, "a");
            let (pb, nb) = variant_pat(e, v, "b");
            let pairs: Vec<(String, String)> = na.into_iter().zip(nb).collect();
            s.push_str(&format!(
                "            ({pa}, {pb}) => {{\n{}            }},\n",
                body(&pairs)
            ));
        }
        s.push_str(&format!("            _ => {{ {other} }},\n"));
        s
    };
    Some(match tr {
        // The first variant, which must carry no payload.
        "Default" => match e.variants.first().map(|v| &v.kind) {
            Some(EnumVariantKind::Unit | EnumVariantKind::Value(_)) => format!(
                "    fn default() -> Self {{ return {}::{}; }}\n",
                e.name, e.variants[0].name
            ),
            _ => return None,
        },
        "Clone" => format!(
            "    fn clone(self) -> Self {{\n        return match self {{\n{}        }};\n    }}\n",
            arms(&|v, pat, names| {
                let cloned: Vec<String> = names.iter().map(|n| format!("{n}.clone()")).collect();
                match &v.kind {
                    EnumVariantKind::Unit | EnumVariantKind::Value(_) => pat.to_owned(),
                    EnumVariantKind::Tuple(_) => {
                        format!("{}::{}({})", e.name, v.name, cloned.join(", "))
                    }
                    EnumVariantKind::Named(fs) => {
                        let inits: Vec<String> = fs
                            .iter()
                            .zip(&cloned)
                            .map(|(f, c)| format!("{}: {c}", f.name))
                            .collect();
                        format!("{}::{} {{ {} }}", e.name, v.name, inits.join(", "))
                    }
                }
            })
        ),
        "Debug" => debug_fn(&format!(
            "        match self {{\n{}        }}\n",
            arms(&|v, _, names| {
                let shown: Vec<(Option<&str>, String)> = match &v.kind {
                    EnumVariantKind::Named(fs) => fs
                        .iter()
                        .zip(names)
                        .map(|(f, n)| (Some(f.name.as_str()), n.clone()))
                        .collect(),
                    _ => names.iter().map(|n| (None, n.clone())).collect(),
                };
                let named = matches!(v.kind, EnumVariantKind::Named(_));
                format!(
                    "{{\n{}            }}",
                    debug_writes(&v.name, named, &shown, "                ")
                )
            })
        )),
        "PartialEq" => format!(
            "    fn eq(self, rhs: Self) -> bool {{\n        match (self, rhs) {{\n{}        }}\n    }}\n    fn ne(self, rhs: Self) -> bool {{ return !self.eq(rhs); }}\n",
            pair_arms(
                &|pairs| {
                    let conj: Vec<String> =
                        pairs.iter().map(|(a, b)| format!("{a}.eq({b})")).collect();
                    let all = if conj.is_empty() {
                        "true".to_owned()
                    } else {
                        conj.join(" && ")
                    };
                    format!("                return {all};\n")
                },
                "return false;"
            )
        ),
        "PartialOrd" => ["lt", "gt", "le", "ge"]
            .iter()
            .map(|m| {
                let last = if m.ends_with('e') { "true" } else { "false" };
                let tag_test = if m.starts_with('l') { "<" } else { ">" };
                format!(
                    "    fn {m}(self, rhs: Self) -> bool {{\n{}{}        if ta != tb {{ return ta {tag_test} tb; }}\n        match (self, rhs) {{\n{}        }}\n    }}\n",
                    tag("self", "ta"),
                    tag("rhs", "tb"),
                    pair_arms(&|pairs| lex_cmp(pairs, m, last), &format!("return {last};"))
                )
            })
            .collect(),
        "Ord" => format!(
            "    fn cmp(self, rhs: Self) -> {ORDERING} {{\n{}{}        if ta != tb {{ return ta.cmp(tb); }}\n        match (self, rhs) {{\n{}        }}\n    }}\n",
            tag("self", "ta"),
            tag("rhs", "tb"),
            pair_arms(&ord_cmp, &format!("return {ORDERING}::Equal;"))
        ),
        // The variant's index (none for a single variant), then its
        // fields.
        "Hash" => {
            let one = e.variants.len() == 1;
            let arms: String = e
                .variants
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let (pat, names) = variant_pat(e, v, "a");
                    let tag = if one {
                        String::new()
                    } else {
                        format!("                h.write_usize({i});\n")
                    };
                    let steps: String = names
                        .iter()
                        .map(|n| format!("                {n}.hash(ref h);\n"))
                        .collect();
                    format!("            {pat} => {{\n{tag}{steps}            }},\n")
                })
                .collect();
            hash_fn(&format!("        match self {{\n{arms}        }}\n"))
        }
        _ => String::new(),
    })
}

/// `Debug::fmt` with `body`.
fn debug_fn(body: &str) -> String {
    format!(
        "    fn fmt(self, ref f: rok::core::fmt::Formatter) throws (rok::core::fmt::FmtError) {{\n{body}    }}\n"
    )
}

/// The writes showing `name` with `fields` (label, value): `Name`,
/// `Name(a, b)` or `Name { x: a, y: b }` (`{:#?}`: a field a line).
fn debug_writes(name: &str, named: bool, fields: &[(Option<&str>, String)], ind: &str) -> String {
    if fields.is_empty() {
        return format!("{ind}try f.write_str(\"{name}\");\n");
    }
    let (open, close) = if named { (" {", "}") } else { ("(", ")") };
    let mut out = format!("{ind}try f.write_str(\"{name}{open}\");\n");
    for (i, (label, value)) in fields.iter().enumerate() {
        let label = label.map_or_else(String::new, |l| format!("{l}: "));
        out.push_str(&format!(
            "{ind}try rok::core::fmt::Formatter::debug_entry(ref f, \"{label}\", {value}, {}, {named});\n",
            i == 0
        ));
    }
    out.push_str(&format!(
        "{ind}try rok::core::fmt::Formatter::debug_close(ref f, \"{close}\", false, {named});\n"
    ));
    out
}
