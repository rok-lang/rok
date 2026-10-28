//! `Reflect` (refs-lang 6.10.1, stdlib 2.12.2): every type has it without
//! an impl. Calls nothing else answers land here: `T::type_name()` /
//! `size()` / `align()` are constants; `x.type_name_of()` / `x.size_of()`
//! switch on an `any` or sum's held variant. Also `rok.any_cast`,
//! `rok.method_table` and `rok.field_table`, behind `#[derive(Inspect)]`.

#[allow(clippy::wildcard_imports)]
use super::*;

use super::expr::{any_dispatcher, discard_args, enum_base_name, hir_local};

/// The trait key of `Reflect`.
pub(super) const REFLECT: &str = "rok::core::reflect::Reflect";

/// The trait key of `Inspect`.
pub(super) const INSPECT: &str = "rok::core::reflect::Inspect";

const INTS: [PrimTy; 10] = [
    PrimTy::I8,
    PrimTy::I16,
    PrimTy::I32,
    PrimTy::I64,
    PrimTy::Isize,
    PrimTy::U8,
    PrimTy::U16,
    PrimTy::U32,
    PrimTy::U64,
    PrimTy::Usize,
];

/// `ty`'s `type_name`: no module paths (`List<i64>`), block-local items by
/// their own name (spec 3.9.1).
pub(super) fn type_name(ctx: &LowerCtx<'_>, ty: &HirTy) -> String {
    let full = ctx.ty_name(ty);
    let full = rokc_core::local::unmangle(&full);
    let mut out = String::with_capacity(full.len());
    let mut word = String::new();
    let mut chars = full.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            word.clear();
        } else {
            out.push_str(&word);
            word.clear();
            out.push(c);
        }
    }
    out + &word
}

/// Help for a missing `method` on `ty` when `Inspect` declares it and
/// `ty` is a struct or enum without the derive.
pub(super) fn inspect_help(ctx: &LowerCtx<'_>, ty: &HirTy, method: &str) -> Option<String> {
    let declares = ctx
        .env
        .trait_of(INSPECT)
        .is_some_and(|t| t.methods.iter().any(|m| m == method));
    if !declares {
        return None;
    }
    derive_inspect_help(ctx, ty).map(|h| format!("{h} to use `{method}`"))
}

/// "add `#[derive(Inspect)]` to `Point`" for a struct or enum lacking it.
pub(super) fn derive_inspect_help(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<String> {
    if !matches!(ty, HirTy::Struct(_) | HirTy::Enum(_)) || ctx.has_trait(ty, INSPECT) {
        return None;
    }
    let name = type_name(ctx, ty);
    let base = name.split('<').next().unwrap_or(&name);
    Some(format!("add `#[derive(Inspect)]` to `{base}`"))
}

/// `inspect_help` for a type named by its key (`pkg::Point`).
pub(super) fn inspect_help_key(ctx: &LowerCtx<'_>, key: &str, method: &str) -> Option<String> {
    let ty = ctx
        .lookup_struct(key)
        .map(HirTy::Struct)
        .or_else(|| ctx.lookup_enum(key).map(HirTy::Enum))?;
    inspect_help(ctx, &ty, method)
}

/// "no associated function" on `ty`, with the `Inspect` derive help.
pub(super) fn no_assoc_fn(
    ctx: &LowerCtx<'_>,
    ty: &HirTy,
    method: &str,
    span: rokc_diagnostic::Span,
) -> HirExpr {
    let e = ctx.error_expr(
        span,
        format!("no associated function `{method}` on `{}`", ctx.ty_name(ty)),
    );
    ctx.help_last(inspect_help(ctx, ty, method));
    e
}

const fn str_const(s: String, span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Const(ConstVal::Str(s)),
        ty: HirTy::Prim(PrimTy::Str),
        span,
    }
}

