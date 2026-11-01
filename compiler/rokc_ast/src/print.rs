//! The AST as rok source (`--emit=source`, spec 9.9.10).
//!
//! Printed after meta expansion and desugaring, without comments or
//! layout, non-atomic operands parenthesized. Parser markers print as the
//! syntax they stand for and `#[derive]` as a comment, so the output
//! parses back to itself. A debug view, not a formatter.

use std::fmt::Write as _;

use rokc_core::Visibility;

use crate::{
    AccessTail, AttrArg, AttrBody, Attribute, Block, CallArg, CaptureSpec, ConstExpr,
    EnumVariantKind, Expr, ExprKind, ExternItem, FStringPart, FnDef, GenericParam, Generics,
    ImplItem, Item, LitArg, Literal, NamedField, Param, Path, Pattern, PatternKind, SpecPiece,
    Stmt, StmtKind, StructKind, ThrowsKind, TraitItem, TryKind, Ty, TyKind, UseTree, UseTreeKind,
    WherePred,
};

/// A file: its inner attributes, items, then top-level statements.
#[must_use]
pub fn print_file(attrs: &[Attribute], items: &[Item], stmts: &[Stmt]) -> String {
    let mut p = Printer::default();
    // Block-local items (spec 3.9.1) print from the copy their block
    // keeps, every name as written.
    let mut back = crate::rename::Renames::default();
    for n in items.iter().filter(|i| i.is_local()).filter_map(Item::name) {
        let leaf = rokc_core::local::leaf(n).into_owned();
        back.types.insert(n.to_owned(), leaf.clone());
        back.values.insert(n.to_owned(), leaf);
    }
    let renamed;
    let (items, stmts) = if back.is_empty() {
        (items, stmts)
    } else {
        let (mut items, mut stmts) = (items.to_vec(), stmts.to_vec());
        items.iter_mut().for_each(|i| back.item(i));
        stmts.iter_mut().for_each(|s| back.stmt(s));
        renamed = (items, stmts);
        (renamed.0.as_slice(), renamed.1.as_slice())
    };
    // Lifted nested fns print where they were written.
    let (lifted, items): (Vec<&Item>, Vec<&Item>) = items
        .iter()
        .filter(|i| !i.is_local())
        .partition(|i| matches!(i, Item::Fn(f) if f.name.contains('/')));
    for i in lifted {
        if let Item::Fn(f) = i {
            let leaf = f.name.rsplit("::").next().unwrap_or_default();
            p.lifted.insert(leaf.to_owned(), f.clone());
        }
    }
    for a in attrs {
        p.attr(a);
        p.nl();
    }
    for (i, item) in items.iter().enumerate() {
        if i > 0 || !attrs.is_empty() {
            p.nl();
        }
        p.item(item);
        p.nl();
    }
    if !(stmts.is_empty() || items.is_empty() && attrs.is_empty()) {
        p.nl();
    }
    for s in stmts {
        p.stmt(s);
        p.nl();
    }
    p.out
}

/// One expression, on one level of indentation.
#[must_use]
pub fn print_expr(e: &Expr) -> String {
    let mut p = Printer::default();
    p.expr(e);
    p.out
}

/// One statement.
#[must_use]
pub fn print_stmt(s: &Stmt) -> String {
    let mut p = Printer::default();
    p.stmt(s);
    p.out
}

/// A function's signature: no attributes, visibility or body.
#[must_use]
pub fn print_fn_sig(f: &FnDef) -> String {
    let sig = f.signature();
    let mut p = Printer::default();
    p.fn_def(&sig);
    p.out.trim_end_matches(';').to_owned()
}

/// One type.
#[must_use]
pub fn print_ty(t: &Ty) -> String {
    let mut p = Printer::default();
    p.ty(t);
    p.out
}

/// One attribute argument (`unix`, `target_os = "linux"`, `all(..)`).
#[must_use]
pub fn print_attr_arg(a: &AttrArg) -> String {
    let mut p = Printer::default();
    p.attr_arg(a);
    p.out
}

#[derive(Default)]
struct Printer {
    out: String,
    indent: usize,
    /// Lifted nested fns by lifted name.
    lifted: std::collections::HashMap<String, FnDef>,
    /// Inside the trailing closure of a call to this: `return` prints
    /// as `return@name`.
    ret_label: Option<String>,
    /// The label the next closure printed takes (a trailing closure's).
    closure_label: Option<String>,
}

