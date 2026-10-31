//! Attribute placement and argument checks (spec 6.1 / 6.2 / 6.3), after
//! `#[cfg]` filtering and meta expansion. A name neither built in nor
//! defined by a meta block is an error (6.1.6).

use rokc_ast::{
    AttrArg, AttrBody, Attribute, EnumVariantKind, ExternItem, ImplItem, Item, Literal, StructKind,
    TraitItem,
};
use rokc_diagnostic::{ErrorKind, RokDiagnostic};
use rokc_parser::MetaTable;

/// Where an attribute sits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pos {
    File,
    Fn,
    Method,
    ExternFn,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Const,
    Static,
    TypeAlias,
    Mod,
    Use,
    Extern,
    Field,
    Variant,
}

impl Pos {
    const fn name(self) -> &'static str {
        match self {
            Self::File => "a file",
            Self::Fn => "a function",
            Self::Method => "a method",
            Self::ExternFn => "an extern function",
            Self::Struct => "a struct",
            Self::Enum => "an enum",
            Self::Union => "a union",
            Self::Trait => "a trait",
            Self::Impl => "an impl",
            Self::Const => "a const",
            Self::Static => "a static",
            Self::TypeAlias => "a type alias",
            Self::Mod => "a module",
            Self::Use => "a use",
            Self::Extern => "an extern block",
            Self::Field => "a field",
            Self::Variant => "a variant",
        }
    }
}

/// Attributes the compiler gives meaning to in places of its own
/// choosing (magic, 6.4 - 6.8); only their names are checked.
const MAGIC: &[&str] = &[
    "lang",
    "intrinsic",
    "infix",
    "prefix",
    "postfix",
    "literal",
    "keyword",
    "niche",
    "export_name",
    "no_mangle",
    "impl_derive",
    "impl_derive_attribute",
    "impl_attribute",
];

/// Check the `#[cfg]` predicates of a file (before filtering drops the
/// items they guard).
pub fn check_cfg(
    file_attrs: &[Attribute],
    items: &[Item],
    metas: &MetaTable,
    diags: &mut Vec<RokDiagnostic>,
) {
    Cx {
        metas,
        diags,
        cfg_only: true,
    }
    .file(file_attrs, items);
}

/// Check every other attribute of a file's remaining items.
pub fn check(
    file_attrs: &[Attribute],
    items: &[Item],
    metas: &MetaTable,
    diags: &mut Vec<RokDiagnostic>,
) {
    Cx {
        metas,
        diags,
        cfg_only: false,
    }
    .file(file_attrs, items);
}

struct Cx<'a> {
    metas: &'a MetaTable,
    diags: &'a mut Vec<RokDiagnostic>,
    /// Checking only `#[cfg]` (its items may be dropped).
    cfg_only: bool,
}

