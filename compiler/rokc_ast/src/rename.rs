//! Renaming block-local items.
//!
//! A block's item is lifted under a name no source can write and every use
//! in scope is renamed. Values (`static`, unit / tuple structs) are renamed
//! at bindings too, so shadowing still works. Fields, methods and labels
//! are never renamed.
//!
//! The same walk resolves a block item's `use`s (`paths`) outside fn and
//! closure bodies (`headers_only`; the parser copies the `use`s into
//! those).

use std::collections::HashMap;

use crate::{
    AccessTail, Block, CallArg, CaptureSpec, ConstExpr, EnumVariantKind, Expr, ExprKind,
    FStringPart, FnDef, GenericParam, Generics, ImplItem, Item, LitArg, Param, Path, Pattern,
    PatternKind, QPath, SpecPiece, Stmt, StmtKind, StructKind, TraitItem, Ty, TyKind, UseTree,
    UseTreeKind, WherePred,
};

/// The names a `use` tree binds (a glob's are unknown here).
pub fn use_names<'a>(t: &'a UseTree, out: &mut Vec<&'a str>) {
    match &t.kind {
        UseTreeKind::Simple(alias) => {
            let last = t.prefix.segments.last().map(|s| s.name.as_str());
            out.extend(alias.as_deref().or(last));
        }
        UseTreeKind::Group(ts) => ts.iter().for_each(|t| use_names(t, out)),
        UseTreeKind::Glob | UseTreeKind::GlobRecursive => {}
    }
}

/// The `(name, path)` pairs a `use` tree binds under `parent`, and the
/// prefixes of its globs.
pub fn use_paths(
    t: &UseTree,
    parent: &[String],
    out: &mut Vec<(String, Vec<String>)>,
    globs: &mut Vec<Vec<String>>,
) {
    let mut path = parent.to_vec();
    path.extend(t.prefix.segments.iter().map(|s| s.name.clone()));
    match &t.kind {
        UseTreeKind::Simple(alias) => {
            if let Some(name) = alias.clone().or_else(|| path.last().cloned()) {
                out.push((name, path));
            }
        }
        UseTreeKind::Group(ts) => ts.iter().for_each(|t| use_paths(t, &path, out, globs)),
        UseTreeKind::Glob => globs.push(path),
        UseTreeKind::GlobRecursive => {}
    }
}

/// The names to rename, by namespace, and the paths to put for names.
#[derive(Clone, Default, Debug)]
pub struct Renames {
    pub types: HashMap<String, String>,
    pub values: HashMap<String, String>,
    /// A name's path, in both namespaces (a `use`).
    pub paths: HashMap<String, Vec<String>>,
    /// Leave fn and closure bodies alone.
    pub headers_only: bool,
}

