//! Codegen on hand-built MIR: the MLIR each construct becomes.

use rokc_diagnostic::Span;
use rokc_ir_common::{BlockId, FnFacts, FunctionId, LocalId, PrimOp, PrimTy};
use rokc_mir::{
    BasicBlock, LocalDecl, MirFunction, MirModule, MirTy, Operand, RValue, Statement, Terminator,
};

use crate::{CodegenOptions, codegen};

const I32: MirTy = MirTy::Prim(PrimTy::I32);

const fn local(i: u32, ty: MirTy) -> LocalDecl {
    LocalDecl {
        id: LocalId(i),
        name: None,
        ty,
    }
}

const fn block(id: u32, stmts: Vec<Statement>, terminator: Terminator) -> BasicBlock {
    BasicBlock {
        id: BlockId(id),
        params: vec![],
        stmts,
        terminator,
    }
}

const fn copy(i: u32) -> Operand {
    Operand::Copy(LocalId(i))
}

/// `fn <name>(_0: ty, _1: ty) -> ret`, locals from `_2` typed `more`.
fn function(
    id: u32,
    name: &str,
    (ty, ret): (&MirTy, &MirTy),
    more: Vec<MirTy>,
    blocks: Vec<BasicBlock>,
) -> MirFunction {
    let mut locals = vec![local(0, ty.clone()), local(1, ty.clone())];
    locals.extend(more.into_iter().zip(2..).map(|(t, i)| local(i, t)));
    MirFunction {
        id: FunctionId(id),
        def_id: None,
        name: name.into(),
        facts: FnFacts::default(),
        strips: Vec::new(),
        pins: Vec::new(),
        params: vec![LocalId(0), LocalId(1)],
        locals,
        blocks,
        param_tys: vec![ty.clone(), ty.clone()],
        ret_ty: ret.clone(),
        span: Span::default(),
    }
}

/// `fn f(_0: ty, _1: ty) -> ty { _2 = _0 <op> _1; return _2 }`
fn binary(op: PrimOp, ty: &MirTy) -> MirFunction {
    let stmts = vec![Statement::Assign {
        target: LocalId(2),
        rvalue: RValue::BinOp {
            op,
            op_ty: ty.clone(),
            lhs: copy(0),
            rhs: copy(1),
        },
    }];
    let ret = Terminator::Return(Some(copy(2)));
    function(
        0,
        "f",
        (ty, ty),
        vec![ty.clone()],
        vec![block(0, stmts, ret)],
    )
}

fn mlir(functions: Vec<MirFunction>, overflow_checks: bool) -> String {
    let module = MirModule {
        functions,
        ..MirModule::default()
    };
    let opts = CodegenOptions {
        overflow_checks,
        ..CodegenOptions::default()
    };
    codegen(&module, &opts, &rokc_diagnostic::SourceMap::new())
        .expect("codegen")
        .text
}

/// The body of function `name` (symbol `_Y<hash>__<name>_`).
fn body<'a>(text: &'a str, name: &str) -> &'a str {
    let head = text
        .find(&format!("__{name}_("))
        .and_then(|at| text[..at].rfind("func.func"))
        .unwrap_or_else(|| panic!("no function {name} in\n{text}"));
    let rest = &text[head..];
    &rest[..rest.find("\n  }").unwrap_or(rest.len())]
}

/// The symbol of function `name`.
fn symbol<'a>(text: &'a str, name: &str) -> &'a str {
    let end = text.find(&format!("__{name}_(")).expect("the function") + name.len() + 3;
    let start = text[..end].rfind('@').expect("a symbol") + 1;
    &text[start..end]
}

#[test]
fn checked_add_panics_on_overflow() {
    let checked = mlir(vec![binary(PrimOp::Add, &I32)], true);
    let f = body(&checked, "f");
    assert!(f.contains("arith.addi"), "{f}");
    assert!(
        f.contains("cf.cond_br") && f.contains("@rok_panic_at"),
        "{f}"
    );
    let plain = mlir(vec![binary(PrimOp::Add, &I32)], false);
    let f = body(&plain, "f");
    assert!(f.contains("arith.addi"), "{f}");
    assert!(!f.contains("rok_panic_at"), "{f}");
}