impl Printer {
    fn s(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// A local's name; a parser temp (`#using12`) becomes `__rok_using12`
    /// and a keyword `r#match`, so the output parses back.
    fn name(&mut self, n: &str) {
        match n.strip_prefix('#') {
            Some(rest) => {
                self.s("__rok_");
                self.s(rest);
            }
            None if raw(n) => {
                self.s("r#");
                self.s(n);
            }
            None => self.s(n),
        }
    }

    fn nl(&mut self) {
        self.out.push('\n');
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    fn list<T>(&mut self, xs: &[T], sep: &str, mut f: impl FnMut(&mut Self, &T)) {
        for (i, x) in xs.iter().enumerate() {
            if i > 0 {
                self.s(sep);
            }
            f(self, x);
        }
    }

    /// `{` items one per line `}`.
    fn braced<T>(&mut self, xs: &[T], mut f: impl FnMut(&mut Self, &T)) {
        if xs.is_empty() {
            self.s("{}");
            return;
        }
        self.s("{");
        self.indent += 1;
        for x in xs {
            self.nl();
            f(self, x);
        }
        self.indent -= 1;
        self.nl();
        self.s("}");
    }

    fn attr(&mut self, a: &Attribute) {
        self.s(if a.inner { "#![" } else { "#[" });
        self.s(&a.path.join("::"));
        match &a.body {
            // `#[literal(<pattern>, priority = N)]` keeps its pattern as
            // one string (spec 6.5.3): written back bare.
            AttrBody::Args(args) if a.path == ["literal"] => {
                if let [AttrArg::Lit(Literal::Str(pat)), rest @ ..] = args.as_slice() {
                    self.s("(");
                    self.s(pat);
                    for arg in rest {
                        self.s(", ");
                        self.attr_arg(arg);
                    }
                    self.s(")");
                } else {
                    self.attr_body(&a.body);
                }
            }
            b => self.attr_body(b),
        }
        self.s("]");
    }

    fn attr_arg(&mut self, a: &AttrArg) {
        match a {
            AttrArg::Lit(l) => self.lit(l),
            AttrArg::Word(w) => self.s(w),
            AttrArg::KeyValue(k, l) => {
                self.s(k);
                self.s(" = ");
                self.lit(l);
            }
            AttrArg::Nested { path, body } => {
                self.s(&path.join("::"));
                self.attr_body(body);
            }
            AttrArg::Range { lo, hi, inclusive } => {
                if let Some(lo) = lo {
                    self.attr_arg(lo);
                }
                self.s(if *inclusive { "..=" } else { ".." });
                if let Some(hi) = hi {
                    self.attr_arg(hi);
                }
            }
        }
    }

    fn attr_body(&mut self, b: &AttrBody) {
        match b {
            AttrBody::Empty => {}
            AttrBody::Args(args) => {
                self.s("(");
                self.list(args, ", ", Self::attr_arg);
                self.s(")");
            }
            AttrBody::Value(l) => {
                self.s(" = ");
                self.lit(l);
            }
        }
    }

    /// Outer attributes, one per line; markers print as their syntax. A
    /// module's inner ones go in its body.
    fn attrs(&mut self, attrs: &[Attribute]) {
        for a in attrs.iter().filter(|a| !a.inner) {
            if a.path.first().is_some_and(|p| p == crate::CFG_OFF_ATTR) {
                continue;
            }
            if a.path.len() == 1
                && [
                    crate::EXTERN_FN_ATTR,
                    crate::C_VARIADIC_ATTR,
                    crate::CONST_FN_ATTR,
                    crate::LOCAL_ITEM_ATTR,
                    crate::BLOCK_GLOB_ATTR,
                    crate::UNIT_RET_ATTR,
                ]
                .contains(&a.path[0].as_str())
            {
                continue;
            }
            // Its impls follow the item: as an attribute it would derive
            // them again.
            if a.path == ["derive"] {
                self.s("// ");
            }
            self.attr(a);
            self.nl();
        }
    }

    fn vis(&mut self, v: Visibility) {
        self.s(match v {
            Visibility::Pub => "pub ",
            Visibility::Pkg => "pub(pkg) ",
            Visibility::Sub => "pub(sub) ",
            Visibility::Private => "",
        });
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::Fn(f) => self.fn_def(f),
            Item::Struct(s) => {
                self.attrs(&s.attrs);
                self.vis(s.vis);
                self.s("struct ");
                self.s(&s.name);
                self.generics(&s.generics);
                match &s.kind {
                    StructKind::Unit => {
                        self.where_clause(&s.where_clause);
                        self.s(";");
                    }
                    StructKind::Tuple(fields) => {
                        self.s("(");
                        self.list(fields, ", ", |p, f| {
                            for a in &f.attrs {
                                p.attr(a);
                                p.s(" ");
                            }
                            p.vis(f.vis);
                            p.ty(&f.ty);
                        });
                        self.s(")");
                        self.where_clause(&s.where_clause);
                        self.s(";");
                    }
                    StructKind::Named(fields) => {
                        self.where_clause(&s.where_clause);
                        self.s(" ");
                        self.fields(fields);
                    }
                }
            }
            Item::Enum(e) => {
                self.attrs(&e.attrs);
                self.vis(e.vis);
                self.s("enum ");
                self.s(&e.name);
                self.generics(&e.generics);
                self.where_clause(&e.where_clause);
                self.s(" ");
                self.braced(&e.variants, |p, v| {
                    p.attrs(&v.attrs);
                    p.s(&v.name);
                    match &v.kind {
                        EnumVariantKind::Unit => {}
                        EnumVariantKind::Tuple(tys) => {
                            p.s("(");
                            p.list(tys, ", ", Self::ty);
                            p.s(")");
                        }
                        EnumVariantKind::Named(fields) => {
                            p.s(" ");
                            p.fields(fields);
                        }
                        EnumVariantKind::Value(e) => {
                            p.s(" = ");
                            p.expr(e);
                        }
                    }
                    p.s(",");
                });
            }
            Item::Union(u) => {
                self.attrs(&u.attrs);
                self.vis(u.vis);
                self.s("union ");
                self.s(&u.name);
                self.generics(&u.generics);
                self.where_clause(&u.where_clause);
                self.s(" ");
                self.fields(&u.fields);
            }
            Item::Trait(t) => {
                self.attrs(&t.attrs);
                self.vis(t.vis);
                self.s("trait ");
                self.s(&t.name);
                self.generics(&t.generics);
                if !t.supertraits.is_empty() {
                    self.s(": ");
                    self.list(&t.supertraits, " + ", Self::ty);
                }
                self.where_clause(&t.where_clause);
                self.s(" ");
                self.braced(&t.items, |p, it| match it {
                    TraitItem::Method(f) => p.fn_def(f),
                    TraitItem::AssocType {
                        name,
                        bounds,
                        default,
                    } => {
                        p.s("type ");
                        p.s(name);
                        if !bounds.is_empty() {
                            p.s(": ");
                            p.list(bounds, " + ", Self::ty);
                        }
                        if let Some(d) = default {
                            p.s(" = ");
                            p.ty(d);
                        }
                        p.s(";");
                    }
                    TraitItem::Const { name, ty, default } => {
                        p.s("const ");
                        p.s(name);
                        p.s(": ");
                        p.ty(ty);
                        if let Some(d) = default {
                            p.s(" = ");
                            p.expr(d);
                        }
                        p.s(";");
                    }
                });
            }
            Item::Impl(i) => {
                self.attrs(&i.attrs);
                self.vis(i.vis);
                if i.is_unsafe {
                    self.s("unsafe ");
                }
                self.s("impl");
                self.generics(&i.generics);
                self.s(" ");
                if let Some(t) = &i.trait_path {
                    if i.negative {
                        self.s("!");
                    }
                    self.ty_path(t);
                    self.s(" for ");
                }
                self.ty(&i.self_ty);
                self.where_clause(&i.where_clause);
                self.s(" ");
                self.braced(&i.items, |p, it| match it {
                    ImplItem::Method(f) => p.fn_def(f),
                    ImplItem::AssocType { name, ty } => {
                        p.s("type ");
                        p.s(name);
                        p.s(" = ");
                        p.ty(ty);
                        p.s(";");
                    }
                    ImplItem::Const(c) => p.item(&Item::Const(c.clone())),
                });
            }
            Item::Const(c) => {
                self.attrs(&c.attrs);
                self.vis(c.vis);
                self.s("const ");
                self.s(&c.name);
                self.s(": ");
                self.ty(&c.ty);
                self.s(" = ");
                self.expr(&c.value);
                self.s(";");
            }
            Item::Static(s) => {
                self.attrs(&s.attrs);
                self.vis(s.vis);
                self.s(if s.is_mut { "static mut " } else { "static " });
                self.s(&s.name);
                self.s(": ");
                self.ty(&s.ty);
                if let Some(v) = &s.value {
                    self.s(" = ");
                    self.expr(v);
                }
                self.s(";");
            }
            Item::TypeAlias(t) => {
                self.attrs(&t.attrs);
                self.vis(t.vis);
                self.s("type ");
                self.s(&t.name);
                self.generics(&t.generics);
                self.where_clause(&t.where_clause);
                self.s(" = ");
                self.ty(&t.ty);
                self.s(";");
            }
            Item::Mod(m) => {
                self.attrs(&m.attrs);
                self.vis(m.vis);
                self.s("mod ");
                self.s(&m.name);
                match &m.items {
                    None => self.s(";"),
                    Some(items) => {
                        self.s(" {");
                        self.indent += 1;
                        for a in m.attrs.iter().filter(|a| a.inner) {
                            self.nl();
                            self.attr(a);
                        }
                        for x in items {
                            self.nl();
                            self.item(x);
                        }
                        self.indent -= 1;
                        self.nl();
                        self.s("}");
                    }
                }
            }
            Item::Use(u) => {
                self.attrs(&u.attrs);
                self.vis(u.vis);
                self.s("use ");
                self.use_tree(&u.tree);
                self.s(";");
            }
            Item::Extern(e) => {
                self.attrs(&e.attrs);
                if let Some(w) = &e.wit {
                    let kind = if w.export { "export" } else { "import" };
                    let paths: Vec<String> = w.paths.iter().map(|p| format!("{p:?}")).collect();
                    let _ = write!(self.out, "ffi::wit::{kind} {{ {} }}", paths.join(", "));
                    return;
                }
                let _ = write!(self.out, "extern \"{}\" ", e.abi);
                self.braced(&e.items, |p, it| match it {
                    ExternItem::Fn(f) => p.fn_def(f),
                    ExternItem::Static(s) => p.item(&Item::Static(s.clone())),
                });
            }
        }
    }

    /// `{ [attrs] [vis] name: T [= default]; .. }`.
    fn fields(&mut self, fields: &[NamedField]) {
        self.braced(fields, |p, f| {
            p.attrs(&f.attrs);
            p.vis(f.vis);
            p.s(&f.name);
            p.s(": ");
            p.ty(&f.ty);
            if let Some(d) = &f.default {
                p.s(" = ");
                p.expr(d);
            }
            p.s(";");
        });
    }

    fn use_tree(&mut self, t: &UseTree) {
        let prefix: Vec<&str> = t.prefix.segments.iter().map(|s| s.name.as_str()).collect();
        self.s(&prefix.join("::"));
        let sep = if prefix.is_empty() { "" } else { "::" };
        match &t.kind {
            UseTreeKind::Simple(None) => {}
            UseTreeKind::Simple(Some(alias)) => {
                self.s(" as ");
                self.s(alias);
            }
            UseTreeKind::Glob => {
                self.s(sep);
                self.s("*");
            }
            UseTreeKind::GlobRecursive => {
                self.s(sep);
                self.s("**");
            }
            UseTreeKind::Group(children) => {
                self.s(sep);
                self.s("{");
                self.list(children, ", ", Self::use_tree);
                self.s("}");
            }
        }
    }

    fn fn_def(&mut self, f: &FnDef) {
        self.attrs(&f.attrs);
        self.vis(f.vis);
        let marked = |m: &str| f.attrs.iter().any(|a| a.path.len() == 1 && a.path[0] == m);
        if marked(crate::CONST_FN_ATTR) {
            self.s("const ");
        }
        if let Some(abi) = crate::extern_fn_abi(&f.attrs) {
            self.s(&format!("extern {abi:?} "));
        }
        if f.is_unsafe {
            self.s("unsafe ");
        }
        if f.is_async {
            self.s("async ");
        }
        if f.is_generator {
            self.s("yield ");
        }
        self.s("fn ");
        if raw(&f.name) {
            self.s("r#");
        }
        self.s(&f.name);
        self.generics(&f.generics);
        self.s("(");
        self.list(&f.params, ", ", Self::param);
        if marked(crate::C_VARIADIC_ATTR) {
            self.s(if f.params.is_empty() { "..." } else { ", ..." });
        }
        self.s(")");
        match &f.throws {
            ThrowsKind::None => {}
            ThrowsKind::Untyped => self.s(" throws"),
            ThrowsKind::Typed(tys) if tys.len() == 1 && is_untyped_err(&tys[0]) => {
                self.s(" throws");
            }
            // `rethrows` is `throws (#E0 | ..)` over synthetic generics.
            ThrowsKind::Typed(tys) if !tys.is_empty() && tys.iter().all(is_rethrows_err) => {
                self.s(" rethrows");
            }
            ThrowsKind::Typed(tys) => {
                self.s(" throws (");
                self.list(tys, " | ", Self::ty);
                self.s(")");
            }
        }
        if let Some(r) = &f.ret_ty {
            self.s(" -> ");
            self.ty(r);
        }
        self.where_clause(&f.where_clause);
        match &f.body {
            Some(b) => {
                self.s(" ");
                let outer = self.ret_label.take();
                self.block(b);
                self.ret_label = outer;
            }
            None => self.s(";"),
        }
    }

    fn param(&mut self, p: &Param) {
        if p.by_ref {
            self.s("ref ");
        }
        self.s(&p.name);
        if let Some(t) = &p.ty {
            self.s(": ");
            match &t.kind {
                // `xs: ...T` is `xs: Slice<T>`.
                TyKind::Path(path) if p.variadic => {
                    self.s("...");
                    if let Some(elem) = path.segments.last().and_then(|s| s.generics.first()) {
                        self.ty(elem);
                    }
                }
                _ => self.ty(t),
            }
        }
        if let Some(d) = &p.default {
            self.s(" = ");
            self.expr(d);
        }
    }

    fn generics(&mut self, g: &Generics) {
        // Not the parser's own (`rethrows`' `#E0`).
        let params: Vec<&GenericParam> = g
            .params
            .iter()
            .filter(|gp| match gp {
                GenericParam::Type { name, .. } | GenericParam::Const { name, .. } => {
                    !name.starts_with('#')
                }
            })
            .collect();
        if params.is_empty() {
            return;
        }
        self.s("<");
        self.list(&params, ", ", |p, gp| match gp {
            GenericParam::Type {
                name,
                bounds,
                default,
            } => {
                p.s(name);
                if !bounds.is_empty() {
                    p.s(": ");
                    p.list(bounds, " + ", Self::ty);
                }
                if let Some(d) = default {
                    p.s(" = ");
                    p.ty(d);
                }
            }
            GenericParam::Const { name, ty } => {
                p.s("const ");
                p.s(name);
                p.s(": ");
                p.ty(ty);
            }
        });
        self.s(">");
    }

    fn where_clause(&mut self, preds: &[WherePred]) {
        if preds.is_empty() {
            return;
        }
        self.s(" where ");
        self.list(preds, ", ", |p, w| {
            p.ty(&w.ty);
            p.s(": ");
            p.list(&w.bounds, " + ", Self::ty);
        });
    }

    fn ty(&mut self, t: &Ty) {
        match &t.kind {
            TyKind::Path(p) => self.ty_path(p),
            TyKind::QPath(q) => self.qpath(q),
            TyKind::Sum(members) => {
                // `T?` is `#nest<T> | none` (spec 2.6.5).
                if let [inner, none] = members.as_slice() {
                    if let (Some(x), TyKind::None) = (inner.nest_inner(), &none.kind) {
                        self.ty_tight(x);
                        self.s("?");
                        return;
                    }
                }
                self.list(members, " | ", Self::ty);
            }
            TyKind::Tuple(tys) => {
                self.s("(");
                self.list(tys, ", ", Self::ty);
                if tys.len() == 1 {
                    self.s(",");
                }
                self.s(")");
            }
            TyKind::Fn(args, ret, bounds) => {
                if bounds.unsafe_fn {
                    self.s("unsafe ");
                }
                if bounds.system {
                    self.s("extern \"system\" ");
                } else if bounds.c_abi {
                    self.s("extern \"C\" ");
                }
                self.s("fn(");
                self.list(args, ", ", Self::ty);
                self.s(")");
                let ret = match &ret.kind {
                    TyKind::Throws { ok, err } if is_untyped_err(err) || is_rethrows_err(err) => {
                        self.s(" throws");
                        ok
                    }
                    TyKind::Throws { ok, err } => {
                        self.s(" throws (");
                        self.ty(err);
                        self.s(")");
                        ok
                    }
                    _ => ret,
                };
                self.s(" -> ");
                self.ty_tight(ret);
                for b in &bounds.auto {
                    self.s(" + ");
                    self.ty_path(b);
                }
            }
            TyKind::Throws { ok, err } => {
                // Only a fn type's return slot; standalone for completeness.
                self.s("throws (");
                self.ty(err);
                self.s(") -> ");
                self.ty_tight(ok);
            }
            TyKind::Array(elem, len) => {
                self.s("[");
                self.ty(elem);
                self.s("; ");
                self.const_expr(len);
                self.s("]");
            }
            TyKind::Any => self.s("any"),
            TyKind::AnyTrait(bounds) => {
                self.s("any ");
                if bounds.len() == 1 {
                    self.ty_path(&bounds[0]);
                } else {
                    self.s("(");
                    self.list(bounds, " + ", Self::ty_path);
                    self.s(")");
                }
            }
            TyKind::None => self.s("none"),
            TyKind::SelfUpper => self.s("Self"),
            TyKind::Never => self.s("!"),
            TyKind::Nest(x) => self.ty_tight(x),
            TyKind::Binding(name, t) => {
                self.s(name);
                self.s(" = ");
                self.ty(t);
            }
            TyKind::Const(c) => match c {
                ConstExpr::Expr(e) => {
                    self.s("{ ");
                    self.expr(e);
                    self.s(" }");
                }
                c => self.const_expr(c),
            },
        }
    }

    /// A type where `|` / `->` would read wrong unless grouped.
    fn ty_tight(&mut self, t: &Ty) {
        let group = match &t.kind {
            TyKind::Sum(m) => !(m.len() == 2 && m[0].nest_inner().is_some()),
            TyKind::Fn(..) | TyKind::AnyTrait(_) => true,
            _ => false,
        };
        if group {
            self.s("(");
            self.ty(t);
            self.s(")");
        } else {
            self.ty(t);
        }
    }

    fn ty_path(&mut self, p: &Path) {
        self.path(p, false);
    }

    /// `<T as Tr>::item`.
    fn qpath(&mut self, q: &crate::QPath) {
        self.s("<");
        self.ty_tight(&q.self_ty);
        self.s(" as ");
        self.ty_path(&q.trait_path);
        self.s(">::");
        self.name(&q.item);
        if let Some(m) = &q.member {
            self.s("::");
            self.name(m);
        }
    }

    /// `a::b<T>` (types, struct literals) or `a::b::<T>` (`turbofish`,
    /// expressions).
    fn path(&mut self, p: &Path, turbofish: bool) {
        for (i, seg) in p.segments.iter().enumerate() {
            if i > 0 {
                self.s("::");
            }
            self.name(&seg.name);
            if !seg.generics.is_empty() {
                self.s(if turbofish { "::<" } else { "<" });
                let at = self.out.len();
                self.list(&seg.generics, ", ", Self::ty);
                // `< <T as Tr>::X>`: `<<` is a shift.
                if self.out[at..].starts_with('<') {
                    self.out.insert(at, ' ');
                }
                self.s(">");
            }
        }
    }

    fn const_expr(&mut self, c: &ConstExpr) {
        match c {
            ConstExpr::Literal(l) => self.lit(l),
            ConstExpr::Path(p) => self.path(p, true),
            ConstExpr::Expr(e) => self.expr(e),
        }
    }

    fn block(&mut self, b: &Block) {
        if let Some(l) = &b.label {
            let _ = write!(self.out, "'{l}: ");
        }
        if b.stmts.is_empty() && b.trailing.is_none() {
            self.s("{}");
            return;
        }
        self.s("{");
        self.indent += 1;
        for s in &b.stmts {
            self.nl();
            self.stmt(s);
        }
        if let Some(t) = &b.trailing {
            self.nl();
            self.stmt_expr(t);
        }
        self.indent -= 1;
        self.nl();
        self.s("}");
    }

    /// An expression starting a statement: a `Catch` around a block
    /// would end at the block, so it is grouped.
    fn stmt_expr(&mut self, e: &Expr) {
        if matches!(&e.kind, ExprKind::Catch { .. }) {
            self.s("(");
            self.expr(e);
            self.s(")");
        } else {
            self.expr(e);
        }
    }

    fn let_tail(&mut self, ty: Option<&Ty>, init: &Expr) {
        if let Some(t) = ty {
            self.s(": ");
            self.ty(t);
        }
        self.s(" = ");
        self.expr(init);
        self.s(";");
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { name, ty, init } => {
                self.s("let ");
                self.name(name);
                self.let_tail(ty.as_ref(), init);
            }
            StmtKind::LetTuple {
                names,
                rest,
                ty,
                init,
            } => {
                let mut parts: Vec<String> = names.clone();
                if let Some((at, n)) = rest {
                    let r = format!("..{}", n.as_deref().unwrap_or(""));
                    parts.insert((*at).min(parts.len()), r);
                }
                self.s("let (");
                self.s(&parts.join(", "));
                if parts.len() == 1 && rest.is_none() {
                    self.s(",");
                }
                self.s(")");
                self.let_tail(ty.as_ref(), init);
            }
            StmtKind::LetStruct {
                path,
                fields,
                rest,
                ty,
                init,
            } => {
                self.s("let ");
                self.ty_path(path);
                self.s(" { ");
                let mut parts: Vec<String> = fields
                    .iter()
                    .map(|(f, b)| {
                        if f == b {
                            f.clone()
                        } else {
                            format!("{f}: {b}")
                        }
                    })
                    .collect();
                if *rest {
                    parts.push("..".to_owned());
                }
                self.s(&parts.join(", "));
                self.s(" }");
                self.let_tail(ty.as_ref(), init);
            }
            StmtKind::LetPattern { pat, ty, init } => {
                self.s("let ");
                self.pat(pat);
                self.let_tail(ty.as_ref(), init);
            }
            StmtKind::Assign(target, value) => {
                self.stmt_expr(target);
                self.s(" = ");
                self.expr(value);
                self.s(";");
            }
            StmtKind::CompoundAssign { target, op, value } => {
                self.stmt_expr(target);
                let _ = write!(self.out, " {op} ");
                self.expr(value);
                self.s(";");
            }
            StmtKind::Item(Item::Fn(f)) if crate::nested_marker_of(f).is_some() => {
                if let Some((lifted, false)) = crate::nested_marker_of(f) {
                    if let Some(mut def) = self.lifted.get(lifted).cloned() {
                        def.name.clone_from(&f.name);
                        self.fn_def(&def);
                    }
                }
            }
            // A block's `use` copied into a lifted fn's body (spec 3.9.1).
            StmtKind::Item(Item::Use(u))
                if u.attrs
                    .iter()
                    .any(|a| a.path == [crate::INHERITED_USE_ATTR]) => {}
            StmtKind::Item(item) => self.item(item),
            StmtKind::Expr(e) => {
                self.stmt_expr(e);
                self.s(";");
            }
            StmtKind::Trailing(e) => {
                self.stmt_expr(e);
                // Grouped, it no longer ends at its block.
                if matches!(&e.kind, ExprKind::Catch { .. }) {
                    self.s(";");
                }
            }
            StmtKind::Return(v) => {
                self.s("return");
                if let Some(l) = &self.ret_label {
                    let _ = write!(self.out, "@{l}");
                }
                if let Some(v) = v {
                    self.s(" ");
                    self.expr(v);
                }
                self.s(";");
            }
            StmtKind::Break { label, value } => {
                self.s("break");
                if let Some(l) = label {
                    let _ = write!(self.out, " '{l}");
                }
                if let Some(v) = value {
                    self.s(" ");
                    self.expr(v);
                }
                self.s(";");
            }
            StmtKind::Continue { label } => {
                self.s("continue");
                if let Some(l) = label {
                    let _ = write!(self.out, " '{l}");
                }
                self.s(";");
            }
            StmtKind::Throw(e) => {
                self.s("throw ");
                self.expr(e);
                self.s(";");
            }
        }
    }

