//! `rokc lint` (spec 901, 6.2.9): style and common-mistake checks over a
//! type-checked package (no codegen).
//!
//! Most lints walk the parsed AST; `redundant_cast` / `redundant_clone`
//! read HIR types. Lowering reports uses of marked functions, fields,
//! consts and statics (`deprecated`, `deprecated_safe`) as warnings that
//! carry the lint; deprecated types are matched by path here. Levels: each lint's default, then `-A/-W/-D name`
//! (later wins; `rok` prepends `rok.rokon` `lints`), then
//! `#[allow/warn/deny]` on an enclosing item, `mod` or `#![..]`
//! (innermost wins).
//!
//! Items only have start spans, so an attribute's scope runs to the next
//! sibling's start. Name-based lints match the last path segment, so a
//! same-named item elsewhere can hide a finding (never invent one).
//! Capitalized names aren't tracked as locals (a pattern `Foo` may be a
//! constant).

use std::collections::{HashMap, HashSet};

use rokc_ast::{
    AccessTail, AttrArg, AttrBody, Attribute, Block, CallArg, CaptureSpec, Expr, ExprKind,
    ExternItem, FnDef, ImplItem, Item, Literal, Pattern, PatternKind, Stmt, StmtKind, TraitItem,
    TryKind, Ty, TyKind, UseTree, UseTreeKind,
};
use rokc_core::Visibility;
use rokc_diagnostic::{ErrorKind, RokDiagnostic, Severity, Span};
use rokc_hir::walk::{AstVisitor, walk_fn, walk_item, walk_stmt};
use rokc_hir::{HirExprKind, HirTy};
use rokc_interface::{CompileResult, SourceFile};

use crate::analysis::each_block;

pub use rokc_core::lint::Level;

mod release;

pub struct Lint {
    pub name: &'static str,
    pub default: Level,
    pub desc: &'static str,
    /// The release that added it (`lint --list` shows it).
    pub since: &'static str,
}

const fn warn(name: &'static str, desc: &'static str) -> Lint {
    Lint {
        name,
        default: Level::Warn,
        desc,
        since: "0.1.0",
    }
}

/// Every lint, with its default level.
pub const LINTS: &[Lint] = &[
    warn(
        "unused_variables",
        "a local or parameter that is never read",
    ),
    warn(
        "unused_imports",
        "a `use` whose name the module never mentions",
    ),
    warn("dead_code", "a non-`pub` function nothing calls"),
    warn(
        "unreachable_code",
        "statements after `return` / `break` / `continue` / `throw` / `panic`",
    ),
    warn(
        "unused_must_use",
        "the result of a `#[must_use]` function is discarded",
    ),
    warn(
        "ignored_error",
        "a `try? f();` statement silently drops the error",
    ),
    warn(
        "shadowed_loop_var",
        "a `let` in a `for` body re-binds the loop variable",
    ),
    Lint {
        name: "nan_comparison",
        default: Level::Deny,
        desc: "comparing with NaN is always false (`!=`: true)",
        since: "0.1.0",
    },
    warn("none_comparison", "`x == none` instead of `x is none`"),
    warn(
        "bool_comparison",
        "comparing a `bool` with `true` / `false`",
    ),
    Lint {
        name: "self_assignment",
        default: Level::Deny,
        desc: "`x = x;`",
        since: "0.1.0",
    },
    warn("while_true", "`while true` instead of `loop`"),
    warn("redundant_cast", "`x as T` where `x` already has type `T`"),
    warn("redundant_clone", "`.clone()` of a primitive value"),
    warn(
        "non_snake_case",
        "functions, locals, parameters and modules are snake_case",
    ),
    warn(
        "non_camel_case_types",
        "types, traits and variants are UpperCamelCase",
    ),
    warn(
        "non_upper_case_globals",
        "`const` / `static` names are UPPER_SNAKE_CASE",
    ),
    warn("confusable_idents", "identifiers that look alike"),
    warn(
        "deprecated",
        "use of a `#[deprecated]` function, method, field, type, constant or static",
    ),
    warn(
        "write_to_copy",
        "a field set on a copy (a by-value parameter or `self`, a `for` binding, a `let` of `xs[i]`) that nothing reads afterwards",
    ),
    warn(
        "unused_doc_comments",
        "a doc comment on what is not documented (a statement, an expression, a match arm)",
    ),
    warn(
        "missing_f_prefix",
        "a plain string holds `{name}` of a local in scope: it needs `f\"..\"`",
    ),
    Lint {
        name: "literal_spread_mismatch",
        default: Level::Warn,
        desc: "`f(..[a, b, c])` or `f(..{\"k\": v})` whose literal cannot fill the parameters: the call panics",
        since: "0.1.6",
    },
    Lint {
        name: "deprecated_safe",
        default: Level::Warn,
        desc: "a call of a `#[deprecated_safe]` function outside `unsafe`",
        since: "0.1.6",
    },
    warn_since(
        "trait_not_in_scope",
        "a method from a trait outside `rok::` that is neither defined nor `use`d in the module",
    ),
    warn_since(
        "guard_escape",
        "a `throw` inside `using` whose error holds what the lock protects",
    ),
    warn_since(
        "thread_local_across_await",
        "a `ThreadLocal::get()` value used after an `await`: the task may resume on another thread",
    ),
    warn_since(
        "unqualified_meta",
        "another package's macro, derive or attribute used by its bare name: write `dep::name!` or `use dep::name!;`",
    ),
    Lint {
        name: "assoc_type_shadowing",
        default: Level::Warn,
        desc: "an associated type named like an outer type, which it hides inside its trait or impl",
        since: "0.2.0",
    },
    Lint {
        name: "builtin_attribute_name",
        default: Level::Warn,
        desc: "a meta block that takes the name of a built-in attribute",
        since: "0.2.0",
    },
    Lint {
        name: "unused_result",
        default: Level::Warn,
        desc: "a statement drops a value other than `()` or `none`: use it, or write `_ = expr;`",
        since: "0.2.0",
    },
    Lint {
        name: "missing_docs",
        default: Level::Allow,
        desc: "a `pub` item, method or module without a `///` doc comment",
        since: "0.2.0",
    },
];

/// Names of lints that are gone (most became errors). Naming one in
/// `#[allow/warn/deny]`, `-A/-W/-D` or `rok.rokon` `lints` is accepted
/// and does nothing, so old level settings keep building.
pub const REMOVED_LINTS: &[&str] = &[
    "keyword_idents",
    "zero_prefixed_literal",
    "try_coalesce_precedence",
    "trailing_closure_operator",
    "trailing_closure_return",
    "capture_field_write",
    "const_item_mutation",
    "pattern_binds_const",
    "ambiguous_int_literal",
    "finally_jump",
    "float_to_bool",
    // Errors now: affine resources (stdlib 2.13.4).
    "unclosed_resource",
    "unclosed_sender",
    "using_twice",
    "extern_static_unsafe",
    "improper_ctypes",
    "implicit_core_fn",
    "discarded_ref_write",
    "fstring_g_precision",
];

/// A lint added in 0.1.6.
const fn warn_since(name: &'static str, desc: &'static str) -> Lint {
    Lint {
        name,
        default: Level::Warn,
        desc,
        since: "0.1.6",
    }
}

/// Lint groups usable wherever a lint name is (`all`: every lint;
/// `warnings`: every warning-level lint, later ones included).
const GROUPS: &[(&str, &[&str])] = &[
    (
        "unused",
        &[
            "unused_variables",
            "unused_imports",
            "dead_code",
            "unreachable_code",
            "unused_must_use",
            "unused_result",
            "unused_doc_comments",
        ],
    ),
    (
        "nonstandard_style",
        &[
            "non_snake_case",
            "non_camel_case_types",
            "non_upper_case_globals",
        ],
    ),
];

/// The lints `name` stands for (a lint, a group, `all`, `warnings`, a
/// removed lint: none), or `None` for an unknown name.
#[must_use]
pub fn expand(name: &str) -> Option<Vec<&'static str>> {
    expand_in(LINTS, REMOVED_LINTS, name)
}

