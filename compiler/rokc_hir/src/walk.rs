//! One exhaustive pre-order AST walker for the lowering pre-passes and the
//! driver: exhaustive matches make a new node kind a compile error here,
//! not a skipped subtree.

use rokc_ast::{
    AccessTail, Block, CallArg, EnumVariantKind, Expr, ExprKind, FnDef, ImplItem, Item, Pattern,
    PatternKind, Stmt, StmtKind, StructKind, TraitItem, Ty, TyKind,
};

/// Callbacks for `walk_*`. Scope events follow lexical rules: a `let` binds
/// after its initializer; match / for / closure binders are scoped to
/// their body.
pub trait AstVisitor {
    fn expr(&mut self, _e: &Expr) {}
    /// Every path written: in expressions, types, patterns and an impl's
    /// trait.
    fn path(&mut self, _p: &rokc_ast::Path) {}
    fn stmt(&mut self, _s: &Stmt) {}
    /// A block, before its statements.
    fn block(&mut self, _b: &Block) {}
    fn ty(&mut self, _t: &Ty) {}
    fn bind(&mut self, _name: &str) {}
    /// `bind`, with the declaring construct's span (a `let` statement, a
    /// parameter, a pattern) - what identifies the binding.
    fn bind_at(&mut self, name: &str, _at: rokc_diagnostic::Span) {
        self.bind(name);
    }
    fn push_scope(&mut self) {}
    fn pop_scope(&mut self) {}
    /// Around a closure body or a coroutine (`async` / `yield`) block.
    fn enter_body(&mut self) {}
    fn leave_body(&mut self) {}
}

pub fn walk_item(v: &mut impl AstVisitor, item: &Item) {
    match item {
        Item::Fn(f) => walk_fn(v, f),
        Item::Struct(s) => match &s.kind {
            StructKind::Named(fields) => {
                for f in fields {
                    walk_ty(v, &f.ty);
                    if let Some(d) = &f.default {
                        walk_expr(v, d);
                    }
                }
            }
            StructKind::Tuple(fields) => {
                for f in fields {
                    walk_ty(v, &f.ty);
                }
            }
            StructKind::Unit => {}
        },
        Item::Enum(e) => {
            for var in &e.variants {
                match &var.kind {
                    EnumVariantKind::Unit => {}
                    EnumVariantKind::Tuple(tys) => {
                        for t in tys {
                            walk_ty(v, t);
                        }
                    }
                    EnumVariantKind::Named(fields) => {
                        for f in fields {
                            walk_ty(v, &f.ty);
                        }
                    }
                    EnumVariantKind::Value(e) => walk_expr(v, e),
                }
            }
        }
        Item::Union(u) => {
            for f in &u.fields {
                walk_ty(v, &f.ty);
            }
        }
        Item::Impl(impl_def) => {
            if let Some(t) = &impl_def.trait_path {
                v.path(t);
            }
            walk_ty(v, &impl_def.self_ty);
            for ii in &impl_def.items {
                match ii {
                    ImplItem::Method(m) => walk_fn(v, m),
                    ImplItem::AssocType { ty, .. } => walk_ty(v, ty),
                    ImplItem::Const(c) => {
                        walk_ty(v, &c.ty);
                        walk_expr(v, &c.value);
                    }
                }
            }
        }
        Item::Trait(t) => {
            for ti in &t.items {
                match ti {
                    TraitItem::Method(m) => walk_fn(v, m),
                    TraitItem::AssocType { .. } => {}
                    TraitItem::Const { ty, default, .. } => {
                        walk_ty(v, ty);
                        if let Some(d) = default {
                            walk_expr(v, d);
                        }
                    }
                }
            }
        }
        Item::Const(c) => {
            walk_ty(v, &c.ty);
            walk_expr(v, &c.value);
        }
        Item::Static(s) => {
            walk_ty(v, &s.ty);
            if let Some(e) = &s.value {
                walk_expr(v, e);
            }
        }
        Item::TypeAlias(a) => walk_ty(v, &a.ty),
        Item::Extern(blk) => {
            for ei in &blk.items {
                if let rokc_ast::ExternItem::Fn(f) = ei {
                    walk_fn(v, f);
                }
            }
        }
        Item::Use(_) | Item::Mod(_) => {}
    }
}

