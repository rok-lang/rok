//! `f"..."` (refs-lang 1.7.3, stdlib 2.10) written into one runtime
//! text (a `core::fmt::Formatter`'s; `rok_fmt_*` return it, maybe moved):
//!
//! ```text
//! f"a {n} b {s:^5} {p:?}"   (n: i32, s: str, p: P)
//!   ==>  { let #f0 = n; let #f1 = s; let #f2 = p;
//!          let #t = rok_fmt_new(55);
//!          #t = rok_fmt_str(#t, "a ");
//!          #t = rok_fmt_int(#t, <no spec>, #f0 as i64, 1, 32);
//!          #t = rok_fmt_str(#t, " b ");
//!          let #s = rok_fmt_len(#t); #t = rok_fmt_str(#t, #f1);
//!          #t = rok_fmt_pad(#t, <`^5` packed>, #s, #s, 0);
//!          #t = rok_fmt_str(#t, " ");
//!          #t = rok::core::fmt::fstr_debug(#t, #f2, <no spec>);
//!          rok_fmt_finish(#t) }
//! ```
//!
//! Holes and `${..}` specs are evaluated left to right into temps, then
//! written in order: primitives by the runtime, other values through
//! `Display::fmt` / `Debug::fmt` (`fstr_display` / `fstr_debug`). Numbers
//! honour their spec; anything else is written plain (but for `#`), then
//! cut and padded.
//! A static spec is checked here with the runtime's own parser, a dynamic
//! one at run time.

use super::bignum::Big;
#[allow(clippy::wildcard_imports)]
use super::*;
use crate::fmt_spec::{self, FmtSpec};
use rokc_ast::{FStringPart, SpecPiece};
use rokc_diagnostic::{Code, Span, Tag};

/// Per-hole capacity guess, beyond the literal bytes.
const HOLE_BYTES: usize = 16;

/// A hole, bound to a temp.
struct Hole {
    value: Expr,
    kind: Kind,
    spec: Spec,
    /// `?`: the Debug.
    debug: bool,
    /// Implements `Display` / `Debug` (picked at run time for a dynamic
    /// spec).
    both: bool,
}

enum Spec {
    None,
    Static(FmtSpec),
    /// The spec text, in a temp.
    Dynamic(Expr),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int {
        signed: bool,
        bits: u32,
        /// An `i64` already: passed to the runtime as is.
        i64: bool,
    },
    Float {
        f32: bool,
    },
    /// `int` / `float`.
    Big,
    Str,
    Char,
    Other,
}

impl Kind {
    const fn number(self) -> bool {
        matches!(self, Self::Int { .. } | Self::Float { .. } | Self::Big)
    }
}

fn kind_of(ctx: &LowerCtx<'_>, ty: &HirTy) -> Kind {
    match ty {
        HirTy::Prim(p) if p.is_signed_int() || p.is_unsigned_int() => Kind::Int {
            i64: *p == PrimTy::I64,
            signed: p.is_signed_int(),
            bits: p.bit_width(super::const_eval::pointer_width()),
        },
        HirTy::Prim(PrimTy::F32) => Kind::Float { f32: true },
        HirTy::Prim(PrimTy::F64) => Kind::Float { f32: false },
        HirTy::Prim(PrimTy::Str) => Kind::Str,
        HirTy::Prim(PrimTy::Char) => Kind::Char,
        _ if super::bignum::big_of(ctx, ty).is_some() => Kind::Big,
        _ => Kind::Other,
    }
}