/// A removed name stands for no lint: known, but sets nothing.
fn expand_in(lints: &'static [Lint], removed: &[&str], name: &str) -> Option<Vec<&'static str>> {
    if removed.contains(&name) {
        return Some(Vec::new());
    }
    match name {
        "all" => return Some(lints.iter().map(|l| l.name).collect()),
        "warnings" => {
            return Some(
                lints
                    .iter()
                    .filter(|l| l.default == Level::Warn)
                    .map(|l| l.name)
                    .collect(),
            );
        }
        _ => {}
    }
    if let Some(l) = lints.iter().find(|l| l.name == name) {
        return Some(vec![l.name]);
    }
    GROUPS
        .iter()
        .find(|(g, _)| *g == name)
        .map(|(_, ls)| ls.to_vec())
}

/// "unknown lint `name`" with a did-you-mean and where the list is.
fn unknown(name: &str, at: &str) -> String {
    let names = LINTS
        .iter()
        .map(|l| l.name)
        .chain(GROUPS.iter().map(|(g, _)| *g))
        .chain(["all", "warnings"]);
    format!(
        "unknown lint `{name}`{at}{} (`{} lint --list` shows every lint and group)",
        rokc_core::cli::did_you_mean(name, names, &[]),
        rokc_core::cli::invoked_as()
    )
}

/// Levels set outside the source: `-A/-W/-D` in order. The first
/// `manifest.1` of them are `rok.rokon` `lints`, from the manifest at
/// `manifest.0`.
#[derive(Clone, Debug, Default)]
pub struct Levels {
    pub flags: Vec<(Level, String)>,
    pub manifest: Option<(String, usize)>,
    /// `--check-library`: the library's own files are linted too, at
    /// the library's levels (`LIBRARY_LEVELS`) under the flags.
    pub library: bool,
}

impl From<Vec<(Level, String)>> for Levels {
    fn from(flags: Vec<(Level, String)>) -> Self {
        Self {
            flags,
            ..Self::default()
        }
    }
}

/// The library's lint levels: its manifest, were it a package.
const LIBRARY_LEVELS: &[(Level, &str)] = &[(Level::Warn, "missing_docs")];

/// `rokc lint --list` text.
#[must_use]
pub fn list() -> String {
    let mut s = String::from("lints (default level, release that added it):\n");
    for l in LINTS {
        s.push_str(&format!(
            "  {:<24} {:<5} {:<6} {}\n",
            l.name,
            l.default.name(),
            l.since,
            l.desc
        ));
    }
    s.push_str("groups: all (every lint), warnings (every warning-level lint)");
    for (g, ls) in GROUPS {
        s.push_str(&format!(", {g} ({})", ls.join(", ")));
    }
    s.push('\n');
    s
}

/// A lint hit before its level is known.
struct Finding {
    lint: &'static str,
    span: Span,
    msg: String,
    notes: Vec<String>,
    secondary: Option<(Span, String)>,
    /// A compiler warning that carries the lint: it keeps its labels,
    /// notes and fixes.
    base: Option<Box<RokDiagnostic>>,
}

/// An attribute scope: `[start, end)` of file `file`, with the levels its
/// `#[allow/warn/deny]` attributes set.
struct Region {
    file: u32,
    start: u32,
    end: u32,
    levels: Vec<(Level, &'static str, Span)>,
}

/// A macro or meta template renames its own locals with a hygiene suffix
/// (`x__mac` / `x__meta`); the user never wrote it, so strip it before a name
/// reaches a diagnostic.
fn demangle(name: &str) -> &str {
    for suf in ["__mac", "__meta"] {
        if let Some(base) = name.strip_suffix(suf) {
            if !base.is_empty() {
                return base;
            }
        }
    }
    name
}

/// A level set for a lint: the level and the note naming where (`None`:
/// the default).
type Set = (Level, Option<String>);

/// Lint a front-end result (`Session::set_keep_ast`).
///
/// `levels` are the manifest's and the command line's `-A/-W/-D`.
/// Returns compile warnings (those that carry a lint are leveled as that
/// lint), then findings as warnings or (deny) errors.
#[must_use]
pub fn run(result: &CompileResult, levels: &Levels) -> Vec<RokDiagnostic> {
    let mut out = Vec::new();
    let mut base = defaults(levels.library);
    for (i, (lvl, name)) in levels.flags.iter().enumerate() {
        let (from, at) = match &levels.manifest {
            Some((path, n)) if i < *n => (
                format!("`\"{name}\": \"{}\"` in `lints` of {path}", lvl.name()),
                format!(" in `lints` of {path}"),
            ),
            _ => (
                format!("`{} {name}` on the command line", lvl.flag()),
                " on the command line".to_owned(),
            ),
        };
        match expand(name) {
            Some(ls) => {
                for l in ls {
                    if let Some(sets) = base.get_mut(l) {
                        sets.push((*lvl, Some(from.clone())));
                    }
                }
            }
            None => out.push(RokDiagnostic::warning(
                ErrorKind::Lint,
                Span::DUMMY,
                unknown(name, &at),
            )),
        }
    }

    let texts = result.source_map.files();
    // A prebuilt library's build: its files are the dependency it is
    // mounted as.
    let own = result
        .modules
        .first()
        .and_then(|m| m.interface.as_ref())
        .map_or("", |i| i.name.as_str());
    let user: Vec<&SourceFile> = result.sources.iter().filter(|f| f.package == own).collect();
    let mut cx = Cx::new(&result.sources, result.library, texts);
    let mut regions = Vec::new();
    rokc_diagnostic::time("lints", || {
        for f in &user {
            regions.extend(file_regions(f, &user, &mut out));
            cx.file(f);
        }
        cx.dead_code(&user);
        // The library's own files with `--check-library`.
        let library = result
            .sources
            .iter()
            .filter(|f| levels.library && f.package.starts_with("rok::"));
        for items in user.iter().copied().chain(library).map(|f| &f.items) {
            missing_docs(items, &mut cx.findings);
        }
        if let Some(p) = result.library.filter(|_| levels.library) {
            missing_docs(&p.items, &mut cx.findings);
        }
    });
    if let Some(m) = result.modules.first() {
        rokc_diagnostic::time("lints hir", || cx.hir(&m.hir, &user));
    }
    for w in &result.warnings {
        let lint = w
            .lint
            .as_deref()
            .and_then(|n| LINTS.iter().find(|l| l.name == n));
        match lint {
            Some(l) => cx.findings.push(Finding {
                lint: l.name,
                span: w.primary.as_ref().map_or(Span::DUMMY, |p| p.span),
                msg: w.message.clone(),
                notes: Vec::new(),
                secondary: None,
                base: Some(Box::new(w.clone())),
            }),
            None => out.push(w.clone()),
        }
    }

    let mut findings = cx.findings;
    findings.sort_by_key(|f| (f.span.file, f.span.start, f.lint));
    findings.dedup_by(|a, b| a.lint == b.lint && a.span == b.span);
    // A dropped `#[must_use]` value or `try?` error has its own lint.
    let stronger: HashSet<(u32, u32)> = findings
        .iter()
        .filter(|f| matches!(f.lint, "unused_must_use" | "ignored_error"))
        .map(|f| key(f.span))
        .collect();
    findings.retain(|f| f.lint != "unused_result" || !stronger.contains(&key(f.span)));
    for f in findings {
        let generated = text_at(texts, f.span).is_some_and(|t| t.starts_with('#'));
        if generated {
            continue;
        }
        let (mut level, mut from) = base[f.lint]
            .last()
            .map(|s| (s.0, s.1.clone()))
            .expect("the default level is always set");
        let mut at = None;
        for r in &regions {
            if r.file == f.span.file && r.start <= f.span.start && f.span.start < r.end {
                for &(lvl, name, span) in &r.levels {
                    if name == f.lint {
                        level = lvl;
                        at = Some((span, lvl));
                        from = None;
                    }
                }
            }
        }
        if level == Level::Allow {
            continue;
        }
        let severity = if level == Level::Deny {
            Severity::Error
        } else {
            Severity::Warning
        };
        let mut d = match f.base {
            Some(b) => RokDiagnostic {
                severity,
                kind: ErrorKind::Lint,
                ..*b
            },
            None => RokDiagnostic {
                severity,
                tag: if f.lint == "deprecated" {
                    rokc_diagnostic::Tag::Deprecated
                } else {
                    rokc_diagnostic::Tag::None
                },
                ..RokDiagnostic::error(ErrorKind::Lint, f.span, f.msg).with_lint(f.lint)
            },
        };
        if let Some((span, msg)) = f.secondary {
            d = d.with_secondary(span, msg);
        }
        for n in f.notes {
            d = d.with_note(n);
        }
        d = match (at, from) {
            (Some((span, lvl)), _) => {
                d.with_secondary(span, format!("`#[{}({})]` set here", lvl.name(), f.lint))
            }
            (None, Some(from)) => d.with_note(from),
            (None, None) => d.with_note(format!("`#[{}({})]` on by default", level.name(), f.lint)),
        };
        out.push(d);
    }
    out
}

/// Each lint's default level; with `library` the library's levels on top.
fn defaults(library: bool) -> HashMap<&'static str, Vec<Set>> {
    let mut base: HashMap<&'static str, Vec<Set>> = LINTS
        .iter()
        .map(|l| (l.name, vec![(l.default, None)]))
        .collect();
    for &(lvl, name) in LIBRARY_LEVELS.iter().filter(|_| library) {
        if let Some(sets) = base.get_mut(name) {
            let from = format!("`{name}` is {} in the library", lvl.name());
            sets.push((lvl, Some(from)));
        }
    }
    base
}

/// The source text from `span` to the end of its line.
fn text_at(texts: &[(String, String)], span: Span) -> Option<&str> {
    let (_, text) = texts.get(usize::try_from(span.file).ok()?.checked_sub(1)?)?;
    let rest = text.get(usize::try_from(span.start).ok()?..)?;
    Some(rest.split('\n').next().unwrap_or(rest))
}

/// The name `name` a `let` at `at` binds (`at` covers `let` only).
fn name_span(texts: &[(String, String)], at: Span, name: &str) -> Option<Span> {
    let line = text_at(texts, at)?;
    let rest = line.strip_prefix("let")?;
    let off = 3 + rest.find(|c: char| !c.is_whitespace())?;
    let word = line.get(off..)?;
    let end = word
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(word.len());
    if &word[..end] != name {
        return None;
    }
    let skip = u32::try_from(line[..off].chars().count()).ok()?;
    let off = u32::try_from(off).ok()?;
    Some(Span {
        start: at.start + off,
        end: at.start + off + u32::try_from(name.len()).ok()?,
        col: at.col + skip,
        ..at
    })
}

/// `(level, lint, attribute span)` for each lint an `allow` / `warn` /
/// `deny` attribute names; unknown names are reported to `out`.
fn levels_of(
    attrs: &[Attribute],
    out: &mut Vec<RokDiagnostic>,
) -> Vec<(Level, &'static str, Span)> {
    let mut levels = Vec::new();
    for a in attrs {
        let Some(lvl) = Level::parse(a.name()) else {
            continue;
        };
        let AttrBody::Args(args) = &a.body else {
            continue;
        };
        for arg in args {
            let name = match arg {
                AttrArg::Word(w) => w.as_str(),
                AttrArg::Nested { path, .. } => path.last().map_or("", String::as_str),
                _ => "",
            };
            match expand(name) {
                Some(ls) => {
                    levels.extend(ls.into_iter().map(|l| (lvl, l, a.span)));
                }
                None => out.push(RokDiagnostic::warning(
                    ErrorKind::Lint,
                    a.span,
                    unknown(name, ""),
                )),
            }
        }
    }
    levels
}

/// Where an item's text starts: its first attribute or its first token.
fn item_start(attrs: &[Attribute], span: Span) -> u32 {
    attrs
        .iter()
        .map(|a| a.span.start)
        .chain([span.start])
        .min()
        .unwrap_or(span.start)
}

/// One file's attribute scopes, outermost first: the file (its `mod`
/// declarations up the path, then `#![..]`), then each item down the tree.
fn file_regions(f: &SourceFile, user: &[&SourceFile], out: &mut Vec<RokDiagnostic>) -> Vec<Region> {
    let mut attrs: Vec<Attribute> = Vec::new();
    // `#[allow(x)] mod m;` in the parent file covers all of `m.rok`.
    for depth in 1..=f.module.len() {
        let (parent, name) = (&f.module[..depth - 1], &f.module[depth - 1]);
        for p in user.iter().filter(|p| p.module == parent) {
            for item in &p.items {
                if let Item::Mod(m) = item {
                    if m.name == *name && m.items.is_none() {
                        attrs.extend(m.attrs.iter().cloned());
                    }
                }
            }
        }
    }
    attrs.extend(f.attrs.iter().cloned());
    let mut regions = vec![Region {
        file: f.file_id,
        start: 0,
        end: u32::MAX,
        levels: levels_of(&attrs, out),
    }];
    let spans: Vec<(u32, &[Attribute])> = f
        .items
        .iter()
        .map(|i| (item_start(i.attrs(), i.span()), i.attrs().as_slice()))
        .collect();
    item_regions(f.file_id, &f.items, &spans, u32::MAX, &mut regions, out);
    regions
}

fn item_regions(
    file: u32,
    items: &[Item],
    starts: &[(u32, &[Attribute])],
    end: u32,
    regions: &mut Vec<Region>,
    out: &mut Vec<RokDiagnostic>,
) {
    for (i, item) in items.iter().enumerate() {
        let (start, attrs) = starts[i];
        // Past the item's own keyword: what `#[derive]` generates carries
        // the attribute's span, before it.
        let next = starts
            .iter()
            .map(|(s, _)| *s)
            .filter(|s| *s > item.span().start)
            .min()
            .unwrap_or(end)
            .min(end);
        let levels = levels_of(attrs, out);
        if !levels.is_empty() {
            regions.push(Region {
                file,
                start,
                end: next,
                levels,
            });
        }
        let fns: Vec<&FnDef> = match item {
            Item::Impl(im) => im
                .items
                .iter()
                .filter_map(|ii| match ii {
                    ImplItem::Method(m) => Some(m),
                    _ => None,
                })
                .collect(),
            Item::Trait(t) => t
                .items
                .iter()
                .filter_map(|ti| match ti {
                    TraitItem::Method(m) => Some(m),
                    _ => None,
                })
                .collect(),
            Item::Mod(m) => {
                if let Some(inner) = &m.items {
                    let s: Vec<(u32, &[Attribute])> = inner
                        .iter()
                        .map(|i| (item_start(i.attrs(), i.span()), i.attrs().as_slice()))
                        .collect();
                    item_regions(file, inner, &s, next, regions, out);
                }
                Vec::new()
            }
            _ => Vec::new(),
        };
        let starts: Vec<u32> = fns.iter().map(|m| item_start(&m.attrs, m.span)).collect();
        for (k, m) in fns.iter().enumerate() {
            let levels = levels_of(&m.attrs, out);
            if levels.is_empty() {
                continue;
            }
            let mend = starts
                .iter()
                .copied()
                .filter(|s| *s > starts[k])
                .min()
                .unwrap_or(next)
                .min(next);
            regions.push(Region {
                file,
                start: starts[k],
                end: mend,
                levels,
            });
        }
    }
}

/// `#[name]` / `#[name = "msg"]` / `#[name("msg")]`: its message (empty
/// when it has none), or `None` without the attribute.
fn attr_msg(attrs: &[Attribute], name: &str) -> Option<String> {
    let a = attrs.iter().find(|a| a.name() == name)?;
    let lit = match &a.body {
        AttrBody::Value(l) => Some(l),
        AttrBody::Args(args) => args.iter().find_map(|x| match x {
            AttrArg::Lit(l) => Some(l),
            _ => None,
        }),
        AttrBody::Empty => None,
    };
    Some(match lit {
        Some(Literal::Str(s) | Literal::RawStr(s)) => s.trim_matches('"').to_owned(),
        _ => String::new(),
    })
}

fn has_attr(attrs: &[Attribute], names: &[&str]) -> bool {
    attrs
        .iter()
        .any(|a| names.contains(&a.name()) || a.path.first().is_some_and(|p| p.starts_with('#')))
}

/// `missing_docs`: the `pub` items of `items` without a doc comment, and
/// the `pub` methods of their inherent impls and of `pub` traits. A
/// `mod m;` (or the empty stub a library stage keeps) is left out: its
/// file's `//!` documents it.
fn missing_docs(items: &[Item], out: &mut Vec<Finding>) {
    // `#[doc(hidden)]` counts as documented; a parser marker is no item
    // the user wrote.
    let documented = |attrs: &[Attribute]| {
        attrs
            .iter()
            .any(|a| a.name() == "doc" || a.path.first().is_some_and(|p| p.starts_with('#')))
    };
    let hit = |what: &str, name: &str, span: Span| Finding {
        lint: "missing_docs",
        span,
        msg: format!("`pub {what} {}` has no doc comment", leaf(name)),
        notes: vec![format!(
            "help: add a `///` line above it that says what it {}",
            if what == "fn" { "does" } else { "is" }
        )],
        secondary: None,
        base: None,
    };
    for item in items {
        match item {
            Item::Impl(im) if im.trait_path.is_none() => {
                for ii in &im.items {
                    if let ImplItem::Method(m) = ii {
                        if m.vis == Visibility::Pub && !documented(&m.attrs) {
                            out.push(hit("fn", &m.name, m.span));
                        }
                    }
                }
                continue;
            }
            Item::Mod(m) => {
                if let (Some(inner), Visibility::Pub) = (&m.items, m.vis) {
                    if !inner.is_empty() && !documented(&m.attrs) {
                        out.push(hit("mod", &m.name, m.span));
                    }
                    missing_docs(inner, out);
                }
                continue;
            }
            _ => {}
        }
        let Some((what, name, vis, attrs)) = crate::doc::item_info(item) else {
            continue;
        };
        if vis != Visibility::Pub {
            continue;
        }
        if !documented(attrs) {
            out.push(hit(what, name, item.span()));
        }
        if let Item::Trait(t) = item {
            for ti in &t.items {
                if let TraitItem::Method(m) = ti {
                    if !documented(&m.attrs) {
                        out.push(hit("fn", &m.name, m.span));
                    }
                }
            }
        }
    }
}

/// Every function in `items`, impl / trait methods and inline modules
/// included.
fn each_fn<'i>(items: &'i [Item], visit: &mut dyn FnMut(&'i FnDef)) {
    for i in items {
        match i {
            Item::Fn(f) => visit(f),
            Item::Impl(im) => im.items.iter().for_each(|ii| {
                if let ImplItem::Method(m) = ii {
                    visit(m);
                }
            }),
            Item::Trait(t) => t.items.iter().for_each(|ti| {
                if let TraitItem::Method(m) = ti {
                    visit(m);
                }
            }),
            Item::Mod(m) => each_fn(m.items.as_deref().unwrap_or(&[]), visit),
            _ => {}
        }
    }
}

/// The last segment of a (library stage's qualified) item name.
fn leaf(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Trait names declared in `items`.
fn collect_traits(items: &[Item], out: &mut HashSet<String>) {
    for i in items {
        match i {
            Item::Trait(t) => {
                out.insert(leaf(&t.name).to_owned());
            }
            Item::Mod(m) => collect_traits(m.items.as_deref().unwrap_or(&[]), out),
            _ => {}
        }
    }
}

fn collect_variants(items: &[Item], out: &mut HashSet<String>) {
    for i in items {
        match i {
            Item::Enum(e) => out.extend(e.variants.iter().map(|v| v.name.clone())),
            Item::Mod(m) => collect_variants(m.items.as_deref().unwrap_or(&[]), out),
            _ => {}
        }
    }
}

/// Every function in `items` by short name: per attribute in `attrs`, its
/// message, for names no unmarked function shares.
fn marked_fns<const N: usize>(items: &[&[Item]], attrs: [&str; N]) -> [HashMap<String, String>; N] {
    let mut marked: [HashMap<String, String>; N] = std::array::from_fn(|_| HashMap::new());
    for i in items {
        each_fn(i, &mut |f| {
            for (k, a) in attrs.iter().enumerate() {
                if let Some(m) = attr_msg(&f.attrs, a) {
                    marked[k].insert(leaf(&f.name).to_owned(), m);
                }
            }
        });
    }
    if marked.iter().all(HashMap::is_empty) {
        return marked;
    }
    for i in items {
        each_fn(i, &mut |f| {
            for (k, a) in attrs.iter().enumerate() {
                if !marked[k].is_empty() && !f.attrs.iter().any(|x| x.name() == *a) {
                    marked[k].remove(leaf(&f.name));
                }
            }
        });
    }
    marked
}

/// The structs and enums of `items` with an `attr` attribute, by short
/// name: its message.
fn marked_types(items: &[&[Item]], attr: &str) -> HashMap<String, String> {
    fn walk(items: &[Item], attr: &str, out: &mut HashMap<String, String>) {
        for i in items {
            let (name, attrs) = match i {
                Item::Struct(s) => (&s.name, &s.attrs),
                Item::Enum(e) => (&e.name, &e.attrs),
                Item::Mod(m) => {
                    walk(m.items.as_deref().unwrap_or(&[]), attr, out);
                    continue;
                }
                _ => continue,
            };
            if let Some(m) = attr_msg(attrs, attr) {
                out.insert(leaf(name).to_owned(), m);
            }
        }
    }
    let mut out = HashMap::new();
    for i in items {
        walk(i, attr, &mut out);
    }
    out
}

/// `e` as source for a fix: a long call shows its arguments as `..`.
fn short_expr(e: &Expr) -> String {
    let s = rokc_ast::print::print_expr(e);
    if s.len() <= 40 {
        return s;
    }
    match &e.kind {
        ExprKind::Call { callee, .. } => format!("{}(..)", rokc_ast::print::print_expr(callee)),
        _ => "..".to_owned(),
    }
}

fn is_snake(n: &str) -> bool {
    !n.chars().any(|c| c.is_ascii_uppercase())
}

/// rustc's rule: no lowercase first letter, no `__`, and an inner `_`
/// only between caseless characters (`V1_0`).
fn is_camel(n: &str) -> bool {
    let t = n.trim_matches('_');
    let cased = |c: char| c.is_lowercase() || c.is_uppercase();
    let cs: Vec<char> = t.chars().collect();
    !t.starts_with(char::is_lowercase)
        && !t.contains("__")
        && !cs
            .windows(2)
            .any(|w| (w[1] == '_' && cased(w[0])) || (w[0] == '_' && cased(w[1])))
}

fn is_upper(n: &str) -> bool {
    !n.chars().any(|c| c.is_ascii_lowercase())
}

fn to_snake(n: &str) -> String {
    let mut s = String::new();
    for (i, c) in n.char_indices() {
        if c.is_ascii_uppercase() {
            if i > 0 && !s.ends_with('_') {
                s.push('_');
            }
            s.push(c.to_ascii_lowercase());
        } else {
            s.push(c);
        }
    }
    s
}

fn to_camel(n: &str) -> String {
    n.split('_')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut cs = p.chars();
            cs.next()
                .map(|c| c.to_ascii_uppercase().to_string() + cs.as_str())
                .unwrap_or_default()
        })
        .collect()
}