pub fn walk_fn(v: &mut impl AstVisitor, f: &FnDef) {
    v.push_scope();
    for p in &f.params {
        if let Some(t) = &p.ty {
            walk_ty(v, t);
        }
        if let Some(d) = &p.default {
            walk_expr(v, d);
        }
        v.bind_at(&p.name, p.span);
    }
    if let Some(t) = &f.ret_ty {
        walk_ty(v, t);
    }
    if let rokc_ast::ThrowsKind::Typed(tys) = &f.throws {
        for t in tys {
            walk_ty(v, t);
        }
    }
    if let Some(body) = &f.body {
        walk_block(v, body);
    }
    v.pop_scope();
}

pub fn walk_block(v: &mut impl AstVisitor, b: &Block) {
    v.block(b);
    v.push_scope();
    for s in &b.stmts {
        walk_stmt(v, s);
    }
    if let Some(t) = &b.trailing {
        walk_expr(v, t);
    }
    v.pop_scope();
}

pub fn walk_stmt(v: &mut impl AstVisitor, s: &Stmt) {
    v.stmt(s);
    match &s.kind {
        StmtKind::Let { name, ty, init } => {
            walk_expr(v, init);
            if let Some(t) = ty {
                walk_ty(v, t);
            }
            v.bind_at(name, s.span);
        }
        StmtKind::LetTuple {
            names,
            rest,
            ty,
            init,
        } => {
            walk_expr(v, init);
            if let Some(t) = ty {
                walk_ty(v, t);
            }
            for n in names.iter().filter(|n| *n != "_") {
                v.bind_at(n, s.span);
            }
            if let Some((_, Some(r))) = rest {
                v.bind_at(r, s.span);
            }
        }
        StmtKind::LetPattern { pat, ty, init } => {
            walk_expr(v, init);
            if let Some(t) = ty {
                walk_ty(v, t);
            }
            walk_pat(v, pat);
        }
        StmtKind::LetStruct {
            path,
            fields,
            ty,
            init,
            ..
        } => {
            walk_expr(v, init);
            if let Some(t) = ty {
                walk_ty(v, t);
            }
            walk_path_generics(v, path);
            for (_, bind) in fields {
                v.bind_at(bind, s.span);
            }
        }
        StmtKind::Assign(a, b) => {
            walk_expr(v, a);
            walk_expr(v, b);
        }
        StmtKind::CompoundAssign { target, value, .. } => {
            walk_expr(v, target);
            walk_expr(v, value);
        }
        // Nested items have their own scopes; their bodies can't see the
        // enclosing function's locals.
        StmtKind::Item(_) | StmtKind::Continue { .. } => {}
        StmtKind::Expr(e) | StmtKind::Trailing(e) | StmtKind::Throw(e) => walk_expr(v, e),
        StmtKind::Return(e) | StmtKind::Break { value: e, .. } => {
            if let Some(e) = e {
                walk_expr(v, e);
            }
        }
    }
}

fn walk_args(v: &mut impl AstVisitor, args: &[CallArg]) {
    for a in args {
        match a {
            CallArg::Positional(e)
            | CallArg::Spread(e)
            | CallArg::Ref(e)
            | CallArg::Named { value: e, .. } => {
                walk_expr(v, e);
            }
        }
    }
}

fn walk_path_generics(v: &mut impl AstVisitor, p: &rokc_ast::Path) {
    v.path(p);
    for seg in &p.segments {
        for g in &seg.generics {
            walk_ty(v, g);
        }
    }
}

fn walk_fstring(v: &mut impl AstVisitor, parts: &[rokc_ast::FStringPart]) {
    for p in parts {
        if let rokc_ast::FStringPart::Hole { expr, spec } = p {
            walk_expr(v, expr);
            for piece in spec {
                if let rokc_ast::SpecPiece::Expr(e) = piece {
                    walk_expr(v, e);
                }
            }
        }
    }
}

