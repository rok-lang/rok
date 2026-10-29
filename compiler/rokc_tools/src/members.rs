//! Member index: the fields and methods of each type, by the type's own
//! name (`List`, `str`, `i32`), from the ASTs of a check. Completion,
//! hover and definition after `recv.` read it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rokc_ast::{ImplItem, Item, StructKind, TraitItem, TyKind, doc_hidden};
use rokc_core::Visibility;
use rokc_diagnostic::Span;

use crate::analysis::Analysis;
use crate::doc::{doc_text, fn_sig_text, ty_text};

/// A field or method of a type.
#[derive(Clone, Debug)]
pub struct Member {
    pub name: String,
    /// `fn push(self, x: T)` or `x: i32`.
    pub sig: String,
    pub doc: String,
    /// The function's span (its keyword), the field's.
    pub span: Span,
    pub kind: Kind,
    pub public: bool,
    /// Declared in the user package: private members are shown too.
    pub own: bool,
    /// The trait it comes from (`impl Tr for T`).
    pub via_trait: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Field,
    /// Takes `self`.
    Method,
    /// An associated function (`List::new`).
    Assoc,
}

impl Member {
    /// Shown outside its declaring package?
    pub const fn shown(&self) -> bool {
        self.public || self.own
    }
}

#[derive(Default)]
struct Index {
    by_type: HashMap<String, Vec<Member>>,
    /// Trait methods by the trait's own name.
    traits: HashMap<String, Vec<Member>>,
}

/// The members of a check: the library's (built once per library) and
/// the user package's.
pub struct Members {
    library: Arc<Index>,
    user: Index,
}

impl Members {
    pub fn of(a: &Analysis) -> Self {
        let library = a
            .library
            .map_or_else(|| Arc::new(Index::default()), library);
        let mut user = Index::default();
        for f in &a.sources {
            add(
                &mut user,
                &f.items,
                &f.module.join("::"),
                f.package.is_empty(),
            );
        }
        traits(
            &mut user,
            a.sources.iter().map(|f| f.items.as_slice()),
            &library,
        );
        Self { library, user }
    }

    /// The members of type `ty` (its own name, without generics), the
    /// user package's first.
    pub fn get<'a>(&'a self, ty: &str) -> impl Iterator<Item = &'a Member> + 'a {
        let key = type_key(ty).to_owned();
        let user = self.user.by_type.get(&key).into_iter().flatten();
        let lib = self.library.by_type.get(&key).into_iter().flatten();
        user.chain(lib).filter(|m| m.shown())
    }
}

/// `List<i32>` → `List`; a module path or a block-local name to its leaf.
pub fn type_key(ty: &str) -> &str {
    let base = ty.split('<').next().unwrap_or(ty).trim();
    let base = base.rsplit("::").next().unwrap_or(base);
    base.rsplit('/').next().unwrap_or(base)
}

/// The library's index, cached by its address (the library stage lives
/// for the process).
fn library(p: &'static rokc_ast::Program) -> Arc<Index> {
    static CACHE: Mutex<Option<(usize, Arc<Index>)>> = Mutex::new(None);
    let key = std::ptr::from_ref(p) as usize;
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((k, ix)) = cache.as_ref() {
        if *k == key {
            return ix.clone();
        }
    }
    let mut ix = Index::default();
    add(&mut ix, &p.items, "", false);
    let empty = Index::default();
    traits(&mut ix, std::iter::once(p.items.as_slice()), &empty);
    let ix = Arc::new(ix);
    *cache = Some((key, ix.clone()));
    ix
}