struct Binding {
    name: String,
    span: Span,
    used: bool,
    is_loop: bool,
    /// A `ref` parameter: a write reaches the caller, so it is a use.
    by_ref: bool,
}

struct Scope {
    bindings: Vec<Binding>,
    /// Parameters fixed by a trait or with no body: never "unused".
    exempt: bool,
}

/// The AST pass: one visitor over every user file.
struct Cx<'a> {
    texts: &'a [(String, String)],
    findings: Vec<Finding>,
    scopes: Vec<Scope>,
    exempt_next: bool,
    /// Plain-assignment targets (`x = ..`): not reads.
    assign_targets: HashSet<(u32, u32)>,
    /// Spans of `ref` parameters.
    ref_params: HashSet<(u32, u32)>,
    /// Spans of `for` patterns and their parts.
    loop_pats: HashSet<(u32, u32)>,
    /// Names mentioned in the current file / any user file.
    file_names: HashSet<String>,
    all_names: HashSet<String>,
    /// `(file, start)` of user-written casts (`x as T`; f-strings
    /// desugar to casts too).
    casts: HashSet<(u32, u32)>,
    /// `(file, start, end)` of the receivers of user-written `x.clone()`
    /// calls: a primitive's clone lowers to exactly that span.
    clones: HashSet<(u32, u32, u32)>,
    must_use: HashMap<String, String>,
    /// `#[must_use]` structs and enums by short name: the message.
    must_use_types: HashMap<String, String>,
    /// `(file, start)` of statement expressions: the expression as source.
    stmt_exprs: HashMap<(u32, u32), String>,
    /// `(file, start)` of a `let` initializer: the binding's span and name.
    let_inits: HashMap<(u32, u32), (Span, String)>,
    types: TypeMarks,
    /// In an impl's head: its type is not a use.
    impl_head: bool,
    /// The last segment of the type whose impl is walked: a deprecated
    /// type's own impl does not warn about it.
    impl_of: Option<String>,
    traits: HashSet<String>,
    /// Every enum variant name: a bare one in a pattern may be a `use`d
    /// variant, not a binding.
    variants: HashSet<String>,
}

