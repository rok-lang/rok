//! Parser unit tests focused on the surface grammar.

use rokc_ast::{AttrArg, AttrBody, ExprKind, Item, Literal, StmtKind};
use rokc_lexer::Lexer;

use crate::{Fixity, collect_prec_table, parse_program};

fn parse(src: &str) -> rokc_ast::Program {
    let lexer = Lexer::new(src, "test");
    let (tokens, lex_diags) = lexer.lex_all();
    assert!(lex_diags.is_empty(), "lex diagnostics: {lex_diags:?}");
    let (program, diags) = parse_program(&tokens, "test");
    assert!(diags.is_empty(), "parse diagnostics: {diags:?}");
    program
}

#[test]
fn attr_flag_only() {
    let p = parse("#[inline] fn f() { }");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    assert_eq!(f.attrs.len(), 1);
    assert_eq!(f.attrs[0].name(), "inline");
    assert!(matches!(f.attrs[0].body, AttrBody::Empty));
}

#[test]
fn attr_args_mixed() {
    let p = parse(r#"#[attr("+", key = 9, left)] fn f() { }"#);
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let attr = &f.attrs[0];
    assert_eq!(attr.name(), "attr");
    let AttrBody::Args(args) = &attr.body else {
        panic!()
    };
    assert_eq!(args.len(), 3);
    assert!(matches!(&args[0], AttrArg::Lit(Literal::Str(s)) if s == "+"));
    assert!(matches!(&args[1], AttrArg::KeyValue(k, Literal::Int(v)) if k == "key" && v == "9"));
    assert!(matches!(&args[2], AttrArg::Word(w) if w == "left"));
}

#[test]
fn attr_ranges() {
    let p = parse("#[niche(I32_MIN..=-1, ..5, 0.., f64::NAN)] struct S(i32);");
    let Item::Struct(s) = &p.items[0] else {
        panic!()
    };
    let AttrBody::Args(args) = &s.attrs[0].body else {
        panic!()
    };
    assert!(
        matches!(&args[0], AttrArg::Range { lo: Some(lo), hi: Some(hi), inclusive: true }
        if matches!(&**lo, AttrArg::Word(w) if w == "I32_MIN")
            && matches!(&**hi, AttrArg::Lit(Literal::Int(v)) if v == "-1"))
    );
    assert!(matches!(
        &args[1],
        AttrArg::Range {
            lo: None,
            hi: Some(_),
            inclusive: false
        }
    ));
    assert!(matches!(
        &args[2],
        AttrArg::Range {
            lo: Some(_),
            hi: None,
            inclusive: false
        }
    ));
    assert!(matches!(&args[3], AttrArg::Nested { path, .. } if path == &["f64", "NAN"]));
}

#[test]
fn attr_value_form() {
    let p = parse(r#"#[doc = "hi"] fn f() { }"#);
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let attr = &f.attrs[0];
    assert_eq!(attr.name(), "doc");
    assert!(matches!(&attr.body, AttrBody::Value(Literal::Str(s)) if s == "hi"));
}

#[test]
fn attr_nested() {
    let p = parse(r#"#[cfg(target_os = "linux")] fn f() { }"#);
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let attr = &f.attrs[0];
    assert_eq!(attr.name(), "cfg");
    let AttrBody::Args(args) = &attr.body else {
        panic!()
    };
    assert!(matches!(
        &args[0],
        AttrArg::KeyValue(k, Literal::Str(v)) if k == "target_os" && v == "linux"
    ));
}

#[test]
fn attr_nested_call() {
    let p = parse(r#"#[cfg_attr(unix, link(name = "m"))] fn f() { }"#);
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let AttrBody::Args(args) = &f.attrs[0].body else {
        panic!()
    };
    assert_eq!(args.len(), 2);
    let AttrArg::Nested { path, body } = &args[1] else {
        panic!()
    };
    assert_eq!(path, &vec!["link".to_owned()]);
    let AttrBody::Args(inner) = body.as_ref() else {
        panic!()
    };
    assert!(matches!(
        &inner[0],
        AttrArg::KeyValue(k, Literal::Str(v)) if k == "name" && v == "m"
    ));
}

/// Strip the wrapping `fn t() { return EXPR; }` and return the EXPR AST.
fn return_expr(p: &rokc_ast::Program) -> &rokc_ast::Expr {
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let body = f.body.as_ref().unwrap();
    let StmtKind::Return(Some(e)) = &body.stmts[0].kind else {
        panic!()
    };
    e
}

#[test]
fn pratt_left_assoc() {
    // `a - b - c` parses as `(a - b) - c`.
    let p = parse("fn t() { return a - b - c; }");
    let e = return_expr(&p);
    let ExprKind::OpInfix { op, lhs, rhs: _ } = &e.kind else {
        panic!()
    };
    assert_eq!(op, "-");
    let ExprKind::OpInfix { op: inner_op, .. } = &lhs.kind else {
        panic!("expected nested infix on lhs")
    };
    assert_eq!(inner_op, "-");
}

#[test]
fn pratt_precedence_mul_over_add() {
    // `a + b * c` parses as `a + (b * c)`.
    let p = parse("fn t() { return a + b * c; }");
    let e = return_expr(&p);
    let ExprKind::OpInfix { op, rhs, .. } = &e.kind else {
        panic!()
    };
    assert_eq!(op, "+");
    let ExprKind::OpInfix { op: rop, .. } = &rhs.kind else {
        panic!("expected `*` on rhs")
    };
    assert_eq!(rop, "*");
}

#[test]
fn pratt_prefix_neg() {
    // `-a + b` parses as `(-a) + b`, not `-(a + b)`.
    let p = parse("fn t() { return -a + b; }");
    let e = return_expr(&p);
    let ExprKind::OpInfix { op, lhs, .. } = &e.kind else {
        panic!()
    };
    assert_eq!(op, "+");
    let ExprKind::OpPrefix { op: pop, .. } = &lhs.kind else {
        panic!("expected unary `-` on lhs")
    };
    assert_eq!(pop, "-");
}

#[test]
fn as_binds_below_prefix_above_arith() {
    // Spec 3.2.3: `-x as i64` is `(-x) as i64`, `a + b as i64` is
    // `a + (b as i64)`.
    let p = parse("fn t() { return -x as i64; }");
    let ExprKind::Cast(inner, _) = &return_expr(&p).kind else {
        panic!()
    };
    assert!(matches!(inner.kind, ExprKind::OpPrefix { .. }));
    let p = parse("fn t() { return a + b is i32; }");
    let ExprKind::OpInfix { op, rhs, .. } = &return_expr(&p).kind else {
        panic!()
    };
    assert_eq!(op, "+");
    assert!(matches!(rhs.kind, ExprKind::Is(..)));
}

#[test]
fn infix_registration_rejects_precedence() {
    let reg = |src: &str| {
        let (tokens, _) = Lexer::new(src, "test").lex_all();
        let (program, diags) = parse_program(&tokens, "test");
        assert!(diags.is_empty(), "{diags:#?}");
        collect_prec_table(&program, &[]).1
    };
    let errs = reg("trait A { #[infix(\"+\", prec = 7, left)] fn add(self, r: Self) -> Self; }");
    assert!(
        errs[0].message.contains("fixed by the language"),
        "{errs:#?}"
    );
    let errs = reg("trait A { #[infix(\"+\", like = \"*\")] fn add(self, r: Self) -> Self; }");
    assert!(errs[0].message.contains("fixed precedence"), "{errs:#?}");
    let errs = reg("trait A { #[infix(\"⊕\")] fn f(self, r: Self) -> Self; }");
    assert!(errs[0].message.contains("needs `like"), "{errs:#?}");
    let errs = reg(
        "trait A { #[infix(\"<\")] #[infix(\"⊲\", like = \"+\")] fn lt(self, r: Self) -> bool; }",
    );
    assert!(
        errs[0].message.contains("share one precedence"),
        "{errs:#?}"
    );
}

#[test]
fn trait_with_self_method() {
    let p = parse(
        r#"trait Add<Rhs> {
            type Output;
            #[infix("+")]
            fn add(self, rhs: Rhs) -> Output;
        }"#,
    );
    let Item::Trait(t) = &p.items[0] else {
        panic!("expected trait, got {:?}", p.items)
    };
    assert_eq!(t.name, "Add");
    assert_eq!(t.items.len(), 2, "trait items: {:?}", t.items);
}

#[test]
fn two_traits_back_to_back() {
    let src = r#"trait Add<Rhs> {
    type Output;
    #[infix("+")]
    fn add(self, rhs: Rhs) -> Output;
}

trait Sub<Rhs> {
    type Output;
    #[infix("-")]
    fn sub(self, rhs: Rhs) -> Output;
}
"#;
    let lexer = Lexer::new(src, "test");
    let (tokens, _) = lexer.lex_all();
    let (program, diags) = parse_program(&tokens, "test");
    assert!(diags.is_empty(), "parse diagnostics: {diags:#?}");
    assert_eq!(program.items.len(), 2);
}

#[test]
fn collect_prec_from_trait_attrs() {
    let src = r#"trait Add<Rhs> {
    type Output;
    #[infix("+")]
    fn add(self, rhs: Rhs) -> Output;
}

trait Cmp<Rhs> {
    #[infix("<=")]
    #[infix("⊴", like = "<=")]
    fn le(self, rhs: Rhs) -> bool;
}

trait Neg {
    type Output;
    #[prefix("-")]
    fn neg(self) -> Output;
}
"#;
    let lexer = Lexer::new(src, "test");
    let (tokens, _) = lexer.lex_all();
    let (program, diags) = parse_program(&tokens, "test");
    assert!(diags.is_empty(), "{diags:#?}");
    let (table, errs) = collect_prec_table(&program, &[]);
    assert!(errs.is_empty(), "{errs:#?}");

    let plus = table.lookup("+", Fixity::Infix).expect("`+` registered");
    assert_eq!(plus.method_name, "add");

    // `<=` and the extension token `⊴` share the same method.
    assert!(table.lookup("<=", Fixity::Infix).is_some());
    assert!(table.lookup("⊴", Fixity::Infix).is_some());

    let neg = table.lookup("-", Fixity::Prefix).expect("prefix `-`");
    assert_eq!(neg.trait_name, "Neg");
}

#[test]
fn visibility_qualifiers_are_contextual() {
    use rokc_core::Visibility;
    // `sub` is reserved only inside `pub(...)`; a method named `sub` and
    // a `pub(sub)` qualifier coexist on the same item.
    let p = parse(
        r#"trait Sub<Rhs> {
            #[infix("-")]
            pub(sub) fn sub(self, rhs: Rhs) -> Self;
        }"#,
    );
    let Item::Trait(t) = &p.items[0] else {
        panic!()
    };
    let rokc_ast::TraitItem::Method(m) = &t.items[0] else {
        panic!()
    };
    assert_eq!(m.name, "sub");
    assert_eq!(m.vis, Visibility::Sub);
}