fn leaf(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// An item's path: library items are named by theirs already.
fn full(module: &str, name: &str) -> String {
    if module.is_empty() || name.contains("::") {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

/// Traits whose methods are spelled as operators (`a + b`, `xs[i]`)
/// or that the compiler alone calls: not offered after `.`.
fn hook_trait(module: &str, name: &str) -> bool {
    let path = full(module, name);
    ["rok::core::ops::", "rok::core::index::", "rok::bare::"]
        .iter()
        .any(|p| path.starts_with(p))
        || path == "rok::core::marker::Transfer"
}

/// Methods of a function list: not hidden, not compiler hooks (`__`).
fn method(f: &rokc_ast::FnDef, public: bool, own: bool, via: Option<&str>) -> Option<Member> {
    let name = leaf(&f.name);
    if doc_hidden(&f.attrs) || name.starts_with("__") {
        return None;
    }
    Some(Member {
        name: name.to_owned(),
        sig: fn_sig_text(f, name),
        doc: doc_text(&f.attrs),
        span: f.span,
        kind: if f.params.first().is_some_and(|p| p.name == "self") {
            Kind::Method
        } else {
            Kind::Assoc
        },
        public,
        own,
        via_trait: via.map(str::to_owned),
    })
}

/// The name an impl's `Self` type goes by.
fn self_name(im: &rokc_ast::ImplDef) -> Option<&str> {
    match &im.self_ty.kind {
        TyKind::Path(p) => p.segments.last().map(|s| leaf(&s.name)),
        _ => None,
    }
}

/// Fields, inherent methods and trait declarations of `items`.
fn add(ix: &mut Index, items: &[Item], module: &str, own: bool) {
    for item in items {
        match item {
            Item::Struct(s) if !doc_hidden(&s.attrs) => {
                let StructKind::Named(fs) = &s.kind else {
                    continue;
                };
                let key = rokc_core::local::leaf(leaf(&s.name)).into_owned();
                let out = ix.by_type.entry(key).or_default();
                for f in fs.iter().filter(|f| !doc_hidden(&f.attrs)) {
                    out.push(Member {
                        name: f.name.clone(),
                        sig: format!("{}: {}", f.name, ty_text(&f.ty)),
                        doc: doc_text(&f.attrs),
                        span: f.span,
                        kind: Kind::Field,
                        public: f.vis == Visibility::Pub,
                        own,
                        via_trait: None,
                    });
                }
            }
            Item::Impl(im) if im.trait_path.is_none() => {
                let Some(key) = self_name(im) else { continue };
                let ms: Vec<Member> = im
                    .items
                    .iter()
                    .filter_map(|ii| match ii {
                        ImplItem::Method(m) => method(m, m.vis == Visibility::Pub, own, None),
                        _ => None,
                    })
                    .collect();
                ix.by_type.entry(key.to_owned()).or_default().extend(ms);
            }
            Item::Trait(t) if !doc_hidden(&t.attrs) && !hook_trait(module, &t.name) => {
                let public = t.vis == Visibility::Pub;
                let ms: Vec<Member> = t
                    .items
                    .iter()
                    .filter_map(|ti| match ti {
                        TraitItem::Method(m) => method(m, public, own, Some(leaf(&t.name))),
                        _ => None,
                    })
                    .collect();
                ix.traits
                    .entry(leaf(&t.name).to_owned())
                    .or_default()
                    .extend(ms);
            }
            Item::Mod(m) => {
                let sub = full(module, &m.name);
                add(ix, m.items.as_deref().unwrap_or_default(), &sub, own);
            }
            _ => {}
        }
    }
}

/// `impl Tr for T`: the trait's methods (from `ix` or `library`) become
/// members of `T`. A blanket `impl<T> Tr for T` adds none.
fn traits<'a>(ix: &mut Index, files: impl Iterator<Item = &'a [Item]>, library: &Index) {
    fn walk(items: &[Item], out: &mut Vec<(String, String)>) {
        for item in items {
            match item {
                Item::Impl(im) if !im.negative => {
                    let (Some(tr), Some(ty)) = (&im.trait_path, self_name(im)) else {
                        continue;
                    };
                    let generic = im.generics.params.iter().any(
                        |p| matches!(p, rokc_ast::GenericParam::Type { name, .. } if name == ty),
                    );
                    if let Some(t) = tr.segments.last().filter(|_| !generic) {
                        out.push((ty.to_owned(), leaf(&t.name).to_owned()));
                    }
                }
                Item::Mod(m) => walk(m.items.as_deref().unwrap_or_default(), out),
                _ => {}
            }
        }
    }
    let mut pairs = Vec::new();
    for items in files {
        walk(items, &mut pairs);
    }
    for (ty, tr) in pairs {
        let ms = ix.traits.get(&tr).or_else(|| library.traits.get(&tr));
        let ms: Vec<Member> = ms.into_iter().flatten().cloned().collect();
        ix.by_type.entry(ty).or_default().extend(ms);
    }
}
