//! HIR pretty-printer for `--emit hir`.

use std::fmt::Write;

use rokc_ir_common::{ConstVal, PrimTy};

use crate::{HirBlock, HirExpr, HirExprKind, HirFunction, HirModule, HirPattern, HirStmt, HirTy};

#[must_use]
pub fn print_hir(module: &HirModule) -> String {
    let mut out = String::new();
    for f in &module.functions {
        print_function(&mut out, f);
        out.push('\n');
    }
    out
}

/// `name(params) -> ret`.
fn header(out: &mut String, name: &str, f: &HirFunction) {
    let _ = write!(out, "{name}(");
    for (i, p) in f.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{}: {}", p.name, ty_str(&p.ty));
    }
    let _ = write!(out, ") -> {}", ty_str(&f.ret_ty));
}

/// A generic instance as its item path and concrete signature
/// (`id(x: i32) -> i32`); `None` for other functions.
#[must_use]
pub fn instance(f: &HirFunction) -> Option<String> {
    f.facts.mono_args.as_ref()?;
    let mut out = String::new();
    header(&mut out, f.facts.path(&f.name), f);
    Some(out)
}

fn print_function(out: &mut String, f: &HirFunction) {
    out.push_str("fn ");
    header(out, &f.name, f);
    if let Some(body) = &f.body {
        out.push_str(" {\n");
        print_block(out, body, 1);
        out.push_str("}\n");
    } else {
        out.push_str(";\n");
    }
}

fn print_block(out: &mut String, b: &HirBlock, indent: usize) {
    for s in &b.stmts {
        pad(out, indent);
        print_stmt(out, s);
        out.push('\n');
    }
    if let Some(t) = &b.trailing {
        pad(out, indent);
        out.push_str("trailing ");
        print_expr(out, t);
        out.push('\n');
    }
}

fn print_stmt(out: &mut String, s: &HirStmt) {
    match s {
        HirStmt::Let { local, init } => {
            let _ = write!(out, "let _{}: {} = ", local.0, ty_str(&init.ty));
            print_expr(out, init);
            out.push(';');
        }
        HirStmt::Assign { local, value } => {
            let _ = write!(out, "_{} = ", local.0);
            print_expr(out, value);
            out.push(';');
        }
        HirStmt::ArrayAssign {
            array_local,
            path,
            index,
            value,
            ..
        } => {
            let _ = write!(out, "_{}", array_local.0);
            for k in path {
                let _ = write!(out, ".{k}");
            }
            out.push('[');
            print_expr(out, index);
            out.push_str("] = ");
            print_expr(out, value);
            out.push(';');
        }
        HirStmt::Expr(e) => {
            print_expr(out, e);
            out.push(';');
        }
        HirStmt::Return(v) => {
            out.push_str("return");
            if let Some(e) = v {
                out.push(' ');
                print_expr(out, e);
            }
            out.push(';');
        }
        HirStmt::Break { label, value } => {
            out.push_str("break");
            if let Some(l) = label {
                let _ = write!(out, " {l}");
            }
            if let Some(e) = value {
                out.push(' ');
                print_expr(out, e);
            }
            out.push(';');
        }
        HirStmt::Continue { label } => {
            out.push_str("continue");
            if let Some(l) = label {
                let _ = write!(out, " {l}");
            }
            out.push(';');
        }
        HirStmt::StaticAssign { name, value } => {
            let _ = write!(out, "static {name} = ");
            print_expr(out, value);
            out.push(';');
        }
        HirStmt::Panic(call) => {
            out.push_str("panic ");
            print_expr(out, call);
            out.push(';');
        }
    }
}