    /// `e` as an operand: grouped unless it is atomic.
    fn operand(&mut self, e: &Expr) {
        if atomic(e) {
            self.expr(e);
        } else {
            self.s("(");
            self.expr(e);
            self.s(")");
        }
    }

    /// The head of `if` / `while` / `match` / `for`: a `{` in it would
    /// be taken for the body, so such a head is grouped.
    fn head(&mut self, e: &Expr) {
        self.grouped_if(e, |s| s.contains('{'));
    }

    fn grouped(&mut self, e: &Expr) {
        self.s("(");
        self.expr(e);
        self.s(")");
    }

    /// `e`, grouped when its text meets `group`.
    fn grouped_if(&mut self, e: &Expr, group: impl Fn(&str) -> bool) {
        let mut p = Self {
            out: String::new(),
            indent: self.indent,
            lifted: self.lifted.clone(),
            ret_label: self.ret_label.clone(),
            closure_label: None,
        };
        p.expr(e);
        if group(&p.out) {
            self.s("(");
            self.s(&p.out);
            self.s(")");
        } else {
            self.s(&p.out);
        }
    }

    fn args(&mut self, args: &[CallArg]) {
        self.s("(");
        self.list(args, ", ", |p, a| match a {
            CallArg::Positional(e) => p.expr(e),
            CallArg::Named { name, value } => {
                p.s(name);
                p.s(": ");
                p.expr(value);
            }
            CallArg::Spread(e) => {
                p.s("..");
                p.operand(e);
            }
            CallArg::Ref(e) => {
                p.s("ref ");
                p.expr(e);
            }
        });
        self.s(")");
    }