pub fn walk_expr(v: &mut impl AstVisitor, e: &Expr) {
    v.expr(e);
    match &e.kind {
        // `InModule`: another module's scope, no names of this one.
        ExprKind::Literal(_)
        | ExprKind::SelfLower
        | ExprKind::SelfUpper
        | ExprKind::InModule(..) => {}
        ExprKind::Path(p) => walk_path_generics(v, p),
        ExprKind::QPath(q) => walk_qpath(v, q),
        ExprKind::Tuple(items) | ExprKind::List(items) | ExprKind::Set(items) => {
            for i in items {
                walk_expr(v, i);
            }
        }
        ExprKind::ArrayRepeat { value, len } => {
            walk_expr(v, value);
            if let rokc_ast::ConstExpr::Expr(e) = len {
                walk_expr(v, e);
            }
        }
        ExprKind::Dict(pairs) => {
            for (k, val) in pairs {
                walk_expr(v, k);
                walk_expr(v, val);
            }
        }
        // Every candidate captured the same source; walk it once.
        ExprKind::MagicLiteral(cands) => {
            let args = cands.iter().take(1).flat_map(|c| &c.args);
            args.flat_map(rokc_ast::LitArg::exprs)
                .for_each(|x| walk_expr(v, x));
        }
        ExprKind::Block(b) | ExprKind::Unsafe(b) => walk_block(v, b),
        ExprKind::AsyncBlock(b) | ExprKind::YieldBlock(b) | ExprKind::AsyncYieldBlock(b) => {
            v.enter_body();
            walk_block(v, b);
            v.leave_body();
        }
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            walk_expr(v, cond);
            walk_block(v, then_branch);
            if let Some(eb) = else_branch {
                walk_expr(v, eb);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(v, scrutinee);
            for arm in arms {
                v.push_scope();
                walk_pat(v, &arm.pat);
                if let Some(g) = &arm.guard {
                    walk_expr(v, g);
                }
                walk_expr(v, &arm.body);
                v.pop_scope();
            }
        }
        ExprKind::Loop { body, .. } => walk_block(v, body),
        ExprKind::While { cond, body, .. } => {
            walk_expr(v, cond);
            walk_block(v, body);
        }
        ExprKind::For {
            pat, iter, body, ..
        } => {
            walk_expr(v, iter);
            v.push_scope();
            walk_pat(v, pat);
            walk_block(v, body);
            v.pop_scope();
        }
        ExprKind::Try { operand, .. } => walk_expr(v, operand),
        ExprKind::Catch {
            body,
            catches,
            finally,
        } => {
            walk_expr(v, body);
            for c in catches {
                v.push_scope();
                walk_pat(v, &c.pat);
                walk_block(v, &c.body);
                v.pop_scope();
            }
            if let Some(f) = finally {
                walk_block(v, f);
            }
        }
        ExprKind::OpInfix { lhs, rhs, .. }
        | ExprKind::NullishCoalesce(lhs, rhs)
        | ExprKind::Index(lhs, rhs) => {
            walk_expr(v, lhs);
            walk_expr(v, rhs);
        }
        ExprKind::OpPrefix { operand, .. } | ExprKind::OpPostfix { operand, .. } => {
            walk_expr(v, operand);
        }
        ExprKind::Cast(inner, ty) => {
            walk_expr(v, inner);
            walk_ty(v, ty);
        }
        ExprKind::Is(inner, pat) => {
            walk_expr(v, inner);
            walk_pat(v, pat);
        }
        ExprKind::Field(t, _) | ExprKind::TupleField(t, _) => walk_expr(v, t),
        ExprKind::MethodCall {
            receiver,
            type_args,
            args,
            trailing_closure,
            ..
        } => {
            walk_expr(v, receiver);
            for t in type_args {
                walk_ty(v, t);
            }
            walk_args(v, args);
            if let Some(tc) = trailing_closure {
                walk_expr(v, tc);
            }
        }
        ExprKind::Broadcast { callee, args } => {
            walk_expr(v, callee);
            walk_args(v, args);
        }
        ExprKind::Call {
            callee,
            args,
            trailing_closure,
        } => {
            walk_expr(v, callee);
            walk_args(v, args);
            if let Some(tc) = trailing_closure {
                walk_expr(v, tc);
            }
        }
        ExprKind::Slice {
            target,
            start,
            end,
            step,
        } => {
            walk_expr(v, target);
            for x in [start, end, step].into_iter().flatten() {
                walk_expr(v, x);
            }
        }
        ExprKind::Range { start, end, .. } => {
            for x in [start, end].into_iter().flatten() {
                walk_expr(v, x);
            }
        }
        ExprKind::QuestionDot(t, tails) => {
            walk_expr(v, t);
            for tail in tails {
                match tail {
                    AccessTail::Field(_) | AccessTail::TupleField(_) => {}
                    AccessTail::Method {
                        type_args, args, ..
                    } => {
                        for t in type_args {
                            walk_ty(v, t);
                        }
                        walk_args(v, args);
                    }
                    AccessTail::Index(i) => walk_expr(v, i),
                }
            }
        }
        ExprKind::TryQ(inner) | ExprKind::Await(inner) => walk_expr(v, inner),
        ExprKind::FString(parts) => walk_fstring(v, parts),
        ExprKind::Yield(inner) => {
            if let Some(i) = inner {
                walk_expr(v, i);
            }
        }
        ExprKind::Closure {
            params,
            ret_ty,
            body,
            ..
        } => {
            v.push_scope();
            for p in params {
                if let Some(t) = &p.ty {
                    walk_ty(v, t);
                }
                v.bind_at(&p.name, p.span);
            }
            if let Some(t) = ret_ty {
                walk_ty(v, t);
            }
            v.enter_body();
            walk_expr(v, body);
            v.leave_body();
            v.pop_scope();
        }
        ExprKind::StructInit {
            path,
            fields,
            spread,
        } => {
            walk_path_generics(v, path);
            for (_, f) in fields {
                walk_expr(v, f);
            }
            if let Some(s) = spread {
                walk_expr(v, s);
            }
        }
    }
}

