//! `#[track_caller]`: the function takes its caller's location
//! (`path:line:col`) as a hidden last `str`. A call passes its own
//! position, or inside another `#[track_caller]` function the one it
//! received, so panics report the first caller outside the chain. On an
//! `extern` declaration the runtime receives it; a function value of one
//! is a shim passing where the value was taken.

use rokc_core::{HashMap, HashSet};

use rokc_ir_common::{FunctionId, LocalId, PrimTy};

use crate::{
    HirBlock, HirExpr, HirExprKind, HirFunction, HirLocal, HirParam, HirTy, each_block_mut,
};

const PARAM: &str = "#caller";

pub(super) fn lower(
    functions: &mut Vec<HirFunction>,
    tracked: &HashSet<FunctionId>,
    sources: &rokc_diagnostic::SourceMap,
) {
    if tracked.is_empty() {
        return;
    }
    let str_ty = HirTy::Prim(PrimTy::Str);
    let mut sigs: HashMap<FunctionId, (Vec<HirParam>, HirTy)> = HashMap::default();
    for f in functions.iter_mut().filter(|f| tracked.contains(&f.id)) {
        sigs.insert(f.id, (f.params.clone(), f.ret_ty.clone()));
        let local = LocalId(u32::try_from(f.locals.len()).expect("LocalId overflow"));
        f.locals.push(HirLocal {
            id: local,
            name: Some(PARAM.to_owned()),
            ty: str_ty.clone(),
            boxed: false,
        });
        f.params.push(HirParam {
            name: PARAM.to_owned(),
            local,
            ty: str_ty.clone(),
            by_ref: false,
        });
        f.facts.track_caller = true;
    }
    let mut next_id = functions.iter().map(|f| f.id.0 + 1).max().unwrap_or(0);
    let mut shims: HashMap<(FunctionId, String), FunctionId> = HashMap::default();
    let mut new_fns = Vec::new();
    for f in functions.iter_mut() {
        let Some(body) = &mut f.body else { continue };
        let own = f.facts.track_caller.then(|| {
            let p = f.params.last().expect("hidden parameter");
            HirExprKind::Local(p.local)
        });
        let here = |span| {
            own.clone().unwrap_or_else(|| {
                HirExprKind::Const(rokc_ir_common::ConstVal::Str(sources.location(span)))
            })
        };
        each_block_mut(body, &mut |e| match &mut e.kind {
            HirExprKind::Call { callee, args } if sigs.contains_key(callee) => {
                args.push(HirExpr {
                    kind: here(e.span),
                    ty: HirTy::Prim(PrimTy::Str),
                    span: e.span,
                });
            }
            HirExprKind::FnRef(id) if sigs.contains_key(id) => {
                let at = sources.location(e.span);
                *id = *shims.entry((*id, at.clone())).or_insert_with(|| {
                    let shim = FunctionId(next_id);
                    next_id += 1;
                    let (params, ret) = &sigs[id];
                    new_fns.push(shim_fn(shim, *id, params, ret, at, e.span));
                    shim
                });
            }
            _ => {}
        });
    }
    functions.extend(new_fns);
}

/// `fn(params) -> ret { target(params, at) }`.
fn shim_fn(
    id: FunctionId,
    target: FunctionId,
    params: &[HirParam],
    ret: &HirTy,
    at: String,
    span: rokc_diagnostic::Span,
) -> HirFunction {
    let locals: Vec<HirLocal> = params
        .iter()
        .enumerate()
        .map(|(i, p)| HirLocal {
            id: LocalId(u32::try_from(i).expect("LocalId overflow")),
            name: Some(p.name.clone()),
            ty: p.ty.clone(),
            boxed: false,
        })
        .collect();
    let mut args: Vec<HirExpr> = locals
        .iter()
        .map(|l| HirExpr {
            kind: HirExprKind::Local(l.id),
            ty: l.ty.clone(),
            span,
        })
        .collect();
    args.push(HirExpr {
        kind: HirExprKind::Const(rokc_ir_common::ConstVal::Str(at)),
        ty: HirTy::Prim(PrimTy::Str),
        span,
    });
    let call = HirExpr {
        kind: HirExprKind::Call {
            callee: target,
            args,
        },
        ty: ret.clone(),
        span,
    };
    HirFunction {
        id,
        def_id: None,
        name: format!("track_caller_shim{}", id.0),
        params: params
            .iter()
            .zip(&locals)
            .map(|(p, l)| HirParam {
                local: l.id,
                ..p.clone()
            })
            .collect(),
        locals,
        ret_ty: ret.clone(),
        body: Some(HirBlock {
            stmts: Vec::new(),
            trailing: Some(call),
            label: None,
        }),
        is_async: false,
        is_generator: false,
        span,
        facts: rokc_ir_common::FnFacts::default(),
    }
}