pub(super) fn lower_fstring(ctx: &mut LowerCtx<'_>, parts: &[FStringPart], span: Span) -> HirExpr {
    ctx.push_scope();
    let mut stmts = Vec::new();
    // `Err(text)`: a literal piece.
    let mut pieces: Vec<Result<Hole, String>> = Vec::new();
    for part in parts {
        match part {
            FStringPart::Lit(text) => pieces.push(Err(text.clone())),
            FStringPart::Hole { expr, spec } => {
                let at = point(expr.span);
                let Some(value) = bind(ctx, &mut stmts, expr) else {
                    continue;
                };
                let ty = ctx.local_ty(local_of(ctx, &value));
                // The spec as text, or as f-string pieces when dynamic.
                let mut text = String::new();
                let mut dynamic = Vec::new();
                for piece in spec {
                    match piece {
                        SpecPiece::Text(t) => {
                            text.push_str(t);
                            dynamic.push(FStringPart::Lit(t.clone()));
                        }
                        SpecPiece::Expr(e) => {
                            if let Some(v) = bind(ctx, &mut stmts, e) {
                                dynamic.push(FStringPart::Hole {
                                    expr: v,
                                    spec: Vec::new(),
                                });
                            }
                        }
                    }
                }
                let is_dynamic = spec.iter().any(|p| matches!(p, SpecPiece::Expr(_)));
                let outer = ctx.at.replace(expr.span);
                let hole = if is_dynamic {
                    let spec_text = lower_fstring(ctx, &dynamic, at);
                    let spec = Spec::Dynamic(bind_hir(ctx, &mut stmts, "#fs", spec_text, at));
                    Some(dynamic_hole(ctx, value, &ty, spec, text.ends_with('?')))
                } else {
                    static_hole(ctx, value, &ty, &text)
                };
                ctx.at.set(outer);
                if let Some(h) = hole {
                    pieces.push(Ok(h));
                }
            }
        }
    }
    let tail = write_all(ctx, &mut stmts, pieces, span);
    ctx.pop_scope();
    HirExpr {
        ty: HirTy::Prim(PrimTy::Str),
        kind: HirExprKind::Block(Box::new(HirBlock {
            stmts,
            trailing: Some(tail),
            label: None,
        })),
        span,
    }
}

/// The writes, in order, against one formatter state; the block's
/// value is its text.
fn write_all(
    ctx: &mut LowerCtx<'_>,
    stmts: &mut Vec<HirStmt>,
    mut pieces: Vec<Result<Hole, String>>,
    span: Span,
) -> HirExpr {
    let holes = pieces.iter().filter(|p| p.is_ok()).count();
    if holes == 0 {
        let text: String = pieces.into_iter().filter_map(Result::err).collect();
        return lower_expr(ctx, &mk(ExprKind::Literal(AstLit::Str(text)), span));
    }
    // `f"{s}"` of a `str` is `s`.
    if let [Ok(h)] = pieces.as_slice() {
        if h.kind == Kind::Str && !h.debug && matches!(h.spec, Spec::None) {
            let Some(Ok(h)) = pieces.pop() else {
                unreachable!("one hole")
            };
            return lower_expr(ctx, &h.value);
        }
    }
    let lit_bytes: usize = pieces
        .iter()
        .filter_map(|p| p.as_ref().err())
        .map(String::len)
        .sum();
    let new = rt(
        "rok_fmt_new",
        vec![int_lit(lit_bytes + HOLE_BYTES * holes, span)],
        span,
    );
    let init = lower_expr(ctx, &new);
    let st = bind_hir(ctx, stmts, "#t", init, span);
    for piece in pieces {
        match piece {
            Err(text) => {
                let lit = mk(ExprKind::Literal(AstLit::Str(text)), span);
                write(
                    ctx,
                    stmts,
                    &st,
                    &rt("rok_fmt_str", vec![st.clone(), lit], span),
                );
            }
            Ok(h) => write_hole(ctx, stmts, &st, h),
        }
    }
    lower_expr(ctx, &rt("rok_fmt_finish", vec![st], span))
}