fn print_expr(out: &mut String, e: &HirExpr) {
    match &e.kind {
        HirExprKind::Const(c) => print_const(out, c),
        HirExprKind::Local(id) => {
            let _ = write!(out, "_{}", id.0);
        }
        HirExprKind::StaticRef(name) => {
            let _ = write!(out, "static {name}");
        }
        HirExprKind::Block(b) => {
            out.push_str("{\n");
            print_block(out, b, 2);
            out.push('}');
        }
        HirExprKind::UnresolvedPath(parts) => {
            out.push_str(&parts.join("::"));
        }
        HirExprKind::PrimBinOp { op, lhs, rhs } => {
            let _ = write!(out, "{op:?}(");
            print_expr(out, lhs);
            out.push_str(", ");
            print_expr(out, rhs);
            out.push(')');
        }
        HirExprKind::PrimUnaryOp { op, operand } => {
            let _ = write!(out, "{op:?}(");
            print_expr(out, operand);
            out.push(')');
        }
        HirExprKind::Call { callee, args } => {
            let _ = write!(out, "fn#{}", callee.0);
            print_args(out, args);
        }
        HirExprKind::FnRef(id) => {
            let _ = write!(out, "&fn#{}", id.0);
        }
        HirExprKind::CallIndirect { callee, args, .. } => {
            print_expr(out, callee);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_expr(out, a);
            }
            out.push(')');
        }
        HirExprKind::Cast { operand, to_ty, .. } => {
            print_expr(out, operand);
            let _ = write!(out, " as {}", ty_str(to_ty));
        }
        HirExprKind::ClosureValue { fn_id, env, .. } => {
            let _ = write!(out, "closure(&fn#{}, ", fn_id.0);
            print_expr(out, env);
            out.push(')');
        }
        HirExprKind::CallClosure { closure, args, .. } => {
            print_expr(out, closure);
            out.push_str(".call(");
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_expr(out, a);
            }
            out.push(')');
        }
        HirExprKind::If {
            cond,
            then_block,
            else_branch,
        } => {
            out.push_str("if ");
            print_expr(out, cond);
            out.push_str(" {\n");
            print_block(out, then_block, 2);
            out.push('}');
            if let Some(e) = else_branch {
                out.push_str(" else ");
                print_expr(out, e);
            }
        }
        HirExprKind::While { label, cond, body } => {
            if let Some(l) = label {
                let _ = write!(out, "{l}: ");
            }
            out.push_str("while ");
            print_expr(out, cond);
            out.push_str(" {\n");
            print_block(out, body, 2);
            out.push('}');
        }
        HirExprKind::Loop { label, body, .. } => {
            if let Some(l) = label {
                let _ = write!(out, "{l}: ");
            }
            out.push_str("loop {\n");
            print_block(out, body, 2);
            out.push('}');
        }
        HirExprKind::StructInit { struct_id, fields } => {
            let _ = write!(out, "Struct#{}{{", struct_id.0);
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_expr(out, f);
            }
            out.push('}');
        }
        HirExprKind::FieldRead {
            target,
            struct_id,
            field_index,
            ..
        } => {
            print_expr(out, target);
            let _ = write!(out, ".#{}.{}", struct_id.0, field_index);
        }
        HirExprKind::WithField {
            target,
            struct_id,
            field_index,
            value,
        } => {
            print_expr(out, target);
            let _ = write!(out, " with #{}.{} = ", struct_id.0, field_index);
            print_expr(out, value);
        }
        HirExprKind::Tuple { fields } => {
            out.push('(');
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_expr(out, f);
            }
            out.push(')');
        }
        HirExprKind::TupleField { target, index, .. } => {
            print_expr(out, target);
            let _ = write!(out, ".{index}");
        }
        HirExprKind::ArrayRepeat { value, len, .. } => {
            out.push('[');
            print_expr(out, value);
            let _ = write!(out, "; {len}]");
        }
        HirExprKind::ArrayInit { elements, .. } => {
            out.push('[');
            for (i, e) in elements.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_expr(out, e);
            }
            out.push(']');
        }
        HirExprKind::BufNew { len, .. } => {
            out.push_str("buf_new(");
            print_expr(out, len);
            out.push(')');
        }
        HirExprKind::AddrOf { place } => {
            out.push_str("addr_of(");
            print_expr(out, place);
            out.push(')');
        }
        HirExprKind::RawRead { addr, .. } => {
            out.push_str("ptr_read(");
            print_expr(out, addr);
            out.push(')');
        }
        HirExprKind::RawWrite { addr, value, .. } => {
            out.push_str("ptr_write(");
            print_expr(out, addr);
            out.push_str(", ");
            print_expr(out, value);
            out.push(')');
        }
        HirExprKind::SizeOf { align, .. } => {
            out.push_str(if *align { "align_of()" } else { "size_of()" });
        }
        HirExprKind::BufGet { buf, index, .. } => {
            out.push_str("buf_get(");
            print_expr(out, buf);
            out.push_str(", ");
            print_expr(out, index);
            out.push(')');
        }
        HirExprKind::BufSet {
            buf, index, value, ..
        } => {
            out.push_str("buf_set(");
            print_expr(out, buf);
            out.push_str(", ");
            print_expr(out, index);
            out.push_str(", ");
            print_expr(out, value);
            out.push(')');
        }
        HirExprKind::BufAtomic {
            op,
            buf,
            index,
            args,
            ..
        } => {
            let _ = write!(out, "atomic_{op}(");
            print_expr(out, buf);
            out.push_str(", ");
            print_expr(out, index);
            for a in args {
                out.push_str(", ");
                print_expr(out, a);
            }
            out.push(')');
        }
        HirExprKind::ArrayIndex { array, index, .. } => {
            print_expr(out, array);
            out.push('[');
            print_expr(out, index);
            out.push(']');
        }
        HirExprKind::SumWrap {
            variant_index,
            value,
            ..
        } => {
            let _ = write!(out, "Sum::v{}", variant_index);
            if let Some(v) = value {
                out.push('(');
                print_expr(out, v);
                out.push(')');
            }
        }
        HirExprKind::EnumVariantInit {
            enum_id,
            variant_index,
            args,
        } => {
            let _ = write!(out, "Enum#{}::v{}", enum_id.0, variant_index);
            if !args.is_empty() {
                out.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    print_expr(out, a);
                }
                out.push(')');
            }
        }
        HirExprKind::UnionWrite {
            struct_id,
            value,
            storage_ty,
            ..
        } => {
            let _ = write!(
                out,
                "Union#{}::write_as<{}>(",
                struct_id.0,
                ty_str(storage_ty)
            );
            print_expr(out, value);
            out.push(')');
        }
        HirExprKind::UnionRead {
            target,
            field_name,
            field_ty,
            ..
        } => {
            print_expr(out, target);
            let _ = write!(out, ".{field_name}<as {}>", ty_str(field_ty));
        }
        HirExprKind::Suspend { .. } | HirExprKind::CoroNew { .. } => print_coro(out, e),
        HirExprKind::Match { scrutinee, arms } => {
            out.push_str("match ");
            print_expr(out, scrutinee);
            out.push_str(" {\n");
            for arm in arms {
                out.push_str("    ");
                print_pattern(out, &arm.pat);
                if let Some(g) = &arm.guard {
                    out.push_str(" if ");
                    print_expr(out, g);
                }
                out.push_str(" => ");
                print_expr(out, &arm.body);
                out.push_str(",\n");
            }
            out.push('}');
        }
    }
}