/// `type_name` / `size` / `align` (and the instance forms on a value
/// whose type is fixed) of `ty`.
fn constant(ctx: &LowerCtx<'_>, ty: &HirTy, method: &str, span: rokc_diagnostic::Span) -> HirExpr {
    match method {
        "type_name" | "type_name_of" => str_const(type_name(ctx, ty), span),
        _ => HirExpr {
            kind: HirExprKind::SizeOf {
                ty: ty.clone(),
                align: method == "align",
            },
            ty: HirTy::Prim(PrimTy::Usize),
            span,
        },
    }
}

/// `T::type_name()` / `T::size()` / `T::align()`; `None` for any other
/// call.
pub(super) fn static_call(
    ctx: &LowerCtx<'_>,
    ty: &HirTy,
    method: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    (args.is_empty() && matches!(method, "type_name" | "size" | "align"))
        .then(|| constant(ctx, ty, method, span))
}

/// `recv.type_name_of()` / `recv.size_of()`; `None` for any other call.
pub(super) fn method_call(
    ctx: &mut LowerCtx<'_>,
    recv: HirExpr,
    method: &str,
    args: &[CallArg],
) -> Result<HirExpr, HirExpr> {
    if !args.is_empty() || !matches!(method, "type_name_of" | "size_of") {
        return Err(recv);
    }
    Ok(dynamic(ctx, recv, method))
}

/// `Reflect::type_name_of(x)` / `Reflect::type_name()` - the trait named
/// as the path.
pub(super) fn trait_call(
    ctx: &mut LowerCtx<'_>,
    self_ty: &HirTy,
    recv: Option<HirExpr>,
    method: &str,
    args: &[CallArg],
    span: rokc_diagnostic::Span,
) -> Option<HirExpr> {
    match recv {
        Some(r) => method_call(ctx, r, method, args).ok(),
        None => static_call(ctx, self_ty, method, args, span),
    }
}

/// The instance value: a switch over what a sum / `any` holds, else the
/// constant (after evaluating `recv`).
pub(super) fn dynamic(ctx: &mut LowerCtx<'_>, recv: HirExpr, method: &str) -> HirExpr {
    let span = recv.span;
    let ret = if method == "type_name_of" {
        HirTy::Prim(PrimTy::Str)
    } else {
        HirTy::Prim(PrimTy::Usize)
    };
    match recv.ty.clone() {
        HirTy::Sum(members) => {
            let mut arms = Vec::new();
            for (i, m) in members.iter().enumerate() {
                let index = u32::try_from(i).expect("variant overflow");
                let (binder, payload_ty, body) = if m.is_none() {
                    (None, None, constant(ctx, m, method, span))
                } else {
                    let x = ctx.declare(None, m.clone());
                    let body = dynamic(ctx, hir_local(x, m, span), method);
                    (Some(x), Some(m.clone()), body)
                };
                arms.push(HirMatchArm {
                    pat: HirPattern::SumVariant {
                        sum_ty: recv.ty.clone(),
                        variant_index: index,
                        binder,
                        payload_ty,
                    },
                    guard: None,
                    body,
                });
            }
            HirExpr {
                kind: HirExprKind::Match {
                    scrutinee: Box::new(recv),
                    arms,
                },
                ty: ret,
                span,
            }
        }
        HirTy::AnyTrait(_) => {
            let sig = FnSig {
                id: FunctionId(u32::MAX),
                ret_ty: ret.clone(),
                param_defaults: vec![None],
                param_tys: vec![recv.ty.clone()],
                param_names: vec!["#0".to_owned()],
                param_refs: Vec::new(),
                variadic: false,
            };
            let id = any_dispatcher(ctx, &recv.ty, REFLECT, method, &sig, span);
            HirExpr {
                kind: HirExprKind::Call {
                    callee: id,
                    args: vec![recv],
                },
                ty: ret,
                span,
            }
        }
        t => {
            let value = constant(ctx, &t, method, span);
            HirExpr {
                kind: HirExprKind::Block(Box::new(HirBlock {
                    stmts: vec![HirStmt::Expr(recv)],
                    trailing: Some(value),
                    label: None,
                })),
                ty: ret,
                span,
            }
        }
    }
}