/// One hole's writes.
fn write_hole(ctx: &mut LowerCtx<'_>, stmts: &mut Vec<HirStmt>, t: &Expr, h: Hole) {
    let at = h.value.span;
    let plain = || words(fmt_spec::PLAIN, at);
    let (flags, counts) = match &h.spec {
        // A number's `{x:?}` is its rendering under type `?`.
        Spec::None if h.debug && h.kind.number() => words(
            (
                fmt_spec::PLAIN.0 | (u64::from(b'?') << 48),
                fmt_spec::PLAIN.1,
            ),
            at,
        ),
        Spec::None => plain(),
        Spec::Static(s) => words(fmt_spec::pack(s), at),
        Spec::Dynamic(_) if !h.kind.number() && h.both => {
            let Spec::Dynamic(text) = h.spec else {
                unreachable!("dynamic")
            };
            let c = hook("fstr_text_any", vec![t.clone(), h.value, text], at);
            write(ctx, stmts, t, &c);
            return;
        }
        Spec::Dynamic(text) => {
            let text_only = mk(ExprKind::Literal(AstLit::Bool(!h.kind.number())), at);
            let packed = lower_expr(ctx, &hook("fstr_pack", vec![text.clone(), text_only], at));
            let p = bind_hir(ctx, stmts, "#sp", packed, at);
            let field = |i| mk(ExprKind::TupleField(Box::new(p.clone()), i), at);
            (field(0), field(1))
        }
    };
    if h.kind.number() || matches!(h.spec, Spec::None) {
        // A number applies the spec in its own rendering.
        write(
            ctx,
            stmts,
            t,
            &plain_write(t, h.value, h.kind, h.debug, flags, counts),
        );
        return;
    }
    // Written plain, then cut and padded as text. `#` (`{x:#?}`) reaches
    // the value's own `fmt`.
    let len = lower_expr(ctx, &rt("rok_fmt_len", vec![t.clone()], at));
    let start = bind_hir(ctx, stmts, "#s", len, at);
    let (pf, pc) = plain();
    let lit = |v: u64| mk(ExprKind::Literal(AstLit::Int(v.to_string())), at);
    let op = |o: &str, a: Expr, b: Expr| {
        mk(
            ExprKind::OpInfix {
                op: o.to_owned(),
                lhs: Box::new(a),
                rhs: Box::new(b),
            },
            at,
        )
    };
    let pf = match &h.spec {
        Spec::Static(s) if s.alt => lit(fmt_spec::PLAIN.0 | fmt_spec::ALT),
        Spec::Dynamic(_) => op("|", pf, op("&", flags.clone(), lit(fmt_spec::ALT))),
        _ => pf,
    };
    write(
        ctx,
        stmts,
        t,
        &plain_write(t, h.value, h.kind, h.debug, pf, pc),
    );
    let pad = rt(
        "rok_fmt_pad",
        vec![
            t.clone(),
            flags,
            counts,
            start.clone(),
            start,
            int_lit(0, at),
        ],
        at,
    );
    write(ctx, stmts, t, &pad);
}

/// The packed spec words as literals.
fn words((flags, counts): (u64, u64), at: Span) -> (Expr, Expr) {
    let lit = |v: u64| mk(ExprKind::Literal(AstLit::Int(v.to_string())), at);
    (lit(flags), lit(counts))
}

/// `value` written under the spec words: primitives by the runtime,
/// anything else through its `Display::fmt` (`Debug::fmt` for `debug`).
fn plain_write(t: &Expr, value: Expr, kind: Kind, debug: bool, flags: Expr, counts: Expr) -> Expr {
    let at = value.span;
    // Only a real conversion: an `i64` / `f64` cast to itself would carry
    // the hole's span, which a user's own cast there starts with too, and
    // `redundant_cast` would see it as one.
    let cast = |v: Expr, to: &str, already: bool| {
        if already {
            return v;
        }
        mk(
            ExprKind::Cast(
                Box::new(v),
                rokc_ast::Ty {
                    kind: rokc_ast::TyKind::Path(path(&[to], at)),
                    span: at,
                },
            ),
            at,
        )
    };
    let flag = |b: bool| int_lit(usize::from(b), at);
    let fmt = |f: &str, args: Vec<Expr>| hook(f, args, at);
    match (kind, debug) {
        (Kind::Int { signed, bits, i64 }, _) => rt(
            "rok_fmt_int",
            vec![
                t.clone(),
                flags,
                counts,
                cast(value, "i64", i64),
                flag(signed),
                int_lit(bits as usize, at),
            ],
            at,
        ),
        (Kind::Float { f32 }, _) => rt(
            "rok_fmt_f64",
            vec![
                t.clone(),
                flags,
                counts,
                cast(value, "f64", !f32),
                flag(f32),
            ],
            at,
        ),
        (Kind::Big, _) => fmt("fstr_num", vec![t.clone(), value, flags, counts]),
        (Kind::Str, false) => rt("rok_fmt_str", vec![t.clone(), value], at),
        (Kind::Str, true) => rt(
            "rok_fmt_escaped",
            vec![t.clone(), value, int_lit(usize::from(b'"'), at)],
            at,
        ),
        (Kind::Char, false) => rt(
            "rok_fmt_char",
            vec![t.clone(), cast(value, "u32", false)],
            at,
        ),
        (_, true) => fmt("fstr_debug", vec![t.clone(), value, flags]),
        (_, false) => fmt("fstr_display", vec![t.clone(), value, flags]),
    }
}

