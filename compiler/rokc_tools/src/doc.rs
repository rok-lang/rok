//! `rokc doc` (spec 901 `rokc doc`, 6.2.8): static HTML API docs.
//!
//! Layout under the output directory (`target/doc/`):
//!
//! ```text
//! index.html                    the packages
//! <pkg>/index.html              the package's root module
//! <pkg>/<mod>/.../index.html    one page per public module: its doc, then
//!                               each public item (anchor `#<kind>.<name>`)
//! search-index.js               `SEARCH_INDEX = [{p, k, u, d}, ..]`
//! search.js, style.css
//! ```
//!
//! Documented: the user package, its `--extern` deps and, with `--std`,
//! the standard library; `pub` items in `pub` modules, but not those
//! marked `#[doc(hidden)]` (compiler hooks). Doc text comes from
//! `#[doc]` (`///`, `//!`) in a markdown subset: headings, fenced code,
//! inline code, links, `**bold**`, lists, paragraphs. Type names in
//! signatures and code that name a documented item link to it.
//! Signatures are printed from the AST as written.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rokc_ast::escape::{EscapeMode, unescape};
use rokc_ast::{
    AttrBody, Attribute, ConstExpr, EnumVariantKind, FnDef, GenericParam, Generics, ImplItem, Item,
    Literal, NamedField, StructKind, ThrowsKind, TraitItem, Ty, TyKind, doc_hidden,
};
use rokc_core::Visibility;
use rokc_diagnostic::Json;
use rokc_interface::{CompileResult, SourceFile};

/// One documented item.
struct Entry {
    kind: &'static str,
    name: String,
    /// Signature (HTML).
    sig: String,
    doc: String,
    /// Fields, variants, methods: `(anchor, signature HTML, doc)`.
    members: Vec<(String, String, String)>,
}

#[derive(Default)]
struct Module {
    doc: String,
    entries: Vec<Entry>,
    /// Public child modules.
    children: Vec<String>,
}

/// A page key: package and module path inside it.
type Key = (String, Vec<String>);

/// Which packages `generate` documents.
#[derive(Clone, Copy)]
pub struct Scope {
    /// The user package and its dependencies.
    pub user: bool,
    /// The standard library (`rok::..`).
    pub std: bool,
}

/// Write the docs of `result` (built with `Session::set_keep_ast`) into
/// `out`; `name` is the user package's. Returns `out/index.html`.
pub fn generate(
    result: &CompileResult,
    name: &str,
    out: &Path,
    scope: Scope,
) -> std::io::Result<PathBuf> {
    // The user package (`""`) as `name`, then each dependency.
    let files: Vec<(String, Vec<String>, &SourceFile)> = result
        .sources
        .iter()
        .filter(|f| {
            if f.package.starts_with("rok::") {
                scope.std
            } else {
                scope.user
            }
        })
        .map(|f| {
            let (pkg, rel) = if f.package.is_empty() {
                (name.to_owned(), f.module.clone())
            } else {
                // The module path past the package's mount prefix.
                let skip = f.package.split("::").count().min(f.module.len());
                (f.package.clone(), f.module[skip..].to_vec())
            };
            (pkg, rel, f)
        })
        .collect();

    let mut d = Docs {
        modules: BTreeMap::new(),
        links: HashMap::new(),
        private: Vec::new(),
    };
    // Pass 1: module docs and visibility, and every item's link target.
    for (pkg, rel, f) in &files {
        let m = d.modules.entry((pkg.clone(), rel.clone())).or_default();
        m.doc = doc_text(&f.attrs);
        d.scan(pkg, rel, &f.items);
    }
    // Pass 2: entries, which link to any item from pass 1.
    for (pkg, rel, f) in &files {
        d.module(pkg, rel, &f.items);
    }
    let private = std::mem::take(&mut d.private);
    d.modules
        .retain(|(pkg, rel), _| !private.iter().any(|(p, r)| p == pkg && rel.starts_with(r)));
    let hidden: Vec<String> = private
        .iter()
        .map(|(p, r)| page(p, r).trim_end_matches("index.html").to_owned())
        .collect();
    d.links
        .retain(|_, u| !hidden.iter().any(|h| u.starts_with(h.as_str())));
    d.write(out, name)
}

struct Docs {
    modules: BTreeMap<Key, Module>,
    /// Item name → its URL from the doc root.
    links: HashMap<String, String>,
    /// Modules that are not `pub` (and so neither is what is inside).
    private: Vec<Key>,
}

fn page(pkg: &str, rel: &[String]) -> String {
    // `rok::core` is one directory, `rok-core/`.
    let mut p = pkg.replace("::", "-");
    for r in rel {
        p.push('/');
        p.push_str(r);
    }
    p + "/index.html"
}