/// Division checks its divisor, and `MIN / -1` when signed, with or
/// without overflow checks.
#[test]
fn division_checks_follow_signedness() {
    let text = mlir(vec![binary(PrimOp::Div, &I32)], false);
    let f = body(&text, "f");
    assert!(f.contains("arith.divsi"), "{f}");
    assert_eq!(f.matches("@rok_panic_at").count(), 2, "{f}");
    assert!(text.contains("\"attempt to divide by zero\\00\""));
    assert!(text.contains("\"attempt to divide with overflow\\00\""));
    let text = mlir(vec![binary(PrimOp::Div, &MirTy::Prim(PrimTy::U32))], false);
    let f = body(&text, "f");
    assert!(f.contains("arith.divui"), "{f}");
    assert_eq!(f.matches("@rok_panic_at").count(), 1, "{f}");
}

#[test]
fn branches_and_calls() {
    // g(_0, _1) = if _0 < _1 { f(_0, _1) } else { _1 }
    let cmp = vec![Statement::Assign {
        target: LocalId(2),
        rvalue: RValue::BinOp {
            op: PrimOp::Lt,
            op_ty: I32,
            lhs: copy(0),
            rhs: copy(1),
        },
    }];
    let call = vec![Statement::Assign {
        target: LocalId(3),
        rvalue: RValue::Call {
            callee: FunctionId(0),
            args: vec![copy(0), copy(1)],
            ret_ty: I32,
        },
    }];
    let branch = Terminator::If {
        cond: copy(2),
        then_bb: BlockId(1),
        then_args: vec![],
        else_bb: BlockId(2),
        else_args: vec![],
    };
    let g = function(
        1,
        "g",
        (&I32, &I32),
        vec![MirTy::Prim(PrimTy::Bool), I32],
        vec![
            block(0, cmp, branch),
            block(1, call, Terminator::Return(Some(copy(3)))),
            block(2, vec![], Terminator::Return(Some(copy(1)))),
        ],
    );
    let text = mlir(vec![binary(PrimOp::Add, &I32), g], false);
    let g = body(&text, "g");
    assert!(g.contains("arith.cmpi slt"), "{g}");
    assert!(g.contains("cf.cond_br"), "{g}");
    let f = symbol(&text, "f");
    assert!(g.contains(&format!("call @{f}(")), "{g}");
}

/// A function's symbol depends on its package hash, not on its id.
#[test]
fn symbols_are_stable_across_ids() {
    let a = mlir(vec![binary(PrimOp::Sub, &I32)], false);
    let mut moved = binary(PrimOp::Sub, &I32);
    moved.id = FunctionId(7);
    let b = mlir(vec![moved], false);
    assert_eq!(symbol(&a, "f"), symbol(&b, "f"));
    assert!(symbol(&a, "f").starts_with("_Y"));
}

/// `-g`: a subprogram named by the rok path at the function's line, in
/// a unit that says whether the build is optimized.
#[test]
fn debug_info_describes_the_function() {
    let mut sources = rokc_diagnostic::SourceMap::new();
    let file = sources.add("dbg.rok".into(), "\nfn f() {}\n".into());
    let mut f = binary(PrimOp::Add, &I32);
    f.span = Span {
        file,
        line: 2,
        col: 1,
        ..Span::default()
    };
    let module = MirModule {
        functions: vec![f],
        ..MirModule::default()
    };
    let text = |optimized| {
        let opts = CodegenOptions {
            debug: true,
            optimized,
            ..CodegenOptions::default()
        };
        codegen(&module, &opts, &sources).expect("codegen").text
    };
    let plain = text(false);
    assert!(plain.contains("isOptimized = false"), "{plain}");
    assert!(plain.contains("name = \"f\""), "{plain}");
    assert!(plain.contains("line = 2, scopeLine = 2"), "{plain}");
    assert!(text(true).contains("isOptimized = true"));
}

