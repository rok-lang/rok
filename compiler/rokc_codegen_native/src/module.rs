//! A MIR module -> one object, as the MLIR emitter's `emit_module`.

use rokc_core::{HashMap, HashSet};

use rokc_codegen_mlir::abi;
use rokc_codegen_mlir::mangle::{self, Pkgs};
use rokc_codegen_mlir::units::{self, Core};
use rokc_ir_common::{Abi, ConstVal, FnRole, FunctionId, Linkage, PrimTy};
use rokc_mir::{Link, MirModule, MirTy};

use crate::cc;
use crate::lir::{Func, SymId};
use crate::lower::{Ctx, FnLower, Sig, classify};
use crate::obj::{Obj, Sect};
use crate::{Options, wrap};

/// What one module compiled to.
pub struct Compiled {
    pub obj: Obj,
    pub gc_types: Vec<Vec<u32>>,
    pub errors: Vec<(rokc_diagnostic::Span, String)>,
}

/// One codegen unit of a plan (`rokc_codegen_mlir::units`).
pub struct UnitArgs<'a> {
    pub core: &'a Core,
    pub u: usize,
    pub glue: bool,
    /// The plan's GC types.
    pub tables: &'a [Vec<u32>],
}

pub fn compile(
    module: &MirModule,
    opts: &Options,
    sources: &rokc_diagnostic::SourceMap,
    unit: Option<&UnitArgs<'_>>,
    emit: &mut dyn FnMut(&mut Obj, &Func),
) -> Compiled {
    let t = &module.target;
    let conv = cc::Conv::of(t).expect("a supported target");
    let mut pkgs = Pkgs {
        link: module.link.clone(),
        ..Pkgs::default()
    };
    if let Some(a) = unit {
        a.core.unit_pkgs(&mut pkgs);
    }
    let glue = unit.is_none_or(|a| a.glue);
    let in_unit = |f: FunctionId| unit.is_none_or(|a| a.core.defines(a.u, f));
    let shared = |f: FunctionId| unit.is_some_and(|a| a.core.shared.contains(&f));
    let renamed = unit.map(|a| a.core.renamed.clone()).unwrap_or_default();
    let view: Vec<&rokc_mir::MirFunction> = unit.map_or_else(
        || module.functions.iter().collect(),
        |a| a.core.view(module, a.u),
    );
    pkgs.set_statics(&module.statics, t.os == "windows");
    if pkgs.link.packages.is_empty() {
        pkgs.link.packages = vec![(
            String::new(),
            mangle::pkg_hash(&rokc_mir::print_mir(module)),
        )];
    }
    if opts.repl.is_some() {
        for (p, h) in &mut pkgs.link.packages {
            if p.is_empty() {
                rokc_codegen_mlir::emit::REPL_PKG_HASH.clone_into(h);
            }
        }
    }
    let coff = t.os == "windows";
    let mut obj = Obj::new(t.arch == "x86_64", !coff && t.os != "macos", coff);
    let mut errors = Vec::new();
    let defines = |name: &str| {
        opts.repl
            .as_ref()
            .is_some_and(|r| r.define.iter().any(|d| d == name))
    };
    let prebuilt_static = |name: &str| {
        !glue
            || pkgs.link.in_prebuilt(name)
            || opts
                .repl
                .as_ref()
                .is_some_and(|r| r.statics.iter().any(|s| s == name))
    };

    // Signatures.
    let mut fns: HashMap<FunctionId, Sig> = HashMap::default();
    let mut c_entries = Vec::new();
    for &f in &view {
        let c = match f.facts.abi {
            // `system` is `stdcall` only on 32-bit x86, which has no
            // native backend.
            Abi::C { variadic, .. } => {
                classify(&mut errors, f.span, t, &f.param_tys, &f.ret_ty, variadic)
            }
            Abi::Rok if f.facts.linkage == Linkage::Extern => {
                abi::classify_rok(t, &f.param_tys, &f.ret_ty)
            }
            Abi::Rok => None,
        };
        let mut name = units::symbol(f, &pkgs, renamed.contains(&f.id));
        let mut sig_c = None;
        let mut c_entry = None;
        match c {
            Some(a) if f.facts.linkage == Linkage::Extern => sig_c = Some(a),
            Some(a) => {
                name = units::body_symbol(f, &pkgs, renamed.contains(&f.id));
                let entry = f
                    .facts
                    .export_name
                    .clone()
                    .unwrap_or_else(|| format!("{name}.c"));
                c_entry = Some(obj.sym(&entry));
                if in_unit(f.id) {
                    // Another unit may take its address.
                    let global = f.facts.export_name.is_some() || shared(f.id);
                    c_entries.push((f.id, a, global, f.span));
                }
            }
            None => {}
        }
        let sym = obj.sym(&name);
        fns.insert(
            f.id,
            Sig {
                sym,
                params: f.param_tys.clone(),
                ret: f.ret_ty.clone(),
                c: sig_c,
                c_entry,
                local: false,
                tls: f.facts.takes_tls(),
            },
        );
    }
    let private_copy = |f: &rokc_mir::MirFunction| match &pkgs.link.lib {
        Some(lib) => !Link::owns(lib, f.path()) && f.facts.role != FnRole::AnyDispatcher,
        None => pkgs.link.in_prebuilt(f.path()),
    };
    // A REPL cell compiles only what the session does not have.
    let compiled: HashSet<FunctionId> = module
        .functions
        .iter()
        .filter(|f| {
            f.facts.linkage == Linkage::Defined
                && in_unit(f.id)
                && (private_copy(f)
                    || opts
                        .fresh_fns
                        .as_ref()
                        .is_none_or(|s| s.contains(&mangle::symbol_for(f, &pkgs))))
        })
        .map(|f| f.id)
        .collect();
    // Defined here (a unit: in the program): reached directly, not
    // through the GOT.
    let defined: HashSet<SymId> = view
        .iter()
        .filter(|f| match unit {
            None => compiled.contains(&f.id),
            Some(_) => f.facts.linkage == Linkage::Defined,
        })
        .flat_map(|f| [Some(fns[&f.id].sym), fns[&f.id].c_entry])
        .flatten()
        .collect();
    for s in fns.values_mut() {
        s.local = defined.contains(&s.sym);
    }
    let (quiet, reentrant);
    if let Some(a) = unit {
        pkgs.lazy_statics.clone_from(&a.core.lazy_statics);
        quiet = std::sync::Arc::clone(&a.core.quiet);
        reentrant = std::sync::Arc::clone(&a.core.reentrant);
    } else {
        pkgs.lazy_statics = module
            .statics
            .iter()
            .filter_map(|s| {
                let init = s
                    .init_fn
                    .and_then(|i| fns.get(&i))
                    .map(|f| obj.syms[f.sym as usize].name.clone());
                // A prebuilt library's static is initialized by its `.a`.
                let init = init.or_else(|| pkgs.link.in_prebuilt(&s.name).then(String::new));
                Some((s.name.clone(), init?))
            })
            .collect();
        quiet = std::sync::Arc::new(rokc_codegen_mlir::emit::quiet_functions(module, &pkgs, t));
        reentrant = std::sync::Arc::new(rokc_codegen_mlir::emit::reentrant_functions(module));
    }
    // A unit's statics are the glue's, in the same executable.
    let local_statics: HashSet<String> = module
        .statics
        .iter()
        .filter(|s| {
            (!s.is_extern || defines(&s.name))
                && (unit.is_some() || !prebuilt_static(&s.name))
                && !pkgs.link.in_prebuilt(&s.name)
        })
        .map(|s| s.name.clone())
        .collect();

    let gc_type_base = rokc_mir::gc_abi::USER_TYPE_BASE + pkgs.link.type_offset;
    let mut gc_types: Vec<Vec<u32>> = match unit {
        Some(a) => a.tables.to_vec(),
        None => opts
            .repl
            .as_ref()
            .map(|r| r.gc_tables.clone())
            .unwrap_or_default(),
    };
    let pre_tables = if glue && unit.is_some() {
        0
    } else {
        gc_types.len()
    };
    let mut ctx = Ctx {
        sources,
        t,
        conv,
        obj: &mut obj,
        fns: &fns,
        pkgs: &pkgs,
        gc_types: &mut gc_types,
        gc_type_base,
        gc_pkg: unit.map(|a| a.core.placeholder.as_str()),
        overflow_checks: opts.overflow_checks,
        debug: opts.debug.then_some(&module.debug),
        local_statics: &local_statics,
        quiet: &quiet,
        reentrant: &reentrant,
        errors: Vec::new(),
        thunks: std::collections::BTreeSet::new(),
    };
    let mut funcs: Vec<(Func, bool, bool)> = Vec::new();

    for f in &module.functions {
        if !compiled.contains(&f.id) {
            continue;
        }
        let sym = fns[&f.id].sym;
        let func = FnLower::new(&mut ctx, f, sym).lower();
        let weak = pkgs.link.lib.is_some() && f.facts.role == FnRole::AnyDispatcher;
        let private = match &pkgs.link.lib {
            Some(lib) => !Link::owns(lib, f.path()) && f.facts.role != FnRole::AnyDispatcher,
            None => pkgs.link.in_prebuilt(f.path()),
        } && !shared(f.id);
        funcs.push((func, !private, weak));
    }
    // Thunks of the function values taken (lowering one takes none).
    for id in std::mem::take(&mut ctx.thunks) {
        let sig = &fns[&id];
        let name = format!("{}.v", ctx.obj.syms[sig.sym as usize].name);
        let sym = ctx.sym(&name);
        let mut f = thunk(&name, id, sig);
        // A debugger shows a function value as the function itself.
        if let Some(g) = module.functions.iter().find(|g| g.id == id) {
            f.name.clone_from(&g.name);
            f.span = g.span;
            let at = rokc_mir::Statement::Loc(g.span, rokc_mir::Scope::default());
            f.blocks[0].stmts.insert(0, at);
        }
        funcs.push((FnLower::new(&mut ctx, &f, sym).lower(), false, false));
    }
    for (id, a, exported, span) in &c_entries {
        let sig = &fns[id];
        let Some(entry) = sig.c_entry else { continue };
        let name = ctx.obj.syms[entry as usize].name.clone();
        let func = wrap::generated(&mut ctx, &name, entry, *span, |l| l.c_entry(sig, a));
        funcs.push((func, *exported, false));
    }
    if glue {
        glue_code(
            module,
            opts,
            unit.is_some(),
            &fns,
            &mut ctx,
            &mut funcs,
            &mut errors,
            pre_tables,
        );
    }
    errors.append(&mut ctx.errors);
    drop(ctx);

    for (f, global, weak) in &funcs {
        emit(&mut obj, f);
        let s = &mut obj.syms[f.sym as usize];
        s.global = *global;
        s.weak = *weak;
    }
    Compiled {
        obj,
        gc_types,
        errors,
    }
}

