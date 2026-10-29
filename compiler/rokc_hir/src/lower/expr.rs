//! Blocks, statements and expressions.

#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn lower_block(ctx: &mut LowerCtx<'_>, block: &Block) -> HirBlock {
    ctx.push_scope();
    declare_nested_fns(ctx, block);
    let mut stmts = Vec::with_capacity(block.stmts.len());
    for (i, s) in block.stmts.iter().enumerate() {
        match &s.kind {
            StmtKind::LetStruct {
                path,
                fields,
                rest: _,
                ty,
                init,
            } => ctx.at_span(s.span, |ctx| {
                lower_let_struct(ctx, path, fields, ty.as_ref(), init, s.span, &mut stmts);
            }),
            StmtKind::LetPattern { pat, ty, init } => ctx.at_span(s.span, |ctx| {
                guard_sink(ctx, init);
                let value = match ty.as_ref().map(|t| ctx.resolve_ast_ty(t)) {
                    Some(t) => {
                        let v = lower_expected(ctx, init, t.clone());
                        coerce(ctx, &t, v, &|| "the destructured value".to_owned())
                    }
                    None => lower_expected(ctx, init, HirTy::Unknown),
                };
                // The temp's `using` drops the whole value (drop glue), so
                // its guard parts need no `using` of their own.
                let tracked = ctx.guard_lets.len();
                ctx.at_span(pat.span, |ctx| {
                    super::desugar::bind_irrefutable(ctx, pat, value, &mut stmts);
                });
                if matches!(&init.kind, ExprKind::Path(p) if p.segments[0].name.starts_with("#using"))
                {
                    ctx.guard_lets.truncate(tracked);
                }
            }),
            StmtKind::LetTuple {
                names,
                rest,
                ty,
                init,
            } => ctx.at_span(s.span, |ctx| {
                lower_let_tuple(
                    ctx,
                    names,
                    rest.as_ref(),
                    ty.as_ref(),
                    init,
                    s.span,
                    &mut stmts,
                );
            }),
            StmtKind::Let {
                name,
                ty: None,
                init,
            } => {
                let rest = &block.stmts[i + 1..];
                stmts.extend(super::infer::lower_let(ctx, s, name, init, rest, block));
            }
            _ => {
                if let Some(stmt) = lower_stmt(ctx, s) {
                    stmts.push(stmt);
                }
            }
        }
    }
    // The tail inherits the block's expected type; in a labelled block it
    // is a `break` to it (spec 3.9.6) and must agree with the others.
    let label_target = (ctx.breakables.len().checked_sub(1))
        .filter(|&i| block.label.is_some() && ctx.breakables[i].0 == block.label);
    let trailing = block.trailing.as_ref().map(|e| match label_target {
        Some(idx) => lower_break_value(ctx, idx, e),
        None => lower_expr(ctx, e),
    });
    ctx.pop_scope();
    HirBlock {
        stmts,
        trailing,
        label: block.label.clone(),
    }
}

/// The nested fns of `block` (spec 4.10), visible in the whole block
/// (hoisted): the ones declared here over those of enclosing blocks.
fn declare_nested_fns(ctx: &mut LowerCtx<'_>, block: &Block) {
    // Name → the statement and the fn of its first definition here.
    let mut own: HashMap<&str, (rokc_diagnostic::Span, rokc_diagnostic::Span)> = HashMap::new();
    for inherited in [true, false] {
        if !inherited {
            // A `use` of the block hides the nested fns of the blocks
            // around it (an empty key, `resolve_any_fn`).
            for s in &block.stmts {
                let StmtKind::Item(Item::Use(u)) = &s.kind else {
                    continue;
                };
                if u.attrs
                    .iter()
                    .any(|a| a.path == [rokc_ast::INHERITED_USE_ATTR])
                {
                    continue;
                }
                let mut names = Vec::new();
                rokc_ast::rename::use_names(&u.tree, &mut names);
                if let Some(m) = ctx.nested_fns.last_mut() {
                    m.extend(names.into_iter().map(|n| (n.to_owned(), String::new())));
                }
            }
        }
        for s in &block.stmts {
            let StmtKind::Item(Item::Fn(f)) = &s.kind else {
                continue;
            };
            let Some((lifted, from_outer)) = rokc_ast::nested_marker_of(f) else {
                continue;
            };
            if from_outer != inherited {
                continue;
            }
            if let Some(&(first, first_fn)) = own.get(f.name.as_str()).filter(|_| !inherited) {
                ctx.emit_diag(rokc_diagnostic::duplicate(
                    rokc_diagnostic::ErrorKind::Type,
                    format!(
                        "nested function `{}` is defined twice in this block",
                        f.name
                    ),
                    &f.name,
                    s.span,
                    Some(first),
                    true,
                ));
                // Calls keep to the first, reporting no argument errors.
                ctx.env.duplicated.borrow_mut().insert(first_fn);
                continue;
            }
            if !inherited {
                own.insert(&f.name, (s.span, f.span));
            }
            let full = if ctx.current_module.is_empty() {
                lifted.to_owned()
            } else {
                format!("{}::{lifted}", ctx.current_module)
            };
            if let Some(m) = ctx.nested_fns.last_mut() {
                m.insert(f.name.clone(), full);
            }
        }
    }
    if own.is_empty() {
        return;
    }
    for s in &block.stmts {
        let names: Vec<&str> = match &s.kind {
            StmtKind::Let { name, .. } => vec![name.as_str()],
            StmtKind::LetTuple { names, .. } => names.iter().map(String::as_str).collect(),
            _ => continue,
        };
        for n in names {
            if own.contains_key(n) {
                ctx.emit_error_at(
                    rokc_diagnostic::ErrorKind::Type,
                    s.span,
                    format!(
                        "`{n}` names both a variable and a nested function in this block \
                         (spec 4.10.8)"
                    ),
                );
            }
        }
    }
}

/// Spec 3.9.1: a block-local item sees no local or generic parameter of
/// the code around it.
fn check_local_item(ctx: &LowerCtx<'_>, item: &Item) {
    use crate::walk::AstVisitor;
    #[derive(Default)]
    struct Free {
        scopes: Vec<Vec<String>>,
        values: Vec<(String, rokc_diagnostic::Span)>,
        types: Vec<(String, rokc_diagnostic::Span)>,
    }
    impl AstVisitor for Free {
        fn expr(&mut self, e: &Expr) {
            if let ExprKind::Path(p) = &e.kind {
                if let [seg] = p.segments.as_slice() {
                    if !self.scopes.iter().flatten().any(|n| *n == seg.name) {
                        self.values.push((seg.name.clone(), e.span));
                    }
                }
            }
        }
        fn ty(&mut self, t: &Ty) {
            if let rokc_ast::TyKind::Path(p) = &t.kind {
                if let [seg] = p.segments.as_slice() {
                    self.types.push((seg.name.clone(), t.span));
                }
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
    let mut free = Free::default();
    free.push_scope();
    crate::walk::walk_item(&mut free, item);
    let outer = rokc_core::local::qualified(item.local_outer().unwrap_or_default());
    let (what, own) = match item.name() {
        Some(n) => (format!("`{}`", rokc_core::local::leaf(n)), true),
        None => ("this `impl`".to_owned(), false),
    };
    let mut seen = std::collections::HashSet::new();
    for (name, span) in &free.values {
        if ctx.lookup(name).is_none() || !seen.insert(name) {
            continue;
        }
        let fix = match item {
            Item::Static(_) => "a `static` is made once, so its value can use only items",
            Item::Const(_) => "a `const` is computed at compile time, so it can use only items",
            Item::Struct(_) | Item::Enum(_) | Item::Union(_) if own => {
                "pass it as a field or a parameter instead"
            }
            _ => "pass it as a parameter instead",
        };
        ctx.emit_error_at(
            rokc_diagnostic::ErrorKind::Type,
            *span,
            format!(
                "{what} is declared inside `{outer}` and can't use `{name}`, a local of \
                 `{outer}`; {fix} (spec 3.9.1)"
            ),
        );
    }
    let mut generics = Vec::new();
    local_item_generics(item, &mut generics);
    for (name, span) in free.types.iter().chain(&free.values) {
        if !ctx.ty_subst.contains_key(name)
            || generics.contains(&name.as_str())
            || !seen.insert(name)
        {
            continue;
        }
        let leaf = item.name().map(rokc_core::local::leaf).unwrap_or_default();
        let keyword = match item {
            Item::Struct(_) => "struct",
            Item::Enum(_) => "enum",
            Item::Union(_) => "union",
            Item::Trait(_) => "trait",
            Item::TypeAlias(_) => "type",
            _ => "fn",
        };
        let fix = match item {
            Item::Static(_) | Item::Const(_) => {
                format!("it has one value, whatever `{outer}` is called with")
            }
            Item::Impl(_) => format!("declare it on the impl instead: `impl<{name}>`"),
            _ => format!("declare its own instead: `{keyword} {leaf}<{name}>`"),
        };
        ctx.emit_error_at(
            rokc_diagnostic::ErrorKind::Type,
            *span,
            format!(
                "{what} is declared inside `{outer}` and can't use `{name}`, a generic \
                 parameter of `{outer}`; {fix} (spec 3.9.1)"
            ),
        );
    }
}

/// The generic parameters and associated types `item` declares
/// anywhere (on itself and its methods).
fn local_item_generics<'a>(item: &'a Item, out: &mut Vec<&'a str>) {
    let mut gs: Vec<&rokc_ast::Generics> = Vec::new();
    match item {
        Item::Fn(f) => gs.push(&f.generics),
        Item::Struct(x) => gs.push(&x.generics),
        Item::Enum(x) => gs.push(&x.generics),
        Item::Union(x) => gs.push(&x.generics),
        Item::TypeAlias(x) => gs.push(&x.generics),
        Item::Trait(t) => {
            gs.push(&t.generics);
            for ti in &t.items {
                match ti {
                    rokc_ast::TraitItem::Method(m) => gs.push(&m.generics),
                    rokc_ast::TraitItem::AssocType { name, .. } => out.push(name),
                    rokc_ast::TraitItem::Const { .. } => {}
                }
            }
        }
        Item::Impl(i) => {
            gs.push(&i.generics);
            for ii in &i.items {
                match ii {
                    rokc_ast::ImplItem::Method(m) => gs.push(&m.generics),
                    rokc_ast::ImplItem::AssocType { name, .. } => out.push(name),
                    rokc_ast::ImplItem::Const(_) => {}
                }
            }
        }
        _ => {}
    }
    out.extend(gs.iter().flat_map(|g| &g.params).map(|p| match p {
        rokc_ast::GenericParam::Type { name, .. } | rokc_ast::GenericParam::Const { name, .. } => {
            name.as_str()
        }
    }));
}

/// `let S { f1, f2: alias, .. } [: T] = init;` → temp local + one field
/// read per binding.
pub(super) fn lower_let_struct(
    ctx: &mut LowerCtx<'_>,
    path: &AstPath,
    fields: &[(String, String)],
    ty: Option<&Ty>,
    init: &Expr,
    span: rokc_diagnostic::Span,
    stmts: &mut Vec<HirStmt>,
) {
    let pre_ty = ty.map(|t| ctx.resolve_ast_ty(t));
    let init_h = lower_expected(ctx, init, pre_ty.clone().unwrap_or(HirTy::Unknown));
    let init_h = match &pre_ty {
        Some(t) => coerce(ctx, t, init_h, &|| "the destructured value".to_owned()),
        None => init_h,
    };
    let init_ty = init_h.ty.clone();
    let temp_local = ctx.declare(None, init_ty.clone());
    stmts.push(HirStmt::Let {
        local: temp_local,
        init: init_h,
    });
    let segs: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
    let named = ctx
        .canon_type(&segs)
        .and_then(|full| ctx.lookup_struct(&full));
    let sid = match (&init_ty, named) {
        (HirTy::Unknown, _) => {
            // Init failed (reported); bind the names so later uses don't
            // cascade into "cannot find value".
            for (_, bind) in fields {
                ctx.declare(Some(bind.clone()), HirTy::Unknown);
            }
            return;
        }
        (HirTy::Struct(sid), Some(n)) if *sid == n => *sid,
        (HirTy::Struct(sid), None) if path.segments.is_empty() => *sid,
        _ => {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "struct pattern `{}` does not match the value's type `{}`",
                    segs.join("::"),
                    ctx.ty_name(&init_ty)
                ),
            );
            for (_, bind) in fields {
                ctx.declare(Some(bind.clone()), HirTy::Unknown);
            }
            return;
        }
    };
    let def = ctx.struct_def(sid);
    for (field_name, bind_name) in fields {
        let Some(idx) = def.fields.iter().position(|f| &f.name == field_name) else {
            ctx.no_field(sid, field_name, span);
            ctx.declare(Some(bind_name.clone()), HirTy::Unknown);
            continue;
        };
        ctx.check_field_visible(sid, field_name);
        let field_ty = def.fields[idx].ty.clone();
        let read = HirExpr {
            ty: field_ty.clone(),
            span,
            kind: HirExprKind::FieldRead {
                target: Box::new(HirExpr {
                    ty: init_ty.clone(),
                    span,
                    kind: HirExprKind::Local(temp_local),
                }),
                struct_id: sid,
                field_index: u32::try_from(idx).expect("field idx overflow"),
                field_ty: field_ty.clone(),
            },
        };
        stmts.push(ctx.declare_var(bind_name, field_ty, read, span));
    }
}

/// `let (a, b, ..rest) [: T] = init;` → fresh tuple local plus per-name
/// index reads (spec 3.15.2 / 3.15.3).
pub(super) fn lower_let_tuple(
    ctx: &mut LowerCtx<'_>,
    names: &[String],
    rest: Option<&(usize, Option<String>)>,
    ty: Option<&Ty>,
    init: &Expr,
    span: rokc_diagnostic::Span,
    stmts: &mut Vec<HirStmt>,
) {
    let pre_ty = ty.map(|t| ctx.resolve_ast_ty(t));
    let init_h = lower_expected(ctx, init, pre_ty.clone().unwrap_or(HirTy::Unknown));
    let init_h = match &pre_ty {
        Some(t) => coerce(ctx, t, init_h, &|| "the destructured value".to_owned()),
        None => init_h,
    };
    let init_ty = init_h.ty.clone();
    let elem_tys: Vec<HirTy> = match &init_ty {
        HirTy::Tuple(ts) => ts.clone(),
        HirTy::Unknown => vec![HirTy::Unknown; names.len()],
        other => {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "tuple destructure of a non-tuple value of type `{}`",
                    ctx.ty_name(other)
                ),
            );
            vec![HirTy::Unknown; names.len()]
        }
    };
    // Spec 3.15.2/3.15.3: length must match (no rest), or be ≥
    // names.len() when a `..` is present.
    let needed = names.len();
    let have = elem_tys.len();
    if rest.is_none() && have != needed {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "tuple destructure length mismatch: pattern has {needed} element{}, value \
                 has {have} (spec 3.15.2)",
                if needed == 1 { "" } else { "s" }
            ),
        );
    } else if rest.is_some() && have < needed {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "tuple destructure too short: pattern needs at least {needed} non-rest \
                 element{}, value has {have} (spec 3.15.3)",
                if needed == 1 { "" } else { "s" }
            ),
        );
    }
    let tuple_local = ctx.declare(None, init_ty.clone());
    stmts.push(HirStmt::Let {
        local: tuple_local,
        init: init_h,
    });
    // With a rest at `r` and an `N`-tuple: `names[..r]` take `init.0..r`,
    // the rest takes the middle, `names[r..]` the last ones.
    let rest_take = if rest.is_some() {
        have.saturating_sub(needed)
    } else {
        0
    };
    let rest_idx = rest.map_or(needed, |(i, _)| *i);
    let read = |index: usize, ety: HirTy| HirExpr {
        ty: ety.clone(),
        span,
        kind: HirExprKind::TupleField {
            target: Box::new(HirExpr {
                ty: init_ty.clone(),
                span,
                kind: HirExprKind::Local(tuple_local),
            }),
            index: u32::try_from(index).expect("tuple index overflow"),
            field_ty: ety,
        },
    };
    for (i, name) in names.iter().enumerate() {
        if name == "_" {
            continue;
        }
        let init_index = if i < rest_idx { i } else { i + rest_take };
        let elem_ty = elem_tys.get(init_index).cloned().unwrap_or(HirTy::Unknown);
        let init = read(init_index, elem_ty.clone());
        stmts.push(ctx.declare_var(name, elem_ty, init, span));
    }
    // Synthesize the rest-binding's sub-tuple if it was named.
    if let Some((rest_idx, Some(rest_name))) = rest {
        let sub: Vec<HirExpr> = (0..rest_take)
            .map(|k| {
                let src = rest_idx + k;
                read(src, elem_tys.get(src).cloned().unwrap_or(HirTy::Unknown))
            })
            .collect();
        let rest_ty = HirTy::Tuple(sub.iter().map(|e| e.ty.clone()).collect());
        let init = HirExpr {
            ty: rest_ty.clone(),
            span,
            kind: HirExprKind::Tuple { fields: sub },
        };
        stmts.push(ctx.declare_var(rest_name, rest_ty, init, span));
    }
}

pub(super) fn lower_stmt(ctx: &mut LowerCtx<'_>, stmt: &Stmt) -> Option<HirStmt> {
    // Diagnostics raised while lowering default to this location.
    ctx.at_span(stmt.span, |ctx| lower_stmt_inner(ctx, stmt))
}

fn lower_stmt_inner(ctx: &mut LowerCtx<'_>, stmt: &Stmt) -> Option<HirStmt> {
    match &stmt.kind {
        StmtKind::Let { name, ty, init } => {
            // Same-scope redeclaration is an error (typo guard, no
            // shadowing); an inner scope makes a fresh binding.
            if name != "_" && ctx.lookup_in_current_scope(name).is_some() {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!(
                        "`{name}` is already declared in this scope, and rok does not \
                         let a second `let` hide it"
                    ),
                );
                let rhs = snippet(init);
                let ann = ty
                    .as_ref()
                    .map(|t| format!(": {}", rokc_ast::print::print_ty(t)))
                    .unwrap_or_default();
                ctx.help_last(Some(format!(
                    "to change its value write `{name} = {rhs};`, or give the new value its \
                     own name: `let {name}2{ann} = {rhs};`"
                )));
            }
            // The declared type (if any) is the init's expected type, so
            // literals and generic-enum leaves (`Box::None`) pick it up.
            let pre_ty = ty.as_ref().map(|t| ctx.resolve_ast_ty(t));
            let ast_init = init;
            guard_sink(ctx, init);
            // `using (a, b) = (..)` drops the whole tuple.
            if let (true, ExprKind::Tuple(items)) = (name.starts_with("#using"), &init.kind) {
                for i in items {
                    guard_sink(ctx, i);
                }
            }
            let init = lower_expected(ctx, init, pre_ty.clone().unwrap_or(HirTy::Unknown));
            let (declared_ty, init) = if let Some(t) = pre_ty {
                let init = coerce(ctx, &t, init, &|| format!("`let {name}`"));
                (t, init)
            } else {
                check_let_inferable(ctx, name, &init);
                (init.ty.clone(), init)
            };
            let ty = declared_ty.clone();
            let fresh = ctx.sole_names.contains(name.as_str()) && fresh_future(ctx, ast_init);
            let s = ctx.declare_var(name, declared_ty, init, stmt.span);
            if let HirStmt::Let { local, .. } = s {
                ctx.track_guard(local, &ty, stmt.span, name);
                if fresh {
                    ctx.sole_fresh.insert(local, ctx.breakables.len());
                }
            }
            Some(s)
        }
        StmtKind::Return(_) if ctx.implicit_main => {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                "`return` is not allowed in top-level statements (spec 4.16.4); use \
                 `env::exit(code)` or an explicit `fn main() -> i32`"
                    .to_owned(),
            );
            None
        }
        StmtKind::Return(value) => {
            let ret = ctx.fn_ret_ty.clone();
            let value = if let Some(e) = value {
                guard_sink(ctx, e);
                let lowered = lower_expected(ctx, e, ret.clone());
                // A closure without a declared return type takes it from
                // its first `return`.
                if ret == HirTy::Unknown && ctx.infer_ret {
                    ctx.fn_ret_ty = lowered.ty.clone();
                    // A poisoned value leaves the type unknown, not unset.
                    ctx.infer_ret = lowered.ty != HirTy::Unknown;
                    Some(lowered)
                } else if let Some(tys) = ctx.ret_tys.as_mut() {
                    tys.push(lowered.ty.clone());
                    Some(lowered)
                } else {
                    Some(coerce(ctx, &ret, lowered, &|| {
                        "the return value".to_owned()
                    }))
                }
            } else {
                if let Some(tys) = ctx.ret_tys.as_mut() {
                    tys.push(HirTy::Unit);
                }
                if !matches!(ret, HirTy::Unit | HirTy::Unknown) {
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!(
                            "`return;` without a value in a function returning `{}`",
                            ctx.ty_name(&ret)
                        ),
                    );
                }
                None
            };
            Some(lower_return(ctx, value, stmt.span))
        }
        // Statement position: the value is discarded (expected `()`), so
        // `if` / `match` arms needn't agree on a type.
        StmtKind::Expr(expr) => {
            guard_sink(ctx, expr);
            let h = lower_expected(ctx, expr, HirTy::Unit);
            if let Some(g) = lock_guard(ctx, &h.ty) {
                ctx.emit_error_at(
                    rokc_diagnostic::ErrorKind::Type,
                    stmt.span,
                    format!(
                        "this `{g}` is discarded without being released: only `using` \
                         releases a lock guard (`using g = .. {{ .. }}`, stdlib 2.16.3)"
                    ),
                );
            }
            Some(HirStmt::Expr(h))
        }
        // A block-like statement without `;` before more statements
        // (`if c { 1 } else { 2 }  next();`) must not produce a value.
        StmtKind::Trailing(expr) => {
            let h = lower_expected(ctx, expr, HirTy::Unit);
            if let Some(t) = arm_value_ty(&h) {
                ctx.emit_error_at(
                    rokc_diagnostic::ErrorKind::Type,
                    stmt.span,
                    format!(
                        "this block-like statement leaves a value of type `{}` without `;`: a \
                         statement must be `()` (add `;` to discard it, or make it the \
                         block's last expression)",
                        ctx.ty_name(&t)
                    ),
                );
            }
            Some(HirStmt::Expr(h))
        }
        StmtKind::Item(rokc_ast::Item::Use(u)) => {
            // Function-local `use`: visible in the current scope only.
            let mut entries = Vec::new();
            crate::uses::flatten_tree_for_local(&u.tree, &ctx.current_module, &mut entries);
            for e in entries {
                ctx.add_local_use(e);
            }
            None
        }
        StmtKind::Item(Item::Fn(f)) if rokc_ast::nested_marker_of(f).is_some() => None,
        StmtKind::Item(item) if item.is_local() => {
            check_local_item(ctx, item);
            None
        }
        // The parser keeps no other item in a block.
        StmtKind::Item(_) => unreachable!("a block item other than `fn` / `const` / `use`"),
        StmtKind::Assign(target, value) => {
            let a = lower_assign(ctx, target, value, stmt.span);
            write_through(ctx, a, stmt.span)
        }
        StmtKind::CompoundAssign { target, op, value } => {
            if let Some(call) = assign_trait_call(ctx, target, op, value, stmt.span) {
                return Some(HirStmt::Expr(call));
            }
            // Stdlib 2.4.1: without an `*Assign` impl, `a = a op b`
            // (also a primitive's intrinsic impl: its call is the op).
            let op = op.trim_end_matches('=').to_owned();
            let combined = Expr {
                span: stmt.span,
                kind: ExprKind::OpInfix {
                    op,
                    lhs: Box::new(target.clone()),
                    rhs: Box::new(value.clone()),
                },
            };
            let a = lower_assign(ctx, target, &combined, stmt.span);
            write_through(ctx, a, stmt.span)
        }
        StmtKind::LetTuple { .. } | StmtKind::LetStruct { .. } | StmtKind::LetPattern { .. } => {
            unreachable!("destructuring lets are expanded by lower_block")
        }
        StmtKind::Break { label, value } => {
            check_jump_target(ctx, "break", label.as_deref(), false);
            let depth = jump_index(ctx, label.as_deref(), false);
            let value = if let Some(e) = value {
                Some(lower_break_value(ctx, depth, e))
            } else {
                match ctx.break_tys.get(&depth).cloned() {
                    None => {
                        ctx.break_tys.insert(depth, (HirTy::Unit, true));
                    }
                    Some((t, _)) if t != HirTy::Unit => {
                        let t = ctx.ty_name(&t);
                        ctx.emit_error(
                            rokc_diagnostic::ErrorKind::Type,
                            format!(
                                "`break` without a value, but this loop's `break` values are \
                                 `{t}` (spec 3.13.1)"
                            ),
                        );
                        ctx.help_last(Some(format!("write `break <value>;` with a `{t}` value")));
                    }
                    Some(_) => {}
                }
                None
            };
            let label = label.clone();
            Some(exit_through(
                ctx,
                Some(depth),
                value,
                |value| HirStmt::Break { label, value },
                stmt.span,
            ))
        }
        StmtKind::Continue { label } => {
            check_jump_target(ctx, "continue", label.as_deref(), true);
            let depth = jump_index(ctx, label.as_deref(), true);
            let label = label.clone();
            Some(exit_through(
                ctx,
                Some(depth),
                None,
                |_| HirStmt::Continue { label },
                stmt.span,
            ))
        }
        StmtKind::Throw(value) => Some(lower_throw(ctx, value, stmt.span)),
    }
}

/// A method call's receiver. One with an unfixed generic parameter
/// (`B::new().with(1)`) is retried against the call's expected type when
/// that fixes it exactly: builder chains.
fn lower_receiver(ctx: &mut LowerCtx<'_>, receiver: &Expr) -> HirExpr {
    let mark = ctx.probe_mark();
    let recv = lower_expected(ctx, receiver, HirTy::Unknown);
    if ctx.env.sink.uninferred.get() == mark.uninferred {
        return recv;
    }
    let Some(expected) = ctx
        .current_expected()
        .filter(|t| !matches!(t, HirTy::Unknown | HirTy::Unit | HirTy::Any))
        .cloned()
    else {
        return recv;
    };
    let retry = ctx.probe_mark();
    let mut probe = ctx.clone();
    let fits = lower_expected(&mut probe, receiver, expected.clone()).ty == expected
        && probe.probe_clean(&retry);
    ctx.rollback(&retry);
    if !fits {
        return recv;
    }
    ctx.rollback(&mark);
    lower_expected(ctx, receiver, expected)
}

/// An unannotated `let` needs a typed initializer: not `none` alone nor
/// one that never yields (empty `[]` and mismatched branches are rejected
/// where lowered).
fn check_let_inferable(ctx: &LowerCtx<'_>, name: &str, init: &HirExpr) {
    let why = if init.ty.is_none() {
        format!("`none` alone has no value type - write `let {name}: T | none = none;`")
    } else if hir_expr_diverges(init) {
        "the initializer never produces a value (it always returns / breaks / loops)".to_owned()
    } else {
        return;
    };
    ctx.emit_error(
        rokc_diagnostic::ErrorKind::Type,
        format!("cannot infer the type of `{name}`: {why}"),
    );
}

/// `name = value;` for local `local`.
fn assign_local(ctx: &mut LowerCtx<'_>, name: &str, local: LocalId, value: &Expr) -> HirStmt {
    let mark = ctx.probe_mark();
    let mut v = lower_expected(ctx, value, ctx.var_ty(local));
    let mut local = local;
    if ctx.narrowed.contains_key(&local) && !coercible(ctx, &ctx.var_ty(local), &v.ty) {
        ctx.rollback(&mark);
        local = unnarrow(ctx, name, local, value);
        v = lower_expected(ctx, value, ctx.var_ty(local));
    }
    let slot_ty = ctx.var_ty(local);
    let value = coerce(ctx, &slot_ty, v, &|| format!("assignment to `{name}`"));
    ctx.store_local(local, value)
}

/// Spec 2.2.6: assigning a value the narrowed `local` can't hold ends the
/// narrowing for the rest of the scope; the value goes to the innermost
/// binding that takes it (else the declared one, which reports it).
fn unnarrow(ctx: &mut LowerCtx<'_>, name: &str, local: LocalId, value: &Expr) -> LocalId {
    let mut chain = vec![local];
    while let Some((orig, orig_ty)) = ctx.narrowed.get(chain.last().unwrap()).cloned() {
        chain.push(orig);
        let retry = ctx.probe_mark();
        let mut probe = ctx.clone();
        let pv = lower_expected(&mut probe, value, orig_ty.clone());
        let fits = probe.probe_clean(&retry) && coercible(ctx, &orig_ty, &pv.ty);
        ctx.rollback(&retry);
        if fits {
            break;
        }
    }
    let target = chain.pop().expect("narrowed local has an origin");
    for rebound in chain {
        if let Some(i) = ctx
            .scopes
            .iter()
            .rposition(|s| s.get(name) == Some(&rebound))
        {
            ctx.scopes[i].remove(name);
            ctx.unnarrowed.push((name.to_owned(), rebound, i));
        }
    }
    target
}

/// Restore narrowings ended since `mark` for the next branch;
/// `rejoin_unnarrowings` ends them again at the join (spec 2.2.6).
pub(super) fn branch_unnarrowings(
    ctx: &mut LowerCtx<'_>,
    mark: usize,
) -> Vec<(String, LocalId, usize)> {
    let ended = ctx.unnarrowed.split_off(mark);
    for (name, local, i) in &ended {
        if let Some(scope) = ctx.scopes.get_mut(*i) {
            scope.insert(name.clone(), *local);
        }
    }
    ended
}

pub(super) fn rejoin_unnarrowings(ctx: &mut LowerCtx<'_>, ended: Vec<(String, LocalId, usize)>) {
    for (name, local, i) in ended {
        if let Some(scope) = ctx.scopes.get_mut(i) {
            if scope.get(&name) == Some(&local) {
                scope.remove(&name);
                ctx.unnarrowed.push((name, local, i));
            }
        }
    }
}

/// `e` as source for a diagnostic's suggested fix; `..` when long.
pub(super) fn snippet(e: &Expr) -> String {
    let s = rokc_ast::print::print_expr(e);
    if s.len() > 40 || s.contains('\n') {
        "..".to_owned()
    } else {
        s
    }
}

/// A loop body that ends an outer narrowing is lowered again with it
/// ended from the entry, since the head joins the back edge (spec 2.2.6).
fn lower_loop(ctx: &mut LowerCtx<'_>, expr: &Expr) -> HirExpr {
    let mark = ctx.probe_mark();
    let log = ctx.unnarrowed.len();
    let e = lower_expr_inner(ctx, expr);
    let outer: Vec<_> = ctx.unnarrowed[log..]
        .iter()
        .filter(|u| (u.1.0 as usize) < mark.locals)
        .cloned()
        .collect();
    if outer.is_empty() {
        return e;
    }
    ctx.rollback(&mark);
    ctx.unnarrowed.truncate(log);
    ctx.unnarrowed.extend(outer);
    lower_expr_inner(ctx, expr)
}

/// `target = value;` for a local / static, a struct-field chain rooted
/// at a local, or an element of a local array.
#[allow(clippy::cognitive_complexity)] // one arm per assignable place form
pub(super) fn lower_assign(
    ctx: &mut LowerCtx<'_>,
    target: &Expr,
    value: &Expr,
    span: rokc_diagnostic::Span,
) -> Option<HirStmt> {
    match &target.kind {
        // Bare-name assignment (local, else `static`).
        // Spec 4.2.7: `_ = e;` evaluates `e` and discards it.
        ExprKind::Path(p) if p.segments.len() == 1 && p.segments[0].name == "_" => {
            let v = lower_expected(ctx, value, HirTy::Unknown);
            Some(HirStmt::Expr(v))
        }
        ExprKind::Path(p) if p.segments.len() == 1 => {
            let name = &p.segments[0].name;
            if let Some(local) = ctx.lookup(name) {
                guard_sink(ctx, value);
                if ctx.value_captured.contains(&local) {
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!(
                            "`{name}` is value-captured; to mutate the outer variable, add \
                             `[ref {name}]` to the closure's capture list (spec 4.6.4)"
                        ),
                    );
                }
                return Some(assign_local(ctx, name, local, value));
            }
            let segs = [name.as_str()];
            if let Some(full) = ctx.resolve_in(&segs, true) {
                let full = ctx.env.names.value_reexport(&full).cloned().unwrap_or(full);
                ctx.check_item_visible(&full, "static");
                if !ctx.env.is_mut_static(&full) {
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!(
                            "cannot assign to static `{name}`: statics are immutable \
                             (spec 5.3); only an `extern` `static mut` can be written"
                        ),
                    );
                }
                let ty = ctx.env.static_ty(&full).expect("a static").clone();
                let v = lower_expected(ctx, value, ty.clone());
                let value = coerce(ctx, &ty, v, &|| format!("assignment to `{name}`"));
                return Some(HirStmt::StaticAssign { name: full, value });
            }
            if ctx.resolve_in(&segs, false).is_some() {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!("cannot assign to const `{name}` (spec 5.2)"),
                );
                return None;
            }
            // Only `let` declares. Declare it anyway so later uses don't
            // report again.
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!("`{name}` is not declared; a variable is created only by `let`"),
            );
            ctx.help_last(Some(format!("write `let {name} = {};`", snippet(value))));
            let v = lower_expected(ctx, value, HirTy::Unknown);
            Some(ctx.declare_var(name, v.ty.clone(), v, span))
        }
        // Spec 4.7: `ref self` names the caller's slot; `self = v` writes it
        // (from a `[ref self]` closure too, through the shared cell).
        ExprKind::SelfLower
            if ctx.lookup("self").is_some_and(|l| {
                ctx.is_boxed(l) || ctx.ref_params.iter().any(|(r, _)| *r == l)
            }) =>
        {
            let local = ctx.lookup("self")?;
            let slot_ty = ctx.var_ty(local);
            let v = lower_expected(ctx, value, slot_ty.clone());
            let value = coerce(ctx, &slot_ty, v, &|| "assignment to `self`".to_owned());
            Some(ctx.store_local(local, value))
        }
        // `arr[idx] = value` - element store on a local array.
        ExprKind::Index(arr_expr, idx_expr) => {
            let arr_h = lower_expected(ctx, arr_expr, HirTy::Unknown);
            // A struct / enum stores through `IndexSet::index_set`.
            if let HirTy::GcBuf(elem) = arr_h.ty.clone() {
                let set = buf_access(ctx, arr_h, *elem, idx_expr, Some(value), target.span);
                return Some(HirStmt::Expr(set));
            }
            if matches!(arr_h.ty, HirTy::Struct(_) | HirTy::Enum(_)) {
                let rest = [CallArg::Positional(value.clone())];
                let span = target.span;
                return Some(HirStmt::Expr(index_call(
                    ctx,
                    arr_h,
                    "index_set",
                    idx_expr,
                    &rest,
                    span,
                )));
            }
            let idx_h = match arr_h.ty {
                HirTy::Array(_, n) => array_position(ctx, idx_expr, n),
                _ => lower_expected(ctx, idx_expr, HirTy::Unknown),
            };
            let elem = match (&arr_h.kind, &arr_h.ty) {
                (HirExprKind::Local(l), HirTy::Array(elem, _)) => Some((*l, (**elem).clone())),
                (_, HirTy::Unknown) => return None,
                _ => None,
            };
            // `s.arr[i] = v` on a field chain of a local: in place.
            if let (None, HirTy::Array(elem_ty, _)) = (&elem, &arr_h.ty) {
                if let Some((array_local, path)) = local_field_path(ctx, &arr_h) {
                    let elem_ty = (**elem_ty).clone();
                    let v = lower_expected(ctx, value, elem_ty.clone());
                    let value = coerce(ctx, &elem_ty, v, &|| "the array element".to_owned());
                    return Some(HirStmt::ArrayAssign {
                        array_local,
                        path,
                        index: idx_h,
                        value,
                        elem_ty,
                    });
                }
            }
            // `xs[j][i] = v` on a value array that isn't a local: copy the
            // array, store into the copy, write it back.
            if elem.is_none()
                && matches!(arr_h.ty, HirTy::Array(..))
                && matches!(arr_expr.kind, ExprKind::Field(..) | ExprKind::Index(..))
            {
                return Some(rewrite_index_store(
                    ctx,
                    idx_expr,
                    |copy, i| {
                        let store = Expr {
                            kind: ExprKind::Index(Box::new(copy.clone()), Box::new(i.clone())),
                            span: target.span,
                        };
                        vec![
                            let_stmt(copy, (**arr_expr).clone()),
                            assign_stmt(store, value.clone()),
                            assign_stmt((**arr_expr).clone(), copy.clone()),
                        ]
                    },
                    target.span,
                ));
            }
            let Some((array_local, elem_ty)) = elem else {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!(
                        "indexed assignment needs a local array variable as target \
                         (found `{}`)",
                        ctx.ty_name(&arr_h.ty)
                    ),
                );
                return None;
            };
            let v = lower_expected(ctx, value, elem_ty.clone());
            let value = coerce(ctx, &elem_ty, v, &|| "the array element".to_owned());
            Some(HirStmt::ArrayAssign {
                array_local,
                path: Vec::new(),
                index: idx_h,
                value,
                elem_ty,
            })
        }
        // `a.f1.f2 = value` (or `s.0`): peel to the root local and thread
        // `WithField` updates back out.
        ExprKind::Field(_, _) | ExprKind::TupleField(_, _) => {
            // `struct_id` is `None` for a plain tuple's element (`t.0 = x`).
            struct Step {
                ty: HirTy,
                struct_id: Option<StructId>,
                field_index: u32,
            }
            let mut chain: Vec<String> = Vec::new();
            let mut cur: &Expr = target;
            loop {
                match &cur.kind {
                    ExprKind::Field(inner, name) => {
                        chain.push(name.clone());
                        cur = inner;
                    }
                    ExprKind::TupleField(inner, i) => {
                        chain.push(i.to_string());
                        cur = inner;
                    }
                    _ => break,
                }
            }
            chain.reverse();
            // `xs[i].f = v`: the element is a value, so read it, update
            // the copy and store it back (`xs[i] = copy`).
            if let ExprKind::Index(arr, idx) = &cur.kind {
                let field_target = |elem: &Expr| {
                    chain.iter().fold(elem.clone(), |acc, name| Expr {
                        kind: ExprKind::Field(Box::new(acc), name.clone()),
                        span: target.span,
                    })
                };
                return Some(rewrite_index_store(
                    ctx,
                    idx,
                    |elem, i| {
                        let at = Expr {
                            kind: ExprKind::Index(arr.clone(), Box::new(i.clone())),
                            span: cur.span,
                        };
                        vec![
                            let_stmt(elem, at.clone()),
                            assign_stmt(field_target(elem), value.clone()),
                            assign_stmt(at, elem.clone()),
                        ]
                    },
                    target.span,
                ));
            }
            let root_h = lower_expected(ctx, cur, HirTy::Unknown);
            if root_h.ty == HirTy::Unknown {
                return None;
            }
            if super::mono::place_root(&root_h).is_none() {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    "field assignment needs a local variable at the root of the target \
                     (`v.f = x`)"
                        .to_owned(),
                );
                return None;
            }
            let root_place = root_h.clone();
            let mut steps: Vec<Step> = Vec::new();
            let mut step_ty = root_h.ty.clone();
            // Spec 2.5.2: writing a union field (safe) rebuilds the union
            // from that field; it must be the last step of the target.
            let mut union_value: Option<HirExpr> = None;
            for (ci, name) in chain.iter().enumerate() {
                if let HirTy::Tuple(parts) = &step_ty {
                    if let Some(i) = name.parse::<usize>().ok().filter(|i| *i < parts.len()) {
                        let next = parts[i].clone();
                        steps.push(Step {
                            ty: step_ty.clone(),
                            struct_id: None,
                            field_index: u32::try_from(i).expect("tuple field overflow"),
                        });
                        step_ty = next;
                        continue;
                    }
                }
                let HirTy::Struct(sid) = step_ty.clone() else {
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!("no field `{name}` on type `{}`", ctx.ty_name(&step_ty)),
                    );
                    return None;
                };
                let def = ctx.struct_def(sid);
                if let Some(uk) = &def.union_kind {
                    let Some(fty) = uk
                        .aliases
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, t)| t.clone())
                    else {
                        ctx.no_field(sid, name, span);
                        return None;
                    };
                    if ci + 1 != chain.len() {
                        ctx.emit_error(
                            rokc_diagnostic::ErrorKind::Type,
                            format!(
                                "cannot assign through union field `{name}`: read it (in \
                                 `unsafe`), change the copy, and write the field back"
                            ),
                        );
                        return None;
                    }
                    ctx.check_field_visible(sid, name);
                    let v = lower_expected(ctx, value, fty.clone());
                    let v = coerce(ctx, &fty, v, &|| format!("union field `{name}`"));
                    let storage_ty = def.fields[0].ty.clone();
                    union_value = Some(HirExpr {
                        kind: HirExprKind::StructInit {
                            struct_id: sid,
                            fields: vec![HirExpr {
                                kind: HirExprKind::UnionWrite {
                                    struct_id: sid,
                                    from_ty: v.ty.clone(),
                                    storage_ty: storage_ty.clone(),
                                    value: Box::new(v),
                                },
                                ty: storage_ty,
                                span,
                            }],
                        },
                        ty: HirTy::Struct(sid),
                        span,
                    });
                    break;
                }
                let Some(idx) = def.fields.iter().position(|f| f.name == *name) else {
                    ctx.no_field(sid, name, span);
                    return None;
                };
                ctx.check_field_visible(sid, name);
                steps.push(Step {
                    ty: step_ty.clone(),
                    struct_id: Some(sid),
                    field_index: u32::try_from(idx).expect("field index overflow"),
                });
                step_ty = def.fields[idx].ty.clone();
            }
            let last_name = chain.last().cloned().unwrap_or_default();
            let mut acc = match union_value {
                // `u.f = x` with `u` a local: replace the whole union.
                Some(u) if steps.is_empty() => {
                    return super::mono::assign_place(ctx, &root_place, u);
                }
                Some(u) => u,
                None => {
                    let v = lower_expected(ctx, value, step_ty.clone());
                    coerce(ctx, &step_ty, v, &|| format!("field `{last_name}`"))
                }
            };
            // Materialize parent values via field reads so each
            // WithField has a concrete target.
            let mut targets: Vec<HirExpr> = Vec::new();
            let read = |ctx: &LowerCtx<'_>, from: &HirExpr, step: &Step| {
                let (kind, ty) = match (step.struct_id, &step.ty) {
                    (Some(sid), _) => {
                        let ty = ctx.struct_def(sid).fields[step.field_index as usize]
                            .ty
                            .clone();
                        let kind = HirExprKind::FieldRead {
                            target: Box::new(from.clone()),
                            struct_id: sid,
                            field_index: step.field_index,
                            field_ty: ty.clone(),
                        };
                        (kind, ty)
                    }
                    (None, HirTy::Tuple(parts)) => {
                        let ty = parts[step.field_index as usize].clone();
                        let kind = HirExprKind::TupleField {
                            target: Box::new(from.clone()),
                            index: step.field_index,
                            field_ty: ty.clone(),
                        };
                        (kind, ty)
                    }
                    (None, _) => unreachable!("a tuple step is on a tuple"),
                };
                HirExpr {
                    kind,
                    ty,
                    span: target.span,
                }
            };
            let mut walk = root_h;
            for step in &steps[..steps.len() - 1] {
                let next = read(ctx, &walk, step);
                targets.push(walk);
                walk = next;
            }
            targets.push(walk);
            for (i, step) in steps.iter().enumerate().rev() {
                let kind = match (step.struct_id, &step.ty) {
                    (Some(sid), _) => HirExprKind::WithField {
                        target: Box::new(targets[i].clone()),
                        struct_id: sid,
                        field_index: step.field_index,
                        value: Box::new(acc),
                    },
                    // A tuple is rebuilt with the one element replaced.
                    (None, HirTy::Tuple(parts)) => {
                        let mut acc = Some(acc);
                        let fields = (0..parts.len())
                            .map(|j| {
                                let j = u32::try_from(j).expect("tuple field overflow");
                                if j == step.field_index {
                                    acc.take().expect("replaced once")
                                } else {
                                    read(
                                        ctx,
                                        &targets[i],
                                        &Step {
                                            ty: step.ty.clone(),
                                            struct_id: None,
                                            field_index: j,
                                        },
                                    )
                                }
                            })
                            .collect();
                        HirExprKind::Tuple { fields }
                    }
                    (None, _) => unreachable!("a tuple step is on a tuple"),
                };
                acc = HirExpr {
                    ty: step.ty.clone(),
                    span: target.span,
                    kind,
                };
            }
            super::mono::assign_place(ctx, &root_place, acc)
        }
        _ => {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!("invalid assignment target (spec 3.4) at line {}", span.line),
            );
            None
        }
    }
}

fn tmp_path(ctx: &LowerCtx<'_>, what: &str, span: rokc_diagnostic::Span) -> Expr {
    let n = ctx.temps.get();
    ctx.temps.set(n + 1);
    Expr {
        kind: ExprKind::Path(rokc_ast::Path {
            segments: vec![rokc_ast::PathSegment {
                name: format!("#{what}{n}"),
                generics: Vec::new(),
            }],
            span,
        }),
        span,
    }
}

fn path_name(e: &Expr) -> String {
    let ExprKind::Path(p) = &e.kind else {
        unreachable!("temp is a path")
    };
    p.segments[0].name.clone()
}

fn let_stmt(name: &Expr, init: Expr) -> Stmt {
    Stmt {
        span: init.span,
        kind: StmtKind::Let {
            name: path_name(name),
            ty: None,
            init,
        },
    }
}

const fn assign_stmt(target: Expr, value: Expr) -> Stmt {
    Stmt {
        span: target.span,
        kind: StmtKind::Assign(target, value),
    }
}

/// `l.f.g` (struct fields, tuple elements) of a plain local `l`: `l` and
/// the field indices outward.
fn local_field_path(ctx: &LowerCtx<'_>, e: &HirExpr) -> Option<(LocalId, Vec<u32>)> {
    let mut path = Vec::new();
    let mut cur = e;
    loop {
        match &cur.kind {
            HirExprKind::FieldRead {
                target,
                field_index: i,
                ..
            }
            | HirExprKind::TupleField {
                target, index: i, ..
            } => {
                path.push(*i);
                cur = target;
            }
            HirExprKind::Local(l) if !path.is_empty() && !ctx.is_boxed(*l) => {
                path.reverse();
                return Some((*l, path));
            }
            _ => return None,
        }
    }
}

fn rewrite_index_store(
    ctx: &mut LowerCtx<'_>,
    idx: &Expr,
    body: impl FnOnce(&Expr, &Expr) -> Vec<Stmt>,
    span: rokc_diagnostic::Span,
) -> HirStmt {
    let tmp = tmp_path(ctx, "tmp", span);
    // A literal / name index is read as is (and keeps its expected
    // `usize` type); anything else goes through a temp.
    let mut stmts = Vec::new();
    let i = if matches!(idx.kind, ExprKind::Literal(_) | ExprKind::Path(_)) {
        idx.clone()
    } else {
        let i = tmp_path(ctx, "idx", span);
        stmts.push(let_stmt(&i, idx.clone()));
        i
    };
    stmts.extend(body(&tmp, &i));
    let block = Expr {
        kind: ExprKind::Block(Block {
            stmts,
            trailing: None,
            span,
            label: None,
        }),
        span,
    };
    HirStmt::Expr(lower_expr(ctx, &block))
}

/// `break` / `continue` must have an enclosing target (a labelled block for
/// a labelled `break`); MIR would emit `unreachable` otherwise.
pub(super) fn check_jump_target(
    ctx: &LowerCtx<'_>,
    kw: &str,
    label: Option<&str>,
    needs_loop: bool,
) {
    let ok = match label {
        None => ctx.breakables.iter().any(|(_, is_loop)| *is_loop),
        Some(l) => ctx
            .breakables
            .iter()
            .any(|(bl, is_loop)| bl.as_deref() == Some(l) && (*is_loop || !needs_loop)),
    };
    if !ok {
        let what = label.map_or_else(
            || "a loop".to_owned(),
            |l| format!("a loop labelled `'{l}`"),
        );
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!("`{kw}` outside of {what}"),
        );
    }
}

/// Array indices are integers.
pub(super) fn check_index_ty(ctx: &LowerCtx<'_>, idx: &HirExpr) {
    if !matches!(&idx.ty, HirTy::Prim(p) if p.is_int()) && idx.ty != HirTy::Unknown {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "array index must be an integer, found `{}`",
                ctx.ty_name(&idx.ty)
            ),
        );
    }
}

/// Stdlib 2.16.3: a guard received without `using` must be bound with
/// `using`. `e` (and its branches) may hand one over unreleased: to a
/// binding or out of the function.
pub(super) fn guard_sink(ctx: &mut LowerCtx<'_>, e: &Expr) {
    ctx.guard_sinks.insert(std::ptr::from_ref(e) as usize);
    let tail = |ctx: &mut LowerCtx<'_>, b: &Block| {
        if let Some(t) = &b.trailing {
            guard_sink(ctx, t);
        }
    };
    match &e.kind {
        ExprKind::Block(b) | ExprKind::Unsafe(b) => tail(ctx, b),
        ExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            tail(ctx, then_branch);
            if let Some(x) = else_branch {
                guard_sink(ctx, x);
            }
        }
        ExprKind::Match { arms, .. } => {
            for a in arms {
                guard_sink(ctx, &a.body);
            }
        }
        ExprKind::Catch { body, catches, .. } => {
            guard_sink(ctx, body);
            for c in catches {
                tail(ctx, &c.body);
            }
        }
        ExprKind::Try { operand, .. } => guard_sink(ctx, operand),
        _ => {}
    }
}

/// A guard `expr` produces outside a `guard_sink` is never released; a
/// guard variable read in one is moved to its new holder.
fn guard_flow(ctx: &mut LowerCtx<'_>, expr: &Expr, e: &HirExpr) {
    let Some(g) = lock_guard(ctx, &e.ty) else {
        return;
    };
    let sink = ctx
        .guard_sinks
        .contains(&(std::ptr::from_ref(expr) as usize));
    match &expr.kind {
        ExprKind::Path(p) => {
            if let (true, [seg]) = (sink, p.segments.as_slice()) {
                if let Some(l) = ctx.lookup(&seg.name) {
                    ctx.guard_lets.retain(|x| x.0 != l);
                }
            }
        }
        // Reads of a place, and forms whose value comes from inner ones.
        ExprKind::SelfLower
        | ExprKind::Field(..)
        | ExprKind::TupleField(..)
        | ExprKind::Index(..)
        | ExprKind::Block(_)
        | ExprKind::Unsafe(_)
        | ExprKind::If { .. }
        | ExprKind::Match { .. }
        | ExprKind::Catch { .. }
        | ExprKind::Try { .. }
        | ExprKind::Loop { .. } => {}
        _ if !sink => ctx.emit_error_at(
            rokc_diagnostic::ErrorKind::Type,
            expr.span,
            format!(
                "this `{g}` is never released: a guard received without `using` must be \
                 bound with `using g = .. {{ .. }}` (stdlib 2.16.3)"
            ),
        ),
        _ => {}
    }
}

pub(super) fn lower_expr(ctx: &mut LowerCtx<'_>, expr: &Expr) -> HirExpr {
    let e = ctx.at_span(expr.span, |ctx| match expr.kind {
        ExprKind::While { .. } | ExprKind::For { .. } | ExprKind::Loop { .. } => {
            lower_loop(ctx, expr)
        }
        _ => lower_expr_inner(ctx, expr),
    });
    guard_flow(ctx, expr, &e);
    if e.ty != HirTy::Never || hir_expr_diverges(&e) {
        return e;
    }
    // A call of a `-> !` function: emit it as a never-returning call
    // (MIR: call + `unreachable`) typed as the slot it sits in.
    let span = e.span;
    let ty = match ctx.current_expected() {
        Some(t) if *t != HirTy::Unknown => t.clone(),
        _ => HirTy::Never,
    };
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![HirStmt::Panic(HirExpr {
                ty: HirTy::Unit,
                ..e
            })],
            trailing: None,
            label: None,
        })),
        ty,
        span,
    }
}

/// `operand as ty` (spec 2.10.5): numeric / bool / char casts.
fn lower_cast(
    ctx: &mut LowerCtx<'_>,
    operand: &Expr,
    ty: &Ty,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let mark = ctx.probe_mark();
    let operand_h = lower_expected(ctx, operand, HirTy::Unknown);
    // In a monomorphized body `as T` names the concrete type.
    let to_ty = ctx.resolve_ast_ty(ty);
    let from_ty = operand_h.ty.clone();
    // `int` / `float`: a conversion call (stdlib 2.11.1).
    if super::bignum::big_of(ctx, &from_ty).is_some()
        || super::bignum::big_of(ctx, &to_ty).is_some()
    {
        ctx.rollback(&mark);
        return super::bignum::cast(ctx, operand, &from_ty, &to_ty, ty, span);
    }
    // A field-less enum casts as its stored tag (a value variant's value).
    let from_ty = match &from_ty {
        HirTy::Enum(id) if matches!(&to_ty, HirTy::Prim(p) if p.is_int()) => {
            let def = ctx.enum_def(*id);
            if def.variants.iter().all(|v| v.payload_ty.is_none()) {
                HirTy::Prim(def.repr.int.unwrap_or(PrimTy::I32))
            } else {
                from_ty
            }
        }
        _ => from_ty,
    };
    let castable = |t: &HirTy| matches!(t, HirTy::Prim(p) if p.is_int() || p.is_float() || matches!(p, PrimTy::Bool | PrimTy::CChar | PrimTy::Char));
    if !(from_ty == to_ty
        || matches!(from_ty, HirTy::Unknown)
        || matches!(to_ty, HirTy::Unknown)
        || (castable(&from_ty) && castable(&to_ty)))
    {
        let to_name = ctx.ty_name(&to_ty);
        if let Some(h) = super::wide_int_help(&to_name) {
            return ctx.error_expr(span, format!("cannot cast to `{to_name}`: {h}"));
        }
        return ctx.error_expr(
            span,
            format!(
                "cannot cast `{}` as `{}` (only numeric / bool / char casts){}",
                ctx.ty_name(&from_ty),
                ctx.ty_name(&to_ty),
                optional_cast_hint(ty)
            ),
        );
    }
    // Like Rust: only `u8` (always a scalar value) casts to
    // `char`; a wider value may be a surrogate or past U+10FFFF.
    if to_ty == HirTy::Prim(PrimTy::Char)
        && !matches!(
            from_ty,
            HirTy::Unknown | HirTy::Prim(PrimTy::U8 | PrimTy::Char)
        )
    {
        return ctx.error_expr(
            span,
            format!(
                "only `u8` can be cast as `char`, not `{}`; use `char::from_u32(n)` \
                 (`char | none`) for a wider value",
                ctx.ty_name(&from_ty)
            ),
        );
    }
    HirExpr {
        kind: HirExprKind::Cast {
            operand: Box::new(operand_h),
            from_ty,
            to_ty: to_ty.clone(),
        },
        ty: to_ty,
        span,
    }
}

/// `x as T?` casts to the type `T?` (spec 2.6.5); say so when the cast
/// fails, since `?` propagation was likely meant.
fn optional_cast_hint(ty: &Ty) -> &'static str {
    let optional = matches!(&ty.kind, AstTyKind::Sum(ps)
        if matches!(ps.as_slice(), [n, t] if n.nest_inner().is_some()
            && matches!(t.kind, AstTyKind::None)));
    if optional {
        "; the trailing `?` is part of the type `T?` - to propagate, write `(x as T)?`"
    } else {
        ""
    }
}

fn lower_expr_inner(ctx: &mut LowerCtx<'_>, expr: &Expr) -> HirExpr {
    let span = expr.span;
    match &expr.kind {
        ExprKind::Literal(AstLit::ByteStr(body)) => lower_byte_str(ctx, body, span),
        ExprKind::Literal(lit) => super::bignum::literal(ctx, lit, false, span)
            .unwrap_or_else(|| lower_literal(ctx, lit, false, span)),
        ExprKind::Path(p) => lower_path(ctx, p, span),
        ExprKind::QPath(q) => lower_qpath_value(ctx, expr, q, span),
        ExprKind::SelfLower => match ctx.lookup("self") {
            Some(local) => ctx.read_local(local, span),
            None => ctx.error_expr(span, "`self` is only available inside methods".to_owned()),
        },
        ExprKind::SelfUpper => ctx.error_expr(span, "`Self` is a type, not a value".to_owned()),
        ExprKind::Unsafe(b) => {
            ctx.unsafe_depth += 1;
            let block = Expr {
                kind: ExprKind::Block(b.clone()),
                span,
            };
            let e = lower_expr_inner(ctx, &block);
            ctx.unsafe_depth -= 1;
            e
        }
        ExprKind::Block(b) => {
            let labelled = b.label.is_some();
            let idx = ctx.breakables.len();
            if labelled {
                open_break_target(ctx, idx);
                ctx.breakables.push((b.label.clone(), false));
            }
            let block = lower_block(ctx, b);
            // Spec 3.9.10: a labelled block's value may come from
            // `break 'label value`.
            let break_ty = if labelled {
                ctx.breakables.pop();
                ctx.break_tys.remove(&idx).map(|(t, _)| t)
            } else {
                None
            };
            let ty = block
                .trailing
                .as_ref()
                .map_or_else(|| break_ty.clone().unwrap_or(HirTy::Unit), |e| e.ty.clone());
            HirExpr {
                kind: HirExprKind::Block(Box::new(block)),
                ty,
                span,
            }
        }
        ExprKind::OpInfix { op, .. } if super::broadcast::scalar_op(op).is_some() => {
            super::broadcast::lower(ctx, expr)
        }
        ExprKind::OpInfix { op, lhs, rhs } => lower_infix(ctx, op, lhs, rhs, span),
        ExprKind::OpPrefix { op, operand } => lower_prefix(ctx, op, operand, span),
        ExprKind::OpPostfix { op, operand } => {
            let operand_h = lower_expr(ctx, operand);
            if operand_h.ty == HirTy::Unknown {
                return operand_h;
            }
            dispatch_user_op(ctx, op, Fixity::Postfix, operand_h, None, span)
        }
        ExprKind::Call {
            callee,
            args,
            trailing_closure,
        } => {
            // Spec 4.8: trailing closure becomes the last arg.
            let all_args = with_trailing_closure(args, trailing_closure.as_deref());
            if let ExprKind::Path(p) = &callee.kind {
                check_direct_drop_path(ctx, p);
            }
            let since = ctx.env.sink.diagnostics.borrow().len();
            let call = match &callee.kind {
                ExprKind::QPath(q) => qpath_call(ctx, q, &all_args, span),
                _ => lower_call(ctx, callee, &all_args, span),
            };
            print_args_help(ctx, callee, &all_args, since);
            unpack_call(ctx, call)
        }
        ExprKind::MethodCall {
            receiver,
            method,
            args,
            trailing_closure,
            type_args,
        } => {
            let all_args = with_trailing_closure(args, trailing_closure.as_deref());
            let recv_h = literal_conv_receiver(ctx, receiver, method)
                .unwrap_or_else(|| lower_receiver(ctx, receiver));
            ctx.method_type_args.clone_from(type_args);
            let method = match drop_method(ctx, &recv_h.ty, method, span) {
                Ok(m) => m,
                Err(e) => return e,
            };
            if method == rokc_ast::USING_DROP_METHOD {
                if let HirExprKind::Local(l) = recv_h.kind {
                    ctx.guard_lets.retain(|g| g.0 != l);
                }
                let stmts = drop_glue(ctx, recv_h, span, 0);
                return HirExpr {
                    kind: HirExprKind::Block(Box::new(HirBlock {
                        stmts,
                        trailing: None,
                        label: None,
                    })),
                    ty: HirTy::Unit,
                    span,
                };
            }
            let call = method_call_on(ctx, recv_h, method, &all_args, span);
            unpack_call(ctx, call)
        }
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => lower_if(ctx, cond, then_branch, else_branch.as_deref(), span),
        ExprKind::While { label, cond, body } => {
            let cond_h = lower_cond(ctx, cond);
            ctx.breakables.push((label.clone(), true));
            ctx.push_expected(HirTy::Unit);
            let body_h = lower_narrowed_block(ctx, cond, body);
            ctx.pop_expected();
            ctx.breakables.pop();
            HirExpr {
                kind: HirExprKind::While {
                    label: label.clone(),
                    cond: Box::new(cond_h),
                    body: Box::new(body_h),
                },
                ty: HirTy::Unit,
                span,
            }
        }
        ExprKind::For {
            label,
            pat,
            iter,
            body,
            is_await,
        } => lower_for(ctx, label.as_ref(), pat, iter, body, *is_await, span),
        ExprKind::Match { scrutinee, arms } => {
            guard_sink(ctx, scrutinee);
            lower_match(ctx, scrutinee, arms, span)
        }
        ExprKind::Try { kind, operand } => lower_try(ctx, *kind, operand, span),
        ExprKind::Catch {
            body,
            catches,
            finally,
        } => lower_catch(ctx, body, catches, finally.as_ref(), span),
        ExprKind::StructInit {
            path,
            fields,
            spread,
        } => lower_struct_init(ctx, path, fields, spread.as_deref(), span),
        ExprKind::Field(target, name) => lower_field(ctx, target, name, span),
        ExprKind::Tuple(items) if items.is_empty() => HirExpr {
            kind: HirExprKind::Const(ConstVal::Unit),
            ty: HirTy::Unit,
            span,
        },
        ExprKind::Tuple(items) => {
            let expected: Option<Vec<HirTy>> = match ctx.current_expected() {
                Some(HirTy::Tuple(ts)) if ts.len() == items.len() => Some(ts.clone()),
                _ => None,
            };
            let lowered: Vec<HirExpr> = items
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let slot = expected.as_ref().map_or(HirTy::Unknown, |ts| ts[i].clone());
                    let v = lower_expected(ctx, e, slot.clone());
                    wrap_into_sum(&slot, v)
                })
                .collect();
            let tys: Vec<HirTy> = lowered.iter().map(|e| e.ty.clone()).collect();
            HirExpr {
                ty: HirTy::Tuple(tys),
                span,
                kind: HirExprKind::Tuple { fields: lowered },
            }
        }
        ExprKind::TupleField(target, index) => {
            let target_h = lower_expected(ctx, target, HirTy::Unknown);
            let i = *index as usize;
            match target_h.ty.clone() {
                HirTy::Tuple(ts) => match ts.get(i) {
                    Some(field_ty) => HirExpr {
                        ty: field_ty.clone(),
                        span,
                        kind: HirExprKind::TupleField {
                            target: Box::new(target_h),
                            index: *index,
                            field_ty: field_ty.clone(),
                        },
                    },
                    None => ctx.error_expr(
                        span,
                        format!("tuple index {index} out of range for a {}-tuple", ts.len()),
                    ),
                },
                // Spec 2.7.4: tuple-form structs (`struct Foo(i32);`)
                // expose their fields via the same `.N` syntax.
                HirTy::Struct(sid) => {
                    let def = ctx.struct_def(sid);
                    match def.fields.get(i) {
                        Some(f) if f.name == i.to_string() => {
                            ctx.check_field_visible(sid, &f.name);
                            HirExpr {
                                ty: f.ty.clone(),
                                span,
                                kind: HirExprKind::FieldRead {
                                    target: Box::new(target_h),
                                    struct_id: sid,
                                    field_index: *index,
                                    field_ty: f.ty.clone(),
                                },
                            }
                        }
                        _ => ctx.error_expr(
                            span,
                            format!("struct `{}` has no field `{index}`", def.name),
                        ),
                    }
                }
                HirTy::Unknown => target_h,
                other => ctx.error_expr(
                    span,
                    format!("no field `{index}` on type `{}`", ctx.ty_name(&other)),
                ),
            }
        }
        ExprKind::List(items) => lower_list_lit(ctx, items, span),
        ExprKind::ArrayRepeat { value, len } => lower_array_repeat(ctx, value, len, span),
        // `xs[a..b]` / `xs[..]` (stdlib 2.9.7 spelling) is `xs[a:b]` / `xs[:]`.
        ExprKind::Index(arr, idx)
            if matches!(
                &idx.kind,
                ExprKind::Range {
                    inclusive: false,
                    ..
                }
            ) =>
        {
            let ExprKind::Range { start, end, .. } = &idx.kind else {
                unreachable!()
            };
            lower_slice(ctx, arr, start.as_deref(), end.as_deref(), None, span)
        }
        ExprKind::Index(arr, idx) => {
            let arr_h = lower_expected(ctx, arr, HirTy::Unknown);
            // A struct / enum indexes through `Index::index` (spec 3.6).
            if matches!(arr_h.ty, HirTy::Struct(_) | HirTy::Enum(_)) {
                return index_call(ctx, arr_h, "index", idx, &[], span);
            }
            // `GcBuf<T>`'s `Index` is the unchecked `rok.buf_get` primitive.
            if let HirTy::GcBuf(elem) = arr_h.ty.clone() {
                return buf_access(ctx, arr_h, *elem, idx, None, span);
            }
            let idx_h = match arr_h.ty {
                HirTy::Array(_, n) => array_position(ctx, idx, n),
                _ => lower_expected(ctx, idx, HirTy::Unknown),
            };
            let elem_ty = match &arr_h.ty {
                HirTy::Array(t, _) => (**t).clone(),
                HirTy::Unknown => return arr_h,
                other => {
                    let e = ctx.error_expr(
                        span,
                        format!("cannot index into a value of type `{}`", ctx.ty_name(other)),
                    );
                    if *other == HirTy::Prim(PrimTy::Str) {
                        str_index_help(ctx, arr, idx);
                    }
                    return e;
                }
            };
            HirExpr {
                ty: elem_ty.clone(),
                span,
                kind: HirExprKind::ArrayIndex {
                    array: Box::new(arr_h),
                    index: Box::new(idx_h),
                    elem_ty,
                },
            }
        }
        ExprKind::Loop { label, body } => {
            let idx = ctx.breakables.len();
            open_break_target(ctx, idx);
            ctx.breakables.push((label.clone(), true));
            let body_h = lower_loop_body(ctx, body);
            ctx.breakables.pop();
            let result_ty = ctx.break_tys.remove(&idx).map_or(HirTy::Unit, |(t, _)| t);
            // No `break` out: `loop {}` has type `!`.
            let ty = if block_breaks_out(&body_h, label.as_deref(), true) {
                result_ty.clone()
            } else {
                HirTy::Never
            };
            HirExpr {
                kind: HirExprKind::Loop {
                    label: label.clone(),
                    body: Box::new(body_h),
                    result_ty,
                },
                ty,
                span,
            }
        }
        ExprKind::Closure {
            captures,
            params,
            ret_ty,
            body,
            is_async,
            is_generator,
        } => {
            let body = super::coro::closure_body(body, *is_async, *is_generator);
            lower_closure_expr(ctx, captures, params, ret_ty.as_ref(), &body, span)
        }
        ExprKind::QuestionDot(receiver, tails) => {
            let recv_h = lower_expected(ctx, receiver, HirTy::Unknown);
            desugar_question_dot(ctx, recv_h, tails, span)
        }
        ExprKind::TryQ(inner) => {
            let since = ctx.env.sink.diagnostics.borrow().len();
            let inner_h = lower_expected(ctx, inner, HirTy::Unknown);
            lower_try_q(ctx, inner, inner_h, since, span)
        }
        ExprKind::NullishCoalesce(lhs, rhs) => {
            let lhs_h = lower_expected(ctx, lhs, HirTy::Unknown);
            desugar_nullish_coalesce(ctx, lhs_h, rhs, span)
        }
        ExprKind::Cast(operand, ty) => lower_cast(ctx, operand, ty, span),
        ExprKind::Await(inner) => super::coro::lower_await(ctx, inner, span),
        ExprKind::InModule(module, inner) => {
            let module = std::mem::replace(&mut ctx.current_module, module.clone());
            let scopes = std::mem::replace(&mut ctx.scopes, vec![HashMap::new()]);
            let e = lower_expr(ctx, inner);
            ctx.scopes = scopes;
            ctx.current_module = module;
            e
        }
        ExprKind::Yield(value) => super::coro::lower_yield(ctx, value.as_deref(), span),
        ExprKind::Range {
            start,
            end,
            inclusive,
        } => lower_range_value(ctx, start.as_deref(), end.as_deref(), *inclusive, span),
        ExprKind::Dict(pairs) => {
            let items: Vec<Expr> = pairs
                .iter()
                .map(|(k, v)| Expr {
                    kind: ExprKind::Tuple(vec![k.clone(), v.clone()]),
                    span: k.span,
                })
                .collect();
            let args = vec![rokc_ast::LitArg::Many(items)];
            lower_shape(ctx, rokc_parser::Shape::Dict, args, "`Dict`", span)
        }
        ExprKind::Set(xs) => {
            let args = vec![rokc_ast::LitArg::Many(xs.clone())];
            lower_shape(ctx, rokc_parser::Shape::Set, args, "`Set`", span)
        }
        ExprKind::MagicLiteral(cands) => lower_magic(ctx, cands, span),
        ExprKind::Is(operand, pat) => lower_is(ctx, operand, pat, span),
        ExprKind::Slice {
            target,
            start,
            end,
            step,
        } => lower_slice(
            ctx,
            target,
            start.as_deref(),
            end.as_deref(),
            step.as_deref(),
            span,
        ),
        ExprKind::AsyncBlock(_) | ExprKind::YieldBlock(_) | ExprKind::AsyncYieldBlock(_) => {
            super::coro::lower_coro_expr(ctx, &expr.kind, span)
        }
        ExprKind::Broadcast { .. } => super::broadcast::lower(ctx, expr),
        ExprKind::FString(parts) => super::fstring::lower_fstring(ctx, parts, span),
    }
}

/// `if` / `while` conditions are `bool`.
pub(super) fn lower_cond(ctx: &mut LowerCtx<'_>, cond: &Expr) -> HirExpr {
    let c = lower_expected(ctx, cond, HirTy::Prim(PrimTy::Bool));
    coerce(ctx, &HirTy::Prim(PrimTy::Bool), c, &|| {
        "the condition".to_owned()
    })
}

/// Loop bodies yield no value (expected `()`).
pub(super) fn lower_loop_body(ctx: &mut LowerCtx<'_>, body: &Block) -> HirBlock {
    ctx.push_expected(HirTy::Unit);
    let b = lower_block(ctx, body);
    ctx.pop_expected();
    b
}

/// Open a `loop` / labelled block at `breakables[idx]`; an annotated slot
/// types its `break` values.
pub(super) fn open_break_target(ctx: &mut LowerCtx<'_>, idx: usize) {
    ctx.break_tys.remove(&idx);
    if let Some(t) = ctx
        .current_expected()
        .filter(|t| !matches!(t, HirTy::Unknown | HirTy::Unit))
    {
        ctx.break_tys.insert(idx, (t.clone(), false));
    }
}

/// `break value` to the target at `depth`: typed by the slot, else by the
/// first `break`, which the others must match.
pub(super) fn lower_break_value(ctx: &mut LowerCtx<'_>, depth: usize, e: &Expr) -> HirExpr {
    let slot = ctx.break_tys.get(&depth).cloned();
    let v = lower_expected(
        ctx,
        e,
        slot.as_ref().map_or(HirTy::Unknown, |(t, _)| t.clone()),
    );
    match slot {
        None => {
            if v.ty != HirTy::Unknown && !hir_expr_diverges(&v) {
                ctx.break_tys.insert(depth, (v.ty.clone(), true));
            }
            v
        }
        // An earlier bare `break;` made the loop's value `()`.
        Some((HirTy::Unit, true))
            if !matches!(v.ty, HirTy::Unknown | HirTy::Unit) && !hir_expr_diverges(&v) =>
        {
            let t = ctx.ty_name(&v.ty);
            let h = ctx.error_expr(
                v.span,
                format!(
                    "`break` with a `{t}` value, but an earlier `break` of this loop has no \
                     value (spec 3.13.1)"
                ),
            );
            ctx.help_last(Some(format!(
                "write `break <value>;` with a `{t}` value for the bare `break`, or remove \
                 this value"
            )));
            h
        }
        Some((t, true)) if v.ty != HirTy::Unknown && !coercible(ctx, &t, &v.ty) => ctx.error_expr(
            v.span,
            format!(
                "`break` values have different types `{}` and `{}` - rok does not infer \
                     a sum type; annotate the slot (`let v: {} | {} = loop ...`)",
                ctx.ty_name(&t),
                ctx.ty_name(&v.ty),
                ctx.ty_name(&t),
                ctx.ty_name(&v.ty)
            ),
        ),
        Some((t, _)) => coerce(ctx, &t, v, &|| "this `break` value".to_owned()),
    }
}

pub(super) fn enum_base_name(name: &str) -> &str {
    name.split("__").next().unwrap_or(name)
}

/// Resolve an enum variant path. A generic enum's instance comes from
/// `hint`, else from the one instance whose payload fits `arg_tys`.
pub(super) fn resolve_enum_variant(
    ctx: &LowerCtx<'_>,
    segs: &[&str],
    hint: Option<&HirTy>,
    arg_tys: Option<&[HirTy]>,
) -> EnumRes {
    let Some((full, vname)) = ctx.enum_path(segs) else {
        return EnumRes::NotEnum;
    };
    let variant_of = |eid: EnumId| {
        ctx.enum_def(eid)
            .variants
            .iter()
            .position(|v| v.name == vname)
    };
    let of_full = |e: &EnumId| enum_base_name(&ctx.enum_def(*e).name) == full;
    // The expected type, or the one member of an expected sum
    // (`-> Result<T, E> | none`) that is an instance of this enum.
    let hinted: Option<EnumId> = match hint {
        Some(HirTy::Enum(e)) if of_full(e) => Some(*e),
        Some(HirTy::Sum(parts)) => {
            let mut hits = parts.iter().filter_map(|p| match p {
                HirTy::Enum(e) if of_full(e) => Some(*e),
                _ => None,
            });
            hits.next().filter(|_| hits.next().is_none())
        }
        _ => None,
    };
    let eid = if let Some(h) = hinted {
        h
    } else if let Some(eid) = ctx.lookup_enum(&full) {
        eid
    } else {
        let Some(def) = ctx.env.defs.generic_enum(&full) else {
            return EnumRes::NotEnum;
        };
        match infer_enum_instance(ctx, def, &full, &vname, arg_tys, segs) {
            Some(eid) => eid,
            None => return EnumRes::Error,
        }
    };
    if let Some(i) = variant_of(eid) {
        EnumRes::Found(eid, i)
    } else if pick_method(ctx, &HirTy::Enum(eid), &vname, None).is_ok_and(|p| p.is_some()) {
        // `Mode::default()`: an associated function, not a variant.
        EnumRes::NotEnum
    } else {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!("enum `{full}` has no variant `{vname}`"),
        );
        EnumRes::Error
    }
}

/// The instance of generic enum `def` built by `vname(arg_tys)`: each type
/// argument from the payload (`Pair::Two(1, "a")`: `Pair<i32, str>`), else
/// its default, else a `let` inference variable; `None` (reported) if one
/// stays open.
fn infer_enum_instance(
    ctx: &LowerCtx<'_>,
    def: &rokc_ast::EnumDef,
    full: &str,
    vname: &str,
    arg_tys: Option<&[HirTy]>,
    segs: &[&str],
) -> Option<EnumId> {
    let names: std::collections::HashSet<String> = def
        .generics
        .params
        .iter()
        .map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. }
            | rokc_ast::GenericParam::Const { name, .. } => name.clone(),
        })
        .collect();
    let Some(variant) = def.variants.iter().find(|v| v.name == vname) else {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!("enum `{full}` has no variant `{vname}`"),
        );
        return None;
    };
    let payload: Vec<&Ty> = match &variant.kind {
        rokc_ast::EnumVariantKind::Tuple(ts) => ts.iter().collect(),
        rokc_ast::EnumVariantKind::Named(fs) => fs.iter().map(|f| &f.ty).collect(),
        rokc_ast::EnumVariantKind::Unit | rokc_ast::EnumVariantKind::Value(_) => Vec::new(),
    };
    let mut subst: HashMap<String, HirTy> = HashMap::new();
    for (t, a) in payload.iter().zip(arg_tys.unwrap_or_default()) {
        if *a != HirTy::Unknown {
            unify_param_ty_with(t, a, &names, &mut subst, ctx);
        }
    }
    let mut args = Vec::new();
    for gp in &def.generics.params {
        let arg = match gp {
            rokc_ast::GenericParam::Type { name, default, .. } => subst
                .get(name)
                .cloned()
                .filter(|t| *t != HirTy::Unknown)
                .or_else(|| default.as_ref().map(|d| ctx.resolve_ast_ty(d))),
            rokc_ast::GenericParam::Const { name, ty } => subst
                .get(name)
                .and_then(HirTy::as_const)
                .and_then(|v| super::mono::typed_const(ctx, name, ty, v)),
        };
        match arg {
            Some(t) => args.push(t),
            None if ctx.infer_vars => args.push(super::infer::fresh(ctx)),
            None => {
                ctx.emit_uninferred(format!(
                    "cannot infer the type arguments of `{full}` for `{}` - annotate the \
                     binding with the concrete type",
                    segs.join("::")
                ));
                return None;
            }
        }
    }
    ctx.instantiate_enum(full, args, ctx.here())
}

/// `segs` names an enum (or generic enum base) that has no variant of
/// that name - so `E::f(..)` is an associated function call.
fn enum_lacks_variant(ctx: &LowerCtx<'_>, segs: &[&str]) -> bool {
    let Some((full, vname)) = ctx.enum_path(segs) else {
        return false;
    };
    if let Some(e) = ctx.lookup_enum(&full) {
        return ctx.enum_def(e).variants.iter().all(|v| v.name != vname);
    }
    ctx.env
        .defs
        .generic_enum(&full)
        .is_some_and(|d| d.variants.iter().all(|v| v.name != vname))
}

/// Payload slot types of a variant, one per constructor argument.
pub(super) fn variant_arg_tys(v: &HirEnumVariant) -> Vec<HirTy> {
    match (&v.payload_ty, v.arity) {
        (None, _) => Vec::new(),
        // A named-field variant's payload is always the tuple of fields.
        (Some(HirTy::Tuple(ps)), _) if !v.named_fields.is_empty() => ps.clone(),
        (Some(t), 1) => vec![t.clone()],
        (Some(HirTy::Tuple(ps)), _) => ps.clone(),
        (Some(t), _) => vec![t.clone()],
    }
}

fn check_duplicate_fields(ctx: &LowerCtx<'_>, segs: &[&str], fields: &[(String, Expr)]) {
    let mut seen = std::collections::HashSet::new();
    for (n, _) in fields {
        if !seen.insert(n.as_str()) {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "field `{n}` specified more than once in `{}` literal",
                    segs.join("::")
                ),
            );
        }
    }
}

/// A struct's declared `(field, type)`s; a tuple struct's are `0`, `1`, ...
fn struct_decl_fields(def: &AstStructDef) -> Vec<(String, &Ty)> {
    match &def.kind {
        AstStructKind::Named(fs) => fs.iter().map(|f| (f.name.clone(), &f.ty)).collect(),
        AstStructKind::Tuple(fs) => fs
            .iter()
            .enumerate()
            .map(|(i, f)| (i.to_string(), &f.ty))
            .collect(),
        AstStructKind::Unit => Vec::new(),
    }
}

/// Parameters and declared fields of generic struct or union `full`.
fn generic_template<'a>(
    ctx: &LowerCtx<'a>,
    full: &str,
) -> Option<(&'a rokc_ast::Generics, Vec<(String, &'a Ty)>)> {
    if let Some(d) = ctx.env.defs.generic_struct(full) {
        return Some((&d.generics, struct_decl_fields(d)));
    }
    let u = ctx.env.defs.generic_union(full)?;
    Some((
        &u.generics,
        u.fields.iter().map(|f| (f.name.clone(), &f.ty)).collect(),
    ))
}

/// `Self` of an impl of a struct, as a struct path (spec 4.9.4).
pub(super) fn self_struct_id(ctx: &LowerCtx<'_>, segs: &[&str]) -> Option<StructId> {
    match (segs, &ctx.self_ty) {
        (["Self"], Some(HirTy::Struct(id))) => Some(*id),
        _ => None,
    }
}

/// `E::<i32>::V`: the enum instance its written arguments name (they
/// take the place of the expected type).
fn explicit_enum(ctx: &LowerCtx<'_>, p: &AstPath) -> Option<HirTy> {
    let [.., e, _] = p.segments.as_slice() else {
        return None;
    };
    if e.generics.is_empty() {
        return None;
    }
    let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
    ctx.enum_path(&segs)?;
    let n = p.segments.len();
    Some(ctx.resolve_ast_ty(&Ty {
        kind: AstTyKind::Path(AstPath {
            segments: p.segments[..n - 1].to_vec(),
            span: p.span,
        }),
        span: p.span,
    }))
}

/// The variant a struct-literal path names, if any. A non-core struct
/// beats a same-named core variant from the implicit `rok::core::**`
/// (`struct Sha256` vs `HashAlgorithm::Sha256`).
fn init_variant(ctx: &LowerCtx<'_>, segs: &[&str], hint: Option<&HirTy>) -> EnumRes {
    let shadowed = segs.len() == 1
        && ctx
            .canon_type(segs)
            .is_some_and(|t| !t.starts_with("rok::core::"))
        && ctx
            .enum_path(segs)
            .is_some_and(|(e, _)| e.starts_with("rok::core::"));
    if shadowed {
        return EnumRes::NotEnum;
    }
    resolve_enum_variant(ctx, segs, hint, None)
}

/// `S { a: v, .. }`, or with `spread` `S { a: v, ..base }` (spec 2.3.4):
/// unwritten fields come from `base`, evaluated after the written ones.
#[allow(clippy::cognitive_complexity)] // struct, enum variant, `Self` and generic forms
pub(super) fn lower_struct_init(
    ctx: &mut LowerCtx<'_>,
    path: &AstPath,
    fields: &[(String, Expr)],
    spread: Option<&Expr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let segs: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
    check_duplicate_fields(ctx, &segs, fields);
    // `Enum::Variant { x: .., y: .. }` (spec 2.4): fields packed into the
    // tuple payload in declaration order.
    let mut hint = explicit_enum(ctx, path).or_else(|| ctx.current_expected().cloned());
    match init_variant(ctx, &segs, hint.as_ref()) {
        EnumRes::Error => return poison(span),
        EnumRes::Found(..) if spread.is_some() => {
            return ctx.error_expr(
                span,
                format!(
                    "`..base` needs a struct; `{}` is an enum variant (spec 2.3.4)",
                    segs.join("::")
                ),
            );
        }
        EnumRes::Found(enum_id, idx) => {
            let variant = ctx.enum_def(enum_id).variants[idx].clone();
            if variant.named_fields.is_empty() {
                return ctx.error_expr(
                    span,
                    format!("`{}` is not a struct-like enum variant", segs.join("::")),
                );
            }
            let field_tys = variant_arg_tys(&variant);
            let mut payload_args: Vec<Option<HirExpr>> = vec![None; variant.named_fields.len()];
            for (n, e) in fields {
                match variant.named_fields.iter().position(|f| f == n) {
                    Some(i) => {
                        let v = lower_expected(ctx, e, field_tys[i].clone());
                        payload_args[i] =
                            Some(coerce(ctx, &field_tys[i], v, &|| format!("field `{n}`")));
                    }
                    None => {
                        ctx.emit_error(
                            rokc_diagnostic::ErrorKind::Type,
                            format!(
                                "enum variant `{}` has no field `{n}` (spec 2.4)",
                                segs.join("::")
                            ),
                        );
                    }
                }
            }
            let mut args = Vec::with_capacity(payload_args.len());
            for (i, a) in payload_args.into_iter().enumerate() {
                if let Some(a) = a {
                    args.push(a);
                } else {
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!(
                            "enum variant `{}` is missing field `{}` (spec 2.4)",
                            segs.join("::"),
                            variant.named_fields[i]
                        ),
                    );
                    args.push(poison(span));
                }
            }
            return HirExpr {
                kind: HirExprKind::EnumVariantInit {
                    enum_id,
                    variant_index: u32::try_from(idx).expect("variant overflow"),
                    args,
                },
                ty: HirTy::Enum(enum_id),
                span,
            };
        }
        EnumRes::NotEnum => {}
    }

    let base_h = spread.map(|b| lower_update_base(ctx, b, &mut hint));
    let self_struct = self_struct_id(ctx, &segs);
    let full =
        self_struct.map_or_else(|| ctx.canon_type(&segs), |id| Some(ctx.struct_def(id).name));
    let Some(full) = full else {
        for (_, e) in fields {
            lower_expected(ctx, e, HirTy::Unknown);
        }
        let help = ctx.type_help(&segs);
        return ctx.unknown_name(
            span,
            format!("cannot find struct `{}` in this scope", segs.join("::")),
            help,
        );
    };
    // Values lowered before the struct is known (generic inference);
    // reused below so each field expression is evaluated exactly once.
    let mut pre: Vec<Option<HirExpr>> = vec![None; fields.len()];
    let explicit_generics = path.segments.last().is_some_and(|s| !s.generics.is_empty());
    let struct_id = if let Some(id) = self_struct {
        id
    } else if explicit_generics {
        let synth = Ty {
            kind: AstTyKind::Path(path.clone()),
            span: path.span,
        };
        let errors = ctx.env.sink.diagnostics.borrow().len();
        match ctx.resolve_ast_ty(&synth) {
            HirTy::Struct(id) => id,
            // Resolving reported why.
            _ if ctx.env.sink.diagnostics.borrow().len() > errors => return poison(span),
            _ => {
                return ctx.error_expr(
                    span,
                    format!(
                        "cannot instantiate `{}` with these type arguments",
                        segs.join("::")
                    ),
                );
            }
        }
    } else if let Some(id) = ctx.lookup_struct(&full) {
        id
    } else if let Some((generics, decl_fields)) = generic_template(ctx, &full) {
        // `Box { v: 5 }`: the expected type picks the instantiation;
        // otherwise infer the type arguments from the field values.
        match &hint {
            Some(HirTy::Struct(h))
                if enum_base_name(&ctx.struct_def(*h).name) == full
                    && !ctx.struct_def(*h).generic_args.is_empty() =>
            {
                *h
            }
            _ => {
                let generic_names: std::collections::HashSet<String> = generics
                    .params
                    .iter()
                    .map(|gp| match gp {
                        rokc_ast::GenericParam::Type { name, .. }
                        | rokc_ast::GenericParam::Const { name, .. } => name.clone(),
                    })
                    .collect();
                let mut subst: HashMap<String, HirTy> = HashMap::new();
                for (i, (n, e)) in fields.iter().enumerate() {
                    let v = lower_expected(ctx, e, HirTy::Unknown);
                    if let Some((_, ty)) = decl_fields.iter().find(|(name, _)| name == n) {
                        unify_param_ty_with(ty, &v.ty, &generic_names, &mut subst, ctx);
                    }
                    pre[i] = Some(v);
                }
                let mut args = Vec::new();
                for gp in &generics.params {
                    if let rokc_ast::GenericParam::Const { name, ty } = gp {
                        let c = subst
                            .get(name)
                            .and_then(HirTy::as_const)
                            .and_then(|v| super::mono::typed_const(ctx, name, ty, v));
                        let Some(c) = c else {
                            ctx.emit_uninferred(format!(
                                "cannot infer const parameter `{name}` of `{full}` - \
                                 annotate the binding (`let v: {}<..> = ..`)",
                                segs.join("::")
                            ));
                            return poison(span);
                        };
                        args.push(c);
                    }
                    if let rokc_ast::GenericParam::Type { name, default, .. } = gp {
                        match subst
                            .get(name)
                            .cloned()
                            .or_else(|| default.as_ref().map(|d| ctx.resolve_ast_ty(d)))
                        {
                            Some(t) if t != HirTy::Unknown => args.push(t),
                            Some(_) => return poison(span),
                            None if ctx.infer_vars => args.push(super::infer::fresh(ctx)),
                            None => {
                                ctx.emit_uninferred(format!(
                                    "cannot infer type parameter `{name}` of `{full}` - \
                                     write `{}<...> {{ ... }}`",
                                    segs.join("::")
                                ));
                                ctx.tag_last(rokc_diagnostic::Tag::TypeParamInfer);
                                return poison(span);
                            }
                        }
                    }
                }
                match ctx.instantiate_struct(&full, args, ctx.here()) {
                    Some(id) => id,
                    None => return poison(span),
                }
            }
        }
    } else {
        return ctx.error_expr(span, format!("`{full}` is not a struct"));
    };
    let def = ctx.struct_def(struct_id);
    let ty_display = ctx.ty_name(&HirTy::Struct(struct_id));
    // `union { f: v }`: exactly one field, cast/bitcast into the
    // synthetic `__data` slot.
    if let Some(union_kind) = def.union_kind.clone() {
        if spread.is_some() {
            return ctx.error_expr(span, format!("`..base` cannot build union `{ty_display}`"));
        }
        if fields.len() != 1 {
            return ctx.error_expr(
                span,
                format!(
                    "union `{ty_display}` initializer must list exactly one field, found {}",
                    fields.len()
                ),
            );
        }
        let (fname, value) = &fields[0];
        let Some(field_ty) = union_kind
            .aliases
            .iter()
            .find(|(n, _)| n == fname)
            .map(|(_, t)| t.clone())
        else {
            return ctx.no_field(struct_id, fname, span);
        };
        ctx.check_field_visible(struct_id, fname);
        let lowered = lower_expected(ctx, value, field_ty.clone());
        let lowered = coerce(ctx, &field_ty, lowered, &|| format!("field `{fname}`"));
        let storage_ty = def.fields[0].ty.clone();
        let casted = HirExpr {
            kind: HirExprKind::UnionWrite {
                struct_id,
                from_ty: lowered.ty.clone(),
                storage_ty: storage_ty.clone(),
                value: Box::new(lowered),
            },
            ty: storage_ty,
            span,
        };
        return HirExpr {
            kind: HirExprKind::StructInit {
                struct_id,
                fields: vec![casted],
            },
            ty: HirTy::Struct(struct_id),
            span,
        };
    }
    let mut ordered: Vec<Option<HirExpr>> = vec![None; def.fields.len()];
    for (i, (name, value)) in fields.iter().enumerate() {
        let Some(idx) = def.fields.iter().position(|f| f.name == *name) else {
            if pre[i].is_none() {
                lower_expected(ctx, value, HirTy::Unknown);
            }
            ctx.no_field(struct_id, name, span);
            continue;
        };
        ctx.check_field_visible(struct_id, name);
        let target_ty = def.fields[idx].ty.clone();
        let lowered = match pre[i].take() {
            Some(v) => v,
            None => lower_expected(ctx, value, target_ty.clone()),
        };
        ordered[idx] = Some(coerce(ctx, &target_ty, lowered, &|| {
            format!("field `{name}`")
        }));
    }
    if let Some(base_h) = base_h {
        return struct_update(ctx, struct_id, ordered, base_h, span);
    }
    // A missing field takes its declared default, else it is an error.
    let mut out = Vec::with_capacity(ordered.len());
    for (f, v) in def.fields.iter().zip(ordered) {
        match (v, &f.default) {
            (Some(v), _) => out.push(v),
            (None, Some(default_expr)) => {
                out.push(field_default(ctx, &def.name, &f.name, &f.ty, default_expr));
            }
            (None, None) => {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!(
                        "struct `{ty_display}` is missing field `{}` and no default is \
                         declared",
                        f.name
                    ),
                );
                out.push(poison(span));
            }
        }
    }
    HirExpr {
        kind: HirExprKind::StructInit {
            struct_id,
            fields: out,
        },
        ty: HirTy::Struct(struct_id),
        span,
    }
}

/// Field default of `owner` (spec 2.3.3): a constant expression resolved in
/// the struct's module, evaluated and embedded like a const.
pub(super) fn field_default(
    ctx: &mut LowerCtx<'_>,
    owner: &str,
    field: &str,
    ty: &HirTy,
    value: &Expr,
) -> HirExpr {
    let module = module_of(enum_base_name(owner));
    let saved_module = std::mem::replace(&mut ctx.current_module, module);
    let saved_uses = std::mem::replace(&mut ctx.local_uses, vec![Vec::new()]);
    let saved_scopes = std::mem::replace(&mut ctx.scopes, vec![HashMap::new()]);
    let v = lower_expected(ctx, value, ty.clone());
    ctx.current_module = saved_module;
    ctx.local_uses = saved_uses;
    ctx.scopes = saved_scopes;
    let what = format!("the default of field `{field}`");
    let v = coerce(ctx, ty, v, &|| what.clone());
    super::const_eval::constant(ctx, &what, v)
}

/// The `..base` of a struct update, lowered against the expected
/// struct; its type is the hint for a generic struct's arguments.
fn lower_update_base(ctx: &mut LowerCtx<'_>, base: &Expr, hint: &mut Option<HirTy>) -> HirExpr {
    let want = match hint {
        Some(t @ HirTy::Struct(_)) => t.clone(),
        _ => HirTy::Unknown,
    };
    let h = lower_expected(ctx, base, want);
    if matches!(h.ty, HirTy::Struct(_)) && !matches!(hint, Some(HirTy::Struct(_))) {
        *hint = Some(h.ty.clone());
    }
    h
}

/// `{ let #f.. = written values; let #b = base; S { .., f: #b.f } }`.
fn struct_update(
    ctx: &mut LowerCtx<'_>,
    struct_id: StructId,
    written: Vec<Option<HirExpr>>,
    base_h: HirExpr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let ty = HirTy::Struct(struct_id);
    let base_h = coerce(ctx, &ty, base_h, &|| "the `..` base".to_owned());
    let def = ctx.struct_def(struct_id);
    let mut lets = Vec::new();
    let mut vals = Vec::with_capacity(written.len());
    for v in written {
        vals.push(v.map(|v| {
            let vty = v.ty.clone();
            let local = ctx.declare(None, vty.clone());
            lets.push(HirStmt::Let { local, init: v });
            hir_local(local, &vty, span)
        }));
    }
    let base = ctx.declare(None, ty.clone());
    lets.push(HirStmt::Let {
        local: base,
        init: base_h,
    });
    let out = def
        .fields
        .iter()
        .zip(vals)
        .map(|(f, v)| {
            v.unwrap_or_else(|| field_access(ctx, hir_local(base, &ty, span), &f.name, span))
        })
        .collect();
    with_lets(
        lets,
        HirExpr {
            kind: HirExprKind::StructInit {
                struct_id,
                fields: out,
            },
            ty,
            span,
        },
    )
}

// `expr.field` on a struct.
pub(super) fn lower_field(
    ctx: &mut LowerCtx<'_>,
    target: &Expr,
    name: &str,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let target_h = lower_expected(ctx, target, HirTy::Unknown);
    let has_field = match &target_h.ty {
        HirTy::Struct(id) => {
            let def = ctx.struct_def(*id);
            def.fields.iter().any(|f| f.name == name)
                || def
                    .union_kind
                    .as_ref()
                    .is_some_and(|u| u.aliases.iter().any(|(n, _)| n == name))
        }
        HirTy::Unknown => true,
        _ => false,
    };
    // Spec 3.5.4: `obj.method` is not a function value.
    if !has_field && matches!(pick_method(ctx, &target_h.ty, name, None), Ok(Some(_))) {
        let recv = rokc_ast::print::print_expr(target);
        let r = ctx.error_expr(
            span,
            format!(
                "`{recv}.{name}` names a method without calling it; `.method` is not a \
                 function value (spec 3.5.4)"
            ),
        );
        ctx.help_last(Some(format!(
            "call it (`{recv}.{name}(..)`), wrap it in a closure (`|..| {recv}.{name}(..)`), \
             or use the method path `{}::{name}`",
            ctx.ty_name(&target_h.ty)
        )));
        return r;
    }
    if !has_field && name == "await" {
        let recv = rokc_ast::print::print_expr(target);
        let r = ctx.error_expr(
            span,
            "`await` goes before the expression, not after it".to_owned(),
        );
        ctx.help_last(Some(format!("write `await {recv}`")));
        return r;
    }
    field_access(ctx, target_h, name, span)
}

/// Read field `name` of an already-lowered struct / union value.
pub(super) fn field_access(
    ctx: &LowerCtx<'_>,
    target_h: HirExpr,
    name: &str,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let struct_id = match &target_h.ty {
        HirTy::Struct(id) => *id,
        HirTy::Unknown => return target_h,
        // An unknown type name: reported where it is written.
        HirTy::Path(_) => return poison(span),
        other => {
            return ctx.error_expr(
                span,
                format!("no field `{name}` on type `{}`", ctx.ty_name(other)),
            );
        }
    };
    let def = ctx.struct_def(struct_id);
    // A union field read is a `UnionRead`: a cast/bitcast of the one
    // `__data` slot.
    if let Some(union_kind) = def.union_kind.clone() {
        // Spec 4.15.2: reinterpreting the bits can make an invalid value
        // (a bad `bool`, a forged GC reference).
        if ctx.unsafe_depth == 0 {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "reading field `{name}` of union `{}` needs an `unsafe` block \
                     (spec 4.15.2: the bits may not be a valid `{name}`)",
                    ctx.ty_name(&HirTy::Struct(struct_id))
                ),
            );
        }
        let storage_ty = def.fields[0].ty.clone();
        let Some(field_ty) = union_kind
            .aliases
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.clone())
        else {
            return ctx.no_field(struct_id, name, span);
        };
        ctx.check_field_visible(struct_id, name);
        return HirExpr {
            kind: HirExprKind::UnionRead {
                struct_id,
                target: Box::new(target_h),
                field_name: name.to_owned(),
                storage_ty,
                field_ty: field_ty.clone(),
            },
            ty: field_ty,
            span,
        };
    }
    let Some(idx) = def.fields.iter().position(|f| f.name == name) else {
        return ctx.no_field(struct_id, name, span);
    };
    ctx.check_field_visible(struct_id, name);
    let field_ty = def.fields[idx].ty.clone();
    HirExpr {
        kind: HirExprKind::FieldRead {
            target: Box::new(target_h),
            struct_id,
            field_index: u32::try_from(idx).expect("field index overflow"),
            field_ty: field_ty.clone(),
        },
        ty: field_ty,
        span,
    }
}

/// An untyped literal built from integers only (`1`, `-1`, `1 + 1`).
fn is_untyped_int(e: &Expr) -> bool {
    fn no_float(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Literal(l) => !matches!(l, AstLit::Float(_)),
            ExprKind::OpPrefix { operand, .. } => no_float(operand),
            ExprKind::OpInfix { lhs, rhs, .. } => no_float(rhs) && no_float(lhs),
            _ => true,
        }
    }
    is_untyped_literal(e) && no_float(e)
}

/// An unsuffixed numeric literal (optionally negated): its type comes
/// from context.
pub(super) fn is_untyped_literal(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Literal(AstLit::Int(t)) => !has_int_suffix(t),
        ExprKind::Literal(AstLit::Float(t)) => !t.ends_with("f32") && !t.ends_with("f64"),
        ExprKind::OpPrefix { op, operand } if op == "-" => is_untyped_literal(operand),
        // `1 + 1`: arithmetic on untyped literals stays untyped.
        ExprKind::OpInfix { op, lhs, rhs }
            if matches!(op.as_str(), "+" | "-" | "*" | "/" | "%") =>
        {
            // The right side first: a left-leaning chain's is shallow.
            is_untyped_literal(rhs) && is_untyped_literal(lhs)
        }
        _ => false,
    }
}

/// Two operands that share a type: an untyped literal takes the other
/// side's. Left to right, unless the left is a literal (no effects).
pub(super) fn lower_sibling_pair(
    ctx: &mut LowerCtx<'_>,
    lhs: &Expr,
    rhs: &Expr,
    lhs_expected: HirTy,
) -> (HirExpr, HirExpr) {
    if is_untyped_literal(lhs) && !is_untyped_literal(rhs) {
        let r = lower_expected(ctx, rhs, lhs_expected);
        let l = lower_expected(ctx, lhs, r.ty.clone());
        (l, r)
    } else {
        let l = lower_expected(ctx, lhs, lhs_expected);
        let r = lower_expected(ctx, rhs, l.ty.clone());
        (l, r)
    }
}

pub(super) fn hir_local(local: LocalId, ty: &HirTy, span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Local(local),
        ty: ty.clone(),
        span,
    }
}

pub(super) const fn hir_int(value: i128, prim: PrimTy, span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Const(ConstVal::Int { value, ty: prim }),
        ty: HirTy::Prim(prim),
        span,
    }
}

pub(super) fn hir_binop(
    op: PrimOp,
    lhs: HirExpr,
    rhs: HirExpr,
    ty: HirTy,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    HirExpr {
        kind: HirExprKind::PrimBinOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        },
        ty,
        span,
    }
}

// Decode a `"..."` literal body (spec 1.7.1). The lexer already
// rejected malformed escapes, so the fallback is never taken.
pub(super) fn decode_str_escapes(s: &str) -> String {
    rokc_ast::escape::unescape(s, rokc_ast::escape::EscapeMode::Str)
        .unwrap_or_else(|_| s.to_owned())
}

pub(super) fn has_int_suffix(text: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "isize", "usize", "i64", "u64", "i32", "u32", "i16", "u16", "i8", "u8",
    ];
    SUFFIXES.iter().any(|s| text.ends_with(s))
}

pub(super) fn lower_if(
    ctx: &mut LowerCtx<'_>,
    cond: &Expr,
    then_block: &Block,
    else_branch: Option<&Expr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let cond_h = lower_cond(ctx, cond);
    let expected = ctx
        .current_expected()
        .cloned()
        .filter(|t| *t != HirTy::Unknown);
    let ended = ctx.unnarrowed.len();
    let mut then_h = lower_narrowed_block(ctx, cond, then_block);
    let ended = branch_unnarrowings(ctx, ended);
    let build = |cond_h: HirExpr, then_h: HirBlock, else_h: Option<HirExpr>, ty: HirTy| HirExpr {
        kind: HirExprKind::If {
            cond: Box::new(cond_h),
            then_block: Box::new(then_h),
            else_branch: else_h.map(Box::new),
        },
        ty,
        span,
    };
    let Some(else_e) = else_branch else {
        // No else: the if is a statement; any then-value is discarded.
        rejoin_unnarrowings(ctx, ended);
        return build(cond_h, then_h, None, HirTy::Unit);
    };
    let mut else_h = lower_expr(ctx, else_e);
    rejoin_unnarrowings(ctx, ended);
    // An unsuffixed integer literal branch takes the other branch's
    // integer type: `if c { 1 } else { n }` with `n: usize`.
    let int_ty = |h: &HirTy| match h {
        HirTy::Prim(p) if p.is_int() => Some(h.clone()),
        _ => None,
    };
    let then_lit = then_block.trailing.as_deref().filter(|e| is_untyped_int(e));
    let else_lit = match &else_e.kind {
        ExprKind::Block(b) => b.trailing.as_deref().filter(|e| is_untyped_int(e)),
        _ => None,
    };
    if let (Some(lit), Some(t)) = (then_lit, int_ty(&else_h.ty)) {
        if then_h.trailing.as_ref().is_some_and(|e| e.ty != t) {
            then_h.trailing = Some(lower_expected(ctx, lit, t));
        }
    } else if let (Some(lit), Some(t)) = (
        else_lit,
        then_h.trailing.as_ref().and_then(|e| int_ty(&e.ty)),
    ) {
        if else_h.ty != t {
            let v = lower_expected(ctx, lit, t.clone());
            if let HirExprKind::Block(b) = &mut else_h.kind {
                b.trailing = Some(v);
            }
            else_h.ty = t;
        }
    }
    // Spec 3.14.1: a diverging branch (`return` / `throw` / ...) takes
    // the other branch's type.
    let then_ty = then_h
        .trailing
        .as_ref()
        .map_or(HirTy::Unit, |e| e.ty.clone());
    let else_ty = else_h.ty.clone();
    let then_div = hir_block_diverges(&then_h);
    let else_div = hir_expr_diverges(&else_h);
    let result_ty = if expected == Some(HirTy::Unit) {
        // Statement position: branch values are discarded.
        HirTy::Unit
    } else if then_div {
        else_ty
    } else if else_div || then_ty == else_ty {
        then_ty
    } else if then_ty == HirTy::Unknown || else_ty == HirTy::Unknown {
        HirTy::Unknown
    } else if let Some(t) = expected.filter(|t| !super::infer::has_var(ctx, t)) {
        // A typed slot: each branch must fit it, as a `match` arm does.
        if !then_div {
            then_h.trailing = fit_if_branch(ctx, &t, then_h.trailing.take(), "then", &else_ty);
        }
        if !else_div {
            else_h = match else_h.kind {
                HirExprKind::Block(mut b) => {
                    b.trailing = fit_if_branch(ctx, &t, b.trailing.take(), "else", &then_ty);
                    HirExpr {
                        ty: t.clone(),
                        kind: HirExprKind::Block(b),
                        span: else_h.span,
                    }
                }
                _ => {
                    fit_if_branch(ctx, &t, Some(else_h), "else", &then_ty).expect("a branch value")
                }
            };
        }
        t
    } else {
        // Branches of different types are an error, not an inferred
        // sum - the slot's annotation must name the sum.
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "`if` branches have different types `{}` and `{}` - rok does not infer a \
                 sum type; annotate the slot (`let v: {} | {} = if ...`)",
                ctx.ty_name(&then_ty),
                ctx.ty_name(&else_ty),
                ctx.ty_name(&then_ty),
                ctx.ty_name(&else_ty)
            ),
        );
        HirTy::Unknown
    };
    if matches!(result_ty, HirTy::Sum(_) | HirTy::AnyTrait(_)) {
        let what = || "the `if` branch".to_owned();
        if let Some(t) = then_h.trailing.take() {
            then_h.trailing = Some(coerce(ctx, &result_ty, t, &what));
        }
        else_h = match else_h.kind {
            HirExprKind::Block(mut b) => {
                if let Some(t) = b.trailing.take() {
                    b.trailing = Some(coerce(ctx, &result_ty, t, &what));
                }
                HirExpr {
                    ty: result_ty.clone(),
                    kind: HirExprKind::Block(b),
                    span: else_h.span,
                }
            }
            _ => coerce(ctx, &result_ty, else_h, &what),
        };
    }
    build(cond_h, then_h, Some(else_h), result_ty)
}

/// The `which` (`then` / `else`) branch value of an `if` in a slot of
/// type `t`: coerced, or reported naming the branch (`other` the other
/// branch's type, for the help). `None` is a unit branch.
fn fit_if_branch(
    ctx: &mut LowerCtx<'_>,
    t: &HirTy,
    v: Option<HirExpr>,
    which: &str,
    other: &HirTy,
) -> Option<HirExpr> {
    let before = ctx.env.sink.diagnostics.borrow().len();
    let what = || format!("the `{which}` branch of `if`");
    let v = match v {
        Some(v) => {
            let at = if v.span.is_dummy() {
                ctx.here()
            } else {
                v.span
            };
            let outer = ctx.at.replace(at);
            let v = coerce(ctx, t, v, &what);
            ctx.at.set(outer);
            Some(v)
        }
        None if !coercible(ctx, t, &HirTy::Unit) => {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "mismatched types: {} expects `{}`, found `()`",
                    what(),
                    ctx.ty_name(t)
                ),
            );
            None
        }
        None => None,
    };
    if ctx.env.sink.diagnostics.borrow().len() > before {
        let have = v.as_ref().map_or(HirTy::Unit, |v| v.ty.clone());
        let t_name = ctx.ty_name(t);
        let mut help = format!(
            "the slot is typed `{t_name}`; give the `{which}` branch a value of type `{t_name}`"
        );
        if coercible(ctx, t, other) {
            help += &format!(
                ", or widen the slot's type to `{t_name} | {}`",
                ctx.ty_name(&have)
            );
        }
        ctx.help_last(Some(help));
    }
    v
}

/// Spec 2.2.6: the `x is T` conjuncts of `cond` (`a && b && ..`): locals
/// that are `T` wherever `cond` holds.
fn narrowings(ctx: &LowerCtx<'_>, cond: &Expr) -> Vec<(String, rokc_ast::Ty)> {
    match &cond.kind {
        ExprKind::OpInfix { op, lhs, rhs } if op == "&&" => {
            let mut v = narrowings(ctx, lhs);
            v.extend(narrowings(ctx, rhs));
            v
        }
        ExprKind::Is(operand, pat) => match (&operand.kind, &pat.kind) {
            (ExprKind::Path(p), AstPatternKind::TypeCheck { binder: None, ty })
                if p.segments.len() == 1
                    && ctx.lookup(&p.segments[0].name).is_some_and(|l| {
                        // An enum's `x is E::V` is a variant test: no narrowing.
                        // A plain member of the target sum stays as it is.
                        let lt = ctx.var_ty(l);
                        !matches!(lt, HirTy::Enum(_))
                            && !matches!(ctx.resolve_ast_ty(ty), HirTy::Sum(ws) if ws.contains(&lt))
                    }) =>
            {
                vec![(p.segments[0].name.clone(), ty.clone())]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Rebind each narrowed local in the current scope as
/// `let x = match x { x: T => x };`; writes go through to the original
/// (`narrowed`). A `[ref]` cell is re-tested on every read (`cell_views`),
/// since a capture may store a non-`T`.
fn bind_narrowed(ctx: &mut LowerCtx<'_>, cond: &Expr) -> Vec<HirStmt> {
    let mut lets = Vec::new();
    for (name, ty) in narrowings(ctx, cond) {
        let Some(orig) = ctx.lookup(&name) else {
            continue;
        };
        let orig_ty = ctx.var_ty(orig);
        let pat = rokc_ast::Pattern {
            kind: AstPatternKind::TypeCheck {
                binder: Some(name.clone()),
                ty,
            },
            span: cond.span,
        };
        let pat_h = lower_pattern(ctx, &pat, &orig_ty);
        // A narrowing rebinding is never a `[ref]` cell.
        ctx.pending_cells.clear();
        let resums = ctx.take_resums(cond.span);
        let Some(new) = ctx.lookup_in_current_scope(&name) else {
            continue;
        };
        let new_ty = ctx.local_ty(new);
        ctx.narrowed.insert(new, (orig, orig_ty.clone()));
        let mut arms = vec![HirMatchArm {
            pat: pat_h,
            guard: None,
            body: with_lets(resums, hir_local(new, &new_ty, cond.span)),
        }];
        let is_cell = ctx.is_boxed(orig) || ctx.cell_views.contains_key(&orig);
        if is_cell {
            let msg = format!(
                "`{name}` no longer holds `{}`: a `[ref {name}]` capture changed it inside \
                 its narrowing",
                ctx.ty_name(&new_ty)
            );
            arms.push(HirMatchArm {
                pat: HirPattern::Wildcard,
                guard: None,
                body: HirExpr {
                    kind: HirExprKind::Block(Box::new(HirBlock {
                        stmts: vec![HirStmt::Panic(super::throws::hook_call(
                            ctx,
                            "rok_panic_at",
                            &msg,
                            cond.span,
                        ))],
                        trailing: None,
                        label: None,
                    })),
                    ty: new_ty.clone(),
                    span: cond.span,
                },
            });
        }
        let read = HirExpr {
            kind: HirExprKind::Match {
                scrutinee: Box::new(ctx.read_local(orig, cond.span)),
                arms,
            },
            ty: new_ty,
            span: cond.span,
        };
        if is_cell {
            ctx.cell_views.insert(new, read.clone());
        }
        lets.push(HirStmt::Let {
            local: new,
            init: read,
        });
    }
    lets
}

/// A block that runs only when `cond` held, with its narrowings bound.
pub(super) fn lower_narrowed_block(ctx: &mut LowerCtx<'_>, cond: &Expr, block: &Block) -> HirBlock {
    ctx.push_scope();
    let lets = bind_narrowed(ctx, cond);
    let mut b = lower_block(ctx, block);
    ctx.pop_scope();
    if !lets.is_empty() {
        b.stmts.splice(0..0, lets);
    }
    b
}

/// Write-through for an assignment to a narrowed binding: also store
/// the new value, converted back, into the original slot.
fn write_through(
    ctx: &mut LowerCtx<'_>,
    stmt: Option<HirStmt>,
    span: rokc_diagnostic::Span,
) -> Option<HirStmt> {
    let local = match &stmt {
        Some(HirStmt::Assign { local, .. }) => *local,
        Some(HirStmt::ArrayAssign { array_local, .. }) => *array_local,
        _ => return stmt,
    };
    if !ctx.narrowed.contains_key(&local) {
        return stmt;
    }
    // A narrowing of a narrowing writes back through every level.
    let mut stmts = vec![stmt?];
    let mut cur = local;
    while let Some((orig, orig_ty)) = ctx.narrowed.get(&cur).cloned() {
        let ty = ctx.local_ty(cur);
        let back = coerce(ctx, &orig_ty, hir_local(cur, &ty, span), &|| {
            "the narrowed slot".to_owned()
        });
        stmts.push(ctx.store_local(orig, back));
        cur = orig;
    }
    Some(HirStmt::Expr(HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: None,
            label: None,
        })),
        ty: HirTy::Unit,
        span,
    }))
}

/// Spec 2.2.5: `v is T` - `match v { _: T => true, _ => false }`.
fn lower_is(
    ctx: &mut LowerCtx<'_>,
    operand: &Expr,
    pat: &rokc_ast::Pattern,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let scrut = lower_expected(ctx, operand, HirTy::Unknown);
    if scrut.ty == HirTy::Unknown {
        return scrut;
    }
    let lit = |b: bool| HirExpr {
        kind: HirExprKind::Const(ConstVal::Bool(b)),
        ty: HirTy::Prim(PrimTy::Bool),
        span,
    };
    // `e is Enum::Variant` on an enum value: a variant test (any
    // payload), not a type test.
    let variant_pat;
    let pat = match (&scrut.ty, &pat.kind) {
        (
            HirTy::Enum(_),
            AstPatternKind::TypeCheck {
                binder: None,
                ty:
                    rokc_ast::Ty {
                        kind: rokc_ast::TyKind::Path(path),
                        ..
                    },
            },
        ) if path.segments.len() >= 2 => {
            variant_pat = rokc_ast::Pattern {
                kind: AstPatternKind::EnumVariant {
                    path: path.clone(),
                    args: vec![rokc_ast::Pattern {
                        kind: AstPatternKind::Rest(None),
                        span: pat.span,
                    }],
                },
                span: pat.span,
            };
            &variant_pat
        }
        _ => pat,
    };
    // A static (non-`any`, non-sum) operand: the answer is known now.
    if let (false, AstPatternKind::TypeCheck { ty, .. }) = (
        matches!(scrut.ty, HirTy::AnyTrait(_) | HirTy::Sum(_)),
        &pat.kind,
    ) {
        // `A is (A | B)`: membership of the value's type in the sum.
        let same = match ctx.resolve_ast_ty(ty) {
            HirTy::Sum(ws) => ws.contains(&scrut.ty),
            t => t == scrut.ty,
        };
        return with_lets(vec![HirStmt::Expr(scrut)], lit(same));
    }
    ctx.push_scope();
    let pat_h = lower_pattern(ctx, pat, &scrut.ty);
    // `is` binders aren't `[ref]` cells (no scope to start
    // one in); capturing one by `ref` is reported at the closure.
    ctx.pending_cells.clear();
    // An `is v: A | B` sub-sum binder stays unconverted; the
    // `if x is (A | B)` narrowing form converts.
    ctx.pending_resums.clear();
    ctx.pop_scope();
    HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(scrut),
            arms: vec![
                HirMatchArm {
                    pat: pat_h,
                    guard: None,
                    body: lit(true),
                },
                HirMatchArm {
                    pat: HirPattern::Wildcard,
                    guard: None,
                    body: lit(false),
                },
            ],
        },
        ty: HirTy::Prim(PrimTy::Bool),
        span,
    }
}

thread_local! {
    /// Spans of trailing closures (spec 4.8): they fill the last parameter
    /// (4.8.3), past defaulted or named ones (4.8.9).
    static TRAILING_CLOSURES: std::cell::RefCell<std::collections::HashSet<(u32, u32)>> =
        std::cell::RefCell::default();
}

fn is_trailing_closure(e: &Expr) -> bool {
    TRAILING_CLOSURES.with(|t| t.borrow().contains(&(e.span.file, e.span.start)))
}

/// `args` plus the trailing closure as the last positional argument.
fn with_trailing_closure(args: &[CallArg], tc: Option<&Expr>) -> Vec<CallArg> {
    let mut all = args.to_vec();
    if let Some(tc) = tc {
        TRAILING_CLOSURES.with(|t| t.borrow_mut().insert((tc.span.file, tc.span.start)));
        all.push(CallArg::Positional(tc.clone()));
    }
    all
}

/// Lower arguments against a signature (spec 4.2): named ones to their
/// slot, tuple spreads to consecutive slots, missing trailing ones to
/// defaults, each checked. `receiver` fills slot 0.
pub(super) fn lower_args_for(
    ctx: &mut LowerCtx<'_>,
    sig: &FnSig,
    callee: &str,
    receiver: Option<HirExpr>,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> (Vec<HirExpr>, Vec<HirStmt>) {
    let (out, lets, _) = lower_args_ref(ctx, sig, callee, receiver, args, span);
    (out, lets)
}

/// `lower_args_for`, also returning each `ref` parameter's argument
/// place (by parameter index) for the copy-out (spec 4.7).
#[allow(clippy::cognitive_complexity)]
pub(super) fn lower_args_ref(
    ctx: &mut LowerCtx<'_>,
    sig: &FnSig,
    callee: &str,
    receiver: Option<HirExpr>,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> (Vec<HirExpr>, Vec<HirStmt>, Vec<Option<HirExpr>>) {
    let has_receiver = receiver.is_some();
    let explicit_recv = ctx.explicit_recv.take();
    let n = sig.param_tys.len();
    let mut lets: Vec<HirStmt> = Vec::new();
    let mut slots: Vec<Option<ArgSrc<'_>>> = (0..n).map(|_| None).collect();
    let mut next = 0usize;
    let mut extra = 0usize;
    let mut c_varargs: Vec<HirExpr> = Vec::new();
    if let Some(r) = receiver {
        if n > 0 {
            slots[0] = Some(ArgSrc::Lowered(r));
        }
        next = 1;
    }
    // Spec 4.2.5: a variadic `...T` (the last slot) takes every argument
    // from its position on, spreads included.
    let var_at = if sig.variadic { n - 1 } else { usize::MAX };
    let mut var_parts: Vec<VarPart<'_>> = Vec::new();
    // Slot → its argument's written position (spec 4.2.8).
    let mut written: Vec<Option<usize>> = vec![None; n];
    // `..d` with `d: Dict<str, V>`: its temp and written position.
    let mut dicts: Vec<(String, usize, HirTy)> = Vec::new();
    for (k, a) in args.iter().enumerate() {
        match a {
            CallArg::Positional(e) | CallArg::Ref(e) | CallArg::Spread(e) if next >= var_at => {
                var_parts.push(if matches!(a, CallArg::Spread(_)) {
                    VarPart::Spread(e)
                } else {
                    VarPart::Arg(e)
                });
                next += 1;
            }
            CallArg::Positional(e)
                if !sig.variadic
                    && n > next
                    && slots[n - 1].is_none()
                    && is_trailing_closure(e) =>
            {
                slots[n - 1] = Some(ArgSrc::Ast(e));
                written[n - 1] = Some(k);
                next = n;
            }
            CallArg::Positional(e) | CallArg::Ref(e) => {
                if next < n {
                    written[next] = Some(k);
                    slots[next] = Some(if matches!(a, CallArg::Ref(_)) {
                        ArgSrc::Ref(e)
                    } else {
                        ArgSrc::Ast(e)
                    });
                } else {
                    c_varargs.push(lower_expected(ctx, e, HirTy::Unknown));
                }
                next += 1;
            }
            CallArg::Named { name, value } => {
                match sig.param_names.iter().position(|p| p == name) {
                    Some(i) if slots[i].is_none() => {
                        slots[i] = Some(ArgSrc::Ast(value));
                        written[i] = Some(k);
                    }
                    Some(i) => {
                        let p = sig.param_names.get(i).map_or(name.as_str(), String::as_str);
                        ctx.emit_error(
                            rokc_diagnostic::ErrorKind::Type,
                            format!("argument `{p}` provided more than once (spec 4.2.4)"),
                        );
                        ctx.help_last(Some(twice_help(p)));
                    }
                    None => ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!("`{callee}` has no parameter named `{name}` (spec 4.2.4)"),
                    ),
                }
            }
            CallArg::Spread(e) => {
                // `f(..t)` with a tuple value (spec 4.2.6).
                let lh = lower_expected(ctx, e, HirTy::Unknown);
                match lh.ty.clone() {
                    HirTy::Tuple(parts) => {
                        // Evaluate the tuple once into a temp, then read
                        // its fields.
                        let tuple_ty = lh.ty.clone();
                        let tmp = ctx.declare(None, tuple_ty.clone());
                        lets.push(HirStmt::Let {
                            local: tmp,
                            init: lh,
                        });
                        for (i, part_ty) in parts.iter().enumerate() {
                            let field = HirExpr {
                                kind: HirExprKind::TupleField {
                                    target: Box::new(hir_local(tmp, &tuple_ty, span)),
                                    index: u32::try_from(i).expect("tuple field overflow"),
                                    field_ty: part_ty.clone(),
                                },
                                ty: part_ty.clone(),
                                span,
                            };
                            if next >= var_at {
                                var_parts.push(VarPart::Value(field));
                            } else if next < n {
                                slots[next] = Some(ArgSrc::Lowered(field));
                            } else {
                                extra += 1;
                            }
                            next += 1;
                        }
                    }
                    HirTy::Unknown => {}
                    // An array fills its length's slots; a `List` / `Slice`
                    // fills up to the next argument, checked at run time.
                    ty @ HirTy::Array(..) => {
                        let HirTy::Array(_, len) = ty else {
                            unreachable!()
                        };
                        let name = format!("#spread{}", ctx.locals.len());
                        let local = ctx.declare(Some(name.clone()), ty);
                        lets.push(HirStmt::Let { local, init: lh });
                        for i in 0..len {
                            let item = spread_item(&name, i as usize, span);
                            if next >= var_at {
                                let v = lower_expected(ctx, &item, HirTy::Unknown);
                                var_parts.push(VarPart::Value(v));
                            } else if next < n {
                                let v = lower_expected(ctx, &item, sig.param_tys[next].clone());
                                slots[next] = Some(ArgSrc::Lowered(v));
                            } else {
                                extra += 1;
                            }
                            next += 1;
                        }
                    }
                    ty if spreads_at_run_time(ctx, &ty) => {
                        let later = args[k + 1..]
                            .iter()
                            .filter(|a| !matches!(a, CallArg::Named { .. }))
                            .count();
                        let end = if sig.variadic {
                            var_at
                        } else {
                            n.saturating_sub(later)
                        };
                        let fill = end.saturating_sub(next);
                        let name = format!("#spread{}", ctx.locals.len());
                        let local = ctx.declare(Some(name.clone()), ty);
                        lets.push(HirStmt::Let { local, init: lh });
                        let path = super::throws::path_expr(&name, span);
                        let len = Expr {
                            kind: ExprKind::MethodCall {
                                receiver: Box::new(path),
                                method: "len".to_owned(),
                                type_args: Vec::new(),
                                args: Vec::new(),
                                trailing_closure: None,
                            },
                            span,
                        };
                        let want = Expr {
                            kind: ExprKind::Literal(AstLit::Int(format!("{fill}usize"))),
                            span,
                        };
                        let check = index_fn("spread_len", vec![len, want], span);
                        lets.push(HirStmt::Expr(lower_expected(ctx, &check, HirTy::Unit)));
                        for i in 0..fill {
                            let item = spread_item(&name, i, span);
                            let v = lower_expected(ctx, &item, sig.param_tys[next].clone());
                            slots[next] = Some(ArgSrc::Lowered(v));
                            next += 1;
                        }
                    }
                    ty if is_str_dict(ctx, &ty) => {
                        let name = format!("#spread{}", ctx.locals.len());
                        let v = dict_value_ty(ctx, &ty);
                        let local = ctx.declare(Some(name.clone()), ty);
                        lets.push(HirStmt::Let { local, init: lh });
                        dicts.push((name, k, v));
                    }
                    other => ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!(
                            "spread `f(..x)` needs a tuple, a `List`, a `Slice`, an array or a \
                             `Dict<str, V>`, found `{}` (spec 4.2.6)",
                            ctx.ty_name(&other)
                        ),
                    ),
                }
            }
        }
    }
    let end = if sig.variadic { n - 1 } else { n };
    let empty = (0..end)
        .filter(|i| slots[*i].is_none())
        .map(|i| {
            let d = sig.param_defaults.get(i).cloned().flatten();
            (i, sig.param_names[i].clone(), d, sig.param_tys[i].clone())
        })
        .collect();
    let names: Vec<(String, HirTy)> = dicts.iter().map(|d| (d.0.clone(), d.2.clone())).collect();
    let mut dict_slots = Vec::new();
    for (i, e) in dict_spread(ctx, &names, empty, &mut lets, span) {
        slots[i] = Some(ArgSrc::Dict(dict_slots.len()));
        written[i] = dicts.first().map(|d| d.1);
        dict_slots.push(e);
    }
    if sig.variadic && slots[var_at].is_none() {
        let elem = variadic_elem(ctx, &sig.param_tys[var_at]);
        slots[var_at] = Some(ArgSrc::Lowered(lower_variadic(
            ctx, callee, elem, var_parts, span,
        )));
    }
    let c_varargs = check_arity(ctx, sig, callee, has_receiver, extra, c_varargs);
    let mut out = Vec::with_capacity(n);
    let mut places: Vec<Option<HirExpr>> = vec![None; n];
    // Places other (non-`ref`) arguments read, for the exclusivity check.
    let mut reads: Vec<(String, Vec<String>)> = Vec::new();
    for (i, slot) in slots.into_iter().enumerate() {
        let pty = sig.param_tys[i].clone();
        let pname = sig.param_names.get(i).cloned().unwrap_or_default();
        let by_ref = sig.param_refs.get(i).copied().unwrap_or(false);
        let is_receiver = i == 0 && has_receiver;
        match &slot {
            Some(ArgSrc::Ref(_)) if !by_ref => ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "argument `{pname}` of `{callee}` is passed with `ref`, but the parameter \
                     is not `ref` (spec 4.7.3)"
                ),
            ),
            Some(ArgSrc::Ast(_)) if by_ref && !is_receiver => ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "parameter `{pname}` of `{callee}` is `ref` - pass `ref x` at the call \
                     site (spec 4.7.3)"
                ),
            ),
            Some(ArgSrc::Ast(e)) => reads.extend(crate::walk::free_places(&[], e)),
            _ => {}
        }
        let v = match slot {
            Some(ArgSrc::Ast(e)) => lower_expected(ctx, e, pty.clone()),
            Some(ArgSrc::Dict(i)) => lower_expected(ctx, &dict_slots[i], pty.clone()),
            Some(ArgSrc::Ref(e)) => {
                let h = lower_expected(ctx, e, pty.clone());
                if super::mono::place_root(&h).is_some() {
                    places[i] = Some(h.clone());
                } else if h.ty != HirTy::Unknown {
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!(
                            "`ref` argument `{pname}` of `{callee}` must name a variable or \
                             a field of one (spec 4.7)"
                        ),
                    );
                }
                h
            }
            Some(ArgSrc::Lowered(h)) => {
                if is_receiver {
                    check_path_receiver(ctx, explicit_recv, by_ref, &h, &pname, callee);
                }
                if is_receiver && by_ref {
                    places[i] = Some(h.clone());
                }
                h
            }
            None => {
                if let Some(d) = sig.param_defaults.get(i).cloned().flatten() {
                    lower_expected(ctx, &d, pty.clone())
                } else {
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!("missing argument `{pname}` in call to `{callee}`"),
                    );
                    let sig = sig_of(ctx, callee, sig);
                    ctx.help_last(Some(format!("it is declared `{sig}`; pass `{pname}` too")));
                    poison(span)
                }
            }
        };
        // A mismatch points at the argument, not the whole call.
        let at = if v.span.is_dummy() {
            ctx.here()
        } else {
            v.span
        };
        let outer = ctx.at.replace(at);
        out.push(coerce(ctx, &pty, v, &|| {
            format!("argument `{pname}` of `{callee}`")
        }));
        ctx.at.set(outer);
    }
    check_ref_exclusive(ctx, callee, &out, &places, &reads, has_receiver);
    // Spec 4.2.8: written order. Named arguments out of parameter order go
    // through temps, after the receiver.
    let mut order: Vec<(usize, usize)> = written
        .iter()
        .enumerate()
        .filter_map(|(i, w)| w.filter(|_| places[i].is_none()).map(|w| (w, i)))
        .collect();
    if order.windows(2).any(|w| w[0].0 > w[1].0) {
        order.sort_unstable();
        let recv = (has_receiver && places.first().is_some_and(Option::is_none)).then_some(0);
        for i in recv.into_iter().chain(order.into_iter().map(|(_, i)| i)) {
            let v = std::mem::replace(&mut out[i], poison(span));
            let ty = v.ty.clone();
            let tmp = ctx.declare(None, ty.clone());
            lets.push(HirStmt::Let {
                local: tmp,
                init: v,
            });
            out[i] = hir_local(tmp, &ty, span);
        }
    }
    out.extend(c_varargs);
    (out, lets, places)
}

/// Spec 4.2.6: dict spreads as named arguments. Each parameter in `empty`
/// reads `#d["p"]` from the first dict with key `p`, else its default (or
/// panics in `Dict::index`); every key must name one (`spread_names`,
/// checked at run time). A defaulted parameter skips a dict whose value
/// type does not fit it; that dict holding its key panics (`spread_misfit`).
fn dict_spread(
    ctx: &mut LowerCtx<'_>,
    dicts: &[(String, HirTy)],
    empty: Vec<(usize, String, Option<Expr>, HirTy)>,
    lets: &mut Vec<HirStmt>,
    span: rokc_diagnostic::Span,
) -> Vec<(usize, Expr)> {
    if dicts.is_empty() {
        return Vec::new();
    }
    let lit = |s: &str| Expr {
        kind: ExprKind::Literal(AstLit::Str(s.to_owned())),
        span,
    };
    let mut names = Vec::new();
    let mut misfits = Vec::new();
    let mut out = Vec::new();
    for (i, pname, default, pty) in empty {
        names.push(lit(&pname));
        let fill: Vec<&String> = dicts
            .iter()
            .filter(|(d, v)| {
                let fits = default.is_none() || dict_value_fits(ctx, &pty, v);
                if !fits {
                    let why = format!(
                        "whose value type `{}` does not fit parameter `{pname}: {}`",
                        ctx.ty_name(v),
                        ctx.ty_name(&pty)
                    );
                    misfits.push((d.clone(), pname.clone(), why));
                }
                fits
            })
            .map(|(d, _)| d)
            .collect();
        let Some(last) = fill.last() else {
            continue;
        };
        let skip_last = usize::from(default.is_none());
        let mut e = default.unwrap_or_else(|| spread_item_at(last, &lit(&pname), span));
        for d in fill.iter().rev().skip(skip_last) {
            let has = Expr {
                kind: ExprKind::MethodCall {
                    receiver: Box::new(super::throws::path_expr(d, span)),
                    method: "contains_key".to_owned(),
                    type_args: Vec::new(),
                    args: vec![CallArg::Positional(lit(&pname))],
                    trailing_closure: None,
                },
                span,
            };
            e = Expr {
                kind: ExprKind::If {
                    cond: Box::new(has),
                    then_branch: Block {
                        stmts: Vec::new(),
                        trailing: Some(Box::new(spread_item_at(d, &lit(&pname), span))),
                        span,
                        label: None,
                    },
                    else_branch: Some(Box::new(e)),
                },
                span,
            };
        }
        out.push((i, e));
    }
    for (d, pname, why) in misfits {
        let check = index_fn(
            "spread_misfit",
            vec![super::throws::path_expr(&d, span), lit(&pname), lit(&why)],
            span,
        );
        lets.push(HirStmt::Expr(lower_expected(ctx, &check, HirTy::Unit)));
    }
    let names = Expr {
        kind: ExprKind::List(names),
        span,
    };
    for (d, _) in dicts {
        let check = index_fn(
            "spread_names",
            vec![super::throws::path_expr(d, span), names.clone()],
            span,
        );
        lets.push(HirStmt::Expr(lower_expected(ctx, &check, HirTy::Unit)));
    }
    out
}

/// Can a dict value of type `v` fill a parameter of type `p`? An open
/// type (generic, not yet inferred) is taken to fit.
fn dict_value_fits(ctx: &LowerCtx<'_>, p: &HirTy, v: &HirTy) -> bool {
    let open = |t: &HirTy| {
        super::bounds::mentions(ctx, t, &|t| {
            matches!(
                t,
                HirTy::Unknown
                    | HirTy::Path(_)
                    | HirTy::Param(_)
                    | HirTy::Var(_)
                    | HirTy::OpenConst(_)
            )
        })
    };
    open(p) || open(v) || coercible(ctx, p, v)
}

/// What to write for an argument `p` given twice.
fn twice_help(p: &str) -> String {
    format!("pass `{p}` once: remove the extra `{p}: ..` (or the positional argument it repeats)")
}

/// A generic fn's parameter type, when it is plain (primitives, unit and
/// tuples of them) and so known before inference; else `Unknown`.
fn plain_param_ty(ty: &Ty) -> HirTy {
    fn plain(t: &HirTy) -> bool {
        match t {
            HirTy::Prim(_) | HirTy::Unit => true,
            HirTy::Tuple(ps) => ps.iter().all(plain),
            _ => false,
        }
    }
    let t = crate::ty::resolve_ty(ty);
    if plain(&t) { t } else { HirTy::Unknown }
}

/// `V` of a `Dict<str, V>`.
fn dict_value_ty(ctx: &LowerCtx<'_>, ty: &HirTy) -> HirTy {
    match ty {
        HirTy::Struct(id) => ctx
            .struct_def(*id)
            .generic_args
            .get(1)
            .cloned()
            .unwrap_or(HirTy::Unknown),
        _ => HirTy::Unknown,
    }
}

/// `Dict<str, V>`: what `..d` spreads as named arguments.
fn is_str_dict(ctx: &LowerCtx<'_>, ty: &HirTy) -> bool {
    let HirTy::Struct(id) = ty else {
        return false;
    };
    let def = ctx.struct_def(*id);
    enum_base_name(&def.name) == DICT && def.generic_args.first() == Some(&HirTy::Prim(PrimTy::Str))
}

const DICT: &str = "rok::core::collections::dict::Dict";
const LIST: &str = "rok::core::collections::list::List";
const SLICE: &str = "rok::core::collections::slice::Slice";

/// `b"..."`: a `[u8; N]` of its bytes; where a `Slice<u8>` is expected
/// (alone, or in a sum without an array member), a view of that array.
fn lower_byte_str(ctx: &mut LowerCtx<'_>, body: &str, span: rokc_diagnostic::Span) -> HirExpr {
    #[allow(clippy::cast_possible_truncation)] // the lexer allows up to `\xFF`
    let bytes: Vec<u8> = rokc_ast::escape::unescape(body, rokc_ast::escape::EscapeMode::CChar)
        .unwrap_or_default()
        .chars()
        .map(|c| c as u8)
        .collect();
    let Ok(n) = u32::try_from(bytes.len()) else {
        return ctx.error_expr(span, "byte string is too long".to_owned());
    };
    let arr = HirExpr {
        ty: HirTy::Array(Box::new(HirTy::Prim(PrimTy::U8)), n),
        kind: HirExprKind::Const(ConstVal::Bytes(bytes)),
        span,
    };
    let is_byte_slice = |t: &HirTy| {
        matches!(t, HirTy::Struct(id) if {
            let d = ctx.struct_def(*id);
            enum_base_name(&d.name) == SLICE && d.generic_args == [HirTy::Prim(PrimTy::U8)]
        })
    };
    let view = match ctx.current_expected() {
        Some(t) if is_byte_slice(t) => true,
        Some(HirTy::Sum(vs)) => {
            vs.iter().any(is_byte_slice) && !vs.iter().any(|v| matches!(v, HirTy::Array(..)))
        }
        _ => false,
    };
    if !view {
        return arr;
    }
    let name = format!("#bs{}", ctx.locals.len());
    let local = ctx.declare(Some(name.clone()), arr.ty.clone());
    let slice = Expr {
        kind: ExprKind::Slice {
            target: Box::new(super::throws::path_expr(&name, span)),
            start: None,
            end: None,
            step: None,
        },
        span,
    };
    let trailing = lower_expected(ctx, &slice, HirTy::Unknown);
    HirExpr {
        ty: trailing.ty.clone(),
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![HirStmt::Let { local, init: arr }],
            trailing: Some(trailing),
            label: None,
        })),
        span,
    }
}

/// A `List` / `Slice`: what `..x` spreads with a run-time length.
fn spreads_at_run_time(ctx: &LowerCtx<'_>, ty: &HirTy) -> bool {
    let HirTy::Struct(id) = ty else {
        return false;
    };
    let name = ctx.struct_def(*id).name;
    matches!(enum_base_name(&name), LIST | SLICE)
}

/// A `List` literal item: `..x` spreads a tuple, array, `List` or `Slice`
/// (the handler is variadic, spec 4.2.6); otherwise it's a range.
fn list_item(ctx: &mut LowerCtx<'_>, e: &Expr) -> CallArg {
    let ExprKind::Range {
        start: None,
        end: Some(x),
        inclusive: false,
    } = &e.kind
    else {
        return CallArg::Positional(e.clone());
    };
    let h = lower_expected(ctx, x, HirTy::Unknown);
    let spread =
        matches!(h.ty, HirTy::Tuple(_) | HirTy::Array(..)) || spreads_at_run_time(ctx, &h.ty);
    let x = splice(ctx, h);
    if spread {
        return CallArg::Spread(x);
    }
    CallArg::Positional(Expr {
        kind: ExprKind::Range {
            start: None,
            end: Some(Box::new(x)),
            inclusive: false,
        },
        span: e.span,
    })
}

/// One trailing argument of a variadic `...T` call (spec 4.2.5).
pub(super) enum VarPart<'e> {
    Arg(&'e Expr),
    /// `..x`: a tuple's fields, or a `List` / `Slice` / array's elements.
    Spread(&'e Expr),
    /// A field of a tuple spread that started in a fixed slot.
    Value(HirExpr),
}

/// The element type `T` of the `Slice<T>` a variadic parameter takes
/// (`Unknown` when `ty` is not one yet).
pub(super) fn variadic_elem(ctx: &LowerCtx<'_>, ty: &HirTy) -> HirTy {
    match ty {
        HirTy::Struct(sid) => ctx
            .struct_def(*sid)
            .generic_args
            .first()
            .cloned()
            .unwrap_or(HirTy::Unknown),
        _ => HirTy::Unknown,
    }
}

/// Spec 4.2.5 / 4.2.6: a variadic call's trailing arguments as one
/// `Slice<T>` (`elem` `Unknown`: the first argument's type). Plain
/// arguments become a fixed array viewed as a slice; a spread makes a fresh
/// `List`.
pub(super) fn lower_variadic(
    ctx: &mut LowerCtx<'_>,
    callee: &str,
    mut elem: HirTy,
    parts: Vec<VarPart<'_>>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    // `[1, 2, 3]` calls `list_literal(...T)`: name the element, not the
    // argument.
    let list = leaf_name(callee) == "list_literal";
    let mut stmts: Vec<HirStmt> = Vec::new();
    // Each argument once into a temp, in order: (is a spread collection, temp).
    let mut temps: Vec<(bool, String, HirTy)> = Vec::new();
    let temp = |ctx: &mut LowerCtx<'_>, stmts: &mut Vec<HirStmt>, init: HirExpr| {
        let name = format!("#va{}", ctx.locals.len());
        let ty = init.ty.clone();
        let local = ctx.declare(Some(name.clone()), ty.clone());
        stmts.push(HirStmt::Let { local, init });
        (name, ty)
    };
    let what = if list {
        "list element"
    } else {
        "variadic argument"
    };
    let one = |ctx: &mut LowerCtx<'_>, elem: &mut HirTy, v: HirExpr| {
        if *elem == HirTy::Unknown {
            elem.clone_from(&v.ty);
            v
        } else {
            coerce(ctx, elem, v, &|| what.to_owned())
        }
    };
    for part in parts {
        let v = match part {
            VarPart::Arg(e) => lower_expected(ctx, e, elem.clone()),
            VarPart::Value(v) => v,
            VarPart::Spread(e) => {
                let h = lower_expected(ctx, e, HirTy::Unknown);
                match h.ty.clone() {
                    HirTy::Tuple(fields) => {
                        let (name, ty) = temp(ctx, &mut stmts, h);
                        let local = ctx.lookup(&name).expect("temp just declared");
                        for (k, fty) in fields.iter().enumerate() {
                            let field = HirExpr {
                                kind: HirExprKind::TupleField {
                                    target: Box::new(hir_local(local, &ty, span)),
                                    index: u32::try_from(k).expect("tuple field overflow"),
                                    field_ty: fty.clone(),
                                },
                                ty: fty.clone(),
                                span,
                            };
                            let v = one(ctx, &mut elem, field);
                            let (name, ty) = temp(ctx, &mut stmts, v);
                            temps.push((false, name, ty));
                        }
                    }
                    HirTy::Array(..) | HirTy::Struct(_) => {
                        if elem == HirTy::Unknown {
                            elem = match &h.ty {
                                HirTy::Array(t, _) => (**t).clone(),
                                t => variadic_elem(ctx, t),
                            };
                        }
                        let (name, ty) = temp(ctx, &mut stmts, h);
                        temps.push((true, name, ty));
                    }
                    HirTy::Unknown => {}
                    other => ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!(
                            "spread into a variadic `...T` needs a tuple, a `List`, a `Slice` or \
                             an array, found `{}` (spec 4.2.6)",
                            ctx.ty_name(&other)
                        ),
                    ),
                }
                continue;
            }
        };
        let v = one(ctx, &mut elem, v);
        let (name, ty) = temp(ctx, &mut stmts, v);
        temps.push((false, name, ty));
    }
    // No argument fixed `T` (`count()`): the call reports it uninferred.
    if elem == HirTy::Unknown {
        return poison(span);
    }
    let path = |name: &str| super::throws::path_expr(name, span);
    let ast = |kind: ExprKind| Expr { kind, span };
    let slice_of = |target: Expr| {
        ast(ExprKind::Slice {
            target: Box::new(target),
            start: None,
            end: None,
            step: None,
        })
    };
    let trailing = if temps.iter().all(|(many, ..)| !many) {
        // `[a, b, c][:]`: a fixed array (built here, not by the list
        // literal handler, which is itself variadic) viewed as a slice.
        let n = u32::try_from(temps.len()).expect("variadic argument count overflow");
        let elements = temps
            .iter()
            .map(|(_, name, ty)| {
                let local = ctx.lookup(name).expect("temp declared");
                hir_local(local, ty, span)
            })
            .collect();
        let arr = HirExpr {
            kind: HirExprKind::ArrayInit {
                elem_ty: elem.clone(),
                elements,
            },
            ty: HirTy::Array(Box::new(elem), n),
            span,
        };
        let (name, _) = temp(ctx, &mut stmts, arr);
        lower_expected(ctx, &slice_of(path(&name)), HirTy::Unknown)
    } else {
        // `let l = List::new(); l.push(a); for x in xs { l.push(x); } l[:]`
        let new = ast(ExprKind::Call {
            callee: Box::new(super::bignum::path_expr(LIST, "new", span)),
            args: Vec::new(),
            trailing_closure: None,
        });
        let list_ty = ctx
            .instantiate_struct(LIST, vec![elem], ctx.here())
            .map_or(HirTy::Unknown, HirTy::Struct);
        let init = lower_expected(ctx, &new, list_ty);
        let (list, _) = temp(ctx, &mut stmts, init);
        let push = |x: Expr| {
            ast(ExprKind::MethodCall {
                receiver: Box::new(path(&list)),
                method: "push".to_owned(),
                type_args: Vec::new(),
                args: vec![CallArg::Positional(x)],
                trailing_closure: None,
            })
        };
        for (many, name, _) in &temps {
            let e = if *many {
                let x = format!("#vx{}", ctx.locals.len());
                ast(ExprKind::For {
                    label: None,
                    pat: rokc_ast::Pattern {
                        kind: rokc_ast::PatternKind::Binding {
                            name: x.clone(),
                            sub: None,
                        },
                        span,
                    },
                    iter: Box::new(path(name)),
                    body: rokc_ast::Block {
                        stmts: Vec::new(),
                        trailing: Some(Box::new(push(path(&x)))),
                        span,
                        label: None,
                    },
                    is_await: false,
                })
            } else {
                push(path(name))
            };
            let h = lower_expected(ctx, &e, HirTy::Unknown);
            stmts.push(HirStmt::Expr(h));
        }
        lower_expected(ctx, &slice_of(path(&list)), HirTy::Unknown)
    };
    HirExpr {
        ty: trailing.ty.clone(),
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: Some(trailing),
            label: None,
        })),
        span,
    }
}

/// `name[at]`.
pub(super) fn spread_item_at(name: &str, at: &Expr, span: rokc_diagnostic::Span) -> Expr {
    Expr {
        kind: ExprKind::Index(
            Box::new(super::throws::path_expr(name, span)),
            Box::new(at.clone()),
        ),
        span,
    }
}

/// `#spread[i]` (`i: usize`).
fn spread_item(name: &str, i: usize, span: rokc_diagnostic::Span) -> Expr {
    Expr {
        kind: ExprKind::Index(
            Box::new(super::throws::path_expr(name, span)),
            Box::new(Expr {
                kind: ExprKind::Literal(AstLit::Int(format!("{i}usize"))),
                span,
            }),
        ),
        span,
    }
}

/// `f(a: i32, b: i32)` of a resolved signature.
fn sig_of(ctx: &LowerCtx<'_>, callee: &str, sig: &FnSig) -> String {
    let tys = sig.param_tys.iter().map(|t| ctx.ty_name(t));
    super::sig_text(callee, sig.param_names.iter().map(String::as_str).zip(tys))
}

/// `f(a: i32, b: i32)` of a declaration.
fn sig_of_def(callee: &str, def: &FnDef) -> String {
    let params = def.params.iter().map(|p| {
        let t =
            p.ty.as_ref()
                .map_or("_".to_owned(), rokc_ast::print::print_ty);
        let t = if p.variadic { format!("...{t}") } else { t };
        (p.name.as_str(), t)
    });
    super::sig_text(callee, params)
}

/// Report a wrong argument count, with the signature as help.
fn arity_error(ctx: &LowerCtx<'_>, callee: &str, want: usize, got: usize, sig: Option<String>) {
    let (msg, help) = super::arity_msg(callee, want, got, sig);
    ctx.emit_error(rokc_diagnostic::ErrorKind::Type, msg);
    ctx.help_last(help);
}

/// Report surplus arguments, unless `sig` is C variadic: then `surplus`
/// are its `...` arguments, returned promoted. A method's receiver
/// (`recv`) is not counted.
fn check_arity(
    ctx: &LowerCtx<'_>,
    sig: &FnSig,
    callee: &str,
    recv: bool,
    extra: usize,
    surplus: Vec<HirExpr>,
) -> Vec<HirExpr> {
    if ctx.env.is(super::FnSet::CVariadic, sig.id) {
        return surplus
            .into_iter()
            .map(|a| c_vararg_promote(ctx, callee, a))
            .collect();
    }
    let extra = extra + surplus.len();
    if extra > 0 {
        let n = sig.param_tys.len() - usize::from(recv && !sig.param_tys.is_empty());
        arity_error(ctx, callee, n, n + extra, Some(sig_of(ctx, callee, sig)));
    }
    Vec::new()
}

/// C default argument promotions past a variadic declaration's parameters
/// (spec 8.6.10): narrow integers and `bool` to `i32`, `f32` to `f64`.
/// Aggregates are refused.
fn c_vararg_promote(ctx: &LowerCtx<'_>, callee: &str, a: HirExpr) -> HirExpr {
    let to = match &a.ty {
        HirTy::Prim(
            PrimTy::I8 | PrimTy::I16 | PrimTy::U8 | PrimTy::U16 | PrimTy::CChar | PrimTy::Bool,
        ) => PrimTy::I32,
        HirTy::Prim(PrimTy::F32) => PrimTy::F64,
        HirTy::Prim(_) | HirTy::Fn(..) | HirTy::Unknown => return a,
        other => {
            let name = ctx.ty_name(other);
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "`{callee}`: a `{name}` cannot be passed as a C variadic `...` argument \
                     (only numbers, `bool`, `char`, `str` and C function pointers; spec 8.6.10)"
                ),
            );
            return a;
        }
    };
    let span = a.span;
    let from_ty = a.ty.clone();
    HirExpr {
        kind: HirExprKind::Cast {
            operand: Box::new(a),
            from_ty,
            to_ty: HirTy::Prim(to),
        },
        ty: HirTy::Prim(to),
        span,
    }
}

/// A path call's first argument standing for `self` (`T::m(ref x)`):
/// `ref` exactly when `self` is (spec 4.7.3). `explicit`: `None` for a
/// method call.
fn check_path_receiver(
    ctx: &LowerCtx<'_>,
    explicit: Option<bool>,
    by_ref: bool,
    arg: &HirExpr,
    pname: &str,
    callee: &str,
) {
    match explicit {
        Some(false) if by_ref => ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "parameter `{pname}` of `{callee}` is `ref` - pass `ref x` at the call site \
                 (spec 4.7.3)"
            ),
        ),
        Some(true) if !by_ref => ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "argument `{pname}` of `{callee}` is passed with `ref`, but the parameter is \
                 not `ref` (spec 4.7.3)"
            ),
        ),
        Some(true) if super::mono::place_root(arg).is_none() && arg.ty != HirTy::Unknown => ctx
            .emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "`ref` argument `{pname}` of `{callee}` must name a variable or a field of \
                     one (spec 4.7)"
                ),
            ),
        _ => {}
    }
}

/// Field names from the root of a place (`p.a.b` → `[a, b]`).
fn field_path(ctx: &LowerCtx<'_>, e: &HirExpr) -> Vec<String> {
    match &e.kind {
        HirExprKind::FieldRead {
            target,
            struct_id,
            field_index,
            ..
        } => {
            let mut v = field_path(ctx, target);
            let def = ctx.struct_def(*struct_id);
            v.extend(
                def.fields
                    .get(*field_index as usize)
                    .map(|f| f.name.clone()),
            );
            v
        }
        _ => Vec::new(),
    }
}

/// Spec 4.7.2: a place passed `ref` may not be passed `ref` again or read
/// by another argument of the same call through an overlapping path, when
/// visible syntactically.
fn check_ref_exclusive(
    ctx: &LowerCtx<'_>,
    callee: &str,
    args: &[HirExpr],
    places: &[Option<HirExpr>],
    reads: &[(String, Vec<String>)],
    has_receiver: bool,
) {
    let refs: Vec<(LocalId, Vec<String>)> = places
        .iter()
        .flatten()
        .filter_map(|p| Some((super::mono::place_root(p)?, field_path(ctx, p))))
        .collect();
    let name = |l: LocalId| {
        ctx.locals
            .get(l.0 as usize)
            .and_then(|x| x.name.clone())
            .unwrap_or_default()
    };
    let overlaps = |p: &[String], q: &[String]| {
        let n = p.len().min(q.len());
        p[..n] == q[..n]
    };
    let shown = |l: LocalId, path: &[String]| {
        std::iter::once(name(l))
            .chain(path.iter().cloned())
            .collect::<Vec<_>>()
            .join(".")
    };
    for (k, (a, path)) in refs.iter().enumerate() {
        if let Some((_, q)) = refs[..k].iter().find(|(b, q)| b == a && overlaps(path, q)) {
            let what = if q == path {
                format!("`{}` is passed as `ref` twice", shown(*a, path))
            } else {
                format!(
                    "`{}` and `{}` overlap and are both passed as `ref`",
                    shown(*a, q),
                    shown(*a, path)
                )
            };
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!("{what} in one call to `{callee}` (spec 4.7.2)"),
            );
            return;
        }
    }
    // A by-value receiver is read by the call too.
    let receiver_read = if has_receiver && places.first().is_some_and(Option::is_none) {
        args.first()
            .and_then(|r| Some((super::mono::place_root(r)?, field_path(ctx, r))))
    } else {
        None
    };
    for (root, path) in &refs {
        let n = name(*root);
        let read = receiver_read
            .as_ref()
            .is_some_and(|(r, q)| r == root && overlaps(path, q))
            || reads
                .iter()
                .any(|(x, q)| *x == n && ctx.lookup(x) == Some(*root) && overlaps(path, q));
        if read {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "`{}` is passed as `ref` to `{callee}` and also read by another argument \
                     of the same call (spec 4.7.2)",
                    shown(*root, path)
                ),
            );
        }
    }
}

/// `Call` to a statically known function with checked arguments.
pub(super) fn call_sig(
    ctx: &mut LowerCtx<'_>,
    sig: &FnSig,
    callee: &str,
    receiver: Option<HirExpr>,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if ctx.env.is(super::FnSet::Unsafe, sig.id) {
        check_unsafe_call(ctx, callee);
    }
    // By span: reading the declaration would tie the caller's
    // incremental unit to the callee's body.
    let decl = ctx.env.decls.get(sig.id.0 as usize);
    if decl.is_some_and(|d| ctx.env.duplicated(d.def().span)) {
        discard_args(ctx, args);
        return poison(span);
    }
    let (lowered, lets, places) = lower_args_ref(ctx, sig, callee, receiver, args, span);
    let out = ctx.env.sink.ref_out.borrow().get(&sig.id).cloned();
    if let Some(out) = out {
        let tys: Vec<HirTy> = out.refs.iter().map(|(_, t)| t.clone()).collect();
        // `rok.OP_assign.TYPE`: the op's result is the new `ref self`.
        let kind = match (ctx.env.intrinsic(sig.id), <[HirExpr; 2]>::try_from(lowered)) {
            (Some(&Intrinsic::Prim(op)), Ok([lhs, rhs])) => HirExprKind::PrimBinOp {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            (_, Ok(two)) => HirExprKind::Call {
                callee: sig.id,
                args: two.into(),
            },
            (_, Err(args)) => HirExprKind::Call {
                callee: sig.id,
                args,
            },
        };
        let call = HirExpr {
            kind,
            ty: super::mono::ref_packed(&out.ret, &tys),
            span,
        };
        let v = super::mono::ref_unpack(ctx, call, &out, &places);
        return with_lets(lets, v);
    }
    // A C function's `fn` parameter is a bare code pointer (an
    // `extern "rok"` one takes the whole `{code, env}`).
    if ctx.env.is(super::FnSet::ExternC, sig.id) {
        for a in &lowered {
            // An `extern "C" fn` value is already a C function pointer.
            if matches!(a.ty, HirTy::Fn(..) | HirTy::Closure { .. })
                && !a.ty.is_c_fn()
                && !matches!(a.kind, HirExprKind::FnRef(_))
            {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!(
                        "`{callee}` is a C function: a `fn` argument must name a function \
                         (a C callback has no room for a closure's captures, spec 5.9)"
                    ),
                );
            }
        }
    }
    // Stdlib spec 0.3.2: an intrinsic method's call is its primitive op.
    let kind = match (ctx.env.intrinsic(sig.id), <[HirExpr; 2]>::try_from(lowered)) {
        (Some(&Intrinsic::Prim(op)), Ok([lhs, rhs])) => HirExprKind::PrimBinOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        },
        (Some(&Intrinsic::Convert), Err(mut one)) if one.len() == 1 => {
            let operand = one.remove(0);
            if operand.ty == sig.ret_ty {
                return with_lets(lets, operand);
            }
            HirExprKind::Cast {
                from_ty: operand.ty.clone(),
                to_ty: sig.ret_ty.clone(),
                operand: Box::new(operand),
            }
        }
        (Some(&Intrinsic::Prim(op)), Err(mut one)) if one.len() == 1 => {
            let operand = one.remove(0);
            let op = if op == PrimOp::Not && operand.ty == HirTy::Prim(PrimTy::Bool) {
                PrimOp::LogicalNot
            } else {
                op
            };
            HirExprKind::PrimUnaryOp {
                op,
                operand: Box::new(operand),
            }
        }
        (_, Ok(two)) => HirExprKind::Call {
            callee: sig.id,
            args: two.into(),
        },
        (_, Err(args)) => HirExprKind::Call {
            callee: sig.id,
            args,
        },
    };
    check_const_overflow(ctx, &kind, &sig.ret_ty, span);
    with_lets(
        lets,
        HirExpr {
            kind,
            ty: sig.ret_ty.clone(),
            span,
        },
    )
}

/// The type of a value an `if` / `match` / block arm leaves (lowered in
/// statement position, the value is otherwise silently dropped).
fn arm_value_ty(e: &HirExpr) -> Option<HirTy> {
    let block = |b: &HirBlock| b.trailing.as_ref().and_then(arm_value_ty);
    match &e.kind {
        HirExprKind::Block(b) => block(b),
        HirExprKind::If {
            then_block,
            else_branch,
            ..
        } => block(then_block).or_else(|| else_branch.as_deref().and_then(arm_value_ty)),
        HirExprKind::Match { arms, .. } => arms.iter().find_map(|a| arm_value_ty(&a.body)),
        _ if matches!(e.ty, HirTy::Unit | HirTy::Never | HirTy::Unknown) => None,
        _ => Some(e.ty.clone()),
    }
}

/// The value of integer arithmetic over literals (`2 * 200`), if it is
/// one; `None` past an overflow (reported where it happened).
fn const_int(e: &HirExpr) -> Option<i128> {
    let HirTy::Prim(p) = e.ty else {
        return None;
    };
    if !p.is_int() {
        return None;
    }
    let v = match &e.kind {
        HirExprKind::Const(ConstVal::Int { value, .. }) => *value,
        HirExprKind::Const(ConstVal::UInt { value, .. }) => i128::try_from(*value).ok()?,
        HirExprKind::PrimUnaryOp {
            op: PrimOp::Neg,
            operand,
        } => const_int(operand)?.checked_neg()?,
        HirExprKind::PrimBinOp { op, lhs, rhs } => {
            // The right side first: a left-leaning chain's is shallow.
            let b = const_int(rhs)?;
            const_arith(*op, const_int(lhs)?, b)?
        }
        _ => return None,
    };
    super::pattern::int_fits(v, p).then_some(v)
}

const fn const_arith(op: PrimOp, a: i128, b: i128) -> Option<i128> {
    match op {
        PrimOp::Add => a.checked_add(b),
        PrimOp::Sub => a.checked_sub(b),
        PrimOp::Mul => a.checked_mul(b),
        PrimOp::Div => a.checked_div(b),
        PrimOp::Rem => a.checked_rem(b),
        PrimOp::Mod => match a.checked_rem(b) {
            Some(r) if r != 0 && (r < 0) != (b < 0) => Some(r + b),
            r => r,
        },
        _ => None,
    }
}

/// `let m: u8 = 2 * 200;`: integer arithmetic folded from literals that
/// leaves its type (or divides by zero) is a compile error.
fn check_const_overflow(
    ctx: &LowerCtx<'_>,
    kind: &HirExprKind,
    ty: &HirTy,
    span: rokc_diagnostic::Span,
) {
    let HirTy::Prim(p) = *ty else {
        return;
    };
    let (op, lhs, rhs) = match kind {
        HirExprKind::PrimBinOp { op, lhs, rhs } => (*op, lhs, rhs),
        _ => return,
    };
    if !p.is_int()
        || !matches!(
            op,
            PrimOp::Add | PrimOp::Sub | PrimOp::Mul | PrimOp::Div | PrimOp::Rem | PrimOp::Mod
        )
    {
        return;
    }
    let Some(b) = const_int(rhs) else {
        return;
    };
    let Some(a) = const_int(lhs) else {
        return;
    };
    let msg = match const_arith(op, a, b) {
        None if b == 0 => format!("constant expression divides by zero (`{a}` by `0`)"),
        Some(v) if super::pattern::int_fits(v, p) => return,
        Some(v) => format!(
            "constant expression overflows `{}`: the result is {v}",
            p.name()
        ),
        None => format!("constant expression overflows `{}`", p.name()),
    };
    ctx.emit_error_at(rokc_diagnostic::ErrorKind::Type, span, msg);
}

/// `{ lets; e }` - temporaries a spread argument introduced, so the
/// spread tuple is evaluated once.
pub(super) fn with_lets(lets: Vec<HirStmt>, e: HirExpr) -> HirExpr {
    if lets.is_empty() {
        return e;
    }
    let (ty, span) = (e.ty.clone(), e.span);
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: lets,
            trailing: Some(e),
            label: None,
        })),
        ty,
        span,
    }
}

/// Bind explicit `::<..>` arguments by position to `def`'s parameters not
/// fixed by `self_ty`. Too many is an error; too few leave the rest to
/// inference (1.2.2).
pub(super) fn bind_explicit(
    ctx: &LowerCtx<'_>,
    def: &FnDef,
    self_ty: Option<&Ty>,
    targs: &[Ty],
    callee: &str,
) -> Result<HashMap<String, HirTy>, String> {
    let own: Vec<&String> = def
        .generics
        .params
        .iter()
        .map(|gp| {
            let (rokc_ast::GenericParam::Type { name, .. }
            | rokc_ast::GenericParam::Const { name, .. }) = gp;
            name
        })
        // A bound template's trait parameters come from the bound.
        .skip(
            ctx.env
                .bound_trait(&def.name)
                .and_then(|k| ctx.env.trait_of(k))
                .map_or(0, |t| t.generics.params.len()),
        )
        .filter(|n| !self_ty.is_some_and(|t| ast_ty_mentions(t, n)))
        .collect();
    if targs.len() > own.len() {
        return Err(super::mono::arity_msg(
            leaf_name(callee),
            own.len(),
            own.len(),
            targs.len(),
        ));
    }
    Ok(own
        .into_iter()
        .zip(targs)
        .map(|(n, t)| (n.clone(), ctx.resolve_ast_ty(t)))
        .collect())
}

/// Every type argument of generic fn `def` inferred from the expected
/// `fn(..) -> R` slot of its value, or `None`.
fn generic_fn_args_from_slot(ctx: &LowerCtx<'_>, def: &FnDef) -> Option<HashMap<String, HirTy>> {
    let Some(HirTy::Fn(ptys, ret, _)) = ctx.current_expected() else {
        return None;
    };
    if ptys.len() != def.params.len() {
        return None;
    }
    let names: std::collections::HashSet<String> = def
        .generics
        .params
        .iter()
        .map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. }
            | rokc_ast::GenericParam::Const { name, .. } => name.clone(),
        })
        .collect();
    let mut subst = HashMap::new();
    for (p, t) in def.params.iter().zip(ptys) {
        super::mono::unify_param_ty_with(p.ty.as_ref()?, t, &names, &mut subst, ctx);
    }
    if let Some(r) = &def.ret_ty {
        super::mono::unify_param_ty_with(r, ret, &names, &mut subst, ctx);
    }
    (subst.len() == names.len()).then_some(subst)
}

/// Does AST type `ty` name `name` (a type parameter)?
pub(super) fn ast_ty_mentions(ty: &Ty, name: &str) -> bool {
    let rec = |t: &Ty| ast_ty_mentions(t, name);
    match &ty.kind {
        AstTyKind::Path(p) => p
            .segments
            .iter()
            .any(|s| s.name == name || s.generics.iter().any(rec)),
        AstTyKind::Sum(ts) | AstTyKind::Tuple(ts) => ts.iter().any(rec),
        AstTyKind::Fn(a, r, _) => a.iter().any(rec) || rec(r),
        AstTyKind::Throws { ok, err } => rec(ok) || rec(err),
        AstTyKind::Array(e, _) | AstTyKind::Nest(e) | AstTyKind::Binding(_, e) => rec(e),
        AstTyKind::QPath(q) => rec(&q.self_ty),
        _ => false,
    }
}

/// `x.drop()` is banned for `Drop::drop`; the `using` desugar's
/// `x.#drop()` requires `Drop` (spec 3.17.7).
fn drop_method<'m>(
    ctx: &LowerCtx<'_>,
    recv_ty: &HirTy,
    method: &'m str,
    span: rokc_diagnostic::Span,
) -> Result<&'m str, HirExpr> {
    match method {
        rokc_ast::USING_DROP_METHOD => {
            if *recv_ty != HirTy::Unknown && !needs_drop(ctx, recv_ty, 0) {
                return Err(ctx.error_expr(
                    span,
                    format!(
                        "`using` needs a resource whose type implements `Drop`; `{}` does not \
                         implement `Drop` nor hold a `Drop` field (spec 3.17.7)",
                        ctx.ty_name(recv_ty)
                    ),
                ));
            }
            Ok(method)
        }
        "drop" if is_drop_method(ctx, recv_ty) => {
            direct_drop_error(ctx, span).map_or(Ok("drop"), Err)
        }
        m => Ok(m),
    }
}

/// Nesting past which drop glue stops looking (types are finite; this
/// bounds the work on deep ones).
const DROP_GLUE_DEPTH: u32 = 64;

/// Does releasing a `ty` run any `Drop::drop` - its own, or one of a
/// field / tuple element / payload's (stdlib spec 2.13.4)?
fn needs_drop(ctx: &LowerCtx<'_>, ty: &HirTy, depth: u32) -> bool {
    if depth > DROP_GLUE_DEPTH {
        return false;
    }
    if ctx.has_trait(ty, ctx.lang("drop_method")) {
        return true;
    }
    let any = |ts: &[HirTy]| ts.iter().any(|t| needs_drop(ctx, t, depth + 1));
    match ty {
        HirTy::Struct(sid) => {
            let fields: Vec<HirTy> = ctx
                .struct_def(*sid)
                .fields
                .iter()
                .map(|f| f.ty.clone())
                .collect();
            any(&fields)
        }
        HirTy::Enum(eid) => {
            let tys: Vec<HirTy> = ctx
                .enum_def(*eid)
                .variants
                .iter()
                .flat_map(variant_arg_tys)
                .collect();
            any(&tys)
        }
        HirTy::Tuple(ts) | HirTy::Sum(ts) => any(ts),
        _ => false,
    }
}

/// A `using` exit for `value` (stdlib 2.13.4): its `drop`, then each
/// `Drop` field / element in reverse order, recursively; a sum releases
/// the held variant's payload.
fn drop_glue(
    ctx: &mut LowerCtx<'_>,
    value: HirExpr,
    span: rokc_diagnostic::Span,
    depth: u32,
) -> Vec<HirStmt> {
    let mut out = Vec::new();
    if !needs_drop(ctx, &value.ty, depth) {
        return out;
    }
    let ty = value.ty.clone();
    if ctx.has_trait(&ty, ctx.lang("drop_method")) {
        out.push(HirStmt::Expr(method_call_on(
            ctx,
            value.clone(),
            "drop",
            &[],
            span,
        )));
    }
    let block = |stmts: Vec<HirStmt>| HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: None,
            label: None,
        })),
        ty: HirTy::Unit,
        span,
    };
    let unit_arm = || HirMatchArm {
        pat: HirPattern::Wildcard,
        guard: None,
        body: block(Vec::new()),
    };
    match &ty {
        HirTy::Struct(sid) => {
            let fields: Vec<HirTy> = ctx
                .struct_def(*sid)
                .fields
                .iter()
                .map(|f| f.ty.clone())
                .collect();
            for (i, fty) in fields.iter().enumerate().rev() {
                let field = HirExpr {
                    kind: HirExprKind::FieldRead {
                        target: Box::new(value.clone()),
                        struct_id: *sid,
                        field_index: u32::try_from(i).expect("field overflow"),
                        field_ty: fty.clone(),
                    },
                    ty: fty.clone(),
                    span,
                };
                out.extend(drop_glue(ctx, field, span, depth + 1));
            }
        }
        HirTy::Tuple(ts) => {
            for (i, t) in ts.iter().enumerate().rev() {
                let elem = HirExpr {
                    kind: HirExprKind::TupleField {
                        target: Box::new(value.clone()),
                        index: u32::try_from(i).expect("tuple field overflow"),
                        field_ty: t.clone(),
                    },
                    ty: t.clone(),
                    span,
                };
                out.extend(drop_glue(ctx, elem, span, depth + 1));
            }
        }
        HirTy::Enum(eid) => {
            let variants = ctx.enum_def(*eid).variants;
            let mut arms = Vec::new();
            for (vi, v) in variants.iter().enumerate() {
                let tys = variant_arg_tys(v);
                if !tys.iter().any(|t| needs_drop(ctx, t, depth + 1)) {
                    continue;
                }
                let locals: Vec<LocalId> =
                    tys.iter().map(|t| ctx.declare(None, t.clone())).collect();
                let mut stmts = Vec::new();
                for (l, t) in locals.iter().zip(&tys).rev() {
                    stmts.extend(drop_glue(ctx, hir_local(*l, t, span), span, depth + 1));
                }
                arms.push(HirMatchArm {
                    pat: HirPattern::EnumVariant {
                        enum_id: *eid,
                        variant_index: u32::try_from(vi).expect("variant overflow"),
                        args: locals
                            .iter()
                            .zip(&tys)
                            .map(|(l, t)| HirPattern::Binding {
                                local: *l,
                                ty: t.clone(),
                            })
                            .collect(),
                    },
                    guard: None,
                    body: block(stmts),
                });
            }
            arms.push(unit_arm());
            out.push(HirStmt::Expr(HirExpr {
                kind: HirExprKind::Match {
                    scrutinee: Box::new(value),
                    arms,
                },
                ty: HirTy::Unit,
                span,
            }));
        }
        HirTy::Sum(ts) => {
            let mut arms = Vec::new();
            for (vi, t) in ts.iter().enumerate() {
                if !needs_drop(ctx, t, depth + 1) {
                    continue;
                }
                let l = ctx.declare(None, t.clone());
                let stmts = drop_glue(ctx, hir_local(l, t, span), span, depth + 1);
                arms.push(HirMatchArm {
                    pat: HirPattern::SumVariant {
                        sum_ty: ty.clone(),
                        variant_index: u32::try_from(vi).expect("variant overflow"),
                        binder: Some(l),
                        payload_ty: Some(t.clone()),
                    },
                    guard: None,
                    body: block(stmts),
                });
            }
            arms.push(unit_arm());
            out.push(HirStmt::Expr(HirExpr {
                kind: HirExprKind::Match {
                    scrutinee: Box::new(value),
                    arms,
                },
                ty: HirTy::Unit,
                span,
            }));
        }
        _ => {}
    }
    out
}

/// The standard handle types a hand-off copies through their
/// `Transfer::transfer_copy` (stdlib 2.13.9); a coroutine future's copies
/// what its frame captured. Other types are copied part by part, or kept
/// when `Sync` or `unsafe impl Send` (they manage their own sharing:
/// `Task`, files, sockets).
const TRANSFER_TYPES: &[&str] = &[
    "rok::core::collections::list::List",
    "rok::core::collections::dict::Dict",
    "rok::core::collections::deque::Deque",
    "rok::core::collections::slice::Slice",
    "rok::core::str::StrBuf",
    "rok::bare::gc::GcPtr",
    "rok::core::future::AsyncFuture",
    "rok::core::future::TryAsyncFuture",
    // Not copied: marked as handed on (its joiners copy the result).
    "rok::core::task::Task",
];

fn transfer_type(ctx: &LowerCtx<'_>, ty: &HirTy) -> bool {
    let HirTy::Struct(sid) = ty else {
        return false;
    };
    let name = ctx.struct_def(*sid).name;
    let base = name.split("__").next().unwrap_or(&name);
    TRANSFER_TYPES.contains(&base)
}

/// Does a hand-off of a `ty` copy anything: does it hold, by value, a
/// handle `TRANSFER_TYPES` lists outside `Sync` and `unsafe impl Send`
/// parts?
pub(super) fn transfer_needs(ctx: &LowerCtx<'_>, ty: &HirTy, depth: u32) -> bool {
    if depth > DROP_GLUE_DEPTH {
        return false;
    }
    if transfer_type(ctx, ty) {
        return true;
    }
    let rec = |t: &HirTy| transfer_needs(ctx, t, depth + 1);
    let shared = |ty: &HirTy| {
        ctx.has_trait(ty, ctx.lang("sync_trait"))
            || ctx.has_impl(ty, &TraitRef::plain(ctx.lang("send_trait")), 0)
    };
    match ty {
        HirTy::Struct(sid) => {
            let def = ctx.struct_def(*sid);
            def.union_kind.is_none() && !shared(ty) && def.fields.iter().any(|f| rec(&f.ty))
        }
        HirTy::Enum(eid) => {
            !shared(ty)
                && ctx
                    .enum_def(*eid)
                    .variants
                    .iter()
                    .flat_map(variant_arg_tys)
                    .any(|t| rec(&t))
        }
        HirTy::Tuple(ts) | HirTy::Sum(ts) => !shared(ty) && ts.iter().any(rec),
        HirTy::Array(e, _) => rec(e),
        _ => false,
    }
}

/// Is `ty` one listed handle whose elements need no copy?
fn transfer_flat(ctx: &LowerCtx<'_>, ty: &HirTy) -> bool {
    let HirTy::Struct(sid) = ty else {
        return false;
    };
    transfer_type(ctx, ty)
        && ctx
            .struct_def(*sid)
            .generic_args
            .iter()
            .all(|t| !transfer_needs(ctx, t, 1))
}

/// Does a `ty` hold no GC reference, so its bytes copy as they are?
fn transfer_plain(ctx: &LowerCtx<'_>, ty: &HirTy, depth: u32) -> bool {
    if depth > DROP_GLUE_DEPTH {
        return false;
    }
    let rec = |t: &HirTy| transfer_plain(ctx, t, depth + 1);
    match ty {
        HirTy::Prim(p) => *p != PrimTy::Str,
        HirTy::Unit | HirTy::None => true,
        HirTy::Array(e, _) => rec(e),
        HirTy::Tuple(ts) | HirTy::Sum(ts) => ts.iter().all(rec),
        HirTy::Struct(sid) => {
            let def = ctx.struct_def(*sid);
            match &def.union_kind {
                Some(u) => u.aliases.iter().all(|(_, t)| rec(t)),
                None => def.fields.iter().all(|f| rec(&f.ty)),
            }
        }
        HirTy::Enum(eid) => ctx
            .enum_def(*eid)
            .variants
            .iter()
            .flat_map(variant_arg_tys)
            .all(|t| rec(&t)),
        _ => false,
    }
}

/// The generic functions `rok.hand_over` calls when it copies (with one
/// argument, and with the value the slot keeps first).
const HAND_OVER_COPY: &str = "rok::core::marker::hand_over_copy";
const HAND_OVER_INTO_COPY: &str = "rok::core::marker::hand_over_into_copy";

/// `rok.transfer::<T>(x, m)` (`x` with every collection it reaches copied,
/// `m` the hand-off's memo), `rok.transfer_keep` (each noted as its own
/// copy), `rok.transfer_needed::<T>()`, `rok.transfer_plain::<T>()` and
/// `rok.hand_over::<T>(x)` / `(kept, x)` (no call at all when nothing is
/// copied).
fn lower_transfer(
    ctx: &mut LowerCtx<'_>,
    op: Intrinsic,
    name: &str,
    targs: &[Ty],
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let Some(t) = targs.first().filter(|_| targs.len() == 1) else {
        discard_args(ctx, args);
        return ctx.error_expr(span, format!("`{name}` is called as `{name}::<T>(..)`"));
    };
    let t = ctx.resolve_ast_ty(t);
    if op == Intrinsic::HandOver {
        let (x, copy) = match args {
            [CallArg::Positional(x)] => (x, HAND_OVER_COPY),
            [CallArg::Positional(kept), CallArg::Positional(x)] => {
                if !transfer_needs(ctx, &t, 0) {
                    let kept = lower_expected(ctx, kept, t.clone());
                    let x = lower_expected(ctx, x, t.clone());
                    let x = coerce(ctx, &t, x, &|| format!("the value of `{name}`"));
                    let local = ctx.declare(None, t.clone());
                    return HirExpr {
                        kind: HirExprKind::Block(Box::new(HirBlock {
                            stmts: vec![HirStmt::Expr(kept), HirStmt::Let { local, init: x }],
                            trailing: Some(hir_local(local, &t, span)),
                            label: None,
                        })),
                        ty: t,
                        span,
                    };
                }
                (x, HAND_OVER_INTO_COPY)
            }
            _ => {
                discard_args(ctx, args);
                return ctx.error_expr(
                    span,
                    format!("`{name}` takes one value (or the kept one first)"),
                );
            }
        };
        if transfer_needs(ctx, &t, 0) {
            if let Some(def) = ctx.env.generic_fn(copy) {
                return call_generic(ctx, def, None, None, args, copy, span);
            }
        }
        let x = lower_expected(ctx, x, t.clone());
        return coerce(ctx, &t, x, &|| format!("the value of `{name}`"));
    }
    if matches!(
        op,
        Intrinsic::TransferNeeded | Intrinsic::TransferPlain | Intrinsic::TransferFlat
    ) {
        discard_args(ctx, args);
        let yes = match op {
            Intrinsic::TransferNeeded => transfer_needs(ctx, &t, 0),
            Intrinsic::TransferPlain => transfer_plain(ctx, &t, 0),
            _ => transfer_flat(ctx, &t),
        };
        return HirExpr {
            kind: HirExprKind::Const(ConstVal::Bool(yes)),
            ty: HirTy::Prim(PrimTy::Bool),
            span,
        };
    }
    let [CallArg::Positional(x), CallArg::Positional(memo)] = args else {
        discard_args(ctx, args);
        return ctx.error_expr(span, format!("`{name}` takes a value and a memo"));
    };
    let x = lower_expected(ctx, x, t.clone());
    let x = coerce(ctx, &t, x, &|| format!("the value of `{name}`"));
    let method = match op {
        Intrinsic::TransferKeep => "transfer_keep",
        Intrinsic::TransferVisit => "transfer_visit",
        _ => "transfer_copy",
    };
    transfer_glue(ctx, x, memo, method, span, 0)
}

/// `value` with every collection it reaches copied: a listed handle by
/// its `method` (`transfer_copy` / `transfer_keep`), a struct, tuple,
/// array, enum or sum part by part. Recursion follows the type, never the
/// data: a handle's elements wait on the memo's queue.
fn transfer_glue(
    ctx: &mut LowerCtx<'_>,
    value: HirExpr,
    memo: &Expr,
    method: &str,
    span: rokc_diagnostic::Span,
    depth: u32,
) -> HirExpr {
    let ty = value.ty.clone();
    if !transfer_needs(ctx, &ty, depth) {
        return value;
    }
    if transfer_type(ctx, &ty) {
        return method_call_on(
            ctx,
            value,
            method,
            &[CallArg::Positional(memo.clone())],
            span,
        );
    }
    let at = |i: usize| u32::try_from(i).expect("index overflow");
    let local = ctx.declare(None, ty.clone());
    let v = hir_local(local, &ty, span);
    let part =
        |ctx: &mut LowerCtx<'_>, e: HirExpr| transfer_glue(ctx, e, memo, method, span, depth + 1);
    if let HirTy::Array(e, n) = &ty {
        return transfer_array(ctx, value, local, e, *n, &part, span);
    }
    let copy = match &ty {
        HirTy::Struct(sid) => {
            let fields: Vec<HirTy> = ctx
                .struct_def(*sid)
                .fields
                .iter()
                .map(|f| f.ty.clone())
                .collect();
            let fields = fields
                .iter()
                .enumerate()
                .map(|(i, fty)| {
                    let read = HirExpr {
                        kind: HirExprKind::FieldRead {
                            target: Box::new(v.clone()),
                            struct_id: *sid,
                            field_index: at(i),
                            field_ty: fty.clone(),
                        },
                        ty: fty.clone(),
                        span,
                    };
                    part(ctx, read)
                })
                .collect();
            HirExprKind::StructInit {
                struct_id: *sid,
                fields,
            }
        }
        HirTy::Tuple(ts) => HirExprKind::Tuple {
            fields: ts
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let read = HirExpr {
                        kind: HirExprKind::TupleField {
                            target: Box::new(v.clone()),
                            index: at(i),
                            field_ty: t.clone(),
                        },
                        ty: t.clone(),
                        span,
                    };
                    part(ctx, read)
                })
                .collect(),
        },
        HirTy::Enum(eid) => {
            let variants = ctx.enum_def(*eid).variants;
            let mut arms = Vec::new();
            for (vi, var) in variants.iter().enumerate() {
                let tys = variant_arg_tys(var);
                if !tys.iter().any(|t| transfer_needs(ctx, t, depth + 1)) {
                    continue;
                }
                let locals: Vec<LocalId> =
                    tys.iter().map(|t| ctx.declare(None, t.clone())).collect();
                let args = locals
                    .iter()
                    .zip(&tys)
                    .map(|(l, t)| part(ctx, hir_local(*l, t, span)))
                    .collect();
                arms.push(HirMatchArm {
                    pat: HirPattern::EnumVariant {
                        enum_id: *eid,
                        variant_index: at(vi),
                        args: locals
                            .iter()
                            .zip(&tys)
                            .map(|(l, t)| HirPattern::Binding {
                                local: *l,
                                ty: t.clone(),
                            })
                            .collect(),
                    },
                    guard: None,
                    body: HirExpr {
                        kind: HirExprKind::EnumVariantInit {
                            enum_id: *eid,
                            variant_index: at(vi),
                            args,
                        },
                        ty: ty.clone(),
                        span,
                    },
                });
            }
            arms.push(HirMatchArm {
                pat: HirPattern::Wildcard,
                guard: None,
                body: v.clone(),
            });
            HirExprKind::Match {
                scrutinee: Box::new(v),
                arms,
            }
        }
        HirTy::Sum(ts) => {
            let mut arms = Vec::new();
            for (vi, t) in ts.iter().enumerate() {
                if !transfer_needs(ctx, t, depth + 1) {
                    continue;
                }
                let l = ctx.declare(None, t.clone());
                let inner = part(ctx, hir_local(l, t, span));
                arms.push(HirMatchArm {
                    pat: HirPattern::SumVariant {
                        sum_ty: ty.clone(),
                        variant_index: at(vi),
                        binder: Some(l),
                        payload_ty: Some(t.clone()),
                    },
                    guard: None,
                    body: HirExpr {
                        kind: HirExprKind::SumWrap {
                            sum_ty: ty.clone(),
                            variant_index: at(vi),
                            value: Some(Box::new(inner)),
                        },
                        ty: ty.clone(),
                        span,
                    },
                });
            }
            arms.push(HirMatchArm {
                pat: HirPattern::Wildcard,
                guard: None,
                body: v.clone(),
            });
            HirExprKind::Match {
                scrutinee: Box::new(v),
                arms,
            }
        }
        _ => return value,
    };
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![HirStmt::Let { local, init: value }],
            trailing: Some(HirExpr {
                kind: copy,
                ty: ty.clone(),
                span,
            }),
            label: None,
        })),
        ty,
        span,
    }
}

/// `let a = value; let i = 0; while i < n { a[i] = part(a[i]); i += 1 } a`.
fn transfer_array(
    ctx: &mut LowerCtx<'_>,
    value: HirExpr,
    a: LocalId,
    elem: &HirTy,
    n: u32,
    part: &dyn Fn(&mut LowerCtx<'_>, HirExpr) -> HirExpr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let usize_ty = HirTy::Prim(PrimTy::Usize);
    let arr_ty = HirTy::Array(Box::new(elem.clone()), n);
    let i = ctx.declare(None, usize_ty.clone());
    let num = |v: u32| HirExpr {
        kind: HirExprKind::Const(ConstVal::Int {
            value: i128::from(v),
            ty: PrimTy::Usize,
        }),
        ty: usize_ty.clone(),
        span,
    };
    let bin = |op: PrimOp, lhs: HirExpr, rhs: HirExpr, ty: HirTy| HirExpr {
        kind: HirExprKind::PrimBinOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        },
        ty,
        span,
    };
    let read = HirExpr {
        kind: HirExprKind::ArrayIndex {
            array: Box::new(hir_local(a, &arr_ty, span)),
            index: Box::new(hir_local(i, &usize_ty, span)),
            elem_ty: elem.clone(),
        },
        ty: elem.clone(),
        span,
    };
    let body = HirBlock {
        stmts: vec![
            HirStmt::ArrayAssign {
                array_local: a,
                path: Vec::new(),
                index: hir_local(i, &usize_ty, span),
                value: part(ctx, read),
                elem_ty: elem.clone(),
            },
            HirStmt::Assign {
                local: i,
                value: bin(
                    PrimOp::Add,
                    hir_local(i, &usize_ty, span),
                    num(1),
                    usize_ty.clone(),
                ),
            },
        ],
        trailing: None,
        label: None,
    };
    let cond = bin(
        PrimOp::Lt,
        hir_local(i, &usize_ty, span),
        num(n),
        HirTy::Prim(PrimTy::Bool),
    );
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![
                HirStmt::Let {
                    local: a,
                    init: value,
                },
                HirStmt::Let {
                    local: i,
                    init: num(0),
                },
                HirStmt::Expr(HirExpr {
                    kind: HirExprKind::While {
                        label: None,
                        cond: Box::new(cond),
                        body: Box::new(body),
                    },
                    ty: HirTy::Unit,
                    span,
                }),
            ],
            trailing: Some(hir_local(a, &arr_ty, span)),
            label: None,
        })),
        ty: arr_ty,
        span,
    }
}

/// Does `ty`'s `drop` method come from `Drop`?
fn is_drop_method(ctx: &LowerCtx<'_>, ty: &HirTy) -> bool {
    let trait_name = match pick_method(ctx, ty, "drop", None) {
        Ok(Some(MethodPick::Concrete(id))) => {
            ctx.env.method_info(id).and_then(|i| i.trait_name.clone())
        }
        Ok(Some(MethodPick::Generic(gm))) => gm.info.trait_name.clone(),
        _ => None,
    };
    trait_name.as_deref() == Some(ctx.lang("drop_method"))
}

/// Spec 2.13.4: `Drop::drop` runs only on `using` exit; a direct call is
/// an error outside `unsafe` bare-tier code.
fn direct_drop_error(ctx: &LowerCtx<'_>, span: rokc_diagnostic::Span) -> Option<HirExpr> {
    if ctx.unsafe_depth > 0 && ctx.current_module.starts_with("rok::bare") {
        return None;
    }
    Some(
        ctx.error_expr(
            span,
            "`Drop::drop` cannot be called directly; it runs when a `using` block exits \
         (spec 2.13.4) - expose a method such as `close()` to release early"
                .to_owned(),
        ),
    )
}

/// `Drop::drop(x)` / `File::drop(f)` / `T::drop(x)`.
fn check_direct_drop_path(ctx: &LowerCtx<'_>, p: &rokc_ast::Path) {
    let [prefix @ .., last] = p.segments.as_slice() else {
        return;
    };
    if last.name != "drop" || prefix.is_empty() {
        return;
    }
    let segs: Vec<&str> = prefix.iter().map(|s| s.name.as_str()).collect();
    let is_drop = ctx
        .env
        .names
        .trait_key(&segs, &ctx.current_module, ctx.here())
        == ctx.lang("drop_method")
        || {
            let ty = ctx.resolve_ast_ty(&rokc_ast::Ty {
                kind: rokc_ast::TyKind::Path(rokc_ast::Path {
                    segments: prefix.to_vec(),
                    span: p.span,
                }),
                span: p.span,
            });
            is_drop_method(ctx, &ty)
        };
    if is_drop {
        direct_drop_error(ctx, p.span);
    }
}

/// A trait a sum has when all its members do (spec 2.13.6, 2.5.7, 2.10.9)?
pub(super) fn is_sum_trait(ctx: &LowerCtx<'_>, key: &str) -> bool {
    [
        "clone_trait",
        "debug_trait",
        "eq_trait",
        "hash_trait",
        "cmp_trait",
        "ord_trait",
    ]
    .into_iter()
    .any(|s| ctx.lang(s) == key)
        || [COPY_TRAIT, EQ_TRAIT].contains(&key)
}

/// Their methods, dispatched on the variant by `sum_structural_call`.
const SUM_METHODS: &[&str] = &[
    "clone", "fmt", "eq", "ne", "hash", "lt", "gt", "le", "ge", "cmp",
];

/// `clone` / `Debug::fmt` / `eq` / `ne` / `hash` / `lt`.. / `cmp` on a sum:
/// a `match` calling the variant's method. Across variants `eq` is false
/// and ordering follows the canonical member order (`HirTy::sum`), like a
/// derived enum. `none` clones, prints, compares and hashes as itself.
/// `lowered_rhs`: an operator's already lowered right operand.
fn sum_structural_call(
    ctx: &mut LowerCtx<'_>,
    recv: HirExpr,
    method: &str,
    args: &[CallArg],
    lowered_rhs: Option<HirExpr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let HirTy::Sum(variants) = recv.ty.clone() else {
        return recv;
    };
    let sum_ty = recv.ty.clone();
    let binary = matches!(method, "eq" | "ne" | "lt" | "gt" | "le" | "ge" | "cmp");
    let bool_c = |b: bool| HirExpr {
        kind: HirExprKind::Const(ConstVal::Bool(b)),
        ty: HirTy::Prim(PrimTy::Bool),
        span,
    };
    let ordering = |ctx: &mut LowerCtx<'_>, variant: &str| {
        let segments = ["rok", "core", "cmp", "Ordering", variant]
            .iter()
            .map(|n| rokc_ast::PathSegment {
                name: (*n).to_owned(),
                generics: Vec::new(),
            })
            .collect();
        lower_expr(
            ctx,
            &Expr {
                kind: ExprKind::Path(AstPath { segments, span }),
                span,
            },
        )
    };
    let result_ty = match method {
        "clone" => sum_ty.clone(),
        "fmt" => HirTy::Unit,
        "hash" => HirTy::Prim(PrimTy::U64),
        "cmp" => ordering(ctx, "Equal").ty,
        _ => HirTy::Prim(PrimTy::Bool),
    };
    // The result when the two sides are different variants (`less`: the
    // left one comes first) or both `none`.
    let across = |ctx: &mut LowerCtx<'_>, less: bool| match method {
        "cmp" => ordering(ctx, if less { "Less" } else { "Greater" }),
        "lt" | "le" => bool_c(less),
        "gt" | "ge" => bool_c(!less),
        _ => bool_c(method == "ne"),
    };
    let same_none = |ctx: &mut LowerCtx<'_>| match method {
        "cmp" => ordering(ctx, "Equal"),
        _ => bool_c(matches!(method, "eq" | "le" | "ge")),
    };
    let mut stmts = Vec::new();
    let s = ctx.declare(None, sum_ty.clone());
    stmts.push(HirStmt::Let {
        local: s,
        init: recv,
    });
    let rhs = if binary {
        let v = if let Some(v) = lowered_rhs {
            v
        } else {
            let [CallArg::Positional(e)] = args else {
                discard_args(ctx, args);
                return ctx.error_expr(span, format!("`{method}` takes one argument"));
            };
            lower_expected(ctx, e, sum_ty.clone())
        };
        let v = coerce(ctx, &sum_ty, v, &|| format!("the argument of `{method}`"));
        let r = ctx.declare(None, sum_ty.clone());
        stmts.push(HirStmt::Let { local: r, init: v });
        Some(r)
    } else {
        if method != "fmt" {
            discard_args(ctx, args);
        }
        None
    };
    let path = |name: &str| Expr {
        kind: ExprKind::Path(AstPath {
            segments: vec![rokc_ast::PathSegment {
                name: name.to_owned(),
                generics: Vec::new(),
            }],
            span,
        }),
        span,
    };
    let call = |m: &str, arg: Option<&str>| Expr {
        kind: ExprKind::MethodCall {
            receiver: Box::new(path("#a")),
            method: m.to_owned(),
            type_args: Vec::new(),
            args: arg
                .map(|a| CallArg::Positional(path(a)))
                .into_iter()
                .collect(),
            trailing_closure: None,
        },
        span,
    };
    let arm_pat = |ctx: &mut LowerCtx<'_>, i: usize, name: &str| {
        let vt = &variants[i];
        let none = vt.is_none();
        let binder = (!none).then(|| ctx.declare(Some(name.to_owned()), vt.clone()));
        HirPattern::SumVariant {
            sum_ty: sum_ty.clone(),
            variant_index: u32::try_from(i).expect("variant overflow"),
            binder,
            payload_ty: (!none).then(|| vt.clone()),
        }
    };
    let mut arms = Vec::new();
    for (i, vt) in variants.iter().enumerate() {
        ctx.push_scope();
        let pat = arm_pat(ctx, i, "#a");
        let none = vt.is_none();
        let body = match (method, rhs) {
            ("clone", _) if none => hir_local(s, &sum_ty, span),
            ("clone", _) => {
                let v = lower_expr(ctx, &call("clone", None));
                wrap_into_sum(&sum_ty, v)
            }
            ("fmt", _) => {
                // `Debug::fmt(#a, ref f)`, or `none` written.
                let fmt_path = |names: &[&str]| Expr {
                    kind: ExprKind::Path(AstPath {
                        segments: names
                            .iter()
                            .map(|n| rokc_ast::PathSegment {
                                name: (*n).to_owned(),
                                generics: Vec::new(),
                            })
                            .collect(),
                        span,
                    }),
                    span,
                };
                let debug_fmt = format!("{}::fmt", ctx.lang("debug_trait"));
                let debug_fmt: Vec<&str> = debug_fmt.split("::").collect();
                let (callee, first) = if none {
                    let lit = Expr {
                        kind: ExprKind::Literal(AstLit::Str("none".to_owned())),
                        span,
                    };
                    (&["rok", "core", "fmt", "Formatter", "write_str"][..], lit)
                } else {
                    (&debug_fmt[..], path("#a"))
                };
                let mut call_args = args.to_vec();
                if none {
                    call_args.push(CallArg::Positional(first));
                } else {
                    call_args.insert(0, CallArg::Positional(first));
                }
                let c = Expr {
                    kind: ExprKind::Call {
                        callee: Box::new(fmt_path(callee)),
                        args: call_args,
                        trailing_closure: None,
                    },
                    span,
                };
                lower_expr(ctx, &c)
            }
            ("hash", _) => {
                let tag = hir_int(i128::try_from(i).unwrap_or(0), PrimTy::U64, span);
                if none {
                    tag
                } else {
                    let h = lower_expr(ctx, &call("hash", None));
                    HirExpr {
                        kind: HirExprKind::PrimBinOp {
                            op: PrimOp::BitXor,
                            lhs: Box::new(h),
                            rhs: Box::new(tag),
                        },
                        ty: HirTy::Prim(PrimTy::U64),
                        span,
                    }
                }
            }
            (_, Some(r)) => {
                // Same variant: compare payloads; else the variant order
                // decides.
                let eq_like = matches!(method, "eq" | "ne");
                let mut inner = Vec::new();
                for j in 0..variants.len() {
                    if j != i && eq_like {
                        continue;
                    }
                    ctx.push_scope();
                    let inner_pat = arm_pat(ctx, j, "#b");
                    let body = if j != i {
                        across(ctx, i < j)
                    } else if none {
                        same_none(ctx)
                    } else {
                        lower_expr(ctx, &call(method, Some("#b")))
                    };
                    ctx.pop_scope();
                    inner.push(HirMatchArm {
                        pat: inner_pat,
                        guard: None,
                        body,
                    });
                }
                if eq_like {
                    inner.push(HirMatchArm {
                        pat: HirPattern::Wildcard,
                        guard: None,
                        body: across(ctx, false),
                    });
                }
                HirExpr {
                    kind: HirExprKind::Match {
                        scrutinee: Box::new(hir_local(r, &sum_ty, span)),
                        arms: inner,
                    },
                    ty: result_ty.clone(),
                    span,
                }
            }
            _ => lower_expr(ctx, &call(method, None)),
        };
        ctx.pop_scope();
        let body = coerce(ctx, &result_ty, body, &|| {
            format!("`{method}` of a sum value")
        });
        arms.push(HirMatchArm {
            pat,
            guard: None,
            body,
        });
    }
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: Some(HirExpr {
                kind: HirExprKind::Match {
                    scrutinee: Box::new(hir_local(s, &sum_ty, span)),
                    arms,
                },
                ty: result_ty.clone(),
                span,
            }),
            label: None,
        })),
        ty: result_ty,
        span,
    }
}

/// `Ord`'s `min` / `max` / `clamp` on a sum, inlined from the trait's
/// default bodies (its `Ord` is compiler-dispatched).
fn sum_min_max(
    ctx: &mut LowerCtx<'_>,
    recv: HirExpr,
    method: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let ty = recv.ty.clone();
    let want = if method == "clamp" { 2 } else { 1 };
    if args.len() != want || args.iter().any(|a| !matches!(a, CallArg::Positional(_))) {
        discard_args(ctx, args);
        let (msg, _) = super::arity_msg(method, want, args.len(), None);
        return ctx.error_expr(span, msg);
    }
    let mut stmts = Vec::new();
    let mut local = |ctx: &mut LowerCtx<'_>, init: HirExpr| {
        let l = ctx.declare(None, ty.clone());
        stmts.push(HirStmt::Let { local: l, init });
        l
    };
    let a = local(ctx, recv);
    let mut ls = Vec::new();
    for arg in args {
        let CallArg::Positional(e) = arg else {
            unreachable!("checked above")
        };
        let v = lower_expected(ctx, e, ty.clone());
        let v = coerce(ctx, &ty, v, &|| format!("the argument of `{method}`"));
        ls.push(local(ctx, v));
    }
    let get = |l: LocalId| hir_local(l, &ty, span);
    let test = |ctx: &mut LowerCtx<'_>, op: &str, l: LocalId, r: LocalId| {
        sum_structural_call(ctx, get(l), op, &[], Some(get(r)), span)
    };
    let pick = |cond: HirExpr, then: HirExpr, other: HirExpr| HirExpr {
        kind: HirExprKind::If {
            cond: Box::new(cond),
            then_block: Box::new(HirBlock {
                stmts: Vec::new(),
                trailing: Some(then),
                label: None,
            }),
            else_branch: Some(Box::new(other)),
        },
        ty: ty.clone(),
        span,
    };
    let value = match (method, ls.as_slice()) {
        ("min", [b]) => pick(test(ctx, "le", a, *b), get(a), get(*b)),
        ("max", [b]) => pick(test(ctx, "ge", a, *b), get(a), get(*b)),
        (_, [lo, hi]) => {
            let above = pick(test(ctx, "gt", a, *hi), get(*hi), get(a));
            pick(test(ctx, "lt", a, *lo), get(*lo), above)
        }
        _ => unreachable!("arity checked above"),
    };
    with_lets(stmts, value)
}

/// `x` / `x.f.g` for a local or a field chain on one.
fn place_text(ctx: &LowerCtx<'_>, e: &HirExpr) -> Option<String> {
    match &e.kind {
        HirExprKind::Local(l) => ctx.locals.get(l.0 as usize)?.name.clone(),
        HirExprKind::FieldRead {
            target,
            struct_id,
            field_index,
            ..
        } => {
            let base = place_text(ctx, target)?;
            let def = ctx.struct_def(*struct_id);
            Some(format!("{base}.{}", def.fields[*field_index as usize].name))
        }
        _ => None,
    }
}

/// `method` on a sum value that only some of its cases have: name the
/// case and show the `if let` that reaches it.
fn sum_no_method(
    ctx: &LowerCtx<'_>,
    recv: &HirExpr,
    method: &str,
    no_args: bool,
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    let HirTy::Sum(cases) = &recv.ty else {
        return None;
    };
    let case = cases
        .iter()
        .find(|c| matches!(pick_method(ctx, c, method, None), Ok(Some(_))))?;
    let (ty, case) = (ctx.ty_name(&recv.ty), ctx.ty_name(case));
    let place = place_text(ctx, recv).filter(|p| !p.starts_with('#'));
    let what = place
        .as_deref()
        .map_or("this value".to_owned(), |p| format!("`{p}`"));
    let an = |t: &str| {
        if t.starts_with(['a', 'e', 'i', 'o', 'u']) {
            "an"
        } else {
            "a"
        }
    };
    let e = ctx.error_expr(
        span,
        format!(
            "{what} is {} `{ty}`, and only {} `{case}` has the method `{method}`",
            an(&ty),
            an(&case)
        ),
    );
    let src = place.as_deref().unwrap_or("value");
    let bind = match src.rsplit_once('.') {
        Some((_, field)) => field.to_owned(),
        None => case.chars().next().unwrap_or('v').to_lowercase().collect(),
    };
    let call = if no_args { "()" } else { "(..)" };
    let fix = format!("`if let {bind}: {case} = {src} {{ {bind}.{method}{call} }}`");
    ctx.help_last(Some(if src.contains('.') {
        format!("`is` narrows local variables only, not fields; bind the field first: {fix}")
    } else {
        format!("check which case it holds first: {fix}")
    }));
    Some(e)
}

/// `inner?` (`inner_h` lowered). For Rust's `f()?` on a throwing call,
/// errors about the call come after the one saying `?` isn't for errors.
fn lower_try_q(
    ctx: &mut LowerCtx<'_>,
    inner: &Expr,
    inner_h: HirExpr,
    since: usize,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let call = matches!(
        inner.kind,
        ExprKind::Call { .. } | ExprKind::MethodCall { .. }
    );
    match ctx.src(inner.span).filter(|_| call) {
        Some(text) if try_q_errors(ctx, text, inner.span, since) => poison(span),
        _ => desugar_try_q(ctx, inner_h, span),
    }
}

fn try_q_errors(ctx: &LowerCtx<'_>, text: &str, at: rokc_diagnostic::Span, since: usize) -> bool {
    let mut diags = ctx.env.sink.diagnostics.borrow_mut();
    // The two errors a throwing call under `?` gets, by message.
    let hit = diags[since..].iter().any(|d| {
        d.is_error()
            && (d.message.starts_with("this call can throw")
                || d.message.starts_with("type annotations needed"))
    });
    if hit {
        let d = rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Type,
            at,
            "`?` only works on `T | none`; it does not pass errors on (spec 3.14 / 4.14)",
        )
        .with_help(format!("in a `throws` function, write `try {text}`"));
        diags.insert(since, d);
    }
    hit
}

/// Python's `s[0]` on a `str`: its characters.
fn str_index_help(ctx: &LowerCtx<'_>, s: &Expr, i: &Expr) {
    if let (Some(s), Some(i)) = (ctx.src(s.span), ctx.src(i.span)) {
        ctx.help_last(Some(format!(
            "a `str` is UTF-8 text, not an array of characters; take one with \
             `{s}.chars().nth({i})`"
        )));
    }
}

/// Python's `println("x =", x)` as one f-string, `println(f"x = {x}")`.
fn print_args_help(ctx: &LowerCtx<'_>, callee: &Expr, args: &[CallArg], since: usize) {
    let Some(help) = print_args_text(ctx, callee, args) else {
        return;
    };
    // It replaces the generic help showing the signature.
    let mut diags = ctx.env.sink.diagnostics.borrow_mut();
    if diags.len() > since {
        if let Some(d) = diags.last_mut().filter(|d| d.is_error()) {
            d.notes.retain(|n| !n.starts_with("help: "));
            d.notes.push(format!("help: {help}"));
        }
    }
}

fn print_args_text(ctx: &LowerCtx<'_>, callee: &Expr, args: &[CallArg]) -> Option<String> {
    let ExprKind::Path(p) = &callee.kind else {
        return None;
    };
    let [seg] = p.segments.as_slice() else {
        return None;
    };
    if args.len() < 2
        || !matches!(
            seg.name.as_str(),
            "print" | "println" | "eprint" | "eprintln"
        )
    {
        return None;
    }
    let mut parts = Vec::new();
    for a in args {
        let CallArg::Positional(e) = a else {
            return None;
        };
        let t = ctx.src(e.span)?;
        parts.push(match &e.kind {
            ExprKind::Literal(AstLit::Str(_)) if t.len() >= 2 && t.starts_with('"') => {
                t[1..t.len() - 1].replace('{', "{{").replace('}', "}}")
            }
            _ => format!("{{{t}}}"),
        });
    }
    Some(format!(
        "`{}` takes one string; put the values in an f-string: `{}(f\"{}\")`",
        seg.name,
        seg.name,
        parts.join(" ")
    ))
}

/// Python's `"a {} b".format(x)` as an f-string, `f"a {x} b"`.
fn format_help(ctx: &LowerCtx<'_>, args: &[CallArg], span: rokc_diagnostic::Span) -> String {
    let lit = ctx
        .src(span)
        .and_then(|t| t.rsplit_once(".format(").map(|(r, _)| r.trim()))
        .filter(|r| r.len() >= 2 && r.starts_with('"') && r.ends_with('"'));
    let vals: Vec<&str> = args
        .iter()
        .filter_map(|a| match a {
            CallArg::Positional(e) => ctx.src(e.span),
            _ => None,
        })
        .collect();
    let filled = lit.and_then(|l| {
        let parts: Vec<&str> = l[1..l.len() - 1].split("{}").collect();
        (parts.len() == vals.len() + 1).then(|| {
            let mut out = parts[0].to_owned();
            for (v, p) in vals.iter().zip(&parts[1..]) {
                out.push_str(&format!("{{{v}}}{p}"));
            }
            out
        })
    });
    match filled {
        Some(f) => format!("use an f-string: `f\"{f}\"`"),
        None => "use an f-string: `f\"..{x}..\"` puts `x` in the text".to_owned(),
    }
}

pub(super) fn method_call_on(
    ctx: &mut LowerCtx<'_>,
    recv_h: HirExpr,
    method: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let lower_args_only = |ctx: &mut LowerCtx<'_>| discard_args(ctx, args);
    let type_args = std::mem::take(&mut ctx.method_type_args);
    if recv_h.ty == HirTy::Unknown {
        lower_args_only(ctx);
        return recv_h;
    }
    if matches!(recv_h.ty, HirTy::Sum(_)) && SUM_METHODS.contains(&method) {
        return sum_structural_call(ctx, recv_h, method, args, None, span);
    }
    let recv_h = if matches!(recv_h.ty, HirTy::Sum(_)) {
        match super::reflect::method_call(ctx, recv_h, method, args) {
            Ok(r) => return r,
            Err(r) => r,
        }
    } else {
        recv_h
    };
    if matches!(recv_h.ty, HirTy::Sum(_)) && matches!(method, "min" | "max" | "clamp") {
        return sum_min_max(ctx, recv_h, method, args, span);
    }
    if let HirTy::AnyTrait(bounds) = recv_h.ty.clone() {
        return any_method_call(ctx, recv_h, &bounds, method, None, args, span);
    }
    let pick = match pick_method(ctx, &recv_h.ty, method, None) {
        Ok(p) => p,
        Err(msg) => {
            let recv = Some(recv_h.clone());
            if let Some(call) =
                call_overload(ctx, &recv_h.ty, method, None, recv, args, method, span)
            {
                return call;
            }
            if let Some(p) = pick_by_args(ctx, &recv_h.ty, method, args) {
                Some(p)
            } else {
                lower_args_only(ctx);
                return ctx.error_expr(span, msg);
            }
        }
    };
    let fn_id = match pick {
        Some(MethodPick::Concrete(id)) => Some(id),
        Some(MethodPick::Generic(_)) | None => None,
    };
    let Some(fn_id) = fn_id else {
        let gm = match pick {
            Some(MethodPick::Generic(gm)) => Some(gm),
            _ => None,
        };
        if let Some(gm) = gm {
            check_method_info_visible(ctx, &gm.info, method);
            if gm.def.params.first().map(|p| p.name.as_str()) != Some("self") {
                lower_args_only(ctx);
                return ctx.error_expr(
                    span,
                    format!(
                        "`{method}` is an associated function, not a method - call it as \
                         `{}::{method}(...)`",
                        ctx.ty_name(&recv_h.ty)
                    ),
                );
            }
            if is_addr_intrinsic(&gm.def, &gm.info.module) {
                // `MaybeUninit<T>::as_mut_ptr`: the receiver's place, as
                // a pointer to its type argument.
                lower_args_only(ctx);
                let pointee = match &recv_h.ty {
                    HirTy::Struct(sid) => ctx.struct_def(*sid).generic_args.first().cloned(),
                    _ => None,
                };
                let pointee = pointee.unwrap_or_else(|| recv_h.ty.clone());
                return addr_expr(ctx, recv_h, &gm.def, method, pointee, span);
            }
            match bind_explicit(ctx, &gm.def, Some(&gm.self_ty), &type_args, method) {
                Ok(bound) => ctx.explicit_generics = bound,
                Err(msg) => {
                    lower_args_only(ctx);
                    return ctx.error_expr(span, msg);
                }
            }
            return call_generic(
                ctx,
                &gm.def,
                Some(&gm.self_ty),
                Some(recv_h),
                args,
                method,
                span,
            );
        }
        let since = ctx.env.sink.diagnostics.borrow().len();
        lower_args_only(ctx);
        // A generic impl that would provide it but whose bound fails:
        // name the bound rather than "no method".
        let unmet = ctx
            .env
            .impls
            .generic_by(super::Pick::Method(method))
            .find_map(|gi| {
                ctx.generic_impl_match(gi, &recv_h.ty, 0)
                    .err()
                    .flatten()
                    .map(|b| (gi, b))
            });
        if let Some((gi, (bty, bound))) = unmet {
            return ctx.error_expr(
                span,
                format!(
                    "method `{method}` of the generic impl of `{}` does not apply to `{}`: \
                     its bound needs `{}: {}`, which does not hold",
                    gi.trait_key,
                    ctx.ty_name(&recv_h.ty),
                    ctx.ty_name(&bty),
                    ctx.trait_name(&bound)
                ),
            );
        }
        // Spec 3.5.2: a missing method is a hard error, not a silent
        // unit return - unless `Reflect` (every type's) answers it.
        let recv_h = match super::reflect::method_call(ctx, recv_h, method, args) {
            Ok(r) => return r,
            Err(r) => r,
        };
        let ty = ctx.ty_name(&recv_h.ty);
        if let HirTy::Param(p) = recv_h.ty {
            return super::bounds::no_method(ctx, p, method, span);
        }
        if let Some(e) = super::suggest::optional_method(ctx, &recv_h, method, args, span) {
            return e;
        }
        if let Some(e) = sum_no_method(ctx, &recv_h, method, args.is_empty(), span) {
            return e;
        }
        // No method, so no parameter types for its closures: drop the
        // "cannot infer" errors they gave.
        retain_since(ctx, since, |d| {
            d.tag != rokc_diagnostic::Tag::ClosureParamInfer
        });
        let e = ctx.error_expr(span, format!("no method `{method}` found for type `{ty}`"));
        let format = (method == "format" && recv_h.ty == HirTy::Prim(PrimTy::Str))
            .then(|| format_help(ctx, args, span));
        let help = format
            .or_else(|| super::reflect::inspect_help(ctx, &recv_h.ty, method))
            .unwrap_or_else(|| super::suggest::no_method_help(ctx, &recv_h, method, span));
        ctx.help_last(Some(help));
        return e;
    };
    check_method_visible(ctx, fn_id, method);
    if !type_args.is_empty() {
        lower_args_only(ctx);
        return ctx.error_expr(span, super::mono::arity_msg(method, 0, 0, type_args.len()));
    }
    let sig = ctx.sig_by_id(fn_id);
    if sig.param_names.first().map(String::as_str) != Some("self") {
        lower_args_only(ctx);
        return ctx.error_expr(
            span,
            format!(
                "`{method}` is an associated function, not a method - call it as \
                 `{}::{method}(...)`",
                ctx.ty_name(&recv_h.ty)
            ),
        );
    }
    call_sig(ctx, &sig, method, Some(recv_h), args, span)
}

/// Keep only the diagnostics after the first `since` that `keep` accepts.
fn retain_since(
    ctx: &LowerCtx<'_>,
    since: usize,
    keep: impl Fn(&rokc_diagnostic::RokDiagnostic) -> bool,
) {
    let mut diags = ctx.env.sink.diagnostics.borrow_mut();
    if since < diags.len() {
        let tail = diags.split_off(since);
        diags.extend(tail.into_iter().filter(keep));
    }
}

/// Stdlib 2.4.1: `a op= b` is `a.op_assign(b)` when `a`'s type implements
/// the `op=` trait; `None` otherwise, or for a primitive's `#[intrinsic]`
/// impl, and the caller writes `a = a op b`.
fn assign_trait_call(
    ctx: &mut LowerCtx<'_>,
    target: &Expr,
    op: &str,
    value: &Expr,
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    let entry = ctx.env.prec.lookup(op, Fixity::Infix)?;
    let method = entry.method_name.clone();
    let key = entry.trait_name.clone();
    ctx.env.trait_of(&key)?;
    let mark = ctx.probe_mark();
    let ty = lower_expected(ctx, target, HirTy::Unknown).ty;
    ctx.rollback(&mark);
    if ty == HirTy::Unknown || !ctx.has_impl(&ty, &TraitRef::plain(&key), 0) {
        return None;
    }
    let env = ctx.env;
    let intrinsic = env
        .methods_on(&ty, &method)
        .is_some_and(|ids| ids.iter().any(|id| env.intrinsic(*id).is_some()));
    if intrinsic {
        return None;
    }
    // `{ let #t = a; #t.op_assign(b); a = #t; }`: `ref self` updates the
    // temp, written back to any place (`xs[i] op= b`, `s.f op= b`).
    let t = tmp_path(ctx, "oa", span);
    let call = Expr {
        kind: ExprKind::MethodCall {
            receiver: Box::new(t.clone()),
            method,
            type_args: Vec::new(),
            args: vec![CallArg::Positional(value.clone())],
            trailing_closure: None,
        },
        span,
    };
    let stmts = vec![
        let_stmt(&t, target.clone()),
        Stmt {
            kind: StmtKind::Expr(call),
            span,
        },
        Stmt {
            kind: StmtKind::Assign(target.clone(), t),
            span,
        },
    ];
    let block = Expr {
        kind: ExprKind::Block(Block {
            stmts,
            trailing: None,
            span,
            label: None,
        }),
        span,
    };
    Some(lower_expected(ctx, &block, HirTy::Unit))
}

/// A trait implemented with several arguments (`Index<usize>` and
/// `Index<(usize, usize)>`): the one impl whose first parameter takes the
/// first argument (tried in rolled-back probes), or with no argument the
/// one returning the expected type (spec 3.1.3).
fn pick_by_args<'e>(
    ctx: &mut LowerCtx<'e>,
    ty: &HirTy,
    method: &str,
    args: &[CallArg],
) -> Option<MethodPick<'e>> {
    let arg = match args.first() {
        Some(CallArg::Positional(arg)) => Some(arg),
        None => None,
        Some(_) => return None,
    };
    let expected = ctx.current_expected().cloned();
    if arg.is_none() && expected.as_ref().is_none_or(|t| *t == HirTy::Unknown) {
        return None;
    }
    let env = ctx.env;
    // (trait, slot: the first parameter's type, else the return type).
    let mut cands: Vec<(String, HirTy, MethodPick<'e>)> = Vec::new();
    let ids = env.methods_on(ty, method);
    for id in ids.into_iter().flatten() {
        let tr = env.method_info(*id).and_then(|i| i.trait_name.clone())?;
        let sig = ctx.sig_by_id(*id);
        let slot = match arg {
            Some(_) => sig.param_tys.get(1).cloned(),
            None => Some(sig.ret_ty.clone()),
        };
        if let Some(slot) = slot {
            cands.push((tr, slot, MethodPick::Concrete(*id)));
        }
    }
    let gis: Vec<_> = ctx
        .generic_impls(ty, super::Pick::Method(method), 0)
        .collect();
    for (gi, subst) in gis {
        let gm = &gi.methods[method];
        let slot = match arg {
            Some(_) => gm.def.params.get(1).and_then(|p| p.ty.clone()),
            None => Some(super::throws::sig_ret_ast(&gm.def)),
        };
        let Some(p) = slot else {
            continue;
        };
        // A slot naming a parameter the receiver leaves open (`[T; N]`
        // of `impl<T, const N: usize> Tr<[T; N]> for T`) is no candidate.
        let mark = ctx.probe_mark();
        let mut rcx = LowerCtx::new(env, &subst, None, ctx.here());
        rcx.current_module.clone_from(&gi.module);
        let slot = rcx.resolve_ast_ty(&p);
        if !ctx.probe_clean(&mark) {
            ctx.rollback(&mark);
            continue;
        }
        cands.push((gi.trait_key.clone(), slot, MethodPick::Generic(gm)));
    }
    if cands.len() < 2 || cands.iter().any(|c| c.0 != cands[0].0) {
        return None;
    }
    let mut hits = Vec::new();
    for (_, slot, pick) in cands {
        let ok = match arg {
            Some(arg) => {
                let mark = ctx.probe_mark();
                let h = lower_expected(ctx, arg, slot.clone());
                let ok = ctx.probe_clean(&mark) && coercible(ctx, &slot, &h.ty);
                ctx.rollback(&mark);
                ok
            }
            None => expected.as_ref() == Some(&slot),
        };
        if ok {
            hits.push(pick);
        }
    }
    match hits[..] {
        [p] => Some(p),
        _ => None,
    }
}

/// `v.m(args)` on `any Tr`: only the bound traits' methods; the call
/// switches on the tag over every implementor in the program.
fn any_method_call(
    ctx: &mut LowerCtx<'_>,
    recv_h: HirExpr,
    bounds: &[TraitRef],
    method: &str,
    only: Option<&str>,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let mut seen: Vec<String> = Vec::new();
    let mut stack = bounds.to_vec();
    // The trait declaring `method`, and the bound that named it (with
    // its arguments / bindings; a supertrait reached indirectly has none).
    let mut provider: Option<(String, TraitRef)> = None;
    while let Some(t) = stack.pop() {
        let base = t.path.clone();
        if seen.contains(&base) {
            continue;
        }
        if let Some(info) = ctx.env.trait_of(&base) {
            if provider.is_none()
                && only.is_none_or(|k| k == base)
                && info.methods.iter().any(|m| m == method)
            {
                provider = Some((base.clone(), t.clone()));
            }
            stack.extend(info.supers.iter().map(|s| TraitRef::plain(s)));
        }
        seen.push(base);
    }
    let recv_ty = recv_h.ty.clone();
    let Some((provider, bound)) = provider else {
        if let Ok(r) = super::reflect::method_call(ctx, recv_h, method, args) {
            return r;
        }
        discard_args(ctx, args);
        return ctx.error_expr(
            span,
            format!(
                "no method `{method}` on `{}` - only methods of its trait bound(s) and their \
                 supertraits can be called on an `any` value; narrow it with a type pattern \
                 (`match v {{ x: T => ... }}`) to reach other methods",
                ctx.ty_name(&recv_ty)
            ),
        );
    };
    if method == "fmt" && is_fmt_trait(ctx, &provider) {
        return any_fmt(ctx, recv_h, &provider, args, span);
    }
    // Implementors share one signature only if it doesn't mention `Self`,
    // trait arguments the bound leaves open, or unbound associated types.
    let has_args = !bound.args.is_empty();
    let pinfo = ctx
        .env
        .trait_of(&provider)
        .expect("the provider is a trait");
    let needs = pinfo.needs.get(method).cloned().unwrap_or_default();
    let no_self = pinfo
        .decls
        .get(method)
        .is_some_and(|d| d.params.first().is_none_or(|p| p.name != "self"));
    let why = no_self.then(|| {
        "it takes no `self`, so there is no value to dispatch on (not object-safe)".to_owned()
    });
    let why = why.or_else(|| {
        needs.iter().find_map(|n| {
            if n == "Self" {
                Some(
                    "its signature mentions `Self`, which is a different type for each implementor"
                        .to_owned(),
                )
            } else if let Some(x) = n.strip_prefix("Self::") {
                (!bound.binds.iter().any(|(n, _)| n == x)).then(|| {
                    format!(
                        "its signature uses `Self::{x}`, which the bound leaves open - bind it \
                     (`any {provider}<{x} = ..>`)"
                    )
                })
            } else {
                (!has_args).then(|| {
                    format!(
                        "its signature uses type parameters of `{provider}` that the bound leaves \
                     open - name them (`any {provider}<..>`)"
                    )
                })
            }
        })
    });
    if let Some(why) = why {
        discard_args(ctx, args);
        return ctx.error_expr(
            span,
            format!(
                "cannot call `{method}` on `{}`: {why}; narrow the value with `is` or a type \
                 pattern first",
                ctx.ty_name(&recv_ty)
            ),
        );
    }
    // The signature, the same for every implementor (checked above):
    // a concrete impl's, else the trait declaration's.
    let concrete = ctx
        .env
        .impls
        .tys_of(&bounds[0].path)
        .find(|t| {
            bounds
                .iter()
                .all(|b| ctx.env.impls.refs_of(t, &b.path).any(|r| r.satisfies(b)))
        })
        .and_then(|t| match pick_method(ctx, t, method, Some(&provider)) {
            Ok(Some(MethodPick::Concrete(id))) => Some(ctx.sig_by_id(id)),
            _ => None,
        })
        .map(|sig| (sig.param_tys[1..].to_vec(), sig.ret_ty.clone()));
    let Some((params, ret)) = concrete.or_else(|| trait_decl_sig(ctx, &provider, &bound, method))
    else {
        discard_args(ctx, args);
        return ctx.error_expr(
            span,
            format!(
                "cannot call `{method}` on `{}`: its signature can't be fixed from the bound \
                 `{}`",
                ctx.ty_name(&recv_ty),
                ctx.trait_name(&bound)
            ),
        );
    };
    let mut param_tys = vec![recv_ty.clone()];
    param_tys.extend(params);
    let sig = FnSig {
        id: FunctionId(u32::MAX),
        ret_ty: ret.clone(),
        param_defaults: vec![None; param_tys.len()],
        param_names: (0..param_tys.len()).map(|i| format!("#{i}")).collect(),
        param_refs: Vec::new(),
        param_tys,
        variadic: false,
    };
    let (lowered, lets) = lower_args_for(ctx, &sig, method, Some(recv_h), args, span);
    let id = any_dispatcher(ctx, &recv_ty, &provider, method, &sig, span);
    with_lets(
        lets,
        HirExpr {
            kind: HirExprKind::Call {
                callee: id,
                args: lowered,
            },
            ty: ret,
            span,
        },
    )
}

fn is_fmt_trait(ctx: &LowerCtx<'_>, key: &str) -> bool {
    key == ctx.lang("display_trait") || key == ctx.lang("debug_trait")
}

/// `Display::fmt` / `Debug::fmt` on `any`: the dispatcher renders the text
/// (`build_dispatcher`), then it is written to `f`.
fn any_fmt(
    ctx: &mut LowerCtx<'_>,
    recv_h: HirExpr,
    provider: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let str_ty = HirTy::Prim(PrimTy::Str);
    let sig = FnSig {
        id: FunctionId(u32::MAX),
        ret_ty: str_ty.clone(),
        param_defaults: vec![None],
        param_names: vec!["#0".to_owned()],
        param_refs: Vec::new(),
        param_tys: vec![recv_h.ty.clone()],
        variadic: false,
    };
    let id = any_dispatcher(ctx, &recv_h.ty, provider, "fmt", &sig, span);
    let text = HirExpr {
        kind: HirExprKind::Call {
            callee: id,
            args: vec![recv_h],
        },
        ty: str_ty.clone(),
        span,
    };
    ctx.push_scope();
    let name = format!("#t{}", ctx.locals.len());
    let local = ctx.declare(Some(name.clone()), str_ty);
    let path = |names: &[&str]| Expr {
        kind: ExprKind::Path(AstPath {
            segments: names
                .iter()
                .map(|n| rokc_ast::PathSegment {
                    name: (*n).to_owned(),
                    generics: Vec::new(),
                })
                .collect(),
            span,
        }),
        span,
    };
    let mut call_args = args.to_vec();
    call_args.push(CallArg::Positional(path(&[&name])));
    let write = Expr {
        kind: ExprKind::Call {
            callee: Box::new(path(&["rok", "core", "fmt", "Formatter", "write_str"])),
            args: call_args,
            trailing_closure: None,
        },
        span,
    };
    let write = lower_expr(ctx, &write);
    ctx.pop_scope();
    with_lets(vec![HirStmt::Let { local, init: text }], write)
}

/// `method`'s signature from `provider`'s declaration with the trait's
/// parameters and associated types from `bound`; `None` for too many
/// arguments.
fn trait_decl_sig(
    ctx: &LowerCtx<'_>,
    provider: &str,
    bound: &TraitRef,
    method: &str,
) -> Option<(Vec<HirTy>, HirTy)> {
    let info = ctx.env.trait_of(provider)?;
    let def = info.decls.get(method)?;
    if bound.args.len() > info.params.len() {
        return None;
    }
    let subst: HashMap<String, HirTy> = info
        .params
        .iter()
        .cloned()
        .zip(bound.args.iter().cloned())
        .chain(bound.binds.iter().cloned())
        .collect();
    let mut rc = LowerCtx::new(ctx.env, &subst, None, ctx.here());
    rc.current_module.clone_from(&info.module);
    let resolve = |t: &Ty| {
        let mut t = t.clone();
        strip_self_assoc(&mut t);
        rc.resolve_ast_ty(&t)
    };
    let params = def
        .params
        .iter()
        .filter(|p| p.name != "self")
        .map(|p| p.ty.as_ref().map_or(HirTy::Unknown, resolve))
        .collect();
    Some((params, resolve(&sig_ret_ast(def))))
}

/// `Self::Item` → `Item` (the associated type, bound by name).
fn strip_self_assoc(ty: &mut Ty) {
    match &mut ty.kind {
        AstTyKind::Path(p) => {
            if p.segments.len() >= 2 && p.segments[0].name == "Self" {
                p.segments.remove(0);
            }
            for s in &mut p.segments {
                s.generics.iter_mut().for_each(strip_self_assoc);
            }
        }
        AstTyKind::Sum(parts) | AstTyKind::Tuple(parts) => {
            parts.iter_mut().for_each(strip_self_assoc);
        }
        AstTyKind::Fn(args, ret, _) => {
            args.iter_mut().for_each(strip_self_assoc);
            strip_self_assoc(ret);
        }
        AstTyKind::Throws { ok, err } => {
            strip_self_assoc(ok);
            strip_self_assoc(err);
        }
        AstTyKind::Array(elem, _) | AstTyKind::Nest(elem) | AstTyKind::Binding(_, elem) => {
            strip_self_assoc(elem);
        }
        AstTyKind::QPath(q) => strip_self_assoc(&mut q.self_ty),
        _ => {}
    }
}

/// One dispatcher per (`any` type, method); bodies are built by
/// `build_any_dispatchers` once all stored types are known (closed world).
#[derive(Clone)]
pub(super) struct AnyDispatch {
    pub(super) id: FunctionId,
    pub(super) recv_ty: HirTy,
    /// The trait declaring `method` (each implementor's method of it,
    /// whatever other traits share the name).
    pub(super) trait_key: String,
    pub(super) method: String,
    pub(super) params: Vec<HirTy>,
    pub(super) ret: HirTy,
    pub(super) span: rokc_diagnostic::Span,
}

pub(super) fn any_dispatcher(
    ctx: &LowerCtx<'_>,
    recv_ty: &HirTy,
    trait_key: &str,
    method: &str,
    sig: &FnSig,
    span: rokc_diagnostic::Span,
) -> FunctionId {
    let mut all = ctx.env.sink.any_dispatch.borrow_mut();
    let found = all
        .iter()
        .find(|d| d.recv_ty == *recv_ty && d.method == method && d.trait_key == trait_key)
        .map(|d| d.id);
    super::incr::log(
        || format!("d|{recv_ty:?}|{trait_key}|{method}"),
        || {
            super::incr::Lookup::AnyDispatch(
                recv_ty.clone(),
                trait_key.to_owned(),
                method.to_owned(),
                found,
            )
        },
    );
    if let Some(id) = found {
        return id;
    }
    let id = FunctionId(ctx.env.sink.next_id.get());
    ctx.env.sink.next_id.set(id.0 + 1);
    all.push(AnyDispatch {
        id,
        recv_ty: recv_ty.clone(),
        trait_key: trait_key.to_owned(),
        method: method.to_owned(),
        params: sig.param_tys[1..].to_vec(),
        ret: sig.ret_ty.clone(),
        span,
    });
    id
}

/// `any` across packages (spec 904). Dispatchers are closed-world, so each
/// gets a stable symbol `rok.any.<hash>` (weak in a prebuilt library,
/// strong in its user), and a library ships its stored types and
/// dispatchers as rok source `seeds` in its ROKHIR for the user to
/// compile into its own. Returns `(seeds, dispatcher exports)`.
// Types are named as `ty_name` prints them: closures are skipped, and a
// type not visible from the seed's module fails the user's build.
#[allow(clippy::type_complexity)] // two lists, each read once
pub(super) fn any_link(
    env: &Env<'_>,
    lib: Option<&str>,
) -> (Vec<(String, String)>, Vec<(FunctionId, String)>) {
    let empty = HashMap::new();
    let c = LowerCtx::new(env, &empty, None, rokc_diagnostic::Span::DUMMY);
    let dispatch = env.sink.any_dispatch.borrow().clone();
    let names = |tys: &[HirTy]| tys.iter().map(|t| c.ty_name(t)).collect::<Vec<_>>();
    let exports = dispatch
        .iter()
        .map(|d| {
            let key = format!(
                "{}|{}|{}|{}|{}",
                c.ty_name(&d.recv_ty),
                d.trait_key,
                d.method,
                names(&d.params).join(","),
                c.ty_name(&d.ret)
            );
            (
                d.id,
                format!("rok.any.{:016x}", rokc_core::hash::fnv(key.as_bytes())),
            )
        })
        .collect();
    let Some(lib) = lib else {
        return (Vec::new(), exports);
    };
    let under = format!("{lib}::");
    // The module of the first `lib` item a type names (else the root).
    let home = |text: &str| -> String {
        text.split(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == ':'))
            .find(|w| w.starts_with(&under))
            .map_or_else(|| lib.to_owned(), module_of)
    };
    let nameable = |s: &str| {
        !["closure", "{unknown}", "Self", "#", "GcBuf"]
            .iter()
            .any(|b| s.contains(b))
    };
    let mut seeds = Vec::new();
    let stored: Vec<HirTy> = env
        .sink
        .any_tags
        .borrow()
        .iter()
        .map(|(t, _)| t.clone())
        .collect();
    for t in stored {
        let name = c.ty_name(&t);
        if matches!(t, HirTy::AnyTrait(_)) || !nameable(&name) {
            continue;
        }
        let n = seeds.len();
        seeds.push((
            home(&name),
            format!(
                "#[export_name(\"__rok_seed_{lib}_{n}\")]\nfn __rok_seed_{n}(x: {name}) -> any {{ return x; }}\n"
            ),
        ));
    }
    for d in &dispatch {
        let recv = c.ty_name(&d.recv_ty);
        let params = names(&d.params);
        if !nameable(&recv) || !params.iter().all(|p| nameable(p)) {
            continue;
        }
        let n = seeds.len();
        let decl: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(i, p)| format!(", a{i}: {p}"))
            .collect();
        let args: Vec<String> = (0..params.len()).map(|i| format!("a{i}")).collect();
        let (throws, try_) = if matches!(d.ret, HirTy::Throws { .. }) {
            (" throws", "try ")
        } else {
            ("", "")
        };
        // A `fmt` dispatcher renders text (`any_fmt`); an f-string makes it.
        let body = if d.method == "fmt" && is_fmt_trait(&c, &d.trait_key) {
            let q = if d.trait_key == c.lang("debug_trait") {
                ":?"
            } else {
                ""
            };
            format!("let _ = f\"{{x{q}}}\";")
        } else {
            format!("{try_}x.{}({});", d.method, args.join(", "))
        };
        seeds.push((
            home(&recv),
            format!(
                "#[export_name(\"__rok_seed_{lib}_{n}\")]\nfn __rok_seed_{n}(x: {recv}{}){throws} {{ {body} }}\n",
                decl.concat(),
            ),
        ));
    }
    (seeds, exports)
}

/// Every type the program stores into an `any` slot that source can
/// name (`HirModule::any_types`), as `ty_name` prints it.
pub(super) fn any_types(env: &Env<'_>) -> Vec<String> {
    let empty = HashMap::new();
    let c = LowerCtx::new(env, &empty, None, rokc_diagnostic::Span::DUMMY);
    let stored: Vec<HirTy> = env
        .sink
        .any_tags
        .borrow()
        .iter()
        .map(|(t, _)| t.clone())
        .collect();
    stored
        .iter()
        .filter(|t| !matches!(t, HirTy::AnyTrait(_)))
        .map(|t| c.ty_name(t))
        .filter(|s| {
            !["closure", "{unknown}", "Self", "#", "GcBuf"]
                .iter()
                .any(|b| s.contains(b))
        })
        .collect()
}

/// Build every dispatcher body. Bodies can instantiate code that stores
/// new types or needs new dispatchers, so repeat to a fixpoint.
pub(super) fn build_any_dispatchers(env: &Env<'_>) {
    loop {
        let (n_tags, todo) = (
            env.sink.any_tags.borrow().len(),
            env.sink.any_dispatch.borrow().clone(),
        );
        let fns: Vec<HirFunction> = todo.iter().map(|d| build_dispatcher(env, d)).collect();
        if env.sink.any_tags.borrow().len() == n_tags
            && env.sink.any_dispatch.borrow().len() == todo.len()
        {
            env.sink.extras.borrow_mut().extend(fns);
            return;
        }
    }
}

fn build_dispatcher(env: &Env<'_>, d: &AnyDispatch) -> HirFunction {
    let span = d.span;
    let empty = HashMap::new();
    let mut c = LowerCtx::new(env, &empty, None, span);
    let HirTy::AnyTrait(bounds) = &d.recv_ty else {
        unreachable!("dispatcher on a non-`any` type")
    };
    let mut params = vec![HirParam {
        name: "#recv".to_owned(),
        local: c.declare(Some("#recv".to_owned()), d.recv_ty.clone()),
        ty: d.recv_ty.clone(),
        by_ref: false,
    }];
    for (i, t) in d.params.iter().enumerate() {
        let name = format!("#a{i}");
        params.push(HirParam {
            name: name.clone(),
            local: c.declare(Some(name), t.clone()),
            ty: t.clone(),
            by_ref: false,
        });
    }
    let args: Vec<CallArg> = (0..d.params.len())
        .map(|i| {
            CallArg::Positional(Expr {
                kind: ExprKind::Path(AstPath {
                    segments: vec![rokc_ast::PathSegment {
                        name: format!("#a{i}"),
                        generics: Vec::new(),
                    }],
                    span,
                }),
                span,
            })
        })
        .collect();
    let stored: Vec<HirTy> = env
        .sink
        .any_tags
        .borrow()
        .iter()
        .map(|(t, _)| t.clone())
        .collect();
    let mut arms = Vec::new();
    for t in stored {
        if matches!(t, HirTy::AnyTrait(_)) || !bounds.iter().all(|b| c.implements_trait(&t, b, 0)) {
            continue;
        }
        c.push_scope();
        let x = c.declare(Some("#x".to_owned()), t.clone());
        let recv = hir_local(x, &t, span);
        let call = if d.method == "fmt" && is_fmt_trait(&c, &d.trait_key) {
            // The text, `f"{x}"` / `f"{x:?}"` (`any_fmt`).
            let debug = d.trait_key == c.lang("debug_trait");
            let hole = rokc_ast::FStringPart::Hole {
                expr: Expr {
                    kind: ExprKind::Path(AstPath {
                        segments: vec![rokc_ast::PathSegment {
                            name: "#x".to_owned(),
                            generics: Vec::new(),
                        }],
                        span,
                    }),
                    span,
                },
                spec: if debug {
                    vec![rokc_ast::SpecPiece::Text("?".to_owned())]
                } else {
                    Vec::new()
                },
            };
            super::fstring::lower_fstring(&mut c, &[hole], span)
        } else if d.trait_key == super::reflect::REFLECT {
            super::reflect::dynamic(&mut c, recv, &d.method)
        } else {
            call_impl_method(
                &mut c,
                &t,
                &d.method,
                Some(&d.trait_key),
                Some(recv),
                &args,
                &d.method,
                span,
            )
            .unwrap_or_else(|| poison(span))
        };
        let body = coerce(&mut c, &d.ret, call, &|| {
            format!("`{}` of an `any` value", d.method)
        });
        c.pop_scope();
        arms.push(HirMatchArm {
            pat: HirPattern::SumVariant {
                sum_ty: d.recv_ty.clone(),
                variant_index: c.any_tag(&t),
                binder: Some(x),
                payload_ty: Some(t),
            },
            guard: None,
            body,
        });
    }
    let m = HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(hir_local(params[0].local, &d.recv_ty, span)),
            arms,
        },
        ty: d.ret.clone(),
        span,
    };
    let body = if d.ret == HirTy::Unit {
        HirBlock {
            stmts: vec![HirStmt::Expr(m)],
            trailing: None,
            label: None,
        }
    } else {
        HirBlock {
            stmts: Vec::new(),
            trailing: Some(m),
            label: None,
        }
    };
    HirFunction {
        id: d.id,
        def_id: None,
        name: format!("__any_{}_{}", d.method, d.id.0),
        params,
        locals: c.into_locals(),
        ret_ty: d.ret.clone(),
        body: Some(body),
        is_async: false,
        is_generator: false,
        span,
        facts: rokc_ir_common::FnFacts {
            role: rokc_ir_common::FnRole::AnyDispatcher,
            ..rokc_ir_common::FnFacts::default()
        },
    }
}

/// Spec 5.6 for methods: an inherent method without `pub` is private to
/// the module of its impl block.
pub(super) fn check_method_visible(ctx: &LowerCtx<'_>, fn_id: FunctionId, method: &str) {
    if let Some(info) = ctx.env.method_info(fn_id) {
        check_method_info_visible(ctx, info, method);
    }
}

/// Generic-impl method `method` for a receiver of type `ty`, looked up
/// by the receiver's generic base (`Box__i32` → `Box`).
pub(super) fn generic_method_for<'e>(
    ctx: &LowerCtx<'e>,
    ty: &HirTy,
    method: &str,
) -> &'e [GenericMethod] {
    let (name, generic_args) = match ty {
        HirTy::Struct(id) => {
            let d = ctx.struct_def(*id);
            (d.name, d.generic_args)
        }
        HirTy::Enum(id) => {
            let d = ctx.enum_def(*id);
            (d.name, d.generic_args)
        }
        HirTy::Prim(p) => (p.name().to_owned(), Vec::new()),
        // `impl<T> GcBuf<T>` (stdlib 2.9.12).
        HirTy::GcBuf(_) => (super::resolve::GC_BUF.to_owned(), Vec::new()),
        _ => return &[],
    };
    let base = if generic_args.is_empty() {
        name
    } else {
        enum_base_name(&name).to_owned()
    };
    ctx.env
        .generic_methods(&base, method)
        .map_or(&[], Vec::as_slice)
}

/// A method candidate for a receiver type: a concrete function or a
/// generic template.
#[derive(Clone, Copy)]
pub(super) enum MethodPick<'e> {
    Concrete(FunctionId),
    Generic(&'e GenericMethod),
}

/// Method `method` of `ty` (spec 3.5.2 / 4.9.6), from `trait_key` if given:
/// inherent first, then trait methods, where a trait defined or `use`d in
/// this package beats one seen only through the implicit `rok::core::**`;
/// other ties are ambiguous.
pub(super) fn pick_method<'e>(
    ctx: &LowerCtx<'e>,
    ty: &HirTy,
    method: &str,
    trait_key: Option<&str>,
) -> Result<Option<MethodPick<'e>>, String> {
    let env = ctx.env;
    let mut cands: Vec<(Option<&'e str>, MethodPick<'e>)> = Vec::new();
    // A type parameter's bounds declare it (spec 2.10.9); one candidate
    // per bound trait.
    if let HirTy::Param(p) = ty {
        for gm in super::bounds::bound_methods(ctx, *p, method, trait_key) {
            cands.push((gm.info.trait_name.as_deref(), MethodPick::Generic(gm)));
        }
    }
    if cands.is_empty() {
        if let Some(ids) = env.methods_on(ty, method) {
            for id in ids {
                let tn = env.method_info(*id).and_then(|i| i.trait_name.as_deref());
                cands.push((tn, MethodPick::Concrete(*id)));
            }
        }
        for gm in generic_method_for(ctx, ty, method) {
            cands.push((gm.info.trait_name.as_deref(), MethodPick::Generic(gm)));
        }
        for (gi, _) in ctx.generic_impls(ty, super::Pick::Method(method), 0) {
            // An empty key: a generic inherent impl (`impl<..> [T; N]`).
            let key = Some(gi.trait_key.as_str()).filter(|k| !k.is_empty());
            cands.push((key, MethodPick::Generic(&gi.methods[method])));
        }
    }
    if let Some(k) = trait_key {
        cands.retain(|(t, _)| *t == Some(k));
    }
    if let Some((_, p)) = cands.iter().find(|(t, _)| t.is_none()) {
        return Ok(Some(*p));
    }
    if cands.len() > 1 {
        let own: Vec<_> = cands
            .iter()
            .filter(|(t, _)| t.is_some_and(|t| !core_only_trait(ctx, t)))
            .copied()
            .collect();
        if own.len() == 1 {
            return Ok(Some(own[0].1));
        }
        let mut traits: Vec<String> = cands
            .iter()
            .map(|(t, _)| format!("`{}`", t.unwrap_or_default()))
            .collect();
        traits.sort();
        traits.dedup();
        let leaf = |t: &str| leaf_name(t).to_owned();
        return Err(format!(
            "method `{method}` on `{}` is ambiguous: traits {} all provide it - call it as \
             `Trait::{method}(x)` (e.g. `{}::{method}(x)`) (spec 4.9.6)",
            ctx.ty_name(ty),
            traits.join(", "),
            leaf(cands[0].0.unwrap_or_default())
        ));
    }
    Ok(cands.first().map(|(_, p)| *p))
}

/// Is trait `key` from another package and not `use`d here (seen only
/// through the implicit core glob, or a user trait seen from core)?
fn core_only_trait(ctx: &LowerCtx<'_>, key: &str) -> bool {
    let pkg = super::resolve::package_of;
    if pkg(&module_of(key)) == pkg(&ctx.current_module) {
        return false;
    }
    let named = |e: &crate::uses::UseEntry| match e {
        crate::uses::UseEntry::Single { full, .. } => full == key,
        crate::uses::UseEntry::Glob { prefix } => module_of(key) == *prefix,
        crate::uses::UseEntry::GlobDeep { prefix } => key.starts_with(&format!("{prefix}::")),
    };
    !ctx.local_uses
        .iter()
        .flatten()
        .chain(ctx.env.uses_of(&ctx.current_module).into_iter().flatten())
        .any(named)
}

/// `method` from the generic impl covering `ty` (`impl<T> Tr for Box<T>`,
/// blankets); two covering impls are an error.
pub(super) fn generic_trait_method_for<'e>(
    ctx: &LowerCtx<'e>,
    ty: &HirTy,
    method: &str,
) -> Result<Option<&'e GenericMethod>, String> {
    let hits: Vec<&'e Impl> = ctx
        .generic_impls(ty, super::Pick::Method(method), 0)
        .map(|(gi, _)| gi)
        .collect();
    match hits.as_slice() {
        [] => Ok(None),
        [gi] => Ok(gi.methods.get(method)),
        _ => Err(format!(
            "method `{method}` on `{}` is ambiguous: generic impls of {} all cover it",
            ctx.ty_name(ty),
            hits.iter()
                .map(|gi| format!("`{}`", gi.trait_key))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub(super) fn check_method_info_visible(ctx: &LowerCtx<'_>, info: &MethodInfo, method: &str) {
    if !ctx.is_visible(&info.module, info.vis) {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Module,
            format!(
                "method `{method}` is private to module `{}` and cannot be called from `{}` \
                 (spec 5.6 - add `pub` to expose it)",
                module_display(&info.module),
                module_display(&ctx.current_module)
            ),
        );
    }
}

/// Call through a function-pointer or closure value.
pub(super) fn call_value(
    ctx: &mut LowerCtx<'_>,
    callee_h: HirExpr,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let (arg_tys, ret, env) = match callee_h.ty.clone() {
        HirTy::Fn(a, r, b) => {
            if b.unsafe_fn {
                check_unsafe_call(ctx, "(unsafe fn value)");
            }
            (a, *r, None)
        }
        HirTy::Closure { args, ret, env, .. } => (args, *ret, Some(*env)),
        HirTy::Unknown => {
            discard_args(ctx, args);
            return callee_h;
        }
        other => {
            return ctx.error_expr(
                span,
                format!("a value of type `{}` is not callable", ctx.ty_name(&other)),
            );
        }
    };
    if args.iter().any(|a| !matches!(a, CallArg::Positional(_))) {
        return ctx.error_expr(
            span,
            "named / spread arguments need a statically known callee (spec 4.2.4)".to_owned(),
        );
    }
    let sig = FnSig {
        id: FunctionId(u32::MAX),
        ret_ty: ret.clone(),
        param_defaults: vec![None; arg_tys.len()],
        param_tys: arg_tys.clone(),
        param_names: (0..arg_tys.len()).map(|i| format!("#{}", i + 1)).collect(),
        param_refs: Vec::new(),
        variadic: false,
    };
    let (lowered, lets) = lower_args_for(ctx, &sig, "the function value", None, args, span);
    let kind = match env {
        None => HirExprKind::CallIndirect {
            callee: Box::new(callee_h),
            args: lowered,
            ret_ty: ret.clone(),
        },
        Some(env_ty) => HirExprKind::CallClosure {
            closure: Box::new(callee_h),
            args: lowered,
            arg_tys,
            ret_ty: ret.clone(),
            env_ty,
        },
    };
    with_lets(
        lets,
        HirExpr {
            kind,
            ty: ret,
            span,
        },
    )
}

/// Spec 4.15.3: an `unsafe fn` (or C function) is callable only in an
/// `unsafe fn` body or `unsafe { }` block.
fn check_unsafe_call(ctx: &LowerCtx<'_>, callee: &str) {
    if ctx.unsafe_depth == 0 {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "call to unsafe function `{}` outside an `unsafe` block or `unsafe fn` \
                 (spec 4.15; C functions are implicitly unsafe)",
                leaf_name(callee)
            ),
        );
    }
}

pub(super) fn call_generic(
    ctx: &mut LowerCtx<'_>,
    def: &FnDef,
    self_ty: Option<&Ty>,
    receiver: Option<HirExpr>,
    args: &[CallArg],
    callee: &str,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    call_generic_on(ctx, def, self_ty, None, receiver, args, callee, span)
}

/// `call_generic` with `Self` known up front (`self_hint`): a generic
/// impl's static method binds the impl's parameters from it.
#[allow(clippy::too_many_arguments)]
pub(super) fn call_generic_on(
    ctx: &mut LowerCtx<'_>,
    def: &FnDef,
    self_ty: Option<&Ty>,
    self_hint: Option<&HirTy>,
    receiver: Option<HirExpr>,
    args: &[CallArg],
    callee: &str,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if ctx.env.duplicated(def.span) {
        discard_args(ctx, args);
        return poison(span);
    }
    if args
        .iter()
        .all(|a| matches!(a, CallArg::Positional(_) | CallArg::Ref(_)))
    {
        return call_generic_positional(ctx, def, self_ty, self_hint, receiver, args, callee, span);
    }
    let mut lets = Vec::new();
    let offset = usize::from(receiver.is_some());
    let Some(args) = generic_positional_args(ctx, def, offset, args, callee, &mut lets) else {
        return poison(span);
    };
    let call = call_generic_positional(ctx, def, self_ty, self_hint, receiver, &args, callee, span);
    if lets.is_empty() {
        return call;
    }
    HirExpr {
        ty: call.ty.clone(),
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: lets,
            trailing: Some(call),
            label: None,
        })),
        span,
    }
}

/// A generic call's named and spread arguments as positional ones (spec
/// 4.2): skipped parameters take defaults, a spread tuple goes through a
/// temp in `lets`, a trailing closure fills the last parameter (4.8.3);
/// `offset` receiver slots first. `None` after an error.
fn generic_positional_args(
    ctx: &mut LowerCtx<'_>,
    def: &FnDef,
    offset: usize,
    args: &[CallArg],
    callee: &str,
    lets: &mut Vec<HirStmt>,
) -> Option<Vec<CallArg>> {
    let params = def.params.get(offset..).unwrap_or_default();
    let n = params.len();
    let mut slots: Vec<Option<CallArg>> = vec![None; n];
    let (mut next, mut extra) = (0usize, 0usize);
    // Spec 4.2.6: a variadic `...T` takes every argument past its slot
    // (spread tuple fields too); `call_generic_positional` wraps them.
    let variadic = params.last().is_some_and(|p| p.variadic);
    let mut rest: Vec<CallArg> = Vec::new();
    let mut ok = true;
    let mut dicts: Vec<(String, HirTy)> = Vec::new();
    for (k, a) in args.iter().enumerate() {
        match a {
            CallArg::Positional(e)
                if !variadic && n > next && slots[n - 1].is_none() && is_trailing_closure(e) =>
            {
                slots[n - 1] = Some(a.clone());
                next = n;
            }
            CallArg::Positional(_) | CallArg::Ref(_) => {
                match slots.get_mut(next) {
                    Some(s) => *s = Some(a.clone()),
                    None if variadic => rest.push(a.clone()),
                    None => extra += 1,
                }
                next += 1;
            }
            CallArg::Named { name, value } => match params.iter().position(|p| p.name == *name) {
                Some(i) if slots[i].is_none() => {
                    slots[i] = Some(CallArg::Positional(value.clone()));
                }
                Some(_) => {
                    ok = false;
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!("argument `{name}` provided more than once (spec 4.2.4)"),
                    );
                    ctx.help_last(Some(twice_help(name)));
                }
                None => {
                    ok = false;
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!("`{callee}` has no parameter named `{name}` (spec 4.2.4)"),
                    );
                }
            },
            // In the variadic slot: `lower_variadic` expands it.
            CallArg::Spread(_) if variadic && next + 1 >= n => {
                match slots.get_mut(next) {
                    Some(s) => *s = Some(a.clone()),
                    None => rest.push(a.clone()),
                }
                next += 1;
            }
            CallArg::Spread(e) => {
                let h = lower_expected(ctx, e, HirTy::Unknown);
                let span = e.span;
                // A tuple's fields, an array's elements, or a `List` /
                // `Slice` up to the next argument (checked at run time).
                let count = match &h.ty {
                    HirTy::Tuple(parts) => parts.len(),
                    HirTy::Array(_, len) => *len as usize,
                    t if spreads_at_run_time(ctx, t) => {
                        let later = args[k + 1..]
                            .iter()
                            .filter(|a| !matches!(a, CallArg::Named { .. }))
                            .count();
                        let end = if variadic {
                            n - 1
                        } else {
                            n.saturating_sub(later)
                        };
                        end.saturating_sub(next)
                    }
                    HirTy::Unknown => return None,
                    t if is_str_dict(ctx, t) => {
                        let name = format!("#spread{}", ctx.locals.len());
                        let vty = dict_value_ty(ctx, &h.ty);
                        let local = ctx.declare(Some(name.clone()), h.ty.clone());
                        lets.push(HirStmt::Let { local, init: h });
                        dicts.push((name, vty));
                        continue;
                    }
                    other => {
                        ctx.emit_error(
                            rokc_diagnostic::ErrorKind::Type,
                            format!(
                                "spread `f(..x)` needs a tuple, a `List`, a `Slice`, an array \
                                 or a `Dict<str, V>`, found `{}` (spec 4.2.6)",
                                ctx.ty_name(other)
                            ),
                        );
                        return None;
                    }
                };
                let tuple = matches!(h.ty, HirTy::Tuple(_));
                let run_time = spreads_at_run_time(ctx, &h.ty);
                let name = format!("#spread{}", ctx.locals.len());
                let local = ctx.declare(Some(name.clone()), h.ty.clone());
                lets.push(HirStmt::Let { local, init: h });
                if run_time {
                    let len = Expr {
                        kind: ExprKind::MethodCall {
                            receiver: Box::new(super::throws::path_expr(&name, span)),
                            method: "len".to_owned(),
                            type_args: Vec::new(),
                            args: Vec::new(),
                            trailing_closure: None,
                        },
                        span,
                    };
                    let want = Expr {
                        kind: ExprKind::Literal(AstLit::Int(format!("{count}usize"))),
                        span,
                    };
                    let check = index_fn("spread_len", vec![len, want], span);
                    lets.push(HirStmt::Expr(lower_expected(ctx, &check, HirTy::Unit)));
                }
                for k in 0..count {
                    let field = if tuple {
                        Expr {
                            kind: ExprKind::TupleField(
                                Box::new(super::throws::path_expr(&name, span)),
                                u32::try_from(k).expect("tuple field overflow"),
                            ),
                            span,
                        }
                    } else {
                        spread_item(&name, k, span)
                    };
                    match slots.get_mut(next) {
                        Some(s) => *s = Some(CallArg::Positional(field)),
                        None if variadic => rest.push(CallArg::Positional(field)),
                        None => extra += 1,
                    }
                    next += 1;
                }
            }
        }
    }
    if extra > 0 {
        arity_error(ctx, callee, n, n + extra, Some(sig_of_def(callee, def)));
        return None;
    }
    let empty = params
        .iter()
        .enumerate()
        .filter(|(i, p)| slots[*i].is_none() && !p.variadic)
        .map(|(i, p)| {
            let d = p
                .default
                .clone()
                .map(|d| in_module(&module_of(&def.name), d));
            (
                i,
                p.name.clone(),
                d,
                p.ty.as_ref().map_or(HirTy::Unknown, plain_param_ty),
            )
        })
        .collect();
    let span = args
        .first()
        .map_or(rokc_diagnostic::Span::DUMMY, |a| match a {
            CallArg::Positional(e) | CallArg::Ref(e) | CallArg::Spread(e) => e.span,
            CallArg::Named { value, .. } => value.span,
        });
    for (i, e) in dict_spread(ctx, &dicts, empty, lets, span) {
        slots[i] = Some(CallArg::Positional(e));
    }
    let mut out = Vec::with_capacity(n);
    for (slot, p) in slots.into_iter().zip(params) {
        match (slot, &p.default) {
            (Some(a), _) => out.push(a),
            (None, _) if p.variadic => {}
            (None, Some(d)) => {
                out.push(CallArg::Positional(in_module(
                    &module_of(&def.name),
                    d.clone(),
                )));
            }
            (None, None) => {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!("missing argument `{}` in call to `{callee}`", p.name),
                );
                let sig = sig_of_def(callee, def);
                ctx.help_last(Some(format!(
                    "it is declared `{sig}`; pass `{}` too",
                    p.name
                )));
                ok = false;
            }
        }
    }
    out.extend(rest);
    ok.then_some(out)
}

/// `call_generic_on` with positional (and `ref`) arguments only.
#[allow(clippy::too_many_arguments)]
fn call_generic_positional(
    ctx: &mut LowerCtx<'_>,
    def: &FnDef,
    self_ty: Option<&Ty>,
    self_hint: Option<&HirTy>,
    receiver: Option<HirExpr>,
    args: &[CallArg],
    callee: &str,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if def.is_unsafe {
        check_unsafe_call(ctx, callee);
    }
    // Spec 4.2.5: a variadic call's trailing arguments become one
    // `Slice<T>` once the others are in (`lower_variadic`).
    let (args, tail) = match def.params.last().filter(|p| p.variadic) {
        Some(_) => {
            let k = (def.params.len() - 1).saturating_sub(usize::from(receiver.is_some()));
            args.split_at_checked(k)
                .map_or((args, None), |(a, t)| (a, Some(t)))
        }
        None => (args, None),
    };
    // A method's `Self` is its receiver's type.
    let recv_ty = receiver
        .as_ref()
        .filter(|_| self_hint.is_none() && self_ty.is_some())
        .map(|r| r.ty.clone());
    let self_hint = self_hint.or(recv_ty.as_ref());
    // A bound template's signature names `Self` (a type parameter).
    let hint_self = self_hint.filter(|_| ctx.env.bound_trait(&def.name).is_some());
    // Explicit `::<..>` arguments apply to this call only, not to the
    // generic calls inside its arguments.
    let explicit = std::mem::take(&mut ctx.explicit_generics);
    let mut subst: HashMap<String, HirTy> = explicit.clone();
    if let (Some(t), Some(hint)) = (self_ty, self_hint) {
        bind_self(ctx, def, t, hint, &mut subst);
    }
    let mut lowered: Vec<HirExpr> = Vec::with_capacity(args.len() + 1);
    let mut places: Vec<Option<HirExpr>> = vec![None; def.params.len()];
    let has_receiver = receiver.is_some();
    let explicit_recv = ctx.explicit_recv.take();
    if let Some(r) = receiver {
        let by_ref = def.params.first().is_some_and(|p| p.by_ref);
        check_path_receiver(ctx, explicit_recv, by_ref, &r, "self", callee);
        bind_from_arg(ctx, def, self_ty, 0, &r.ty, &mut subst);
        if let (Some(p), true) = (
            places.first_mut(),
            def.params.first().is_some_and(|p| p.by_ref),
        ) {
            *p = Some(r.clone());
        }
        lowered.push(r);
    }
    let mut reads: Vec<(String, Vec<String>)> = Vec::new();
    let mut deferred: Vec<(usize, &Expr)> = Vec::new();
    for a in args {
        let (CallArg::Positional(e) | CallArg::Ref(e)) = a else {
            unreachable!("named / spread arguments are made positional first")
        };
        let i = lowered.len();
        let is_ref = matches!(a, CallArg::Ref(_));
        let by_ref = def.params.get(i).is_some_and(|p| p.by_ref);
        if is_ref != by_ref {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                if is_ref {
                    format!(
                        "argument {} of `{callee}` is passed with `ref`, but the parameter is \
                         not `ref` (spec 4.7.3)",
                        i + 1
                    )
                } else {
                    format!(
                        "parameter {} of `{callee}` is `ref` - pass `ref x` at the call site \
                         (spec 4.7.3)",
                        i + 1
                    )
                },
            );
        }
        if !is_ref {
            reads.extend(crate::walk::free_places(&[], e));
        }
        let mut hint = generic_arg_hint(ctx, def, &subst, hint_self, i, e);
        // A slot several bounds of the trait type differently: an
        // unsuffixed literal takes the one it fits (spec 3.1.3).
        if let (HirTy::Unknown, Some(HirTy::Param(p))) = (&hint, hint_self) {
            let choices = super::bounds::slot_choices(ctx, def, *p, i);
            if !choices.is_empty() {
                match pick_literal_impl(ctx, e, &choices) {
                    Ok(Some(k)) => hint = choices[k].clone(),
                    Ok(None) => {}
                    Err(fits) => {
                        return ctx.error_expr(
                            span,
                            format!(
                                "`{callee}` on `{}`: the literal fits several of its bounds \
                                 ({fits}); add a suffix or `as` (spec 3.1.3)",
                                ctx.ty_name(&HirTy::Param(*p))
                            ),
                        );
                    }
                }
            }
        }
        // An untyped literal for an open `T` waits for a sibling argument of
        // the same bare `T` (`range(0, n)` with `n: i64` gives `i64`); with
        // none, the literal binds it (`fold(0, ..)`).
        let bare = |k: usize| {
            def.params
                .get(k)
                .and_then(|p| p.ty.as_ref())
                .and_then(|t| match &t.kind {
                    rokc_ast::TyKind::Path(p) if p.segments.len() == 1 => {
                        Some(p.segments[0].name.as_str())
                    }
                    _ => None,
                })
                .filter(|n| {
                    def.generics.params.iter().any(
                        |g| matches!(g, rokc_ast::GenericParam::Type { name, .. } if name == n),
                    )
                })
        };
        let offset = usize::from(has_receiver);
        let bare_param = bare(i).is_some_and(|n| {
            args.iter().enumerate().any(|(j, a)| {
                j + offset != i
                    && bare(j + offset) == Some(n)
                    && matches!(a, CallArg::Positional(x) if !is_untyped_literal(x))
            })
        });
        if hint == HirTy::Unknown && !is_ref && bare_param && is_untyped_literal(e) {
            deferred.push((i, e));
            lowered.push(poison(e.span));
            continue;
        }
        let h = lower_expected(ctx, e, hint);
        bind_from_arg(ctx, def, self_ty, i, &h.ty, &mut subst);
        if is_ref && super::mono::place_root(&h).is_some() {
            if let Some(p) = places.get_mut(i) {
                *p = Some(h.clone());
            }
        } else if is_ref && h.ty != HirTy::Unknown {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "`ref` argument {} of `{callee}` must name a variable or a field of one \
                     (spec 4.7)",
                    i + 1
                ),
            );
        }
        lowered.push(h);
    }
    for (i, e) in deferred {
        let hint = generic_arg_hint(ctx, def, &subst, hint_self, i, e);
        let h = lower_expected(ctx, e, hint);
        bind_from_arg(ctx, def, self_ty, i, &h.ty, &mut subst);
        lowered[i] = h;
    }
    // Missing trailing arguments take their declared defaults, lowered
    // at the call site like a non-generic call's (`ordering = SeqCst`).
    while let Some(d) = def
        .params
        .get(lowered.len())
        .and_then(|p| p.default.clone())
        .map(|d| in_module(&module_of(&def.name), d))
    {
        let h = lower_expected(ctx, &d, HirTy::Unknown);
        bind_from_arg(ctx, def, self_ty, lowered.len(), &h.ty, &mut subst);
        lowered.push(h);
    }
    if let Some(tail) = tail.filter(|_| lowered.len() + 1 == def.params.len()) {
        let i = lowered.len();
        let hint = generic_arg_hint(
            ctx,
            def,
            &subst,
            hint_self,
            i,
            &super::throws::path_expr("#", span),
        );
        let mut elem = variadic_elem(ctx, &hint);
        // `...[T; N]` with `N` unbound: the first array literal's length
        // is `N` (as for a `[T; N]` parameter).
        if let (HirTy::Unknown, Some(first)) = (&elem, tail.first()) {
            let slice_elem =
                def.params
                    .get(i)
                    .and_then(|p| p.ty.as_ref())
                    .and_then(|t| match &t.kind {
                        rokc_ast::TyKind::Path(p) => p.segments.last()?.generics.first(),
                        _ => None,
                    });
            if let (Some(t), CallArg::Positional(e)) = (slice_elem, first) {
                elem = ty_hint(ctx, def, &subst, hint_self, Some(t), e);
            }
        }
        let parts = tail
            .iter()
            .filter_map(|a| match a {
                CallArg::Positional(e) | CallArg::Ref(e) => Some(VarPart::Arg(e)),
                CallArg::Spread(e) => Some(VarPart::Spread(e)),
                CallArg::Named { .. } => None,
            })
            .collect();
        let h = lower_variadic(ctx, callee, elem, parts, span);
        bind_from_arg(ctx, def, self_ty, i, &h.ty, &mut subst);
        lowered.push(h);
    }
    check_ref_exclusive(ctx, callee, &lowered, &places, &reads, has_receiver);
    if lowered.len() != def.params.len() {
        let sig = sig_of_def(callee, def);
        arity_error(ctx, callee, def.params.len(), lowered.len(), Some(sig));
        return poison(span);
    }
    ctx.explicit_generics = explicit;
    let mono = monomorphize_on(ctx, def, self_ty, self_hint, &lowered, span);
    ctx.explicit_generics.clear();
    let Some((mono_id, ret_ty)) = mono else {
        return poison(span);
    };
    let param_tys = ctx
        .env
        .sink
        .mono_sigs
        .borrow()
        .get(&mono_id)
        .map(|s| s.param_tys.clone())
        .unwrap_or_default();
    let lowered: Vec<HirExpr> = lowered
        .into_iter()
        .enumerate()
        .map(|(i, a)| match param_tys.get(i) {
            Some(t) => coerce(ctx, t, a, &|| format!("argument {} of `{callee}`", i + 1)),
            None => a,
        })
        .collect();
    let out = ctx.env.sink.ref_out.borrow().get(&mono_id).cloned();
    if let Some(out) = out {
        let tys: Vec<HirTy> = out.refs.iter().map(|(_, t)| t.clone()).collect();
        let call = HirExpr {
            kind: HirExprKind::Call {
                callee: mono_id,
                args: lowered,
            },
            ty: super::mono::ref_packed(&out.ret, &tys),
            span,
        };
        return super::mono::ref_unpack(ctx, call, &out, &places);
    }
    HirExpr {
        kind: HirExprKind::Call {
            callee: mono_id,
            args: lowered,
        },
        ty: ret_ty,
        span,
    }
}

/// The expected type of argument `i` of a generic call, under the
/// parameters bound so far.
fn generic_arg_hint(
    ctx: &LowerCtx<'_>,
    def: &FnDef,
    subst: &HashMap<String, HirTy>,
    self_ty: Option<&HirTy>,
    i: usize,
    e: &Expr,
) -> HirTy {
    ty_hint(
        ctx,
        def,
        subst,
        self_ty,
        def.params.get(i).and_then(|p| p.ty.as_ref()),
        e,
    )
}

/// The expected type of argument `e` for a parameter of `def` typed
/// `param` (see `generic_arg_hint`).
fn ty_hint(
    ctx: &LowerCtx<'_>,
    def: &FnDef,
    subst: &HashMap<String, HirTy>,
    self_ty: Option<&HirTy>,
    param: Option<&Ty>,
    e: &Expr,
) -> HirTy {
    let hint = param.map_or(HirTy::Unknown, |t| {
        // Type params not bound yet stay opaque paths - never
        // resolved as same-named types visible from `def`.
        let mut partial = subst.clone();
        for gp in &def.generics.params {
            match gp {
                rokc_ast::GenericParam::Type { name, .. } => {
                    partial
                        .entry(name.clone())
                        .or_insert_with(|| HirTy::Path(vec![name.clone()]));
                }
                // An unbound const parameter: `#N` (no value yet).
                rokc_ast::GenericParam::Const { name, .. } => {
                    partial
                        .entry(name.clone())
                        .or_insert_with(|| HirTy::OpenConst(name.clone()));
                }
            }
        }
        let mut ty = resolve_in_mono(ctx, def, &partial, self_ty.cloned(), t);
        erase_unbound_errors(&mut ty);
        let mut leftover = Vec::new();
        collect_unknown_paths(&ty, &mut leftover);
        let open = |t: &HirTy| {
            let mut l = Vec::new();
            collect_unknown_paths(t, &mut l);
            !l.is_empty()
        };
        match ty {
            _ if leftover.is_empty() => ty,
            // `fn(T) -> U` with `U` open: the closure learns its parameters;
            // `U` binds from it afterwards.
            HirTy::Fn(args, ret, b) => HirTy::Fn(
                args.into_iter()
                    .map(|a| if open(&a) { HirTy::Unknown } else { a })
                    .collect(),
                Box::new(if open(&ret) { HirTy::Unknown } else { *ret }),
                b,
            ),
            _ => HirTy::Unknown,
        }
    });
    // `[T; N]` with `N` open and an array literal: the literal's length
    // (spec 2.11.6), not a `List` default.
    match (&hint, param, &e.kind) {
        (HirTy::Unknown, Some(t), ExprKind::List(items)) => match &t.kind {
            rokc_ast::TyKind::Array(elem, _) => {
                let mut et = resolve_in_mono(ctx, def, subst, None, elem);
                let mut open = Vec::new();
                collect_unknown_paths(&et, &mut open);
                if !open.is_empty() {
                    et = HirTy::Unknown;
                }
                u32::try_from(items.len()).map_or(hint, |n| HirTy::Array(Box::new(et), n))
            }
            _ => hint,
        },
        _ => hint,
    }
}

/// `f::<T, 3>(..)`: stash explicit arguments in `ctx.explicit_generics` for
/// the next generic call; `false` after reporting too many.
fn bind_explicit_generics(
    ctx: &mut LowerCtx<'_>,
    def: &FnDef,
    explicit: &[rokc_ast::Ty],
    full: &str,
) -> bool {
    if explicit.len() > def.generics.params.len() {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            super::mono::arity_msg(
                leaf_name(full),
                def.generics.params.len(),
                def.generics.params.len(),
                explicit.len(),
            ),
        );
        return false;
    }
    // Seeded by a literal handler call (`lower_magic`); empty otherwise.
    let mut bound = std::mem::take(&mut ctx.explicit_generics);
    for (gp, t) in def.generics.params.iter().zip(explicit) {
        let (rokc_ast::GenericParam::Type { name, .. }
        | rokc_ast::GenericParam::Const { name, .. }) = gp;
        bound.insert(name.clone(), ctx.resolve_ast_ty(t));
    }
    ctx.explicit_generics = bound;
    true
}

pub(super) fn lower_call(
    ctx: &mut LowerCtx<'_>,
    callee: &Expr,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let ExprKind::Path(p) = &callee.kind else {
        // `(s.f)(x)`, `make()(x)`, ...: call through the value.
        let callee_h = lower_expected(ctx, callee, HirTy::Unknown);
        return call_value(ctx, callee_h, args, span);
    };
    // A local of `fn(...)` / closure type shadows items.
    if let [s] = p.segments.as_slice() {
        if let Some(local) = ctx.lookup(&s.name) {
            let callee_h = ctx.read_local(local, callee.span);
            return call_value(ctx, callee_h, args, span);
        }
    }
    match alias_path(ctx, p) {
        Ok(a) => lower_path_call(ctx, a.as_ref().unwrap_or(p), args, span),
        Err(msg) => {
            discard_args(ctx, args);
            ctx.error_expr(span, msg)
        }
    }
}

const SPAWN: &str = "rok::core::task::spawn";
const SPAWN_PLAIN: &str = "rok::core::task::spawn_plain";

/// `task::spawn(e)`'s `e` makes the future right there (an `async` block,
/// an `async fn` call) and captures nothing a hand-off copies (a
/// collection, a future, a `[ref x]` cell): no other handle has its frame,
/// so `spawn_plain` hands nothing over (stdlib 2.18.1).
fn plain_fresh_future(ctx: &LowerCtx<'_>, args: &[CallArg]) -> bool {
    let [CallArg::Positional(e)] = args else {
        return false;
    };
    // A local such a future went into, named nowhere else, at the loop
    // depth it was made at (another pass would hand the same one over).
    if let ExprKind::Path(p) = &e.kind {
        if let [s] = p.segments.as_slice() {
            return ctx
                .lookup(&s.name)
                .and_then(|l| ctx.sole_fresh.get(&l))
                .is_some_and(|&depth| depth == ctx.breakables.len());
        }
    }
    fresh_future(ctx, e)
}

/// `e` makes a future right there that captures nothing a hand-off
/// copies (see `plain_fresh_future`).
fn fresh_future(ctx: &LowerCtx<'_>, e: &Expr) -> bool {
    let plain = |t: &HirTy| *t != HirTy::Unknown && !transfer_needs(ctx, t, 0);
    match &e.kind {
        ExprKind::AsyncBlock(_) => crate::walk::free_names(&[], e).iter().all(|n| {
            ctx.lookup(n)
                .is_none_or(|l| !ctx.is_boxed(l) && plain(&ctx.var_ty(l)))
        }),
        ExprKind::Call { callee, .. } => {
            let ExprKind::Path(p) = &callee.kind else {
                return false;
            };
            let [s] = p.segments.as_slice() else {
                return false;
            };
            if ctx.lookup(&s.name).is_some() {
                return false;
            }
            let Some(key) = ctx.resolve_fn_name(&[s.name.as_str()]) else {
                return false;
            };
            let Some(sig) = ctx.env.sig(&key) else {
                return false;
            };
            let coro = ctx.env.decls.get(sig.id.0 as usize).is_some_and(|d| {
                d.def()
                    .attrs
                    .iter()
                    .any(|a| a.path.first().map(String::as_str) == Some(rokc_ast::CORO_FN_ATTR))
            });
            coro && sig.param_tys.iter().all(plain)
        }
        _ => false,
    }
}

/// `spawn_plain` for a `task::spawn` call that may skip the hand-off.
fn plain_spawn<'a>(ctx: &LowerCtx<'a>, full: &str, args: &[CallArg]) -> Option<&'a FnDef> {
    (full == SPAWN && plain_fresh_future(ctx, args))
        .then(|| ctx.env.generic_fn(SPAWN_PLAIN))
        .flatten()
}

/// A call of path `p`, not a local.
fn lower_path_call(
    ctx: &mut LowerCtx<'_>,
    p: &AstPath,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let self_segs = self_path_segs(ctx, p);
    let segs: Vec<&str> = self_segs.iter().map(String::as_str).collect();
    let display = segs.join("::");

    let errors = ctx.env.sink.diagnostics.borrow().len();
    let concrete = ctx.resolve_fn_name(&segs);
    let generic = concrete
        .is_none()
        .then(|| ctx.resolve_generic_fn_name(&segs))
        .flatten();
    // An ambiguous name (reported) picks one: checking the arguments
    // against it would read as if that was meant.
    if ctx.env.sink.diagnostics.borrow().len() > errors {
        discard_args(ctx, args);
        return poison(span);
    }

    // Free function / extern / re-export, module-aware.
    if let Some(full) = concrete {
        ctx.check_item_visible(&full, "function");
        let sig = ctx.sig_of_key(&full);
        return call_sig(ctx, &sig, &full, None, args, span);
    }

    // Generic function - monomorphize on the argument types.
    if let Some(full) = generic {
        ctx.check_item_visible(&full, "function");
        let generic_def = ctx.env.generic_fn(&full).expect("a generic function");
        if let Some(&op) = ctx.env.generic_intrinsic(&generic_def.name) {
            if generic_def.is_unsafe {
                check_unsafe_call(ctx, &full);
            }
            let targs = p.segments.last().map_or(&[][..], |s| s.generics.as_slice());
            let name = leaf_name(&generic_def.name).to_owned();
            if op == Intrinsic::AddrOf {
                return lower_addr_of(ctx, generic_def, &name, args, span);
            }
            return lower_intrinsic(ctx, op, &name, targs, args, span);
        }
        let explicit = p.segments.last().map_or(&[][..], |s| s.generics.as_slice());
        if !bind_explicit_generics(ctx, generic_def, explicit, &full) {
            return poison(span);
        }
        // Named as written in its errors.
        let def = plain_spawn(ctx, &full, args).unwrap_or(generic_def);
        return call_generic(ctx, def, None, None, args, &full, span);
    }

    // `Box::get(b)` / `Box::new(v)` - generic-impl method by path.
    if let Some((method, ty_segs)) = segs.split_last() {
        let env = ctx.env;
        let gm = ctx
            .canon_type(ty_segs)
            .filter(|_| !ty_segs.is_empty())
            .and_then(|full| env.generic_methods(&full, method))
            .and_then(|v| {
                v.iter()
                    .find(|g| g.info.trait_name.is_none())
                    .or_else(|| v.first())
            });
        if let Some(gm) = gm {
            check_method_info_visible(ctx, &gm.info, method);
            // `Box::<i32>::new(v)`: the type's arguments fix `Self`;
            // `Box::m::<U>(..)`: the method's own.
            let n = p.segments.len();
            let self_hint = (!p.segments[n - 2].generics.is_empty()).then(|| {
                ctx.resolve_ast_ty(&Ty {
                    kind: AstTyKind::Path(rokc_ast::Path {
                        segments: p.segments[..n - 1].to_vec(),
                        span: p.span,
                    }),
                    span: p.span,
                })
            });
            // `let a: Atomic<i64> = Atomic::new(0)`: a constructor that
            // returns `Self` takes `Self` from the expected type.
            let returns_self = gm.def.ret_ty.as_ref().is_some_and(|t| {
                matches!(&t.kind, AstTyKind::Path(rp) if rp.segments.len() == 1
                    && (rp.segments[0].name == "Self" || rp.segments[0].name == *ty_segs[ty_segs.len() - 1]))
            });
            let self_hint = self_hint.or_else(|| {
                let full = ctx.canon_type(ty_segs)?;
                ctx.current_expected().cloned().filter(|t| {
                    returns_self
                        && matches!(t, HirTy::Struct(id)
                            if enum_base_name(&ctx.struct_def(*id).name) == full)
                })
            });
            match bind_explicit(
                ctx,
                &gm.def,
                Some(&gm.self_ty),
                &p.segments[n - 1].generics,
                method,
            ) {
                Ok(bound) => ctx.explicit_generics = bound,
                Err(msg) => {
                    discard_args(ctx, args);
                    return ctx.error_expr(span, msg);
                }
            }
            return call_generic_on(
                ctx,
                &gm.def,
                Some(&gm.self_ty),
                self_hint.as_ref(),
                None,
                args,
                &display,
                span,
            );
        }
    }

    // `Pair(3, 4)` of a generic tuple struct: a struct literal with the
    // positional field names, which infers the instantiation.
    let generic_tuple = ctx.canon_type(&segs).is_some_and(|full| {
        ctx.lookup_struct(&full).is_none()
            && ctx
                .env
                .defs
                .generic_struct(&full)
                .is_some_and(|d| matches!(d.kind, rokc_ast::StructKind::Tuple(_)))
    });
    let dup_ctor = ctx.canon_type(&segs).is_some_and(|full| {
        let at = ctx
            .lookup_struct(&full)
            .map(|sid| ctx.struct_def(sid).span)
            .or_else(|| ctx.env.defs.generic_struct(&full).map(|d| d.span));
        at.is_some_and(|at| ctx.env.duplicated(at))
    });
    if dup_ctor {
        discard_args(ctx, args);
        return poison(span);
    }
    if generic_tuple && args.iter().all(|a| matches!(a, CallArg::Positional(_))) {
        let fields: Vec<(String, Expr)> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                CallArg::Positional(e) => Some((i.to_string(), e.clone())),
                _ => None,
            })
            .collect();
        return lower_struct_init(ctx, p, &fields, None, span);
    }
    // `Foo(arg, ...)` - tuple-struct constructor (spec 2.7.4).
    if let Some(sid) = ctx
        .canon_type(&segs)
        .and_then(|full| ctx.lookup_struct(&full))
    {
        let def = ctx.struct_def(sid);
        let is_tuple_form = !def.fields.is_empty()
            && def
                .fields
                .iter()
                .enumerate()
                .all(|(i, f)| f.name == i.to_string());
        if !is_tuple_form {
            return ctx.error_expr(
                span,
                format!("`{display}` is not a tuple struct - use `{display} {{ field: ... }}`"),
            );
        }
        // Spec 5.6: constructing needs every field visible here.
        def.fields
            .iter()
            .for_each(|f| ctx.check_field_visible(sid, &f.name));
        let sig = FnSig {
            id: FunctionId(u32::MAX),
            ret_ty: HirTy::Struct(sid),
            param_defaults: vec![None; def.fields.len()],
            param_tys: def.fields.iter().map(|f| f.ty.clone()).collect(),
            param_names: def.fields.iter().map(|f| f.name.clone()).collect(),
            param_refs: Vec::new(),
            variadic: false,
        };
        let (fields, lets) = lower_args_for(ctx, &sig, &display, None, args, span);
        return with_lets(
            lets,
            HirExpr {
                kind: HirExprKind::StructInit {
                    struct_id: sid,
                    fields,
                },
                ty: HirTy::Struct(sid),
                span,
            },
        );
    }

    // `Enum::Variant(payload)`; a generic enum without an expected type is
    // instantiated from the arguments. `Enum::f(..)` that names no variant
    // is an associated function (below).
    let not_variant = enum_lacks_variant(ctx, &segs);
    let hint = explicit_enum(ctx, p).or_else(|| ctx.current_expected().cloned());
    let is_generic_enum_base = ctx.enum_path(&segs).is_some_and(|(full, _)| {
        ctx.lookup_enum(&full).is_none() && ctx.env.defs.generic_enum(&full).is_some()
    });
    let pre_args: Option<Vec<HirExpr>> = if !not_variant
        && is_generic_enum_base
        && !matches!(&hint, Some(HirTy::Enum(_)))
        && args.iter().all(|a| matches!(a, CallArg::Positional(_)))
    {
        Some(
            args.iter()
                .map(|a| match a {
                    CallArg::Positional(e) => lower_expected(ctx, e, HirTy::Unknown),
                    _ => unreachable!("checked positional"),
                })
                .collect(),
        )
    } else {
        None
    };
    let arg_tys: Option<Vec<HirTy>> = pre_args
        .as_ref()
        .map(|v| v.iter().map(|e| e.ty.clone()).collect());
    let res = if not_variant {
        EnumRes::NotEnum
    } else {
        resolve_enum_variant(ctx, &segs, hint.as_ref(), arg_tys.as_deref())
    };
    match res {
        EnumRes::Error => return poison(span),
        EnumRes::Found(enum_id, idx) => {
            let variant = ctx.enum_def(enum_id).variants[idx].clone();
            let slots = variant_arg_tys(&variant);
            if !variant.named_fields.is_empty() {
                return ctx.error_expr(
                    span,
                    format!("`{display}` has named fields - use `{display} {{ ... }}`"),
                );
            }
            let mut lets = Vec::new();
            let args_h = if let Some(pre) = pre_args {
                if pre.len() != slots.len() {
                    let tys = slots.iter().map(|t| ("", ctx.ty_name(t)));
                    let sig = super::sig_text(&display, tys);
                    arity_error(ctx, &display, slots.len(), pre.len(), Some(sig));
                    return poison(span);
                }
                pre.into_iter()
                    .zip(&slots)
                    .enumerate()
                    .map(|(i, (a, t))| {
                        coerce(ctx, t, a, &|| format!("argument {} of `{display}`", i + 1))
                    })
                    .collect()
            } else {
                let sig = FnSig {
                    id: FunctionId(u32::MAX),
                    ret_ty: HirTy::Enum(enum_id),
                    param_defaults: vec![None; slots.len()],
                    param_tys: slots.clone(),
                    param_names: (0..slots.len()).map(|i| format!("#{}", i + 1)).collect(),
                    param_refs: Vec::new(),
                    variadic: false,
                };
                let (a, l) = lower_args_for(ctx, &sig, &display, None, args, span);
                lets = l;
                a
            };
            return with_lets(
                lets,
                HirExpr {
                    kind: HirExprKind::EnumVariantInit {
                        enum_id,
                        variant_index: u32::try_from(idx).expect("variant overflow"),
                        args: args_h,
                    },
                    ty: HirTy::Enum(enum_id),
                    span,
                },
            );
        }
        EnumRes::NotEnum => {}
    }

    // `Type::method(args)` through the method table; a generic base
    // (`Stack::new()`) resolves when one instance has the method.
    // `Tr::m(x)`, `T::f()`, `i32::add(a, b)`, `Self::f()` go through the
    // trait's impls for the type.
    if let Some((method, ty_segs)) = segs.split_last().filter(|(_, t)| !t.is_empty()) {
        if let Some(r) =
            path_call_via_impls(ctx, &p.segments, ty_segs, method, args, &display, span)
        {
            return r;
        }
    }

    if let Some((method, ty_segs)) = segs.split_last() {
        if !ty_segs.is_empty() {
            if let Some(full) = ctx.canon_type(ty_segs) {
                let method = (*method).to_owned();
                if let Some(r) = named_type_overload(ctx, &full, &method, args, &display, span) {
                    return r;
                }
                let hits = match path_method_hits(ctx, &full, &method) {
                    Ok(h) => h,
                    Err(msg) => {
                        discard_args(ctx, args);
                        return ctx.error_expr(span, msg);
                    }
                };
                match hits.as_slice() {
                    [fn_id] => {
                        check_method_visible(ctx, *fn_id, &method);
                        let sig = ctx.sig_by_id(*fn_id);
                        return call_sig(ctx, &sig, &display, None, args, span);
                    }
                    [] => {
                        if let Some(r) =
                            named_type_call(ctx, &p.segments, &full, &method, args, span)
                        {
                            return r;
                        }
                        // A generic impl's method: `Self` is the expected
                        // type (`let b: Box<i32> = Box::zero()`), else
                        // `self`'s.
                        let of_base = |ctx: &LowerCtx<'_>, t: &HirTy| match t {
                            HirTy::Struct(id) => enum_base_name(&ctx.struct_def(*id).name) == full,
                            HirTy::Enum(id) => enum_base_name(&ctx.enum_def(*id).name) == full,
                            _ => false,
                        };
                        if let Some(t) = ctx.current_expected().cloned().filter(|t| of_base(ctx, t))
                        {
                            if let Some(r) =
                                call_impl_method(ctx, &t, &method, None, None, args, &display, span)
                            {
                                return r;
                            }
                        }
                        if let Some((a @ (CallArg::Positional(e) | CallArg::Ref(e)), rest)) =
                            args.split_first()
                        {
                            let recv = lower_expected(ctx, e, HirTy::Unknown);
                            let t = recv.ty.clone();
                            if of_base(ctx, &t) {
                                ctx.explicit_recv = Some(matches!(a, CallArg::Ref(_)));
                                let r = call_impl_method(
                                    ctx,
                                    &t,
                                    &method,
                                    None,
                                    Some(recv),
                                    rest,
                                    &display,
                                    span,
                                );
                                ctx.explicit_recv = None;
                                if let Some(r) = r {
                                    return r;
                                }
                            }
                            discard_args(ctx, rest);
                        }
                        let e = ctx.error_expr(
                            span,
                            format!("no associated function `{method}` on `{full}`"),
                        );
                        ctx.help_last(super::reflect::inspect_help_key(ctx, &full, &method));
                        return e;
                    }
                    _ => {
                        return ctx.error_expr(
                            span,
                            format!(
                                "`{display}` is ambiguous: several instantiations of `{full}` \
                                 provide `{method}` - name the type arguments"
                            ),
                        );
                    }
                }
            }
        }
    }

    call_item_value(ctx, p, &segs, &display, args, span)
}

/// The rest of a path call: a `const` / `static` of `fn(...)` type is
/// called through its value; anything else is unknown.
fn call_item_value(
    ctx: &mut LowerCtx<'_>,
    p: &AstPath,
    segs: &[&str],
    display: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if ctx.resolve_in(segs, false).is_some() || ctx.resolve_in(segs, true).is_some() {
        let callee_h = lower_path(ctx, p, p.span);
        return call_value(ctx, callee_h, args, span);
    }
    discard_args(ctx, args);
    if let Some(help) = (segs.len() > 1).then(|| ctx.import_help(segs)).flatten() {
        return ctx.unknown_name(
            span,
            format!(
                "cannot find function `{display}`: module `{}` is not imported here",
                segs[0]
            ),
            Some(help),
        );
    }
    let help = habit_call_help(ctx, segs, args).or_else(|| ctx.value_help(segs));
    ctx.unknown_name(
        span,
        format!("cannot find function `{display}` in this scope"),
        help,
    )
}

/// What rok writes for a call another language has: `Some(x)`,
/// `range(n)`.
fn habit_call_help(ctx: &LowerCtx<'_>, segs: &[&str], args: &[CallArg]) -> Option<String> {
    let texts: Vec<&str> = args
        .iter()
        .filter_map(|a| match a {
            CallArg::Positional(e) => ctx.src(e.span),
            _ => None,
        })
        .collect();
    match (segs, texts.as_slice()) {
        (["Some"], [x]) => Some(format!(
            "rok has no `Some`: a `T | none` holds the value itself; write `{x}`"
        )),
        (["range"], [n]) => Some(format!("use a range: `0..{n}`")),
        (["range"], [a, b]) => Some(format!("use a range: `{a}..{b}`")),
        _ => None,
    }
}

/// `Box::<f64>::f()` on a generic impl, or `Reflect`'s `type_name()` /
/// `size()` / `align()` on a struct or enum instance.
fn named_type_call(
    ctx: &mut LowerCtx<'_>,
    segs: &[rokc_ast::PathSegment],
    full: &str,
    method: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    let ty_segs = &segs[..segs.len() - 1];
    let own = if ty_segs.last().is_some_and(|s| !s.generics.is_empty()) {
        ctx.resolve_ast_ty(&Ty {
            kind: AstTyKind::Path(AstPath {
                segments: ty_segs.to_vec(),
                span,
            }),
            span,
        })
    } else {
        ctx.lookup_struct(full)
            .map(HirTy::Struct)
            .or_else(|| ctx.lookup_enum(full).map(HirTy::Enum))?
    };
    if let Some(r) = call_impl_method(ctx, &own, method, None, None, args, method, span) {
        return Some(r);
    }
    super::reflect::static_call(ctx, &own, method, args, span)
}

/// A range (stdlib 2.7.4, spec 6.5.1): each shape calls its `#[literal]`
/// handler (core's `Range`, `RangeInclusive`, ...).
fn lower_range_value(
    ctx: &mut LowerCtx<'_>,
    start: Option<&Expr>,
    end: Option<&Expr>,
    inclusive: bool,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    use rokc_parser::Shape;
    let (shape, what) = match (start, end, inclusive) {
        (Some(_), Some(_), false) => (Shape::Range, "`a..b`"),
        (Some(_), Some(_), true) => (Shape::RangeInclusive, "`a..=b`"),
        (Some(_), None, _) => (Shape::RangeFrom, "`a..`"),
        (None, Some(_), false) => (Shape::RangeTo, "`..b`"),
        (None, Some(_), true) => (Shape::RangeToInclusive, "`..=b`"),
        (None, None, _) => (Shape::RangeFull, "`..`"),
    };
    let args = [start, end]
        .into_iter()
        .flatten()
        .map(|e| rokc_ast::LitArg::One(e.clone()))
        .collect();
    lower_shape(ctx, shape, args, what, span)
}

/// `Type::method` on a named struct / enum (or each instantiation of a
/// generic base): the concrete methods `pick_method` chooses.
fn path_method_hits(
    ctx: &LowerCtx<'_>,
    full: &str,
    method: &str,
) -> Result<Vec<FunctionId>, String> {
    let mut tys: Vec<HirTy> = Vec::new();
    if let Some(sid) = ctx.lookup_struct(full) {
        tys.push(HirTy::Struct(sid));
    } else if let Some(eid) = ctx.lookup_enum(full) {
        tys.push(HirTy::Enum(eid));
    } else {
        tys.extend(
            struct_instantiations(ctx, full)
                .into_iter()
                .map(HirTy::Struct),
        );
        tys.extend(ctx.enum_instantiations(full).into_iter().map(HirTy::Enum));
    }
    let mut hits = Vec::new();
    for ty in tys {
        if let Some(MethodPick::Concrete(id)) = pick_method(ctx, &ty, method, None)? {
            hits.push(id);
        }
    }
    Ok(hits)
}

/// `<T as Tr>::m(args)` (spec 2.13.4): `Tr`'s `m` for `T`, the first
/// argument as `self`; `<T as Tr>::X::m(args)`: `m` of associated type `X`.
fn qpath_member_call(
    ctx: &mut LowerCtx<'_>,
    q: &rokc_ast::QPath,
    method: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let assoc = rokc_ast::Ty {
        kind: rokc_ast::TyKind::QPath(Box::new(rokc_ast::QPath {
            member: None,
            ..q.clone()
        })),
        span,
    };
    let t = ctx.resolve_ast_ty(&assoc);
    if t == HirTy::Unknown {
        discard_args(ctx, args);
        return poison(span);
    }
    let display = format!("{}::{method}", ctx.ty_name(&t));
    call_impl_method(ctx, &t, method, None, None, args, &display, span)
        .or_else(|| super::reflect::static_call(ctx, &t, method, args, span))
        .unwrap_or_else(|| {
            discard_args(ctx, args);
            super::reflect::no_assoc_fn(ctx, &t, method, span)
        })
}

fn qpath_call(
    ctx: &mut LowerCtx<'_>,
    q: &rokc_ast::QPath,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if let Some(m) = &q.member {
        return qpath_member_call(ctx, q, m, args, span);
    }
    let method = q.item.as_str();
    let t = ctx.resolve_ast_ty(&q.self_ty);
    let key = qpath_trait(ctx, q);
    let display = format!(
        "<{} as {}>::{method}",
        ctx.ty_name(&t),
        key.as_deref().unwrap_or("?")
    );
    let Some(key) = key else {
        discard_args(ctx, args);
        return ctx.error_expr(span, format!("`{display}`: not a trait after `as`"));
    };
    if method == "drop" && key == ctx.lang("drop_method") {
        if let Some(e) = direct_drop_error(ctx, span) {
            discard_args(ctx, args);
            return e;
        }
    }
    let (recv, rest) = match args.split_first() {
        Some((a @ (CallArg::Positional(e) | CallArg::Ref(e)), rest)) => {
            let r = lower_expected(ctx, e, t.clone());
            ctx.explicit_recv = Some(matches!(a, CallArg::Ref(_)));
            (Some(r), rest)
        }
        _ => (None, args),
    };
    // `<P as Convert<f64>>::convert(p)`: the impl of those arguments.
    if let Some(fid) = method_by_trait_args(ctx, &t, method, &key, q) {
        ctx.explicit_recv = None;
        check_method_visible(ctx, fid, method);
        let sig = ctx.sig_by_id(fid);
        return call_sig(ctx, &sig, &display, recv, rest, span);
    }
    // `<T as Tr<A>>::m(x)` on a type parameter: the bound `Tr<A>`; the
    // written arguments are the bound template's trait parameters.
    if let (HirTy::Param(_), Some(info), Some(seg)) =
        (&t, ctx.env.trait_of(&key), q.trait_path.segments.last())
    {
        let written = super::resolve::trait_args(&seg.generics, |a| ctx.resolve_ast_ty(a)).0;
        let names = info.generics.params.iter().map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. }
            | rokc_ast::GenericParam::Const { name, .. } => name.clone(),
        });
        let bound: HashMap<String, HirTy> = names.zip(written).collect();
        ctx.explicit_generics.extend(bound);
    }
    let call = call_impl_method(ctx, &t, method, Some(&key), recv, rest, &display, span);
    ctx.explicit_recv = None;
    call.unwrap_or_else(|| {
        ctx.error_expr(
            span,
            format!(
                "`{}` has no `{key}` impl providing `{method}`",
                ctx.ty_name(&t)
            ),
        )
    })
}

/// The concrete `method` of `t` from the impl of `key` with the arguments
/// `<.. as Tr<A, ..>>` writes (defaults fill the rest); `None` without.
fn method_by_trait_args(
    ctx: &LowerCtx<'_>,
    t: &HirTy,
    method: &str,
    key: &str,
    q: &rokc_ast::QPath,
) -> Option<FunctionId> {
    let written = &q.trait_path.segments.last()?.generics;
    if written.is_empty() {
        return None;
    }
    let args: Vec<HirTy> = written.iter().map(|a| ctx.resolve_ast_ty(a)).collect();
    let ids = ctx.env.methods_on(t, method)?;
    let mut hits = ids.iter().filter(|id| {
        ctx.env.method_info(**id).is_some_and(|i| {
            i.trait_name.as_deref() == Some(key) && i.trait_args.starts_with(&args)
        })
    });
    let hit = *hits.next()?;
    hits.next().is_none().then_some(hit)
}

/// `Tr::m(..)` (`Self` from the first argument, else the expected type),
/// or `T::f(..)` / `i32::f(..)` / `Self::f(..)` on a non-struct/enum type.
/// `None`: not such a path.
#[allow(clippy::too_many_arguments)]
fn path_call_via_impls(
    ctx: &mut LowerCtx<'_>,
    all_segs: &[rokc_ast::PathSegment],
    ty_segs: &[&str],
    method: &str,
    args: &[CallArg],
    display: &str,
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    let trait_key = ctx
        .canon_type(ty_segs)
        .filter(|f| ctx.env.names.is_trait(f));
    if let Some(key) = trait_key {
        // A static method (`TryFrom::try_from(8)`) takes `Self` from the
        // expected type; its first argument is not `self`.
        let static_self = ctx
            .env
            .trait_of(&key)
            .and_then(|i| i.decls.get(method))
            .is_some_and(|d| d.params.first().is_none_or(|p| p.name != "self"))
            .then(|| ctx.current_expected().cloned())
            .flatten()
            .filter(|t| *t != HirTy::Unknown);
        let (recv, rest) = match args.split_first() {
            _ if static_self.is_some() => (None, args),
            Some((a @ (CallArg::Positional(e) | CallArg::Ref(e)), rest)) => {
                let r = lower_expected(ctx, e, HirTy::Unknown);
                ctx.explicit_recv = Some(matches!(a, CallArg::Ref(_)));
                (Some(r), rest)
            }
            _ => (None, args),
        };
        let self_ty = static_self
            .or_else(|| recv.as_ref().map(|r| r.ty.clone()))
            .or_else(|| ctx.current_expected().cloned())
            .filter(|t| *t != HirTy::Unknown);
        let Some(t) = self_ty else {
            discard_args(ctx, rest);
            return Some(ctx.error_expr(
                span,
                format!(
                    "cannot tell which type's `{key}` impl `{display}` calls - annotate the \
                     expected type"
                ),
            ));
        };
        if key == super::reflect::REFLECT {
            ctx.explicit_recv = None;
            if let Some(r) = super::reflect::trait_call(ctx, &t, recv.clone(), method, rest, span) {
                return Some(r);
            }
        }
        match (&t, &recv) {
            (HirTy::Sum(_), Some(r)) if method == "fmt" && key == ctx.lang("debug_trait") => {
                ctx.explicit_recv = None;
                return Some(sum_structural_call(
                    ctx,
                    r.clone(),
                    method,
                    rest,
                    None,
                    span,
                ));
            }
            // `Trait::m(v)` on an `any` value: that trait's `m`, dispatched.
            (HirTy::AnyTrait(bounds), Some(r)) => {
                ctx.explicit_recv = None;
                let (bounds, r) = (bounds.clone(), r.clone());
                return Some(any_method_call(
                    ctx,
                    r,
                    &bounds,
                    method,
                    Some(&key),
                    rest,
                    span,
                ));
            }
            _ => {}
        }
        let call = call_impl_method(ctx, &t, method, Some(&key), recv, rest, display, span);
        ctx.explicit_recv = None;
        return Some(call.unwrap_or_else(|| {
            ctx.error_expr(
                span,
                format!(
                    "`{}` has no `{key}` impl providing `{method}`",
                    ctx.ty_name(&t)
                ),
            )
        }));
    }
    // Only paths the struct / enum lookup can't take: type parameters,
    // primitives, `Self`, associated types (2.8.3).
    let ty_path = rokc_ast::Path {
        segments: all_segs[..all_segs.len() - 1].to_vec(),
        span,
    };
    let ty_ast = rokc_ast::Ty {
        kind: rokc_ast::TyKind::Path(ty_path.clone()),
        span,
    };
    let (name, projected) = match ty_segs {
        [name] => (*name, None),
        _ if ctx.canon_type(ty_segs).is_none() && ty_path.segments.len() > 1 => {
            ("", Some(ctx.projection(&ty_path)?))
        }
        _ => return None,
    };
    let is_type_param = ctx.ty_subst.contains_key(name);
    let prim = matches!(crate::ty::resolve_ty(&ty_ast), HirTy::Prim(_));
    let assoc = ctx.self_ty.is_some() && ctx.assoc_names.iter().any(|a| a == name);
    if !(projected.is_some() || is_type_param || prim || assoc || name == "Self") {
        return None;
    }
    let t = projected.unwrap_or_else(|| ctx.resolve_ast_ty(&ty_ast));
    if matches!(t, HirTy::Unknown) {
        discard_args(ctx, args);
        return Some(poison(span));
    }
    // `T::default()` for a `T | none` sum is `none` (stdlib 2.13).
    if let (HirTy::Sum(vs), "default", []) = (&t, method, args) {
        if let Some(i) = vs.iter().position(HirTy::is_none) {
            return Some(HirExpr {
                kind: HirExprKind::SumWrap {
                    sum_ty: t.clone(),
                    variant_index: u32::try_from(i).expect("variant overflow"),
                    value: None,
                },
                ty: t.clone(),
                span,
            });
        }
    }
    // `T::from_str(s)` under `T: FromStr` is the bound's method, not an
    // inherent one of the same name (`int::from_str(s, radix)`).
    let bound_key = ctx
        .ty_bounds
        .get(name)
        .filter(|_| is_type_param)
        .and_then(|bs| {
            bs.iter()
                .map(|b| b.split('<').next().unwrap_or(b))
                .find(|k| {
                    ctx.env
                        .trait_of(k)
                        .is_some_and(|i| i.decls.contains_key(method))
                })
                .map(str::to_owned)
        });
    let call = call_impl_method(
        ctx,
        &t,
        method,
        bound_key.as_deref(),
        None,
        args,
        display,
        span,
    )
    .or_else(|| super::reflect::static_call(ctx, &t, method, args, span));
    Some(call.unwrap_or_else(|| {
        discard_args(ctx, args);
        super::reflect::no_assoc_fn(ctx, &t, method, span)
    }))
}

/// Call `t`'s method `method` - of trait `trait_key` when given -
/// from a concrete impl, else a generic one. `recv` fills `self`.
#[allow(clippy::too_many_arguments)]
fn call_impl_method(
    ctx: &mut LowerCtx<'_>,
    t: &HirTy,
    method: &str,
    trait_key: Option<&str>,
    recv: Option<HirExpr>,
    args: &[CallArg],
    display: &str,
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    if let Some(call) = call_overload(ctx, t, method, trait_key, recv.clone(), args, display, span)
    {
        return Some(call);
    }
    let gm = match pick_method(ctx, t, method, trait_key) {
        Ok(Some(MethodPick::Concrete(fid))) => {
            check_method_visible(ctx, fid, method);
            let sig = ctx.sig_by_id(fid);
            return Some(call_sig(ctx, &sig, display, recv, args, span));
        }
        Ok(Some(MethodPick::Generic(gm))) => gm,
        Ok(None) => return None,
        Err(msg) => {
            let Some(gm) = pick_generic_by_bounds(ctx, t, method, args) else {
                discard_args(ctx, args);
                return Some(ctx.error_expr(span, msg));
            };
            gm
        }
    };
    check_method_info_visible(ctx, &gm.info, method);
    // `Self` = `t` binds the impl's parameters, so a static method
    // (`fn zero() -> Self`) instantiates too.
    Some(call_generic_on(
        ctx,
        &gm.def,
        Some(&gm.self_ty),
        Some(t),
        recv,
        args,
        display,
        span,
    ))
}

/// A trait implemented several times with generic methods
/// (`FromIterator<char>` / `<str>` for `str`): the one template whose
/// bounds hold with the argument types (rolled-back probe).
fn pick_generic_by_bounds<'e>(
    ctx: &mut LowerCtx<'e>,
    t: &HirTy,
    method: &str,
    args: &[CallArg],
) -> Option<&'e GenericMethod> {
    let cands = generic_method_for(ctx, t, method);
    let tr = cands.first()?.info.trait_name.as_deref()?;
    if cands.len() < 2
        || cands
            .iter()
            .any(|g| g.info.trait_name.as_deref() != Some(tr))
    {
        return None;
    }
    let mark = ctx.probe_mark();
    let tys: Vec<HirTy> = args
        .iter()
        .map(|a| match a {
            CallArg::Positional(e) => lower_expected(ctx, e, HirTy::Unknown).ty,
            _ => HirTy::Unknown,
        })
        .collect();
    let clean = ctx.probe_clean(&mark);
    ctx.rollback(&mark);
    if !clean {
        return None;
    }
    let holds = |gm: &GenericMethod| {
        let def = &gm.def;
        let mut subst: HashMap<String, HirTy> = HashMap::new();
        for (p, ty) in def.params.iter().zip(&tys) {
            let Some(rokc_ast::TyKind::Path(path)) = p.ty.as_ref().map(|t| &t.kind) else {
                continue;
            };
            if let [seg] = path.segments.as_slice() {
                subst.insert(seg.name.clone(), ty.clone());
            }
        }
        let module = module_of(&def.name);
        def.generics.params.iter().all(|gp| {
            let rokc_ast::GenericParam::Type { name, bounds, .. } = gp else {
                return true;
            };
            let Some(concrete) = subst.get(name) else {
                return true;
            };
            bounds.iter().all(|b| {
                ctx.bound_ref(b, &module, |t| {
                    super::mono::resolve_in_mono(ctx, def, &subst, None, t)
                })
                .is_none_or(|want| ctx.implements_trait(concrete, &want, 0))
            })
        })
    };
    let mut hits = cands.iter().filter(|gm| holds(gm));
    let hit = hits.next()?;
    hits.next().is_none().then_some(hit)
}

/// `call_overload` on the struct / enum named `full`.
fn named_type_overload(
    ctx: &mut LowerCtx<'_>,
    full: &str,
    method: &str,
    args: &[CallArg],
    display: &str,
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    let t = ctx
        .lookup_struct(full)
        .map(HirTy::Struct)
        .or_else(|| ctx.lookup_enum(full).map(HirTy::Enum))?;
    call_overload(ctx, &t, method, None, None, args, display, span)
}

/// A trait implemented over several types (`From<i32>` + `From<i8>`):
/// pick by the first parameter the impls disagree on (spec 3.1.3), an
/// unsuffixed literal via `pick_literal_impl`, anything else by exact
/// type. `None` if `method` isn't overloaded so, or (method call) no impl
/// has a non-literal argument's type.
#[allow(clippy::too_many_arguments)]
fn call_overload(
    ctx: &mut LowerCtx<'_>,
    t: &HirTy,
    method: &str,
    trait_key: Option<&str>,
    recv: Option<HirExpr>,
    args: &[CallArg],
    display: &str,
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    let ids = ctx.env.methods_on(t, method)?;
    let traits: Vec<Option<&str>> = ids
        .iter()
        .map(|id| {
            ctx.env
                .method_info(*id)
                .and_then(|i| i.trait_name.as_deref())
        })
        .collect();
    let tn = *traits
        .iter()
        .flatten()
        .find(|t| trait_key.is_none_or(|k| k == **t))?;
    let sigs: Vec<FnSig> = ids
        .iter()
        .zip(&traits)
        .filter(|(_, t)| **t == Some(tn))
        .map(|(id, _)| ctx.sig_by_id(*id))
        .collect();
    if sigs.len() < 2 || traits.contains(&None) {
        return None;
    }
    let arity = sigs.iter().map(|s| s.param_tys.len()).min()?;
    let slot = (0..arity).find(|&k| sigs.iter().any(|s| s.param_tys[k] != sigs[0].param_tys[k]))?;
    let params: Vec<HirTy> = sigs.iter().map(|s| s.param_tys[slot].clone()).collect();
    let mut literal = false;
    let arg_ty = if let (Some(r), 0) = (&recv, slot) {
        r.ty.clone()
    } else {
        let Some(CallArg::Positional(e)) = args.get(slot - usize::from(recv.is_some())) else {
            return None;
        };
        literal = untyped_lit_value(e).is_some();
        match pick_literal_impl(ctx, e, &params) {
            Ok(Some(i)) => params[i].clone(),
            Ok(None) => {
                let mark = ctx.probe_mark();
                let ty = lower_expected(ctx, e, HirTy::Unknown).ty;
                ctx.rollback(&mark);
                ty
            }
            Err(fits) => {
                discard_args(ctx, args);
                return Some(ctx.error_expr(
                    span,
                    format!(
                        "`{display}`: the literal fits several `{}` impls of `{}` ({fits}); \
                         add a suffix or `as`",
                        leaf_name(tn),
                        ctx.ty_name(t)
                    ),
                ));
            }
        }
    };
    let Some(i) = params.iter().position(|p| *p == arg_ty) else {
        if recv.is_some() && !literal {
            return None;
        }
        discard_args(ctx, args);
        if arg_ty == HirTy::Unknown {
            return Some(poison(span));
        }
        return Some(ctx.error_expr(
            span,
            format!(
                "`{display}`: no `{}<{}>` impl for `{}`",
                leaf_name(tn),
                ctx.ty_name(&arg_ty),
                ctx.ty_name(t)
            ),
        ));
    };
    check_method_visible(ctx, sigs[i].id, method);
    Some(call_sig(ctx, &sigs[i], display, recv, args, span))
}

/// An unsuffixed numeric literal's value, read off the AST.
// Plain and negated literals only: `1 + 1` keeps its default type.
#[derive(Clone, Copy)]
enum LitVal {
    Int(i128),
    Float(f64),
}

fn untyped_lit_value(e: &Expr) -> Option<LitVal> {
    let (lit, neg) = match &e.kind {
        ExprKind::Literal(l) => (l, false),
        ExprKind::OpPrefix { op, operand } if op == "-" => match &operand.kind {
            ExprKind::Literal(l) => (l, true),
            _ => return None,
        },
        _ => return None,
    };
    if !is_untyped_literal(e) {
        return None;
    }
    match lit {
        AstLit::Int(t) => parse_int_literal(t).map(|(v, _)| LitVal::Int(if neg { -v } else { v })),
        AstLit::Float(t) => {
            let cleaned: String = t.chars().filter(|c| *c != '_').collect();
            cleaned.parse::<f64>().ok().map(LitVal::Float)
        }
        _ => None,
    }
}

/// Can a literal `v` take `ty`: an in-range integer type (or `int`), a
/// float type it stays finite in (or `float`).
fn literal_fits(ctx: &LowerCtx<'_>, v: LitVal, ty: &HirTy) -> bool {
    #[allow(clippy::cast_possible_truncation)]
    match (v, ty) {
        (LitVal::Int(v), HirTy::Prim(p)) => {
            p.is_int()
                && !matches!(p, PrimTy::Char | PrimTy::CChar)
                && super::pattern::int_fits(v, *p)
        }
        (LitVal::Float(f), HirTy::Prim(PrimTy::F32)) => (f as f32).is_finite(),
        (LitVal::Float(f), HirTy::Prim(PrimTy::F64)) => f.is_finite(),
        (LitVal::Int(_), _) => super::bignum::big_of(ctx, ty) == Some(super::bignum::Big::Int),
        (LitVal::Float(_), _) => super::bignum::big_of(ctx, ty) == Some(super::bignum::Big::Float),
    }
}

/// Spec 3.1.3: which impl an unsuffixed literal takes among candidate slot
/// types `params` (`a * 2`, `T::from(2)`, `2.into()`): its default type if
/// offered, else the one it fits. `Ok(None)`: not a literal or nothing
/// fits; `Err`: several fit (named for the diagnostic).
fn pick_literal_impl(
    ctx: &LowerCtx<'_>,
    lit: &Expr,
    params: &[HirTy],
) -> Result<Option<usize>, String> {
    let Some(v) = untyped_lit_value(lit) else {
        return Ok(None);
    };
    let default = match v {
        LitVal::Int(_) => HirTy::Prim(PrimTy::I32),
        LitVal::Float(_) => HirTy::Prim(PrimTy::F64),
    };
    if let Some(i) = params.iter().position(|p| *p == default) {
        if literal_fits(ctx, v, &default) {
            return Ok(Some(i));
        }
    }
    let fits: Vec<usize> = (0..params.len())
        .filter(|i| literal_fits(ctx, v, &params[*i]))
        .collect();
    match fits[..] {
        [] => Ok(None),
        [i] => Ok(Some(i)),
        _ => Err(fits
            .iter()
            .map(|i| format!("`{}`", ctx.ty_name(&params[*i])))
            .collect::<Vec<_>>()
            .join(", ")),
    }
}

/// `lit.into()` / `lit.try_into()` in a slot of `expected`: the literal
/// takes the `From` / `TryFrom` impl of `expected` it fits.
fn literal_conv_receiver(ctx: &mut LowerCtx<'_>, receiver: &Expr, method: &str) -> Option<HirExpr> {
    let conv = match method {
        "into" => "from",
        "try_into" => "try_from",
        _ => return None,
    };
    untyped_lit_value(receiver)?;
    let expected = ctx.current_expected()?.clone();
    let ids = ctx.env.methods_on(&expected, conv)?;
    let params: Vec<HirTy> = ids
        .iter()
        .filter_map(|id| ctx.sig_by_id(*id).param_tys.first().cloned())
        .collect();
    // Ambiguous or nothing fits: the ordinary lowering reports it.
    let i = pick_literal_impl(ctx, receiver, &params).ok()??;
    Some(lower_expected(ctx, receiver, params[i].clone()))
}

/// `[a, b]` is a `List` (spec 6.5.1); a fixed array only in a slot
/// typed `[T; N]`.
fn lower_list_lit(ctx: &mut LowerCtx<'_>, items: &[Expr], span: rokc_diagnostic::Span) -> HirExpr {
    let Some(HirTy::Array(e, _)) = ctx.current_expected().cloned() else {
        let args = vec![rokc_ast::LitArg::Many(items.to_vec())];
        return lower_shape(ctx, rokc_parser::Shape::List, args, "`List`", span);
    };
    let mut elem_ty = (*e).clone();
    let mut lowered: Vec<HirExpr> = Vec::with_capacity(items.len());
    for (i, e) in items.iter().enumerate() {
        let v = lower_expected(ctx, e, elem_ty.clone());
        // An open element type (`[T; N]` slot, `T` unbound) is the
        // first element's.
        if elem_ty == HirTy::Unknown {
            elem_ty = v.ty.clone();
        }
        lowered.push(coerce(ctx, &elem_ty, v, &|| format!("array element {i}")));
    }
    let len = u32::try_from(lowered.len()).expect("array literal length overflow");
    HirExpr {
        ty: HirTy::Array(Box::new(elem_ty.clone()), len),
        span,
        kind: HirExprKind::ArrayInit {
            elem_ty,
            elements: lowered,
        },
    }
}

/// `[v; N]`: `v` evaluated once, then copied into each element (one
/// node per element up to `REPEAT_UNROLL`, a fill loop beyond).
fn lower_array_repeat(
    ctx: &mut LowerCtx<'_>,
    value: &Expr,
    len: &rokc_ast::ConstExpr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let elem_hint = match ctx.current_expected() {
        Some(HirTy::Array(e, _)) => (**e).clone(),
        _ => HirTy::Unknown,
    };
    let v = lower_expected(ctx, value, elem_hint.clone());
    let v = if elem_hint == HirTy::Unknown {
        v
    } else {
        coerce(ctx, &elem_hint, v, &|| "array repeat value".to_owned())
    };
    let Some(n) = ctx.eval_const(len).and_then(|n| u32::try_from(n).ok()) else {
        return ctx.error_expr(
            span,
            "the length of `[v; N]` must be a non-negative compile-time integer (spec 2.11)"
                .to_owned(),
        );
    };
    let elem_ty = v.ty.clone();
    let tmp = ctx.declare(None, elem_ty.clone());
    let ty = HirTy::Array(Box::new(elem_ty.clone()), n);
    HirExpr {
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts: vec![HirStmt::Let {
                local: tmp,
                init: v,
            }],
            trailing: Some(HirExpr {
                kind: if n > REPEAT_UNROLL {
                    HirExprKind::ArrayRepeat {
                        value: Box::new(hir_local(tmp, &elem_ty, span)),
                        elem_ty,
                        len: n,
                    }
                } else {
                    HirExprKind::ArrayInit {
                        elements: (0..n).map(|_| hir_local(tmp, &elem_ty, span)).collect(),
                        elem_ty,
                    }
                },
                ty: ty.clone(),
                span,
            }),
            label: None,
        })),
        ty,
        span,
    }
}

/// Longest `[v; N]` lowered element by element.
const REPEAT_UNROLL: u32 = 16;

use super::collect::Intrinsic;
use super::resolve::{str_lit_of, str_targ};

/// `__mlir_attr::<"text">` (also called, `()`, or a plain string): the
/// attribute's text.
fn mlir_attr_text(e: &Expr) -> Option<String> {
    if let Some(s) = str_lit_of(e) {
        return Some(s.to_owned());
    }
    let p = match &e.kind {
        ExprKind::Path(p) => p,
        ExprKind::Call { callee, args, .. } if args.is_empty() => match &callee.kind {
            ExprKind::Path(p) => p,
            _ => return None,
        },
        _ => return None,
    };
    let last = p.segments.last()?;
    match (last.name.as_str(), last.generics.as_slice()) {
        ("__mlir_attr", [t]) => str_targ(t).map(str::to_owned),
        _ => None,
    }
}

/// `attributes = { "name": __mlir_attr::<"text">, .. }`: its pairs.
fn mlir_attrs(e: &Expr) -> Option<Vec<(String, String)>> {
    let pair = |k: &Expr, v: &Expr| Some((str_lit_of(k)?.to_owned(), mlir_attr_text(v)?));
    match &e.kind {
        ExprKind::Dict(pairs) => pairs.iter().map(|(k, v)| pair(k, v)).collect(),
        // `{ k: v }` matched by the `Dict` literal handler: its one
        // repetition of `(k, v)` items.
        ExprKind::MagicLiteral(cands) => cands.iter().find_map(|c| match c.args.as_slice() {
            [rokc_ast::LitArg::Many(items)] => items
                .iter()
                .map(|it| match &it.kind {
                    ExprKind::Tuple(kv) if kv.len() == 2 => pair(&kv[0], &kv[1]),
                    _ => None,
                })
                .collect(),
            _ => None,
        }),
        ExprKind::Block(b) if b.stmts.is_empty() && b.trailing.is_none() => Some(Vec::new()),
        _ => None,
    }
}

/// `regions = [|a: A, ..| -> R { ..; terminator }, ..]` (bare 1.9): each
/// closure is a region lowered inline; parameters are the entry block's
/// arguments, captures plain uses (regions see values above them). The body
/// ends in its terminator op; a trailing value isn't evaluated.
fn lower_mlir_regions(ctx: &mut LowerCtx<'_>, e: &Expr) -> Vec<(Vec<LocalId>, HirExpr)> {
    let items: &[Expr] = match &e.kind {
        ExprKind::List(xs) => xs,
        ExprKind::MagicLiteral(cands) => cands
            .iter()
            .find_map(|c| match c.args.as_slice() {
                [rokc_ast::LitArg::Many(xs)] => Some(xs.as_slice()),
                _ => None,
            })
            .unwrap_or(&[]),
        _ => {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                "`regions` of `__mlir_op` is a list of closures `[|args| { .. }, ..]` (bare 1.9)"
                    .to_owned(),
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for it in items {
        let ExprKind::Closure {
            params,
            ret_ty,
            body,
            is_async: false,
            is_generator: false,
            ..
        } = &it.kind
        else {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                "a region of `__mlir_op` is a closure `|args| { .. }` (bare 1.9)".to_owned(),
            );
            continue;
        };
        ctx.push_scope();
        let locals = params
            .iter()
            .map(|p| {
                let ty = p.ty.as_ref().map_or_else(
                    || {
                        ctx.emit_error(
                            rokc_diagnostic::ErrorKind::Type,
                            format!(
                                "region argument `{}` of `__mlir_op` needs a type (bare 1.9)",
                                p.name
                            ),
                        );
                        HirTy::Unknown
                    },
                    |t| ctx.resolve_ast_ty(t),
                );
                ctx.declare(Some(p.name.clone()), ty)
            })
            .collect();
        let ret = ret_ty
            .as_ref()
            .map_or(HirTy::Unknown, |t| ctx.resolve_ast_ty(t));
        let body = lower_expected(ctx, body, ret);
        ctx.pop_scope();
        if has_control_flow(&body) {
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                "a region of `__mlir_op` is straight-line ops (no `if` / `match` / loops / \
                 `return`; use `scf` ops) (bare 1.9)"
                    .to_owned(),
            );
        }
        out.push((locals, body));
    }
    out
}

/// Whether `e` has control flow (MIR blocks), which a region body
/// can't hold.
fn has_control_flow(e: &HirExpr) -> bool {
    let mut branchy = false;
    crate::each_expr(e, &mut |x| match &x.kind {
        HirExprKind::If { .. }
        | HirExprKind::While { .. }
        | HirExprKind::Loop { .. }
        | HirExprKind::Match { .. }
        | HirExprKind::Suspend { .. } => branchy = true,
        HirExprKind::Block(b) => {
            if b.stmts.iter().any(|s| {
                matches!(
                    s,
                    HirStmt::Return(_)
                        | HirStmt::Break { .. }
                        | HirStmt::Continue { .. }
                        | HirStmt::Panic(_)
                )
            }) {
                branchy = true;
            }
        }
        _ => {}
    });
    branchy
}

/// `__mlir_op::<"dialect.op", R>(operands.., attributes = {..}, regions =
/// [..])` (bare 1.9): one op in generic form. `R`: a rok type or
/// `__mlir_type<"..">` (`_` expected type, `()` no result).
fn lower_mlir_op(
    ctx: &mut LowerCtx<'_>,
    targs: &[Ty],
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let shape = "`__mlir_op` is called as `__mlir_op::<\"dialect.op\", R>(operands..)` (bare 1.9)";
    let (Some(op), Some(r)) = (targs.first().and_then(str_targ), targs.get(1)) else {
        discard_args(ctx, args);
        return ctx.error_expr(span, shape.to_owned());
    };
    let op = op.to_owned();
    if targs.len() != 2 || !op.contains('.') {
        discard_args(ctx, args);
        return ctx.error_expr(span, shape.to_owned());
    }
    let infer = matches!(&r.kind, rokc_ast::TyKind::Path(p)
        if p.segments.len() == 1 && p.segments[0].name == "_");
    let ty = if infer {
        ctx.current_expected().cloned().unwrap_or(HirTy::Unknown)
    } else {
        ctx.resolve_ast_ty(r)
    };
    let mut attrs = Vec::new();
    let mut regions = Vec::new();
    let mut lowered = Vec::new();
    for a in args {
        match a {
            CallArg::Positional(e) => lowered.push(lower_expected(ctx, e, HirTy::Unknown)),
            CallArg::Named { name, value } if name == "attributes" => match mlir_attrs(value) {
                Some(kv) => attrs = kv,
                None => ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    "`attributes` of `__mlir_op` is `{ \"name\": __mlir_attr::<\"text\">, .. }` \
                     (bare 1.9)"
                        .to_owned(),
                ),
            },
            CallArg::Named { name, value } if name == "regions" => {
                regions = lower_mlir_regions(ctx, value);
            }
            _ => {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    "`__mlir_op` takes operands, `attributes = {..}` and `regions = [..]` \
                     (bare 1.9)"
                        .to_owned(),
                );
            }
        }
    }
    if ty == HirTy::Unknown {
        return ctx.error_expr(
            span,
            format!("type annotations needed: the result type of `__mlir_op::<\"{op}\", _>`"),
        );
    }
    HirExpr {
        kind: HirExprKind::MlirOp {
            op,
            attrs,
            args: lowered,
            regions,
        },
        ty,
        span,
    }
}

/// A generic `#[intrinsic]` call; `size_of::<T>()` / `align_of::<T>()`
/// take `T` from `targs`.
fn lower_intrinsic(
    ctx: &mut LowerCtx<'_>,
    op: Intrinsic,
    name: &str,
    targs: &[Ty],
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    match op {
        Intrinsic::AnyCast | Intrinsic::MethodTable | Intrinsic::FieldTable | Intrinsic::Shown => {
            super::reflect::intrinsic(ctx, op, name, targs, args, span)
        }
        Intrinsic::MlirOp => lower_mlir_op(ctx, targs, args, span),
        Intrinsic::Transfer
        | Intrinsic::TransferKeep
        | Intrinsic::TransferVisit
        | Intrinsic::TransferNeeded
        | Intrinsic::TransferPlain
        | Intrinsic::TransferFlat
        | Intrinsic::HandOver => lower_transfer(ctx, op, name, targs, args, span),
        Intrinsic::MlirAttr => {
            discard_args(ctx, args);
            ctx.error_expr(
                span,
                "`__mlir_attr::<\"..\">` is only an `attributes` value of `__mlir_op` (bare 1.9)"
                    .to_owned(),
            )
        }
        Intrinsic::SizeOf | Intrinsic::AlignOf => {
            let (Some(t), true) = (targs.first(), args.is_empty() && targs.len() == 1) else {
                discard_args(ctx, args);
                return ctx.error_expr(span, format!("`{name}` is called as `{name}::<T>()`"));
            };
            HirExpr {
                kind: HirExprKind::SizeOf {
                    ty: ctx.resolve_ast_ty(t),
                    align: op == Intrinsic::AlignOf,
                },
                ty: HirTy::Prim(PrimTy::Usize),
                span,
            }
        }
        Intrinsic::PtrRead { volatile } => {
            lower_ptr_intrinsic(ctx, name, false, volatile, args, span)
        }
        Intrinsic::PtrWrite { volatile } => {
            lower_ptr_intrinsic(ctx, name, true, volatile, args, span)
        }
        Intrinsic::Atomic(atomic) => lower_atomic_intrinsic(ctx, atomic, args, span),
        Intrinsic::DropInPlace => {
            let value = lower_ptr_intrinsic(ctx, name, false, false, args, span);
            if matches!(value.ty, HirTy::Unknown) {
                return value;
            }
            let ty = value.ty.clone();
            let local = ctx.declare(None, ty.clone());
            let mut stmts = vec![HirStmt::Let { local, init: value }];
            stmts.extend(drop_glue(ctx, hir_local(local, &ty, span), span, 0));
            HirExpr {
                kind: HirExprKind::Block(Box::new(HirBlock {
                    stmts,
                    trailing: None,
                    label: None,
                })),
                ty: HirTy::Unit,
                span,
            }
        }
        // `collect::intrinsic_of` keeps primitive ops and conversions off generic
        // functions; `addr_of` is lowered at the call (`lower_addr_of`).
        Intrinsic::Prim(_) | Intrinsic::Convert | Intrinsic::AddrOf => poison(span),
        Intrinsic::BufNew
        | Intrinsic::BufGet
        | Intrinsic::BufSet
        | Intrinsic::BufSame
        | Intrinsic::BufErase
        | Intrinsic::BufCast => lower_buf_intrinsic(ctx, op, name, args, span),
    }
}

/// `rok::bare` raw memory: `raw_read(p)`, `raw_write(p, v)` (and
/// `_volatile`) on a pointer struct whose field 0 is the address.
fn lower_ptr_intrinsic(
    ctx: &mut LowerCtx<'_>,
    op: &str,
    write: bool,
    volatile: bool,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let usize_ty = HirTy::Prim(PrimTy::Usize);
    let pos: Vec<&Expr> = args
        .iter()
        .filter_map(|a| match a {
            CallArg::Positional(e) => Some(e),
            _ => None,
        })
        .collect();
    let want = if write { 2 } else { 1 };
    if pos.len() != want || args.len() != want {
        discard_args(ctx, args);
        return ctx.error_expr(span, format!("`{op}` takes {want} argument(s)"));
    }
    let p = lower_expected(ctx, pos[0], HirTy::Unknown);
    let HirTy::Struct(sid) = p.ty else {
        discard_args(ctx, &args[1..]);
        if p.ty == HirTy::Unknown {
            return p;
        }
        return ctx.error_expr(
            span,
            format!("`{op}` needs a `Ptr<T>`, found `{}`", ctx.ty_name(&p.ty)),
        );
    };
    let def = ctx.struct_def(sid);
    let (Some(ty), Some(HirTy::Prim(PrimTy::Usize))) = (
        def.generic_args.first().cloned(),
        def.fields.first().map(|f| f.ty.clone()),
    ) else {
        return ctx.error_expr(span, format!("`{op}` needs a `Ptr<T>`"));
    };
    let addr = Box::new(HirExpr {
        kind: HirExprKind::FieldRead {
            target: Box::new(p),
            struct_id: sid,
            field_index: 0,
            field_ty: usize_ty.clone(),
        },
        ty: usize_ty,
        span,
    });
    if write {
        let v = lower_expected(ctx, pos[1], ty.clone());
        let v = coerce(ctx, &ty, v, &|| "the stored value".to_owned());
        HirExpr {
            kind: HirExprKind::RawWrite {
                addr,
                value: Box::new(v),
                ty,
                volatile,
            },
            ty: HirTy::Unit,
            span,
        }
    } else {
        HirExpr {
            kind: HirExprKind::RawRead {
                addr,
                ty: ty.clone(),
                volatile,
            },
            ty,
            span,
        }
    }
}

/// A standard-library method marked `#[intrinsic("rok.addr_of")]`.
fn is_addr_intrinsic(def: &FnDef, module: &str) -> bool {
    module.starts_with("rok::")
        && def.attrs.iter().any(|a| {
            a.name() == "intrinsic"
                && matches!(&a.body, rokc_ast::AttrBody::Args(args) if args.iter().any(|x|
                    matches!(x, rokc_ast::AttrArg::Lit(rokc_ast::Literal::Str(s)) if s == "rok.addr_of")))
        })
}

/// `addr_of(place)` / `many_addr_of(place)` (bare 1.1): a pointer to
/// the place itself, not to a copy.
fn lower_addr_of(
    ctx: &mut LowerCtx<'_>,
    def: &FnDef,
    name: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let [CallArg::Positional(e)] = args else {
        discard_args(ctx, args);
        return ctx.error_expr(span, format!("`{name}` takes one place: `{name}(x)`"));
    };
    let place = lower_expected(ctx, e, HirTy::Unknown);
    // `xs[i]` on a collection is an `Index::index` call returning a copy;
    // only fixed-array elements are places.
    let target = match &e.kind {
        ExprKind::Index(target, _) => Some(target),
        _ => None,
    };
    if let Some(target) = target.filter(|_| {
        place.ty != HirTy::Unknown && !matches!(place.kind, HirExprKind::ArrayIndex { .. })
    }) {
        let target = rokc_ast::print::print_expr(target);
        let elem = ctx.ty_name(&place.ty);
        let r = ctx.error_expr(
            span,
            format!(
                "`{name}` needs a place, but `{target}[..]` is an `Index` call that returns a \
                 copy: only an element of a fixed array `[T; N]` is a place"
            ),
        );
        ctx.help_last(Some(format!(
            "a `[..]` literal is a `List` unless its slot is a fixed array \
             (`let {target}: [{elem}; N] = [..];`)"
        )));
        return r;
    }
    let pointee = place.ty.clone();
    addr_expr(ctx, place, def, name, pointee, span)
}

/// A `rok::bare::ptr` pointer struct `def` to `place` over `pointee`; the
/// place's indices are evaluated once, first.
pub(super) fn addr_expr(
    ctx: &mut LowerCtx<'_>,
    place: HirExpr,
    def: &FnDef,
    name: &str,
    pointee: HirTy,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if place.ty == HirTy::Unknown {
        return place;
    }
    let mut lets = Vec::new();
    let Some(place) = addr_place(ctx, place, &mut lets) else {
        return ctx.error_expr(
            span,
            format!(
                "`{name}` needs a place: a local variable or `static`, or a field or array \
                 element of one"
            ),
        );
    };
    let ptr = def.ret_ty.as_ref().and_then(|t| match &t.kind {
        AstTyKind::Path(p) => p.segments.last().map(|s| s.name.clone()),
        _ => None,
    });
    let Some(sid) = ptr
        .and_then(|p| ctx.instantiate_struct(&format!("rok::bare::ptr::{p}"), vec![pointee], span))
    else {
        return ctx.error_expr(
            span,
            "`rok.addr_of` returns a `rok::bare::ptr` pointer".to_owned(),
        );
    };
    let addr = HirExpr {
        kind: HirExprKind::AddrOf {
            place: Box::new(place),
        },
        ty: HirTy::Prim(PrimTy::Usize),
        span,
    };
    with_lets(
        lets,
        HirExpr {
            kind: HirExprKind::StructInit {
                struct_id: sid,
                fields: vec![addr],
            },
            ty: HirTy::Struct(sid),
            span,
        },
    )
}

/// `e` as a place rooted at a local or `static`, its non-trivial indices
/// moved into `lets`; `None` if it is not one.
fn addr_place(ctx: &mut LowerCtx<'_>, e: HirExpr, lets: &mut Vec<HirStmt>) -> Option<HirExpr> {
    let HirExpr { kind, ty, span } = e;
    let kind = match kind {
        HirExprKind::Local(_) | HirExprKind::StaticRef(_) => kind,
        HirExprKind::FieldRead {
            target,
            struct_id,
            field_index,
            field_ty,
        } if ctx.struct_def(struct_id).union_kind.is_none() => HirExprKind::FieldRead {
            target: Box::new(addr_place(ctx, *target, lets)?),
            struct_id,
            field_index,
            field_ty,
        },
        HirExprKind::TupleField {
            target,
            index,
            field_ty,
        } => HirExprKind::TupleField {
            target: Box::new(addr_place(ctx, *target, lets)?),
            index,
            field_ty,
        },
        HirExprKind::ArrayIndex {
            array,
            index,
            elem_ty,
        } => {
            let array = Box::new(addr_place(ctx, *array, lets)?);
            let index = if matches!(index.kind, HirExprKind::Local(_) | HirExprKind::Const(_)) {
                index
            } else {
                let ity = index.ty.clone();
                let local = ctx.declare(None, ity.clone());
                lets.push(HirStmt::Let {
                    local,
                    init: *index,
                });
                Box::new(hir_local(local, &ity, span))
            };
            HirExprKind::ArrayIndex {
                array,
                index,
                elem_ty,
            }
        }
        _ => return None,
    };
    Some(HirExpr { kind, ty, span })
}

/// `atomic_OP(buf, i, operands..)` over a `GcBuf<T>` of an integer or
/// `bool` `T`; yields the element's previous value.
fn lower_atomic_intrinsic(
    ctx: &mut LowerCtx<'_>,
    op: &'static str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let pos: Vec<&Expr> = args
        .iter()
        .filter_map(|a| match a {
            CallArg::Positional(e) => Some(e),
            _ => None,
        })
        .collect();
    // Value operands, then the ordering(s) (`cas`: success, failure).
    let (values, orders) = match op {
        "load" => (0, 1),
        "cas" => (2, 2),
        _ => (1, 1),
    };
    let want = 2 + values + orders;
    if pos.len() != want || args.len() != want {
        discard_args(ctx, args);
        return ctx.error_expr(span, format!("`atomic_{op}` takes {want} argument(s)"));
    }
    // The orderings must be literal `AtomicOrdering` variants: LLVM fixes
    // them per instruction.
    let mut order = ["seq_cst"; 2];
    for (k, e) in pos[2 + values..].iter().enumerate() {
        let name = match &e.kind {
            ExprKind::Path(p) => p.segments.last().map(|s| s.name.as_str()),
            _ => None,
        };
        let llvm = match name {
            Some("Relaxed") => "monotonic",
            Some("Acquire") => "acquire",
            Some("Release") => "release",
            Some("AcqRel") => "acq_rel",
            Some("SeqCst") => "seq_cst",
            _ => {
                discard_args(ctx, args);
                return ctx.error_expr(
                    span,
                    format!(
                        "`atomic_{op}`: the memory ordering must be a constant `AtomicOrdering` \
                         variant (`Relaxed`, `Acquire`, `Release`, `AcqRel`, `SeqCst`)"
                    ),
                );
            }
        };
        // A load cannot release; a failed `cas` is a load.
        if (op == "load" || k == 1) && matches!(llvm, "release" | "acq_rel") {
            discard_args(ctx, args);
            let what = if op == "load" {
                "a load"
            } else {
                "a failed compare-exchange"
            };
            return ctx.error_expr(
                span,
                format!(
                    "`atomic_{op}`: `{}` is not a valid ordering for {what}",
                    name.unwrap_or("")
                ),
            );
        }
        order[k] = llvm;
    }
    let buf = lower_expected(ctx, pos[0], HirTy::Unknown);
    if let HirTy::GcBuf(e) = &buf.ty {
        if let Some((id, word)) = transparent_word(ctx, e) {
            if !matches!(op, "load" | "xchg" | "cas") {
                discard_args(ctx, &args[1..]);
                return ctx.error_expr(
                    span,
                    format!("`atomic_{op}` needs an integer, found `{}`", ctx.ty_name(e)),
                );
            }
            return atomic_on_word(ctx, (op, order), buf, id, word, &pos[1..2 + values], span);
        }
    }
    let elem = match &buf.ty {
        HirTy::GcBuf(e) if matches!(**e, HirTy::Prim(p) if p.is_int() || p == PrimTy::Bool) => {
            (**e).clone()
        }
        // A type parameter: checked per instance.
        HirTy::GcBuf(e) if matches!(**e, HirTy::Param(_)) => (**e).clone(),
        HirTy::Unknown => {
            discard_args(ctx, &args[1..]);
            return buf;
        }
        other => {
            let msg = format!(
                "`atomic_{op}` needs a `GcBuf` of an integer or `bool`, found `{}`",
                ctx.ty_name(other)
            );
            discard_args(ctx, &args[1..]);
            return ctx.error_expr(span, msg);
        }
    };
    if elem == HirTy::Prim(PrimTy::Bool) && matches!(op, "add" | "sub") {
        discard_args(ctx, &args[1..]);
        return ctx.error_expr(
            span,
            format!("`atomic_{op}` needs an integer, found `bool`"),
        );
    }
    let usize_ty = HirTy::Prim(PrimTy::Usize);
    let i = lower_expected(ctx, pos[1], usize_ty.clone());
    let i = coerce(ctx, &usize_ty, i, &|| "the buffer index".to_owned());
    let operands = pos[2..2 + values]
        .iter()
        .map(|e| {
            let v = lower_expected(ctx, e, elem.clone());
            coerce(ctx, &elem, v, &|| "the atomic operand".to_owned())
        })
        .collect();
    HirExpr {
        ty: elem.clone(),
        kind: HirExprKind::BufAtomic {
            op,
            buf: Box::new(buf),
            index: Box::new(i),
            args: operands,
            elem,
            order,
        },
        span,
    }
}

fn lower_buf_intrinsic(
    ctx: &mut LowerCtx<'_>,
    op: Intrinsic,
    name: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let usize_ty = HirTy::Prim(PrimTy::Usize);
    let pos: Vec<&Expr> = args
        .iter()
        .filter_map(|a| match a {
            CallArg::Positional(e) => Some(e),
            _ => None,
        })
        .collect();
    let want = match op {
        Intrinsic::BufNew | Intrinsic::BufErase | Intrinsic::BufCast => 1,
        Intrinsic::BufGet | Intrinsic::BufSame => 2,
        _ => 3,
    };
    if pos.len() != want || args.len() != want {
        discard_args(ctx, args);
        return ctx.error_expr(
            span,
            format!("`{name}` takes {want} positional argument(s)"),
        );
    }
    let index = |ctx: &mut LowerCtx<'_>, e: &Expr| {
        let i = lower_expected(ctx, e, usize_ty.clone());
        coerce(ctx, &usize_ty, i, &|| "the buffer index".to_owned())
    };
    if op == Intrinsic::BufNew {
        let Some(HirTy::GcBuf(elem)) = ctx.current_expected().cloned() else {
            discard_args(ctx, args);
            return ctx.error_expr(
                span,
                format!("`{name}` needs its `GcBuf<T>` type from the context - annotate the slot"),
            );
        };
        let len = index(ctx, pos[0]);
        return HirExpr {
            ty: HirTy::GcBuf(elem.clone()),
            kind: HirExprKind::BufNew {
                elem: *elem,
                len: Box::new(len),
            },
            span,
        };
    }
    let buf = lower_expected(ctx, pos[0], HirTy::Unknown);
    let HirTy::GcBuf(elem) = buf.ty.clone() else {
        discard_args(ctx, &args[1..]);
        if buf.ty == HirTy::Unknown {
            return buf;
        }
        return ctx.error_expr(
            span,
            format!(
                "`{name}` needs a `GcBuf<T>`, found `{}`",
                ctx.ty_name(&buf.ty)
            ),
        );
    };
    let u8_buf = HirTy::GcBuf(Box::new(HirTy::Prim(PrimTy::U8)));
    if op == Intrinsic::BufCast {
        let Some(to @ HirTy::GcBuf(_)) = ctx.current_expected().cloned() else {
            return ctx.error_expr(
                span,
                format!("`{name}` needs its `GcBuf<U>` type from the context - annotate the slot"),
            );
        };
        return HirExpr {
            ty: to.clone(),
            kind: HirExprKind::Cast {
                from_ty: buf.ty.clone(),
                to_ty: to,
                operand: Box::new(buf),
            },
            span,
        };
    }
    match op {
        Intrinsic::BufSame => buf_same(ctx, buf, pos[1], span),
        // The same reference as `GcBuf<u8>`, the type of a runtime
        // parameter that takes any GC object.
        Intrinsic::BufErase => HirExpr {
            ty: u8_buf.clone(),
            kind: HirExprKind::Cast {
                from_ty: buf.ty.clone(),
                to_ty: u8_buf,
                operand: Box::new(buf),
            },
            span,
        },
        _ => buf_access(ctx, buf, *elem, pos[1], pos.get(2).copied(), span),
    }
}

/// A `#[repr(transparent)]` struct around one integer (`Ptr<T>`): its id
/// and that integer type, which atomics operate on (stdlib 2.16.2).
fn transparent_word(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<(StructId, HirTy)> {
    let HirTy::Struct(id) = ty else { return None };
    let def = ctx.struct_def(*id);
    match def.fields.as_slice() {
        [f] if def.repr.transparent && matches!(f.ty, HirTy::Prim(p) if p.is_int()) => {
            Some((*id, f.ty.clone()))
        }
        _ => None,
    }
}

/// `atomic_OP` on a `GcBuf` of a transparent word struct: operate on it as
/// `GcBuf<word>`, unwrapping and rewrapping.
fn atomic_on_word(
    ctx: &mut LowerCtx<'_>,
    (op, order): (&'static str, [&'static str; 2]),
    buf: HirExpr,
    id: StructId,
    word: HirTy,
    rest: &[&Expr],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let elem = HirTy::Struct(id);
    let word_buf = HirTy::GcBuf(Box::new(word.clone()));
    let buf = HirExpr {
        ty: word_buf.clone(),
        kind: HirExprKind::Cast {
            from_ty: buf.ty.clone(),
            to_ty: word_buf,
            operand: Box::new(buf),
        },
        span,
    };
    let usize_ty = HirTy::Prim(PrimTy::Usize);
    let i = lower_expected(ctx, rest[0], usize_ty.clone());
    let i = coerce(ctx, &usize_ty, i, &|| "the buffer index".to_owned());
    let operands = rest[1..]
        .iter()
        .map(|e| {
            let v = lower_expected(ctx, e, elem.clone());
            let v = coerce(ctx, &elem, v, &|| "the atomic operand".to_owned());
            HirExpr {
                ty: word.clone(),
                kind: HirExprKind::FieldRead {
                    target: Box::new(v),
                    struct_id: id,
                    field_index: 0,
                    field_ty: word.clone(),
                },
                span,
            }
        })
        .collect();
    let raw = HirExpr {
        ty: word.clone(),
        kind: HirExprKind::BufAtomic {
            op,
            buf: Box::new(buf),
            index: Box::new(i),
            args: operands,
            elem: word,
            order,
        },
        span,
    };
    HirExpr {
        ty: elem,
        kind: HirExprKind::StructInit {
            struct_id: id,
            fields: vec![raw],
        },
        span,
    }
}

/// `buf_same(a, b)`: the addresses are compared where both are loaded, so
/// nothing can move in between and nothing is pinned.
fn buf_same(ctx: &mut LowerCtx<'_>, a: HirExpr, b: &Expr, span: rokc_diagnostic::Span) -> HirExpr {
    let b = lower_expected(ctx, b, a.ty.clone());
    let b = coerce(ctx, &a.ty, b, &|| "the compared buffer".to_owned());
    let usize_ty = HirTy::Prim(PrimTy::Usize);
    let addr = |e: HirExpr| HirExpr {
        ty: usize_ty.clone(),
        kind: HirExprKind::Cast {
            from_ty: e.ty.clone(),
            to_ty: usize_ty.clone(),
            operand: Box::new(e),
        },
        span,
    };
    HirExpr {
        ty: HirTy::Prim(PrimTy::Bool),
        kind: HirExprKind::PrimBinOp {
            op: PrimOp::Eq,
            lhs: Box::new(addr(a)),
            rhs: Box::new(addr(b)),
        },
        span,
    }
}

/// `buf[i]` / `buf[i] = value` on a `GcBuf<T>`: the `rok.buf_get` /
/// `rok.buf_set` primitive, for the intrinsics and `Index` / `IndexSet`.
fn buf_access(
    ctx: &mut LowerCtx<'_>,
    buf: HirExpr,
    elem: HirTy,
    idx: &Expr,
    value: Option<&Expr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    // No bounds check: an index past the allocation reads / writes
    // arbitrary memory.
    if ctx.unsafe_depth == 0 {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            "indexing a `GcBuf` is unchecked and needs an `unsafe` block or `unsafe fn` \
             (spec 4.15)"
                .to_owned(),
        );
    }
    let usize_ty = HirTy::Prim(PrimTy::Usize);
    let i = lower_expected(ctx, idx, usize_ty.clone());
    let i = coerce(ctx, &usize_ty, i, &|| "the buffer index".to_owned());
    let Some(value) = value else {
        return HirExpr {
            ty: elem.clone(),
            kind: HirExprKind::BufGet {
                buf: Box::new(buf),
                index: Box::new(i),
                elem,
            },
            span,
        };
    };
    let v = lower_expected(ctx, value, elem.clone());
    let v = coerce(ctx, &elem, v, &|| "the stored element".to_owned());
    HirExpr {
        ty: HirTy::Unit,
        kind: HirExprKind::BufSet {
            buf: Box::new(buf),
            index: Box::new(i),
            value: Box::new(v),
            elem,
        },
        span,
    }
}

/// Lower call arguments only for their diagnostics (the call itself
/// failed to resolve).
pub(super) fn discard_args(ctx: &mut LowerCtx<'_>, args: &[CallArg]) {
    for a in args {
        let (CallArg::Positional(e)
        | CallArg::Named { value: e, .. }
        | CallArg::Spread(e)
        | CallArg::Ref(e)) = a;
        lower_expected(ctx, e, HirTy::Unknown);
    }
}

pub(super) fn is_arith_op(op: &str) -> bool {
    matches!(
        op,
        "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>"
    )
}

pub(super) fn lower_infix(
    ctx: &mut LowerCtx<'_>,
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    // Spec 3.2 / 3.1: `&&` and `||` short-circuit. Desugar before
    // lowering rhs so that side effects in rhs only run when needed.
    if op == "&&" || op == "||" {
        return lower_short_circuit(ctx, op, lhs, rhs, span);
    }
    // Arithmetic operands share the result's type (so `let x: i64 = 1 + 2`
    // types both literals as i64); comparison operands don't.
    let lhs_expected = if is_arith_op(op) {
        ctx.current_expected().cloned().unwrap_or(HirTy::Unknown)
    } else {
        HirTy::Unknown
    };
    let (lhs_h, rhs_h) = lower_sibling_pair(ctx, lhs, rhs, lhs_expected);
    if lhs_h.ty == HirTy::Unknown || rhs_h.ty == HirTy::Unknown {
        return poison(span);
    }
    let since = ctx.env.sink.diagnostics.borrow().len();
    let tys = (lhs_h.ty.clone(), rhs_h.ty.clone());
    let e = lower_infix_typed(ctx, op, lhs_h, rhs_h, rhs, span);
    if ctx.env.sink.diagnostics.borrow().len() > since {
        if let Some(help) = operand_help(ctx, op, (lhs, &tys.0), (rhs, &tys.1)) {
            ctx.help_since(since, help);
        }
    }
    e
}

/// A fix for `lhs op rhs` whose operand types don't fit: convert, unwrap,
/// await or format the user's operands.
fn operand_help(
    ctx: &LowerCtx<'_>,
    op: &str,
    lhs: (&Expr, &HirTy),
    rhs: (&Expr, &HirTy),
) -> Option<String> {
    let src = |e: &Expr| ctx.src(e.span).map(str::to_owned);
    let (l, r) = (src(lhs.0)?, src(rhs.0)?);
    let numeric = |t: &HirTy| match t {
        HirTy::Prim(p) => p.is_int() || p.is_float(),
        _ => super::bignum::big_of(ctx, t).is_some(),
    };
    let is_str = |t: &HirTy| *t == HirTy::Prim(PrimTy::Str);
    let optional = |t: &HirTy| match t {
        HirTy::Sum(vs) if vs.contains(&HirTy::None) => {
            let rest: Vec<&HirTy> = vs.iter().filter(|v| **v != HirTy::None).collect();
            Some(match rest[..] {
                [one] => one.clone(),
                _ => HirTy::Sum(rest.into_iter().cloned().collect()),
            })
        }
        _ => None,
    };
    let future = |t: &HirTy| {
        matches!(t, HirTy::Struct(_)) && matches!(pick_method(ctx, t, "poll", None), Ok(Some(_)))
    };
    if op == "+" && (is_str(lhs.1) != is_str(rhs.1)) {
        // A plain literal's text goes in as is; anything else as `{..}`.
        let piece = |e: &Expr, s: &str| match &e.kind {
            ExprKind::Literal(AstLit::Str(_))
                if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 =>
            {
                s[1..s.len() - 1].replace('{', "{{").replace('}', "}}")
            }
            _ => format!("{{{s}}}"),
        };
        let (a, b) = (piece(lhs.0, &l), piece(rhs.0, &r));
        return Some(format!(
            "`+` joins two `str`s; put the value in an f-string: `f\"{a}{b}\"`"
        ));
    }
    for (side, t) in [(&l, lhs.1), (&r, rhs.1)] {
        if let Some(inner) = optional(t) {
            let inner_name = ctx.ty_name(&inner);
            let default = match inner {
                HirTy::Prim(p) if p.is_float() => "0.0",
                HirTy::Prim(p) if p.is_int() => "0",
                HirTy::Prim(PrimTy::Str) => "\"\"",
                _ => "..",
            };
            return Some(format!(
                "`{side}` may be `none`; unwrap it first: `{side} ?? {default}`, `if let n: \
                 {inner_name} = {side} {{ .. }}`, or `{side}?`"
            ));
        }
        if future(t) {
            return Some(format!(
                "`{side}` is a future, not its value; did you forget `await`? `await {side}`"
            ));
        }
    }
    if numeric(lhs.1) && numeric(rhs.1) && lhs.1 != rhs.1 {
        return Some(format!(
            "rok never converts numbers implicitly; convert one side: `{l} {op} ({r} as {})`",
            ctx.ty_name(lhs.1)
        ));
    }
    None
}

fn lower_infix_typed(
    ctx: &mut LowerCtx<'_>,
    op: &str,
    lhs_h: HirExpr,
    rhs_h: HirExpr,
    rhs: &Expr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if matches!(lhs_h.ty, HirTy::AnyTrait(_)) || matches!(rhs_h.ty, HirTy::AnyTrait(_)) {
        return dyn_op(ctx, op, lhs_h, Some(rhs_h), span);
    }
    // Spec 6.7.2 `swap`: `a ∈ s` is `s.contains(a)`. Operands still
    // evaluate left to right: `a` goes to a temp first.
    if ctx
        .env
        .prec
        .lookup(op, Fixity::Infix)
        .is_some_and(|e| e.swap)
    {
        let ty = lhs_h.ty.clone();
        let tmp = ctx.declare(None, ty.clone());
        let call = dispatch_user_op(
            ctx,
            op,
            Fixity::Infix,
            rhs_h,
            Some(hir_local(tmp, &ty, span)),
            span,
        );
        let call_ty = call.ty.clone();
        return HirExpr {
            kind: HirExprKind::Block(Box::new(HirBlock {
                stmts: vec![HirStmt::Let {
                    local: tmp,
                    init: lhs_h,
                }],
                trailing: Some(call),
                label: None,
            })),
            ty: call_ty,
            span,
        };
    }
    // Spec 0.4 / 3.1: on user types, `str`, tuples / arrays (core's
    // component-wise impls) and with any Unicode math operator (spec
    // 1.10.3, primitives too) the operator is its trait's method.
    if matches!(
        lhs_h.ty,
        HirTy::Struct(_)
            | HirTy::Enum(_)
            | HirTy::Prim(PrimTy::Str)
            | HirTy::Tuple(_)
            | HirTy::Array(..)
            | HirTy::Param(_)
    ) || !op.is_ascii()
    {
        return dispatch_user_op_lit(ctx, op, Fixity::Infix, lhs_h, Some(rhs_h), Some(rhs), span);
    }
    // A primitive on the left of a user type (`impl Mul<Duration> for f64`),
    // or an operator with no primitive lowering.
    if let HirTy::Prim(lp) = lhs_h.ty {
        if matches!(rhs_h.ty, HirTy::Struct(_) | HirTy::Enum(_)) || infix_for_prim(op, lp).is_none()
        {
            return dispatch_user_op_lit(
                ctx,
                op,
                Fixity::Infix,
                lhs_h,
                Some(rhs_h),
                Some(rhs),
                span,
            );
        }
    }
    // Comparisons on a sum dispatch on the variant (`sum_structural_call`).
    if let HirTy::Sum(_) = lhs_h.ty {
        let method = match op {
            "==" => Some("eq"),
            "!=" => Some("ne"),
            "<" => Some("lt"),
            ">" => Some("gt"),
            "<=" => Some("le"),
            ">=" => Some("ge"),
            _ => None,
        };
        let key = match method {
            Some("eq" | "ne") => ctx.lang("eq_trait"),
            _ => ctx.lang("cmp_trait"),
        };
        if let Some(m) = method.filter(|_| ctx.has_trait(&lhs_h.ty, key)) {
            return sum_structural_call(ctx, lhs_h, m, &[], Some(rhs_h), span);
        }
    }
    if let (HirTy::Prim(lp), HirTy::Prim(rp)) = (&lhs_h.ty, &rhs_h.ty) {
        if lp == rp {
            if let Some((primop, result_ty)) = infix_for_prim(op, *lp) {
                let e = hir_binop(primop, lhs_h, rhs_h, result_ty, span);
                check_const_overflow(ctx, &e.kind, &e.ty, span);
                return e;
            }
        }
    }
    ctx.error_expr(
        span,
        format!(
            "operator `{op}` cannot be applied to `{}` and `{}`",
            ctx.ty_name(&lhs_h.ty),
            ctx.ty_name(&rhs_h.ty)
        ),
    )
}

/// Spec 2.2.3 / 808: an operator on `any` switches on the tag over the
/// primitives that have it (same type both sides; a static operand fixes
/// it). Arithmetic results are boxed back into `any`, comparisons give
/// `bool`; a type without the operator panics.
fn dyn_op(
    ctx: &mut LowerCtx<'_>,
    op: &str,
    lhs: HirExpr,
    rhs: Option<HirExpr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    use PrimTy as P;
    const PRIMS: [PrimTy; 15] = [
        P::I8,
        P::I16,
        P::I32,
        P::I64,
        P::Isize,
        P::U8,
        P::U16,
        P::U32,
        P::U64,
        P::Usize,
        P::F32,
        P::F64,
        P::Bool,
        P::CChar,
        P::Char,
    ];
    let is_dyn = |t: &HirTy| matches!(t, HirTy::AnyTrait(_));
    let fixed = [Some(&lhs.ty), rhs.as_ref().map(|r| &r.ty)]
        .into_iter()
        .flatten()
        .find(|t| !is_dyn(t))
        .cloned();
    let lookup = |p: PrimTy| {
        if rhs.is_some() {
            infix_for_prim(op, p)
        } else {
            prefix_for_prim(op, p)
        }
    };
    let cands: Vec<(PrimTy, PrimOp, HirTy)> = PRIMS
        .into_iter()
        .filter(|p| fixed.as_ref().is_none_or(|f| *f == HirTy::Prim(*p)))
        .filter_map(|p| lookup(p).map(|(o, t)| (p, o, t)))
        .collect();
    if cands.is_empty() {
        let tys: Vec<String> = [Some(&lhs.ty), rhs.as_ref().map(|r| &r.ty)]
            .into_iter()
            .flatten()
            .map(|t| format!("`{}`", ctx.ty_name(t)))
            .collect();
        return ctx.error_expr(
            span,
            format!(
                "operator `{op}` cannot be applied to {} - on an `any` value it dispatches \
                 only over primitive types (narrow it with `is` first)",
                tys.join(" and ")
            ),
        );
    }
    let bool_ty = HirTy::Prim(PrimTy::Bool);
    let result_ty = if cands.iter().all(|c| c.2 == bool_ty) {
        bool_ty
    } else {
        HirTy::AnyTrait(Vec::new())
    };
    // Operands are evaluated once, left to right.
    let mut lets = Vec::new();
    let mut temp = |ctx: &mut LowerCtx<'_>, e: HirExpr| {
        let ty = e.ty.clone();
        let l = ctx.declare(None, ty.clone());
        lets.push(HirStmt::Let { local: l, init: e });
        hir_local(l, &ty, span)
    };
    let l = temp(ctx, lhs);
    let r = rhs.map(|r| temp(ctx, r));
    let panic_arm = |ctx: &LowerCtx<'_>| HirMatchArm {
        pat: HirPattern::Wildcard,
        guard: None,
        body: HirExpr {
            kind: HirExprKind::Block(Box::new(HirBlock {
                stmts: vec![HirStmt::Panic(super::throws::hook_call(
                    ctx,
                    "rok_panic_at",
                    &format!(
                        "operator `{op}` is not defined for the runtime type of an `any` value"
                    ),
                    span,
                ))],
                trailing: None,
                label: None,
            })),
            ty: result_ty.clone(),
            span,
        },
    };
    // `match v { x: P => body(x), _ => panic }`, or `v` itself if static.
    let narrow = |ctx: &mut LowerCtx<'_>,
                  v: &HirExpr,
                  p: PrimTy,
                  body: &dyn Fn(&mut LowerCtx<'_>, HirExpr) -> HirExpr| {
        let pty = HirTy::Prim(p);
        if !is_dyn(&v.ty) {
            return body(ctx, v.clone());
        }
        let x = ctx.declare(None, pty.clone());
        let arm = HirMatchArm {
            pat: HirPattern::SumVariant {
                sum_ty: v.ty.clone(),
                variant_index: ctx.any_tag(&pty),
                binder: Some(x),
                payload_ty: Some(pty.clone()),
            },
            guard: None,
            body: body(ctx, hir_local(x, &pty, span)),
        };
        HirExpr {
            kind: HirExprKind::Match {
                scrutinee: Box::new(v.clone()),
                arms: vec![arm, panic_arm(ctx)],
            },
            ty: result_ty.clone(),
            span,
        }
    };
    let mut arms = Vec::new();
    for (p, primop, ty) in cands {
        let finish = |ctx: &mut LowerCtx<'_>, e: HirExpr| {
            coerce(ctx, &result_ty, e, &|| format!("the result of `{op}`"))
        };
        let pty = HirTy::Prim(p);
        let x = ctx.declare(None, pty.clone());
        let xv = hir_local(x, &pty, span);
        let body = if let Some(r) = &r {
            narrow(ctx, r, p, &|ctx, y| {
                finish(ctx, hir_binop(primop, xv.clone(), y, ty.clone(), span))
            })
        } else {
            let e = HirExpr {
                kind: HirExprKind::PrimUnaryOp {
                    op: primop,
                    operand: Box::new(xv.clone()),
                },
                ty: ty.clone(),
                span,
            };
            finish(ctx, e)
        };
        if !is_dyn(&l.ty) {
            // Static lhs: exactly one candidate; bind it plainly.
            lets.push(HirStmt::Let { local: x, init: l });
            return with_lets(lets, body);
        }
        arms.push(HirMatchArm {
            pat: HirPattern::SumVariant {
                sum_ty: l.ty.clone(),
                variant_index: ctx.any_tag(&pty),
                binder: Some(x),
                payload_ty: Some(pty),
            },
            guard: None,
            body,
        });
    }
    arms.push(panic_arm(ctx));
    with_lets(
        lets,
        HirExpr {
            kind: HirExprKind::Match {
                scrutinee: Box::new(l),
                arms,
            },
            ty: result_ty,
            span,
        },
    )
}

pub(super) fn lower_short_circuit(
    ctx: &mut LowerCtx<'_>,
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let bool_ty = HirTy::Prim(PrimTy::Bool);
    let lhs_h = lower_expected(ctx, lhs, bool_ty.clone());
    // A user `LogicalAnd` / `LogicalOr` impl: a plain call, both operands
    // evaluated (stdlib 2.3.9).
    if matches!(lhs_h.ty, HirTy::Struct(_) | HirTy::Enum(_)) {
        let rhs_h = lower_expr(ctx, rhs);
        return dispatch_user_op(ctx, op, Fixity::Infix, lhs_h, Some(rhs_h), span);
    }
    let lhs_h = coerce(ctx, &bool_ty, lhs_h, &|| "the condition".to_owned());
    // `x is T && rhs`: `x` is `T` in `rhs` (spec 2.2.6).
    let rhs_h = if op == "&&" {
        ctx.push_scope();
        let lets = bind_narrowed(ctx, lhs);
        let r = lower_cond(ctx, rhs);
        ctx.pop_scope();
        with_lets(lets, r)
    } else {
        lower_cond(ctx, rhs)
    };
    let mk_const_bool = |b: bool| HirExpr {
        kind: HirExprKind::Const(ConstVal::Bool(b)),
        ty: bool_ty.clone(),
        span,
    };
    let mk_block = |trailing: HirExpr| HirBlock {
        stmts: Vec::new(),
        trailing: Some(trailing),
        label: None,
    };
    //   a && b  →  if a { b } else { false }
    //   a || b  →  if a { true } else { b }
    let (then_expr, else_expr) = if op == "&&" {
        (rhs_h, mk_const_bool(false))
    } else {
        (mk_const_bool(true), rhs_h)
    };
    HirExpr {
        kind: HirExprKind::If {
            cond: Box::new(lhs_h),
            then_block: Box::new(mk_block(then_expr)),
            else_branch: Some(Box::new(else_expr)),
        },
        ty: bool_ty,
        span,
    }
}

pub(super) fn lower_prefix(
    ctx: &mut LowerCtx<'_>,
    op: &str,
    operand: &Expr,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    // `-5` / `-1.5`: a negative literal (range-checked as negative).
    if op == "-" {
        if let ExprKind::Literal(lit @ (AstLit::Int(_) | AstLit::Float(_))) = &operand.kind {
            return super::bignum::literal(ctx, lit, true, span)
                .unwrap_or_else(|| lower_literal(ctx, lit, true, span));
        }
    }
    let operand_h = lower_expr(ctx, operand);
    if operand_h.ty == HirTy::Unknown {
        return operand_h;
    }
    if matches!(
        operand_h.ty,
        HirTy::Struct(_) | HirTy::Enum(_) | HirTy::Param(_)
    ) || !op.is_ascii()
    {
        return dispatch_user_op(ctx, op, Fixity::Prefix, operand_h, None, span);
    }
    if matches!(operand_h.ty, HirTy::AnyTrait(_)) {
        return dyn_op(ctx, op, operand_h, None, span);
    }
    if let HirTy::Prim(p) = &operand_h.ty {
        if let Some((primop, result_ty)) = prefix_for_prim(op, *p) {
            return HirExpr {
                kind: HirExprKind::PrimUnaryOp {
                    op: primop,
                    operand: Box::new(operand_h),
                },
                ty: result_ty,
                span,
            };
        }
    }
    ctx.error_expr(
        span,
        format!(
            "operator `{op}` cannot be applied to `{}`",
            ctx.ty_name(&operand_h.ty)
        ),
    )
}

/// An operator on a user type calls its trait method (spec 0.4: `a + b` ≡
/// `Add::add(a, b)`); only from a trait impl, not a same-named inherent
/// method.
pub(super) fn dispatch_user_op(
    ctx: &mut LowerCtx<'_>,
    sym: &str,
    fixity: Fixity,
    lhs: HirExpr,
    rhs: Option<HirExpr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    dispatch_user_op_lit(ctx, sym, fixity, lhs, rhs, None, span)
}

/// `dispatch_user_op`; `lit_rhs`: the right operand's source when it may
/// be an unsuffixed literal, free to take the impl's `Rhs` type.
pub(super) fn dispatch_user_op_lit(
    ctx: &mut LowerCtx<'_>,
    sym: &str,
    fixity: Fixity,
    lhs: HirExpr,
    rhs: Option<HirExpr>,
    lit_rhs: Option<&Expr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let ty_name = ctx.ty_name(&lhs.ty);
    let Some(entry) = ctx.env.prec.lookup(sym, fixity) else {
        return ctx.error_expr(
            span,
            format!("operator `{sym}` cannot be applied to `{ty_name}`"),
        );
    };
    let method = entry.method_name.clone();
    let op_trait = &entry.trait_name;
    // Spec 6.7.8: the binding works only where its trait is visible.
    let names = &ctx.env.names;
    let cur = &ctx.current_module;
    let visible = names
        .type_vis(op_trait)
        .is_none_or(|v| super::resolve::visible_from(cur, &module_of(op_trait), v))
        && names.hidden(cur, op_trait).is_none();
    if !visible {
        return ctx.error_expr(
            span,
            format!(
                "operator `{sym}` needs trait `{op_trait}`, which is not visible here (spec \
                 6.7.8)"
            ),
        );
    }
    // The impl must be of the trait that registered the operator (by its
    // full path), not another trait that happens to share its name.
    let is_op_trait = |t: &str| t == op_trait;
    let shown = leaf_name(op_trait);
    if let HirTy::Param(p) = lhs.ty {
        return param_op(ctx, p, sym, &method, op_trait, lhs, rhs, lit_rhs, span);
    }
    let impls: Vec<FunctionId> = ctx
        .env
        .methods_on(&lhs.ty, &method)
        .map(|ids| {
            ids.iter()
                .copied()
                .filter(|id| {
                    ctx.env
                        .method_info(*id)
                        .and_then(|i| i.trait_name.as_deref())
                        .is_some_and(is_op_trait)
                })
                .collect()
        })
        .unwrap_or_default();
    // Several impls (`Sub<Duration>` and `Sub<Instant>` for `Instant`): the
    // one whose `Rhs` is the right operand's type, else the one a literal
    // fits (`pick_literal_impl`), else the first.
    let rhs_param = |ctx: &LowerCtx<'_>, id: FunctionId| {
        ctx.sig_by_id(id)
            .param_tys
            .get(1)
            .cloned()
            .unwrap_or(HirTy::Unknown)
    };
    let exact = rhs
        .as_ref()
        .and_then(|r| impls.iter().copied().find(|id| rhs_param(ctx, *id) == r.ty));
    let mut rhs = rhs;
    let mut lit_pick = None;
    if let (None, Some(e)) = (exact, lit_rhs) {
        let params: Vec<HirTy> = impls.iter().map(|id| rhs_param(ctx, *id)).collect();
        match pick_literal_impl(ctx, e, &params) {
            Ok(Some(i)) => {
                rhs = Some(lower_expected(ctx, e, params[i].clone()));
                lit_pick = Some(impls[i]);
            }
            Ok(None) => {}
            Err(fits) => {
                return ctx.error_expr(
                    span,
                    format!(
                        "operator `{sym}` on `{ty_name}`: the literal fits several \
                         `{shown}` impls ({fits}); add a suffix or `as`"
                    ),
                );
            }
        }
    }
    let fn_id = exact.or(lit_pick).or_else(|| impls.first().copied());
    let Some(fn_id) = fn_id else {
        // `impl<T> Add for Pair<T>` (spec 2.9.4): monomorphize on the
        // operand types.
        let gm = if rhs.is_some() {
            generic_op_by_rhs(ctx, &lhs.ty, rhs.as_ref(), lit_rhs, &method)
        } else {
            generic_trait_method_for(ctx, &lhs.ty, &method)
                .ok()
                .flatten()
        };
        if let Some(gm) = gm {
            if gm.info.trait_name.as_deref().is_some_and(is_op_trait) {
                let mut args = vec![lhs];
                args.extend(rhs);
                let Some((callee, ty)) = monomorphize(ctx, &gm.def, Some(&gm.self_ty), &args, span)
                else {
                    return poison(span);
                };
                return HirExpr {
                    kind: HirExprKind::Call { callee, args },
                    ty,
                    span,
                };
            }
        }
        return ctx.error_expr(
            span,
            format!(
                "operator `{sym}` cannot be applied to `{ty_name}`: it needs \
                 `impl {shown} for {ty_name}` (`{method}` from any other trait or an \
                 inherent `fn {method}` is not enough)"
            ),
        );
    };
    let sig = ctx.sig_by_id(fn_id);
    let expected_arity = if rhs.is_some() { 2 } else { 1 };
    if sig.param_tys.len() != expected_arity {
        return ctx.error_expr(
            span,
            format!(
                "`{method}` for `{ty_name}` must take {expected_arity} parameter(s) to implement \
                 operator `{sym}`"
            ),
        );
    }
    let mut args = vec![lhs];
    if let Some(r) = rhs {
        let pty = sig.param_tys[1].clone();
        args.push(coerce(ctx, &pty, r, &|| {
            format!("the right operand of `{sym}`")
        }));
    }
    HirExpr {
        kind: HirExprKind::Call {
            callee: fn_id,
            args,
        },
        ty: sig.ret_ty,
        span,
    }
}

/// Operator `sym` on a type parameter (spec 2.10.9): the bound on the
/// operator's trait gives the method, and its `Rhs` types the right side.
#[allow(clippy::too_many_arguments)]
fn param_op(
    ctx: &mut LowerCtx<'_>,
    p: u32,
    sym: &str,
    method: &str,
    op_trait: &str,
    lhs: HirExpr,
    rhs: Option<HirExpr>,
    lit_rhs: Option<&Expr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let t = ctx.ty_name(&lhs.ty);
    let bounds = super::bounds::of_trait(ctx, p, op_trait);
    let gm = super::bounds::bound_methods(ctx, p, method, Some(op_trait));
    let (false, [gm]) = (bounds.is_empty(), gm.as_slice()) else {
        return ctx.error_expr(
            span,
            format!(
                "operator `{sym}` on `{t}` needs a `{}` bound on `{t}` (spec 2.10.9)",
                leaf_name(op_trait)
            ),
        );
    };
    let mut args = vec![lhs];
    if let Some(r) = rhs {
        // The bound whose `Rhs` is the right operand's type, else the
        // one an unsuffixed literal takes (spec 3.1.3).
        let rhs_of = |b: &TraitRef| b.args.first().cloned().unwrap_or(HirTy::Unknown);
        let params: Vec<HirTy> = bounds.iter().map(rhs_of).collect();
        let lit = lit_rhs.filter(|e| is_untyped_literal(e));
        let want = if params.contains(&r.ty) || bounds.len() == 1 {
            params
                .iter()
                .find(|w| **w == r.ty)
                .unwrap_or(&params[0])
                .clone()
        } else {
            match lit.map(|e| pick_literal_impl(ctx, e, &params)) {
                Some(Ok(Some(i))) => params[i].clone(),
                Some(Err(fits)) => {
                    return ctx.error_expr(
                        span,
                        format!(
                            "operator `{sym}` on `{t}`: the literal fits several `{}` bounds \
                             ({fits}); add a suffix or `as` (spec 3.1.3)",
                            leaf_name(op_trait)
                        ),
                    );
                }
                _ => {
                    let all: Vec<String> = bounds
                        .iter()
                        .map(|b| format!("`{}`", b.render(leaf_name(op_trait), |t| ctx.ty_name(t))))
                        .collect();
                    return ctx.error_expr(
                        span,
                        format!(
                            "operator `{sym}` on `{t}` needs a bound `{}<{}>`, but `{t}` has {} \
                             (spec 3.1.3)",
                            leaf_name(op_trait),
                            ctx.ty_name(&r.ty),
                            all.join(" and ")
                        ),
                    );
                }
            }
        };
        let r = match lit {
            Some(e) => lower_expected(ctx, e, want.clone()),
            None => r,
        };
        args.push(coerce(ctx, &want, r, &|| {
            format!("the right operand of `{sym}`")
        }));
    }
    let self_ty = args[0].ty.clone();
    let Some((callee, ty)) =
        monomorphize_on(ctx, &gm.def, Some(&gm.self_ty), Some(&self_ty), &args, span)
    else {
        return poison(span);
    };
    HirExpr {
        kind: HirExprKind::Call { callee, args },
        ty,
        span,
    }
}

/// The generic operator impl covering the left operand whose `Rhs` (under
/// the left's binding) is the right operand's type, else one it coerces
/// to, else for a literal a primitive it fits.
fn generic_op_by_rhs<'e>(
    ctx: &LowerCtx<'e>,
    lhs: &HirTy,
    rhs: Option<&HirExpr>,
    lit_rhs: Option<&Expr>,
    method: &str,
) -> Option<&'e GenericMethod> {
    let rhs = rhs?;
    let mut exact = Vec::new();
    let mut loose = Vec::new();
    let mut lit = Vec::new();
    for (gi, mut subst) in ctx.generic_impls(lhs, super::Pick::Method(method), 0) {
        let gm = &gi.methods[method];
        // No written `Rhs`: the operator traits' default, `Self`.
        let want = match gi.trait_args.first() {
            Some(arg) => {
                // Parameters only `Rhs` names (`impl<A, B, C> Compose<fn(A)
                // -> B> for fn(B) -> C`) bind from the right operand.
                let open: std::collections::HashSet<String> = gi
                    .generics
                    .params
                    .iter()
                    .filter_map(|gp| match gp {
                        rokc_ast::GenericParam::Type { name, .. }
                        | rokc_ast::GenericParam::Const { name, .. } => {
                            (!subst.contains_key(name)).then(|| name.clone())
                        }
                    })
                    .collect();
                if !open.is_empty() {
                    unify_param_ty_with(arg, &rhs.ty, &open, &mut subst, ctx);
                }
                let mut rcx = LowerCtx::new(ctx.env, &subst, None, ctx.here());
                rcx.current_module.clone_from(&gi.module);
                rcx.resolve_ast_ty(arg)
            }
            None => lhs.clone(),
        };
        if want == rhs.ty {
            exact.push(gm);
        } else if coercible(ctx, &want, &rhs.ty) {
            loose.push(gm);
        } else if let (HirTy::Prim(_), Some(v)) = (&want, lit_rhs.and_then(untyped_lit_value)) {
            if literal_fits(ctx, v, &want) {
                lit.push(gm);
            }
        }
    }
    match (exact.as_slice(), loose.as_slice(), lit.as_slice()) {
        ([gm], ..) | ([], [gm], _) | ([], [], [gm]) => Some(*gm),
        _ => None,
    }
}

/// Primitive op and result type for `sym` on `prim`; `None` if invalid.
pub(super) fn infix_for_prim(sym: &str, prim: PrimTy) -> Option<(PrimOp, HirTy)> {
    let bool_ty = HirTy::Prim(PrimTy::Bool);
    let same_ty = HirTy::Prim(prim);
    let arith = |op: PrimOp| Some((op, same_ty.clone()));
    let cmp = |op: PrimOp| Some((op, bool_ty.clone()));
    let num = prim.is_int() || prim.is_float();
    match sym {
        "+" if num => arith(PrimOp::Add),
        "-" if num => arith(PrimOp::Sub),
        "*" if num => arith(PrimOp::Mul),
        "/" if num => arith(PrimOp::Div),
        "%" if num => arith(PrimOp::Rem),
        "%%" if num => arith(PrimOp::Mod),
        "&" if prim.is_int() || matches!(prim, PrimTy::Bool) => arith(PrimOp::BitAnd),
        "|" if prim.is_int() || matches!(prim, PrimTy::Bool) => arith(PrimOp::BitOr),
        "^" if prim.is_int() || matches!(prim, PrimTy::Bool) => arith(PrimOp::BitXor),
        "<<" if prim.is_int() => arith(PrimOp::Shl),
        ">>" if prim.is_int() => arith(PrimOp::Shr),
        // `str` compares contents through its `PartialEq` impl.
        "==" if prim != PrimTy::Str => cmp(PrimOp::Eq),
        "!=" if prim != PrimTy::Str => cmp(PrimOp::Ne),
        "<" if num => cmp(PrimOp::Lt),
        "<=" if num => cmp(PrimOp::Le),
        ">" if num => cmp(PrimOp::Gt),
        ">=" if num => cmp(PrimOp::Ge),
        _ => None,
    }
}

pub(super) fn prefix_for_prim(sym: &str, prim: PrimTy) -> Option<(PrimOp, HirTy)> {
    let same_ty = HirTy::Prim(prim);
    match sym {
        "-" if prim.is_signed_int() || prim.is_float() => Some((PrimOp::Neg, same_ty)),
        "!" if matches!(prim, PrimTy::Bool) => Some((PrimOp::LogicalNot, same_ty)),
        // On an integer `!` is the bitwise complement, as `~` (stdlib 2.3.4).
        "~" | "!" if prim.is_int() => Some((PrimOp::Not, same_ty)),
        _ => None,
    }
}

/// `p` with `Self` and alias prefixes replaced by absolute paths: `B::mk`
/// (`type B = S<i32>`) is `S::<i32>::mk`; `Al::mk` (`type Al<T> = S<T>`)
/// is `S::mk` with inferred arguments (each must be a distinct parameter
/// of the alias). `None` if no type prefix.
fn alias_path(ctx: &LowerCtx<'_>, p: &AstPath) -> Result<Option<AstPath>, String> {
    let Some((last, prefix)) = p.segments.split_last().filter(|(_, t)| !t.is_empty()) else {
        return Ok(None);
    };
    let names = &ctx.env.names;
    let at = ctx.here();
    let mut ty = Ty {
        kind: AstTyKind::Path(AstPath {
            segments: prefix.to_vec(),
            span: p.span,
        }),
        span: p.span,
    };
    let mut through = false;
    for _ in 0..32 {
        let AstTyKind::Path(tp) = &ty.kind else {
            return Ok(None);
        };
        let segs: Vec<&str> = tp.segments.iter().map(|s| s.name.as_str()).collect();
        let Some(full) = ctx.canon_type(&segs) else {
            break;
        };
        let generic = names
            .generic_alias(&full)
            .filter(|a| !a.generics.params.is_empty());
        let unapplied = tp.segments.last().is_some_and(|s| s.generics.is_empty());
        ty = match generic {
            Some(a) if unapplied => {
                let params: HashMap<String, Ty> = a
                    .generics
                    .params
                    .iter()
                    .map(|gp| {
                        let (rokc_ast::GenericParam::Type { name, .. }
                        | rokc_ast::GenericParam::Const { name, .. }) = gp;
                        (
                            name.clone(),
                            Ty {
                                kind: AstTyKind::Never,
                                span: at,
                            },
                        )
                    })
                    .collect();
                let mut body = a.ty.clone();
                names.absolutize(
                    &mut body,
                    &module_of(&full),
                    &ctx.current_module,
                    &params,
                    at,
                );
                let AstTyKind::Path(bp) = &mut body.kind else {
                    return Ok(None);
                };
                let mut seen = std::collections::HashSet::new();
                let open = bp.segments.last_mut().filter(|s| {
                    s.generics.iter().all(|g| {
                        matches!(&g.kind, AstTyKind::Path(gp) if gp.segments.len() == 1
                            && params.contains_key(&gp.segments[0].name)
                            && seen.insert(gp.segments[0].name.clone()))
                    })
                });
                let Some(open) = open else {
                    let path = segs.join("::");
                    return Err(format!(
                        "`{path}::{}` can't infer `{}`'s arguments: its aliased type's arguments \
                         are not its distinct parameters (spec 2.6.9) - write `{path}::<..>::{}`",
                        last.name,
                        leaf_name(&full),
                        last.name
                    ));
                };
                open.generics.clear();
                body
            }
            Some(_) => match names.expand_alias(&ty, &ctx.current_module, at) {
                Some(t) => t,
                None => break,
            },
            None => match names.plain_alias_ty(&full) {
                Some(t) => {
                    let mut body = t.clone();
                    let none = HashMap::new();
                    names.absolutize(&mut body, &module_of(&full), &ctx.current_module, &none, at);
                    body
                }
                None => break,
            },
        };
        through = true;
    }
    let AstTyKind::Path(tp) = ty.kind else {
        return Ok(None);
    };
    Ok(through.then(|| AstPath {
        segments: tp.segments.into_iter().chain([last.clone()]).collect(),
        span: p.span,
    }))
}

pub(super) fn self_path_segs(ctx: &LowerCtx<'_>, p: &AstPath) -> Vec<String> {
    let segs: Vec<String> = p.segments.iter().map(|s| s.name.clone()).collect();
    let owner = match (segs.first().map(String::as_str), &ctx.self_ty) {
        (Some("Self"), Some(HirTy::Struct(id))) if segs.len() > 1 => ctx.struct_def(*id).name,
        (Some("Self"), Some(HirTy::Enum(id))) if segs.len() > 1 => ctx.enum_def(*id).name,
        _ => return segs,
    };
    let root = (!owner.starts_with("rok::")).then(|| "pkg".to_owned());
    root.into_iter()
        .chain(owner.split("::").map(str::to_owned))
        .chain(segs.into_iter().skip(1))
        .collect()
}

/// Impls (with their substitution) giving `ty` associated const `name`,
/// from `traits` if given, with `wants`' trait arguments (`<T as
/// Tr<A>>::N`, `T::N` under `T: Tr<A>`).
pub(super) fn assoc_const_hits<'e>(
    ctx: &LowerCtx<'e>,
    ty: &HirTy,
    name: &str,
    traits: Option<&[String]>,
    wants: &[TraitRef],
) -> Vec<(&'e Impl, HashMap<String, HirTy>)> {
    let env: &'e Env<'e> = ctx.env;
    env.impls
        .with_const(name)
        .filter(|imp| traits.is_none_or(|ks| ks.contains(&imp.trait_key)))
        .filter_map(|imp| {
            let (subst, have) = if let Some((t, r)) = &imp.concrete {
                (
                    (t == ty).then(HashMap::new)?,
                    r.as_ref().map(|r| r.args.clone()).unwrap_or_default(),
                )
            } else {
                let subst = ctx.generic_impl_subst(imp, ty, 0)?;
                let mut rcx = LowerCtx::new(ctx.env, &subst, None, ctx.here());
                rcx.current_module.clone_from(&imp.module);
                let have = super::resolve::trait_args(&imp.trait_args, |t| rcx.resolve_ast_ty(t)).0;
                (subst, have)
            };
            let mut of = wants.iter().filter(|w| w.path == imp.trait_key).peekable();
            let fits =
                of.peek().is_none() || of.any(|w| w.args.iter().zip(&have).all(|(w, h)| w == h));
            fits.then_some((imp, subst))
        })
        .collect()
}

fn lower_assoc_const(
    ctx: &mut LowerCtx<'_>,
    ty: &HirTy,
    name: &str,
    traits: Option<&[String]>,
    wants: &[TraitRef],
) -> Option<HirExpr> {
    if let HirTy::Param(p) = ty {
        return super::bounds::assoc_const(ctx, *p, name, traits, wants);
    }
    let env = ctx.env;
    let mut hits = assoc_const_hits(ctx, ty, name, traits, wants);
    // An inherent const shadows the trait ones (`i8::BITS`).
    if hits.len() > 1 && traits.is_none() && hits.iter().any(|(s, _)| s.trait_key.is_empty()) {
        hits.retain(|(s, _)| s.trait_key.is_empty());
    }
    if hits.len() > 1 {
        let from: Vec<String> = hits
            .iter()
            .map(|(s, _)| {
                if s.trait_key.is_empty() {
                    "an inherent impl".to_owned()
                } else {
                    format!("`{}`", leaf_name(&s.trait_key))
                }
            })
            .collect();
        return Some(ctx.error_expr(
            ctx.here(),
            format!(
                "associated const `{name}` of `{}` is ambiguous: {} define it - name the \
                 trait (`<T as Tr>::{name}`) (spec 2.8.3a)",
                ctx.ty_name(ty),
                from.join(" and ")
            ),
        ));
    }
    let (imp, subst) = hits.into_iter().next()?;
    let (_, cty, value) = imp.consts.iter().find(|(n, ..)| n == name)?;
    let sc = Scope {
        names: &env.names,
        module: &imp.module,
        at: ctx.here(),
    };
    let cty = resolve_ty_with_structs(cty, &env.defs, Some(&subst), sc);
    // The value is the impl's: its names resolve in the impl's module,
    // with the impl's type parameters and `Self` bound for `ty`.
    let saved_subst = std::mem::replace(&mut ctx.ty_subst, std::borrow::Cow::Owned(subst));
    let saved_module = std::mem::replace(&mut ctx.current_module, imp.module.clone());
    let saved_self = std::mem::replace(&mut ctx.self_ty, Some(ty.clone()));
    let saved_uses = std::mem::replace(&mut ctx.local_uses, vec![Vec::new()]);
    let saved_scopes = std::mem::replace(&mut ctx.scopes, vec![HashMap::new()]);
    let key = format!("{}::{name}", ctx.ty_name(ty));
    let span = ctx.here();
    let v = super::const_eval::guard(ctx, &key, span, |ctx| {
        let v = lower_expected(ctx, value, cty.clone());
        coerce(ctx, &cty, v, &|| format!("associated const `{name}`"))
    });
    ctx.ty_subst = saved_subst;
    ctx.current_module = saved_module;
    ctx.self_ty = saved_self;
    ctx.local_uses = saved_uses;
    ctx.scopes = saved_scopes;
    Some(v.map_or_else(
        || poison(span),
        |v| super::const_eval::const_item(ctx, &key, v),
    ))
}

/// The name a type's `Type::NAME` consts are keyed under.
fn const_owner(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<String> {
    match ty {
        HirTy::Struct(id) => Some(enum_base_name(&ctx.struct_def(*id).name).to_owned()),
        HirTy::Enum(id) => Some(enum_base_name(&ctx.enum_def(*id).name).to_owned()),
        HirTy::Prim(p) => Some(p.name().to_owned()),
        _ => None,
    }
}

/// The trait key of `<T as Tr>`'s `Tr`, `None` when it names no trait.
fn qpath_trait(ctx: &LowerCtx<'_>, q: &rokc_ast::QPath) -> Option<String> {
    let segs: Vec<&str> = q
        .trait_path
        .segments
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    Some(
        ctx.env
            .names
            .trait_key(&segs, &ctx.current_module, ctx.here()),
    )
    .filter(|k| ctx.env.names.is_trait(k))
}

/// `<T as Tr>::N` (an associated const) or `<T as Tr>::m` (a method
/// value) not in call position (spec 2.13.4).
fn lower_qpath_value(
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    q: &rokc_ast::QPath,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    if let Some(m) = &q.member {
        return ctx.error_expr(
            span,
            format!(
                "`<.. as ..>::{}::{m}` without a call: write a closure `|..| <..>::{}::{m}(..)`",
                q.item, q.item
            ),
        );
    }
    let t = ctx.resolve_ast_ty(&q.self_ty);
    let Some(key) = qpath_trait(ctx, q) else {
        return ctx.error_expr(
            span,
            format!(
                "`<{} as ..>::{}`: not a trait after `as`",
                ctx.ty_name(&t),
                q.item
            ),
        );
    };
    let want = super::resolve::trait_ref(
        &ctx.env.names,
        &q.trait_path,
        &ctx.current_module,
        ctx.here(),
        |t| ctx.resolve_ast_ty(t),
    );
    if let Some(v) = lower_assoc_const(ctx, &t, &q.item, Some(&[key]), &[want]) {
        return v;
    }
    if let Some(v) = super::method_value::method_path_value(ctx, &expr.kind, span) {
        return v;
    }
    ctx.error_expr(
        span,
        format!(
            "`<{} as {}>` has no associated const or method `{}`",
            ctx.ty_name(&t),
            q.trait_path
                .segments
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join("::"),
            q.item
        ),
    )
}

/// `Type::N` / `Box::<i32>::N`, `T::N` / `Self::N`: an associated const
/// from the impl covering that type (spec 2.8.3a).
fn assoc_const_path(ctx: &mut LowerCtx<'_>, path: &AstPath) -> Option<HirExpr> {
    // `Type::N` / `Box::<i32>::N`: the impl covering that type.
    if let [prefix @ .., name] = path.segments.as_slice() {
        let segs: Vec<&str> = prefix.iter().map(|s| s.name.as_str()).collect();
        let owner = rokc_ast::Ty {
            kind: AstTyKind::Path(AstPath {
                segments: prefix.to_vec(),
                span: path.span,
            }),
            span: path.span,
        };
        if !prefix.is_empty()
            && segs != ["Self"]
            && (ctx
                .canon_type(&segs)
                .is_some_and(|f| !ctx.env.names.is_trait(&f))
                || matches!(crate::ty::resolve_ty(&owner), HirTy::Prim(_)))
        {
            let ty = ctx.resolve_ast_ty(&owner);
            if let Some(v) = lower_assoc_const(ctx, &ty, &name.name, None, &[]) {
                return Some(v);
            }
        }
    }
    // `T::N` / `Self::N` on a concrete type: the impl that covers it.
    if let [owner, name] = path.segments.as_slice() {
        let ty = match owner.name.as_str() {
            "Self" => ctx.self_ty.clone(),
            n => ctx
                .ty_subst
                .get(n)
                .or_else(|| ctx.explicit_generics.get(n))
                .cloned(),
        };
        // A type parameter reads the const of a trait it is bounded by.
        let bounds = ctx.ty_bounds.get(&owner.name).cloned();
        let wants = ctx.bound_wants(&owner.name);
        if let Some(v) = ty
            .filter(|_| owner.generics.is_empty())
            .and_then(|t| lower_assoc_const(ctx, &t, &name.name, bounds.as_deref(), &wants))
        {
            return Some(v);
        }
    }
    None
}

pub(super) fn lower_path(
    ctx: &mut LowerCtx<'_>,
    path: &AstPath,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let aliased = match alias_path(ctx, path) {
        Ok(a) => a,
        Err(msg) => return ctx.error_expr(span, msg),
    };
    let path = aliased.as_ref().unwrap_or(path);
    let self_segs = self_path_segs(ctx, path);
    let segs: Vec<&str> = self_segs.iter().map(String::as_str).collect();
    if let [name] = segs.as_slice() {
        if let Some(h) = ctx.splices.get(*name) {
            return h.clone();
        }
        if let Some(local) = ctx.lookup(name) {
            return ctx.read_local(local, span);
        }
    }
    // Spec 2.11.10: a const generic parameter is a value.
    if let [name] = segs.as_slice() {
        if let Some((v, Some(p))) = ctx.ty_subst.get(*name).and_then(HirTy::as_const) {
            let value = match p {
                PrimTy::Bool => ConstVal::Bool(v != 0),
                PrimTy::Char => ConstVal::Char(u32::try_from(v).unwrap_or_default()),
                _ => ConstVal::Int { value: v, ty: p },
            };
            return HirExpr {
                kind: HirExprKind::Const(value),
                ty: HirTy::Prim(p),
                span,
            };
        }
    }
    // `const NAME: T = expr;` is inlined, typed by its declaration; `T::N`
    // / `Self::N` reads the concrete type's const (spec 2.8.3a).
    if let Some(v) = assoc_const_path(ctx, path) {
        return v;
    }
    let assoc = match path.segments.as_slice() {
        [owner, name] if owner.generics.is_empty() => {
            let ty = if owner.name == "Self" {
                ctx.self_ty.clone()
            } else {
                ctx.ty_subst
                    .get(&owner.name)
                    .or_else(|| ctx.explicit_generics.get(&owner.name))
                    .cloned()
            };
            ty.and_then(|t| const_owner(ctx, &t))
                .map(|o| format!("{o}::{}", name.name))
                .filter(|k| ctx.env.konst(k).is_some())
        }
        _ => None,
    };
    if let Some(full) = assoc.or_else(|| ctx.resolve_in(&segs, false)) {
        let full = ctx.env.names.value_reexport(&full).cloned().unwrap_or(full);
        ctx.check_item_visible(&full, "const");
        let (ty, value) = ctx.env.konst(&full).expect("a const").clone();
        // An impl const `m::T::N` resolves its type and value in `m`.
        let owner = module_of(&full);
        let owner_segs: Vec<&str> = owner.split("::").collect();
        let module = if ctx.canon_type(&owner_segs).as_deref() == Some(owner.as_str()) {
            module_of(&owner)
        } else {
            owner
        };
        let sc = Scope {
            names: &ctx.env.names,
            module: &module,
            at: ctx.here(),
        };
        let ty = resolve_ty_with_structs(&ty, &ctx.env.defs, None, sc);
        // The value's names (and field privacy) are the defining module's.
        let saved_module = std::mem::replace(&mut ctx.current_module, module);
        let saved_uses = std::mem::replace(&mut ctx.local_uses, vec![Vec::new()]);
        let saved_scopes = std::mem::replace(&mut ctx.scopes, vec![HashMap::new()]);
        let Some(v) = super::const_eval::guard(ctx, &full, span, |ctx| {
            lower_expected(ctx, &value, ty.clone())
        }) else {
            ctx.current_module = saved_module;
            ctx.local_uses = saved_uses;
            ctx.scopes = saved_scopes;
            return poison(span);
        };
        ctx.current_module = saved_module;
        ctx.local_uses = saved_uses;
        ctx.scopes = saved_scopes;
        let v = coerce(ctx, &ty, v, &|| format!("const `{full}`"));
        return super::const_eval::const_item(ctx, &full, v);
    }
    // Module-level `static` - a load through the global address.
    if let Some(full) = ctx.resolve_in(&segs, true) {
        let full = ctx.env.names.value_reexport(&full).cloned().unwrap_or(full);
        ctx.check_item_visible(&full, "static");
        let ty = ctx.env.static_ty(&full).expect("a static").clone();
        return HirExpr {
            kind: HirExprKind::StaticRef(full),
            ty,
            span,
        };
    }
    // A function used as a value → function pointer.
    if let Some(full) = ctx.resolve_fn_name(&segs) {
        ctx.check_item_visible(&full, "function");
        let sig = ctx.sig_of_key(&full);
        if sig.param_refs.iter().any(|r| *r) {
            return ctx.error_expr(
                span,
                format!(
                    "`{full}` has `ref` parameters and cannot be used as a `fn` value (a `fn` \
                     type has no `ref` parameters, spec 4.7.6)"
                ),
            );
        }
        return HirExpr {
            kind: HirExprKind::FnRef(sig.id),
            // A named function captures nothing: Send + Sync.
            ty: HirTy::Fn(sig.param_tys, Box::new(sig.ret_ty), {
                // An `unsafe fn` / C function value keeps its unsafety.
                let mut b = ctx.env.names.all_auto();
                b.unsafe_fn = ctx.env.is(super::FnSet::Unsafe, sig.id);
                b
            }),
            span,
        };
    }
    if let Some(full) = ctx.resolve_generic_fn_name(&segs) {
        ctx.check_item_visible(&full, "function");
        let def = ctx.env.generic_fn(&full).expect("a generic function");
        // `id::<i32>`: every type argument given - that instance's value.
        let targs = path
            .segments
            .last()
            .map_or(&[][..], |s| s.generics.as_slice());
        // `let f: fn(i32) -> i32 = id;`: the receiving fn type picks the
        // instance (spec 4.4.7).
        let from_slot = if targs.is_empty() {
            generic_fn_args_from_slot(ctx, def)
        } else {
            None
        };
        if !targs.is_empty() || from_slot.is_some() {
            let bound = match from_slot {
                Some(b) => b,
                None => match bind_explicit(ctx, def, None, targs, &full) {
                    Ok(b) => b,
                    Err(msg) => return ctx.error_expr(span, msg),
                },
            };
            if bound.len() < def.generics.params.len() {
                return ctx.error_expr(
                    span,
                    format!(
                        "`{}` as a value needs all {} generic argument(s)",
                        leaf_name(&full),
                        def.generics.params.len()
                    ),
                );
            }
            ctx.explicit_generics = bound;
            let mono = monomorphize(ctx, def, None, &[], span);
            ctx.explicit_generics.clear();
            let Some((id, ret)) = mono else {
                return poison(span);
            };
            let params = ctx
                .env
                .sink
                .mono_sigs
                .borrow()
                .get(&id)
                .map(|s| s.param_tys.clone())
                .unwrap_or_default();
            return HirExpr {
                kind: HirExprKind::FnRef(id),
                ty: HirTy::Fn(params, Box::new(ret), ctx.env.names.all_auto()),
                span,
            };
        }
        if let Some(v) = rethrows_value(ctx, def, span) {
            return v;
        }
        return ctx.error_expr(
            span,
            format!(
                "generic function `{}` as a value: its type arguments are not determined \
                 - write `{}::<..>` or give the slot a `fn(..)` type (spec 4.4.7)",
                segs.join("::"),
                segs.join("::")
            ),
        );
    }
    // `S::new` / `i32::add` / `Tr::m` without a call (spec 3.5.3).
    let callee = ExprKind::Path(path.clone());
    if let Some(v) = super::method_value::method_path_value(ctx, &callee, span) {
        return v;
    }
    // `EnumName::Variant` (unit form). Tuple variants come through
    // `lower_call` because the parser wraps them in a Call expression.
    let hint = explicit_enum(ctx, path).or_else(|| ctx.current_expected().cloned());
    match resolve_enum_variant(ctx, &segs, hint.as_ref(), None) {
        EnumRes::Found(enum_id, idx) => {
            let variant = &ctx.enum_def(enum_id).variants[idx];
            if variant.payload_ty.is_some() {
                return ctx.error_expr(
                    span,
                    format!(
                        "enum variant `{}` takes arguments: `{}(...)`",
                        segs.join("::"),
                        segs.join("::")
                    ),
                );
            }
            return HirExpr {
                kind: HirExprKind::EnumVariantInit {
                    enum_id,
                    variant_index: u32::try_from(idx).expect("variant overflow"),
                    args: Vec::new(),
                },
                ty: HirTy::Enum(enum_id),
                span,
            };
        }
        EnumRes::Error => return poison(span),
        EnumRes::NotEnum => {}
    }
    // A unit struct `Unit` is its own value.
    if let Some(sid) = ctx
        .canon_type(&segs)
        .and_then(|full| ctx.lookup_struct(&full))
    {
        if ctx.env.duplicated(ctx.struct_def(sid).span) {
            return poison(span);
        }
        if ctx.struct_def(sid).fields.is_empty() {
            return HirExpr {
                kind: HirExprKind::StructInit {
                    struct_id: sid,
                    fields: Vec::new(),
                },
                ty: HirTy::Struct(sid),
                span,
            };
        }
    }
    let top_let = ctx
        .env
        .main_lets
        .get(segs[0])
        .filter(|_| !ctx.implicit_main);
    if let (Some(konst), [name]) = (top_let, segs.as_slice()) {
        return ctx.unknown_name(
            span,
            format!(
                "cannot find value `{name}`: it is a top-level `let`, a variable of the \
                 program's statements, and a `fn` cannot see those"
            ),
            Some(format!(
                "make it a constant and use that name, `{konst}`, or pass `{name}` to the \
                 function as an argument"
            )),
        );
    }
    let help = ctx.value_help(&segs);
    ctx.unknown_name(
        span,
        format!("cannot find value `{}` in this scope", segs.join("::")),
        help,
    )
}

/// Integer type an unsuffixed literal takes from its expected slot.
pub(super) fn expected_prim(ctx: &LowerCtx<'_>, want: fn(PrimTy) -> bool) -> Option<PrimTy> {
    match ctx.current_expected()? {
        HirTy::Prim(p) if want(*p) => Some(*p),
        // `let x: i64 | none = 5` - the sum's single matching variant.
        HirTy::Sum(vs) => {
            let mut hits = vs.iter().filter_map(|v| match v {
                HirTy::Prim(p) if want(*p) => Some(*p),
                _ => None,
            });
            let first = hits.next()?;
            hits.next().is_none().then_some(first)
        }
        _ => None,
    }
}

/// A literal (`negate`: under a unary minus). Unsuffixed numbers take the
/// expected type (else `i32` / `f64`); integers are range-checked.
pub(super) fn lower_literal(
    ctx: &LowerCtx<'_>,
    lit: &AstLit,
    negate: bool,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let konst = |kind: ConstVal, ty: HirTy| HirExpr {
        kind: HirExprKind::Const(kind),
        ty,
        span,
    };
    match lit {
        AstLit::Int(text) => {
            if let Some((digits, wide, narrow)) = ["i128", "u128"]
                .into_iter()
                .find_map(|w| Some((text.strip_suffix(w)?, w, super::wide_int_narrow(w)?)))
            {
                // Offer the 64-bit suffix only where the value fits it.
                let max = if narrow == "u64" {
                    u64::MAX.into()
                } else {
                    i128::from(i64::MAX)
                };
                let fits = parse_int_literal(digits)
                    .is_some_and(|(v, _)| v <= max + i128::from(negate && narrow == "i64"));
                let narrow = if fits {
                    format!("`{digits}{narrow}`, or ")
                } else {
                    String::new()
                };
                return ctx.error_expr(
                    span,
                    format!(
                        "integer literal `{text}`: rok has no `{wide}`; write {narrow}`{digits}` \
                         where an `int` is expected (any size)"
                    ),
                );
            }
            let Some((value, suffix_ty)) = parse_int_literal(text) else {
                return ctx.error_expr(
                    span,
                    format!("integer literal `{text}` is too large (an `int` holds any size)"),
                );
            };
            let value = if negate { -value } else { value };
            // An unsuffixed integer in a float slot is that float if exact
            // (`let x: f64 = 1`); an integer member of an expected sum wins
            // (`i32 | f64`). `char` takes no integer literal.
            let int_slot = |p: PrimTy| p.is_int() && !matches!(p, PrimTy::CChar | PrimTy::Char);
            if let Some(fty) = (!has_int_suffix(text) && expected_prim(ctx, int_slot).is_none())
                .then(|| expected_prim(ctx, PrimTy::is_float))
                .flatten()
            {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let (bits, exact) = if fty == PrimTy::F32 {
                    let f = value as f32;
                    (u64::from(f.to_bits()), f as i128 == value)
                } else {
                    let f = value as f64;
                    (f.to_bits(), f as i128 == value)
                };
                if !exact {
                    return ctx.error_expr(
                        span,
                        format!(
                            "integer literal `{text}` is not exactly representable as `{}`",
                            fty.name()
                        ),
                    );
                }
                return konst(ConstVal::Float { bits, ty: fty }, HirTy::Prim(fty));
            }
            let ty = if has_int_suffix(text) {
                suffix_ty
            } else {
                expected_prim(ctx, int_slot).unwrap_or(PrimTy::I32)
            };
            if negate && !ty.is_signed_int() {
                return ctx.error_expr(
                    span,
                    format!(
                        "cannot negate unsigned literal `{text}` of type `{}`",
                        ty.name()
                    ),
                );
            }
            if !int_fits(value, ty) {
                let shown = if negate {
                    format!("-{text}")
                } else {
                    text.clone()
                };
                return ctx.error_expr(
                    span,
                    format!("integer literal `{shown}` out of range for `{}`", ty.name()),
                );
            }
            konst(ConstVal::Int { value, ty }, HirTy::Prim(ty))
        }
        AstLit::Float(text) => {
            let suffixed = text.ends_with("f32") || text.ends_with("f64");
            let ty = if suffixed {
                parse_float_literal(text).map_or(PrimTy::F64, |(_, t)| t)
            } else {
                expected_prim(ctx, PrimTy::is_float).unwrap_or(PrimTy::F64)
            };
            let digits = text.trim_end_matches("f32").trim_end_matches("f64");
            let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
            let Ok(v) = cleaned.parse::<f64>() else {
                return ctx.error_expr(span, format!("malformed float literal `{text}`"));
            };
            if v.is_infinite() || (ty == PrimTy::F32 && (v as f32).is_infinite()) {
                return ctx.error_expr(
                    span,
                    format!("float literal out of range for `{}`", ty.name()),
                );
            }
            let v = if negate { -v } else { v };
            let bits = if ty == PrimTy::F32 {
                #[allow(clippy::cast_possible_truncation)]
                u64::from((v as f32).to_bits())
            } else {
                v.to_bits()
            };
            konst(ConstVal::Float { bits, ty }, HirTy::Prim(ty))
        }
        AstLit::Bool(b) => konst(ConstVal::Bool(*b), HirTy::Prim(PrimTy::Bool)),
        AstLit::Str(s) => konst(
            ConstVal::Str(decode_str_escapes(s)),
            HirTy::Prim(PrimTy::Str),
        ),
        AstLit::RawStr(s) => konst(ConstVal::Str(s.clone()), HirTy::Prim(PrimTy::Str)),
        AstLit::None => konst(ConstVal::None, HirTy::None),
        AstLit::Char(s) => match parse_char_literal(s) {
            // An ASCII `'a'` flowing into a `c_char` slot takes that type.
            Some(c) if c < 0x80 && expected_prim(ctx, |p| p == PrimTy::CChar).is_some() => {
                konst(ConstVal::Char(c), HirTy::Prim(PrimTy::CChar))
            }
            Some(c) => konst(ConstVal::Char(c), HirTy::Prim(PrimTy::Char)),
            None => ctx.error_expr(span, format!("unsupported character literal `'{s}'`")),
        },
        AstLit::CChar(s) => match parse_char_literal(s) {
            Some(c) => konst(ConstVal::Char(c), HirTy::Prim(PrimTy::CChar)),
            None => ctx.error_expr(span, format!("unsupported character literal `c'{s}'`")),
        },
        // Reached only under a unary minus (`lower_byte_str` otherwise).
        AstLit::ByteStr(_) => ctx.error_expr(span, "cannot negate a byte string".to_owned()),
    }
}

// `'A'` / `c'A'` payload is the body as written (without quotes);
// decode it with the shared escape table (spec 1.8).
pub(super) fn parse_char_literal(text: &str) -> Option<u32> {
    let decoded = rokc_ast::escape::unescape(text, rokc_ast::escape::EscapeMode::CChar).ok()?;
    let mut chars = decoded.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c as u32)
}

/// Parse an integer literal; an unsuffixed one defaults to `i32`.
pub(super) fn parse_int_literal(text: &str) -> Option<(i128, PrimTy)> {
    const SUFFIXES: &[(&str, PrimTy)] = &[
        ("isize", PrimTy::Isize),
        ("usize", PrimTy::Usize),
        ("i64", PrimTy::I64),
        ("u64", PrimTy::U64),
        ("i32", PrimTy::I32),
        ("u32", PrimTy::U32),
        ("i16", PrimTy::I16),
        ("u16", PrimTy::U16),
        ("i8", PrimTy::I8),
        ("u8", PrimTy::U8),
    ];

    let (digits, ty) = SUFFIXES
        .iter()
        .find_map(|(suf, ty)| text.strip_suffix(suf).map(|d| (d, *ty)))
        .unwrap_or((text, PrimTy::I32));

    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    let (radix, body) = if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        (16u32, rest.to_owned())
    } else if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        (2u32, rest.to_owned())
    } else if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        (8u32, rest.to_owned())
    } else {
        (10u32, cleaned)
    };

    let value = i128::from_str_radix(&body, radix).ok()?;
    Some((value, ty))
}

pub(super) fn parse_float_literal(text: &str) -> Option<(u64, PrimTy)> {
    const SUFFIXES: &[(&str, PrimTy)] = &[("f32", PrimTy::F32), ("f64", PrimTy::F64)];
    let (digits, ty) = SUFFIXES
        .iter()
        .find_map(|(suf, ty)| text.strip_suffix(suf).map(|d| (d, *ty)))
        .unwrap_or((text, PrimTy::F64));

    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    let bits = match ty {
        PrimTy::F32 => u64::from(cleaned.parse::<f32>().ok()?.to_bits()),
        _ => cleaned.parse::<f64>().ok()?.to_bits(),
    };
    Some((bits, ty))
}

/// A built-in literal shape (`[..]`, `{k: v}`, `{a, b}`, ranges) calls the
/// handlers registered for it (spec 6.5.1, 6.5.4); a user package's apply
/// to its own code.
fn lower_shape(
    ctx: &mut LowerCtx<'_>,
    shape: rokc_parser::Shape,
    args: Vec<rokc_ast::LitArg>,
    what: &str,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let in_lib = ctx.current_module == "rok" || ctx.current_module.starts_with("rok::");
    let cands: Vec<rokc_ast::LitCand> = ctx
        .env
        .literal_shapes
        .iter()
        .filter(|h| h.shape == shape && (!in_lib || h.name.starts_with("rok::")))
        .map(|h| rokc_ast::LitCand {
            handler: AstPath {
                segments: h
                    .name
                    .split("::")
                    .map(|n| rokc_ast::PathSegment {
                        name: n.to_owned(),
                        generics: Vec::new(),
                    })
                    .collect(),
                span,
            },
            args: args.clone(),
            priority: h.priority,
            reg: h.span,
        })
        .collect();
    if cands.is_empty() {
        return ctx.error_expr(
            span,
            format!("no `#[literal]` handler is registered for {what} literals (spec 6.5)"),
        );
    }
    let empty = args
        .iter()
        .any(|a| matches!(a, rokc_ast::LitArg::Many(v) if v.is_empty()));
    if empty && !ctx.infer_vars && ctx.current_expected().is_none_or(|t| *t == HirTy::Unknown) {
        ctx.emit_uninferred(format!(
            "cannot infer the type of an empty {what} literal - annotate the slot"
        ));
        return poison(span);
    }
    lower_magic(ctx, &cands, span)
}

/// The full name of a literal handler's path (`pkg::` is the root).
fn handler_name(c: &rokc_ast::LitCand) -> String {
    let segs: Vec<&str> = c.handler.segments.iter().map(|s| s.name.as_str()).collect();
    match segs.split_first() {
        Some((&"pkg", rest)) => rest.join("::"),
        _ => segs.join("::"),
    }
}

/// The leaf name of the type a handler declares it returns.
fn handler_ret_leaf(ctx: &LowerCtx<'_>, name: &str) -> Option<String> {
    if let Some(def) = ctx.env.generic_fn(name) {
        let rokc_ast::TyKind::Path(p) = &def.ret_ty.as_ref()?.kind else {
            return None;
        };
        return p.segments.last().map(|s| s.name.clone());
    }
    ctx.env.sig(name)?;
    match &ctx.sig_of_key(name).ret_ty {
        HirTy::Struct(id) => Some(leaf_name(enum_base_name(&ctx.struct_def(*id).name)).to_owned()),
        HirTy::Enum(id) => Some(leaf_name(enum_base_name(&ctx.enum_def(*id).name)).to_owned()),
        _ => None,
    }
}

/// Spec 6.5.4 / 6.5.5: pick the handler (return type is the slot's, then
/// highest `priority`; still several is an error) and call it.
fn lower_magic(
    ctx: &mut LowerCtx<'_>,
    cands: &[rokc_ast::LitCand],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let expected = ctx.current_expected().cloned().unwrap_or(HirTy::Unknown);
    let slot_leaf = match &expected {
        HirTy::Struct(id) => Some(leaf_name(enum_base_name(&ctx.struct_def(*id).name)).to_owned()),
        HirTy::Enum(id) => Some(leaf_name(enum_base_name(&ctx.enum_def(*id).name)).to_owned()),
        _ => None,
    };
    let mut live: Vec<&rokc_ast::LitCand> = cands.iter().collect();
    let typed: Vec<&rokc_ast::LitCand> = live
        .iter()
        .copied()
        .filter(|c| slot_leaf.is_some() && handler_ret_leaf(ctx, &handler_name(c)) == slot_leaf)
        .collect();
    if !typed.is_empty() {
        live = typed;
    }
    let top = live.iter().map(|c| c.priority).max().unwrap_or(0);
    live.retain(|c| c.priority == top);
    if live.len() > 1 {
        let names: Vec<String> = live
            .iter()
            .map(|c| format!("`{}`", handler_name(c)))
            .collect();
        let mut d = rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Type,
            span,
            format!(
                "ambiguous literal: handlers {} all match (spec 6.5.5) - give the slot a type \
                 or a handler `priority = N`",
                names.join(", ")
            ),
        );
        for c in &live {
            d = d.with_secondary(c.reg, "registered here");
        }
        ctx.env.sink.diagnostics.borrow_mut().push(d);
        return poison(span);
    }
    let Some(c) = live.first() else {
        return poison(span);
    };
    let name = handler_name(c);
    let def = ctx.env.generic_fn(&name);
    // The handler's last parameter is a variadic `...T`.
    let variadic_at = match def {
        Some(d) => d
            .params
            .last()
            .filter(|p| p.variadic)
            .map(|_| d.params.len() - 1),
        None => ctx
            .env
            .sig(&name)
            .filter(|s| s.variadic)
            .map(|s| s.param_tys.len() - 1),
    };
    let mut args: Vec<CallArg> = Vec::new();
    for (i, a) in c.args.iter().enumerate() {
        match a {
            rokc_ast::LitArg::One(e) => args.push(CallArg::Positional(e.clone())),
            rokc_ast::LitArg::Many(items) if variadic_at == Some(i) => {
                for it in items {
                    let a = list_item(ctx, it);
                    args.push(a);
                }
            }
            rokc_ast::LitArg::Many(items) => args.push(CallArg::Positional(Expr {
                kind: ExprKind::List(items.clone()),
                span,
            })),
        }
    }
    let call = Expr {
        kind: ExprKind::Call {
            callee: Box::new(Expr {
                kind: ExprKind::Path(c.handler.clone()),
                span,
            }),
            args,
            trailing_closure: None,
        },
        span,
    };
    // The slot type fixes the handler's type parameters: `[1, 2]` in a
    // `List<i64>` slot has `i64` items.
    if let (Some(def), Some(ret), true) = (
        def,
        def.and_then(|d| d.ret_ty.as_ref()),
        slot_leaf.is_some() && handler_ret_leaf(ctx, &name) == slot_leaf,
    ) {
        let names: std::collections::HashSet<String> = def
            .generics
            .params
            .iter()
            .map(|gp| match gp {
                rokc_ast::GenericParam::Type { name, .. }
                | rokc_ast::GenericParam::Const { name, .. } => name.clone(),
            })
            .collect();
        let mut subst = HashMap::new();
        super::mono::unify_param_ty_with(ret, &expected, &names, &mut subst, ctx);
        ctx.explicit_generics = subst;
    }
    let mut h = lower_expected(ctx, &call, expected);
    super::const_eval::fold_literal(ctx, &mut h, span);
    h
}

/// An AST stand-in (`#sN` path) for lowered `h`, so a desugar can place it
/// without re-lowering. Each use evaluates `h` again: use it once.
pub(super) fn splice(ctx: &mut LowerCtx<'_>, h: HirExpr) -> Expr {
    let span = h.span;
    let name = format!("#s{}", ctx.splices.len());
    ctx.splices.insert(name.clone(), h);
    Expr {
        kind: ExprKind::Path(rokc_ast::Path {
            segments: vec![rokc_ast::PathSegment {
                name,
                generics: Vec::new(),
            }],
            span,
        }),
        span,
    }
}

const fn is_signed(ty: &HirTy) -> bool {
    matches!(ty, HirTy::Prim(p) if p.is_signed_int())
}

/// An index / slice bound in a `usize` position slot: a negative
/// untyped literal (`xs[-1]`) is an `isize`.
fn lower_position(ctx: &mut LowerCtx<'_>, idx: &Expr) -> HirExpr {
    let neg = matches!(&idx.kind, ExprKind::OpPrefix { op, .. } if op == "-");
    let want = if neg && is_untyped_literal(idx) {
        PrimTy::Isize
    } else {
        PrimTy::Usize
    };
    lower_expected(ctx, idx, HirTy::Prim(want))
}

/// `rok::core::index::<name>(args..)`.
fn index_fn(name: &str, args: Vec<Expr>, span: rokc_diagnostic::Span) -> Expr {
    let names = ["rok", "core", "index", name];
    let path = rokc_ast::Path {
        segments: names
            .iter()
            .map(|n| rokc_ast::PathSegment {
                name: (*n).to_owned(),
                generics: Vec::new(),
            })
            .collect(),
        span,
    };
    Expr {
        kind: ExprKind::Call {
            callee: Box::new(Expr {
                kind: ExprKind::Path(path),
                span,
            }),
            args: args.into_iter().map(CallArg::Positional).collect(),
            trailing_closure: None,
        },
        span,
    }
}

fn isize_ty(span: rokc_diagnostic::Span) -> Ty {
    Ty {
        kind: AstTyKind::Path(rokc_ast::Path {
            segments: vec![rokc_ast::PathSegment {
                name: "isize".to_owned(),
                generics: Vec::new(),
            }],
            span,
        }),
        span,
    }
}

/// `e as isize`.
fn as_isize(e: Expr, span: rokc_diagnostic::Span) -> Expr {
    Expr {
        kind: ExprKind::Cast(Box::new(e), isize_ty(span)),
        span,
    }
}

/// Spec 3.6: `rok::core::index::index_from_end(i as isize, len)` - a
/// signed position `i` < 0 is `len + i`.
fn from_end(i: Expr, len: Expr, span: rokc_diagnostic::Span) -> Expr {
    index_fn("index_from_end", vec![as_isize(i, span), len], span)
}

fn len_of(recv: &Expr, span: rokc_diagnostic::Span) -> Expr {
    Expr {
        kind: ExprKind::MethodCall {
            receiver: Box::new(recv.clone()),
            method: "len".to_owned(),
            type_args: Vec::new(),
            args: Vec::new(),
            trailing_closure: None,
        },
        span,
    }
}

/// Index `idx` of a fixed array of length `n`; a signed one counts
/// from the end (spec 3.6).
fn array_position(ctx: &mut LowerCtx<'_>, idx: &Expr, n: u32) -> HirExpr {
    let i = lower_position(ctx, idx);
    check_index_ty(ctx, &i);
    if !is_signed(&i.ty) {
        return i;
    }
    let span = i.span;
    let len = Expr {
        kind: ExprKind::Literal(AstLit::Int(format!("{n}usize"))),
        span,
    };
    let i = splice(ctx, i);
    lower_expr(ctx, &from_end(i, len, span))
}

/// Does `ty`'s `index` / `index_set` / `slice` take a position (a `List`,
/// `Slice` or `str`, not a `Dict<i64, V>`)?
fn takes_position(ctx: &LowerCtx<'_>, ty: &HirTy, method: &str) -> bool {
    fn text(t: &Ty) -> String {
        let AstTyKind::Path(p) = &t.kind else {
            return String::new();
        };
        let Some(seg) = p.segments.last() else {
            return String::new();
        };
        if seg.generics.is_empty() {
            return seg.name.clone();
        }
        let args: Vec<String> = seg.generics.iter().map(text).collect();
        format!("{}<{}>", seg.name, args.join(", "))
    }
    let param = match pick_method(ctx, ty, method, None) {
        Ok(Some(MethodPick::Concrete(id))) => {
            ctx.sig_by_id(id).param_tys.get(1).map(|t| ctx.ty_name(t))
        }
        Ok(Some(MethodPick::Generic(gm))) => {
            gm.def.params.get(1).and_then(|p| p.ty.as_ref()).map(text)
        }
        _ => None,
    };
    let want = if method == "slice" {
        "Range<usize>"
    } else {
        "usize"
    };
    param.is_some_and(|p| p == want || p.ends_with(&format!("::{want}")))
}

/// `recv.index(idx)` / `recv.index_set(idx, ..)` (spec 3.6). A signed index
/// on a positional receiver counts from the end via `index_from_end`;
/// unsigned ones are plain calls.
fn index_call(
    ctx: &mut LowerCtx<'_>,
    recv_h: HirExpr,
    method: &str,
    idx: &Expr,
    rest: &[CallArg],
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let mut args = rest.to_vec();
    if !takes_position(ctx, &recv_h.ty, method) {
        args.insert(0, CallArg::Positional(idx.clone()));
        return method_call_on(ctx, recv_h, method, &args, span);
    }
    let i = lower_position(ctx, idx);
    if !is_signed(&i.ty) {
        args.insert(0, CallArg::Positional(splice(ctx, i)));
        return method_call_on(ctx, recv_h, method, &args, span);
    }
    let t = tmp_path(ctx, "t", span);
    let i = splice(ctx, i);
    args.insert(0, CallArg::Positional(from_end(i, len_of(&t, span), span)));
    let recv = splice(ctx, recv_h);
    let call = Expr {
        kind: ExprKind::MethodCall {
            receiver: Box::new(t.clone()),
            method: method.to_owned(),
            type_args: Vec::new(),
            args,
            trailing_closure: None,
        },
        span,
    };
    lower_expr(ctx, &block_of(vec![let_stmt(&t, recv)], call, span))
}

pub(super) fn block_of(stmts: Vec<Stmt>, tail: Expr, span: rokc_diagnostic::Span) -> Expr {
    Expr {
        kind: ExprKind::Block(Block {
            stmts,
            trailing: Some(Box::new(tail)),
            span,
            label: None,
        }),
        span,
    }
}

/// `t[a:b:k]` with Python's semantics on a positional target (the
/// `Sliceable` impls clamp to the length):
/// - no step, unsigned bounds: `t.slice(a..b)`;
/// - no step, a signed bound counts from the end:
///   `{ let #t = t; #t.slice(slice_bound(a, #t.len())..b) }`;
/// - a step: `{ let #t = t; let #k: isize = k;
///   #t.slice(slice_range(a, b, #k, #t.len())).step(#k) }`, missing bounds
///   as `none` (their default depends on the step's sign).
fn lower_slice(
    ctx: &mut LowerCtx<'_>,
    target: &Expr,
    start: Option<&Expr>,
    end: Option<&Expr>,
    step: Option<&Expr>,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let t_h = lower_expected(ctx, target, HirTy::Unknown);
    if !takes_position(ctx, &t_h.ty, "slice") {
        let t = splice(ctx, t_h);
        return lower_expr(ctx, &slice_desugar(&t, start, end, step, span));
    }
    let a = start.map(|e| lower_position(ctx, e));
    let b = end.map(|e| lower_position(ctx, e));
    let signed = |h: &Option<HirExpr>| h.as_ref().is_some_and(|h| is_signed(&h.ty));
    if step.is_none() && !signed(&a) && !signed(&b) {
        let t = splice(ctx, t_h);
        let a = a.map(|h| splice(ctx, h));
        let b = b.map(|h| splice(ctx, h));
        return lower_expr(ctx, &slice_desugar(&t, a.as_ref(), b.as_ref(), None, span));
    }
    let t = tmp_path(ctx, "t", span);
    let recv = splice(ctx, t_h);
    let mut bound = |h: Option<HirExpr>| h.map(|h| (is_signed(&h.ty), splice(ctx, h)));
    let (a, b) = (bound(a), bound(b));
    let Some(k) = step else {
        let pos = |x: Option<(bool, Expr)>| {
            x.map(|(neg, e)| {
                if neg {
                    index_fn(
                        "slice_bound",
                        vec![as_isize(e, span), len_of(&t, span)],
                        span,
                    )
                } else {
                    e
                }
            })
        };
        let (a, b) = (pos(a), pos(b));
        let body = slice_desugar(&t, a.as_ref(), b.as_ref(), None, span);
        return lower_expr(ctx, &block_of(vec![let_stmt(&t, recv)], body, span));
    };
    let kt = tmp_path(ctx, "k", span);
    let opt = |x: Option<(bool, Expr)>| {
        x.map_or(
            Expr {
                kind: ExprKind::Literal(AstLit::None),
                span,
            },
            |(_, e)| as_isize(e, span),
        )
    };
    let range = index_fn(
        "slice_range",
        vec![opt(a), opt(b), kt.clone(), len_of(&t, span)],
        span,
    );
    let call = |recv: Expr, method: &str, arg: Expr| Expr {
        kind: ExprKind::MethodCall {
            receiver: Box::new(recv),
            method: method.to_owned(),
            type_args: Vec::new(),
            args: vec![CallArg::Positional(arg)],
            trailing_closure: None,
        },
        span,
    };
    let body = call(call(t.clone(), "slice", range), "step", kt.clone());
    // Any signed integer step widens to `isize`.
    let k_h = lower_expected(ctx, k, HirTy::Prim(PrimTy::Isize));
    let k_signed = is_signed(&k_h.ty);
    let k_e = splice(ctx, k_h);
    let let_k = Stmt {
        span,
        kind: StmtKind::Let {
            name: path_name(&kt),
            ty: Some(isize_ty(span)),
            init: if k_signed { as_isize(k_e, span) } else { k_e },
        },
    };
    lower_expr(ctx, &block_of(vec![let_stmt(&t, recv), let_k], body, span))
}

/// Spec 3.6.2: `t[a:b]` is `t.slice(a..b)` and `t[a:b:k]`
/// `t.slice(a..b).step(k)`; a missing `a` is `0`, a missing `b` is
/// `t.len()` with `t` evaluated once.
fn slice_desugar(
    target: &Expr,
    start: Option<&Expr>,
    end: Option<&Expr>,
    step: Option<&Expr>,
    span: rokc_diagnostic::Span,
) -> Expr {
    const TEMP: &str = "__rok_slice_target";
    let call = |recv: Expr, method: &str, args: Vec<Expr>| Expr {
        kind: ExprKind::MethodCall {
            receiver: Box::new(recv),
            method: method.to_owned(),
            type_args: Vec::new(),
            args: args.into_iter().map(CallArg::Positional).collect(),
            trailing_closure: None,
        },
        span,
    };
    let temp = end.is_none();
    let recv = if temp {
        Expr {
            kind: ExprKind::Path(rokc_ast::Path {
                segments: vec![rokc_ast::PathSegment {
                    name: TEMP.to_owned(),
                    generics: Vec::new(),
                }],
                span,
            }),
            span,
        }
    } else {
        target.clone()
    };
    let start = start.cloned().unwrap_or_else(|| Expr {
        kind: ExprKind::Literal(AstLit::Int("0usize".to_owned())),
        span,
    });
    let end = end
        .cloned()
        .unwrap_or_else(|| call(recv.clone(), "len", Vec::new()));
    let range = Expr {
        kind: ExprKind::Range {
            start: Some(Box::new(start)),
            end: Some(Box::new(end)),
            inclusive: false,
        },
        span,
    };
    let mut e = call(recv, "slice", vec![range]);
    if let Some(k) = step {
        e = call(e, "step", vec![k.clone()]);
    }
    if !temp {
        return e;
    }
    Expr {
        kind: ExprKind::Block(rokc_ast::Block {
            stmts: vec![rokc_ast::Stmt {
                kind: rokc_ast::StmtKind::Let {
                    name: TEMP.to_owned(),
                    ty: None,
                    init: target.clone(),
                },
                span,
            }],
            trailing: Some(Box::new(e)),
            span,
            label: None,
        }),
        span,
    }
}

/// The instances of generic struct `base` made so far.
fn struct_instantiations(ctx: &LowerCtx<'_>, base: &str) -> Vec<StructId> {
    let ids = LowerCtx::instantiations(&ctx.env.defs.struct_table.borrow(), base);
    super::incr::log(
        || format!("is|{base}"),
        || super::incr::Lookup::StructInsts(base.to_owned(), ids.clone()),
    );
    ids
}