/// `let #fN = e;` - the temp's path, or `None` when `e` didn't type.
fn bind(ctx: &mut LowerCtx<'_>, stmts: &mut Vec<HirStmt>, e: &Expr) -> Option<Expr> {
    // A hole has no expected type: the f-string's own `str` slot is not it.
    ctx.push_expected(HirTy::Unknown);
    let init = lower_expr(ctx, e);
    ctx.pop_expected();
    if init.ty == HirTy::Unknown {
        return None;
    }
    Some(bind_hir(ctx, stmts, "#f", init, point(e.span)))
}

/// `let <prefix>N = init;` - the temp's path.
fn bind_hir(
    ctx: &mut LowerCtx<'_>,
    stmts: &mut Vec<HirStmt>,
    prefix: &str,
    init: HirExpr,
    at: Span,
) -> Expr {
    let name = format!("{prefix}{}", ctx.locals.len());
    let local = ctx.declare(Some(name.clone()), init.ty.clone());
    stmts.push(HirStmt::Let { local, init });
    mk(ExprKind::Path(path(&[&name], at)), at)
}

/// `st = e;`: a write returns the state, maybe moved.
fn write(ctx: &mut LowerCtx<'_>, stmts: &mut Vec<HirStmt>, st: &Expr, e: &Expr) {
    let local = local_of(ctx, st);
    let value = lower_expr(ctx, e);
    stmts.push(HirStmt::Assign { local, value });
}

/// The empty span at the start of `s`: diagnostics about the code made
/// for a hole land on the hole, but it covers no source (editor hover
/// finds the user's expression).
const fn point(s: Span) -> Span {
    Span { end: s.start, ..s }
}

fn local_of(ctx: &LowerCtx<'_>, e: &Expr) -> LocalId {
    let ExprKind::Path(p) = &e.kind else {
        unreachable!("a bound temp")
    };
    ctx.lookup(&p.segments[0].name).expect("bound temp")
}

fn hole(ctx: &LowerCtx<'_>, value: Expr, ty: &HirTy, spec: Spec, debug: bool) -> Hole {
    let both =
        ctx.has_trait(ty, ctx.lang("display_trait")) && ctx.has_trait(ty, ctx.lang("debug_trait"));
    Hole {
        value,
        kind: kind_of(ctx, ty),
        spec,
        debug,
        both,
    }
}

/// `{:?}` and `{}` as written in an f-string.
const DEBUG_HOLE: &str = concat!("{", ":?}");
const PLAIN_HOLE: &str = concat!("{", "}");

/// A value the runtime does not format itself needs `Display` (`Debug`
/// for `{:?}`); report a missing impl here, at the hole, rather than
/// inside the library's formatting function.
fn formattable(ctx: &LowerCtx<'_>, ty: &HirTy, debug: bool) -> bool {
    let (slot, name) = if debug {
        ("debug_trait", "Debug")
    } else {
        ("display_trait", "Display")
    };
    let tr = ctx.lang(slot);
    if kind_of(ctx, ty) != Kind::Other || ctx.has_trait(ty, tr) {
        return true;
    }
    // An unknown type name is reported on its own.
    if matches!(ty, HirTy::Path(_)) {
        return false;
    }
    let shown = ctx.ty_name(ty);
    let culprit = match ty {
        HirTy::Sum(vs) => vs
            .iter()
            .find(|v| !ctx.has_trait(v, tr))
            .map(|v| format!(": member `{}` does not implement `{name}`", ctx.ty_name(v))),
        _ => None,
    }
    .unwrap_or_else(|| format!(": it does not implement `{name}`"));
    let hole = if debug { DEBUG_HOLE } else { PLAIN_HOLE };
    ctx.emit_error(
        Code::INVALID_FORMAT,
        format!("`{shown}` cannot be formatted with `{hole}`{culprit}"),
    );
    let (tag, help) = if debug {
        (
            Tag::NoDebug,
            "add `#[derive(Debug)]` to the type, or implement `Debug` for it".to_owned(),
        )
    } else if ctx.has_trait(ty, ctx.lang("debug_trait")) {
        (
            Tag::NoDisplayHasDebug,
            format!("format it with `{DEBUG_HOLE}`, or implement `Display` for it"),
        )
    } else {
        (
            Tag::NoDisplay,
            format!(
                "implement `Display` for it, or derive `Debug` and format it with `{DEBUG_HOLE}`"
            ),
        )
    };
    ctx.help_last(Some(help));
    ctx.tag_last(tag);
    false
}