fn item_info(item: &Item) -> Option<(&'static str, &str, Visibility, &[Attribute])> {
    // An item in a function body is no API (spec 3.9.1).
    if item.is_local() {
        return None;
    }
    Some(match item {
        Item::Fn(x) => ("fn", &x.name, x.vis, &x.attrs),
        Item::Struct(x) => ("struct", &x.name, x.vis, &x.attrs),
        Item::Enum(x) => ("enum", &x.name, x.vis, &x.attrs),
        Item::Union(x) => ("union", &x.name, x.vis, &x.attrs),
        Item::Trait(x) => ("trait", &x.name, x.vis, &x.attrs),
        Item::TypeAlias(x) => ("type", &x.name, x.vis, &x.attrs),
        Item::Const(x) => ("const", &x.name, x.vis, &x.attrs),
        Item::Static(x) => ("static", &x.name, x.vis, &x.attrs),
        Item::Mod(x) => ("mod", &x.name, x.vis, &x.attrs),
        Item::Impl(_) | Item::Use(_) | Item::Extern(_) => return None,
    })
}

/// The text of the `#[doc = ".."]` attributes (`///` lines), one line each.
pub(crate) fn doc_text(attrs: &[Attribute]) -> String {
    let lines: Vec<String> = attrs
        .iter()
        .filter(|a| a.name() == "doc")
        .filter_map(|a| match &a.body {
            AttrBody::Value(Literal::Str(s)) => Some(unescape(s, EscapeMode::Str).ok()?),
            AttrBody::Value(Literal::RawStr(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    lines.join("\n")
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c => o.push(c),
        }
    }
    o
}

/// One `SEARCH_INDEX` record: path, kind, URL, summary.
fn search_entry(path: &str, kind: &str, url: &str, doc: &str) -> Json {
    Json::obj([
        ("p", Json::str(path)),
        ("k", Json::str(kind)),
        ("u", Json::str(url)),
        ("d", Json::str(summary(doc))),
    ])
}

/// First sentence-ish line of a doc, as plain text (search results).
fn summary(doc: &str) -> String {
    doc.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```"))
        .unwrap_or("")
        .replace('`', "")
}

impl Docs {
    fn scan(&mut self, pkg: &str, rel: &[String], items: &[Item]) {
        for item in items {
            let Some((kind, name, vis, attrs)) = item_info(item) else {
                continue;
            };
            if kind == "mod" {
                let mut child = rel.to_vec();
                child.push(name.to_owned());
                if vis != Visibility::Pub || doc_hidden(attrs) {
                    self.private.push((pkg.to_owned(), child.clone()));
                }
                let doc = doc_text(attrs);
                let m = self
                    .modules
                    .entry((pkg.to_owned(), child.clone()))
                    .or_default();
                if !doc.is_empty() {
                    m.doc = if m.doc.is_empty() {
                        doc
                    } else {
                        format!("{doc}\n\n{}", m.doc)
                    };
                }
                if vis == Visibility::Pub && !doc_hidden(attrs) {
                    let parent = self
                        .modules
                        .entry((pkg.to_owned(), rel.to_vec()))
                        .or_default();
                    if !parent.children.contains(&name.to_owned()) {
                        parent.children.push(name.to_owned());
                    }
                }
                self.links
                    .entry(name.to_owned())
                    .or_insert_with(|| page(pkg, &child));
                if let Item::Mod(m) = item {
                    if let Some(inner) = &m.items {
                        self.scan(pkg, &child, inner);
                    }
                }
            } else if vis == Visibility::Pub && !doc_hidden(attrs) {
                self.links
                    .entry(name.to_owned())
                    .or_insert_with(|| format!("{}#{kind}.{name}", page(pkg, rel)));
            }
        }
    }

    fn module(&mut self, pkg: &str, rel: &[String], items: &[Item]) {
        let mut entries = Vec::new();
        for item in items {
            if let Item::Mod(m) = item {
                if let Some(inner) = &m.items {
                    let mut child = rel.to_vec();
                    child.push(m.name.clone());
                    self.module(pkg, &child, inner);
                }
                continue;
            }
            let Some((kind, name, vis, attrs)) = item_info(item) else {
                continue;
            };
            if vis != Visibility::Pub || doc_hidden(attrs) {
                continue;
            }
            let doc = doc_text(attrs);
            let mut members = Vec::new();
            let sig = match item {
                Item::Fn(f) => self.fn_sig(f),
                Item::Struct(s) => {
                    let fields = match &s.kind {
                        StructKind::Named(fs) => {
                            members.extend(self.fields(kind, name, fs));
                            " { .. }".to_owned()
                        }
                        StructKind::Tuple(ts) => {
                            let tys: Vec<String> = ts.iter().map(|t| self.ty(&t.ty)).collect();
                            format!("({});", tys.join(", "))
                        }
                        StructKind::Unit => ";".to_owned(),
                    };
                    format!(
                        "pub struct {}{}{fields}",
                        esc(name),
                        self.generics(&s.generics)
                    )
                }
                Item::Union(u) => {
                    members.extend(self.fields(kind, name, &u.fields));
                    format!(
                        "pub union {}{} {{ .. }}",
                        esc(name),
                        self.generics(&u.generics)
                    )
                }
                Item::Enum(e) => {
                    for v in e.variants.iter().filter(|v| !doc_hidden(&v.attrs)) {
                        let shape = match &v.kind {
                            EnumVariantKind::Unit => String::new(),
                            EnumVariantKind::Tuple(ts) => format!(
                                "({})",
                                ts.iter().map(|t| self.ty(t)).collect::<Vec<_>>().join(", ")
                            ),
                            EnumVariantKind::Named(fs) => format!(
                                " {{ {} }}",
                                fs.iter()
                                    .map(|f| format!("{}: {}", esc(&f.name), self.ty(&f.ty)))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            EnumVariantKind::Value(_) => " = ..".to_owned(),
                        };
                        members.push((
                            format!("variant.{name}.{}", v.name),
                            format!("{}{shape}", esc(&v.name)),
                            doc_text(&v.attrs),
                        ));
                    }
                    format!("pub enum {}{}", esc(name), self.generics(&e.generics))
                }
                Item::Trait(t) => {
                    for ti in &t.items {
                        match ti {
                            TraitItem::Method(m) if doc_hidden(&m.attrs) => {}
                            TraitItem::Method(m) => members.push((
                                format!("tymethod.{name}.{}", m.name),
                                self.fn_sig(m),
                                doc_text(&m.attrs),
                            )),
                            TraitItem::AssocType { name: n, .. } => members.push((
                                format!("associatedtype.{name}.{n}"),
                                format!("type {}", esc(n)),
                                String::new(),
                            )),
                            TraitItem::Const { name: n, ty, .. } => members.push((
                                format!("associatedconstant.{name}.{n}"),
                                format!("const {}: {}", esc(n), self.ty(ty)),
                                String::new(),
                            )),
                        }
                    }
                    let sup = if t.supertraits.is_empty() {
                        String::new()
                    } else {
                        let s: Vec<String> = t.supertraits.iter().map(|x| self.ty(x)).collect();
                        format!(": {}", s.join(" + "))
                    };
                    format!("pub trait {}{}{sup}", esc(name), self.generics(&t.generics))
                }
                Item::TypeAlias(a) => format!(
                    "pub type {}{} = {}",
                    esc(name),
                    self.generics(&a.generics),
                    self.ty(&a.ty)
                ),
                Item::Const(c) => format!("pub const {}: {}", esc(name), self.ty(&c.ty)),
                Item::Static(s) => format!(
                    "pub static {}{}: {}",
                    if s.is_mut { "mut " } else { "" },
                    esc(name),
                    self.ty(&s.ty)
                ),
                _ => continue,
            };
            entries.push(Entry {
                kind,
                name: name.to_owned(),
                sig,
                doc,
                members,
            });
        }
        // Impl blocks attach to their type in this module (not one in a
        // function body: spec 3.9.1).
        for item in items.iter().filter(|i| !i.is_local()) {
            let Item::Impl(im) = item else { continue };
            let TyKind::Path(p) = &im.self_ty.kind else {
                continue;
            };
            let Some(ty_name) = p.segments.last().map(|s| s.name.as_str()) else {
                continue;
            };
            let Some(e) = entries
                .iter_mut()
                .find(|e| e.name == ty_name && e.kind != "fn")
            else {
                continue;
            };
            if let Some(tr) = &im.trait_path {
                let tr: Vec<&str> = tr.segments.iter().map(|s| s.name.as_str()).collect();
                let tr = tr.last().copied().unwrap_or("");
                let label = format!(
                    "impl {}{} for {}",
                    if im.negative { "!" } else { "" },
                    self.link(tr),
                    esc(ty_name)
                );
                e.members
                    .push((format!("impl.{ty_name}.{tr}"), label, String::new()));
                continue;
            }
            for ii in &im.items {
                if let ImplItem::Method(m) = ii {
                    if m.vis == Visibility::Pub && !doc_hidden(&m.attrs) {
                        e.members.push((
                            format!("method.{ty_name}.{}", m.name),
                            self.fn_sig(m),
                            doc_text(&m.attrs),
                        ));
                    }
                }
            }
        }
        self.modules
            .entry((pkg.to_owned(), rel.to_vec()))
            .or_default()
            .entries
            .extend(entries);
    }

    fn fields(&self, kind: &str, owner: &str, fs: &[NamedField]) -> Vec<(String, String, String)> {
        fs.iter()
            .filter(|f| f.vis == Visibility::Pub && !doc_hidden(&f.attrs))
            .map(|f| {
                (
                    format!("{kind}field.{owner}.{}", f.name),
                    format!("{}: {}", esc(&f.name), self.ty(&f.ty)),
                    doc_text(&f.attrs),
                )
            })
            .collect()
    }

    /// `name` linked when it is a documented item (`ROOT/` is replaced by
    /// the page's path to the doc root).
    fn link(&self, name: &str) -> String {
        match self.links.get(name) {
            Some(u) => format!("<a href=\"ROOT/{u}\">{}</a>", esc(name)),
            None => esc(name),
        }
    }

    fn generics(&self, g: &Generics) -> String {
        if g.params.is_empty() {
            return String::new();
        }
        let ps: Vec<String> = g
            .params
            .iter()
            .map(|p| match p {
                GenericParam::Type { name, bounds, .. } if bounds.is_empty() => esc(name),
                GenericParam::Type { name, bounds, .. } => format!(
                    "{}: {}",
                    esc(name),
                    bounds
                        .iter()
                        .map(|b| self.ty(b))
                        .collect::<Vec<_>>()
                        .join(" + ")
                ),
                GenericParam::Const { name, ty } => format!("const {}: {}", esc(name), self.ty(ty)),
            })
            .collect();
        format!("&lt;{}&gt;", ps.join(", "))
    }

    fn fn_sig(&self, f: &FnDef) -> String {
        let mut s = String::new();
        if f.vis == Visibility::Pub {
            s.push_str("pub ");
        }
        for (on, kw) in [
            (f.is_unsafe, "unsafe "),
            (f.is_async, "async "),
            (f.is_generator, "yield "),
        ] {
            if on {
                s.push_str(kw);
            }
        }
        let params: Vec<String> = f
            .params
            .iter()
            .filter(|p| !p.name.starts_with('#'))
            .map(|p| {
                let mut x = String::new();
                if p.by_ref {
                    x.push_str("ref ");
                }
                x.push_str(&esc(&p.name));
                if let Some(t) = &p.ty {
                    x.push_str(": ");
                    // `xs: ...T` is `xs: Slice<T>`.
                    match &t.kind {
                        rokc_ast::TyKind::Path(path) if p.variadic => {
                            x.push_str("...");
                            if let Some(e) = path.segments.last().and_then(|s| s.generics.first()) {
                                x.push_str(&self.ty(e));
                            }
                        }
                        _ => x.push_str(&self.ty(t)),
                    }
                }
                if p.default.is_some() {
                    x.push_str(" = ..");
                }
                x
            })
            .collect();
        let _ = write!(
            s,
            "fn {}{}({})",
            esc(&f.name),
            self.generics(&f.generics),
            params.join(", ")
        );
        if let ThrowsKind::Typed(tys) = &f.throws {
            let untyped = matches!(tys.as_slice(), [t] if rokc_ast::is_untyped_throws(t));
            if untyped || tys.is_empty() {
                s.push_str(" throws");
            } else {
                let e: Vec<String> = tys.iter().map(|t| self.ty(t)).collect();
                let _ = write!(s, " throws ({})", e.join(" | "));
            }
        } else if matches!(f.throws, ThrowsKind::Untyped) {
            s.push_str(" throws");
        }
        if let Some(r) = &f.ret_ty {
            let _ = write!(s, " -&gt; {}", self.ty(r));
        }
        s
    }

    /// A type as written (HTML), documented names linked.
    fn ty(&self, t: &Ty) -> String {
        let list =
            |ts: &[Ty], sep: &str| ts.iter().map(|x| self.ty(x)).collect::<Vec<_>>().join(sep);
        match &t.kind {
            TyKind::Nest(inner) => self.ty(inner),
            TyKind::Binding(n, b) => format!("{} = {}", esc(n), self.ty(b)),
            TyKind::QPath(q) => format!(
                "&lt;{} as {}&gt;::{}",
                self.ty(&q.self_ty),
                self.ty(&Ty {
                    kind: TyKind::Path(q.trait_path.clone()),
                    span: t.span,
                }),
                esc(&q.item)
            ),
            TyKind::Path(p) => {
                let n = p.segments.len();
                p.segments
                    .iter()
                    .enumerate()
                    .map(|(i, seg)| {
                        let name = if i + 1 == n {
                            self.link(&seg.name)
                        } else {
                            esc(&seg.name)
                        };
                        if seg.generics.is_empty() {
                            name
                        } else {
                            format!("{name}&lt;{}&gt;", list(&seg.generics, ", "))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("::")
            }
            TyKind::Sum(ts) => list(ts, " | "),
            TyKind::Tuple(ts) => format!("({})", list(ts, ", ")),
            TyKind::Fn(args, ret, _) => format!("fn({}) -&gt; {}", list(args, ", "), self.ty(ret)),
            TyKind::Throws { ok, err } => {
                format!("throws ({}) -&gt; {}", self.ty(err), self.ty(ok))
            }
            TyKind::Array(e, n) => format!("[{}; {}]", self.ty(e), const_text(n)),
            TyKind::Any => "any".to_owned(),
            TyKind::AnyTrait(ps) => {
                let b: Vec<String> = ps
                    .iter()
                    .map(|p| {
                        p.segments
                            .last()
                            .map_or_else(String::new, |s| self.link(&s.name))
                    })
                    .collect();
                format!("any {}", b.join(" + "))
            }
            TyKind::None => "none".to_owned(),
            TyKind::SelfUpper => "Self".to_owned(),
            TyKind::Never => "!".to_owned(),
            TyKind::Const(c) => const_text(c),
        }
    }

    fn write(&self, out: &Path, user: &str) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(out)?;
        std::fs::write(out.join("style.css"), STYLE)?;
        std::fs::write(out.join("search.js"), SEARCH_JS)?;
        let mut index = String::from("var SEARCH_INDEX = [\n");
        let mut pkgs: Vec<&str> = Vec::new();
        for ((pkg, rel), m) in &self.modules {
            if !pkgs.contains(&pkg.as_str()) {
                pkgs.push(pkg);
            }
            let url = page(pkg, rel);
            let mut path = vec![pkg.clone()];
            path.extend(rel.iter().cloned());
            if !rel.is_empty() {
                let _ = writeln!(
                    index,
                    "{},",
                    search_entry(&path.join("::"), "mod", &url, &m.doc)
                );
            }
            for e in &m.entries {
                let _ = writeln!(
                    index,
                    "{},",
                    search_entry(
                        &format!("{}::{}", path.join("::"), e.name),
                        e.kind,
                        &format!("{url}#{}.{}", e.kind, e.name),
                        &e.doc
                    )
                );
            }
            let file = out.join(&url);
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(file, self.module_page(pkg, rel, m))?;
        }
        index.push_str("];\n");
        std::fs::write(out.join("search-index.js"), index)?;
        // The user package first.
        pkgs.sort_by_key(|p| (*p != user, *p));
        let mut body = String::from("<h1>Packages</h1>\n<ul class=\"items\">\n");
        for p in &pkgs {
            let doc = self
                .modules
                .get(&((*p).to_owned(), Vec::new()))
                .map(|m| summary(&m.doc))
                .unwrap_or_default();
            let _ = writeln!(
                body,
                "<li><a href=\"{}\">{}</a> <span class=\"summary\">{}</span></li>",
                page(p, &[]),
                esc(p),
                esc(&doc)
            );
        }
        body.push_str("</ul>\n");
        let index_html = out.join("index.html");
        std::fs::write(&index_html, html_page("Packages", "", "", &body))?;
        Ok(index_html)
    }

    fn module_page(&self, pkg: &str, rel: &[String], m: &Module) -> String {
        let root = "../".repeat(rel.len() + 1);
        let root = root.trim_end_matches('/');
        let md = |s: &str| self.markdown(s).replace("ROOT/", &format!("{root}/"));
        let fix = |s: &str| s.replace("ROOT/", &format!("{root}/"));
        let mut title = vec![pkg.to_owned()];
        title.extend(rel.iter().cloned());
        let title = title.join("::");
        // Breadcrumbs: each ancestor links to its page.
        let mut crumbs = format!("<a href=\"{root}/{}\">{}</a>", page(pkg, &[]), esc(pkg));
        for i in 0..rel.len() {
            let _ = write!(
                crumbs,
                "::<a href=\"{root}/{}\">{}</a>",
                page(pkg, &rel[..=i]),
                esc(&rel[i])
            );
        }
        let kind = if rel.is_empty() { "Package" } else { "Module" };
        let mut body = format!(
            "<h1>{kind} {crumbs}</h1>\n<div class=\"doc\">{}</div>\n",
            md(&m.doc)
        );
        let mut side = String::new();
        let mut children = m.children.clone();
        children.sort();
        if !children.is_empty() {
            body.push_str("<h2 id=\"modules\">Modules</h2>\n<ul class=\"items\">\n");
            side.push_str("<h3>Modules</h3><ul>");
            for c in &children {
                let mut child = rel.to_vec();
                child.push(c.clone());
                let doc = self
                    .modules
                    .get(&(pkg.to_owned(), child.clone()))
                    .map(|cm| summary(&cm.doc))
                    .unwrap_or_default();
                let href = format!("{root}/{}", page(pkg, &child));
                let _ = writeln!(
                    body,
                    "<li><a class=\"mod\" href=\"{href}\">{}</a> <span class=\"summary\">{}</span></li>",
                    esc(c),
                    esc(&doc)
                );
                let _ = write!(side, "<li><a href=\"{href}\">{}</a></li>", esc(c));
            }
            body.push_str("</ul>\n");
            side.push_str("</ul>");
        }
        for (kind, heading) in [
            ("struct", "Structs"),
            ("enum", "Enums"),
            ("union", "Unions"),
            ("trait", "Traits"),
            ("type", "Type aliases"),
            ("fn", "Functions"),
            ("const", "Constants"),
            ("static", "Statics"),
        ] {
            let mut es: Vec<&Entry> = m.entries.iter().filter(|e| e.kind == kind).collect();
            if es.is_empty() {
                continue;
            }
            es.sort_by(|a, b| a.name.cmp(&b.name));
            let _ = writeln!(body, "<h2 id=\"{kind}s\">{heading}</h2>");
            let _ = write!(side, "<h3>{heading}</h3><ul>");
            for e in es {
                let id = format!("{}.{}", e.kind, e.name);
                let _ = write!(side, "<li><a href=\"#{id}\">{}</a></li>", esc(&e.name));
                let _ = writeln!(
                    body,
                    "<section class=\"item\" id=\"{id}\">\n<pre class=\"sig {kind}\"><code>{}</code></pre>\n<div class=\"doc\">{}</div>",
                    fix(&e.sig),
                    md(&e.doc)
                );
                if !e.members.is_empty() {
                    body.push_str("<ul class=\"members\">\n");
                    for (anchor, sig, doc) in &e.members {
                        let _ = writeln!(
                            body,
                            "<li id=\"{}\"><code>{}</code><div class=\"doc\">{}</div></li>",
                            esc(anchor),
                            fix(sig),
                            md(doc)
                        );
                    }
                    body.push_str("</ul>\n");
                }
                body.push_str("</section>\n");
            }
            side.push_str("</ul>");
        }
        html_page(&title, &format!("{root}/"), &side, &body)
    }

    /// The markdown subset (see the module doc) as HTML.
    fn markdown(&self, src: &str) -> String {
        let mut out = String::new();
        let mut para: Vec<&str> = Vec::new();
        let mut list: Option<&str> = None;
        let mut code: Option<String> = None;
        let flush = |out: &mut String, para: &mut Vec<&str>, list: &mut Option<&str>| {
            if !para.is_empty() {
                let _ = writeln!(out, "<p>{}</p>", self.inline(&para.join(" ")));
                para.clear();
            }
            if let Some(tag) = list.take() {
                let _ = writeln!(out, "</{tag}>");
            }
        };
        for line in src.lines() {
            let t = line.trim();
            if let Some(buf) = &mut code {
                if t.starts_with("```") {
                    let _ = writeln!(out, "{buf}</code></pre>");
                    code = None;
                } else {
                    buf.push_str(&esc(line));
                    buf.push('\n');
                }
                continue;
            }
            if let Some(lang) = t.strip_prefix("```") {
                flush(&mut out, &mut para, &mut list);
                let lang = if lang.is_empty() { "rok" } else { lang };
                code = Some(format!(
                    "<pre class=\"code\"><code class=\"language-{}\">",
                    esc(lang)
                ));
                continue;
            }
            if t.is_empty() {
                flush(&mut out, &mut para, &mut list);
                continue;
            }
            let hashes = t.chars().take_while(|c| *c == '#').count();
            if (1..=4).contains(&hashes) && t[hashes..].starts_with(' ') {
                flush(&mut out, &mut para, &mut list);
                let level = hashes + 2;
                let _ = writeln!(
                    out,
                    "<h{level}>{}</h{level}>",
                    self.inline(t[hashes..].trim())
                );
                continue;
            }
            let ordered = t
                .split_once(". ")
                .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                .map(|(_, rest)| rest);
            let bullet = t.strip_prefix("- ").or_else(|| t.strip_prefix("* "));
            if let Some((tag, rest)) = bullet
                .map(|r| ("ul", r))
                .or_else(|| ordered.map(|r| ("ol", r)))
            {
                if !para.is_empty() || list.is_some_and(|l| l != tag) {
                    flush(&mut out, &mut para, &mut list);
                }
                if list.is_none() {
                    let _ = writeln!(out, "<{tag}>");
                    list = Some(tag);
                }
                let _ = writeln!(out, "<li>{}</li>", self.inline(rest));
                continue;
            }
            if list.is_some() {
                // A continuation line of the last item.
                if out.ends_with("</li>\n") {
                    out.truncate(out.len() - "</li>\n".len());
                    let _ = writeln!(out, " {}</li>", self.inline(t));
                    continue;
                }
            }
            para.push(t);
        }
        if let Some(buf) = code {
            let _ = writeln!(out, "{buf}</code></pre>");
        }
        flush(&mut out, &mut para, &mut list);
        out
    }

    /// Inline markdown: `code`, [text](url), **bold**.
    fn inline(&self, s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        let mut bold = false;
        while let Some(i) = rest.find(['`', '[', '*']) {
            out.push_str(&esc(&rest[..i]));
            let r = &rest[i..];
            if let Some(body) = r.strip_prefix('`') {
                if let Some(end) = body.find('`') {
                    let code = &body[..end];
                    let linked = self
                        .links
                        .get(code.trim_end_matches("()"))
                        .map(|u| format!("<a href=\"ROOT/{u}\"><code>{}</code></a>", esc(code)));
                    out.push_str(&linked.unwrap_or_else(|| format!("<code>{}</code>", esc(code))));
                    rest = &body[end + 1..];
                    continue;
                }
            } else if let Some(body) = r.strip_prefix('[') {
                let link = body.find("](").and_then(|m| {
                    let close = body[m + 2..].find(')')?;
                    Some((&body[..m], &body[m + 2..m + 2 + close], m + 3 + close))
                });
                if let Some((text, url, used)) = link {
                    let _ = write!(out, "<a href=\"{}\">{}</a>", esc(url), self.inline(text));
                    rest = &body[used..];
                    continue;
                }
            } else if let Some(body) = r.strip_prefix("**") {
                out.push_str(if bold { "</strong>" } else { "<strong>" });
                bold = !bold;
                rest = body;
                continue;
            }
            out.push_str(&esc(&r[..1]));
            rest = &r[1..];
        }
        out.push_str(&esc(rest));
        if bold {
            out.push_str("</strong>");
        }
        out
    }
}

fn const_text(c: &ConstExpr) -> String {
    match c {
        ConstExpr::Literal(Literal::Int(s) | Literal::Float(s)) => esc(s),
        ConstExpr::Path(p) => esc(&p
            .segments
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::")),
        _ => "_".to_owned(),
    }
}

fn html_page(title: &str, root: &str, side: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{} - rok docs</title>\n<link rel=\"stylesheet\" href=\"{root}style.css\">\n\
         </head>\n<body data-root=\"{root}\">\n<nav class=\"side\"><a class=\"home\" href=\"{root}index.html\">rok docs</a>{side}</nav>\n\
         <main>\n<input id=\"search\" type=\"search\" placeholder=\"Search (type a name)\" autocomplete=\"off\">\n\
         <div id=\"results\"></div>\n<div id=\"content\">\n{body}</div>\n</main>\n\
         <script src=\"{root}search-index.js\"></script>\n<script src=\"{root}search.js\"></script>\n\
         </body>\n</html>\n",
        esc(title)
    )
}

const STYLE: &str = "\
:root { --bg: #fff; --fg: #1f2328; --muted: #59636e; --link: #0b62c4; --code: #f3f4f6; --line: #d8dee4; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #16181d; --fg: #e6e8eb; --muted: #9aa4b0; --link: #6cb4ff; --code: #22262e; --line: #30363d; }
}
* { box-sizing: border-box; }
body { margin: 0; display: flex; background: var(--bg); color: var(--fg);
  font: 16px/1.55 system-ui, -apple-system, sans-serif; }
a { color: var(--link); text-decoration: none; }
a:hover { text-decoration: underline; }
nav.side { width: 230px; flex: none; padding: 16px; border-right: 1px solid var(--line);
  min-height: 100vh; font-size: 14px; }
nav.side ul { list-style: none; padding-left: 8px; margin: 4px 0 12px; }
nav.side h3 { font-size: 13px; color: var(--muted); margin: 12px 0 0; }
.home { font-weight: 700; }
main { flex: 1; min-width: 0; max-width: 960px; padding: 16px 32px; }
#search { width: 100%; padding: 8px 10px; font-size: 15px; border: 1px solid var(--line);
  border-radius: 6px; background: var(--bg); color: var(--fg); }
#results ul { list-style: none; padding: 0; }
#results li { padding: 3px 0; }
code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 14px; }
code { background: var(--code); padding: 1px 4px; border-radius: 4px; }
pre { background: var(--code); padding: 10px 12px; border-radius: 6px; overflow-x: auto; }
pre code { background: none; padding: 0; }
.sig { border-left: 3px solid var(--link); }
.summary { color: var(--muted); }
.item { margin: 18px 0 26px; }
ul.items { list-style: none; padding-left: 0; }
ul.members { list-style: none; padding-left: 16px; }
ul.members li { margin: 8px 0; }
.doc p { margin: 6px 0; }
@media (max-width: 700px) { body { display: block; } nav.side { width: auto; min-height: 0;
  border-right: none; border-bottom: 1px solid var(--line); } main { padding: 16px; } }
";

const SEARCH_JS: &str = "\
(function () {
  var input = document.getElementById('search');
  var results = document.getElementById('results');
  var content = document.getElementById('content');
  var root = document.body.getAttribute('data-root') || '';
  function esc(s) { return s.replace(/[&<>\"]/g, function (c) {
    return {'&': '&amp;', '<': '&lt;', '>': '&gt;', '\"': '&quot;'}[c]; }); }
  input.addEventListener('input', function () {
    var q = input.value.trim().toLowerCase();
    if (!q) { results.innerHTML = ''; content.style.display = ''; return; }
    var hits = (window.SEARCH_INDEX || []).filter(function (e) {
      return e.p.toLowerCase().indexOf(q) >= 0;
    });
    hits.sort(function (a, b) {
      var an = a.p.split('::').pop().toLowerCase(), bn = b.p.split('::').pop().toLowerCase();
      return (an !== q) - (bn !== q) || (an.indexOf(q) !== 0) - (bn.indexOf(q) !== 0) || a.p.length - b.p.length;
    });
    content.style.display = 'none';
    results.innerHTML = '<ul>' + hits.slice(0, 200).map(function (e) {
      return '<li><a href=\"' + esc(root + e.u) + '\">' + esc(e.p) + '</a> <span class=\"summary\">' +
        e.k + (e.d ? ' &mdash; ' + esc(e.d) : '') + '</span></li>';
    }).join('') + '</ul>' + (hits.length ? '' : '<p>No results.</p>');
  });
})();
";