/// `(a, b, ..)`.
fn print_args(out: &mut String, args: &[HirExpr]) {
    out.push('(');
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        print_expr(out, a);
    }
    out.push(')');
}

fn print_coro(out: &mut String, e: &HirExpr) {
    match &e.kind {
        HirExprKind::Suspend { value, is_final } => {
            out.push_str(if *is_final {
                "suspend_final "
            } else {
                "suspend "
            });
            print_expr(out, value);
        }
        HirExprKind::CoroNew { body_fn, args } => {
            let _ = write!(out, "coro_new fn#{}(", body_fn.0);
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_expr(out, a);
            }
            out.push(')');
        }
        _ => {}
    }
}

fn print_pattern(out: &mut String, p: &HirPattern) {
    match p {
        HirPattern::Wildcard => out.push('_'),
        HirPattern::Binding { local, ty } => {
            let _ = write!(out, "_{}: {}", local.0, ty_str(ty));
        }
        HirPattern::At { local, ty, sub } => {
            let _ = write!(out, "_{}: {} @ ", local.0, ty_str(ty));
            print_pattern(out, sub);
        }
        HirPattern::Literal { value, prim } => {
            let _ = write!(out, "({})", prim.name());
            print_const(out, value);
        }
        HirPattern::Or(alts) => {
            for (i, a) in alts.iter().enumerate() {
                if i > 0 {
                    out.push_str(" | ");
                }
                print_pattern(out, a);
            }
        }
        HirPattern::Tuple(sub) => {
            out.push('(');
            for (i, s) in sub.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_pattern(out, s);
            }
            out.push(')');
        }
        HirPattern::Range {
            start,
            end,
            inclusive,
            prim,
        } => {
            if let Some(s) = start {
                let _ = write!(out, "({})", prim.name());
                print_const(out, s);
            }
            out.push_str(if *inclusive { "..=" } else { ".." });
            if let Some(e) = end {
                let _ = write!(out, "({})", prim.name());
                print_const(out, e);
            }
        }
        HirPattern::Struct { struct_id, fields } => {
            let _ = write!(out, "Struct#{} {{ ", struct_id.0);
            for (i, (idx, sub)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, ".{idx}: ");
                print_pattern(out, sub);
            }
            out.push_str(" }");
        }
        HirPattern::EnumVariant {
            enum_id,
            variant_index,
            args,
        } => {
            let _ = write!(out, "Enum#{}::v{}", enum_id.0, variant_index);
            if !args.is_empty() {
                out.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    print_pattern(out, a);
                }
                out.push(')');
            }
        }
        HirPattern::SumVariant {
            variant_index,
            binder,
            ..
        } => {
            if let Some(b) = binder {
                let _ = write!(out, "_{}: ", b.0);
            }
            let _ = write!(out, "Sum::v{}", variant_index);
        }
        HirPattern::SumMember {
            variant_index, sub, ..
        } => {
            let _ = write!(out, "Sum::v{}(", variant_index);
            print_pattern(out, sub);
            out.push(')');
        }
    }
}

