//! MIR pretty-printer for `--emit mir`.

use std::fmt::Write;

use rokc_ir_common::{ConstVal, PrimTy};

use crate::{BasicBlock, MirFunction, MirModule, MirTy, Operand, RValue, Statement, Terminator};

fn print_arg_list(out: &mut String, args: &[Operand]) {
    if args.is_empty() {
        return;
    }
    out.push('(');
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        print_operand(out, a);
    }
    out.push(')');
}

#[must_use]
pub fn print_mir(module: &MirModule) -> String {
    let mut out = String::new();
    for f in &module.functions {
        print_function(&mut out, f);
        out.push('\n');
    }
    out
}

fn print_function(out: &mut String, f: &MirFunction) {
    let _ = write!(out, "fn {}(", f.name);
    for (i, p) in f.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let ty = f.param_tys.get(i).cloned().unwrap_or(MirTy::Any);
        let _ = write!(out, "_{}: {}", p.0, ty_str(&ty));
    }
    let _ = writeln!(out, ") -> {} {{", ty_str(&f.ret_ty));

    for l in &f.locals {
        if !f.params.iter().any(|p| *p == l.id) {
            let _ = writeln!(out, "    let _{}: {};", l.id.0, ty_str(&l.ty),);
        }
    }
    if !f.locals.is_empty() {
        out.push('\n');
    }

    for b in &f.blocks {
        print_block(out, b);
    }
    out.push_str("}\n");
}

fn print_block(out: &mut String, b: &BasicBlock) {
    let _ = write!(out, "    bb{}", b.id.0);
    if !b.params.is_empty() {
        out.push('(');
        for (i, p) in b.params.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "_{}", p.0);
        }
        out.push(')');
    }
    out.push_str(":\n");
    for s in b.stmts.iter().filter(|s| !matches!(s, Statement::Loc(..))) {
        out.push_str("        ");
        print_stmt(out, s);
        out.push('\n');
    }
    out.push_str("        ");
    print_terminator(out, &b.terminator);
    out.push('\n');
}

fn print_stmt(out: &mut String, s: &Statement) {
    match s {
        // Skipped by `print_block`: the text feeds the package hash (a
        // moved line must not change symbols).
        Statement::Loc(..) => {}
        Statement::Assign { target, rvalue } => {
            let _ = write!(out, "_{} = ", target.0);
            print_rvalue(out, rvalue);
            out.push(';');
        }
        Statement::ArrayWrite {
            array,
            offset,
            index,
            value,
            ..
        } => {
            let _ = write!(out, "_{}{}[", array.0, at_offset(*offset));
            print_operand(out, index);
            out.push_str("] = ");
            print_operand(out, value);
            out.push(';');
        }
        Statement::StorageDead(l) => {
            let _ = write!(out, "dead _{};", l.0);
        }
        Statement::RawStore {
            addr, value, ty, ..
        } => {
            let _ = write!(out, "raw_store<{}> ", ty_str(ty));
            print_operand(out, addr);
            out.push_str(" = ");
            print_operand(out, value);
            out.push(';');
        }
        Statement::BufStore {
            buf, index, value, ..
        } => {
            out.push_str("buf_store ");
            print_operand(out, buf);
            out.push('[');
            print_operand(out, index);
            out.push_str("] = ");
            print_operand(out, value);
            out.push(';');
        }
        Statement::HeapStore {
            ptr, index, value, ..
        } => {
            out.push_str("heap_store ");
            print_operand(out, ptr);
            let _ = write!(out, ".{index} = ");
            print_operand(out, value);
            out.push(';');
        }
        Statement::StaticStore { name, value, .. } => {
            let _ = write!(out, "static {name} = ");
            print_operand(out, value);
            out.push(';');
        }
    }
}