/// Visits a pattern's types and binds its names into the current scope.
pub fn walk_pat(v: &mut impl AstVisitor, p: &Pattern) {
    match &p.kind {
        PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::Range { .. } => {}
        PatternKind::Binding { name, sub } => {
            if let Some(s) = sub {
                walk_pat(v, s);
            }
            if name != "_" {
                v.bind_at(name, p.span);
            }
        }
        PatternKind::TypeCheck { binder, ty } => {
            walk_ty(v, ty);
            if let Some(b) = binder {
                v.bind_at(b, p.span);
            }
        }
        PatternKind::Tuple(parts) => {
            for x in parts {
                walk_pat(v, x);
            }
        }
        // Every alternative binds the same names.
        PatternKind::Or(alts) => {
            for x in alts {
                walk_pat(v, x);
            }
        }
        PatternKind::Rest(name) => {
            if let Some(n) = name {
                v.bind_at(n, p.span);
            }
        }
        PatternKind::Struct { path, fields, .. } => {
            walk_path_generics(v, path);
            for fp in fields {
                match &fp.pat {
                    Some(x) => walk_pat(v, x),
                    None => v.bind_at(&fp.name, p.span),
                }
            }
        }
        PatternKind::EnumVariant { path, args } => {
            walk_path_generics(v, path);
            for a in args {
                walk_pat(v, a);
            }
        }
    }
}

fn walk_qpath(v: &mut impl AstVisitor, q: &rokc_ast::QPath) {
    walk_ty(v, &q.self_ty);
    walk_path_generics(v, &q.trait_path);
}

pub fn walk_ty(v: &mut impl AstVisitor, t: &Ty) {
    v.ty(t);
    match &t.kind {
        TyKind::Path(p) => walk_path_generics(v, p),
        TyKind::QPath(q) => walk_qpath(v, q),
        TyKind::Sum(parts) | TyKind::Tuple(parts) => {
            for p in parts {
                walk_ty(v, p);
            }
        }
        TyKind::Array(elem, _) | TyKind::Nest(elem) | TyKind::Binding(_, elem) => walk_ty(v, elem),
        TyKind::Fn(args, ret, _) => {
            for a in args {
                walk_ty(v, a);
            }
            walk_ty(v, ret);
        }
        TyKind::AnyTrait(bounds) => {
            for b in bounds {
                walk_path_generics(v, b);
            }
        }
        TyKind::Throws { ok, err } => {
            walk_ty(v, ok);
            walk_ty(v, err);
        }
        TyKind::Any | TyKind::None | TyKind::SelfUpper | TyKind::Never | TyKind::Const(_) => {}
    }
}