impl Cx<'_> {
    fn file(&mut self, attrs: &[Attribute], items: &[Item]) {
        self.attrs(attrs, Pos::File);
        self.items(items);
    }

    fn items(&mut self, items: &[Item]) {
        for item in items {
            self.item(item);
        }
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::Fn(f) => self.attrs(&f.attrs, Pos::Fn),
            Item::Struct(s) => {
                self.attrs(&s.attrs, Pos::Struct);
                match &s.kind {
                    StructKind::Named(fs) => {
                        fs.iter().for_each(|f| self.attrs(&f.attrs, Pos::Field));
                    }
                    StructKind::Tuple(fs) => {
                        fs.iter().for_each(|f| self.attrs(&f.attrs, Pos::Field));
                    }
                    StructKind::Unit => {}
                }
            }
            Item::Enum(e) => {
                self.attrs(&e.attrs, Pos::Enum);
                for v in &e.variants {
                    self.attrs(&v.attrs, Pos::Variant);
                    if let EnumVariantKind::Named(fs) = &v.kind {
                        fs.iter().for_each(|f| self.attrs(&f.attrs, Pos::Field));
                    }
                }
            }
            Item::Union(u) => {
                self.attrs(&u.attrs, Pos::Union);
                u.fields
                    .iter()
                    .for_each(|f| self.attrs(&f.attrs, Pos::Field));
            }
            Item::Trait(t) => {
                self.attrs(&t.attrs, Pos::Trait);
                for i in &t.items {
                    if let TraitItem::Method(f) = i {
                        self.attrs(&f.attrs, Pos::Method);
                    }
                }
            }
            Item::Impl(i) => {
                self.attrs(&i.attrs, Pos::Impl);
                for it in &i.items {
                    match it {
                        ImplItem::Method(f) => self.attrs(&f.attrs, Pos::Method),
                        ImplItem::Const(c) => self.attrs(&c.attrs, Pos::Const),
                        ImplItem::AssocType { .. } => {}
                    }
                }
            }
            Item::Const(c) => self.attrs(&c.attrs, Pos::Const),
            Item::Static(s) => self.attrs(&s.attrs, Pos::Static),
            Item::TypeAlias(a) => self.attrs(&a.attrs, Pos::TypeAlias),
            Item::Mod(m) => {
                self.attrs(&m.attrs, Pos::Mod);
                if let Some(inner) = &m.items {
                    self.items(inner);
                }
            }
            Item::Use(u) => self.attrs(&u.attrs, Pos::Use),
            Item::Extern(b) => {
                self.attrs(&b.attrs, Pos::Extern);
                for i in &b.items {
                    match i {
                        ExternItem::Fn(f) => self.attrs(&f.attrs, Pos::ExternFn),
                        ExternItem::Static(s) => self.attrs(&s.attrs, Pos::Static),
                    }
                }
            }
        }
    }

    fn attrs(&mut self, attrs: &[Attribute], pos: Pos) {
        for a in attrs {
            if let Some(msg) = self.problem(a, pos) {
                self.diags
                    .push(RokDiagnostic::error(ErrorKind::Meta, a.span, msg));
            }
        }
    }

    /// What is wrong with `a` at `pos`, if anything.
    fn problem(&self, a: &Attribute, pos: Pos) -> Option<String> {
        use Pos as P;
        let name = a.path.join("::");
        // Parser markers (`#const_fn`, ..).
        if name.starts_with('#') || self.cfg_only != (name == "cfg") {
            return None;
        }
        let fns = [P::Fn, P::Method];
        let anywhere: &[Pos] = &[];
        let (places, args): (&[Pos], Option<&str>) = match name.as_str() {
            "derive" if pos == P::Union => {
                return Some("`#[derive]` on a union is a compile error (spec 6.2.1)".to_owned());
            }
            "derive" => (&[P::Struct, P::Enum], words(a, false)),
            "repr" => (&[P::Struct, P::Enum, P::Union], None),
            "inline" => (
                &fns,
                match &a.body {
                    AttrBody::Empty => None,
                    AttrBody::Args(xs) if matches!(xs.as_slice(), [AttrArg::Word(w)] if w == "always" || w == "never") => {
                        None
                    }
                    _ => Some("`#[inline]`, `#[inline(always)]` or `#[inline(never)]`"),
                },
            ),
            "cold" => (&fns, empty(a)),
            "track_caller" => (&[P::Fn, P::Method, P::ExternFn], empty(a)),
            "must_use" => (&[P::Fn, P::Method, P::Struct, P::Enum], message(a)),
            "test" | "bench" => (&[P::Fn], empty(a)),
            "non_exhaustive" => (&[P::Struct, P::Enum], empty(a)),
            "link" => (&[P::Extern], None),
            "link_name" => (
                &[P::ExternFn],
                (!matches!(&a.body, AttrBody::Value(Literal::Str(_))))
                    .then_some("`#[link_name = \"symbol\"]`"),
            ),
            // The runtime's own: its functions neither allocate nor block.
            "no_safepoint" => (&[P::Extern], empty(a)),
            "path" => (&[P::Mod], None),
            "no_implicit_rok" => (&[P::File], empty(a)),
            // What a WIT export (stdlib 1.8) runs.
            "wit::export" => (&[P::Fn, P::Struct], empty(a)),
            "deprecated" => (
                anywhere,
                rokc_hir::marks::deprecation(a).is_none().then_some(
                    "no argument, a message string, or `(since = \"0.1.6\", note = \"..\")`",
                ),
            ),
            // Spec 6.2.14: a safe library function a later release makes
            // `unsafe`. A meta block of the name from before keeps it.
            "future_unsafe" if !self.metas.knows_attribute("future_unsafe") => {
                (&[P::Fn, P::Method, P::ExternFn, P::Static], message(a))
            }
            "doc" => (
                anywhere,
                (!matches!(
                    &a.body,
                    AttrBody::Value(Literal::Str(_) | Literal::RawStr(_))
                ) && !a.is_doc_hidden())
                .then_some("`#[doc = \"..\"]` or `#[doc(hidden)]`"),
            ),
            "allow" | "warn" | "deny" => (anywhere, words(a, false)),
            "cfg" => return rokc_parser::cfg::attr_problem(a),
            // A well-formed one was resolved by the parser.
            "cfg_attr" => {
                return Some(
                    "`#[cfg_attr]` takes a predicate and at least one attribute: \
                     `#[cfg_attr(pred, attr, ..)]` (spec 6.3.4)"
                        .to_owned(),
                );
            }
            n if MAGIC.contains(&n) => return None,
            n if self.metas.knows_attribute(n) => return None,
            n => {
                return Some(format!(
                    "unknown attribute `#[{n}]`: not built in and no meta block defines it \
                     (spec 6.1.6)"
                ));
            }
        };
        if !places.is_empty() && !places.contains(&pos) {
            let list: Vec<&str> = places.iter().map(|p| p.name()).collect();
            let list = match list.split_last() {
                Some((last, [])) => (*last).to_owned(),
                Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                None => String::new(),
            };
            return Some(format!(
                "`#[{name}]` cannot be attached to {}; it goes on {list}",
                pos.name()
            ));
        }
        args.map(|want| format!("malformed `#[{name}]`: expected {want}"))
    }
}