fn print_rvalue(out: &mut String, r: &RValue) {
    match r {
        RValue::Use(op) => print_operand(out, op),
        RValue::BinOp {
            op,
            op_ty,
            lhs,
            rhs,
        } => {
            let _ = write!(out, "{op:?}<{}>(", ty_str(op_ty));
            print_operand(out, lhs);
            out.push_str(", ");
            print_operand(out, rhs);
            out.push(')');
        }
        RValue::UnaryOp { op, op_ty, operand } => {
            let _ = write!(out, "{op:?}<{}>(", ty_str(op_ty));
            print_operand(out, operand);
            out.push(')');
        }
        RValue::Call {
            callee,
            args,
            ret_ty,
        } => {
            let _ = write!(out, "call fn#{}<{}>(", callee.0, ty_str(ret_ty));
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_operand(out, a);
            }
            out.push(')');
        }
        RValue::StructInit { ty, fields } => {
            let _ = write!(out, "struct<{}>{{", ty_str(ty));
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_operand(out, f);
            }
            out.push('}');
        }
        RValue::FieldRead {
            target,
            index,
            field_ty,
        } => {
            print_operand(out, target);
            let _ = write!(out, ".{index}<{}>", ty_str(field_ty));
        }
        RValue::Cast {
            source,
            from_ty,
            to_ty,
        }
        | RValue::Bitcast {
            source,
            from_ty,
            to_ty,
        } => {
            out.push_str(if matches!(r, RValue::Cast { .. }) {
                "cast<"
            } else {
                "bitcast<"
            });
            out.push_str(&ty_str(from_ty));
            out.push_str(" → ");
            out.push_str(&ty_str(to_ty));
            out.push_str(">(");
            print_operand(out, source);
            out.push(')');
        }
        RValue::WithField {
            source,
            index,
            value,
            ..
        } => {
            print_operand(out, source);
            let _ = write!(out, " with .{index} = ");
            print_operand(out, value);
        }
        RValue::ArrayInit { elements, .. } => {
            out.push('[');
            for (i, e) in elements.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_operand(out, e);
            }
            out.push(']');
        }
        RValue::ArrayIndex {
            array,
            offset,
            index,
            ..
        } => {
            let _ = write!(out, "_{}{}[", array.0, at_offset(*offset));
            print_operand(out, index);
            out.push(']');
        }
        RValue::FnAddr { callee, fn_ty } => {
            let _ = write!(out, "&fn#{}<{}>", callee.0, ty_str(fn_ty));
        }
        RValue::CallIndirect {
            callee,
            args,
            ret_ty,
            abi,
            ..
        } => {
            out.push_str(if *abi == rokc_ir_common::Abi::Rok {
                "call_indirect "
            } else {
                "call_c "
            });
            print_operand(out, callee);
            let _ = write!(out, "<{}>(", ty_str(ret_ty));
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_operand(out, a);
            }
            out.push(')');
        }
        RValue::StaticLoad { name, ty } => {
            let _ = write!(out, "static[{}] {name}", ty_str(ty));
        }
        RValue::HeapBox { value, ty } => {
            let _ = write!(out, "heap_box<{}> ", ty_str(ty));
            print_operand(out, value);
        }
        RValue::HeapAlloc { ty } => {
            let _ = write!(out, "heap_alloc<{}>", ty_str(ty));
        }
        RValue::HeapField { ptr, index, .. } => {
            out.push_str("heap_field ");
            print_operand(out, ptr);
            let _ = write!(out, ".{index}");
        }
        RValue::HeapLoad { ptr, ty } => {
            let _ = write!(out, "heap_load<{}> ", ty_str(ty));
            print_operand(out, ptr);
        }
        RValue::BufAlloc { elem_ty, len } => {
            let _ = write!(out, "buf_alloc<{}> ", ty_str(elem_ty));
            print_operand(out, len);
        }
        RValue::RawLoad { addr, ty, .. } => {
            let _ = write!(out, "raw_load<{}> ", ty_str(ty));
            print_operand(out, addr);
        }
        RValue::AddrOf(l) => {
            let _ = write!(out, "addr_of %{}", l.0);
        }
        RValue::StaticAddr(name) => {
            let _ = write!(out, "addr_of static {name}");
        }
        RValue::SizeOf { ty, align } => {
            let f = if *align { "align_of" } else { "size_of" };
            let _ = write!(out, "{f}<{}>", ty_str(ty));
        }
        RValue::BufLoad {
            buf,
            index,
            elem_ty,
        } => {
            let _ = write!(out, "buf_load<{}> ", ty_str(elem_ty));
            print_operand(out, buf);
            out.push('[');
            print_operand(out, index);
            out.push(']');
        }
        RValue::BufAtomic {
            op,
            buf,
            index,
            args,
            elem_ty,
            order,
        } => {
            let _ = write!(
                out,
                "atomic_{op}<{}, {}> ",
                ty_str(elem_ty),
                order.join("/")
            );
            print_operand(out, buf);
            out.push('[');
            print_operand(out, index);
            out.push(']');
            for a in args {
                out.push_str(", ");
                print_operand(out, a);
            }
        }
    }
}

