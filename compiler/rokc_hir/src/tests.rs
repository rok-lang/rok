//! Unit tests: sum normalization, type lowering, rooted paths, closure
//! free names.

use rokc_ast::{ExprKind, Item, Program, StmtKind, Ty};
use rokc_ir_common::PrimTy;

use crate::HirTy;
use crate::lower::root_path;
use crate::ty::{resolve_ty, throws_ty};
use crate::walk::free_names;

fn parse(src: &str) -> Program {
    let (tokens, lex) = rokc_lexer::Lexer::new(src, "test").lex_all();
    assert!(lex.is_empty(), "{lex:?}");
    let (program, diags) = rokc_parser::parse_program(&tokens, "test");
    assert!(diags.is_empty(), "{diags:?}");
    program
}

/// The type written as `ty`, through a parameter of that type.
fn ty(ty: &str) -> Ty {
    let p = parse(&format!("fn f(x: {ty}) {{ }}"));
    let Item::Fn(f) = &p.items[0] else {
        panic!("{ty}")
    };
    f.params[0].ty.clone().expect("typed parameter")
}

const I32: HirTy = HirTy::Prim(PrimTy::I32);
const STR: HirTy = HirTy::Prim(PrimTy::Str);

#[test]
fn sums_are_sets_with_none_last() {
    let s = HirTy::sum([STR, I32, STR]);
    assert_eq!(s, HirTy::sum([I32, STR]));
    assert_eq!(
        HirTy::sum([HirTy::None, I32]),
        HirTy::Sum(vec![I32, HirTy::None])
    );
    // One member is that member.
    assert_eq!(HirTy::sum([I32, I32]), I32);
    // A nested sum spreads unless it overlaps a member: then it is a
    // layer of its own (a generic `T | none` with `T = i32 | none`).
    let inner = HirTy::sum([I32, HirTy::None]);
    assert_eq!(
        HirTy::sum([HirTy::sum([I32, STR]), HirTy::None]),
        HirTy::sum([I32, STR, HirTy::None])
    );
    assert_eq!(
        HirTy::sum([inner.clone(), HirTy::None]),
        HirTy::Sum(vec![inner, HirTy::None])
    );
}

#[test]
fn generic_terms_layer_against_written_terms_only() {
    let inner = HirTy::sum([I32, HirTy::None]);
    // `T | none`, `T = i32 | none`: the written `none` overlaps.
    assert_eq!(
        HirTy::layered(vec![HirTy::None], vec![inner.clone()]),
        HirTy::Sum(vec![inner.clone(), HirTy::None])
    );
    // `T | U`, `T = i32 | none`, `U = i32` or `U = none`: no written
    // term, so nothing nests.
    assert_eq!(HirTy::layered(vec![], vec![inner.clone(), I32]), inner);
    assert_eq!(
        HirTy::layered(vec![], vec![inner.clone(), HirTy::None]),
        inner
    );
    // `T | str | none`, `T = i32 | str`: `str` overlaps.
    let is = HirTy::sum([I32, STR]);
    assert_eq!(
        HirTy::layered(vec![STR, HirTy::None], vec![is.clone()]),
        HirTy::Sum(vec![is, STR, HirTy::None])
    );
    // `T | U | none`, `T = i32 | none`, `U = i32`: only `T` overlaps a
    // written term. The result is canonical: a layer's key `(..)` sorts
    // before a primitive's name (spec 2.6.4).
    let tun = HirTy::layered(vec![HirTy::None], vec![inner.clone(), I32]);
    assert_eq!(tun, HirTy::Sum(vec![inner, I32, HirTy::None]));
    let mut again = tun.clone();
    again.canonicalize();
    assert_eq!(again, tun);
}

#[test]
fn written_types_lower_by_spelling() {
    // Spec 2.6.4: a written sum spreads its written members.
    assert_eq!(
        resolve_ty(&ty("(i32 | none) | none")),
        HirTy::sum([I32, HirTy::None])
    );
    assert_eq!(resolve_ty(&ty("i32 | str | i32")), HirTy::sum([I32, STR]));
    assert_eq!(resolve_ty(&ty("i32?")), HirTy::sum([I32, HirTy::None]));
    assert_eq!(resolve_ty(&ty("()")), HirTy::Unit);
    assert_eq!(
        resolve_ty(&ty("[u8; 4]")),
        HirTy::Array(Box::new(HirTy::Prim(PrimTy::U8)), 4)
    );
    assert_eq!(resolve_ty(&ty("(i32, str)")), HirTy::Tuple(vec![I32, STR]));
    let HirTy::Fn(args, ret, _) = resolve_ty(&ty("fn(i32) -> str")) else {
        panic!()
    };
    assert_eq!((args, *ret), (vec![I32], STR));
    assert_eq!(resolve_ty(&ty("!")), HirTy::Never);
}

#[test]
fn throws_drops_no_throw() {
    assert_eq!(throws_ty(I32, HirTy::NoThrow), I32);
    assert_eq!(
        throws_ty(I32, HirTy::sum([STR, HirTy::NoThrow])),
        HirTy::Throws {
            ok: Box::new(I32),
            err: Box::new(STR),
        }
    );
}

#[test]
fn rooted_paths() {
    let p = |segs: &[&str], m: &str| root_path(segs, m);
    let v = |s: &[&str]| Some(s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>());
    assert_eq!(p(&["pkg", "a", "f"], "x::y"), v(&["a", "f"]));
    assert_eq!(p(&["sup", "f"], "a::b"), v(&["a", "f"]));
    assert_eq!(p(&["sup", "sup", "f"], "a::b::c"), v(&["a", "f"]));
    assert_eq!(p(&["sup", "f"], ""), v(&["f"]));
    assert_eq!(
        p(&["rok", "core", "List"], "a"),
        v(&["rok", "core", "List"])
    );
    assert_eq!(p(&["a", "f"], "m"), None);
}

#[test]
fn closure_free_names() {
    let p = parse("fn f() { let c = |x: i32| { let y = x + a; y + b(x) + a + self.z }; }");
    let Item::Fn(f) = &p.items[0] else { panic!() };
    let StmtKind::Let { init, .. } = &f.body.as_ref().unwrap().stmts[0].kind else {
        panic!()
    };
    let ExprKind::Closure { params, body, .. } = &init.kind else {
        panic!()
    };
    // First-use order, each once; parameters and inner `let`s are bound.
    assert_eq!(free_names(params, body), ["a", "b", "self"]);
}