/// `rok.method_table` / `rok.field_table` / `rok.any_cast`
/// (`core/reflect.rok`), called as `name`.
pub(super) fn intrinsic(
    ctx: &mut LowerCtx<'_>,
    op: super::collect::Intrinsic,
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
    let table = match op {
        super::collect::Intrinsic::MethodTable => Some(method_table(ctx, &t)),
        super::collect::Intrinsic::FieldTable => Some(field_table(ctx, &t)),
        _ => None,
    };
    if let Some(table) = table {
        discard_args(ctx, args);
        return str_const(table, span);
    }
    let [CallArg::Positional(arg)] = args else {
        discard_args(ctx, args);
        return ctx.error_expr(span, format!("`{name}` takes one value"));
    };
    let any = HirTy::AnyTrait(Vec::new());
    let v = lower_expected(ctx, arg, any.clone());
    let v = coerce(ctx, &any, v, &|| format!("the value of `{name}`"));
    any_cast(ctx, v, &t, span)
}

/// `match v { x: T => x, x: I => x as T if it fits (integers), _ => none }`.
fn any_cast(ctx: &mut LowerCtx<'_>, v: HirExpr, t: &HirTy, span: rokc_diagnostic::Span) -> HirExpr {
    let out = HirTy::sum([t.clone(), HirTy::None]);
    let HirTy::Sum(members) = &out else {
        return poison(span);
    };
    let at = |want: &HirTy| {
        let i = members.iter().position(|m| m == want).expect("cast member");
        u32::try_from(i).expect("variant overflow")
    };
    let (some_ix, none_ix) = (at(t), at(&HirTy::None));
    let wrap = |value: Option<HirExpr>, index: u32| HirExpr {
        kind: HirExprKind::SumWrap {
            sum_ty: out.clone(),
            variant_index: index,
            value: value.map(Box::new),
        },
        ty: out.clone(),
        span,
    };
    let mut arms = Vec::new();
    let mut arm = |ctx: &mut LowerCtx<'_>, from: &HirTy, body: &dyn Fn(HirExpr) -> HirExpr| {
        let x = ctx.declare(None, from.clone());
        arms.push(HirMatchArm {
            pat: HirPattern::SumVariant {
                sum_ty: v.ty.clone(),
                variant_index: ctx.any_tag(from),
                binder: Some(x),
                payload_ty: Some(from.clone()),
            },
            guard: None,
            body: body(hir_local(x, from, span)),
        });
    };
    arm(ctx, t, &|x| wrap(Some(x), some_ix));
    if let HirTy::Prim(to) = t {
        for from in INTS.iter().filter(|p| to.is_int() && *p != to) {
            arm(ctx, &HirTy::Prim(*from), &|x| {
                let fits = int_fits(x.clone(), *from, *to, span);
                HirExpr {
                    kind: HirExprKind::If {
                        cond: Box::new(fits),
                        then_block: Box::new(HirBlock {
                            stmts: Vec::new(),
                            trailing: Some(wrap(Some(cast(x, *from, *to, span)), some_ix)),
                            label: None,
                        }),
                        else_branch: Some(Box::new(wrap(None, none_ix))),
                    },
                    ty: out.clone(),
                    span,
                }
            });
        }
    }
    arms.push(HirMatchArm {
        pat: HirPattern::Wildcard,
        guard: None,
        body: wrap(None, none_ix),
    });
    HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(v),
            arms,
        },
        ty: out,
        span,
    }
}

fn cast(x: HirExpr, from: PrimTy, to: PrimTy, span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Cast {
            operand: Box::new(x),
            from_ty: HirTy::Prim(from),
            to_ty: HirTy::Prim(to),
        },
        ty: HirTy::Prim(to),
        span,
    }
}

fn bin(op: PrimOp, lhs: HirExpr, rhs: HirExpr, span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::PrimBinOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        },
        ty: HirTy::Prim(PrimTy::Bool),
        span,
    }
}