impl Renames {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.values.is_empty() && self.paths.is_empty()
    }

    fn has(&self, n: &str) -> bool {
        self.types.contains_key(n) || self.values.contains_key(n) || self.paths.contains_key(n)
    }

    /// `self` without `names`, if it has any of them.
    fn without<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let names: Vec<&str> = names.into_iter().filter(|n| self.has(n)).collect();
        if names.is_empty() {
            return None;
        }
        let mut inner = self.clone();
        for n in names {
            inner.types.remove(n);
            inner.values.remove(n);
            inner.paths.remove(n);
        }
        Some(inner)
    }

    /// A path's first segment `first` replaced by its `use` path.
    fn put_path(&self, p: &mut Path) {
        let Some(path) = p.segments.first().and_then(|s| self.paths.get(&s.name)) else {
            return;
        };
        let first = p.segments.remove(0);
        let mut new: Vec<crate::PathSegment> = path
            .iter()
            .map(|n| crate::PathSegment {
                name: n.clone(),
                generics: Vec::new(),
            })
            .collect();
        if let Some(last) = new.last_mut() {
            last.generics = first.generics;
        }
        new.append(&mut p.segments);
        p.segments = new;
    }

    fn ty(&self, n: &mut String) {
        if let Some(to) = self.types.get(n.as_str()) {
            n.clone_from(to);
        }
    }

    fn value(&self, n: &mut String) {
        if let Some(to) = self.values.get(n.as_str()) {
            n.clone_from(to);
        }
    }

    pub fn item(&self, item: &mut Item) {
        // A generic parameter of the item hides a `use` of its name.
        let own = match &*item {
            Item::Struct(x) => Some(&x.generics),
            Item::Enum(x) => Some(&x.generics),
            Item::Union(x) => Some(&x.generics),
            Item::Trait(x) => Some(&x.generics),
            Item::Impl(x) => Some(&x.generics),
            Item::TypeAlias(x) => Some(&x.generics),
            _ => None,
        };
        if let Some(inner) = own.and_then(|g| self.paths_without(g)) {
            return inner.item(item);
        }
        match item {
            Item::Fn(f) => {
                self.value(&mut f.name);
                self.fn_def(f);
            }
            Item::Struct(s) => {
                self.ty(&mut s.name);
                if !matches!(s.kind, StructKind::Named(_)) {
                    self.value(&mut s.name);
                }
                self.generics(&mut s.generics, &mut s.where_clause);
                match &mut s.kind {
                    StructKind::Unit => {}
                    StructKind::Tuple(fs) => fs.iter_mut().for_each(|f| self.ty_(&mut f.ty)),
                    StructKind::Named(fs) => {
                        for f in fs {
                            self.ty_(&mut f.ty);
                            if let Some(d) = &mut f.default {
                                self.expr(d);
                            }
                        }
                    }
                }
            }
            Item::Enum(e) => {
                self.ty(&mut e.name);
                self.generics(&mut e.generics, &mut e.where_clause);
                for v in &mut e.variants {
                    match &mut v.kind {
                        EnumVariantKind::Unit => {}
                        EnumVariantKind::Tuple(ts) => ts.iter_mut().for_each(|t| self.ty_(t)),
                        EnumVariantKind::Named(fs) => {
                            for f in fs {
                                self.ty_(&mut f.ty);
                                if let Some(d) = &mut f.default {
                                    self.expr(d);
                                }
                            }
                        }
                        EnumVariantKind::Value(x) => self.expr(x),
                    }
                }
            }
            Item::Union(u) => {
                self.ty(&mut u.name);
                self.generics(&mut u.generics, &mut u.where_clause);
                u.fields.iter_mut().for_each(|f| self.ty_(&mut f.ty));
            }
            Item::Trait(t) => {
                self.ty(&mut t.name);
                self.generics(&mut t.generics, &mut t.where_clause);
                t.supertraits.iter_mut().for_each(|b| self.ty_(b));
                for ti in &mut t.items {
                    match ti {
                        TraitItem::Method(m) => self.fn_def(m),
                        TraitItem::AssocType {
                            bounds, default, ..
                        } => {
                            bounds.iter_mut().for_each(|b| self.ty_(b));
                            if let Some(d) = default {
                                self.ty_(d);
                            }
                        }
                        TraitItem::Const { ty, default, .. } => {
                            self.ty_(ty);
                            if let Some(d) = default {
                                self.expr(d);
                            }
                        }
                    }
                }
            }
            Item::Impl(i) => {
                self.generics(&mut i.generics, &mut i.where_clause);
                if let Some(p) = &mut i.trait_path {
                    self.ty_path(p);
                }
                self.ty_(&mut i.self_ty);
                for ii in &mut i.items {
                    match ii {
                        ImplItem::Method(m) => self.fn_def(m),
                        ImplItem::AssocType { ty, .. } => self.ty_(ty),
                        ImplItem::Const(c) => {
                            self.ty_(&mut c.ty);
                            self.expr(&mut c.value);
                        }
                    }
                }
            }
            Item::Const(c) => {
                self.value(&mut c.name);
                self.ty_(&mut c.ty);
                self.expr(&mut c.value);
            }
            Item::Static(s) => {
                self.value(&mut s.name);
                self.ty_(&mut s.ty);
                if let Some(v) = &mut s.value {
                    self.expr(v);
                }
            }
            Item::TypeAlias(a) => {
                self.ty(&mut a.name);
                self.generics(&mut a.generics, &mut a.where_clause);
                self.ty_(&mut a.ty);
            }
            // `use E::A` of a local `enum E`.
            Item::Use(u) => {
                if let Some(first) = u.tree.prefix.segments.first_mut() {
                    self.ty(&mut first.name);
                }
            }
            Item::Mod(_) | Item::Extern(_) => {}
        }
    }

    fn generics(&self, g: &mut Generics, wc: &mut [WherePred]) {
        for p in &mut g.params {
            match p {
                GenericParam::Type {
                    name,
                    bounds,
                    default,
                } => {
                    self.ty(name);
                    bounds.iter_mut().for_each(|b| self.ty_(b));
                    if let Some(d) = default {
                        self.ty_(d);
                    }
                }
                GenericParam::Const { name, ty } => {
                    self.value(name);
                    self.ty_(ty);
                }
            }
        }
        for w in wc {
            self.ty_(&mut w.ty);
            w.bounds.iter_mut().for_each(|b| self.ty_(b));
        }
    }

    fn paths_without(&self, g: &Generics) -> Option<Self> {
        if self.paths.is_empty() {
            return None;
        }
        let names = g.params.iter().map(|p| match p {
            GenericParam::Type { name, .. } | GenericParam::Const { name, .. } => name.as_str(),
        });
        let mut inner = self.without(names.filter(|n| self.paths.contains_key(*n)))?;
        inner.types.clone_from(&self.types);
        inner.values.clone_from(&self.values);
        Some(inner)
    }

    fn fn_def(&self, f: &mut FnDef) {
        // A nested fn's marker names the lifted fn; it holds nothing else.
        if crate::nested_marker_of(f).is_some() {
            return;
        }
        if let Some(inner) = self.paths_without(&f.generics) {
            return inner.fn_def(f);
        }
        self.generics(&mut f.generics, &mut f.where_clause);
        self.params(&mut f.params);
        if let Some(t) = &mut f.ret_ty {
            self.ty_(t);
        }
        if let crate::ThrowsKind::Typed(ts) = &mut f.throws {
            ts.iter_mut().for_each(|t| self.ty_(t));
        }
        if let Some(b) = f.body.as_mut().filter(|_| !self.headers_only) {
            self.block(b);
        }
    }

    fn params(&self, ps: &mut [Param]) {
        for p in ps {
            self.value(&mut p.name);
            if let Some(t) = &mut p.ty {
                self.ty_(t);
            }
            if let Some(d) = &mut p.default {
                self.expr(d);
            }
        }
    }

    pub fn block(&self, b: &mut Block) {
        let mut used = Vec::new();
        for s in &b.stmts {
            if let StmtKind::Item(Item::Use(u)) = &s.kind {
                use_names(&u.tree, &mut used);
            }
        }
        if let Some(inner) = self.without(used) {
            return inner.block(b);
        }
        b.stmts.iter_mut().for_each(|s| self.stmt(s));
        if let Some(t) = &mut b.trailing {
            self.expr(t);
        }
    }

    pub fn stmt(&self, s: &mut Stmt) {
        match &mut s.kind {
            StmtKind::Let { name, ty, init } => {
                self.value(name);
                if let Some(t) = ty {
                    self.ty_(t);
                }
                self.expr(init);
            }
            StmtKind::LetTuple {
                names,
                rest,
                ty,
                init,
            } => {
                names.iter_mut().for_each(|n| self.value(n));
                if let Some((_, Some(r))) = rest {
                    self.value(r);
                }
                if let Some(t) = ty {
                    self.ty_(t);
                }
                self.expr(init);
            }
            StmtKind::LetStruct {
                path,
                fields,
                ty,
                init,
                ..
            } => {
                self.ty_path(path);
                fields.iter_mut().for_each(|(_, b)| self.value(b));
                if let Some(t) = ty {
                    self.ty_(t);
                }
                self.expr(init);
            }
            StmtKind::LetPattern { pat, ty, init } => {
                self.pat(pat);
                if let Some(t) = ty {
                    self.ty_(t);
                }
                self.expr(init);
            }
            StmtKind::Assign(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Item(i) => self.item(i),
            StmtKind::Expr(e) | StmtKind::Trailing(e) | StmtKind::Throw(e) => self.expr(e),
            StmtKind::Return(e) | StmtKind::Break { value: e, .. } => {
                if let Some(e) = e {
                    self.expr(e);
                }
            }
            StmtKind::Continue { .. } => {}
        }
    }

    /// A path in the type namespace: its first segment, and its generic
    /// arguments.
    fn ty_path(&self, p: &mut Path) {
        if let Some(first) = p.segments.first_mut() {
            self.ty(&mut first.name);
        }
        self.put_path(p);
        self.path_args(p);
    }

    fn path_args(&self, p: &mut Path) {
        for s in &mut p.segments {
            s.generics.iter_mut().for_each(|g| self.ty_(g));
        }
    }

    fn qpath(&self, q: &mut QPath) {
        self.ty_(&mut q.self_ty);
        self.ty_path(&mut q.trait_path);
    }

    fn args(&self, args: &mut [CallArg]) {
        for a in args {
            match a {
                CallArg::Positional(e)
                | CallArg::Spread(e)
                | CallArg::Ref(e)
                | CallArg::Named { value: e, .. } => self.expr(e),
            }
        }
    }

    fn const_expr(&self, c: &mut ConstExpr) {
        match c {
            ConstExpr::Literal(_) => {}
            ConstExpr::Path(p) => self.value_path(p),
            ConstExpr::Expr(e) => self.expr(e),
        }
    }

    /// A path in an expression: a lone name is a value, a longer path
    /// starts with a type (`S::new`, `E::A`).
    fn value_path(&self, p: &mut Path) {
        match p.segments.as_mut_slice() {
            [one] => self.value(&mut one.name),
            [first, ..] => self.ty(&mut first.name),
            [] => {}
        }
        self.put_path(p);
        self.path_args(p);
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // one arm per expression form
    pub fn expr(&self, e: &mut Expr) {
        match &mut e.kind {
            ExprKind::Literal(_)
            | ExprKind::SelfLower
            | ExprKind::SelfUpper
            | ExprKind::InModule(..) => {}
            ExprKind::Path(p) => self.value_path(p),
            ExprKind::QPath(q) => self.qpath(q),
            ExprKind::Tuple(xs) | ExprKind::List(xs) | ExprKind::Set(xs) => {
                xs.iter_mut().for_each(|x| self.expr(x));
            }
            ExprKind::ArrayRepeat { value, len } => {
                self.expr(value);
                self.const_expr(len);
            }
            ExprKind::Dict(kvs) => {
                for (k, v) in kvs {
                    self.expr(k);
                    self.expr(v);
                }
            }
            ExprKind::MagicLiteral(cands) => {
                for c in cands {
                    for a in &mut c.args {
                        match a {
                            LitArg::One(x) => self.expr(x),
                            LitArg::Many(xs) => xs.iter_mut().for_each(|x| self.expr(x)),
                        }
                    }
                }
            }
            ExprKind::Block(b)
            | ExprKind::Unsafe(b)
            | ExprKind::AsyncBlock(b)
            | ExprKind::YieldBlock(b)
            | ExprKind::AsyncYieldBlock(b)
            | ExprKind::Loop { body: b, .. } => self.block(b),
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond);
                self.block(then_branch);
                if let Some(x) = else_branch {
                    self.expr(x);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms {
                    self.pat(&mut a.pat);
                    if let Some(g) = &mut a.guard {
                        self.expr(g);
                    }
                    self.expr(&mut a.body);
                }
            }
            ExprKind::While { cond, body, .. } => {
                self.expr(cond);
                self.block(body);
            }
            ExprKind::For {
                pat, iter, body, ..
            } => {
                self.pat(pat);
                self.expr(iter);
                self.block(body);
            }
            ExprKind::Catch {
                body,
                catches,
                finally,
            } => {
                self.expr(body);
                for c in catches {
                    self.pat(&mut c.pat);
                    self.block(&mut c.body);
                }
                if let Some(f) = finally {
                    self.block(f);
                }
            }
            ExprKind::OpInfix { lhs, rhs, .. }
            | ExprKind::NullishCoalesce(lhs, rhs)
            | ExprKind::Index(lhs, rhs) => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Try { operand, .. }
            | ExprKind::OpPrefix { operand, .. }
            | ExprKind::OpPostfix { operand, .. }
            | ExprKind::Field(operand, _)
            | ExprKind::TupleField(operand, _)
            | ExprKind::TryQ(operand)
            | ExprKind::Await(operand) => self.expr(operand),
            ExprKind::Cast(x, t) => {
                self.expr(x);
                self.ty_(t);
            }
            ExprKind::Is(x, p) => {
                self.expr(x);
                self.pat(p);
            }
            ExprKind::MethodCall {
                receiver,
                type_args,
                args,
                trailing_closure,
                ..
            } => {
                self.expr(receiver);
                type_args.iter_mut().for_each(|t| self.ty_(t));
                self.args(args);
                if let Some(c) = trailing_closure {
                    self.expr(c);
                }
            }
            ExprKind::Call {
                callee,
                args,
                trailing_closure,
            } => {
                self.expr(callee);
                self.args(args);
                if let Some(c) = trailing_closure {
                    self.expr(c);
                }
            }
            ExprKind::Broadcast { callee, args } => {
                self.expr(callee);
                self.args(args);
            }
            ExprKind::Slice {
                target,
                start,
                end,
                step,
            } => {
                self.expr(target);
                for x in [start, end, step].into_iter().flatten() {
                    self.expr(x);
                }
            }
            ExprKind::Range { start, end, .. } => {
                for x in [start, end].into_iter().flatten() {
                    self.expr(x);
                }
            }
            ExprKind::QuestionDot(x, tails) => {
                self.expr(x);
                for t in tails {
                    match t {
                        AccessTail::Field(_) | AccessTail::TupleField(_) => {}
                        AccessTail::Method {
                            type_args, args, ..
                        } => {
                            type_args.iter_mut().for_each(|t| self.ty_(t));
                            self.args(args);
                        }
                        AccessTail::Index(i) => self.expr(i),
                    }
                }
            }
            ExprKind::Closure { .. } if self.headers_only => {}
            ExprKind::Closure {
                captures,
                params,
                ret_ty,
                body,
                ..
            } => {
                for c in captures {
                    match c {
                        CaptureSpec::Value(n) | CaptureSpec::Ref(n) => self.value(n),
                    }
                }
                self.params(params);
                if let Some(t) = ret_ty {
                    self.ty_(t);
                }
                self.expr(body);
            }
            ExprKind::Yield(x) => {
                if let Some(x) = x {
                    self.expr(x);
                }
            }
            ExprKind::StructInit {
                path,
                fields,
                spread,
            } => {
                self.ty_path(path);
                fields.iter_mut().for_each(|(_, x)| self.expr(x));
                if let Some(s) = spread {
                    self.expr(s);
                }
            }
            ExprKind::FString(parts) => {
                for p in parts {
                    if let FStringPart::Hole { expr, spec } = p {
                        self.expr(expr);
                        for s in spec {
                            if let SpecPiece::Expr(x) = s {
                                self.expr(x);
                            }
                        }
                    }
                }
            }
        }
    }

    fn pat(&self, p: &mut Pattern) {
        match &mut p.kind {
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::Range { .. } => {}
            PatternKind::Binding { name, sub } => {
                self.value(name);
                if let Some(s) = sub {
                    self.pat(s);
                }
            }
            PatternKind::TypeCheck { binder, ty } => {
                if let Some(b) = binder {
                    self.value(b);
                }
                self.ty_(ty);
            }
            PatternKind::Tuple(ps) | PatternKind::Or(ps) => ps.iter_mut().for_each(|x| self.pat(x)),
            PatternKind::Rest(n) => {
                if let Some(n) = n {
                    self.value(n);
                }
            }
            PatternKind::Struct { path, fields, .. } => {
                self.ty_path(path);
                for f in fields {
                    match &mut f.pat {
                        Some(x) => self.pat(x),
                        // `S { x }` binds `x`: spelled out when renamed.
                        None if self.values.contains_key(&f.name) => {
                            let mut name = f.name.clone();
                            self.value(&mut name);
                            f.pat = Some(Pattern {
                                kind: PatternKind::Binding { name, sub: None },
                                span: p.span,
                            });
                        }
                        None => {}
                    }
                }
            }
            PatternKind::EnumVariant { path, args } => {
                self.ty_path(path);
                args.iter_mut().for_each(|x| self.pat(x));
            }
        }
    }

    fn ty_(&self, t: &mut Ty) {
        match &mut t.kind {
            TyKind::Path(p) => self.ty_path(p),
            TyKind::QPath(q) => self.qpath(q),
            TyKind::Sum(ts) | TyKind::Tuple(ts) => ts.iter_mut().for_each(|x| self.ty_(x)),
            TyKind::Fn(args, ret, bounds) => {
                args.iter_mut().for_each(|x| self.ty_(x));
                self.ty_(ret);
                bounds.auto.iter_mut().for_each(|p| self.ty_path(p));
            }
            TyKind::Nest(x) | TyKind::Binding(_, x) => self.ty_(x),
            TyKind::Throws { ok, err } => {
                self.ty_(ok);
                self.ty_(err);
            }
            TyKind::Array(x, n) => {
                self.ty_(x);
                self.const_expr(n);
            }
            TyKind::AnyTrait(ps) => ps.iter_mut().for_each(|p| self.ty_path(p)),
            TyKind::Const(c) => self.const_expr(c),
            TyKind::Any | TyKind::None | TyKind::SelfUpper | TyKind::Never => {}
        }
    }
}