fn print_operand(out: &mut String, op: &Operand) {
    match op {
        Operand::Const(c) => print_const(out, c),
        Operand::Copy(id) => {
            let _ = write!(out, "_{}", id.0);
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
            let _ = write!(out, "b{:?}", b.escape_ascii().to_string());
        }
        ConstVal::None => out.push_str("none"),
    }
}

fn print_terminator(out: &mut String, t: &Terminator) {
    match t {
        Terminator::Goto { target, args } => {
            let _ = write!(out, "goto bb{}", target.0);
            print_arg_list(out, args);
        }
        Terminator::If {
            cond,
            then_bb,
            then_args,
            else_bb,
            else_args,
        } => {
            out.push_str("if ");
            print_operand(out, cond);
            let _ = write!(out, " then bb{}", then_bb.0);
            print_arg_list(out, then_args);
            let _ = write!(out, " else bb{}", else_bb.0);
            print_arg_list(out, else_args);
        }
        Terminator::Return(op) => {
            out.push_str("return");
            if let Some(o) = op {
                out.push(' ');
                print_operand(out, o);
            }
        }
        Terminator::Unreachable => out.push_str("unreachable"),
        Terminator::Suspend {
            value,
            resume,
            is_final,
        } => {
            out.push_str(if *is_final {
                "suspend_final"
            } else {
                "suspend"
            });
            if let Some(o) = value {
                out.push(' ');
                print_operand(out, o);
            }
            let _ = write!(out, " -> bb{}", resume.0);
        }
    }
}

fn ty_str(t: &MirTy) -> String {
    match t {
        MirTy::Unit => "()".to_owned(),
        MirTy::Prim(p) => p.name().to_owned(),
        MirTy::Any => "any".to_owned(),
        MirTy::GcRef => "gcref".to_owned(),
        MirTy::Tuple(parts) => {
            let inner = parts.iter().map(ty_str).collect::<Vec<_>>().join(", ");
            format!("({inner})")
        }
        MirTy::Struct(id, fields) => {
            let inner = fields.iter().map(ty_str).collect::<Vec<_>>().join(", ");
            format!("Struct#{}{{{inner}}}", id.0)
        }
        MirTy::Array(t, n) => format!("[{}; {n}]", ty_str(t)),
        MirTy::Union(vs, n, _) => {
            let inner = vs
                .iter()
                .map(|(tag, t)| format!("{tag}: {}", ty_str(t)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("union<{n}>{{{inner}}}")
        }
        MirTy::Fn(args, ret) => {
            let inner = args.iter().map(ty_str).collect::<Vec<_>>().join(", ");
            format!("fn({inner}) -> {}", ty_str(ret))
        }
    }
}

/// `@+8` for an array 8 bytes into its local; nothing at 0.
fn at_offset(offset: u32) -> String {
    if offset == 0 {
        String::new()
    } else {
        format!("@+{offset}")
    }
}