const fn key(s: Span) -> (u32, u32) {
    (s.file, s.start)
}

/// `#[must_use]` HIR types by (is an enum, id): the message.
type MustUse = HashMap<(bool, u32), String>;

impl<'a> Cx<'a> {
    /// `all`: the parsed files; `library`: the items of the library
    /// files `all` leaves out.
    fn new(
        all_files: &[SourceFile],
        library: Option<&rokc_ast::Program>,
        texts: &'a [(String, String)],
    ) -> Self {
        let all: Vec<&[Item]> = all_files
            .iter()
            .map(|f| f.items.as_slice())
            .chain(library.map(|p| p.items.as_slice()))
            .collect();
        let mut traits = HashSet::new();
        let mut variants = HashSet::new();
        for items in &all {
            collect_traits(items, &mut traits);
            collect_variants(items, &mut variants);
        }
        let [must_use] = marked_fns(&all, ["must_use"]);
        let mut types = TypeMarks::default();
        for f in all_files {
            let mut at: Vec<&str> = std::iter::once(f.package.as_str())
                .chain(f.module.iter().map(String::as_str))
                .filter(|s| !s.is_empty())
                .collect();
            types.add(&f.items, &mut at);
        }
        if let Some(p) = library {
            types.add(&p.items, &mut Vec::new());
        }
        Self {
            texts,
            findings: Vec::new(),
            scopes: Vec::new(),
            exempt_next: false,
            assign_targets: HashSet::new(),
            ref_params: HashSet::new(),
            loop_pats: HashSet::new(),
            file_names: HashSet::new(),
            all_names: HashSet::new(),
            casts: HashSet::new(),
            clones: HashSet::new(),
            must_use,
            must_use_types: marked_types(&all, "must_use"),
            stmt_exprs: HashMap::new(),
            let_inits: HashMap::new(),
            types,
            impl_head: false,
            impl_of: None,
            traits,
            variants,
        }
    }