#[test]
fn attr_derive_words() {
    let p = parse("#[derive(Display, Clone, Eq)] fn f() { }");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let AttrBody::Args(args) = &f.attrs[0].body else {
        panic!()
    };
    assert_eq!(args.len(), 3);
    for (a, expected) in args.iter().zip(["Display", "Clone", "Eq"]) {
        assert!(matches!(a, AttrArg::Word(w) if w == expected));
    }
}

#[test]
fn struct_literal_decided_by_syntax_not_prescan() {
    // `size` in a comment must not register as a type name, or
    // `Foo { size: 1 }` parses as a trailing closure.
    let p = parse(
        "// compute the struct size\nstruct Foo { size: i32; }\n\
         fn f() { let v = Foo { size: 1 }; }",
    );
    let Item::Fn(f) = &p.items[1] else { panic!() };
    let StmtKind::Let { init, .. } = &f.body.as_ref().unwrap().stmts[0].kind else {
        panic!()
    };
    assert!(
        matches!(init.kind, ExprKind::StructInit { .. }),
        "{:?}",
        init.kind
    );
}

#[test]
fn struct_patterns_without_capitalisation() {
    // Non-ASCII / lower-case / imported type names work in patterns.
    let p = parse(
        "fn f(p: 점) { let 점 { x, y } = p; let m::pt { a, .. } = q; \
         match p { 점 { x, .. } => x, _ => 0 } }",
    );
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let stmts = &f.body.as_ref().unwrap().stmts;
    assert!(
        matches!(&stmts[0].kind, StmtKind::LetStruct { path, .. } if path.segments[0].name == "점")
    );
    assert!(matches!(&stmts[1].kind, StmtKind::LetStruct { path, .. } if path.segments.len() == 2));
    let m = f.body.as_ref().unwrap().trailing.as_ref().unwrap();
    let ExprKind::Match { arms, .. } = &m.kind else {
        panic!()
    };
    assert!(matches!(
        arms[0].pat.kind,
        rokc_ast::PatternKind::Struct { .. }
    ));
}

#[test]
fn catch_binding_followed_by_block() {
    let p = parse("fn f() { try { g(); } catch e { h(e); } }");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let e = f.body.as_ref().unwrap().trailing.as_ref().unwrap();
    // `try { .. } catch ..` = `catch` around `try` applied to the block.
    let ExprKind::Catch { body, catches, .. } = &e.kind else {
        panic!("{e:?}")
    };
    assert!(matches!(&body.kind, ExprKind::Try { operand, .. }
        if matches!(operand.kind, ExprKind::Block(_))));
    assert!(matches!(
        catches[0].pat.kind,
        rokc_ast::PatternKind::Binding { .. }
    ));
}

#[test]
fn try_covers_rhs_and_catch_binds_loosest() {
    let p = parse(
        "fn f() { let x = try a(b()) catch E::V { 0 } catch { 1 }; \
         let y = try? c(); let z = try! c(); }",
    );
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let stmts = &f.body.as_ref().unwrap().stmts;
    let StmtKind::Let { init, .. } = &stmts[0].kind else {
        panic!()
    };
    let ExprKind::Catch { body, catches, .. } = &init.kind else {
        panic!("{init:?}")
    };
    // `try` covers `a(b())`, the catches wrap the whole `try`.
    assert!(
        matches!(&body.kind, ExprKind::Try { kind: rokc_ast::TryKind::Plain, operand }
        if matches!(operand.kind, ExprKind::Call { .. }))
    );
    assert!(matches!(
        catches[0].pat.kind,
        rokc_ast::PatternKind::EnumVariant { .. }
    ));
    assert!(matches!(
        catches[1].pat.kind,
        rokc_ast::PatternKind::Wildcard
    ));
    for (i, kind) in [
        (1, rokc_ast::TryKind::Optional),
        (2, rokc_ast::TryKind::Force),
    ] {
        let StmtKind::Let { init, .. } = &stmts[i].kind else {
            panic!()
        };
        assert!(matches!(&init.kind, ExprKind::Try { kind: k, .. } if *k == kind));
    }
}