/// A hole with a static spec: checked against the value's type here.
fn static_hole(ctx: &LowerCtx<'_>, value: Expr, ty: &HirTy, text: &str) -> Option<Hole> {
    if text.is_empty() || text == "?" {
        return formattable(ctx, ty, text == "?")
            .then(|| hole(ctx, value, ty, Spec::None, text == "?"));
    }
    let spec = match fmt_spec::parse(text) {
        Ok(s) => s,
        Err(why) => {
            ctx.emit_error(
                Code::INVALID_FORMAT,
                format!("invalid format spec `{text}`: {why} (refs-lang 1.7.3)"),
            );
            return None;
        }
    };
    let (int, float) = match (ty, super::bignum::big_of(ctx, ty)) {
        (HirTy::Prim(p), _) => (p.is_int(), p.is_float()),
        (_, Some(b)) => (b == Big::Int, b == Big::Float),
        _ => (false, false),
    };
    let needs = match spec.ty {
        b'b' | b'o' | b'x' | b'X' if !int => Some("an integer (stdlib 2.10.5)"),
        b'e' | b'E' | b'f' | b'g' if !int && !float => Some("a number (stdlib 2.10.6)"),
        _ => None,
    };
    if let Some(needs) = needs {
        ctx.emit_error(
            Code::INVALID_FORMAT,
            format!(
                "format type `{}` needs {needs}, found `{}`",
                char::from(spec.ty),
                ctx.ty_name(ty)
            ),
        );
        return None;
    }
    let number = kind_of(ctx, ty).number();
    let debug = spec.ty == b'?';
    // Nothing to apply: the plain rendering (a non-number's sign / `0` /
    // `#` flags do nothing).
    let nothing = matches!(spec.ty, 0 | b's' | b'd')
        && spec.width.is_none()
        && spec.precision.is_none()
        && (!number || (spec.sign != b'+' && spec.sign != b' '));
    let spec = if nothing {
        Spec::None
    } else {
        Spec::Static(spec)
    };
    formattable(ctx, ty, debug && !number).then(|| hole(ctx, value, ty, spec, debug && !number))
}

/// A hole with a dynamic spec: a value with only one of `Display` /
/// `Debug` takes that one; `?` known from the text picks the Debug.
fn dynamic_hole(ctx: &LowerCtx<'_>, value: Expr, ty: &HirTy, spec: Spec, q: bool) -> Hole {
    let mut h = hole(ctx, value, ty, spec, q);
    if !h.kind.number() && !q {
        let display = ctx.has_trait(ty, ctx.lang("display_trait"));
        h.debug = !display;
        h.both &= display;
    }
    h.both &= !q;
    h
}

const fn mk(kind: ExprKind, span: Span) -> Expr {
    Expr { kind, span }
}

fn call(names: &[&str], args: Vec<Expr>, at: Span) -> Expr {
    mk(
        ExprKind::Call {
            callee: Box::new(mk(ExprKind::Path(path(names, at)), at)),
            args: args.into_iter().map(CallArg::Positional).collect(),
            trailing_closure: None,
        },
        at,
    )
}

/// `rok::core::fmt::<f>(args..)`, through its lang slot.
fn hook(f: &str, args: Vec<Expr>, at: Span) -> Expr {
    mk(
        ExprKind::Call {
            callee: Box::new(mk(ExprKind::Path(super::expr::lang_fn("fmt", f, at)), at)),
            args: args.into_iter().map(CallArg::Positional).collect(),
            trailing_closure: None,
        },
        at,
    )
}

/// `unsafe { name(args) }`: a runtime entry (`core::rt`).
fn rt(name: &str, args: Vec<Expr>, at: Span) -> Expr {
    mk(
        ExprKind::Unsafe(rokc_ast::Block {
            stmts: Vec::new(),
            trailing: Some(Box::new(call(&[name], args, at))),
            span: at,
            label: None,
        }),
        at,
    )
}

fn path(names: &[&str], span: Span) -> AstPath {
    AstPath {
        segments: names
            .iter()
            .map(|n| rokc_ast::PathSegment {
                name: (*n).to_owned(),
                generics: Vec::new(),
            })
            .collect(),
        span,
        origin: None,
    }
}

fn int_lit(v: usize, at: Span) -> Expr {
    mk(ExprKind::Literal(AstLit::Int(v.to_string())), at)
}