    fn hit(&mut self, lint: &'static str, span: Span, msg: String) {
        self.findings.push(Finding {
            lint,
            span,
            msg,
            notes: Vec::new(),
            secondary: None,
            base: None,
        });
    }

    fn name_lint(&mut self, what: &str, name: &str, span: Span, kind: u8) {
        // An item in a block by its own name (spec 3.9.1).
        let leaf = rokc_core::local::leaf(name);
        let name = demangle(&leaf);
        let (ok, lint, fixed, style) = match kind {
            0 => (
                is_snake(name),
                "non_snake_case",
                to_snake(name),
                "a snake case",
            ),
            1 => (
                is_camel(name),
                "non_camel_case_types",
                to_camel(name),
                "an upper camel case",
            ),
            _ => (
                is_upper(name),
                "non_upper_case_globals",
                to_snake(name).to_ascii_uppercase(),
                "an upper case",
            ),
        };
        if !ok {
            self.findings.push(Finding {
                lint,
                span,
                msg: format!("{what} `{name}` should have {style} name"),
                notes: vec![format!("rename it to `{fixed}`")],
                secondary: None,
                base: None,
            });
        }
    }

    fn file(&mut self, f: &SourceFile) {
        self.file_names.clear();
        for a in &f.attrs {
            self.attr_names(a);
        }
        for item in &f.items {
            self.item(item);
        }
        if !f.stmts.is_empty() {
            self.unreachable(&f.stmts, None);
            self.push_scope();
            for s in &f.stmts {
                walk_stmt(self, s);
            }
            self.pop_scope();
            self.lost_writes(crate::copy_write::check_stmts(&f.stmts));
        }
        self.unused_imports(&f.items);
        let names = std::mem::take(&mut self.file_names);
        self.all_names.extend(names);
    }

    fn attr_names(&mut self, a: &Attribute) {
        self.file_names.extend(a.path.iter().cloned());
        if let AttrBody::Args(args) = &a.body {
            for x in args {
                if let AttrArg::Word(w) = x {
                    self.file_names.insert(w.clone());
                }
            }
        }
    }

    fn item(&mut self, item: &Item) {
        for a in item.attrs() {
            self.attr_names(a);
        }
        match item {
            Item::Fn(f) => {
                if !has_attr(&f.attrs, &["no_mangle", "export_name"]) {
                    self.name_lint("function", &f.name, f.span, 0);
                }
                self.func(f, false);
            }
            Item::Struct(s) => {
                self.name_lint("type", &s.name, s.span, 1);
                walk_item(self, item);
            }
            Item::Union(u) => {
                self.name_lint("type", &u.name, u.span, 1);
                walk_item(self, item);
            }
            Item::TypeAlias(t) => {
                self.name_lint("type", &t.name, t.span, 1);
                walk_item(self, item);
            }
            Item::Enum(e) => {
                self.name_lint("type", &e.name, e.span, 1);
                for v in &e.variants {
                    self.name_lint("variant", &v.name, v.span, 1);
                }
                walk_item(self, item);
            }
            Item::Trait(t) => {
                self.name_lint("trait", &t.name, t.span, 1);
                for ti in &t.items {
                    if let TraitItem::Method(m) = ti {
                        self.name_lint("method", &m.name, m.span, 0);
                        self.func(m, true);
                    }
                }
            }
            Item::Impl(im) => {
                self.impl_head = true;
                self.ty_names(&im.self_ty);
                self.impl_head = false;
                self.impl_of = match &im.self_ty.kind {
                    TyKind::Path(p) => p.segments.last().map(|s| s.name.clone()),
                    _ => None,
                };
                if let Some(p) = &im.trait_path {
                    self.file_names
                        .extend(p.segments.iter().map(|s| s.name.clone()));
                }
                for ii in &im.items {
                    match ii {
                        ImplItem::Method(m) => {
                            if im.trait_path.is_none() {
                                self.name_lint("method", &m.name, m.span, 0);
                            }
                            self.func(m, im.trait_path.is_some());
                        }
                        ImplItem::Const(c) => {
                            self.name_lint("constant", &c.name, c.span, 2);
                            self.scoped(|cx| rokc_hir::walk::walk_expr(cx, &c.value));
                        }
                        ImplItem::AssocType { ty, .. } => self.ty_names(ty),
                    }
                }
                self.impl_of = None;
            }
            Item::Const(c) => {
                self.name_lint("constant", &c.name, c.span, 2);
                self.scoped(|cx| walk_item(cx, item));
            }
            Item::Static(s) => {
                self.name_lint("static", &s.name, s.span, 2);
                self.scoped(|cx| walk_item(cx, item));
            }
            Item::Mod(m) => {
                self.name_lint("module", &m.name, m.span, 0);
                for i in m.items.as_deref().unwrap_or(&[]) {
                    self.item(i);
                }
            }
            // C names are not ours to style; the types are uses.
            Item::Extern(b) => {
                for ei in &b.items {
                    match ei {
                        ExternItem::Fn(f) => {
                            for t in f.params.iter().filter_map(|p| p.ty.as_ref()) {
                                self.ty_names(t);
                            }
                            if let Some(t) = &f.ret_ty {
                                self.ty_names(t);
                            }
                        }
                        ExternItem::Static(s) => self.ty_names(&s.ty),
                    }
                }
            }
            Item::Use(_) => {}
        }
    }

    fn ty_names(&mut self, t: &Ty) {
        rokc_hir::walk::walk_ty(self, t);
    }

    fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        self.push_scope();
        f(self);
        self.pop_scope();
    }

    /// A function body; `fixed`: its signature comes from a trait.
    fn func(&mut self, f: &FnDef, fixed: bool) {
        self.exempt_next = fixed || f.body.is_none();
        self.ref_params
            .extend(f.params.iter().filter(|p| p.by_ref).map(|p| key(p.span)));
        walk_fn(self, f);
        self.lost_writes(crate::copy_write::check_fn(f));
        if let Some(b) = &f.body {
            self.unreachable(&b.stmts, b.trailing.as_deref());
        }
    }

    fn lost_writes(&mut self, lost: Vec<crate::copy_write::Lost>) {
        for l in lost {
            self.findings.push(Finding {
                lint: "write_to_copy",
                span: l.span,
                msg: l.msg,
                notes: vec![format!("help: {}", l.help)],
                secondary: None,
                base: None,
            });
        }
    }

    /// `unreachable_code` in one statement list.
    fn unreachable(&mut self, stmts: &[Stmt], trailing: Option<&Expr>) {
        let Some(i) = stmts.iter().position(diverges) else {
            return;
        };
        let next = stmts
            .get(i + 1)
            .map(|s| s.span)
            .or_else(|| trailing.map(|e| e.span));
        if let Some(span) = next {
            self.findings.push(Finding {
                lint: "unreachable_code",
                span,
                msg: "unreachable statement".to_owned(),
                notes: Vec::new(),
                secondary: Some((
                    stmts[i].span,
                    "any code following this statement is unreachable".to_owned(),
                )),
                base: None,
            });
        }
    }

    fn block(&mut self, b: &Block) {
        self.unreachable(&b.stmts, b.trailing.as_deref());
    }

    fn unused_imports(&mut self, items: &[Item]) {
        // `use a::inner; use inner::f;` - a path's first segment may be an
        // earlier import (spec 5.5).
        for item in items {
            if let Item::Use(u) = item {
                if let Some(first) = u.tree.prefix.segments.first() {
                    self.file_names.insert(first.name.clone());
                }
            }
        }
        for item in items {
            match item {
                Item::Use(u) if u.vis == Visibility::Private => {
                    let mut leaves = Vec::new();
                    use_leaves(&u.tree, &mut Vec::new(), &mut leaves);
                    for (name, path, span) in leaves {
                        if !self.file_names.contains(&name) && !self.traits.contains(&name) {
                            let span = if span.is_dummy() { u.span } else { span };
                            self.hit("unused_imports", span, format!("unused import `{path}`"));
                        }
                    }
                }
                Item::Mod(m) => self.unused_imports(m.items.as_deref().unwrap_or(&[])),
                _ => {}
            }
        }
    }

    /// `dead_code`: non-`pub` free functions and inherent methods whose
    /// name no user file mentions.
    fn dead_code(&mut self, user: &[&SourceFile]) {
        fn walk(cx: &mut Cx<'_>, items: &[Item]) {
            for item in items {
                match item {
                    Item::Fn(f) => cx.dead_fn(f),
                    Item::Impl(im) if im.trait_path.is_none() => {
                        for ii in &im.items {
                            if let ImplItem::Method(m) = ii {
                                cx.dead_fn(m);
                            }
                        }
                    }
                    Item::Mod(m) => walk(cx, m.items.as_deref().unwrap_or(&[])),
                    _ => {}
                }
            }
        }
        for f in user {
            walk(self, &f.items);
        }
    }

    fn dead_fn(&mut self, f: &FnDef) {
        const ROOTS: &[&str] = &[
            "test",
            "bench",
            "export_name",
            "no_mangle",
            "lang",
            "intrinsic",
            "infix",
            "prefix",
            "postfix",
            "literal",
            "keyword",
        ];
        // A fn in a block by its own name (spec 4.10).
        let name = rokc_core::local::leaf(&f.name);
        if f.vis != Visibility::Pub
            && name != "main"
            && !name.starts_with('_')
            && !has_attr(&f.attrs, ROOTS)
            && !self.all_names.contains(&*name)
        {
            self.hit(
                "dead_code",
                f.span,
                format!("function `{name}` is never used"),
            );
        }
    }

    /// `redundant_cast` / `redundant_clone` from typed HIR, skipping generic
    /// instances (a cast to `T` isn't redundant because `T = i32` in one).
    fn hir(&mut self, hir: &rokc_hir::HirModule, user: &[&SourceFile]) {
        let files: HashSet<u32> = user.iter().map(|f| f.file_id).collect();
        let mut hits: Vec<(&'static str, Span, String)> = Vec::new();
        let mut cloned: HashSet<(u32, u32)> = HashSet::new();
        // The package's own bodies.
        let own: Vec<&rokc_hir::HirFunction> = hir
            .functions
            .iter()
            .filter(|f| files.contains(&f.span.file) && !f.name.contains("__mono__"))
            .collect();
        // Every statement list of the bodies, for `must_use_values`.
        let mut blocks: Vec<&rokc_hir::HirBlock> = Vec::new();
        for f in &own {
            let Some(body) = &f.body else { continue };
            blocks.push(body);
            each_block(body, &mut |e| {
                if let HirExprKind::Block(b)
                | HirExprKind::Loop { body: b, .. }
                | HirExprKind::While { body: b, .. }
                | HirExprKind::If { then_block: b, .. } = &e.kind
                {
                    blocks.push(b);
                }
                if !files.contains(&e.span.file) {
                    return;
                }
                match &e.kind {
                    HirExprKind::Cast { from_ty, to_ty, .. }
                        if from_ty == to_ty && self.casts.contains(&key(e.span)) =>
                    {
                        hits.push((
                            "redundant_cast",
                            e.span,
                            format!("casting to the same type `{}`", hir_ty(to_ty)),
                        ));
                    }
                    // A primitive's `clone` is the `rok.clone.copy` intrinsic,
                    // lowered to its receiver. Match the receiver's whole
                    // span: a call on the clone (`x.clone().len()`) or an
                    // f-string hole's temps start at the same place.
                    _ if matches!(e.ty, HirTy::Prim(_))
                        && self
                            .clones
                            .contains(&(e.span.file, e.span.start, e.span.end))
                        && cloned.insert(key(e.span)) =>
                    {
                        hits.push((
                            "redundant_clone",
                            e.span,
                            format!(
                                "redundant `.clone()` of a `{}`: the value is copied anyway",
                                hir_ty(&e.ty)
                            ),
                        ));
                    }
                    _ => {}
                }
            });
        }
        let resources = release::Resources::new(hir);
        let mut released = Vec::new();
        for f in &own {
            release::check(&resources, f, &files, &mut released);
        }
        for h in released {
            self.findings.push(Finding {
                lint: h.lint,
                span: h.span,
                msg: h.msg,
                notes: h.notes,
                secondary: h.secondary,
                base: None,
            });
        }
        let types: MustUse = hir
            .structs
            .iter()
            .map(|s| ((false, s.id.0), s.name.as_str()))
            .chain(hir.enums.iter().map(|e| ((true, e.id.0), e.name.as_str())))
            .filter_map(|(k, n)| {
                let m = self
                    .must_use_types
                    .get(leaf(n.split("__").next().unwrap_or(n)))?;
                Some((k, m.clone()))
            })
            .collect();
        if !types.is_empty() {
            self.must_use_values(&types, &blocks);
        }
        for (lint, span, msg) in hits {
            self.hit(lint, span, msg);
        }
    }

    /// Unused `#[must_use]` values: a statement of the type
    /// (`unused_must_use`) and an unused `let` of it. Messages are
    /// `noun phrase: fix`, `{}` standing for the value.
    fn must_use_values(&mut self, types: &MustUse, blocks: &[&rokc_hir::HirBlock]) {
        let msg_of = |t: &HirTy| {
            match t {
                HirTy::Struct(id) => types.get(&(false, id.0)),
                HirTy::Enum(id) => types.get(&(true, id.0)),
                _ => None,
            }
            .cloned()
        };
        let split = |msg: &str, what: &str| match msg.split_once(": ") {
            Some((head, fix)) => (
                head.to_owned(),
                Some(format!("help: {}", fix.replace("{}", what))),
            ),
            None => (msg.to_owned(), None),
        };
        for s in blocks.iter().flat_map(|b| &b.stmts) {
            match s {
                rokc_hir::HirStmt::Expr(e) => {
                    let (Some(msg), Some(src)) = (msg_of(&e.ty), self.stmt_exprs.get(&key(e.span)))
                    else {
                        continue;
                    };
                    let (head, fix) = split(&msg, src);
                    let msg = format!("this {head}");
                    self.findings.push(Finding {
                        lint: "unused_must_use",
                        span: e.span,
                        msg,
                        notes: fix.into_iter().collect(),
                        secondary: None,
                        base: None,
                    });
                }
                rokc_hir::HirStmt::Let { init, .. } => {
                    let (Some(msg), Some((at, name))) =
                        (msg_of(&init.ty), self.let_inits.get(&key(init.span)))
                    else {
                        continue;
                    };
                    let name = demangle(name);
                    let (head, fix) = split(&msg, name);
                    let at = name_span(self.texts, *at, name).unwrap_or(*at);
                    let unused = self
                        .findings
                        .iter_mut()
                        .find(|f| f.lint == "unused_variables" && f.span == at);
                    if let Some(f) = unused {
                        f.msg = format!("`{name}` is never used: the {head}");
                        f.notes.splice(0..0, fix);
                    }
                }
                _ => {}
            }
        }
    }

    /// `"hello {name}"` with a local `name` in scope: the `f` is missing.
    /// The hole counts as a use, so `name` is not also unused.
    fn missing_f(&mut self, text: &str, span: Span) {
        let bound = |cx: &Self, n: &str| {
            cx.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.name == n))
        };
        let Some(name) = text
            .split('{')
            .skip(1)
            .filter_map(|h| h.split_once('}').map(|(n, _)| n.trim()))
            .find(|n| {
                n.chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_')
                    && n.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && bound(self, n)
            })
            .map(str::to_owned)
        else {
            return;
        };
        self.mark_used(&name);
        let raw = text.replace('\n', "\\n").replace('"', "\\\"");
        self.findings.push(Finding {
            lint: "missing_f_prefix",
            span,
            msg: format!("`{{{name}}}` is plain text here: the string has no `f` prefix"),
            notes: vec![format!(
                "help: to put the value of `{name}` in the text, write `f\"{raw}\"`"
            )],
            secondary: None,
            base: None,
        });
    }

    fn mark_used(&mut self, name: &str) {
        for s in self.scopes.iter_mut().rev() {
            if let Some(b) = s.bindings.iter_mut().rev().find(|b| b.name == name) {
                b.used = true;
                return;
            }
        }
    }

    /// `name` written (assigned, or captured by `ref`): a use when it is a
    /// `ref` parameter.
    fn mark_written(&mut self, name: &str) {
        for s in self.scopes.iter_mut().rev() {
            if let Some(b) = s.bindings.iter_mut().rev().find(|b| b.name == name) {
                b.used |= b.by_ref;
                return;
            }
        }
    }

    /// `==` / `!=` / ordering comparisons with a NaN, `none`, or a `bool`
    /// literal.
    fn comparison(&mut self, e: &Expr, op: &str, lhs: &Expr, rhs: &Expr) {
        if !matches!(op, "==" | "!=" | "<" | "<=" | ">" | ">=") {
            return;
        }
        let nan = |x: &Expr| match &x.kind {
            ExprKind::Path(p) => p.segments.last().is_some_and(|s| s.name == "NAN"),
            ExprKind::Call { callee, args, .. } if args.is_empty() => matches!(
                &callee.kind,
                ExprKind::Path(p) if p.segments.len() == 2
                    && matches!(p.segments[0].name.as_str(), "f32" | "f64")
                    && p.segments[1].name == "nan"
            ),
            _ => false,
        };
        if nan(lhs) || nan(rhs) {
            self.hit(
                "nan_comparison",
                e.span,
                format!(
                    "comparison with NaN is always {}; use `.is_nan()`",
                    if op == "!=" { "true" } else { "false" }
                ),
            );
            return;
        }
        if !matches!(op, "==" | "!=") {
            return;
        }
        let lit = |x: &Expr| match &x.kind {
            ExprKind::Literal(l) => Some(l.clone()),
            _ => None,
        };
        match (lit(lhs), lit(rhs)) {
            (Some(Literal::None), _) | (_, Some(Literal::None)) => {
                let fix = if op == "==" {
                    "x is none"
                } else {
                    "!(x is none)"
                };
                self.hit(
                    "none_comparison",
                    e.span,
                    format!("comparing with `none` using `{op}`; write `{fix}`"),
                );
            }
            (Some(Literal::Bool(b)), _) | (_, Some(Literal::Bool(b))) => {
                let same = (op == "==") == b;
                self.hit(
                    "bool_comparison",
                    e.span,
                    format!(
                        "comparison with `{b}`; use the operand {}",
                        if same { "directly" } else { "negated (`!x`)" }
                    ),
                );
            }
            _ => {}
        }
    }

    /// Statement-position calls: `unused_must_use`, `ignored_error`.
    fn discarded(&mut self, e: &Expr) {
        let mut inner = e;
        if let ExprKind::Try { kind, operand } = &e.kind {
            if *kind == TryKind::Optional {
                self.hit(
                    "ignored_error",
                    e.span,
                    "`try?` result discarded: the error is silently ignored".to_owned(),
                );
            }
            inner = operand;
        }
        let name = match &inner.kind {
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Path(p) => p.segments.last().map(|s| s.name.as_str()),
                _ => None,
            },
            ExprKind::MethodCall { method, .. } => Some(method.as_str()),
            _ => None,
        };
        if let Some(msg) = name.and_then(|n| self.must_use.get(n).cloned()) {
            let mut f = Finding {
                lint: "unused_must_use",
                span: e.span,
                msg: format!(
                    "unused result of `{}`, which is `#[must_use]`",
                    name.unwrap_or("")
                ),
                notes: Vec::new(),
                secondary: None,
                base: None,
            };
            f.notes.extend(Some(msg).filter(|m| !m.is_empty()));
            self.findings.push(f);
        }
    }

    /// `p` names a deprecated type, or one's item (`Old::new`).
    fn type_use(&mut self, p: &rokc_ast::Path) {
        if self.impl_head || p.span.is_dummy() || self.types.marked.is_empty() {
            return;
        }
        let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
        let hit = self
            .types
            .find(&segs)
            .or_else(|| self.types.find(&segs[..segs.len().saturating_sub(1)]));
        let Some((name, what, d)) = hit.map(|(k, w, d)| (leaf(k).to_owned(), *w, d.clone())) else {
            return;
        };
        if self.impl_of.as_deref() == Some(name.as_str()) {
            return;
        }
        self.findings.push(Finding {
            lint: "deprecated",
            span: p.span,
            msg: format!("use of deprecated {what} `{name}`"),
            notes: d.notes(&name),
            secondary: None,
            base: None,
        });
    }
}