fn print_const(out: &mut String, c: &ConstVal) {
    match c {
        ConstVal::Unit => out.push_str("()"),
        ConstVal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        ConstVal::Int { value, ty } => {
            let _ = write!(out, "{value}{}", ty.name());
        }
        ConstVal::UInt { value, ty } => {
            let _ = write!(out, "{value}{}", ty.name());
        }
        ConstVal::Float { bits, ty } => match ty {
            PrimTy::F32 => {
                let _ = write!(out, "{}f32", f32::from_bits(*bits as u32));
            }
            _ => {
                let _ = write!(out, "{}f64", f64::from_bits(*bits));
            }
        },
        ConstVal::Char(c) => {
            let _ = write!(out, "'{}'", char::from_u32(*c).unwrap_or('?'));
        }
        ConstVal::Str(s) => {
            let _ = write!(out, "{s:?}");
        }
        ConstVal::Bytes(b) => {
            let _ = write!(out, "b\"{}\"", rokc_ast::escape::escape_bytes(b));
        }
        ConstVal::None => out.push_str("none"),
    }
}

fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("    ");
    }
}

fn ty_str(t: &HirTy) -> String {
    match t {
        HirTy::GcBuf(e) => format!("GcBuf<{}>", ty_str(e)),
        HirTy::Never => "!".to_owned(),
        HirTy::Unknown => "?".to_owned(),
        HirTy::Unit => "()".to_owned(),
        HirTy::Prim(p) => p.name().to_owned(),
        HirTy::Any => "any".to_owned(),
        HirTy::AnyTrait(b) if b.is_empty() => "any".to_owned(),
        HirTy::AnyTrait(b) => {
            let b: Vec<String> = b.iter().map(|r| r.render(&r.path, ty_str)).collect();
            format!("any ({})", b.join(" + "))
        }
        HirTy::Path(parts) => parts.join("::"),
        HirTy::None => "none".to_owned(),
        HirTy::Const(v, _) => v.to_string(),
        HirTy::OpenConst(n) => format!("#{n}"),
        HirTy::NoThrow => "#nothrow".to_owned(),
        HirTy::Var(v) => format!("#var::{v}"),
        HirTy::Param(v) => format!("#param::{v}"),
        HirTy::Sum(_) => t.sum_name(ty_str),
        HirTy::Tuple(parts) => {
            let inner = parts.iter().map(ty_str).collect::<Vec<_>>().join(", ");
            format!("({inner})")
        }
        HirTy::Struct(id) => format!("Struct#{}", id.0),
        HirTy::Enum(id) => format!("Enum#{}", id.0),
        HirTy::Array(t, n) => format!("[{}; {n}]", ty_str(t)),
        HirTy::Fn(args, ret, _) => {
            let inner = args.iter().map(ty_str).collect::<Vec<_>>().join(", ");
            format!("fn({inner}) -> {}", ty_str(ret))
        }
        HirTy::Closure { args, ret, .. } => {
            let inner = args.iter().map(ty_str).collect::<Vec<_>>().join(", ");
            format!("closure({inner}) -> {}", ty_str(ret))
        }
        HirTy::Throws { ok, err } => format!("throws ({}) {}", ty_str(err), ty_str(ok)),
    }
}
