//! `int` / `float` (stdlib 2.1.2, 2.1.3) are library structs the compiler
//! knows by path; an unsuffixed literal in such a slot becomes a
//! constructor call:
//!
//! ```text
//! let a: int = 5;          ==>  Int::from_i64(5i64)
//! let b: int = 10 ** 30;    ==>  Int::from_literal("1000..0")   (past i64)
//! let c: float = 1.5;       ==>  Float::from_f64(1.5f64)
//! let d: float = 1e400;     ==>  Float::from_literal("1e400")   (past f64)
//! ```
//!
//! `x as T` with `int` / `float` on either side is a conversion trait call
//! (stdlib 2.11.1): `Int as i32` → `i32::wrapping_from(x)`, `i32 as int` →
//! `Int::from(x)`.

#[allow(clippy::wildcard_imports)]
use super::*;
use rokc_ast::{CallArg, PathSegment};
use rokc_diagnostic::Span;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Big {
    Int,
    Float,
}

impl Big {
    /// Its lang slot (stdlib 0.4.2).
    const fn slot(self) -> &'static str {
        match self {
            Self::Int => "int_type",
            Self::Float => "float_type",
        }
    }
}

/// Which of `int` / `float` `ty` is.
pub(super) fn big_of(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<Big> {
    let HirTy::Struct(id) = ty else { return None };
    [Big::Int, Big::Float]
        .into_iter()
        .find(|b| ctx.lookup_struct(ctx.lang(b.slot())) == Some(*id))
}

pub(super) fn path_expr(full: &str, leaf: &str, span: Span) -> Expr {
    let segments = full
        .split("::")
        .chain(std::iter::once(leaf))
        .map(|n| PathSegment {
            name: n.to_owned(),
            generics: Vec::new(),
        })
        .collect();
    Expr {
        kind: ExprKind::Path(AstPath {
            segments,
            span,
            origin: None,
        }),
        span,
    }
}

fn call(callee: Expr, arg: Expr, span: Span) -> Expr {
    Expr {
        kind: ExprKind::Call {
            callee: Box::new(callee),
            args: vec![CallArg::Positional(arg)],
            trailing_closure: None,
        },
        span,
    }
}

const fn lit(l: AstLit, span: Span) -> Expr {
    Expr {
        kind: ExprKind::Literal(l),
        span,
    }
}

/// `lit` (negated if `negate`) as the constructor call; `None` when the
/// slot is neither or the literal is suffixed (a plain mismatch).
pub(super) fn literal(
    ctx: &mut LowerCtx<'_>,
    l: &AstLit,
    negate: bool,
    span: Span,
) -> Option<HirExpr> {
    let big = match ctx.current_expected()? {
        // `int | none`: the sum's `int` / `float` member, unless a
        // primitive number member takes the literal (`expected_prim`).
        HirTy::Sum(vs)
            if !vs
                .iter()
                .any(|v| matches!(v, HirTy::Prim(p) if p.is_int() || p.is_float())) =>
        {
            let bigs: Vec<Big> = vs.iter().filter_map(|v| big_of(ctx, v)).collect();
            let want = if matches!(l, AstLit::Float(_)) {
                Big::Float
            } else {
                Big::Int
            };
            if bigs.contains(&want) {
                want
            } else {
                *bigs.first()?
            }
        }
        t => big_of(ctx, t)?,
    };
    let sign = if negate { "-" } else { "" };
    let ty = ctx.lang(big.slot());
    let e = match (l, big) {
        (AstLit::Int(t), _) if !has_int_suffix(t) => {
            let v = parse_int_literal(t).map(|(v, _)| if negate { -v } else { v });
            match (v, big) {
                (Some(v), Big::Int) if i64::try_from(v).is_ok() => call(
                    path_expr(ty, "from_i64", span),
                    lit(AstLit::Int(format!("{v}i64")), span),
                    span,
                ),
                // Exact in `f64` (2^53).
                (Some(v), Big::Float) if v.unsigned_abs() <= 1 << 53 => {
                    let f = if v < 0 { "-" } else { "" };
                    call(
                        path_expr(ty, "from_f64", span),
                        float_lit(f, &format!("{}.0", v.unsigned_abs()), span),
                        span,
                    )
                }
                _ => {
                    let text = match big {
                        Big::Int => format!("{sign}{t}"),
                        // `0x..` past 2^53: the value's decimal digits.
                        Big::Float => match v {
                            Some(v) => v.to_string(),
                            None => format!("{sign}{t}"),
                        },
                    };
                    call(
                        path_expr(ty, "from_literal", span),
                        lit(AstLit::Str(text), span),
                        span,
                    )
                }
            }
        }
        (AstLit::Float(t), Big::Float) if !t.ends_with("f32") && !t.ends_with("f64") => {
            let clean: String = t.chars().filter(|c| *c != '_').collect();
            let v: f64 = clean.parse().ok()?;
            // A zero from non-zero digits underflowed.
            let mantissa = clean.split(['e', 'E']).next().unwrap_or("");
            let underflow = v == 0.0 && mantissa.chars().any(|c| ('1'..='9').contains(&c));
            if v.is_finite() && (v.is_normal() || v == 0.0) && !underflow {
                call(
                    path_expr(ty, "from_f64", span),
                    float_lit(sign, &clean, span),
                    span,
                )
            } else {
                call(
                    path_expr(ty, "from_literal", span),
                    lit(AstLit::Str(format!("{sign}{clean}")), span),
                    span,
                )
            }
        }
        _ => return None,
    };
    Some(lower_expr(ctx, &e))
}

/// `-?text` as an `f64` literal.
fn float_lit(sign: &str, text: &str, span: Span) -> Expr {
    let l = lit(AstLit::Float(format!("{text}f64")), span);
    if sign.is_empty() {
        return l;
    }
    Expr {
        kind: ExprKind::OpPrefix {
            op: "-".to_owned(),
            operand: Box::new(l),
        },
        span,
    }
}

/// `operand as to` when either side is `int` / `float` (`big_of`;
/// `from_ty` is the operand's type): the conversion call.
pub(super) fn cast(
    ctx: &mut LowerCtx<'_>,
    operand: &Expr,
    from_ty: &HirTy,
    to_ty: &HirTy,
    to: &rokc_ast::Ty,
    span: Span,
) -> HirExpr {
    let into = big_of(ctx, to_ty);
    if from_ty == to_ty {
        return lower_expr(ctx, operand);
    }
    let rokc_ast::TyKind::Path(p) = &to.kind else {
        return ctx.error_expr(
            span,
            format!(
                "cannot cast `{}` as `{}`",
                ctx.ty_name(from_ty),
                ctx.ty_name(to_ty)
            ),
        );
    };
    // `int` / `float` to `f32` / `f64` rounds, which is not a wrapping
    // conversion: name the method that rounds.
    if into.is_none() && matches!(to_ty, HirTy::Prim(p) if p.is_float()) {
        let from = ctx.ty_name(from_ty);
        let to = ctx.ty_name(to_ty);
        let help = if to == "f64" {
            "use `x.to_f64()` (rounds), or `try f64::try_from(x)` to throw past the range"
        } else {
            "use `x.to_f64() as f32` (rounds)"
        };
        ctx.emit_error_at(
            rokc_diagnostic::ErrorKind::Type,
            span,
            format!(
                "cannot cast `{from}` as `{to}`: `as` keeps the low bits, and a float has none"
            ),
        );
        ctx.help_last(Some(help.to_owned()));
        return poison(span);
    }
    // Into `int` / `float`: the lossless `From` (none from `f64` to `int`,
    // reported as such); out of one: stdlib 2.11.1's wrapping conversion.
    let method = if into.is_some() {
        "from"
    } else {
        "wrapping_from"
    };
    let mut callee = p.clone();
    callee.segments.push(PathSegment {
        name: method.to_owned(),
        generics: Vec::new(),
    });
    let e = call(
        Expr {
            kind: ExprKind::Path(callee),
            span,
        },
        operand.clone(),
        span,
    );
    lower_expr(ctx, &e)
}