/// The deprecated types and traits of every file, for the paths that name
/// them: by full path when written so, else by a last segment no other
/// type shares (never invent one).
#[derive(Default)]
struct TypeMarks {
    /// Full path, `type` or `trait`, the deprecation.
    marked: Vec<(String, &'static str, rokc_hir::marks::Deprecation)>,
    /// How many types and traits have each last segment.
    leaves: HashMap<String, usize>,
}

impl TypeMarks {
    /// `items`, in module `at` (a library's names are full already).
    fn add<'i>(&mut self, items: &'i [Item], at: &mut Vec<&'i str>) {
        for i in items {
            let (name, attrs, what) = match i {
                Item::Struct(s) => (&s.name, &s.attrs, "type"),
                Item::Enum(e) => (&e.name, &e.attrs, "type"),
                Item::Union(u) => (&u.name, &u.attrs, "type"),
                Item::TypeAlias(t) => (&t.name, &t.attrs, "type"),
                Item::Trait(t) => (&t.name, &t.attrs, "trait"),
                Item::Mod(m) => {
                    at.push(&m.name);
                    self.add(m.items.as_deref().unwrap_or(&[]), at);
                    at.pop();
                    continue;
                }
                _ => continue,
            };
            *self.leaves.entry(leaf(name).to_owned()).or_default() += 1;
            let d = attrs
                .iter()
                .filter(|a| a.name() == "deprecated")
                .find_map(rokc_hir::marks::deprecation);
            if let Some(d) = d {
                let full = at
                    .iter()
                    .copied()
                    .chain([name.as_str()])
                    .collect::<Vec<_>>();
                self.marked.push((full.join("::"), what, d));
            }
        }
    }

    /// The deprecated type `segs` names.
    fn find(&self, segs: &[&str]) -> Option<&(String, &'static str, rokc_hir::marks::Deprecation)> {
        let segs = match segs {
            ["pkg" | "sup" | "self", rest @ ..] => rest,
            s => s,
        };
        let last = *segs.last()?;
        if last == "Self" {
            return None;
        }
        let unique = self.leaves.get(last) == Some(&1);
        let path = segs.join("::");
        self.marked.iter().find(|(k, ..)| {
            leaf(k) == last
                && (unique || (segs.len() > 1 && (*k == path || k.ends_with(&format!("::{path}")))))
        })
    }
}

fn hir_ty(t: &HirTy) -> String {
    match t {
        HirTy::Prim(p) => p.name().to_owned(),
        other => format!("{other:?}"),
    }
}

/// Whether control never continues past `s`.
fn diverges(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Return(_)
        | StmtKind::Break { .. }
        | StmtKind::Continue { .. }
        | StmtKind::Throw(_) => true,
        StmtKind::Expr(e) => matches!(
            &e.kind,
            ExprKind::Call { callee, .. } if matches!(
                &callee.kind,
                ExprKind::Path(p) if p.segments.last().is_some_and(|s| matches!(
                    s.name.as_str(), "panic" | "exit" | "unreachable" | "todo"
                ))
            )
        ),
        _ => false,
    }
}

/// `(bound name, written path, span)` for each non-glob leaf of a `use`.
fn use_leaves(t: &UseTree, prefix: &mut Vec<String>, out: &mut Vec<(String, String, Span)>) {
    let n = prefix.len();
    prefix.extend(t.prefix.segments.iter().map(|s| s.name.clone()));
    match &t.kind {
        UseTreeKind::Simple(alias) => {
            let last = match prefix.last().map(String::as_str) {
                Some("self") => prefix.len().checked_sub(2).map(|i| prefix[i].clone()),
                l => l.map(str::to_owned),
            };
            if let Some(name) = alias.clone().or(last) {
                out.push((name, prefix.join("::"), t.prefix.span));
            }
        }
        UseTreeKind::Group(children) => {
            for c in children {
                use_leaves(c, prefix, out);
            }
        }
        UseTreeKind::Glob | UseTreeKind::GlobRecursive => {}
    }
    prefix.truncate(n);
}

/// Every pattern span in `p` (a `for` pattern's binders).
fn pat_spans(p: &Pattern, out: &mut HashSet<(u32, u32)>) {
    out.insert(key(p.span));
    match &p.kind {
        PatternKind::Binding { sub: Some(s), .. } => pat_spans(s, out),
        PatternKind::Tuple(ps)
        | PatternKind::Or(ps)
        | PatternKind::EnumVariant { args: ps, .. } => {
            ps.iter().for_each(|x| pat_spans(x, out));
        }
        PatternKind::Struct { fields, .. } => {
            for f in fields {
                if let Some(x) = &f.pat {
                    pat_spans(x, out);
                }
            }
        }
        _ => {}
    }
}

/// Path names a pattern mentions (enum / struct / constant paths).
fn pat_names(p: &Pattern, out: &mut HashSet<String>) {
    match &p.kind {
        PatternKind::Struct { path, fields, .. } => {
            out.extend(path.segments.iter().map(|s| s.name.clone()));
            for f in fields {
                if let Some(x) = &f.pat {
                    pat_names(x, out);
                }
            }
        }
        PatternKind::EnumVariant { path, args } => {
            out.extend(path.segments.iter().map(|s| s.name.clone()));
            args.iter().for_each(|x| pat_names(x, out));
        }
        PatternKind::Binding { name, sub } => {
            out.insert(name.clone());
            if let Some(s) = sub {
                pat_names(s, out);
            }
        }
        PatternKind::TypeCheck { ty, .. } => {
            if let TyKind::Path(p) = &ty.kind {
                out.extend(p.segments.iter().map(|s| s.name.clone()));
            }
        }
        PatternKind::Tuple(ps) | PatternKind::Or(ps) => ps.iter().for_each(|x| pat_names(x, out)),
        _ => {}
    }
}