/// `None` when `a` has no arguments.
fn empty(a: &Attribute) -> Option<&'static str> {
    (!matches!(a.body, AttrBody::Empty)).then_some("no arguments")
}

/// `#[x]`, `#[x = "msg"]` or `#[x("msg")]`.
fn message(a: &Attribute) -> Option<&'static str> {
    let ok = match &a.body {
        AttrBody::Empty | AttrBody::Value(Literal::Str(_) | Literal::RawStr(_)) => true,
        AttrBody::Args(xs) => matches!(
            xs.as_slice(),
            [AttrArg::Lit(Literal::Str(_) | Literal::RawStr(_))]
        ),
        AttrBody::Value(_) => false,
    };
    (!ok).then_some("no argument or a message string")
}

/// `#[x(a, b, ..)]` of names (`a::b` paths too); `allow_empty` permits
/// `#[x]`.
fn words(a: &Attribute, allow_empty: bool) -> Option<&'static str> {
    let ok = match &a.body {
        AttrBody::Empty => allow_empty,
        AttrBody::Args(xs) => {
            !xs.is_empty()
                && xs.iter().all(|x| match x {
                    AttrArg::Word(_) => true,
                    AttrArg::Nested { body, .. } => matches!(**body, AttrBody::Empty),
                    _ => false,
                })
        }
        AttrBody::Value(_) => false,
    };
    (!ok).then_some("a list of names")
}

#[cfg(test)]
mod tests {
    /// Every name `problem` matches (or treats as magic) is reserved
    /// against meta blocks.
    #[test]
    fn checked_names_are_reserved() {
        let src = include_str!("attrs.rs");
        let start = src.find("fn problem(").expect("problem");
        let end = src[start..].find("\nfn empty(").expect("empty") + start;
        let mut names: Vec<&str> = super::MAGIC.to_vec();
        for line in src[start..end].lines().map(str::trim) {
            let Some((pat, _)) = line.split_once("=>") else {
                continue;
            };
            if !line.starts_with('"') {
                continue;
            }
            let pat = pat.split(" if ").next().unwrap_or(pat);
            names.extend(pat.split('|').map(|n| n.trim().trim_matches('"')));
        }
        assert!(names.contains(&"bench") && names.contains(&"no_safepoint"));
        for n in names {
            // A path attribute (`wit::export`) has no single name to take.
            if !n.contains("::") {
                assert!(
                    rokc_parser::BUILTIN_ATTRS.contains(&n),
                    "`#[{n}]` is checked by the driver but missing from BUILTIN_ATTRS"
                );
            }
        }
    }
}