#[test]
fn rethrows_desugars_to_error_generics() {
    let p = parse("fn ap(f: fn(i32) throws -> i32, x: i32) rethrows -> i32 { try f(x) }");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    assert!(matches!(&f.generics.params[..],
        [rokc_ast::GenericParam::Type { name, .. }] if name == "#E0"));
    assert!(matches!(&f.throws, rokc_ast::ThrowsKind::Typed(t) if t.len() == 1));
    let ty = f.params[0].ty.as_ref().unwrap();
    assert!(matches!(&ty.kind, rokc_ast::TyKind::Fn(_, ret, _)
        if matches!(ret.kind, rokc_ast::TyKind::Throws { .. })));
}

#[test]
fn untyped_throws_parses() {
    // `throws` alone = `throws (any Error)`; HIR resolves the marker.
    assert!(diags_of("fn f() throws -> i32 { 1 }").is_empty());
    assert!(diags_of("fn g(h: fn() throws -> i32) -> i32 { 1 }").is_empty());
    assert!(diags_of("fn g(h: fn() throws (E) -> i32) -> i32 { 1 }").is_empty());
}

/// All lexer + parser diagnostics for `src` (arith precedence table).
fn diags_of(src: &str) -> Vec<String> {
    let (tokens, mut diags) = Lexer::new(src, "test").lex_all();
    let (_p, parse_diags) = parse_program(&tokens, "test");
    diags.extend(parse_diags);
    diags.into_iter().map(|d| d.message).collect()
}

fn assert_rejected(src: &str, needle: &str) {
    let d = diags_of(src);
    assert!(
        d.iter().any(|m| m.contains(needle)),
        "{src:?}: expected `{needle}` in {d:#?}"
    );
}

fn fn_param_ty(p: &rokc_ast::Program) -> &rokc_ast::Ty {
    let Item::Fn(f) = &p.items[0] else { panic!() };
    f.params[0].ty.as_ref().unwrap()
}

#[test]
fn cast_target_does_not_swallow_lt_or_pipe() {
    let p = parse("fn t() { return a as i64 < b; }");
    let ExprKind::OpInfix { op, lhs, .. } = &return_expr(&p).kind else {
        panic!()
    };
    assert_eq!(op, "<");
    assert!(matches!(lhs.kind, ExprKind::Cast(..)));

    let p = parse("fn t() { return a as i32 | b; }");
    let ExprKind::OpInfix { op, lhs, .. } = &return_expr(&p).kind else {
        panic!()
    };
    assert_eq!(op, "|");
    assert!(matches!(lhs.kind, ExprKind::Cast(..)));

    // Real generic args and parenthesised sums still work.
    let p = parse("fn t() { return a as Foo<Bar<i32>>; }");
    let ExprKind::Cast(_, ty) = &return_expr(&p).kind else {
        panic!()
    };
    let rokc_ast::TyKind::Path(path) = &ty.kind else {
        panic!()
    };
    assert_eq!(path.segments[0].generics.len(), 1);
    let p = parse("fn t() { return a as (i32 | none); }");
    let ExprKind::Cast(_, ty) = &return_expr(&p).kind else {
        panic!()
    };
    assert!(matches!(ty.kind, rokc_ast::TyKind::Sum(_)));
}

#[test]
fn unclosed_generic_args_rejected() {
    assert_rejected("fn f(x: Vec<i32) { }", "expected `>`");
    assert_rejected("struct S<T { x: T; }", "expected `>`");
}

#[test]
fn split_closing_angles() {
    let p = parse("fn f(x: Vec<Vec<i32>>, y: A<B<C<D>>>) { let z: Vec<i32>= x; }");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    assert_eq!(f.params.len(), 2);
}

#[test]
fn impl_trait_alias_generics_and_where_are_kept() {
    let p = parse(
        "impl<T: Add<T>> Pair<T> where T: Copy, T::Out: Show { } \
         trait Ord<R>: Eq + Cmp<R> where R: Copy { } \
         type Map<K, V = Vec<K>> = Tree<K, V>; \
         fn g<T>(x: T) -> T where T: Clone + Copy { x }",
    );
    let Item::Impl(imp) = &p.items[0] else {
        panic!()
    };
    assert_eq!(imp.generics.params.len(), 1);
    assert_eq!(imp.where_clause.len(), 2);
    let Item::Trait(tr) = &p.items[1] else {
        panic!()
    };
    assert_eq!(tr.generics.params.len(), 1);
    assert_eq!(tr.supertraits.len(), 2);
    assert_eq!(tr.where_clause.len(), 1);
    let Item::TypeAlias(alias) = &p.items[2] else {
        panic!()
    };
    assert_eq!(alias.generics.params.len(), 2);
    let Item::Fn(func) = &p.items[3] else {
        panic!()
    };
    assert_eq!(func.where_clause[0].bounds.len(), 2);
}

#[test]
fn paren_type_grouping_vs_tuple() {
    use rokc_ast::TyKind;
    assert!(matches!(
        fn_param_ty(&parse("fn f(x: (i32)) { }")).kind,
        TyKind::Path(_)
    ));
    assert!(
        matches!(&fn_param_ty(&parse("fn f(x: (i32,)) { }")).kind, TyKind::Tuple(v) if v.len() == 1)
    );
    assert!(
        matches!(&fn_param_ty(&parse("fn f(x: ()) { }")).kind, TyKind::Tuple(v) if v.is_empty())
    );
    // `fn() -> A | B` is `(fn() -> A) | B` (spec 2.1.10).
    let p = parse("fn f(x: fn(i32) -> i32 | none) { }");
    let ty = fn_param_ty(&p);
    let TyKind::Sum(v) = &ty.kind else {
        panic!("{ty:?}")
    };
    assert!(matches!(v[0].kind, TyKind::Fn(..)));
}

#[test]
fn optional_type_suffix() {
    use rokc_ast::TyKind;
    let pty = |s: &str| fn_param_ty(&parse(s)).clone();
    // `T?` is `#nest<T> | none`.
    let opt_of = |t: &rokc_ast::Ty| match &t.kind {
        TyKind::Sum(v) if v.len() == 2 && matches!(v[1].kind, TyKind::None) => {
            v[0].nest_inner().cloned()
        }
        _ => None,
    };
    // Postfix `?` binds tighter than `|`: `i32 | str?` is `i32 | (str?)`.
    let ty = pty("fn f(x: i32 | str?) { }");
    let TyKind::Sum(v) = &ty.kind else {
        panic!("{ty:?}")
    };
    assert!(matches!(v[0].kind, TyKind::Path(_)) && opt_of(&v[1]).is_some());
    // `i32??` is two layers.
    let ty = pty("fn f(x: i32??) { }");
    assert!(opt_of(&ty).and_then(|t| opt_of(&t)).is_some(), "{ty:?}");
    // `fn() -> i32?` returns an optional; `(fn() -> i32)?` is optional.
    let ty = pty("fn f(x: fn() -> i32?) { }");
    assert!(matches!(&ty.kind, TyKind::Fn(_, r, _) if opt_of(r).is_some()));
    let ty = pty("fn f(x: (fn() -> i32)?) { }");
    assert!(opt_of(&ty).is_some_and(|t| matches!(t.kind, TyKind::Fn(..))));
    // After `as`, `?` is the type's but `??` stays coalescing.
    let p = parse("fn t() { return a as i32?; }");
    assert!(matches!(&return_expr(&p).kind, ExprKind::Cast(_, t) if opt_of(t).is_some()));
    let p = parse("fn t() { return a as i32 ?? 0; }");
    assert!(matches!(
        &return_expr(&p).kind,
        ExprKind::NullishCoalesce(..)
    ));
}