impl AstVisitor for Cx<'_> {
    fn push_scope(&mut self) {
        self.scopes.push(Scope {
            bindings: Vec::new(),
            exempt: std::mem::take(&mut self.exempt_next),
        });
    }

    fn pop_scope(&mut self) {
        let Some(s) = self.scopes.pop() else { return };
        if s.exempt {
            return;
        }
        for b in s.bindings {
            if !b.used {
                let name = demangle(&b.name);
                let span = name_span(self.texts, b.span, name).unwrap_or(b.span);
                self.findings.push(Finding {
                    lint: "unused_variables",
                    span,
                    msg: format!("unused variable `{name}`"),
                    notes: vec![format!(
                        "if this is intentional, prefix it with an underscore: `_{name}`"
                    )],
                    secondary: None,
                    base: None,
                });
            }
        }
    }

    fn bind_at(&mut self, name: &str, at: Span) {
        if name == "self" || name.starts_with('_') || self.variants.contains(name) {
            return;
        }
        let is_loop = self.loop_pats.contains(&key(at));
        if !is_loop {
            let shadows = self
                .scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.is_loop && b.name == name));
            if shadows {
                self.hit(
                    "shadowed_loop_var",
                    at,
                    format!("`{name}` shadows the loop variable `{name}`"),
                );
            }
        }
        self.name_lint("variable", name, at, 0);
        if let Some(s) = self.scopes.last_mut() {
            s.bindings.push(Binding {
                name: name.to_owned(),
                span: at,
                used: false,
                is_loop,
                by_ref: self.ref_params.contains(&key(at)),
            });
        }
    }

    fn path(&mut self, p: &rokc_ast::Path) {
        self.type_use(p);
    }

    fn ty(&mut self, t: &Ty) {
        if let TyKind::Path(p) = &t.kind {
            self.file_names
                .extend(p.segments.iter().map(|s| s.name.clone()));
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Assign(target, value) => {
                if let ExprKind::Path(p) = &target.kind {
                    if let [seg] = p.segments.as_slice() {
                        self.assign_targets.insert(key(target.span));
                        self.mark_written(&seg.name);
                        if matches!(&value.kind, ExprKind::Path(q)
                            if q.segments.len() == 1 && q.segments[0].name == seg.name)
                        {
                            self.hit(
                                "self_assignment",
                                s.span,
                                format!("`{0}` is assigned to itself", seg.name),
                            );
                        }
                    }
                }
            }
            StmtKind::Expr(e) => {
                self.stmt_exprs.insert(key(e.span), short_expr(e));
                self.discarded(e);
            }
            StmtKind::Let { name, init, .. } => {
                self.let_inits
                    .insert(key(init.span), (s.span, name.clone()));
            }
            StmtKind::LetPattern { pat, .. } => pat_names(pat, &mut self.file_names),
            StmtKind::LetStruct { path, .. } => {
                self.file_names
                    .extend(path.segments.iter().map(|s| s.name.clone()));
            }
            StmtKind::Item(i) => {
                // A nested item can't see the enclosing locals.
                let saved = std::mem::take(&mut self.scopes);
                self.item(i);
                self.scopes = saved;
            }
            _ => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Path(p) => {
                self.file_names
                    .extend(p.segments.iter().map(|s| s.name.clone()));
                if let [seg] = p.segments.as_slice() {
                    if !self.assign_targets.contains(&key(e.span)) {
                        let n = seg.name.clone();
                        self.mark_used(&n);
                    }
                }
            }
            ExprKind::MethodCall {
                receiver,
                method,
                args,
                ..
            } => {
                if method == "clone" && args.is_empty() {
                    let r = receiver.span;
                    self.clones.insert((r.file, r.start, r.end));
                }
                self.file_names.insert(method.clone());
                for a in args {
                    if let CallArg::Named { name, .. } = a {
                        self.file_names.insert(name.clone());
                    }
                }
            }
            ExprKind::QuestionDot(_, tails) => {
                for t in tails {
                    if let AccessTail::Method { method, .. } = t {
                        self.file_names.insert(method.clone());
                    }
                }
            }
            ExprKind::StructInit { path, .. } => {
                self.file_names
                    .extend(path.segments.iter().map(|s| s.name.clone()));
            }
            ExprKind::Closure {
                captures, params, ..
            } => {
                for c in captures {
                    if let CaptureSpec::Ref(n) = c {
                        self.mark_written(n);
                    }
                }
                self.ref_params
                    .extend(params.iter().filter(|p| p.by_ref).map(|p| key(p.span)));
            }
            ExprKind::Cast(_, ty) => {
                let genuine = self
                    .texts
                    .get(usize::try_from(ty.span.file).unwrap_or(0).wrapping_sub(1))
                    .and_then(|(_, t)| t.get(..usize::try_from(ty.span.start).ok()?))
                    .is_some_and(|before| before.trim_end().ends_with("as"));
                if genuine {
                    self.casts.insert(key(e.span));
                }
            }
            ExprKind::Literal(Literal::Str(text)) => self.missing_f(text, e.span),
            ExprKind::OpInfix { op, lhs, rhs } => self.comparison(e, op, lhs, rhs),
            ExprKind::While { cond, body, .. } => {
                if matches!(cond.kind, ExprKind::Literal(Literal::Bool(true))) {
                    self.hit("while_true", e.span, "`while true` is `loop`".to_owned());
                }
                self.block(body);
            }
            ExprKind::For { pat, body, .. } => {
                pat_spans(pat, &mut self.loop_pats);
                pat_names(pat, &mut self.file_names);
                self.block(body);
            }
            ExprKind::Match { arms, .. } => {
                for a in arms {
                    pat_names(&a.pat, &mut self.file_names);
                }
            }
            ExprKind::Is(_, pat) => pat_names(pat, &mut self.file_names),
            ExprKind::Catch {
                catches, finally, ..
            } => {
                for c in catches {
                    pat_names(&c.pat, &mut self.file_names);
                    self.block(&c.body);
                }
                if let Some(f) = finally {
                    self.block(f);
                }
            }
            ExprKind::Block(b)
            | ExprKind::Unsafe(b)
            | ExprKind::Loop { body: b, .. }
            | ExprKind::AsyncBlock(b)
            | ExprKind::YieldBlock(b)
            | ExprKind::AsyncYieldBlock(b) => self.block(b),
            ExprKind::If { then_branch, .. } => self.block(then_branch),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demangle_hides_hygiene_suffix() {
        assert_eq!(demangle("count__mac"), "count");
        assert_eq!(demangle("Field__meta"), "Field");
        assert_eq!(demangle("plain"), "plain");
        // a lone suffix is not a hygiene name - leave it be.
        assert_eq!(demangle("__mac"), "__mac");
    }

    #[test]
    fn names_and_levels() {
        assert!(is_snake("σ²") && is_snake("a_b1") && !is_snake("aB"));
        assert!(is_camel("Foo") && is_camel("HTTP") && !is_camel("foo") && !is_camel("Foo_Bar"));
        assert!(is_upper("MAX_N") && !is_upper("Max"));
        assert_eq!(to_snake("myVarName"), "my_var_name");
        assert_eq!(to_camel("my_type"), "MyType");
        assert_eq!(expand("unused").unwrap().len(), 7);
        assert_eq!(expand("all").unwrap().len(), LINTS.len());
        assert!(expand("nope").is_none());
        assert_eq!(Level::parse("deny"), Some(Level::Deny));
    }

    #[test]
    fn warnings_group_takes_later_lints() {
        static FAKE: &[Lint] = &[
            warn("old", "a lint of the first release"),
            warn_since("new", "a lint of a later release"),
            Lint {
                name: "quiet",
                default: Level::Allow,
                desc: "a lint off by default",
                since: "0.1.6",
            },
        ];
        assert_eq!(expand_in(FAKE, &[], "warnings").unwrap(), ["old", "new"]);
        assert_eq!(
            expand_in(FAKE, &[], "all").unwrap(),
            ["old", "new", "quiet"]
        );
    }

    #[test]
    fn removed_names_set_nothing() {
        static FAKE: &[Lint] = &[warn("live", "a lint still checked")];
        let removed = ["gone"];
        assert_eq!(expand_in(FAKE, &removed, "gone"), Some(Vec::new()));
        assert_eq!(expand_in(FAKE, &removed, "live").unwrap(), ["live"]);
        assert_eq!(expand_in(FAKE, &removed, "all").unwrap(), ["live"]);
        assert!(expand_in(FAKE, &removed, "nope").is_none());
    }

    #[test]
    fn removed_names_are_not_lints() {
        for &r in REMOVED_LINTS {
            assert!(
                !LINTS.iter().any(|l| l.name == r)
                    && !GROUPS.iter().any(|(g, _)| *g == r)
                    && !["all", "warnings"].contains(&r),
                "`{r}` is both removed and live"
            );
            assert_eq!(expand(r), Some(Vec::new()), "{r}");
        }
    }
}