/// Names a closure body references but doesn't bind, in first-use
/// order. Includes globals (the caller keeps only outer locals).
pub fn free_names(params: &[rokc_ast::Param], body: &Expr) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (name, _) in free_places(params, body) {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Every read of a name `body` doesn't bind, with the field chain read
/// from it (`p.a.b` → `("p", ["a", "b"])`; a bare `p` has none).
pub fn free_places(params: &[rokc_ast::Param], body: &Expr) -> Vec<(String, Vec<String>)> {
    struct Free {
        scopes: Vec<Vec<String>>,
        out: Vec<(String, Vec<String>)>,
        /// Field targets already counted by the chain around them.
        inner: Vec<*const Expr>,
    }
    impl Free {
        fn free(&self, name: &str) -> bool {
            !self.scopes.iter().any(|s| s.iter().any(|b| b == name))
        }
    }
    fn root_name(e: &Expr) -> Option<&str> {
        match &e.kind {
            ExprKind::Path(p) => match p.segments.as_slice() {
                [seg] => Some(seg.name.as_str()),
                _ => None,
            },
            // A method body's closure captures the receiver.
            ExprKind::SelfLower => Some("self"),
            _ => None,
        }
    }
    impl AstVisitor for Free {
        fn expr(&mut self, e: &Expr) {
            if let Some(i) = self.inner.iter().position(|p| std::ptr::eq(*p, e)) {
                self.inner.swap_remove(i);
                if let ExprKind::Field(t, _) | ExprKind::TupleField(t, _) = &e.kind {
                    self.inner.push(&raw const **t);
                }
                return;
            }
            let mut fields = Vec::new();
            let mut cur = e;
            loop {
                let (t, f) = match &cur.kind {
                    ExprKind::Field(t, f) => (t, f.clone()),
                    ExprKind::TupleField(t, i) => (t, i.to_string()),
                    _ => break,
                };
                fields.push(f);
                cur = t;
            }
            let Some(name) = root_name(cur) else { return };
            if let ExprKind::Field(t, _) | ExprKind::TupleField(t, _) = &e.kind {
                self.inner.push(&raw const **t);
            }
            if self.free(name) {
                fields.reverse();
                self.out.push((name.to_owned(), fields));
            }
        }
        fn bind(&mut self, name: &str) {
            if let Some(s) = self.scopes.last_mut() {
                s.push(name.to_owned());
            }
        }
        fn push_scope(&mut self) {
            self.scopes.push(Vec::new());
        }
        fn pop_scope(&mut self) {
            self.scopes.pop();
        }
    }
    let mut f = Free {
        scopes: vec![params.iter().map(|p| p.name.clone()).collect()],
        out: Vec::new(),
        inner: Vec::new(),
    };
    walk_expr(&mut f, body);
    f.out
}

/// Calls `f` on every AST type in `item` (signatures, fields, bodies).
pub fn for_each_ty(item: &Item, f: &mut dyn FnMut(&Ty)) {
    struct Tys<'f>(&'f mut dyn FnMut(&Ty));
    impl AstVisitor for Tys<'_> {
        fn ty(&mut self, t: &Ty) {
            (self.0)(t);
        }
        fn expr(&mut self, e: &Expr) {
            // `Pair<i32> { ... }` / `Box::<T>` carry type uses on paths.
            let (ExprKind::StructInit { path, .. } | ExprKind::Path(path)) = &e.kind else {
                return;
            };
            if path.segments.last().is_some_and(|s| !s.generics.is_empty()) {
                (self.0)(&Ty {
                    kind: TyKind::Path(path.clone()),
                    span: path.span,
                });
            }
        }
    }
    walk_item(&mut Tys(f), item);
}