/// `<f>.v`, a function value of rok body `id`: takes `sig`'s parameters
/// and passes them on, with this thread's `rok_tls` address.
fn thunk(name: &str, id: FunctionId, sig: &Sig) -> rokc_mir::MirFunction {
    use rokc_ir_common::LocalId;
    use rokc_mir::{BasicBlock, LocalDecl, Operand, RValue, Statement, Terminator};
    let n = u32::try_from(sig.params.len()).expect("parameter count");
    let mut f = wrap::shell(name);
    f.params = (0..n).map(LocalId).collect();
    f.param_tys.clone_from(&sig.params);
    f.ret_ty = sig.ret.clone();
    f.locals = sig
        .params
        .iter()
        .chain([&sig.ret])
        .zip(0..)
        .map(|(ty, i)| LocalDecl {
            id: LocalId(i),
            name: None,
            ty: ty.clone(),
        })
        .collect();
    let ret = (sig.ret != MirTy::Unit).then_some(Operand::Copy(LocalId(n)));
    f.blocks = vec![BasicBlock {
        id: rokc_ir_common::BlockId(0),
        params: Vec::new(),
        stmts: vec![Statement::Assign {
            target: LocalId(n),
            rvalue: RValue::Call {
                callee: id,
                args: (0..n).map(|i| Operand::Copy(LocalId(i))).collect(),
                ret_ty: sig.ret.clone(),
            },
        }],
        terminator: Terminator::Return(ret),
    }];
    f
}