/// Each operation follows its operand type: unsigned and float forms,
/// and the checks (with their messages) only where they can fail.
#[test]
fn ops_follow_operand_types() {
    let u32_ = MirTy::Prim(PrimTy::U32);
    let f64_ = MirTy::Prim(PrimTy::F64);
    let i64_ = MirTy::Prim(PrimTy::I64);
    let one = |op, ty: &MirTy| mlir(vec![binary(op, ty)], true);

    let text = one(PrimOp::Rem, &u32_);
    let f = body(&text, "f");
    assert!(f.contains("arith.remui"), "{f}");
    assert_eq!(f.matches("@rok_panic_at").count(), 1, "{f}");
    assert!(text.contains("\"attempt to calculate the remainder with a divisor of zero\\00\""));

    let text = one(PrimOp::Add, &f64_);
    let f = body(&text, "f");
    assert!(
        f.contains("arith.addf") && !f.contains("rok_panic_at"),
        "{f}"
    );

    let text = one(PrimOp::Mul, &i64_);
    assert!(body(&text, "f").contains("arith.muli"));
    assert!(text.contains("\"attempt to multiply with overflow\\00\""));
    let text = one(PrimOp::Sub, &u32_);
    assert!(text.contains("\"attempt to subtract with overflow\\00\""));

    // `wrapping_*` never checks.
    let text = one(PrimOp::WrappingAdd, &I32);
    let f = body(&text, "f");
    assert!(
        f.contains("arith.addi") && !f.contains("rok_panic_at"),
        "{f}"
    );

    let text = one(PrimOp::Shl, &I32);
    assert!(body(&text, "f").contains("arith.shli"));
    assert!(text.contains("\"attempt to shift left with overflow\\00\""));
    assert!(body(&one(PrimOp::Shr, &I32), "f").contains("arith.shrsi"));
    assert!(body(&one(PrimOp::Shr, &u32_), "f").contains("arith.shrui"));
}

/// A frame links before the first safepoint of each path:
/// not on a path that reaches none, before a loop whose back edge polls,
/// and at the end of a block whose successor is linked from elsewhere.
#[test]
fn frames_link_before_the_first_safepoint() {
    use crate::emit::plan_links;
    let nop = || Statement::StorageDead(LocalId(0));
    let goto = |t| Terminator::Goto {
        target: BlockId(t),
        args: vec![],
    };
    let branch = |a, b| Terminator::If {
        cond: copy(0),
        then_bb: BlockId(a),
        then_args: vec![],
        else_bb: BlockId(b),
        else_args: vec![],
    };
    let ret = || Terminator::Return(None);
    let f = |blocks| function(0, "f", (&I32, &MirTy::Unit), vec![], blocks);

    // 0 → 1 (returns) | 2 (safepoint at 1, returns).
    let g = f(vec![
        block(0, vec![nop()], branch(1, 2)),
        block(1, vec![nop()], ret()),
        block(2, vec![nop(), nop()], ret()),
    ]);
    let l = plan_links(&g, |p, i| p == 2 && i == 1, |_| false).unwrap();
    assert_eq!(l.at.len(), 1);
    assert_eq!(l.at[&2], 1);
    assert!(l.linked.contains(&2) && !l.linked.contains(&1) && !l.linked.contains(&0));

    // 0 → 1 (header) → 2 (back edge, polls) → 1 | 3: linked before the
    // loop, at the end of 0.
    let g = f(vec![
        block(0, vec![], goto(1)),
        block(1, vec![], branch(2, 3)),
        block(2, vec![nop()], goto(1)),
        block(3, vec![], ret()),
    ]);
    let l = plan_links(&g, |_, _| false, |b| b == 2).unwrap();
    assert_eq!(l.at.len(), 1);
    assert_eq!(l.at[&0], usize::MAX);
    assert!((0..4).all(|b| l.linked.contains(&b)));

    // A join of a linked and an unlinked path: the unlinked one links at
    // its end.
    let g = f(vec![
        block(0, vec![], branch(1, 2)),
        block(1, vec![nop()], goto(3)),
        block(2, vec![], goto(3)),
        block(3, vec![], ret()),
    ]);
    let l = plan_links(&g, |p, _| p == 1, |_| false).unwrap();
    assert_eq!(l.at[&1], 0);
    assert_eq!(l.at[&2], usize::MAX);
    assert!(l.linked.contains(&3) && !l.linked.contains(&0));

    // A back edge into the entry block: linked at entry.
    let g = f(vec![
        block(0, vec![nop()], branch(0, 1)),
        block(1, vec![], ret()),
    ]);
    assert!(plan_links(&g, |_, _| true, |_| false).is_none());
}