#[test]
fn tuple_field_indices() {
    let p = parse("fn t() { return x.0.1; }");
    let ExprKind::TupleField(inner, 1) = &return_expr(&p).kind else {
        panic!()
    };
    assert!(matches!(inner.kind, ExprKind::TupleField(_, 0)));
    for bad in ["x.1u8", "x.0x1", "x.1_0", "x.01", "x.1e1", "x?.1u8"] {
        assert_rejected(
            &format!("fn t() {{ return {bad}; }}"),
            "invalid tuple field index",
        );
    }
}

#[test]
fn deep_nesting_is_a_diagnostic_not_a_crash() {
    // The depth cap must trip before the parser thread's stack does.
    let n = 5000;
    for src in [
        format!("fn f() {{ return {}1{}; }}", "(".repeat(n), ")".repeat(n)),
        format!("fn f() {}{}", "{".repeat(n), "}".repeat(n)),
        format!("fn f(x: {}i32{}) {{ }}", "(".repeat(n), ",)".repeat(n)),
        format!(
            "fn f() {{ match x {{ {}_{} => 1 }} }}",
            "(".repeat(n),
            ")".repeat(n)
        ),
        format!("fn f() {{ return {}1; }}", "-".repeat(n)),
        format!("fn f() {{ {}1{} }}", "if a { ".repeat(n), " }".repeat(n)),
        format!(
            "fn f() {{ {}1{} }}",
            "match a { _ => ".repeat(n),
            " }".repeat(n)
        ),
        format!("fn f() {{ return {}1; }}", "|| ".repeat(n)),
        format!("fn f() {{ return {}1{}; }}", "g(".repeat(n), ")".repeat(n)),
        format!("fn f() {{ return {}1{}; }}", "[".repeat(n), "]".repeat(n)),
        format!(
            "fn f() {{ return {}1{}; }}",
            "S { a: ".repeat(n),
            " }".repeat(n)
        ),
        format!("fn f(x: {}i32{}) {{ }}", "List<".repeat(n), ">".repeat(n)),
    ] {
        assert_rejected(&src, "nesting too deep");
    }
}

#[test]
fn open_ranges() {
    let range = |src: &str| {
        let p = parse(&format!("fn t() {{ return {src}; }}"));
        let ExprKind::Range {
            start,
            end,
            inclusive,
        } = return_expr(&p).kind.clone()
        else {
            panic!("{src}")
        };
        (start.is_some(), end.is_some(), inclusive)
    };
    assert_eq!(range("a..b"), (true, true, false));
    assert_eq!(range("..b"), (false, true, false));
    assert_eq!(range("a.."), (true, false, false));
    assert_eq!(range(".."), (false, false, false));
    assert_eq!(range("..=b + 1"), (false, true, true));
    let p = parse("fn t() { for i in 0.. { } f(xs[1..], ..ys); }");
    assert!(matches!(p.items[0], Item::Fn(_)));
    assert_rejected("fn t() { return a..b..c; }", "cannot be chained");
}