#[cfg(test)]
mod tests {
    use super::*;

    fn docs() -> Docs {
        let mut links = HashMap::new();
        links.insert("Point".to_owned(), "app/index.html#struct.Point".to_owned());
        Docs {
            modules: BTreeMap::new(),
            links,
            private: Vec::new(),
        }
    }

    #[test]
    fn markdown_subset() {
        let d = docs();
        let html = d.markdown(
            "# Title\nSome `x` and [a link](https://e.x) **bold**.\n\n- one\n- two\n\n1. first\n\n\
             ```\nlet a = 1 < 2;\n```\nSee `Point`.",
        );
        assert!(html.contains("<h3>Title</h3>"), "{html}");
        assert!(html.contains("<code>x</code>"));
        assert!(html.contains("<a href=\"https://e.x\">a link</a>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(
            html.contains("<ul>\n<li>one</li>\n<li>two</li>\n</ul>"),
            "{html}"
        );
        assert!(html.contains("<ol>\n<li>first</li>\n</ol>"), "{html}");
        assert!(html.contains("let a = 1 &lt; 2;\n</code></pre>"), "{html}");
        assert!(
            html.contains("<a href=\"ROOT/app/index.html#struct.Point\"><code>Point</code></a>")
        );
    }

    #[test]
    fn doc_comments_are_attributes() {
        let src = "//! Module doc.\n//!More.\n\n/// Adds.\n#[inline]\n/// Twice.\n//// not doc\n\
                   pub fn add() {}\n";
        let (tokens, _) = rokc_lexer::Lexer::new(src, "a.rok").lex_all();
        let (p, diags) = rokc_parser::parse_program(&tokens, "a.rok");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(doc_text(&p.attrs), "Module doc.\nMore.");
        let Item::Fn(f) = &p.items[0] else { panic!() };
        assert_eq!(doc_text(&f.attrs), "Adds.\nTwice.");
        assert_eq!(summary("\n`a` b\nc"), "a b");
        let src =
            "/*! Module\n    doc. */\n/**\n * Adds \"1\".\n *\n * \\n\n */\npub fn add() {}\n";
        let (tokens, _) = rokc_lexer::Lexer::new(src, "a.rok").lex_all();
        let (p, diags) = rokc_parser::parse_program(&tokens, "a.rok");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(doc_text(&p.attrs), "Module\n   doc.");
        let Item::Fn(f) = &p.items[0] else { panic!() };
        assert_eq!(doc_text(&f.attrs), "Adds \"1\".\n\n\\n");
    }

    /// Items in a function body (spec 3.9.1) are not the package's API.
    #[test]
    fn block_local_items_are_not_documented() {
        let src = "pub trait Show { fn show(self) -> str; }\npub struct P {}\n\
                   pub fn f() {\n    pub struct Q {}\n    impl Show for P { fn show(self) -> str { \"\" } }\n    \
                   impl P { pub fn m(self) {} }\n}\n";
        let (tokens, _) = rokc_lexer::Lexer::new(src, "a.rok").lex_all();
        let (p, _) = rokc_parser::parse_program(&tokens, "a.rok");
        let mut d = docs();
        d.module("app", &[], &p.items);
        let m = &d.modules[&("app".to_owned(), Vec::new())];
        let names: Vec<&str> = m.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Show", "P", "f"]);
        let p = m.entries.iter().find(|e| e.name == "P").unwrap();
        assert!(p.members.is_empty(), "{:?}", p.members);
    }

    /// `#[doc(hidden)]` items, members and modules stay out of the docs.
    #[test]
    fn doc_hidden_is_not_documented() {
        let src = "/// A hook.\n#[doc(hidden)]\npub fn hook() {}\npub fn shown() {}\n\
                   pub struct P { pub x: i32; #[doc(hidden)] pub raw: i32; }\n\
                   impl P { pub fn get(self) {} #[doc(hidden)] pub fn peek(self) {} }\n\
                   #[doc(hidden)]\npub mod inner { pub fn deep() {} }\n";
        let (tokens, _) = rokc_lexer::Lexer::new(src, "a.rok").lex_all();
        let (p, diags) = rokc_parser::parse_program(&tokens, "a.rok");
        assert!(diags.is_empty(), "{diags:?}");
        let mut d = docs();
        d.scan("app", &[], &p.items);
        d.module("app", &[], &p.items);
        let m = &d.modules[&("app".to_owned(), Vec::new())];
        let names: Vec<&str> = m.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["shown", "P"]);
        let p = m.entries.iter().find(|e| e.name == "P").unwrap();
        let members: Vec<&str> = p.members.iter().map(|(a, ..)| a.as_str()).collect();
        assert_eq!(members, ["structfield.P.x", "method.P.get"]);
        assert!(m.children.is_empty(), "{:?}", m.children);
        assert!(!d.links.contains_key("hook"));
        assert!(
            d.private
                .contains(&("app".to_owned(), vec!["inner".to_owned()]))
        );
    }
}