    /// A trailing closure of a call to `label`.
    fn trailing(&mut self, closure: Option<&Expr>, label: Option<&str>) {
        let Some(c) = closure else { return };
        self.s(" ");
        let label = label.map(str::to_owned);
        // `f() { .. }`: after a call `||` would be logical or.
        if let ExprKind::Closure {
            captures,
            params,
            ret_ty: None,
            body,
            is_async: false,
            is_generator: false,
        } = &c.kind
        {
            if let (true, true, ExprKind::Block(b)) =
                (captures.is_empty(), params.is_empty(), &body.kind)
            {
                let outer = std::mem::replace(&mut self.ret_label, label);
                self.block(b);
                self.ret_label = outer;
                return;
            }
        }
        self.closure_label = label;
        self.expr(c);
        self.closure_label = None;
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // one arm per expression form
    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Literal(l) => self.lit(l),
            ExprKind::Path(p) => self.path(p, true),
            ExprKind::QPath(q) => self.qpath(q),
            ExprKind::SelfLower => self.s("self"),
            ExprKind::SelfUpper => self.s("Self"),
            ExprKind::Tuple(xs) => {
                self.s("(");
                self.list(xs, ", ", Self::expr);
                if xs.len() == 1 {
                    self.s(",");
                }
                self.s(")");
            }
            ExprKind::List(xs) => {
                self.s("[");
                self.list(xs, ", ", Self::expr);
                self.s("]");
            }
            ExprKind::ArrayRepeat { value, len } => {
                self.s("[");
                self.expr(value);
                self.s("; ");
                self.const_expr(len);
                self.s("]");
            }
            ExprKind::Dict(pairs) => {
                self.s("{");
                self.list(pairs, ", ", |p, (k, v)| {
                    p.expr(k);
                    p.s(": ");
                    p.expr(v);
                });
                self.s("}");
            }
            ExprKind::Set(xs) => {
                self.s("{");
                self.list(xs, ", ", Self::expr);
                if xs.len() == 1 {
                    self.s(",");
                }
                self.s("}");
            }
            ExprKind::MagicLiteral(cands) => {
                // The handler call the first candidate stands for.
                if let Some(c) = cands.first() {
                    self.path(&c.handler, true);
                    self.s("(");
                    self.list(&c.args, ", ", |p, a| match a {
                        LitArg::One(e) => p.expr(e),
                        LitArg::Many(es) => {
                            p.s("[");
                            p.list(es, ", ", Self::expr);
                            p.s("]");
                        }
                    });
                    self.s(")");
                }
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Unsafe(b) => {
                self.s("unsafe ");
                self.block(b);
            }
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.s("if ");
                self.head(cond);
                self.s(" ");
                self.block(then_branch);
                if let Some(e) = else_branch {
                    self.s(" else ");
                    match &e.kind {
                        ExprKind::If { .. } | ExprKind::Block(_) => self.expr(e),
                        _ => {
                            self.s("{ ");
                            self.expr(e);
                            self.s(" }");
                        }
                    }
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.s("match ");
                self.head(scrutinee);
                self.s(" ");
                self.braced(arms, |p, arm| {
                    p.pat(&arm.pat);
                    if let Some(g) = &arm.guard {
                        p.s(" if ");
                        p.expr(g);
                    }
                    p.s(" => ");
                    p.expr(&arm.body);
                    p.s(",");
                });
            }
            ExprKind::Loop { label, body } => {
                if let Some(l) = label {
                    let _ = write!(self.out, "'{l}: ");
                }
                self.s("loop ");
                self.block(body);
            }
            ExprKind::While { label, cond, body } => {
                if let Some(l) = label {
                    let _ = write!(self.out, "'{l}: ");
                }
                self.s("while ");
                self.head(cond);
                self.s(" ");
                self.block(body);
            }
            ExprKind::For {
                label,
                pat,
                iter,
                body,
                is_await,
            } => {
                if let Some(l) = label {
                    let _ = write!(self.out, "'{l}: ");
                }
                self.s(if *is_await { "for await " } else { "for " });
                self.pat(pat);
                self.s(" in ");
                self.head(iter);
                self.s(" ");
                self.block(body);
            }
            ExprKind::Try { kind, operand } => {
                self.s(match kind {
                    TryKind::Plain => "try ",
                    TryKind::Optional => "try? ",
                    TryKind::Force => "try! ",
                });
                self.expr(operand);
            }
            ExprKind::Catch {
                body,
                catches,
                finally,
            } => {
                if matches!(body.kind, ExprKind::Block(_)) {
                    self.operand(body);
                } else {
                    self.expr(body);
                }
                for c in catches {
                    self.s(" catch ");
                    if !matches!(c.pat.kind, PatternKind::Wildcard) {
                        self.pat(&c.pat);
                        self.s(" ");
                    }
                    self.block(&c.body);
                }
                if let Some(f) = finally {
                    self.s(" finally ");
                    self.block(f);
                }
            }
            ExprKind::OpInfix { op, lhs, rhs } => {
                // A left-associative chain (`1 + 1 + 1`) prints flat: its
                // nesting would otherwise come back as groups.
                const LEFT: &[&str] = &[
                    "+", "-", "*", "/", "%", "%%", "&", "|", "^", "<<", ">>", "&&", "||",
                ];
                // A `|` after a call's `)` opens a trailing closure.
                let pipe = op == "|";
                match &lhs.kind {
                    ExprKind::OpInfix {
                        op: inner, rhs: r, ..
                    } if inner == op
                        && LEFT.contains(&op.as_str())
                        && !(pipe && ends_in_call(r)) =>
                    {
                        self.expr(lhs);
                    }
                    _ if pipe && ends_in_call(lhs) => self.grouped(lhs),
                    _ => self.operand(lhs),
                }
                let _ = write!(self.out, " {op} ");
                // `f | x | {..}`: a callee, `|x|` and a `{` read as a
                // paren-less trailing closure.
                if pipe && atomic(rhs) {
                    self.grouped_if(rhs, |s| s.starts_with('{'));
                } else {
                    self.operand(rhs);
                }
            }
            ExprKind::OpPrefix { op, operand } => {
                self.s(op);
                self.operand(operand);
            }
            ExprKind::OpPostfix { op, operand } => {
                self.operand(operand);
                self.s(op);
            }
            ExprKind::Cast(x, t) => {
                self.operand(x);
                self.s(" as ");
                self.ty_tight(t);
            }
            ExprKind::Is(x, pat) => {
                self.operand(x);
                self.s(" is ");
                match &pat.kind {
                    PatternKind::TypeCheck { binder: None, ty } => self.ty_tight(ty),
                    _ => self.pat(pat),
                }
            }
            ExprKind::Field(x, f) => {
                self.receiver(x);
                self.s(".");
                self.s(f);
            }
            ExprKind::TupleField(x, i) => {
                self.receiver(x);
                let _ = write!(self.out, ".{i}");
            }
            ExprKind::MethodCall {
                receiver,
                method,
                type_args,
                args,
                trailing_closure,
                ..
            } => {
                self.receiver(receiver);
                self.s(".");
                // `using`'s release call `x.#drop()` (spec 3.17).
                self.s(method.strip_prefix('#').unwrap_or(method));
                if !type_args.is_empty() {
                    self.s("::<");
                    self.list(type_args, ", ", Self::ty);
                    self.s(">");
                }
                self.args(args);
                self.trailing(trailing_closure.as_deref(), Some(method));
            }
            ExprKind::Call {
                callee,
                args,
                trailing_closure,
            } => {
                self.receiver(callee);
                self.args(args);
                let label = match &callee.kind {
                    ExprKind::Path(p) => p.segments.last().map(|s| s.name.as_str()),
                    _ => None,
                };
                self.trailing(trailing_closure.as_deref(), label);
            }
            ExprKind::Broadcast { callee, args } => {
                self.receiver(callee);
                self.s(".");
                self.args(args);
            }
            ExprKind::Index(x, i) => {
                self.receiver(x);
                self.s("[");
                self.expr(i);
                self.s("]");
            }
            ExprKind::Slice {
                target,
                start,
                end,
                step,
            } => {
                self.receiver(target);
                self.s("[");
                if let Some(s) = start {
                    self.expr(s);
                }
                self.s(":");
                if let Some(e) = end {
                    self.expr(e);
                }
                if let Some(st) = step {
                    self.s(":");
                    self.expr(st);
                }
                self.s("]");
            }
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                if let Some(s) = start {
                    self.operand(s);
                }
                self.s(if *inclusive { "..=" } else { ".." });
                if let Some(e) = end {
                    self.operand(e);
                }
            }
            ExprKind::QuestionDot(x, tails) => {
                if matches!(x.kind, ExprKind::QuestionDot(..)) {
                    self.operand(x);
                } else {
                    self.receiver(x);
                }
                for (k, tail) in tails.iter().enumerate() {
                    let dot = if k == 0 { "?." } else { "." };
                    match tail {
                        AccessTail::Field(f) => {
                            self.s(dot);
                            self.s(f);
                        }
                        AccessTail::TupleField(i) => {
                            self.s(dot);
                            let _ = write!(self.out, "{i}");
                        }
                        AccessTail::Method {
                            method,
                            type_args,
                            args,
                        } => {
                            self.s(dot);
                            self.s(method);
                            if !type_args.is_empty() {
                                self.s("::<");
                                self.list(type_args, ", ", Self::ty);
                                self.s(">");
                            }
                            self.args(args);
                        }
                        AccessTail::Index(i) => {
                            self.s("[");
                            self.expr(i);
                            self.s("]");
                        }
                    }
                }
            }
            ExprKind::NullishCoalesce(a, b) => {
                self.operand(a);
                self.s(" ?? ");
                self.operand(b);
            }
            ExprKind::TryQ(x) => {
                self.receiver(x);
                self.s("?");
            }
            ExprKind::Closure {
                captures,
                params,
                ret_ty,
                body,
                is_async,
                is_generator,
            } => {
                let label = self.closure_label.take();
                if *is_async {
                    self.s("async ");
                }
                if *is_generator {
                    self.s("yield ");
                }
                if !captures.is_empty() {
                    self.s("[");
                    self.list(captures, ", ", |p, c| match c {
                        CaptureSpec::Value(n) => p.s(n),
                        CaptureSpec::Ref(n) => {
                            p.s("ref ");
                            p.s(n);
                        }
                    });
                    self.s("] ");
                }
                self.s("|");
                self.list(params, ", ", Self::param);
                self.s("| ");
                if let Some(r) = ret_ty {
                    self.s("-> ");
                    self.ty(r);
                    self.s(" ");
                }
                let outer = std::mem::replace(&mut self.ret_label, label);
                self.expr(body);
                self.ret_label = outer;
            }
            ExprKind::Await(x) => {
                self.s("await ");
                self.operand(x);
            }
            ExprKind::InModule(_, x) => self.expr(x),
            ExprKind::Yield(v) => {
                self.s("yield");
                if let Some(v) = v {
                    self.s(" ");
                    self.expr(v);
                }
            }
            ExprKind::AsyncBlock(b) => {
                self.s("async ");
                self.block(b);
            }
            ExprKind::YieldBlock(b) => {
                self.s("yield ");
                self.block(b);
            }
            ExprKind::AsyncYieldBlock(b) => {
                self.s("async yield ");
                self.block(b);
            }
            ExprKind::StructInit {
                path,
                fields,
                spread,
            } => {
                // `W::<T> { .. }`: in an expression `W<T` is a comparison.
                self.path(path, true);
                if fields.is_empty() && spread.is_none() {
                    self.s(" {}");
                    return;
                }
                self.s(" { ");
                self.list(fields, ", ", |p, (name, value)| {
                    let shorthand = matches!(&value.kind, ExprKind::Path(v)
                        if v.segments.len() == 1
                            && v.segments[0].name == *name
                            && v.segments[0].generics.is_empty());
                    p.s(name);
                    if !shorthand {
                        p.s(": ");
                        p.expr(value);
                    }
                });
                if let Some(s) = spread {
                    self.s(if fields.is_empty() { ".." } else { ", .." });
                    self.expr(s);
                }
                self.s(" }");
            }
            ExprKind::FString(parts) => {
                let open = self.out.len();
                self.s("f\"");
                for part in parts {
                    match part {
                        FStringPart::Lit(t) => {
                            // A raw `"` (from `f"""`) is escaped.
                            let mut esc = false;
                            for c in t.chars() {
                                match c {
                                    '"' if !esc => self.s("\\\""),
                                    '{' => self.s("{{"),
                                    '}' => self.s("}}"),
                                    _ => self.out.push(c),
                                }
                                esc = c == '\\' && !esc;
                            }
                        }
                        FStringPart::Hole { expr, spec } => {
                            self.s("{");
                            let at = self.out.len();
                            self.expr(expr);
                            if self.out[at..].starts_with('{') {
                                self.out.insert(at, ' '); // not `{{`
                            }
                            if !spec.is_empty() {
                                self.s(":");
                                for piece in spec {
                                    match piece {
                                        SpecPiece::Text(t) => self.s(&t.replace('$', "$$")),
                                        SpecPiece::Expr(e) => {
                                            self.s("${");
                                            self.expr(e);
                                            self.s("}");
                                        }
                                    }
                                }
                            }
                            self.s("}");
                        }
                    }
                }
                // Hole code printed across lines needs `f"""`.
                if self.out[open..].contains('\n') {
                    self.out.insert_str(open + 2, "\"\"");
                    self.s("\"\"");
                }
                self.s("\"");
            }
        }
    }

    /// The receiver of `.f` / `(..)` / `[..]` / `?`: number literals and
    /// `?.` chains (spec 3.8.8) are grouped.
    fn receiver(&mut self, e: &Expr) {
        if matches!(
            &e.kind,
            ExprKind::Literal(Literal::Int(_) | Literal::Float(_)) | ExprKind::QuestionDot(..)
        ) {
            self.s("(");
            self.expr(e);
            self.s(")");
        } else {
            self.operand(e);
        }
    }

    fn lit(&mut self, l: &Literal) {
        match l {
            Literal::Int(t) | Literal::Float(t) => self.s(t),
            Literal::Bool(b) => self.s(if *b { "true" } else { "false" }),
            Literal::Str(s) => {
                let _ = write!(self.out, "\"{s}\"");
            }
            Literal::RawStr(s) => {
                let mut hashes = String::from("#");
                while s.contains(&format!("\"{hashes}")) {
                    hashes.push('#');
                }
                let _ = write!(self.out, "r{hashes}\"{s}\"{hashes}");
            }
            Literal::ByteStr(s) => {
                let _ = write!(self.out, "b\"{s}\"");
            }
            Literal::Char(c) => {
                let _ = write!(self.out, "'{c}'");
            }
            Literal::CChar(c) => {
                let _ = write!(self.out, "c'{c}'");
            }
            Literal::None => self.s("none"),
        }
    }

    fn pat(&mut self, p: &Pattern) {
        match &p.kind {
            PatternKind::Wildcard => self.s("_"),
            PatternKind::Binding { name, sub } => {
                self.name(name);
                if let Some(sub) = sub {
                    self.s(" @ ");
                    self.pat(sub);
                }
            }
            PatternKind::Literal(l) => self.lit(l),
            PatternKind::Range {
                start,
                end,
                inclusive,
            } => {
                if let Some(s) = start {
                    self.lit(s);
                }
                self.s(if *inclusive { "..=" } else { ".." });
                if let Some(e) = end {
                    self.lit(e);
                }
            }
            PatternKind::TypeCheck { binder, ty } => {
                self.s(binder.as_deref().unwrap_or("_"));
                self.s(": ");
                self.ty_tight(ty);
            }
            PatternKind::Tuple(ps) => {
                self.s("(");
                self.list(ps, ", ", Self::pat);
                if ps.len() == 1 {
                    self.s(",");
                }
                self.s(")");
            }
            PatternKind::Rest(n) => {
                self.s("..");
                self.s(n.as_deref().unwrap_or(""));
            }
            PatternKind::Struct { path, fields, rest } => {
                self.ty_path(path);
                self.s(" { ");
                self.list(fields, ", ", |p, f| {
                    p.s(&f.name);
                    if let Some(sub) = &f.pat {
                        p.s(": ");
                        p.pat(sub);
                    }
                });
                if *rest {
                    self.s(if fields.is_empty() { ".." } else { ", .." });
                }
                self.s(" }");
            }
            PatternKind::EnumVariant { path, args } => {
                self.path(path, true);
                if !args.is_empty() || path.segments.len() == 1 {
                    self.s("(");
                    self.list(args, ", ", Self::pat);
                    self.s(")");
                }
            }
            // `(n: i32) | (m: i64)`: a type pattern's type would take the `|`.
            PatternKind::Or(alts) => self.list(alts, " | ", |p, a| {
                if matches!(a.kind, PatternKind::TypeCheck { .. }) {
                    p.s("(");
                    p.pat(a);
                    p.s(")");
                } else {
                    p.pat(a);
                }
            }),
        }
    }
}