/// Once per program: the C `main`, statics, `rok.register_types` (GC types
/// from `pre_tables` on, static roots). A unit build's glue exports its
/// statics.
#[allow(clippy::too_many_arguments)]
fn glue_code(
    module: &MirModule,
    opts: &Options,
    units: bool,
    fns: &HashMap<FunctionId, Sig>,
    ctx: &mut Ctx<'_>,
    funcs: &mut Vec<(Func, bool, bool)>,
    errors: &mut Vec<(rokc_diagnostic::Span, String)>,
    pre_tables: usize,
) {
    let t = &module.target;
    let pkgs = ctx.pkgs;
    let defines = |name: &str| {
        opts.repl
            .as_ref()
            .is_some_and(|r| r.define.iter().any(|d| d == name))
    };
    let prebuilt_static = |name: &str| {
        pkgs.link.in_prebuilt(name)
            || opts
                .repl
                .as_ref()
                .is_some_and(|r| r.statics.iter().any(|s| s == name))
    };
    let gc_type_base = ctx.gc_type_base;
    // `rok.register_types` (the dependencies' first), called from `main`.
    let register_name = match &pkgs.link.lib {
        Some(lib) => format!("rok.register_types.{lib}"),
        None => "rok.register_types".to_owned(),
    };
    let register = ctx.sym(&register_name);
    if let Some(m) = module
        .functions
        .iter()
        .find(|f| f.facts.role == FnRole::Main)
    {
        let user = fns[&m.id].sym;
        if opts.repl.is_some() {
            let s = ctx.sym("rok_repl_cell");
            let func = wrap::generated(ctx, "rok_repl_cell", s, m.span, |l| {
                l.repl_cell(user, &m.ret_ty, register)
            });
            funcs.push((func, true, false));
        } else {
            let s = ctx.sym("main");
            let func = wrap::generated(ctx, "main", s, m.span, |l| {
                l.main_wrapper(user, &m.ret_ty, register)
            });
            funcs.push((func, true, false));
        }
    }
    // Statics.
    for s in &module.statics {
        // A prebuilt package's statics are in its `.a`; others defined
        // outside get a symbol only if used (DLL ones via the import
        // address).
        if pkgs.link.in_prebuilt(&s.name) || (s.is_extern && !defines(&s.name)) {
            continue;
        }
        let name = pkgs.static_sym(&s.name);
        let sym = ctx.sym(&name);
        let (size, align) = t.size_align(&s.ty);
        if s.is_extern && defines(&s.name) {
            ctx.obj
                .define_data(sym, Sect::Data, &vec![0; size as usize], align, true);
            continue;
        }
        let lazy = pkgs.lazy_statics.get(&s.name).cloned();
        if prebuilt_static(&s.name) {
            // An earlier REPL cell's `.init` stays private, made only if
            // this object reads the static.
            if let Some(init) = lazy.filter(|_| ctx.obj.named(&format!("{name}.init"))) {
                let state = ctx.sym(&format!("{name}.state"));
                let init = ctx.sym(&init);
                let f_sym = ctx.sym(&format!("{name}.init"));
                let iname = format!("{name}.init");
                let func = wrap::generated(ctx, &iname, f_sym, rokc_diagnostic::Span::DUMMY, |l| {
                    l.lazy_init(state, sym, true, init, &s.ty)
                });
                funcs.push((func, false, false));
            }
            continue;
        }
        let global = opts.repl.is_some()
            || units
            || pkgs
                .link
                .lib
                .as_deref()
                .is_some_and(|l| Link::owns(l, &s.name));
        if let Some(init) = lazy {
            ctx.obj
                .define_data(sym, Sect::Data, &vec![0; size as usize], align, global);
            let state = ctx.sym(&format!("{name}.state"));
            ctx.obj.define_data(state, Sect::Data, &[0], 1, global);
            let init = ctx.sym(&init);
            let f_sym = ctx.sym(&format!("{name}.init"));
            let iname = format!("{name}.init");
            let func = wrap::generated(ctx, &iname, f_sym, rokc_diagnostic::Span::DUMMY, |l| {
                l.lazy_init(state, sym, false, init, &s.ty)
            });
            // Exported by a prebuilt library that owns the static (and to
            // the other units).
            let export = units
                || pkgs
                    .link
                    .lib
                    .as_deref()
                    .is_some_and(|l| Link::owns(l, &s.name));
            funcs.push((func, export, false));
            continue;
        }
        if let ConstVal::Str(text) = &s.init {
            let lit = ctx.obj.bytes(text.as_bytes());
            let pw = t.pointer_width / 8;
            let mut bytes = vec![0u8; 2 * pw as usize];
            bytes[pw as usize..].copy_from_slice(&(text.len() as u64).to_le_bytes()[..pw as usize]);
            let sect = if s.is_mut {
                Sect::Data
            } else {
                Sect::DataConst
            };
            let off = ctx.obj.define_data(sym, sect, &bytes, pw, global);
            ctx.obj.abs64(sect, off, lit);
            continue;
        }
        match const_bytes(&s.init, &s.ty, t) {
            Some(bytes) => {
                let sect = if s.is_mut { Sect::Data } else { Sect::Const };
                ctx.obj.define_data(sym, sect, &bytes, align, global);
            }
            None => errors.push((
                rokc_diagnostic::Span::DUMMY,
                format!("unsupported in codegen: initializer of static `{}`", s.name),
            )),
        }
    }
    // GC support: the ScanTables and the static roots.
    let mut tables = Vec::new();
    for i in pre_tables..ctx.gc_types.len() {
        let words = ctx.gc_types[i].clone();
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let sym = ctx.sym(&rokc_codegen_mlir::units::scan_symbol(&words));
        ctx.obj.define_data(sym, Sect::Const, &bytes, 4, true);
        let s = &mut ctx.obj.syms[sym as usize];
        s.weak = true;
        s.hidden = true;
        let id = gc_type_base + i as u32;
        tables.push((id, sym));
        // A unit build's glue: the id units read (`Lower::gc_tid`).
        if let Some(pkg) = ctx.gc_pkg {
            let s = ctx.sym(&rokc_codegen_mlir::units::gc_symbol(pkg, &words));
            ctx.obj
                .define_data(s, Sect::Const, &id.to_le_bytes(), 4, true);
        }
    }
    let mut roots = Vec::new();
    for s in &module.statics {
        if !((!s.is_extern || defines(&s.name)) && !prebuilt_static(&s.name)) {
            continue;
        }
        let sym = ctx.sym(&pkgs.static_sym(&s.name));
        for off in t.root_offsets(&s.ty) {
            roots.push((sym, true, off));
        }
    }
    let deps: Vec<SymId> = pkgs
        .link
        .prebuilt
        .iter()
        .map(|d| ctx.sym(&format!("rok.register_types.{d}")))
        .collect();
    let func = wrap::generated(
        ctx,
        &register_name,
        register,
        rokc_diagnostic::Span::DUMMY,
        |l| l.register_types(&tables, &roots, &deps),
    );
    funcs.push((func, pkgs.link.lib.is_some(), false));
}