#[test]
fn unsafe_raw_strings_and_unsupported_forms() {
    let p = parse(
        r##"unsafe fn f() { let s = r#"a"b"#; unsafe { g(); } } extern "C" { unsafe fn h(); }"##,
    );
    assert_eq!(p.items.len(), 2);
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let StmtKind::Let { init, .. } = &f.body.as_ref().unwrap().stmts[0].kind else {
        panic!()
    };
    assert!(matches!(&init.kind, ExprKind::Literal(Literal::RawStr(s)) if s == "a\"b"));
    assert_rejected(
        "async unsafe fn f() { }",
        "modifiers go in the order `unsafe async yield`",
    );
    // Specs are read by HIR (by the hole's type); `e` and `${..}` parse.
    assert!(diags_of(r#"fn f() { let s = f"x = {x :e} {y:.${p}}"; }"#).is_empty());
    assert!(diags_of("fn f() { using x = g(), y = h(x) { x } }").is_empty());
}

#[test]
fn recovery_reports_each_mistake_once() {
    let src = "fn a() { let q = (1 + ; let r = 2; if r > 1 { f( } else { 3 }; return r; }\n\
               fn b( -> i32 { return 1; }\n\
               enum E { A { 1: i32 } }\n\
               fn c() { if (x + ) { } }\n\
               fn d() { let p = P { x: 1 }; return ~p; }";
    let d = diags_of(src);
    assert_eq!(d.len(), 5, "{d:#?}");
    assert!(d.iter().any(|m| m.contains("expected field name")));
    // No flag leaked out of the failed `if` head in `c`: the struct
    // literal in `d` still parses (no extra diagnostic for it).
}

#[test]
fn impl_const_and_extern_static_parse() {
    let d = diags_of(
        "impl S { const N: i32 = 1; fn f() { } } extern \"C\" { static mut X: i32; fn g(); }",
    );
    assert!(d.is_empty(), "{d:#?}");
    let prog = parse("extern \"C\" { static mut X: i32; static Y: u8; }");
    let Item::Extern(blk) = &prog.items[0] else {
        panic!("expected extern block");
    };
    assert!(matches!(&blk.items[..], [
        rokc_ast::ExternItem::Static(sx),
        rokc_ast::ExternItem::Static(sy),
    ] if sx.is_mut && !sy.is_mut && sx.name == "X"));
}

#[test]
fn any_trait_type_forms() {
    use rokc_ast::TyKind;
    let p = parse("fn f(a: any, b: any Error | none, c: any (A + B)) {}");
    let Item::Fn(f) = &p.items[0] else {
        panic!("expected fn");
    };
    let kinds: Vec<&TyKind> = f
        .params
        .iter()
        .map(|p| &p.ty.as_ref().unwrap().kind)
        .collect();
    assert!(matches!(kinds[0], TyKind::Any));
    // `any Error | none` is `(any Error) | none`.
    let TyKind::Sum(parts) = kinds[1] else {
        panic!("expected a sum, got {:?}", kinds[1]);
    };
    assert!(matches!(&parts[0].kind, TyKind::AnyTrait(b) if b.len() == 1));
    assert!(matches!(parts[1].kind, TyKind::None));
    assert!(matches!(kinds[2], TyKind::AnyTrait(b) if b.len() == 2));
}

#[test]
fn block_like_statement_needs_no_semicolon() {
    // `if c {} -1` is two statements; `f() g()` lacks a `;`.
    let p = parse("fn t() { if c { a(); } -1; match x { _ => {} } loop { break; } }");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let body = f.body.as_ref().unwrap();
    assert_eq!(body.stmts.len(), 3, "{:#?}", body.stmts);
    assert!(body.trailing.is_some());
    assert_rejected("fn t() { f() g(); }", "expected `;`");
    // A postfix chain continues a block-like expression.
    let p = parse("fn t() -> i32 { match x { _ => y }.len() }");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    assert!(f.body.as_ref().unwrap().trailing.is_some());
    // ...but not a call: `(a - b) as T` after it starts a new statement.
    let p = parse("fn t() -> u8 { if c { return 0; } (a - b) as u8 }");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let body = f.body.as_ref().unwrap();
    assert_eq!(body.stmts.len(), 1);
    assert!(matches!(
        body.trailing.as_deref().map(|e| &e.kind),
        Some(ExprKind::Cast(..))
    ));
}

/// Parse with the meta blocks of `src` registered (the driver's pre-pass).
fn parse_meta(src: &str) -> (rokc_ast::Program, Vec<rokc_diagnostic::RokDiagnostic>) {
    let (tokens, _) = Lexer::new(src, "test").lex_all();
    let mut table = crate::MetaTable::default();
    for d in crate::collect_meta_blocks(&tokens, "test") {
        assert!(table.add(d).is_none());
    }
    crate::parse_program_meta(
        &tokens,
        "test",
        "",
        &crate::ExtOps::default(),
        &std::sync::Arc::new(table),
        &std::sync::Arc::default(),
        None,
    )
}

#[test]
fn meta_derive_expands_fields() {
    let (p, diags) = parse_meta(
        "#[impl_derive(Tr)] pub @derive { impl Tr for @Self { fn n(self) -> i32 { \
         let n = 0; @for_fields { n = n + self.@field; } return n; } } }\n\
         #[derive(Tr)] struct S { a: i32; b: i32; }",
    );
    assert!(diags.is_empty(), "{diags:?}");
    // The struct (its user derive dropped from `#[derive]`) and the impl.
    let Item::Struct(s) = &p.items[0] else {
        panic!()
    };
    assert!(s.attrs.is_empty());
    let Item::Impl(i) = &p.items[1] else { panic!() };
    assert_eq!(i.items.len(), 1);
}

#[test]
fn meta_macro_block_registers() {
    // `#[impl_macro(name)] @macro { (matcher) => { template } }` (spec 6.8.12)
    // is collected as a Macro meta definition and registered as `name!`.
    let src = "#[impl_macro(dup)] pub @macro { (@e: expr) => { (@e, @e) } }\n";
    let (tokens, _) = Lexer::new(src, "test").lex_all();
    let defs = crate::collect_meta_blocks(&tokens, "test");
    assert_eq!(defs.len(), 1, "{defs:?}");
    assert!(matches!(defs[0].kind, crate::MetaKind::Macro));
    assert_eq!(defs[0].name, "dup");
    let mut table = crate::MetaTable::default();
    assert!(table.add(defs[0].clone()).is_none());
    assert!(table.find_macro("dup").is_some());
    // A macro is not an item-attached derive / attribute trigger.
    assert!(!table.defines("dup"));
}

#[test]
fn macro_expr_simple() {
    let (p, diags) = parse_meta(
        "#[impl_macro(twice)] pub @macro { (@e: expr) => { (@e, @e) } }\n\
         fn f() -> (i32, i32) { return twice!(1 + 2); }",
    );
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(p.items.last(), Some(Item::Fn(_))));
}

#[test]
fn macro_expr_multi_arg_and_hygiene() {
    // Three `expr` fragments, and a template-local `v` that must be made
    // hygienic (not clash with a caller `v`).
    let (_, diags) = parse_meta(
        "#[impl_macro(clamp)] pub @macro { (@x: expr, @lo: expr, @hi: expr) => { \
         { let v = @x; if v < @lo { @lo } else if v > @hi { @hi } else { v } } } }\n\
         fn f(v: i32) -> i32 { return clamp!(v, 0, 9); }",
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn macro_expr_repetition() {
    let (_, diags) = parse_meta(
        "#[impl_macro(list)] pub @macro { (@rep[sep = \",\"] { @x: expr }) => { \
         { let acc = List::new(); @for @x { acc.push(@x); } acc } } }\n\
         fn f() -> List<i32> { return list![1, 2, 3]; }",
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn macro_item_repetition_impls() {
    let (p, diags) = parse_meta(
        "#[impl_macro(imp)] pub @macro { (@rep[sep = \",\"] { @t: ty }) => { \
         @for @t { impl Foo for @t {} } } }\n\
         imp!(i8, i16, i32);",
    );
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(p.items.len(), 3, "{:?}", p.items);
    assert!(p.items.iter().all(|i| matches!(i, Item::Impl(_))));
}

#[test]
fn macro_item_name_synthesis() {
    let (p, diags) = parse_meta(
        "#[impl_macro(mk)] pub @macro { (@rep[sep = \",\"] { @n: ident }) => { \
         @for @n { fn @concat(do_, @n)() {} } } }\n\
         mk!(a, b);",
    );
    assert!(diags.is_empty(), "{diags:?}");
    let names: Vec<&str> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Fn(f) => Some(f.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["do_a", "do_b"], "{names:?}");
}

#[test]
fn macro_stmt_position() {
    // `name!(..);` at statement position splices in a `let` whose binding
    // comes from a metavariable (so it is visible to the caller's code).
    let (_, diags) = parse_meta(
        "#[impl_macro(decl)] pub @macro { (@n: ident, @v: expr) => { let @n = @v; } }\n\
         fn f() -> i64 { decl!(x, 41i64); return x + 1i64; }",
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn macro_type_position() {
    let (_, diags) = parse_meta(
        "#[impl_macro(pair_of)] pub @macro { (@t: ty) => { (@t, @t) } }\n\
         fn f(x: pair_of!(i32)) -> i32 { return x.0; }",
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn macro_pattern_position() {
    let (_, diags) = parse_meta(
        "#[impl_macro(both)] pub @macro { (@a: pat, @b: pat) => { (@a, @b) } }\n\
         fn f(p: (i32, i32)) -> i32 { match p { both!(a, b) => { return a + b; } } }",
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn macro_expansion_is_recorded() {
    // The expansion is recorded (surfaced by `rokc --emit expanded`, the
    // "expand in place" source, spec 6.8.12 tooling).
    let (p, diags) = parse_meta(
        "#[impl_macro(twice)] pub @macro { (@e: expr) => { (@e, @e) } }\n\
         fn f() -> (i32, i32) { return twice!(1i32); }",
    );
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        p.expanded
            .iter()
            .any(|e| e.contains("expanded from twice!")),
        "{:?}",
        p.expanded
    );
}

#[test]
fn macro_string_interpolation() {
    // `@name` inside a string interpolates the binding (per-element in @for).
    let (p, diags) = parse_meta(
        "#[impl_macro(tag)] pub @macro { (@rep[sep = \",\"] { @t: ident }) => { \
         @for @t { fn @concat(g_, @t)() -> str { return \"@t\"; } } } }\n\
         tag!(a, b);",
    );
    assert!(diags.is_empty(), "{diags:?}");
    let joined = p.expanded.join("\n");
    assert!(
        joined.contains("\"a\"") && joined.contains("\"b\""),
        "{joined}"
    );
}

#[test]
fn macro_expr_no_rule_matches() {
    let (_, diags) = parse_meta(
        "#[impl_macro(one)] pub @macro { (@a: expr, @b: expr) => { (@a, @b) } }\n\
         fn f() -> i32 { return one!(42); }",
    );
    assert!(
        diags.iter().any(|d| format!("{d:?}").contains("no rule")),
        "{diags:?}"
    );
}

#[test]
fn meta_macro_rejects_builtin_bang_name() {
    // `#[impl_macro]` cannot take a reserved built-in `!`-form name.
    let src = "#[impl_macro(cfg)] pub @macro { (@e: expr) => { @e } }\n";
    let (tokens, _) = Lexer::new(src, "test").lex_all();
    let defs = crate::collect_meta_blocks(&tokens, "test");
    assert_eq!(defs.len(), 1, "{defs:?}");
    let mut table = crate::MetaTable::default();
    // `cfg!` is a reserved built-in form, so registration is refused.
    assert!(table.add(defs[0].clone()).is_some());
}

#[test]
fn meta_attribute_late_reserved_name_warns() {
    // A name reserved in 0.1.2 warns but still registers; an older one is
    // refused.
    let src = "#[impl_attribute(bench)] pub @attribute { @item }\n\
               #[impl_attribute(test)] pub @attribute { @item }\n";
    let (tokens, _) = Lexer::new(src, "test").lex_all();
    let defs = crate::collect_meta_blocks(&tokens, "test");
    assert_eq!(defs.len(), 2, "{defs:?}");
    let mut table = crate::MetaTable::default();
    let d = table.add(defs[0].clone()).expect("a warning");
    assert_eq!(d.severity, rokc_diagnostic::Severity::Warning, "{d:?}");
    assert!(d.message.contains("my_bench"), "{d:?}");
    assert!(table.knows_attribute("bench"));
    let d = table.add(defs[1].clone()).expect("an error");
    assert_eq!(d.severity, rokc_diagnostic::Severity::Error, "{d:?}");
    assert!(!table.knows_attribute("test"));
}

#[test]
fn meta_error_points_at_template_and_use() {
    let (_, diags) = parse_meta(
        "#[impl_derive(Tr)] pub @derive {\n  impl Tr for @Self { fn f() { @nope; } }\n}\n\
         #[derive(Tr)]\nstruct S { a: i32; }",
    );
    let d = diags.first().expect("an error");
    assert!(
        d.message.contains("unknown template token `@nope`"),
        "{d:?}"
    );
    assert_eq!(d.primary.as_ref().map(|l| l.span.line), Some(2));
    assert_eq!(d.secondary.first().map(|l| l.span.line), Some(4));
}

#[test]
fn meta_hygiene_renames_locals_only() {
    // `x` / `i` are template locals; `Other` (after `impl .. for`) and
    // `len` (a member) are not.
    let (p, diags) = parse_meta(
        "#[impl_derive(Tr)] pub @derive { impl Tr for Other { fn f() { \
         let x = 1; for i in 0..x { x = i.len(); } } } }\n\
         #[derive(Tr)] struct S { a: i32; }",
    );
    assert!(diags.is_empty(), "{diags:?}");
    let text = format!("{:?}", p.items[1]);
    assert!(text.contains("\"x__meta\"") && text.contains("\"i__meta\""));
    assert!(text.contains("\"Other\"") && text.contains("\"len\""));
    assert!(!text.contains("Other__meta") && !text.contains("len__meta"));
}

/// `--emit=source` (spec 9.9.10): printing a parsed file and parsing
/// the print gives the same print back, for every library and test
/// case file that parses without the extension operators.
#[test]
fn printed_ast_parses_back() {
    // Deep case files (a 2000-term chain) need the compiler's big stack.
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(printed_ast_parses_back_body)
        .unwrap()
        .join()
        .unwrap();
}

fn printed_ast_parses_back_body() {
    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rok") {
                out.push(p);
            }
        }
    }
    let try_parse = |src: &str| {
        let (tokens, lex) = Lexer::new(src, "t").lex_all();
        let (program, diags) = parse_program(&tokens, "t");
        match lex.iter().chain(&diags).next() {
            None => Ok(program),
            Some(d) => Err(format!("{d:?}")),
        }
    };
    // Without the `// #[derive(..)]` notes (comments do not parse back).
    let print = |p: &rokc_ast::Program| {
        let text = rokc_ast::print::print_file(&p.attrs, &p.items, &p.stmts);
        let lines = text.lines().filter(|l| !l.trim_start().starts_with("//"));
        lines.collect::<Vec<_>>().join("\n")
    };
    // Groups the print must keep: without them the text reads back as a
    // trailing closure, a construct body or another precedence.
    let kept = [
        "fn f() { let a = (g(x)) | y; }",
        "fn f() { let a = (g(x)) | y | z; }",
        "fn f() { let a = (o.m(x)) | y; }",
        "fn f() { let a = w | (g(x)) | y; }",
        "fn f() { let a = (o?.m(x)) | y; }",
        "fn f() { let a = (o?.m(x).n(y)) | z; }",
        "fn f() { let a = (g(x) { 1 }) | y; }",
        "fn f() { let a = (g |x| { x }) | y; }",
        "fn f() { let a = g | x | ({ 1: 2 }); }",
        "fn f() { let a = o.g | x | ({ 1: 2 }); }",
        "fn f() { if (g(x)) | y { } }",
        "fn f() { while (g(x)) | y { } }",
        "fn f() { if a == (S { b: 1 }) { } }",
        "fn f() { if (S { b: 1 }).ok() { } }",
        "fn f() { match (S { b: 1 }) { _ => 0, } }",
        "fn f() { for i in (S { b: 1 }).it() { } }",
        "fn f() { if g(S { b: 1 }) { } }",
        "fn f() { if (g { 1 }) { } }",
        "fn f() { let a = (b + c) * d - (e - f); }",
        "fn f() { let a = -(b + c) as i64 < (d as i64); }",
        "fn f() { let a = (b || c) && !(d | e); }",
        "fn f() { let a = (|x| x)(1) + (b..c).len(); }",
        "fn f() { let a = (match b { _ => 1, })[0]; }",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut all = Vec::new();
    files(&root.join("library"), &mut all);
    files(&root.join("compiler/rokc/tests/cases"), &mut all);
    let mut sources: Vec<(String, String)> =
        kept.iter().map(|&s| (s.to_owned(), s.to_owned())).collect();
    let inline = sources.len();
    for f in &all {
        sources.push((f.display().to_string(), std::fs::read_to_string(f).unwrap()));
    }
    let (mut checked, mut bad) = (0, Vec::new());
    for (k, (name, src)) in sources.iter().enumerate() {
        let p = match try_parse(src) {
            Ok(p) => p,
            Err(d) if k < inline => panic!("{name}: {d}"),
            Err(_) => continue,
        };
        checked += 1;
        let once = print(&p);
        match try_parse(&once) {
            Ok(again) if print(&again) == once => {}
            Ok(again) => bad.push(format!("{name}: differs\n{once}\n----\n{}", print(&again))),
            Err(d) => bad.push(format!("{name}: does not parse: {d}\n{once}")),
        }
    }
    assert!(checked > 100, "{checked}");
    assert!(
        bad.is_empty(),
        "{} of {checked}:\n{}",
        bad.len(),
        bad.join("\n=====\n")
    );
}

#[test]
fn expr_file() {
    let parse = |src: &str| {
        let (tokens, _) = Lexer::new(src, "t").lex_all();
        crate::parse_expr_file(&tokens, "t")
    };
    let (e, d) = parse("Package {\n    deps: { \"a\": Dep::git(\"u\", tag: \"v\") },\n}\n");
    assert!(d.is_empty(), "{d:?}");
    assert!(matches!(e.unwrap().kind, ExprKind::StructInit { .. }));
    let (_, d) = parse("Package {} x");
    assert!(
        d[0].message.contains("expected the end of the file"),
        "{d:?}"
    );
}

#[test]
fn compound_expr_spans_cover_all_tokens() {
    let src = "fn f() { let v = a.b(1, 2) + -c[3] as i64; }";
    let p = parse(src);
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let body = f.body.as_ref().unwrap();
    let StmtKind::Let { init, .. } = &body.stmts[0].kind else {
        panic!()
    };
    let text = |s: rokc_diagnostic::Span| &src[s.start as usize..s.end as usize];
    assert_eq!(text(init.span), "a.b(1, 2) + -c[3] as i64");
    let ExprKind::OpInfix { lhs, rhs, .. } = &init.kind else {
        panic!()
    };
    assert_eq!(text(lhs.span), "a.b(1, 2)");
    assert_eq!(text(rhs.span), "-c[3] as i64");
}

/// `<E|W>:<line>:<message up to its first colon>` of parsing `src`.
fn doc_diags(src: &str) -> Vec<String> {
    let (tokens, lex) = Lexer::new(src, "test").lex_all();
    assert!(lex.is_empty(), "{lex:?}");
    let (_, diags) = parse_program(&tokens, "test");
    diags
        .iter()
        .map(|d| {
            let head = if d.is_error() { "E" } else { "W" };
            let line = d.primary.as_ref().map_or(0, |p| p.span.line);
            let word = d.message.split(':').next().unwrap_or("");
            format!("{head}:{line}:{word}")
        })
        .collect()
}

#[test]
fn block_doc_comments_are_doc_attributes() {
    let p =
        parse("/*! Mod. */\n/**\n * Adds \"one\".\n * More.\n */\nfn f() {\n    //! Body.\n}\n");
    assert!(matches!(&p.attrs[0].body, AttrBody::Value(Literal::Str(s)) if s == "Mod."));
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let docs: Vec<&str> = f
        .attrs
        .iter()
        .filter_map(|a| match &a.body {
            AttrBody::Value(Literal::Str(s)) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(docs, ["Adds \\\"one\\\".\\nMore.", "Body."]);
}

#[test]
fn misplaced_doc_comments() {
    assert_eq!(
        doc_diags("/// a\nfn f() {}\nmod m { //! b\n}\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        doc_diags("/// s\nlet x = /** e */ 1;\nmatch x {\n    /// arm\n    _ => {}\n}\n"),
        [
            "W:1:unused doc comment",
            "W:2:unused doc comment",
            "W:4:unused doc comment"
        ]
    );
    assert_eq!(
        doc_diags("fn f(/// p\n x: i32) {\n    /// none\n}\n//! late\nfn g() {}\n/// end\n"),
        [
            "E:1:a doc comment cannot document a parameter; use `//` for a plain comment",
            "E:3:doc comment documents nothing",
            "E:5:expected outer doc comment",
            "E:7:doc comment documents nothing"
        ]
    );
}

#[test]
fn macro_probe_multirule() {
    // Several rules; the first matching one is chosen by arity.
    let (_, d) = parse_meta(
        "#[impl_macro(pick)] pub @macro { (@a: expr) => { (@a,) } (@a: expr, @b: expr) => { (@a, @b) } }\n\
         fn f() -> (i32, i32) { return pick!(1i32, 2i32); }",
    );
    assert!(d.is_empty(), "multirule: {d:?}");
}

#[test]
fn macro_probe_trailing_sep() {
    let (_, d) = parse_meta(
        "#[impl_macro(li)] pub @macro { (@rep[sep = \",\"] { @x: expr }) => { \
         { let acc = List::new(); @for @x { acc.push(@x); } acc } } }\n\
         fn f() -> List<i32> { return li![1, 2, 3,]; }",
    );
    assert!(d.is_empty(), "trailing sep: {d:?}");
}

#[test]
fn macro_probe_nested_rep() {
    // Nested @rep: rows of columns.
    let (_, d) = parse_meta(
        "#[impl_macro(grid)] pub @macro { (@rep[sep = \";\"] { @rep[sep = \",\"] { @x: expr } }) => { \
         { let n = 0i32; @for @x { n = n + @x; } n } } }\n\
         fn f() -> i32 { return grid![1, 2; 3, 4]; }",
    );
    assert!(d.is_empty(), "nested rep: {d:?}");
}

#[test]
fn macro_rejects_shape_tokens() {
    // Type-driven template tokens (@apply_if, @Self, @field, ..) are not
    // valid in a macro; it has no shape to test (spec 6.8.12).
    let (_, d) = parse_meta(
        "#[impl_macro(ai)] pub @macro { (@e: expr) => { @apply_if(true) { @e } } }\n\
         fn f() -> i32 { return ai!(5i32); }",
    );
    assert!(
        d.iter()
            .any(|x| format!("{x:?}").contains("not valid in a macro template")),
        "{d:?}"
    );
}

#[test]
fn macro_unknown_fragment_errors() {
    let (_, d) = parse_meta(
        "#[impl_macro(bad)] pub @macro { (@x: wat) => { @x } }\n\
         fn f() -> i32 { return bad!(1i32); }",
    );
    assert!(
        d.iter()
            .any(|x| format!("{x:?}").contains("unknown fragment")),
        "{d:?}"
    );
}

#[test]
fn macro_tuple_rep() {
    // A @rep of `( @t, @v )` tuples (bounds / defaults carried per type).
    let (p, d) = parse_meta(
        "#[impl_macro(defs)] pub @macro { (@rep[sep = \",\"] { (@t: ty, @v: expr) }) => { \
         @for @t { fn @concat(z_, @t)() -> @t { @v } } } }\n\
         defs!((i8, 0i8), (bool, false));",
    );
    assert!(d.is_empty(), "{d:?}");
    let names: Vec<&str> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Fn(f) => Some(f.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["z_i8", "z_bool"], "{names:?}");
}

#[test]
fn macro_json_int_shape_parses() {
    // The shape used by json/toml `convert.rok` (ToJson/FromJson per int):
    // `let n` is hygienic; `@t::try_from`, `int_of::<@t>` resolve on @t.
    let (p, d) = parse_meta(
        "#[impl_macro(ji)] pub @macro { (@rep[sep = \",\"] { @t: ty }) => { \
         @for @t { \
         impl ToJson for @t { fn to_json(self) -> Value { Value::Int(Int::from(self)) } } \
         impl FromJson for @t { fn from_json(v: Value) throws (JsonError) -> @t { \
         let n = try int_of::<@t>(v); try @t::try_from(n) catch _ { throw oor::<@t>(n); } } } } } }\n\
         ji!(i8, u32);",
    );
    assert!(d.is_empty(), "{d:?}");
    // 2 types x 2 impls = 4 impl items.
    assert_eq!(
        p.items
            .iter()
            .filter(|i| matches!(i, Item::Impl(_)))
            .count(),
        4,
        "{:?}",
        p.items
    );
}

#[test]
fn last_stmt_semi_is_optional() {
    // Same spans either way: `;` and a space are one byte each.
    for body in [
        "x = 1;",
        "a[f()] += 1;",
        "p.x <<= 2;",
        "let y = 1;",
        "let (a, b) = t;",
        "let P { x, y } = p;",
        "let v: i32 = o else { return; };",
        "return;",
        "return 1;",
        "while c { break; }",
        "loop { continue; }",
        "throw e;",
    ] {
        let with = format!("fn f() {{ {body}}}");
        let without = format!("fn f() {{ {}}}", body.replace(';', " "));
        assert_eq!(
            format!("{:?}", parse(&with).items),
            format!("{:?}", parse(&without).items),
            "{without}"
        );
    }
    assert_rejected("fn f() { x = 1 y = 2; }", "expected `;`");
    assert_rejected("fn f() { let a = 1 let b = 2; }", "expected `;`");
}

#[test]
fn ext_ops_scope_ascii_led_to_their_package() {
    let program = parse(
        r#"trait T {
            #[infix("<-", like = "+")] fn a(self, o: i32) -> i32;
            #[infix("⊕", like = "+")] fn b(self, o: i32) -> i32;
        }"#,
    );
    let (table, errs) = collect_prec_table(&program, &[]);
    assert!(errs.is_empty(), "{errs:?}");
    let mut dep = crate::PrecTable::new();
    for mut e in table.entries().cloned() {
        e.package = "dep".to_owned();
        dep.insert(e);
    }
    let ext = dep.ext_ops();
    assert_eq!(ext.in_package("dep").symbols(), ["⊕", "<-"]);
    // `<-` starts like `<`: other packages keep `a < -1`; `⊕` is build-wide.
    assert_eq!(ext.in_package("").symbols(), ["⊕"]);
}

/// The `(lint, message)` of each parse warning in `src`.
fn parse_warnings(src: &str) -> Vec<(String, String)> {
    let (tokens, _) = Lexer::new(src, "test").lex_all();
    let (_, diags) = parse_program(&tokens, "test");
    diags
        .into_iter()
        .map(|d| (d.lint.map(|l| *l).unwrap_or_default(), d.message))
        .collect()
}

#[test]
fn keyword_idents_only_where_names_become_keywords() {
    let src = "struct S { move: i32; }\n\
               fn gen(dyn: i32) -> i32 { dyn }\n\
               fn main() {\n    let s = S { move: 1 };\n    let r#do = s.move + f(defer: 2);\n}\n\
               #[impl_macro(x)] @macro { }\n";
    let got: Vec<String> = parse_warnings(src)
        .into_iter()
        .filter(|(l, _)| l == "keyword_idents")
        .map(|(_, m)| m)
        .collect();
    assert_eq!(
        got,
        [
            "`gen` is a keyword from Rok 0.2.0 on",
            "`dyn` is a keyword from Rok 0.2.0 on",
            "`dyn` is a keyword from Rok 0.2.0 on",
        ]
    );
}

#[test]
fn raw_contextual_keyword_is_a_name() {
    // `r#type = 1;` assigns a local; `type T = i32;` is an alias.
    let p = parse("fn f() {\n    let r#type = 0;\n    r#type = 1;\n    type T = i32;\n}\n");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let body = f.body.as_ref().unwrap();
    assert!(matches!(body.stmts[1].kind, StmtKind::Assign(..)));
}

#[test]
fn try_coalesce_and_closure_operator_warn() {
    let lints =
        |src: &str| -> Vec<String> { parse_warnings(src).into_iter().map(|(l, _)| l).collect() };
    let warn = ["try_coalesce_precedence"];
    assert_eq!(lints("fn f() { let a = try? g() ?? 1; }"), warn);
    for quiet in [
        "fn f() { let a = (try? g()) ?? 1; }",
        "fn f() { let a = try? (g() ?? 1); }",
        "fn f() { let a = try? g(); }",
        "fn f() { let a = try g() ?? 1; }",
    ] {
        assert!(lints(quiet).is_empty(), "{quiet}");
    }
    let warn = ["trailing_closure_operator"];
    assert_eq!(
        lints("fn f() {\n    run {\n        a();\n    }\n    - x;\n}\n"),
        warn
    );
    assert_eq!(
        lints("fn f() {\n    xs.each |x| { a(x); }\n    * y;\n}\n"),
        warn
    );
    for quiet in [
        "fn f() {\n    run {\n        a();\n    };\n    - x;\n}\n",
        "fn f() {\n    run { a(); } - x;\n}\n",
        "fn f() {\n    let v = run { a(); }\n        - x;\n}\n",
        "fn f() {\n    if c { a(); }\n    - x;\n}\n",
    ] {
        assert!(lints(quiet).is_empty(), "{quiet}");
    }
}

/// Parse `src` as package `package`, with the meta blocks of each
/// `(package, source)` registered under that package.
fn parse_in(
    package: &str,
    deps: &[(&str, &str)],
    src: &str,
) -> (rokc_ast::Program, Vec<rokc_diagnostic::RokDiagnostic>) {
    let mut table = crate::MetaTable::default();
    for (pkg, dep) in deps {
        let (tokens, _) = Lexer::new(dep, *pkg).lex_all();
        for mut d in crate::collect_meta_blocks(&tokens, pkg) {
            d.set_package(pkg);
            assert!(table.add(d).is_none());
        }
    }
    let (tokens, _) = Lexer::new(src, "test").lex_all();
    crate::parse_program_meta(
        &tokens,
        "test",
        package,
        &crate::ExtOps::default(),
        &std::sync::Arc::new(table),
        &std::sync::Arc::default(),
        None,
    )
}

const LOG_A: &str = "#[impl_macro(log)] pub @macro { (@e: expr) => { a(@e) } }\n\
                     #[impl_derive(Tr)] pub @derive { impl Tr for @Self {} }\n\
                     #[impl_attribute(traced)] pub @attribute { @item fn traced() {} }";
const LOG_B: &str = "#[impl_macro(log)] pub @macro { (@e: expr) => { b(@e) } }";

/// The body of `main`, as debug text.
fn main_body(p: &rokc_ast::Program) -> String {
    let f = p
        .items
        .iter()
        .find_map(|i| match i {
            Item::Fn(f) if f.name == "main" => Some(f),
            _ => None,
        })
        .unwrap();
    format!("{:?}", f.body)
}

#[test]
fn qualified_macro_picks_its_package() {
    let deps = [("a", LOG_A), ("b", LOG_B)];
    for (call, want) in [("a::log!(1);", "\"a\""), ("b::log!(1);", "\"b\"")] {
        let (p, diags) = parse_in("", &deps, &format!("fn main() {{ {call} }}"));
        assert!(diags.is_empty(), "{diags:?}");
        assert!(main_body(&p).contains(want), "{call}");
    }
    // A bare name two packages define: this package's, else the `use`d one.
    let (p, diags) = parse_in("b", &deps, "fn main() { log!(1); }");
    assert!(diags.is_empty(), "{diags:?}");
    assert!(main_body(&p).contains("\"b\""));
    let (p, diags) = parse_in("", &deps, "use a::log!;\nfn main() { log!(1); }");
    assert!(diags.is_empty(), "{diags:?}");
    assert!(main_body(&p).contains("\"a\""));
    assert!(p.items.iter().all(|i| !matches!(i, Item::Use(_))));
    let (_, diags) = parse_in("", &deps, "fn main() { log!(1); }");
    assert!(diags[0].message.contains("ambiguous"), "{diags:?}");
    let (_, diags) = parse_in("", &deps, "fn main() { b::nope!(1); }");
    assert!(
        diags[0]
            .message
            .contains("package `b` defines no macro `nope`"),
        "{diags:?}"
    );
}

#[test]
fn unqualified_meta_warns_once_per_name() {
    let deps = [("a", LOG_A)];
    let src = "#[derive(Tr)] struct S {}\n#[traced] fn f() {}\n\
               fn main() { log!(1); log!(2); }";
    let (_, diags) = parse_in("", &deps, src);
    let got: Vec<(Option<String>, String)> = diags
        .iter()
        .map(|d| (d.lint.as_deref().cloned(), d.notes.join(";")))
        .collect();
    let lint = Some("unqualified_meta".to_owned());
    assert_eq!(
        got,
        [
            (lint.clone(), "help: write `#[derive(a::Tr)]`".to_owned()),
            (lint.clone(), "help: write `#[a::traced]`".to_owned()),
            (
                lint,
                "help: write `a::log!` or add `use a::log!;`".to_owned()
            ),
        ]
    );
    // Qualified, imported, its own package's, or the standard library's:
    // quiet.
    for (pkg, src) in [
        (
            "",
            "#[derive(a::Tr)] struct S {}\n#[a::traced] fn f() {}\nfn main() { a::log!(1); }",
        ),
        (
            "",
            "use a::{log!, Tr};\n#[derive(Tr)] struct S {}\nfn main() { log!(1); }",
        ),
        ("", "use a::*;\n#[traced] fn f() {}\nfn main() { log!(1); }"),
        ("a", "fn main() { log!(1); }"),
    ] {
        let (_, diags) = parse_in(pkg, &deps, src);
        assert!(diags.is_empty(), "{src}: {diags:?}");
    }
    let (_, diags) = parse_in("", &[("rok::core", LOG_A)], "fn main() { log!(1); }");
    assert!(diags.is_empty(), "{diags:?}");
}