/// A name spelled like a keyword prints as `r#name`.
fn raw(n: &str) -> bool {
    n != "self" && n != "Self" && rokc_core::KEYWORDS.contains(&n)
}

/// The single-segment name of a path type.
fn ty_name(t: &Ty) -> Option<&str> {
    match &t.kind {
        TyKind::Path(p) if p.segments.len() == 1 && p.segments[0].generics.is_empty() => {
            Some(&p.segments[0].name)
        }
        _ => None,
    }
}

/// The error of a `throws` written without a type.
fn is_untyped_err(t: &Ty) -> bool {
    crate::is_untyped_throws(t)
}

/// A `rethrows` error parameter (`#E0`, ..).
fn is_rethrows_err(t: &Ty) -> bool {
    ty_name(t).is_some_and(|n| n.starts_with("#E"))
}

/// An operand whose print ends in a call's `)`.
fn ends_in_call(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::MagicLiteral(_) => {
            atomic(e)
        }
        // The `?.` link itself takes no trailing closure; a later `.m()` does.
        ExprKind::QuestionDot(_, tails) => {
            tails.len() > 1 && matches!(tails.last(), Some(AccessTail::Method { .. }))
        }
        _ => false,
    }
}

/// Needs no parentheses as an operand.
const fn atomic(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Literal(_)
            | ExprKind::Path(_)
            | ExprKind::SelfLower
            | ExprKind::SelfUpper
            | ExprKind::Tuple(_)
            | ExprKind::List(_)
            | ExprKind::ArrayRepeat { .. }
            | ExprKind::Dict(_)
            | ExprKind::Set(_)
            | ExprKind::MagicLiteral(_)
            | ExprKind::Field(..)
            | ExprKind::TupleField(..)
            | ExprKind::MethodCall {
                trailing_closure: None,
                ..
            }
            | ExprKind::Broadcast { .. }
            | ExprKind::Call {
                trailing_closure: None,
                ..
            }
            | ExprKind::Index(..)
            | ExprKind::Slice { .. }
            | ExprKind::QuestionDot(..)
            | ExprKind::TryQ(_)
            | ExprKind::StructInit { .. }
            | ExprKind::FString(_)
    )
}