/// A scalar static's initial bytes (the emitter's `format_const_attr`).
fn const_bytes(v: &ConstVal, ty: &MirTy, t: &rokc_mir::Target) -> Option<Vec<u8>> {
    if let (ConstVal::Bytes(b), MirTy::Array(..)) = (v, ty) {
        return Some(b.clone());
    }
    let MirTy::Prim(p) = ty else { return None };
    let size = t.size_align(ty).0 as usize;
    let int = |x: i128| Some(x.to_le_bytes()[..size].to_vec());
    match v {
        ConstVal::Int { value, .. } if p.is_int() => int(*value),
        ConstVal::UInt { value, .. } if p.is_int() => int(*value as i128),
        ConstVal::Char(c) if p.is_int() => int(i128::from(*c)),
        ConstVal::Bool(b) if *p == PrimTy::Bool => Some(vec![u8::from(*b)]),
        ConstVal::Float { bits, ty: fp } if p.is_float() => {
            let v = if *fp == PrimTy::F32 {
                f64::from(f32::from_bits(*bits as u32))
            } else {
                f64::from_bits(*bits)
            };
            Some(if *p == PrimTy::F32 {
                (v as f32).to_bits().to_le_bytes().to_vec()
            } else {
                v.to_bits().to_le_bytes().to_vec()
            })
        }
        ConstVal::Int { value, .. } if p.is_float() => {
            let v = *value as f64;
            Some(if *p == PrimTy::F32 {
                (v as f32).to_bits().to_le_bytes().to_vec()
            } else {
                v.to_bits().to_le_bytes().to_vec()
            })
        }
        _ => None,
    }
}