/// `x as to` round-trips and keeps its sign: `(x as to) as from == x &&
/// (x < 0) == ((x as to) < 0)` (every integer here is at most 64 bits).
fn int_fits(x: HirExpr, from: PrimTy, to: PrimTy, span: rokc_diagnostic::Span) -> HirExpr {
    let c = cast(x.clone(), from, to, span);
    let round = bin(PrimOp::Eq, cast(c.clone(), to, from, span), x.clone(), span);
    let negative = |e: HirExpr, p: PrimTy| {
        if p.is_signed_int() {
            bin(PrimOp::Lt, e, super::expr::hir_int(0, p, span), span)
        } else {
            HirExpr {
                kind: HirExprKind::Const(ConstVal::Bool(false)),
                ty: HirTy::Prim(PrimTy::Bool),
                span,
            }
        }
    };
    let same_sign = bin(PrimOp::Eq, negative(x, from), negative(c, to), span);
    bin(PrimOp::LogicalAnd, round, same_sign, span)
}

/// `ty`'s inherent methods (concrete and generic-impl ones), sorted by
/// name: `name \t static \t return \t args..` per line.
// A generic impl's method shows its types as written (`T`).
fn method_table(ctx: &LowerCtx<'_>, ty: &HirTy) -> String {
    let mut rows: Vec<(String, String)> = Vec::new();
    let target = super::resolve::method_target_for(ty);
    for ((tgt, name), ids) in ctx.env.all_methods() {
        if Some(tgt) != target.as_ref() {
            continue;
        }
        for id in ids {
            if ctx
                .env
                .method_info(*id)
                .is_some_and(|i| i.trait_name.is_some())
            {
                continue;
            }
            let sig = ctx.sig_by_id(*id);
            let is_static = sig.param_names.first().map(String::as_str) != Some("self");
            let skip = usize::from(!is_static);
            let mut row = format!(
                "{name}\t{}\t{}",
                u8::from(is_static),
                type_name(ctx, &sig.ret_ty)
            );
            for p in &sig.param_tys[skip.min(sig.param_tys.len())..] {
                row.push('\t');
                row.push_str(&type_name(ctx, p));
            }
            rows.push((name.clone(), row));
        }
    }
    let base = match ty {
        HirTy::Struct(id) => Some(ctx.struct_def(*id).name),
        HirTy::Enum(id) => Some(ctx.enum_def(*id).name),
        _ => None,
    };
    if let Some(base) = base {
        let base = enum_base_name(&base).to_owned();
        for ((b, name), gms) in ctx.env.all_generic_methods() {
            if *b != base || rows.iter().any(|(n, _)| n == name) {
                continue;
            }
            for gm in gms.iter().filter(|g| g.info.trait_name.is_none()) {
                let params: Vec<&Param> = gm.def.params.iter().collect();
                let is_static = params.first().map(|p| p.name.as_str()) != Some("self");
                let mut row = format!(
                    "{name}\t{}\t{}",
                    u8::from(is_static),
                    super::collect::impl_def_display(&super::throws::sig_ret_ast(&gm.def))
                );
                for p in params.iter().filter(|p| p.name != "self") {
                    row.push('\t');
                    row.push_str(
                        &p.ty
                            .as_ref()
                            .map_or_else(|| "_".to_owned(), super::collect::impl_def_display),
                    );
                }
                rows.push((name.clone(), row));
            }
        }
    }
    rows.sort();
    rows.into_iter().map(|(_, r)| r + "\n").collect()
}

/// `ty`'s fields in declaration order, `name \t pub \t type` per line;
/// empty unless `ty` is a struct. A generic instance shows its argument
/// types (`f64`, not `T`).
fn field_table(ctx: &LowerCtx<'_>, ty: &HirTy) -> String {
    let HirTy::Struct(id) = ty else {
        return String::new();
    };
    let def = ctx.struct_def(*id);
    if def.union_kind.is_some() {
        return String::new();
    }
    let base = if def.generic_args.is_empty() {
        def.name.as_str()
    } else {
        def.name.split("__").next().unwrap_or(&def.name)
    };
    let vis = ctx.env.fields_vis(base);
    let mut out = String::new();
    for f in &def.fields {
        let is_pub = vis.and_then(|v| v.get(&f.name)) == Some(&Visibility::Pub);
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            f.name,
            u8::from(is_pub),
            type_name(ctx, &f.ty)
        ));
    }
    out
}
