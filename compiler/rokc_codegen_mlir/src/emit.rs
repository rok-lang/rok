//! Standard-MLIR text emission. SSA value numbering is local to each
//! `func.func`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;

use rokc_diagnostic::{SourceMap, Span};
use rokc_ir_common::{Abi, ConstVal, FnRole, FunctionId, Inline, Linkage, LocalId, PrimOp, PrimTy};
use rokc_mir::gc_abi::{
    CARD_SHIFT, FLAG_CARDED, FLAG_UNLOGGED, GC_FLAGS_OFFSET, GC_WORD_OFFSET, HEADER_SIZE,
    LOS_THRESHOLD, MIN_OBJECT_SIZE, PLAIN_TYPE_ID, SUM_DESC_SHIFT, SUM_PAYLOAD, TABLE_ARRAY,
    USER_TYPE_BASE,
};
use rokc_mir::tls_abi::{self, Slot};
use rokc_mir::{
    BasicBlock, CAbi, GcSlot, Link, MirFunction, MirModule, MirTy, Operand, RValue, Statement,
    Target, Terminator,
};

use crate::abi::{ArgAbi, FnAbi, RetAbi, rok_by_ref};

/// The standard library's `List` (`buf_ref_facts`).
const LIST: &str = "rok::core::collections::list::List";

/// TBAA tags. A local's stack slot is never GC object memory, and a GC
/// buffer is only ever accessed as its element type, so differently typed
/// heap accesses never overlap. Memory the collector reads or rewrites is
/// tagged as references (`ref`, also on `REF_ONLY_CALLS`): roots
/// (`gcroot`), heap references (`gc`), heap `str` pointers and write
/// barrier cards (`card`), each its own type. Raw addresses, scratch and
/// runtime words carry no tag and may alias anything.
const TBAA_DEFS: &str = r#"#rok_tbaa_root = #llvm.tbaa_root<id = "rok">
#rok_tbaa_slot_t = #llvm.tbaa_type_desc<id = "rok.slot", members = {<#rok_tbaa_root, 0>}>
#rok_tbaa_heap_t = #llvm.tbaa_type_desc<id = "rok.heap", members = {<#rok_tbaa_root, 0>}>
#rok_tbaa_slot = #llvm.tbaa_tag<base_type = #rok_tbaa_slot_t, access_type = #rok_tbaa_slot_t, offset = 0>
#rok_tbaa_heap = #llvm.tbaa_tag<base_type = #rok_tbaa_heap_t, access_type = #rok_tbaa_heap_t, offset = 0>
#rok_tbaa_i8_t = #llvm.tbaa_type_desc<id = "rok.heap.i8", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_i8 = #llvm.tbaa_tag<base_type = #rok_tbaa_i8_t, access_type = #rok_tbaa_i8_t, offset = 0>
#rok_tbaa_i16_t = #llvm.tbaa_type_desc<id = "rok.heap.i16", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_i16 = #llvm.tbaa_tag<base_type = #rok_tbaa_i16_t, access_type = #rok_tbaa_i16_t, offset = 0>
#rok_tbaa_i32_t = #llvm.tbaa_type_desc<id = "rok.heap.i32", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_i32 = #llvm.tbaa_tag<base_type = #rok_tbaa_i32_t, access_type = #rok_tbaa_i32_t, offset = 0>
#rok_tbaa_i64_t = #llvm.tbaa_type_desc<id = "rok.heap.i64", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_i64 = #llvm.tbaa_tag<base_type = #rok_tbaa_i64_t, access_type = #rok_tbaa_i64_t, offset = 0>
#rok_tbaa_isize_t = #llvm.tbaa_type_desc<id = "rok.heap.isize", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_isize = #llvm.tbaa_tag<base_type = #rok_tbaa_isize_t, access_type = #rok_tbaa_isize_t, offset = 0>
#rok_tbaa_u8_t = #llvm.tbaa_type_desc<id = "rok.heap.u8", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_u8 = #llvm.tbaa_tag<base_type = #rok_tbaa_u8_t, access_type = #rok_tbaa_u8_t, offset = 0>
#rok_tbaa_u16_t = #llvm.tbaa_type_desc<id = "rok.heap.u16", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_u16 = #llvm.tbaa_tag<base_type = #rok_tbaa_u16_t, access_type = #rok_tbaa_u16_t, offset = 0>
#rok_tbaa_u32_t = #llvm.tbaa_type_desc<id = "rok.heap.u32", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_u32 = #llvm.tbaa_tag<base_type = #rok_tbaa_u32_t, access_type = #rok_tbaa_u32_t, offset = 0>
#rok_tbaa_u64_t = #llvm.tbaa_type_desc<id = "rok.heap.u64", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_u64 = #llvm.tbaa_tag<base_type = #rok_tbaa_u64_t, access_type = #rok_tbaa_u64_t, offset = 0>
#rok_tbaa_usize_t = #llvm.tbaa_type_desc<id = "rok.heap.usize", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_usize = #llvm.tbaa_tag<base_type = #rok_tbaa_usize_t, access_type = #rok_tbaa_usize_t, offset = 0>
#rok_tbaa_f32_t = #llvm.tbaa_type_desc<id = "rok.heap.f32", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_f32 = #llvm.tbaa_tag<base_type = #rok_tbaa_f32_t, access_type = #rok_tbaa_f32_t, offset = 0>
#rok_tbaa_f64_t = #llvm.tbaa_type_desc<id = "rok.heap.f64", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_f64 = #llvm.tbaa_tag<base_type = #rok_tbaa_f64_t, access_type = #rok_tbaa_f64_t, offset = 0>
#rok_tbaa_bool_t = #llvm.tbaa_type_desc<id = "rok.heap.bool", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_bool = #llvm.tbaa_tag<base_type = #rok_tbaa_bool_t, access_type = #rok_tbaa_bool_t, offset = 0>
#rok_tbaa_char_t = #llvm.tbaa_type_desc<id = "rok.heap.char", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_char = #llvm.tbaa_tag<base_type = #rok_tbaa_char_t, access_type = #rok_tbaa_char_t, offset = 0>
#rok_tbaa_cchar_t = #llvm.tbaa_type_desc<id = "rok.heap.cchar", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_cchar = #llvm.tbaa_tag<base_type = #rok_tbaa_cchar_t, access_type = #rok_tbaa_cchar_t, offset = 0>
#rok_tbaa_ref_t = #llvm.tbaa_type_desc<id = "rok.heap.ref", members = {<#rok_tbaa_heap_t, 0>}>
#rok_tbaa_ref = #llvm.tbaa_tag<base_type = #rok_tbaa_ref_t, access_type = #rok_tbaa_ref_t, offset = 0>
#rok_tbaa_gcroot_t = #llvm.tbaa_type_desc<id = "rok.gcroot", members = {<#rok_tbaa_ref_t, 0>}>
#rok_tbaa_gcroot = #llvm.tbaa_tag<base_type = #rok_tbaa_gcroot_t, access_type = #rok_tbaa_gcroot_t, offset = 0>
#rok_tbaa_gc_t = #llvm.tbaa_type_desc<id = "rok.heap.gc", members = {<#rok_tbaa_ref_t, 0>}>
#rok_tbaa_gc = #llvm.tbaa_tag<base_type = #rok_tbaa_gc_t, access_type = #rok_tbaa_gc_t, offset = 0>
#rok_tbaa_str_t = #llvm.tbaa_type_desc<id = "rok.heap.str", members = {<#rok_tbaa_ref_t, 0>}>
#rok_tbaa_str = #llvm.tbaa_tag<base_type = #rok_tbaa_str_t, access_type = #rok_tbaa_str_t, offset = 0>
#rok_tbaa_card_t = #llvm.tbaa_type_desc<id = "rok.card", members = {<#rok_tbaa_ref_t, 0>}>
#rok_tbaa_card = #llvm.tbaa_tag<base_type = #rok_tbaa_card_t, access_type = #rok_tbaa_card_t, offset = 0>
"#;

/// The TBAA tag (`TBAA_DEFS`) of a GC buffer element of type `ty`.
const fn heap_tbaa(ty: &MirTy) -> &'static str {
    match ty {
        MirTy::Prim(p) => match p {
            PrimTy::I8 => "#rok_tbaa_i8",
            PrimTy::I16 => "#rok_tbaa_i16",
            PrimTy::I32 => "#rok_tbaa_i32",
            PrimTy::I64 => "#rok_tbaa_i64",
            PrimTy::Isize => "#rok_tbaa_isize",
            PrimTy::U8 => "#rok_tbaa_u8",
            PrimTy::U16 => "#rok_tbaa_u16",
            PrimTy::U32 => "#rok_tbaa_u32",
            PrimTy::U64 => "#rok_tbaa_u64",
            PrimTy::Usize => "#rok_tbaa_usize",
            PrimTy::F32 => "#rok_tbaa_f32",
            PrimTy::F64 => "#rok_tbaa_f64",
            PrimTy::Bool => "#rok_tbaa_bool",
            PrimTy::Char => "#rok_tbaa_char",
            PrimTy::CChar => "#rok_tbaa_cchar",
            PrimTy::Str => "#rok_tbaa_str",
        },
        MirTy::GcRef => "#rok_tbaa_gc",
        _ => "#rok_tbaa_heap",
    }
}

/// The REPL session's package hash (`"repl"`).
pub const REPL_PKG_HASH: &str = "7265706c";

pub struct Emitter {
    out: String,
    target: Target,
    /// Interned string-literal globals: content → symbol, shared by every
    /// function of the module.
    str_globals: std::collections::BTreeMap<Vec<u8>, String>,
    /// Unsupported constructs found while emitting, by function: the module
    /// is rejected rather than shipping placeholder IR.
    pub errors: Vec<(rokc_diagnostic::Span, String)>,
    /// Runtime symbols codegen uses on its own (`runtime_decl`).
    needs: BTreeSet<&'static str>,
    /// `ScanTable` (802) of each GC object type: index `i` is type id
    /// `gc_type_base() + i`. A REPL cell starts with the earlier cells'.
    pub gc_types: Vec<Vec<u32>>,
    pkgs: crate::mangle::Pkgs,
    /// Panic on integer `+ - *` / negation overflow; wrap otherwise.
    pub overflow_checks: bool,
    /// `-g`: DWARF state.
    pub debug: Option<crate::debug::DebugInfo>,
    /// Build a REPL cell instead of a program.
    pub repl: Option<crate::ReplCell>,
    /// LLVM `target datalayout`, when the toolchain told.
    pub data_layout: Option<String>,
    /// `quiet_functions` of the module.
    quiet: std::sync::Arc<HashSet<FunctionId>>,
    /// The `List` instances (`buf_ref_facts`).
    lists: std::sync::Arc<HashSet<u32>>,
    /// `reentrant_functions` of the module.
    reentrant: std::sync::Arc<HashSet<FunctionId>>,
    /// `entered_functions` of the module.
    entered: std::sync::Arc<HashSet<FunctionId>>,
    /// Functions whose function-value thunk this module defines.
    thunks: BTreeSet<FunctionId>,
    /// What the `List` alias scopes rely on.
    list_alias: std::sync::Arc<crate::alias::ListAlias>,
    /// Emit codegen unit `.1` of a plan (`crate::units`), not the module.
    pub(crate) unit: Option<(std::sync::Arc<crate::units::Core>, usize)>,
    /// The unit holds the program's glue (statics, `main`, types).
    pub(crate) glue: bool,
    /// The GC type id symbols the unit's code reads (`units::gc_symbol`).
    gc_syms: BTreeSet<String>,
}

/// How a call reaches a module function.
pub(crate) struct FnSig {
    /// The symbol called: a rok body, or a declaration.
    name: String,
    params: Vec<MirTy>,
    ret: MirTy,
    /// A C declaration: its C lowering.
    c: Option<FnAbi>,
    /// An `extern "C" fn` with aggregates: symbol and
    /// type of the C entry (`emit_c_entry`); `name` is the rok-ABI body.
    c_entry: Option<(String, String)>,
    /// Its C declaration or C entry is `stdcall` (`FnAbi::stdcall`).
    stdcall: bool,
    /// A runtime declaration (`extern "rok"`).
    runtime: bool,
    /// The rok body takes the caller's `rok_tls` block address last
    /// (`FnFacts::takes_tls`).
    tls: bool,
}

impl FnSig {
    /// The rok body's parameter and result types (`rok_fn_types`), the
    /// hidden `rok_tls` address last.
    fn rok_types(&self, target: &Target) -> (Vec<String>, Option<String>) {
        let (mut params, ret) = rok_fn_types(&self.params, &self.ret, target);
        if self.tls {
            params.push("!llvm.ptr".to_owned());
        }
        (params, ret)
    }
}

/// This thread's `rok_tls` block address as `%tls`, in a function no
/// rok caller hands it to.
const TLS_BASE: &str = "    %tlsg = llvm.mlir.addressof @rok_tls : !llvm.ptr
    %tls = \"llvm.intr.threadlocal.address\"(%tlsg) : (!llvm.ptr) -> !llvm.ptr
";

/// What a C call goes through.
#[derive(Clone, Copy)]
enum Callee<'s> {
    Symbol(&'s str),
    /// An `extern "C" fn` value (`!llvm.ptr`).
    Ptr(&'s str),
}

/// Declaration of runtime symbol `name` (805) that codegen calls itself;
/// `None` if not in the table. `str` is passed by pointer on Windows x64
/// and wasm32 (`FnEmit::runtime_str`).
fn runtime_decl(name: &str, target: &Target) -> Option<String> {
    let pw = target.pointer_width;
    if name == "rok.alloc" {
        return Some(alloc_fn(pw));
    }
    let s = if target.c_abi.pair_by_ref() {
        "!llvm.ptr".to_owned()
    } else {
        mlir_prim(PrimTy::Str, pw)
    };
    let global = |ty: &str| {
        format!("llvm.mlir.global external thread_local @{name}() {{addr_space = 0 : i32}} : {ty}")
    };
    let sig = match name {
        "rok_tls" => {
            let n = rokc_mir::tls_abi::SIZE.offset(pw / 8);
            return Some(global(&format!("!llvm.array<{n} x i8>")));
        }
        "rok_panic_at" => format!("({s}, {s})"),
        "rok_cstr_len" => format!("(!llvm.ptr) -> i{pw}"),
        "rok_str_cstr" => format!("({s}) -> !llvm.ptr"),
        "rok_str_cstr_done" => format!("({s}, !llvm.ptr)"),
        "rok_str_cmp" => format!("({s}, {s}) -> i32"),
        "rok_gc_alloc_slow" => format!("(i{pw}, i{pw}, i32) -> !llvm.ptr"),
        "rok_static_init_begin" => format!("({}) -> i32", codegen_params(name, pw)?.join(", ")),
        "rok_gc_register_type" | "rok_gc_wb_slow" | "rok_gc_add_root" | "rok_static_init_end" => {
            format!("({})", codegen_params(name, pw)?.join(", "))
        }
        "rok_runtime_yield"
        | "rok_nosp_enter"
        | "rok_nosp_leave"
        | "rok_init_once"
        | "rok_runtime_init"
        | "rok_runtime_shutdown"
        | "rok_enter_native"
        | "rok_leave_native" => "()".to_owned(),
        "rok_native_reenter" => "() -> i32".to_owned(),
        "rok_native_resume" => "(i32)".to_owned(),
        _ => return None,
    };
    if REF_ONLY_CALLS.contains(&name) {
        return Some(format!("llvm.func @{name}{sig}{}", runtime_fn_attrs(name)));
    }
    Some(format!(
        "func.func private @{name}{sig}{}",
        runtime_fn_attrs(name)
    ))
}

/// Parameter types codegen uses for runtime symbols whose declared
/// types (`Ptr<u8>`, `ConstPtr<u8>`) differ. A program's own declaration of one
/// is declared and called with these too.
fn codegen_params(name: &str, pw: u32) -> Option<Vec<String>> {
    let p = || "!llvm.ptr".to_owned();
    Some(match name {
        "rok_gc_register_type" => vec!["i32".to_owned(), p()],
        "rok_gc_wb_slow" | "rok_gc_add_root" | "rok_static_init_end" => vec![p()],
        "rok_static_init_begin" => vec![p(), p(), format!("i{pw}")],
        _ => return None,
    })
}

/// Runtime functions that write only references (a collection moves
/// objects and rewrites them) and headers. Called with the `ref` TBAA tag
/// so other heap loads stay valid across them; declared `llvm.func`,
/// since only `llvm.call` carries the tag.
const REF_ONLY_CALLS: [&str; 3] = ["rok_runtime_yield", "rok_gc_alloc_slow", "rok_gc_wb_slow"];

/// `rok.alloc(cursor, limit, size8, size, align, type_id, ok)`: bump-
/// allocate `size8` bytes from this thread's TLAB when they fit and `ok`,
/// writing the header; else `rok_gc_alloc_slow`. The optimizer sees an
/// allocation function and inlines it last (the shim's `InlineAlloc`).
fn alloc_fn(pw: u32) -> String {
    format!(
        "llvm.func internal @rok.alloc(%cp: !llvm.ptr, %lp: !llvm.ptr, %size8: i{pw}, %size: i{pw}, %align: i{pw}, %tid: i32, %ok: i1) -> !llvm.ptr attributes {{passthrough = [\"noinline\", \"nounwind\"]}} {{
    %cur = llvm.load %cp : !llvm.ptr -> i{pw}
    %lim = llvm.load %lp : !llvm.ptr -> i{pw}
    %room = llvm.sub %lim, %cur : i{pw}
    %room_ok = llvm.icmp \"ule\" %size8, %room : i{pw}
    %fits = llvm.and %room_ok, %ok : i1
    llvm.cond_br %fits, ^fast, ^slow
  ^fast:
    %next = llvm.add %cur, %size8 : i{pw}
    llvm.store %next, %cp : i{pw}, !llvm.ptr
    %hp = llvm.inttoptr %cur : i{pw} to !llvm.ptr
    llvm.store %tid, %hp : i32, !llvm.ptr
    %wp = llvm.getelementptr %hp[{GC_WORD_OFFSET}] : (!llvm.ptr) -> !llvm.ptr, i8
    llvm.store %size8, %wp : i{pw}, !llvm.ptr
    llvm.return %hp : !llvm.ptr
  ^slow:
    %obj = llvm.call @rok_gc_alloc_slow(%size, %align, %tid) {{tbaa = [#rok_tbaa_ref]}} : (i{pw}, i{pw}, i32) -> !llvm.ptr
    llvm.return %obj : !llvm.ptr
  }}"
    )
}

/// A module of `rok.alloc` (and what it calls) alone.
pub fn alloc_module(target: &Target) -> String {
    let decl = runtime_decl("rok_gc_alloc_slow", target).unwrap_or_default();
    format!(
        "{TBAA_DEFS}module {{\n  {}\n  {decl}\n}}\n",
        alloc_fn(target.pointer_width)
    )
}

/// LLVM attributes of a runtime declaration: nothing unwinds (a Rust panic
/// there aborts); process-ending ones never return.
fn runtime_fn_attrs(name: &str) -> &'static str {
    match name {
        "rok_panic"
        | "rok_panic_at"
        | "rok_panic_str"
        | "rok_oom"
        | "rok_exit"
        | "rok_uncaught_error"
        | "rok_uncaught_error_at" => {
            r#" attributes {passthrough = ["nounwind", "noreturn", "cold"]}"#
        }
        _ => r#" attributes {passthrough = ["nounwind"]}"#,
    }
}

impl Emitter {
    #[must_use]
    pub fn new(target: Target) -> Self {
        Self {
            out: String::new(),
            target,
            str_globals: std::collections::BTreeMap::new(),
            errors: Vec::new(),
            needs: BTreeSet::new(),
            gc_types: Vec::new(),
            pkgs: crate::mangle::Pkgs::default(),
            overflow_checks: true,
            debug: None,
            repl: None,
            data_layout: None,
            quiet: std::sync::Arc::default(),
            lists: std::sync::Arc::default(),
            reentrant: std::sync::Arc::default(),
            entered: std::sync::Arc::default(),
            thunks: BTreeSet::new(),
            list_alias: std::sync::Arc::default(),
            unit: None,
            glue: true,
            gc_syms: BTreeSet::new(),
        }
    }

    fn pw(&self) -> u32 {
        self.target.pointer_width
    }

    const fn link(&self) -> &Link {
        &self.pkgs.link
    }

    /// First GC type id of this module.
    const fn gc_type_base(&self) -> u32 {
        USER_TYPE_BASE + self.pkgs.link.type_offset
    }

    /// An `extern` static this REPL cell defines (`ReplCell::define`).
    fn defines(&self, name: &str) -> bool {
        self.repl
            .as_ref()
            .is_some_and(|r| r.define.iter().any(|d| d == name))
    }

    /// A definition private to this object: in a prebuilt library
    /// everything but the package's own items; with prebuilt deps, the
    /// copies of their code compiled here; in a program, everything not
    /// exported by name (REPL cells stay visible; the program's dispatcher
    /// overrides a library's weak one). Not a function another unit uses.
    fn private_copy(&self, f: &MirFunction) -> bool {
        if self
            .unit
            .as_ref()
            .is_some_and(|(c, _)| c.shared.contains(&f.id))
        {
            return false;
        }
        match &self.link().lib {
            Some(lib) => !Link::owns(lib, f.path()) && f.facts.role != FnRole::AnyDispatcher,
            None => {
                self.link().in_prebuilt(f.path())
                    || (self.repl.is_none()
                        && f.facts.export_name.is_none()
                        && f.facts.role != FnRole::AnyDispatcher)
            }
        }
    }

    /// A static whose storage is defined elsewhere: a prebuilt dependency's
    /// `.a`, an earlier REPL cell, or the glue unit.
    fn prebuilt_static(&self, name: &str) -> bool {
        !self.glue
            || self.link().in_prebuilt(name)
            || self
                .repl
                .as_ref()
                .is_some_and(|r| r.statics.iter().any(|s| s == name))
    }

    /// Linkage of a defined static: exported from a prebuilt library (its
    /// package's), a REPL cell or the glue unit, since others use them.
    fn static_linkage(&self, name: &str) -> &'static str {
        if self.repl.is_some()
            || self.unit.is_some()
            || self
                .link()
                .lib
                .as_deref()
                .is_some_and(|l| Link::owns(l, name))
        {
            "external"
        } else {
            "internal"
        }
    }

    /// The C lowering of a signature; `None` (and an error) where this
    /// target cannot pass its aggregates.
    fn classify(
        errors: &mut Vec<String>,
        target: &Target,
        what: &str,
        params: &[MirTy],
        ret: &MirTy,
        variadic: bool,
        system: bool,
    ) -> Option<FnAbi> {
        let mut abi = crate::abi::classify(target, params, ret, variadic);
        if let Some(abi) = abi.as_mut() {
            abi.stdcall = system && target.stdcall_system();
        }
        // A `#[repr(transparent)]` struct around a scalar crosses as the
        // scalar: through memory, like any coerced aggregate.
        let coerce = |t: &MirTy| {
            let inner = crate::abi::transparent(t, target);
            (inner != *t && !crate::abi::is_aggregate(&inner) && inner != MirTy::Unit)
                .then(|| mlir_type(&inner, target))
        };
        if let Some(abi) = abi.as_mut() {
            for (a, p) in abi.args.iter_mut().zip(params) {
                if let (ArgAbi::Direct, Some(c)) = (&a, coerce(p)) {
                    *a = ArgAbi::Coerce(c);
                }
            }
            if let (RetAbi::Direct, Some(c)) = (&abi.ret, coerce(ret)) {
                abi.ret = RetAbi::Coerce(c);
            }
        }
        if abi.is_none() {
            errors.push(format!(
                "unsupported in codegen: {what} passes a `struct` by value across the C ABI, \
                 which is not implemented for {}",
                target.name
            ));
        }
        abi
    }

    pub fn emit_module(&mut self, module: &MirModule, sources: &SourceMap) {
        self.pkgs.link = module.link.clone();
        if let Some(di) = self.debug.as_mut() {
            di.struct_names.clone_from(&module.struct_names);
            di.debug.clone_from(&module.debug);
        }
        let windows = self.target.os == "windows";
        self.pkgs.set_statics(&module.statics, windows);
        // Content-based package hashes come from the driver;
        // a bare MIR module (tests) hashes its printed MIR.
        if self.pkgs.link.packages.is_empty() {
            self.pkgs.link.packages = vec![(
                String::new(),
                crate::mangle::pkg_hash(&rokc_mir::print_mir(module)),
            )];
        }
        if self.repl.is_some() {
            // The session's package is one program: its symbols (statics
            // that later cells share) do not change with each cell's text.
            for (p, h) in &mut self.pkgs.link.packages {
                if p.is_empty() {
                    REPL_PKG_HASH.clone_into(h);
                }
            }
        }

        let unit = self.unit.clone();
        if let Some((core, _)) = &unit {
            core.unit_pkgs(&mut self.pkgs);
        }
        let view: Vec<&MirFunction> = unit.as_ref().map_or_else(
            || module.functions.iter().collect(),
            |(core, u)| core.view(module, *u),
        );
        let renamed = unit
            .as_ref()
            .map(|(c, _)| c.renamed.clone())
            .unwrap_or_default();
        let mut fns: HashMap<FunctionId, FnSig> = HashMap::new();
        let mut c_entries: Vec<(FunctionId, FnAbi)> = Vec::new();
        for &f in &view {
            let own = codegen_params(&f.name, self.target.pointer_width)
                .filter(|_| f.facts.linkage == Linkage::Extern)
                .map(|ps| FnAbi {
                    ret: RetAbi::Direct,
                    args: ps
                        .into_iter()
                        .zip(&f.param_tys)
                        .map(|(c, p)| {
                            if c_type(p, &self.target) == c {
                                ArgAbi::Direct
                            } else {
                                ArgAbi::Coerce(c)
                            }
                        })
                        .collect(),
                    variadic: false,
                    rok: f.facts.abi == Abi::Rok,
                    stdcall: false,
                });
            let c = match f.facts.abi {
                _ if own.is_some() => own,
                Abi::C { variadic, system } => {
                    let mut errs = Vec::new();
                    let abi = Self::classify(
                        &mut errs,
                        &self.target,
                        &format!("`{}`", f.name),
                        &f.param_tys,
                        &f.ret_ty,
                        variadic,
                        system,
                    );
                    self.errors.extend(errs.into_iter().map(|e| (f.span, e)));
                    abi
                }
                Abi::Rok if f.facts.linkage == Linkage::Extern => {
                    crate::abi::classify_rok(&self.target, &f.param_tys, &f.ret_ty).or_else(|| {
                        // Declared and called as codegen's own calls of it are.
                        REF_ONLY_CALLS.contains(&f.name.as_str()).then(|| {
                            let abi =
                                crate::abi::classify(&self.target, &f.param_tys, &f.ret_ty, false);
                            abi.map(|a| FnAbi { rok: true, ..a })
                        })?
                    })
                }
                Abi::Rok => None,
            };
            let mut sig = FnSig {
                name: crate::units::symbol(f, &self.pkgs, renamed.contains(&f.id)),
                params: f.param_tys.clone(),
                ret: f.ret_ty.clone(),
                c: None,
                c_entry: None,
                stdcall: c.as_ref().is_some_and(|a| a.stdcall),
                runtime: f.facts.linkage == Linkage::Extern && f.facts.abi == Abi::Rok,
                tls: f.facts.takes_tls(),
            };
            match c {
                Some(abi) if f.facts.linkage == Linkage::Extern => sig.c = Some(abi),
                Some(abi) => {
                    // The exported name (or `<body>.c`) is the C entry:
                    // C's calls come back into rok through it.
                    sig.name = crate::units::body_symbol(f, &self.pkgs, renamed.contains(&f.id));
                    // A WIT export's name (`ns:pkg/iface#f`) is quoted.
                    let entry = f.facts.export_name.as_ref().map_or_else(
                        || format!("{}.c", sig.name),
                        |n| {
                            if n.chars()
                                .all(|c| c.is_ascii_alphanumeric() || "_$.".contains(c))
                            {
                                n.clone()
                            } else {
                                format!("{n:?}")
                            }
                        },
                    );
                    let (params, ret) = c_signature(&sig.params, &sig.ret, &abi, &self.target);
                    let params: Vec<String> = params.into_iter().map(|(t, _)| t).collect();
                    let ty = format!(
                        "({}) -> {}",
                        params.join(", "),
                        ret.unwrap_or_else(|| "()".to_owned())
                    );
                    sig.c_entry = Some((entry, ty));
                    if unit.as_ref().is_none_or(|(c, u)| c.defines(*u, f.id)) {
                        c_entries.push((f.id, abi));
                    }
                }
                _ => {}
            }
            fns.insert(f.id, sig);
        }

        self.lists = std::sync::Arc::new(
            module
                .struct_names
                .iter()
                .filter(|(_, (n, _))| n.split("__").next() == Some(LIST))
                .map(|(id, _)| *id)
                .collect(),
        );
        if let Some((core, _)) = &unit {
            // The plan's whole-program analyses.
            self.pkgs.lazy_statics.clone_from(&core.lazy_statics);
            self.quiet = core.quiet.clone();
            self.reentrant = core.reentrant.clone();
            self.entered = core.entered.clone();
        } else {
            // A prebuilt library's static is initialized by its `.a`: the
            // initializer need not be here.
            self.pkgs.lazy_statics = module
                .statics
                .iter()
                .filter_map(|s| {
                    let init = fns.get(&s.init_fn?).map(|f| f.name.clone());
                    let init =
                        init.or_else(|| self.pkgs.link.in_prebuilt(&s.name).then(String::new));
                    Some((s.name.clone(), init?))
                })
                .collect();
            self.quiet = std::sync::Arc::new(quiet_functions(module, &self.pkgs, &self.target));
            self.reentrant = std::sync::Arc::new(reentrant_functions(module));
            self.entered = std::sync::Arc::new(entered_functions(module));
        }
        self.list_alias = std::sync::Arc::new(crate::alias::ListAlias::new(
            module,
            &self.lists,
            &self.quiet,
            &self.target,
        ));

        self.out.push_str("module");
        let mut attrs = Vec::new();
        attrs.push(format!(
            "llvm.target_triple = \"{}\"",
            self.target.llvm_triple()
        ));
        if let Some(dl) = &self.data_layout {
            attrs.push(format!("llvm.data_layout = \"{dl}\""));
        }
        if !attrs.is_empty() {
            let _ = write!(self.out, " attributes {{{}}}", attrs.join(", "));
        }
        self.out.push_str(" {\n");
        self.emit_functions(module, sources, &view, &fns, &c_entries);
        self.emit_thunks(module, &fns);
        self.emit_rest(module, &view, &fns);
    }

    /// `<f>.v` for each rok body used as a function value: same parameters
    /// and result, reading `rok_tls` and passing it on; internal to each
    /// module.
    fn emit_thunks(&mut self, module: &MirModule, fns: &HashMap<FunctionId, FnSig>) {
        if !self.thunks.is_empty() {
            self.needs.insert("rok_tls");
        }
        for id in std::mem::take(&mut self.thunks) {
            let sig = &fns[&id];
            let (params, ret) = rok_fn_types(&sig.params, &sig.ret, &self.target);
            let head: Vec<String> = params
                .iter()
                .enumerate()
                .map(|(i, t)| format!("%arg{i}: {t}"))
                .collect();
            let mut args: Vec<String> = (0..params.len()).map(|i| format!("%arg{i}")).collect();
            args.push("%tls".to_owned());
            let (args, mut tys) = (args.join(", "), params);
            tys.push("!llvm.ptr".to_owned());
            let tys = tys.join(", ");
            let name = &sig.name;
            let (call, back) = match &ret {
                Some(r) => (
                    format!("%r = func.call @{name}({args}) : ({tys}) -> {r}"),
                    format!("func.return %r : {r}"),
                ),
                None => (
                    format!("func.call @{name}({args}) : ({tys}) -> ()"),
                    "func.return".to_owned(),
                ),
            };
            let arrow = ret
                .as_ref()
                .map_or_else(String::new, |r| format!(" -> {r}"));
            let text = format!(
                "  func.func private @{name}.v({}){arrow} attributes {{llvm.linkage = #llvm.linkage<internal>, passthrough = [\"nounwind\"]}} {{\n{TLS_BASE}    {call}\n    {back}\n  }}\n",
                head.join(", ")
            );
            // A debugger shows a function value as the function itself.
            let text = match module.functions.iter().find(|f| f.id == id) {
                Some(f) => self.described(text, &f.name, f.span),
                None => text,
            };
            self.out.push_str(&text);
        }
    }

    /// The functions of `view` (a unit's other functions declared) and
    /// the C entries.
    fn emit_functions(
        &mut self,
        module: &MirModule,
        sources: &SourceMap,
        view: &[&MirFunction],
        fns: &HashMap<FunctionId, FnSig>,
        c_entries: &[(FunctionId, FnAbi)],
    ) {
        let unit = self.unit.clone();
        // An `extern "C"` declaration of a symbol defined here
        // (`#[export_name]`) is the same function.
        let defined: HashSet<&str> = view
            .iter()
            .filter(|f| f.facts.linkage == Linkage::Defined)
            .flat_map(|f| {
                let sig = &fns[&f.id];
                [
                    Some(sig.name.as_str()),
                    sig.c_entry.as_ref().map(|(e, _)| e.as_str()),
                ]
            })
            .flatten()
            .collect();
        // One declaration per symbol: externs may share one (`#[link_name]`)
        // or name a prebuilt dependency's `#[export_name]` function.
        let mut declared: HashSet<&str> = HashSet::new();
        for &f in view {
            let sym = fns[&f.id].name.as_str();
            if f.facts.linkage != Linkage::Defined
                && (defined.contains(sym) || !declared.insert(sym))
            {
                continue;
            }
            match &unit {
                Some((core, u))
                    if !core.defines(*u, f.id) && f.facts.linkage == Linkage::Defined =>
                {
                    self.declare_unit_function(&fns[&f.id]);
                }
                _ => self.emit_function(f, fns, sources),
            }
        }
        for (id, abi) in c_entries {
            let f = module.functions.iter().find(|f| f.id == *id);
            let exported = f.is_some_and(|f| f.facts.export_name.is_some());
            // Another unit may take its address.
            let shared = unit.as_ref().is_some_and(|(c, _)| c.shared.contains(id));
            let span = f.map_or(Span::DUMMY, |f| f.span);
            self.emit_c_entry(&fns[id], abi, exported, shared, span);
        }
    }

    /// `main`'s wrapper, the GC registration and the statics (a unit's
    /// glue), the runtime declarations and the string literals.
    fn emit_rest(
        &mut self,
        module: &MirModule,
        view: &[&MirFunction],
        fns: &HashMap<FunctionId, FnSig>,
    ) {
        if let Some(m) = module
            .functions
            .iter()
            .find(|f| f.facts.role == FnRole::Main)
            .filter(|_| self.glue)
        {
            let symbol = fns[&m.id].name.clone();
            if self.repl.is_some() {
                // The host already runs the runtime: register this cell's
                // types and roots, then run its statements.
                let (ret, ty) = match &m.ret_ty {
                    MirTy::Unit => ("", "()".to_owned()),
                    t => ("%0 = ", mlir_type(t, &self.target)),
                };
                self.needs.insert("rok_tls");
                let text = format!(
                    "  func.func @rok_repl_cell() {{\n    func.call @rok.register_types() : () -> ()\n{TLS_BASE}    {ret}func.call @{symbol}(%tls) : (!llvm.ptr) -> {ty}\n    func.return\n  }}\n"
                );
                let text = self.described(text, "rok_repl_cell", m.span);
                self.out.push_str(&text);
            } else {
                let start = self.out.len();
                self.emit_main_wrapper(&symbol, &m.ret_ty, m.span);
                let text = self.out.split_off(start);
                let text = self.described(text, "main", m.span);
                self.out.push_str(&text);
            }
        }
        if self.glue {
            self.emit_gc_support(module);
        }
        self.emit_static_globals(module);
        // One declaration per runtime symbol, unless the program declares
        // it itself.
        let declared: HashSet<&str> = view
            .iter()
            .filter(|f| f.facts.linkage == Linkage::Extern)
            .map(|f| f.name.as_str())
            .collect();
        // wasm32 defines `rok_init_once` beside the main wrapper.
        let main_here = self.repl.is_none()
            && self.glue
            && module
                .functions
                .iter()
                .any(|f| f.facts.role == FnRole::Main);
        for name in std::mem::take(&mut self.needs) {
            if declared.contains(name) || (name == "rok_init_once" && main_here) {
                continue;
            }
            match runtime_decl(name, &self.target) {
                Some(decl) => {
                    let _ = writeln!(self.out, "  {decl}");
                }
                None => self.errors.push((
                    rokc_diagnostic::Span::DUMMY,
                    format!("internal: unknown runtime symbol `{name}`"),
                )),
            }
        }
        if !self.glue {
            for sym in std::mem::take(&mut self.gc_syms) {
                let _ = writeln!(
                    self.out,
                    "  llvm.mlir.global external constant @{sym}() {{addr_space = 0 : i32}} : i32"
                );
            }
        }
        self.emit_string_globals();
        self.out.push_str("}\n");
    }

    /// The `ScanTable`s (802) and `rok.register_types`, which registers them
    /// and the static roots right after `rok_runtime_init`.
    fn emit_gc_support(&mut self, module: &MirModule) {
        let mut calls = String::new();
        // Earlier REPL cells registered the first tables.
        let pre = self.repl.as_ref().map_or(0, |r| r.gc_tables.len());
        for (i, words) in self.gc_types.iter().enumerate().skip(pre) {
            let list = words.iter().map(u32::to_string).collect::<Vec<_>>();
            let n = words.len();
            let table = crate::units::scan_symbol(words);
            let _ = writeln!(
                self.out,
                "  llvm.mlir.global weak_odr hidden constant @{table}(dense<[{}]> : tensor<{n}xi32>) {{addr_space = 0 : i32}} : !llvm.array<{n} x i32>",
                list.join(", ")
            );
            let id = self.gc_type_base() + i as u32;
            if let Some((core, _)) = &self.unit {
                let sym = crate::units::gc_symbol(&core.placeholder, words);
                let _ = writeln!(
                    self.out,
                    "  llvm.mlir.global external constant @{sym}({id} : i32) {{addr_space = 0 : i32}} : i32"
                );
            }
            let _ = write!(
                calls,
                "    %t{i} = arith.constant {id} : i32\n    %s{i} = llvm.mlir.addressof @{table} : !llvm.ptr\n    func.call @rok_gc_register_type(%t{i}, %s{i}) : (i32, !llvm.ptr) -> ()\n"
            );
            self.needs.insert("rok_gc_register_type");
        }
        // Statics are GC roots: every reference word of their storage.
        let mut roots = 0;
        let rooted: Vec<_> = module
            .statics
            .iter()
            .filter(|s| (!s.is_extern || self.defines(&s.name)) && !self.prebuilt_static(&s.name))
            .collect();
        for s in rooted {
            let sym = self.pkgs.static_sym(&s.name);
            for off in self.target.root_offsets(&s.ty) {
                let a = format!("%r{roots}");
                let _ = writeln!(calls, "    {a} = llvm.mlir.addressof @{sym} : !llvm.ptr");
                let _ = writeln!(
                    calls,
                    "    {a}w = llvm.getelementptr {a}[{off}] : (!llvm.ptr) -> !llvm.ptr, i8"
                );
                let _ = writeln!(
                    calls,
                    "    func.call @rok_gc_add_root({a}w) : (!llvm.ptr) -> ()"
                );
                self.needs.insert("rok_gc_add_root");
                roots += 1;
            }
        }
        // A prebuilt library exports its registration as
        // `rok.register_types.<pkg>`; a program calls its dependencies'.
        for dep in &self.pkgs.link.prebuilt {
            let _ = writeln!(self.out, "  func.func private @rok.register_types.{dep}()");
            let _ = writeln!(
                calls,
                "    func.call @rok.register_types.{dep}() : () -> ()"
            );
        }
        let head = match &self.pkgs.link.lib {
            Some(lib) => format!("func.func @rok.register_types.{lib}"),
            None => "func.func private @rok.register_types".to_owned(),
        };
        let text = format!("  {head}() {{\n{calls}    func.return\n  }}\n");
        let text = self.described(text, "rok.register_types", Span::DUMMY);
        self.out.push_str(&text);
    }

    /// The function text `text` (`func.func .. {` to its `}`) with a
    /// subprogram `name` at `span` under `-g` (`DebugInfo::synthesized`).
    fn described(&mut self, text: String, name: &str, span: Span) -> String {
        let d = self
            .debug
            .as_mut()
            .and_then(|di| di.synthesized(name, span));
        match (d, text.strip_suffix("  }\n")) {
            (Some(d), Some(body)) => format!(
                "{}  }} loc(fused<{}>[\"{}\":{}:{}])\n",
                crate::debug::locate(body, &d),
                d.subprogram,
                d.file,
                d.line,
                d.col
            ),
            _ => text,
        }
    }

    fn emit_static_globals(&mut self, module: &MirModule) {
        let pw = self.pw();
        let unit = self.unit.clone();
        for s in &module.statics {
            // A unit declares the statics it uses (the glue defines them).
            if let Some((core, u)) = &unit {
                if !self.glue && !core.statics[*u].contains(&s.name) {
                    continue;
                }
            }
            let ty_str = mlir_type(&s.ty, &self.target);
            let constness = if s.is_mut { "" } else { "constant " };
            let sym = self.pkgs.static_sym(&s.name);
            if s.is_extern && self.defines(&s.name) {
                // A REPL binding's slot: zeroed, exported.
                let _ = writeln!(
                    self.out,
                    "  llvm.mlir.global external @{sym}() {{addr_space = 0 : i32}} : {ty_str} {{\n    %0 = llvm.mlir.zero : {ty_str}\n    llvm.return %0 : {ty_str}\n  }}"
                );
                continue;
            }
            if self.pkgs.dll_statics.contains(&s.name) {
                // A DLL's: its import address.
                let imp = self.pkgs.import_sym(&s.name, self.target.arch == "x86");
                let _ = writeln!(
                    self.out,
                    "  llvm.mlir.global external constant @{imp}() {{addr_space = 0 : i32}} : !llvm.ptr"
                );
                continue;
            }
            if self.prebuilt_static(&s.name) {
                // In the dependency's `.a`, a lazy one's state byte and
                // `.init` too (an earlier REPL cell keeps `.init` private).
                // A lazy one is written at run time: not `constant`, or a
                // load could move above its `.init`.
                let lazy = self.pkgs.lazy_statics.contains_key(&s.name);
                let constness = if lazy { "" } else { constness };
                let _ = writeln!(
                    self.out,
                    "  llvm.mlir.global external {constness}@{sym}() {{addr_space = 0 : i32}} : {ty_str}"
                );
                if let Some(init) = self.pkgs.lazy_statics.get(&s.name).cloned() {
                    let _ = writeln!(
                        self.out,
                        "  llvm.mlir.global external @{sym}.state() {{addr_space = 0 : i32}} : i8"
                    );
                    if self.link().in_prebuilt(&s.name) || !self.glue {
                        let s = mlir_prim(PrimTy::Str, self.target.pointer_width);
                        let _ = writeln!(self.out, "  func.func private @{sym}.init({s})");
                    } else {
                        let by_addr = rok_by_ref(&s.ty, &self.target);
                        self.emit_lazy_init(&s.name, &sym, &ty_str, &init, false, by_addr);
                    }
                }
                continue;
            }
            if s.is_extern {
                // Defined elsewhere: a declaration only.
                let _ = writeln!(
                    self.out,
                    "  llvm.mlir.global external {constness}@{sym}() {{addr_space = 0 : i32}} : {ty_str}"
                );
                continue;
            }
            let linkage = self.static_linkage(&s.name);
            if let Some(init) = self.pkgs.lazy_statics.get(&s.name).cloned() {
                let export = unit.is_some()
                    || self
                        .link()
                        .lib
                        .as_deref()
                        .is_some_and(|l| Link::owns(l, &s.name));
                let by_addr = rok_by_ref(&s.ty, &self.target);
                self.emit_lazy_static(&s.name, &sym, &ty_str, &init, linkage, (export, by_addr));
                continue;
            }
            if let ConstVal::Str(text) = &s.init {
                // A pointer-valued global needs an initializer region
                // that takes the literal's address.
                let lit = self.intern_global_string(text);
                let n = text.len();
                let _ = writeln!(
                    self.out,
                    "  llvm.mlir.global {linkage} {constness}@{sym}() {{addr_space = 0 : i32}} : {ty_str} {{\n    %0 = llvm.mlir.addressof @{lit} : !llvm.ptr\n    %1 = llvm.mlir.constant({n} : i{pw}) : i{pw}\n    %2 = llvm.mlir.undef : {ty_str}\n    %3 = llvm.insertvalue %0, %2[0] : {ty_str}\n    %4 = llvm.insertvalue %1, %3[1] : {ty_str}\n    llvm.return %4 : {ty_str}\n  }}"
                );
                continue;
            }
            match format_const_attr(&s.init, &s.ty, &self.target) {
                Some(init) => {
                    let _ = writeln!(
                        self.out,
                        "  llvm.mlir.global {linkage} {constness}@{sym}({init}) {{addr_space = 0 : i32}} : {ty_str}"
                    );
                }
                None => self.errors.push((
                    rokc_diagnostic::Span::DUMMY,
                    format!(
                        "unsupported in codegen: initializer of static `{}` : `{ty_str}`",
                        s.name
                    ),
                )),
            }
        }
    }

    /// A static with a non-literal initializer: zeroed storage, a state byte
    /// (0 new, 1 running, 2 ready) and `@sym.init`, which runs `init` once
    /// across threads (`rok_static_init_begin` / `_end`). Every access checks
    /// the state first (`FnEmit::ensure_static_init`).
    fn emit_lazy_static(
        &mut self,
        name: &str,
        sym: &str,
        ty: &str,
        init: &str,
        linkage: &str,
        (export, by_addr): (bool, bool),
    ) {
        let _ = write!(
            self.out,
            "  llvm.mlir.global {linkage} @{sym}() {{addr_space = 0 : i32}} : {ty} {{
    %0 = llvm.mlir.zero : {ty}
    llvm.return %0 : {ty}
  }}
  llvm.mlir.global {linkage} @{sym}.state(0 : i8) {{addr_space = 0 : i32}} : i8
"
        );
        self.emit_lazy_init(name, sym, ty, init, export, by_addr);
    }

    /// `@sym.init(at)`: run `init` once; `at` locates the panic of a
    /// recursive initialization. Exported by the prebuilt library owning
    /// the static. `by_addr`: `init` returns through a leading pointer
    ///, the static itself.
    fn emit_lazy_init(
        &mut self,
        name: &str,
        sym: &str,
        ty: &str,
        init: &str,
        export: bool,
        by_addr: bool,
    ) {
        self.needs.insert("rok_static_init_begin");
        self.needs.insert("rok_static_init_end");
        let pw = self.target.pointer_width;
        let s = mlir_prim(PrimTy::Str, pw);
        let head = if export {
            format!("func.func @{sym}.init(%at: {s})")
        } else {
            format!(
                "func.func private @{sym}.init(%at: {s}) attributes {{llvm.linkage = #llvm.linkage<internal>}}"
            )
        };
        self.needs.insert("rok_tls");
        let call = if by_addr {
            format!(
                "{TLS_BASE}    %5 = llvm.mlir.addressof @{sym} : !llvm.ptr
    func.call @{init}(%5, %tls) : (!llvm.ptr, !llvm.ptr) -> ()"
            )
        } else {
            format!(
                "{TLS_BASE}    %4 = func.call @{init}(%tls) : (!llvm.ptr) -> {ty}
    %5 = llvm.mlir.addressof @{sym} : !llvm.ptr
    llvm.store %4, %5 : {ty}, !llvm.ptr"
            )
        };
        let text = format!(
            "  {head} {{
    %0 = llvm.mlir.addressof @{sym}.state : !llvm.ptr
    %ap = llvm.extractvalue %at[0] : {s}
    %al = llvm.extractvalue %at[1] : {s}
    %1 = func.call @rok_static_init_begin(%0, %ap, %al) : (!llvm.ptr, !llvm.ptr, i{pw}) -> i32
    %2 = arith.constant 0 : i32
    %3 = arith.cmpi ne, %1, %2 : i32
    cf.cond_br %3, ^bb1, ^bb2
  ^bb1:
{call}
    func.call @rok_static_init_end(%0) : (!llvm.ptr) -> ()
    func.return
  ^bb2:
    func.return
  }}
"
        );
        let text = self.described(text, &format!("{name}.init"), Span::DUMMY);
        self.out.push_str(&text);
    }

    fn intern_global_string(&mut self, text: &str) -> String {
        self.str_globals
            .entry(text.as_bytes().to_vec())
            .or_insert_with(|| crate::mangle::str_global(text.as_bytes()))
            .clone()
    }

    /// One `internal unnamed_addr constant` NUL-terminated byte array per
    /// interned literal (so the pointer is a C string). Addresses aren't
    /// significant: split compiles copy it into each part.
    fn emit_string_globals(&mut self) {
        let entries: Vec<(Vec<u8>, String)> = self
            .str_globals
            .iter()
            .map(|(content, sym)| (content.clone(), sym.clone()))
            .collect();
        for (content, sym) in entries {
            let escaped = mlir_bytes(&content);
            let len = content.len() + 1;
            let _ = writeln!(
                self.out,
                "  llvm.mlir.global internal unnamed_addr constant @{sym}(\"{escaped}\\00\") {{addr_space = 0 : i32}} : !llvm.array<{len} x i8>"
            );
        }
    }

    fn emit_function(
        &mut self,
        f: &MirFunction,
        fns: &HashMap<FunctionId, FnSig>,
        sources: &SourceMap,
    ) {
        let sig = &fns[&f.id];
        // A prebuilt function with its body here is a copy for the
        // optimizer to inline (`HirInput::inline_prebuilt`).
        let inline_copy = f.facts.linkage == Linkage::Prebuilt && !f.blocks.is_empty();
        if f.facts.linkage != Linkage::Defined && !inline_copy {
            if let Some(abi) = &sig.c {
                self.emit_c_decl(
                    &sig.name,
                    &sig.params,
                    &sig.ret,
                    abi,
                    f.facts.wasm_import.as_ref(),
                );
                return;
            }
            // Runtime (`extern "rok"`) functions are C functions; prebuilt
            // rok functions are defined, so declared, without extension
            // attributes.
            let (params, ret) = if f.facts.linkage == Linkage::Prebuilt {
                let (params, ret) = sig.rok_types(&self.target);
                (params, ret.map_or_else(String::new, |r| format!(" -> {r}")))
            } else {
                let params = sig
                    .params
                    .iter()
                    .map(|t| mlir_type(t, &self.target) + c_ext_attr(t, &self.target))
                    .collect();
                let ret = (sig.ret != MirTy::Unit).then(|| mlir_type(&sig.ret, &self.target));
                (
                    params,
                    c_ret_arrow(ret.as_deref(), &sig.ret, None, &self.target),
                )
            };
            let _ = writeln!(
                self.out,
                "  func.func private @{}({}){ret}",
                sig.name,
                params.join(", ")
            );
            return;
        }
        let private = !inline_copy && self.private_copy(f);
        let vis = if private { "private " } else { "" };
        let (mut params, ret) = rok_fn_types(&f.param_tys, &f.ret_ty, &self.target);
        let tls_arg = sig.tls.then(|| {
            params.push("!llvm.ptr".to_owned());
            format!("%arg{}", params.len() - 1)
        });
        let params: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(i, t)| format!("%arg{i}: {t}"))
            .collect();
        let _ = write!(
            self.out,
            "  func.func {vis}@{}({})",
            sig.name,
            params.join(", ")
        );
        if let Some(r) = ret {
            let _ = write!(self.out, " -> {r}");
        }
        let mut attrs = Vec::new();
        if inline_copy {
            attrs.push("llvm.linkage = #llvm.linkage<available_externally>".to_owned());
        } else if private {
            // `private` is MLIR visibility only; this is the LLVM linkage.
            attrs.push("llvm.linkage = #llvm.linkage<internal>".to_owned());
        } else if self.link().lib.is_some() && f.facts.role == FnRole::AnyDispatcher {
            // The program's own (complete) dispatcher replaces it.
            attrs.push("llvm.linkage = #llvm.linkage<weak>".to_owned());
        }
        // Stack probes: a frame over a page touches each page on the way
        // down, so overflow faults in the guard page where the runtime
        // reports it. rok code never unwinds. A loop-free never-returning
        // function is a panic path: kept out of line so its callers'
        // checks stay small enough to inline.
        let cold = if f.facts.inline != Inline::Always
            && f.blocks
                .iter()
                .all(|b| !matches!(b.terminator, Terminator::Return(_)))
            && back_edge_blocks(f).is_empty()
        {
            r#", "noreturn", "noinline", "cold""#
        } else {
            match f.facts.inline {
                Inline::Auto => "",
                Inline::Hint => r#", "inlinehint""#,
                Inline::Always => r#", "alwaysinline""#,
                Inline::Never => r#", "noinline""#,
                Inline::Cold => r#", "noinline", "cold""#,
            }
        };
        // Windows probes through `__chkstk` instead (LLVM would take
        // "inline-asm" there for the probe function's name).
        if self.target.stack_probes == rokc_mir::StackProbes::Inline {
            attrs.push(format!(
                r#"passthrough = ["nounwind", ["probe-stack", "inline-asm"]{cold}]"#
            ));
        } else {
            attrs.push(format!(r#"passthrough = ["nounwind"{cold}]"#));
        }
        if !attrs.is_empty() {
            let _ = write!(self.out, " attributes {{{}}}", attrs.join(", "));
        }
        self.out.push_str(" {\n");

        let str_globals = std::mem::take(&mut self.str_globals);
        let mut emit = FnEmit::new(f, &self.target, fns, str_globals, &self.pkgs, sources);
        emit.quiet = self.quiet.clone();
        emit.back_edges = polled_back_edges(f, &emit.quiet, &self.pkgs, &self.target);
        emit.lists = self.lists.clone();
        emit.list_alias = self.list_alias.clone();
        emit.list_scopes = self.list_alias.scoped(f, &self.lists);
        emit.entry_poll = self.reentrant.contains(&f.id);
        emit.eager_poll = self.entered.contains(&f.id);
        emit.tls_arg = tls_arg;
        emit.overflow_checks = self.overflow_checks;
        emit.gc_type_base = self.gc_type_base();
        emit.gc_types = std::mem::take(&mut self.gc_types);
        emit.gc_pkg = self.unit.as_ref().map(|(c, _)| c.placeholder.clone());
        emit.gc_syms = std::mem::take(&mut self.gc_syms);
        if let Some(di) = self.debug.as_mut() {
            emit.dbg = di.subprogram(f, f.path());
            if let Some(d) = &emit.dbg {
                let lines = crate::debug::decl_lines(f);
                for l in &f.locals {
                    let Some(name) = l.name.as_deref().filter(|n| rokc_mir::debug_visible(n))
                    else {
                        continue;
                    };
                    // Zero-sized values show nothing, and their promoted `{}`
                    // location crashes LLVM's instruction selection.
                    if emit.is_block_param(l.id) || self.target.size_align(&l.ty).0 == 0 {
                        continue;
                    }
                    let arg = f.params.iter().position(|p| *p == l.id).map(|i| i + 1);
                    let (line, scope) = lines.get(&l.id.0).copied().unwrap_or((d.line, 0));
                    let scope = d.scopes.get(&scope).unwrap_or(&d.subprogram);
                    let ty = di
                        .debug
                        .locals
                        .get(&(f.id, l.id))
                        .filter(|t| t.fits(&l.ty))
                        .cloned()
                        .unwrap_or_else(|| rokc_mir::DebugTy::Mir(l.ty.clone()));
                    if let Some(var) = di.local(scope, f.span.file, name, &ty, line, arg) {
                        // LLVM puts a stack variable in the scope of its
                        // declare's location, not of the variable.
                        let at = format!("loc(fused<{scope}>[\"{}\":{line}:1])", d.file);
                        emit.dbg_vars.push((l.id.0, var, at));
                    }
                }
            }
        }
        emit.emit_prelude();
        for b in &f.blocks {
            emit.emit_block(b);
        }
        self.str_globals = std::mem::take(&mut emit.str_globals);
        self.needs.append(&mut emit.needs);
        self.thunks.append(&mut emit.thunks);
        self.gc_types = std::mem::take(&mut emit.gc_types);
        self.gc_syms = std::mem::take(&mut emit.gc_syms);
        for e in std::mem::take(&mut emit.errors) {
            self.errors.push((f.span, format!("in `{}`: {e}", f.name)));
        }
        let fn_loc = emit.dbg.as_ref().map(|d| {
            format!(
                " loc(fused<{}>[\"{}\":{}:{}])",
                d.subprogram, d.file, d.line, d.col
            )
        });
        self.out.push_str(&emit.finish());

        self.out.push_str("  }");
        self.out.push_str(fn_loc.as_deref().unwrap_or_default());
        self.out.push('\n');
    }

    /// A function another unit defines: its rok body, and
    /// its C entry, which a function value may take.
    fn declare_unit_function(&mut self, sig: &FnSig) {
        let (params, ret) = sig.rok_types(&self.target);
        let ret = ret.map_or_else(String::new, |r| format!(" -> {r}"));
        let _ = writeln!(
            self.out,
            "  func.func private @{}({}){ret}",
            sig.name,
            params.join(", ")
        );
        if let Some((entry, ty)) = &sig.c_entry {
            let (params, ret) = ty.split_once(" -> ").unwrap_or((ty, "()"));
            let ret = if ret == "()" {
                String::new()
            } else {
                format!(" -> {ret}")
            };
            if sig.stdcall {
                let _ = writeln!(self.out, "  llvm.func x86_stdcallcc @{entry}{params}{ret}");
            } else {
                let _ = writeln!(self.out, "  func.func private @{entry}{params}{ret}");
            }
        }
    }

    /// C entry of an `extern "C" fn`: takes arguments the C way
    /// (coerced, by pointer, `sret`), calls the rok body and returns the C
    /// way. Exported under `#[export_name]`, else internal unless `shared`
    /// with another unit.
    fn emit_c_entry(&mut self, sig: &FnSig, abi: &FnAbi, exported: bool, shared: bool, span: Span) {
        let Some((entry, _)) = &sig.c_entry else {
            return;
        };
        let (params, ret) = c_signature(&sig.params, &sig.ret, abi, &self.target);
        let head: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(i, (t, a))| format!("%arg{i}: {t}{a}"))
            .collect();
        let arrow = c_ret_arrow(ret.as_deref(), &sig.ret, Some(abi), &self.target);
        let (vis, linkage) = if exported && self.reactor() {
            // A browser module exports it to JS under the same name.
            (
                "",
                format!(r#" attributes {{passthrough = [["wasm-export-name", "{entry}"]]}}"#),
            )
        } else if exported || shared {
            ("", String::new())
        } else {
            (
                "private ",
                " attributes {llvm.linkage = #llvm.linkage<internal>}".to_owned(),
            )
        };
        let op = if abi.stdcall {
            let internal = if vis.is_empty() { "" } else { "internal " };
            format!(
                "llvm.func {internal}x86_stdcallcc @{entry}({}){arrow}",
                head.join(", ")
            )
        } else {
            format!(
                "func.func {vis}@{entry}({}){arrow}{linkage}",
                head.join(", ")
            )
        };
        let mut b = format!(
            "  {op} {{
    %one = llvm.mlir.constant(1 : i32) : i32
"
        );
        let slot = |b: &mut String, n: &str, ty: &MirTy| {
            let size = self.target.size_align(ty).0.next_multiple_of(8).max(16);
            let _ = writeln!(
                b,
                "    {n} = llvm.alloca %one x !llvm.array<{size} x i8> {{alignment = 16 : i64}} : (i32) -> !llvm.ptr"
            );
        };
        let sret = usize::from(abi.ret == RetAbi::Sret);
        // A by-address result goes straight to C's `sret`
        // or to a slot.
        let by_ref_ret = rok_by_ref(&sig.ret, &self.target);
        let rty = mlir_type(&sig.ret, &self.target);
        let mut vals = Vec::new();
        if by_ref_ret {
            if sret == 1 {
                vals.push("%arg0".to_owned());
            } else {
                let _ = writeln!(
                    b,
                    "    %rp = llvm.alloca %one x {rty} {{alignment = 16 : i64}} : (i32) -> !llvm.ptr"
                );
                vals.push("%rp".to_owned());
            }
        }
        for (i, t) in sig.params.iter().enumerate() {
            let a = format!("%arg{}", i + sret);
            let ty = mlir_type(t, &self.target);
            let arg = abi.args.get(i).unwrap_or(&ArgAbi::Direct);
            if rok_by_ref(t, &self.target) {
                // By address: C's own copy when it passed one.
                if let ArgAbi::Indirect { .. } = arg {
                    vals.push(a);
                } else {
                    slot(&mut b, &format!("%s{i}"), t);
                    let c = match arg {
                        ArgAbi::Coerce(c) => c.clone(),
                        _ => ty.clone(),
                    };
                    let _ = writeln!(b, "    llvm.store {a}, %s{i} : {c}, !llvm.ptr");
                    vals.push(format!("%s{i}"));
                }
                continue;
            }
            match arg {
                ArgAbi::Direct => vals.push(a),
                ArgAbi::Coerce(c) => {
                    slot(&mut b, &format!("%s{i}"), t);
                    let _ = write!(
                        b,
                        "    llvm.store {a}, %s{i} : {c}, !llvm.ptr
    %v{i} = llvm.load %s{i} : !llvm.ptr -> {ty}
"
                    );
                    vals.push(format!("%v{i}"));
                }
                ArgAbi::Indirect { .. } => {
                    let _ = writeln!(b, "    %v{i} = llvm.load {a} : !llvm.ptr -> {ty}");
                    vals.push(format!("%v{i}"));
                }
            }
        }
        let (tys, _) = sig.rok_types(&self.target);
        if sig.tls {
            self.needs.insert("rok_tls");
            b.push_str(TLS_BASE);
            vals.push("%tls".to_owned());
        }
        let unit = sig.ret == MirTy::Unit || by_ref_ret;
        // wasm32: the host may call an export before `_start` (or
        // without it): the runtime starts on the first call.
        if exported && self.target.family == "wasm" {
            self.needs.insert("rok_init_once");
            b.push_str("    func.call @rok_init_once() : () -> ()\n");
        }
        // Called from C that this thread entered from rok (`PARKED`):
        // run as a mutator again for the body.
        self.needs.insert("rok_native_reenter");
        self.needs.insert("rok_native_resume");
        let _ = writeln!(
            b,
            "    %parked = func.call @rok_native_reenter() : () -> i32
    {}func.call @{}({}) : ({}) -> {}
    func.call @rok_native_resume(%parked) : (i32) -> ()",
            if unit { "" } else { "%r = " },
            sig.name,
            vals.join(", "),
            tys.join(", "),
            if unit { "()" } else { &rty }
        );
        if by_ref_ret && sret == 0 {
            let _ = writeln!(b, "    %r = llvm.load %rp : !llvm.ptr -> {rty}");
        }
        match &abi.ret {
            _ if sig.ret == MirTy::Unit || (by_ref_ret && sret == 1) => {
                b.push_str("    func.return\n");
            }
            RetAbi::Direct => {
                let _ = writeln!(b, "    func.return %r : {rty}");
            }
            RetAbi::Coerce(c) => {
                slot(&mut b, "%rs", &sig.ret);
                let _ = write!(
                    b,
                    "    llvm.store %r, %rs : {rty}, !llvm.ptr
    %rc = llvm.load %rs : !llvm.ptr -> {c}
    func.return %rc : {c}
"
                );
            }
            RetAbi::Sret => {
                let _ = write!(
                    b,
                    "    llvm.store %r, %arg0 : {rty}, !llvm.ptr
    func.return
"
                );
            }
        }
        if abi.stdcall {
            // An `llvm.func` returns with `llvm.return`.
            b = b.replace("func.return", "llvm.return");
        }
        b.push_str("  }\n");
        let b = self.described(b, entry, span);
        self.out.push_str(&b);
    }

    /// An `extern "C"` declaration under the platform C ABI:
    /// aggregates per `abi::classify`, `str` as `char*`, small integers
    /// caller-extended, variadics as an `llvm.func` with `...`.
    /// `wasm_import`: its wasm32 core import `(module, name)`.
    fn emit_c_decl(
        &mut self,
        symbol: &str,
        params: &[MirTy],
        ret_ty: &MirTy,
        abi: &FnAbi,
        wasm_import: Option<&(String, String)>,
    ) {
        let (params, ret) = c_signature(params, ret_ty, abi, &self.target);
        let ret = c_ret_arrow(ret.as_deref(), ret_ty, Some(abi), &self.target);
        let mut params: Vec<String> = params.into_iter().map(|(t, a)| t + &a).collect();
        if REF_ONLY_CALLS.contains(&symbol) {
            let _ = writeln!(
                self.out,
                "  llvm.func @{symbol}({}){ret}{}",
                params.join(", "),
                runtime_fn_attrs(symbol)
            );
            return;
        }
        if abi.variadic || abi.stdcall {
            if abi.variadic {
                params.push("...".to_owned());
            }
            let cc = if abi.stdcall { "x86_stdcallcc " } else { "" };
            let _ = writeln!(
                self.out,
                "  llvm.func {cc}@{symbol}({}){ret}",
                params.join(", ")
            );
            return;
        }
        let params: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(i, p)| format!("%arg{i}: {p}"))
            .collect();
        let attrs = match wasm_import {
            Some((module, name)) if self.target.family == "wasm" => format!(
                " attributes {{passthrough = [[\"wasm-import-module\", {module:?}], [\"wasm-import-name\", {name:?}]]}}"
            ),
            _ if abi.rok => runtime_fn_attrs(symbol).to_owned(),
            _ => String::new(),
        };
        let _ = writeln!(
            self.out,
            "  func.func private @{symbol}({}){ret}{attrs}",
            params.join(", ")
        );
    }

    /// A browser module (`wasm32-unknown-unknown`): a wasm reactor.
    fn reactor(&self) -> bool {
        self.target.reactor()
    }

    /// The C `main`: runtime init, type registration, the
    /// user's `main`, shutdown; its result is the exit code. On wasm32 it is
    /// `__main_void` (what wasi-libc's `_start` calls), or the export `main`
    /// in a browser reactor.
    fn emit_main_wrapper(
        &mut self,
        user_main: &str,
        user_ret_ty: &MirTy,
        span: rokc_diagnostic::Span,
    ) {
        let pw = self.pw();
        self.needs.insert("rok_runtime_init");
        self.needs.insert("rok_runtime_shutdown");
        let entry = if self.target.family == "wasm" {
            "__main_void"
        } else {
            "main"
        };
        let attrs = if self.reactor() {
            r#" attributes {passthrough = [["wasm-export-name", "main"]]}"#
        } else {
            ""
        };
        let shutdown = "    func.call @rok_runtime_shutdown() : () -> ()\n";
        if self.target.family == "wasm" {
            // Once, by `_start` or the first export called (an export
            // of a component may run without `_start`).
            self.out.push_str(
                "  llvm.mlir.global internal @rok.inited(0 : i8) {addr_space = 0 : i32} : i8
  func.func @rok_init_once() {
    %a = llvm.mlir.addressof @rok.inited : !llvm.ptr
    %v = llvm.load %a : !llvm.ptr -> i8
    %z = arith.constant 0 : i8
    %c = arith.cmpi eq, %v, %z : i8
    cf.cond_br %c, ^init, ^done
  ^init:
    %one = arith.constant 1 : i8
    llvm.store %one, %a : i8, !llvm.ptr
    func.call @rok_runtime_init() : () -> ()
    func.call @rok.register_types() : () -> ()
    cf.br ^done
  ^done:
    func.return
  }
",
            );
            let _ = writeln!(self.out, "  func.func @{entry}() -> i32{attrs} {{");
            self.out
                .push_str("    func.call @rok_init_once() : () -> ()\n");
        } else {
            let _ = writeln!(self.out, "  func.func @{entry}() -> i32 {{");
            self.out.push_str(
                "    func.call @rok_runtime_init() : () -> ()\n    func.call @rok.register_types() : () -> ()\n",
            );
        }
        self.needs.insert("rok_tls");
        self.out.push_str(TLS_BASE);
        if matches!(user_ret_ty, MirTy::Unit) {
            let _ = writeln!(
                self.out,
                "    func.call @{user_main}(%tls) : (!llvm.ptr) -> ()"
            );
            self.out.push_str(shutdown);
            self.out.push_str("    %0 = arith.constant 0 : i32\n");
            self.out.push_str("    func.return %0 : i32\n");
            self.out.push_str("  }\n");
            return;
        }
        let ret = mlir_type(user_ret_ty, &self.target);
        let _ = writeln!(
            self.out,
            "    %0 = func.call @{user_main}(%tls) : (!llvm.ptr) -> {ret}"
        );
        self.out.push_str(shutdown);
        // The OS wants an `i32` exit code.
        let conv = match user_ret_ty {
            MirTy::Prim(p) if p.is_int() || *p == PrimTy::Bool => {
                let w = if *p == PrimTy::Bool {
                    1
                } else {
                    p.bit_width(pw)
                };
                match w.cmp(&32) {
                    std::cmp::Ordering::Equal => None,
                    std::cmp::Ordering::Greater => Some("arith.trunci"),
                    std::cmp::Ordering::Less if p.is_signed_int() => Some("arith.extsi"),
                    std::cmp::Ordering::Less => Some("arith.extui"),
                }
            }
            _ => {
                self.errors.push((
                    span,
                    format!("unsupported in codegen: `main` returning `{ret}`"),
                ));
                None
            }
        };
        if let Some(op) = conv {
            let _ = writeln!(self.out, "    %1 = {op} %0 : {ret} to i32");
            self.out.push_str("    func.return %1 : i32\n");
        } else {
            self.out.push_str("    func.return %0 : i32\n");
        }
        self.out.push_str("  }\n");
    }

    #[must_use]
    pub fn gc_type_count(&self) -> u32 {
        self.gc_types.len() as u32
    }

    #[must_use]
    pub fn finish(self) -> String {
        let defs = self.debug.map(|d| d.defs).unwrap_or_default();
        format!("{TBAA_DEFS}{defs}{}", self.out)
    }
}

struct FnEmit<'a> {
    f: &'a MirFunction,
    /// The compile's source files (panic locations).
    sources: &'a SourceMap,
    target: &'a Target,
    /// `target.pointer_width`.
    pw: u32,
    body: String,
    /// SSA name of each unslotted local (block params, `()`, `any`).
    local_names: Vec<Option<String>>,
    /// The `llvm.alloca` slot of each slotted local.
    slot_names: Vec<Option<String>>,
    /// One `llvm.alloca` per byte-array union type (keyed by its MLIR
    /// type), the scratch memory aggregate payload bitcasts go through.
    union_scratch: HashMap<String, String>,
    /// Set of locals that are block parameters (consumed as SSA values
    /// rather than stack-slotted).
    block_param_set: HashSet<u32>,
    next_ssa: u32,
    fns: &'a HashMap<FunctionId, FnSig>,
    /// The module-wide string table, lent to this function while it is
    /// emitted so identical literals share one global across functions.
    str_globals: std::collections::BTreeMap<Vec<u8>, String>,
    errors: Vec<String>,
    needs: BTreeSet<&'static str>,
    /// Lent `Emitter::gc_types`.
    gc_types: Vec<Vec<u32>>,
    /// This function's shadow-stack frame, if it has one.
    gc_frame: Option<GcFrame>,
    /// The frame of `poll_roots`, linked only on a back-edge poll's yield
    /// path.
    poll_frame: Option<GcFrame>,
    /// Where the frame links and a reentrant function polls
    /// (`plan_links`); `None`: at entry.
    links: Option<Links>,
    /// Root slot of each GC-holding block param, stored on block entry.
    gc_param_slots: HashMap<u32, String>,
    /// Counter for the extra blocks runtime checks split off.
    next_check: u32,
    pkgs: &'a crate::mangle::Pkgs,
    overflow_checks: bool,
    /// Lent `Emitter::gc_type_base()`.
    gc_type_base: u32,
    /// In a codegen unit, the user package's hash in its GC type id
    /// symbols (`gc_tid`).
    gc_pkg: Option<String>,
    /// Lent `Emitter::gc_syms`.
    gc_syms: BTreeSet<String>,
    /// Blocks whose terminator takes a loop back-edge: a safepoint poll
    /// goes right before it. Each maps to its loop headers.
    back_edges: HashMap<u32, HashSet<u32>>,
    /// Loop headers entered through `^rok_re<H>`: every edge but the fast
    /// back edge goes there, so the yield call sits in an outer loop and
    /// LLVM can hoist the inner loop's invariant loads into it.
    reentry: HashSet<u32>,
    /// The block whose terminator is being emitted, and whether on its
    /// poll's yield path.
    edge_from: (u32, bool),
    /// The GC-holding locals live across a safepoint (`emit_prelude`);
    /// `None`: every one.
    rooted: Option<HashSet<LocalId>>,
    /// The frame's root cell for a rooted local, besides its own slot (not
    /// arrays). Writes go to both; reads use the
    /// slot, which nothing else reaches, so loads survive calls (a root's
    /// object never moves).
    root_cells: HashMap<u32, String>,
    /// Rooted locals whose own slot is the root (no root cell).
    root_slots: HashSet<u32>,
    /// The locals a statement's collection or an entry poll sees; the rest
    /// of `rooted` only a back-edge poll's (`emit_prelude`).
    real_roots: Option<HashSet<LocalId>>,
    /// The rest, where they have a root cell: writes skip the cell, which a
    /// poll's yield path fills just for the yield (`poll_live`), in a
    /// frame of their own (`poll_frame`).
    poll_roots: HashSet<u32>,
    /// Block id → its `poll_roots` live at its back-edge poll.
    poll_live: HashMap<u32, Vec<LocalId>>,
    /// Where the frame-rooted locals die (their slots are cleared there),
    /// and each block's index in `f.blocks`.
    deaths: rokc_mir::Deaths,
    block_pos: HashMap<u32, usize>,
    /// `llvm.alloca`s hoisted to the entry block (`entry_alloca`), so a
    /// scratch slot used in a loop does not grow the stack.
    entry_allocas: String,
    /// `-g`: this function's subprogram, and its described locals
    /// (local id, `#di_var` alias, the declare's location).
    dbg: Option<crate::debug::FnDebug>,
    dbg_vars: Vec<(u32, String, String)>,
    /// Position of the last `Statement::Loc`: a failed check reports it.
    at: rokc_diagnostic::Span,
    /// Lent `Emitter::quiet`.
    quiet: std::sync::Arc<HashSet<FunctionId>>,
    /// Lent `Emitter::lists`.
    lists: std::sync::Arc<HashSet<u32>>,
    /// Lent `Emitter::list_alias`, and the locals with a `List` alias
    /// scope (`ListAlias::scoped`).
    list_alias: std::sync::Arc<crate::alias::ListAlias>,
    list_scopes: Vec<u32>,
    /// `rokc_mir::addressed`.
    addressed: HashSet<LocalId>,
    /// In `Emitter::reentrant`: polls on entry, or before its first
    /// safepoint on each path (`links`).
    entry_poll: bool,
    /// In `Emitter::entered`: the entry poll stays at entry.
    eager_poll: bool,
    /// Where the next rok call returning by address writes its result
    /// (`rok_call_args`): the target's slot instead of a temporary.
    sret_into: Option<String>,
    /// This thread's address of each runtime TLS slot used, by offset, and
    /// of the block at `u32::MAX`, computed once in the entry block.
    tls: HashMap<u32, String>,
    /// The hidden parameter holding the block's address (`FnSig::tls`).
    tls_arg: Option<String>,
    /// Functions whose thunk a function value of them needs.
    thunks: BTreeSet<FunctionId>,
}

/// Blocks with an edge to a block on the current DFS path (a loop
/// back-edge), found by an iterative DFS from the entry block.
pub fn back_edge_blocks(f: &MirFunction) -> HashSet<u32> {
    rokc_mir::back_edges(f).into_keys().collect()
}

/// Polling back edges by block, with their headers: all but those of short
/// loops (`rokc_mir::short_loops`) reaching no safepoint.
#[allow(clippy::implicit_hasher)]
pub fn polled_back_edges(
    f: &MirFunction,
    quiet: &HashSet<FunctionId>,
    pkgs: &crate::mangle::Pkgs,
    target: &Target,
) -> HashMap<u32, HashSet<u32>> {
    let mut edges = rokc_mir::back_edges(f);
    let unpolled: HashSet<u32> = rokc_mir::short_loops(f)
        .into_iter()
        .filter(|(_, body)| {
            !f.blocks.iter().any(|b| {
                let skip = rokc_mir::noreturn_call(b);
                body.contains(&b.id.0)
                    && b.stmts
                        .iter()
                        .enumerate()
                        .any(|(i, s)| Some(i) != skip && stmt_safepoint(s, quiet, pkgs, target))
            })
        })
        .map(|(h, _)| h)
        .collect();
    edges.retain(|_, hs| {
        hs.retain(|h| !unpolled.contains(h));
        !hs.is_empty()
    });
    edges
}

/// Can `s` reach a safepoint (a collection may run before it completes):
/// a call outside `quiet`, an allocation, a lazy static init, an MLIR op.
#[allow(clippy::implicit_hasher)]
pub fn stmt_safepoint(
    s: &Statement,
    quiet: &HashSet<FunctionId>,
    pkgs: &crate::mangle::Pkgs,
    target: &Target,
) -> bool {
    match s {
        Statement::Assign { rvalue, .. } => match rvalue {
            RValue::Call { callee, .. } => !quiet.contains(callee),
            RValue::HeapBox { ty, .. } => !target.any_inline(ty),
            RValue::StaticLoad { name, .. } | RValue::StaticAddr(name) => {
                pkgs.lazy_statics.contains_key(name)
            }
            RValue::CallIndirect { .. } | RValue::HeapAlloc { .. } | RValue::BufAlloc { .. } => {
                true
            }
            _ => false,
        },
        Statement::StaticStore { name, .. } => pkgs.lazy_statics.contains_key(name),
        _ => false,
    }
}

/// A shadow-stack frame: addresses of `rok_tls_shadow_stack_top` and of
/// `prev`, and the link stores other than `prev`'s load.
pub(crate) struct GcFrame {
    tls: String,
    prev: String,
    link: String,
}

/// Where a function links its frame and, if reentrant, polls:
/// before the first safepoint of each path, so paths reaching
/// none pay for neither.
#[derive(Default)]
pub(crate) struct Links {
    /// Block id → the statement the link goes before (`usize::MAX`:
    /// before the terminator).
    pub at: HashMap<u32, usize>,
    /// Blocks whose terminator runs with the frame linked.
    pub linked: HashSet<u32>,
}

/// `Links` for `f` from its safepoint statements (`sp`) and blocks ending
/// in a back-edge poll (`end`). A block is entered always linked or never:
/// one linked on some incoming paths links the others at their end. `None`
/// when the entry block is linked on entry (a back edge to it): link at
/// entry.
pub(crate) fn plan_links(
    f: &MirFunction,
    sp: impl Fn(usize, usize) -> bool,
    end: impl Fn(u32) -> bool,
) -> Option<Links> {
    let n = f.blocks.len();
    let pos: HashMap<u32, usize> = f
        .blocks
        .iter()
        .enumerate()
        .map(|(p, b)| (b.id.0, p))
        .collect();
    let mut preds = vec![Vec::new(); n];
    for (p, b) in f.blocks.iter().enumerate() {
        for s in rokc_mir::succs(b) {
            preds[pos[&s]].push(p);
        }
    }
    let first: Vec<Option<usize>> = f
        .blocks
        .iter()
        .enumerate()
        .map(|(p, b)| (0..b.stmts.len()).find(|&i| sp(p, i)))
        .collect();
    let ends: Vec<bool> = f.blocks.iter().map(|b| end(b.id.0)).collect();
    let mut force = vec![false; n];
    loop {
        let (mut inl, mut out) = (vec![false; n], vec![false; n]);
        let mut changed = true;
        while changed {
            changed = false;
            for p in 0..n {
                let i = preds[p].iter().any(|&q| out[q]);
                let o = i || first[p].is_some() || ends[p] || force[p];
                if (i, o) != (inl[p], out[p]) {
                    (inl[p], out[p]) = (i, o);
                    changed = true;
                }
            }
        }
        if inl.first() == Some(&true) {
            return None;
        }
        let mut grew = false;
        for p in (0..n).filter(|&p| inl[p]) {
            for &q in &preds[p] {
                if !out[q] {
                    force[q] = true;
                    grew = true;
                }
            }
        }
        if !grew {
            let mut links = Links::default();
            for (p, b) in f.blocks.iter().enumerate() {
                if out[p] {
                    links.linked.insert(b.id.0);
                    if !inl[p] {
                        links.at.insert(b.id.0, first[p].unwrap_or(usize::MAX));
                    }
                }
            }
            return Some(links);
        }
    }
}

/// Whether some statement of `f` can reach a safepoint (`stmt_safepoint`),
/// a never-returning call aside.
#[allow(clippy::implicit_hasher)]
pub fn body_safepoints(
    f: &MirFunction,
    quiet: &HashSet<FunctionId>,
    pkgs: &crate::mangle::Pkgs,
    target: &Target,
) -> bool {
    f.blocks.iter().any(|b| {
        let skip = rokc_mir::noreturn_call(b);
        b.stmts
            .iter()
            .enumerate()
            .any(|(i, s)| Some(i) != skip && stmt_safepoint(s, quiet, pkgs, target))
    })
}

/// Functions some call cycle may pass through.
///
/// Every such cycle polls. Members of a direct-call cycle,
/// callers of what this module can't see (indirect calls, C), functions
/// called from C or through a pointer, and `any` dispatchers; the rest
/// are only entered from callers that poll.
pub fn reentrant_functions(module: &MirModule) -> HashSet<FunctionId> {
    let index: HashMap<FunctionId, usize> = module
        .functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id, i))
        .collect();
    let mut out = HashSet::new();
    let mut succs: Vec<Vec<usize>> = Vec::with_capacity(module.functions.len());
    for f in &module.functions {
        let mut next = Vec::new();
        let mut opaque = f.facts.abi != Abi::Rok
            || f.facts.role == FnRole::AnyDispatcher
            || f.facts.export_name.is_some();
        for s in f.blocks.iter().flat_map(|b| &b.stmts) {
            let Statement::Assign { rvalue, .. } = s else {
                continue;
            };
            match rvalue {
                RValue::Call { callee, .. } => match index.get(callee) {
                    Some(&j) if module.functions[j].facts.abi != Abi::Rok => opaque = true,
                    Some(&j) => next.push(j),
                    None => opaque = true,
                },
                RValue::CallIndirect { .. } => opaque = true,
                RValue::FnAddr { callee, .. } => {
                    out.insert(*callee);
                }
                _ => {}
            }
        }
        if opaque {
            out.insert(f.id);
        }
        succs.push(next);
    }
    // Tarjan's strongly connected components, iteratively.
    let n = succs.len();
    let (mut idx, mut low) = (vec![usize::MAX; n], vec![0; n]);
    let (mut on, mut stack, mut next_idx) = (vec![false; n], Vec::new(), 0);
    for root in 0..n {
        if idx[root] != usize::MAX {
            continue;
        }
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(top) = work.last_mut() {
            let (v, k) = *top;
            if k == 0 {
                idx[v] = next_idx;
                low[v] = next_idx;
                next_idx += 1;
                stack.push(v);
                on[v] = true;
            }
            if let Some(&w) = succs[v].get(k) {
                top.1 += 1;
                if idx[w] == usize::MAX {
                    work.push((w, 0));
                } else if on[w] {
                    low[v] = low[v].min(idx[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(u, _)) = work.last() {
                low[u] = low[u].min(low[v]);
            }
            if low[v] == idx[v] {
                let mut scc = Vec::new();
                while let Some(w) = stack.pop() {
                    on[w] = false;
                    scc.push(w);
                    if w == v {
                        break;
                    }
                }
                if scc.len() > 1 || succs[v].contains(&v) {
                    out.extend(scc.iter().map(|&w| module.functions[w].id));
                }
            }
        }
    }
    out
}

/// Functions entered other than by a direct rok call: from C, through a
/// pointer, exported, or the `any` dispatcher. They poll at entry; other
/// reentrant functions poll before their first safepoint.
pub fn entered_functions(module: &MirModule) -> HashSet<FunctionId> {
    let mut out: HashSet<FunctionId> = module
        .functions
        .iter()
        .filter(|f| {
            f.facts.abi != Abi::Rok
                || f.facts.role == FnRole::AnyDispatcher
                || f.facts.export_name.is_some()
        })
        .map(|f| f.id)
        .collect();
    for s in module
        .functions
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.stmts)
    {
        if let Statement::Assign {
            rvalue: RValue::FnAddr { callee, .. },
            ..
        } = s
        {
            out.insert(*callee);
        }
    }
    out
}

/// Functions that never reach a safepoint.
///
/// Only short loops, no allocation or indirect call, and calls only to
/// such functions (never-returning ones aside). Least fixpoint, so never
/// recursive. They need no frame or entry poll: they run for bounded time
/// and no collection sees their references.
pub fn quiet_functions(
    module: &MirModule,
    pkgs: &crate::mangle::Pkgs,
    target: &Target,
) -> HashSet<FunctionId> {
    let mut pending: HashMap<FunctionId, usize> = HashMap::new();
    let mut callers: HashMap<FunctionId, Vec<FunctionId>> = HashMap::new();
    let all: HashSet<FunctionId> = module.functions.iter().map(|f| f.id).collect();
    // A prebuilt function's inline copy has its body here too.
    let candidates = module.functions.iter().filter(|f| {
        let body = f.facts.linkage == Linkage::Defined
            || (f.facts.linkage == Linkage::Prebuilt && !f.blocks.is_empty());
        body && f.facts.role != FnRole::AnyDispatcher && {
            let short = rokc_mir::short_loops(f);
            rokc_mir::back_edges(f)
                .values()
                .flatten()
                .all(|h| short.contains_key(h))
        }
    });
    for f in candidates {
        // Every call counts as quiet here; the callees are checked below.
        if body_safepoints(f, &all, pkgs, target) {
            continue;
        }
        let mut callees: HashSet<FunctionId> = HashSet::new();
        for b in &f.blocks {
            let skip = rokc_mir::noreturn_call(b);
            for (i, s) in b.stmts.iter().enumerate() {
                match s {
                    Statement::Assign {
                        rvalue: RValue::Call { callee, .. },
                        ..
                    } if Some(i) != skip => {
                        callees.insert(*callee);
                    }
                    _ => {}
                }
            }
        }
        pending.insert(f.id, callees.len());
        for c in callees {
            callers.entry(c).or_default().push(f.id);
        }
    }
    for f in &module.functions {
        if f.facts.no_safepoint {
            pending.insert(f.id, 0);
        }
    }
    let mut quiet = HashSet::new();
    let mut ready: Vec<FunctionId> = pending
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect();
    while let Some(id) = ready.pop() {
        quiet.insert(id);
        for c in callers.get(&id).into_iter().flatten() {
            if let Some(n) = pending.get_mut(c) {
                *n -= 1;
                if *n == 0 {
                    ready.push(*c);
                }
            }
        }
    }
    quiet
}

impl<'a> FnEmit<'a> {
    fn new(
        f: &'a MirFunction,
        target: &'a Target,
        fns: &'a HashMap<FunctionId, FnSig>,
        str_globals: std::collections::BTreeMap<Vec<u8>, String>,
        pkgs: &'a crate::mangle::Pkgs,
        sources: &'a SourceMap,
    ) -> Self {
        // Block params are SSA values; everything else gets a stack slot.
        let mut block_param_set: HashSet<u32> = HashSet::new();
        for b in &f.blocks {
            for p in &b.params {
                block_param_set.insert(p.0);
            }
        }
        let mut local_names = vec![None; f.locals.len()];
        for (i, p) in f.params.iter().enumerate() {
            local_names[p.0 as usize] = Some(format!("%arg{i}"));
        }
        Self {
            f,
            target,
            pw: target.pointer_width,
            body: String::new(),
            local_names,
            slot_names: vec![None; f.locals.len()],
            union_scratch: HashMap::new(),
            at: f.span,
            block_param_set,
            next_ssa: 0,
            fns,
            str_globals,
            errors: Vec::new(),
            needs: BTreeSet::new(),
            gc_types: Vec::new(),
            gc_frame: None,
            poll_frame: None,
            links: None,
            gc_param_slots: HashMap::new(),
            next_check: 0,
            pkgs,
            sources,
            overflow_checks: true,
            gc_type_base: USER_TYPE_BASE,
            gc_pkg: None,
            gc_syms: BTreeSet::new(),
            back_edges: HashMap::new(),
            reentry: HashSet::new(),
            edge_from: (0, false),
            rooted: None,
            root_cells: HashMap::new(),
            root_slots: HashSet::new(),
            real_roots: None,
            poll_roots: HashSet::new(),
            poll_live: HashMap::new(),
            deaths: rokc_mir::Deaths::default(),
            block_pos: f
                .blocks
                .iter()
                .enumerate()
                .map(|(i, b)| (b.id.0, i))
                .collect(),
            entry_allocas: String::new(),
            dbg: None,
            dbg_vars: Vec::new(),
            quiet: std::sync::Arc::default(),
            lists: std::sync::Arc::default(),
            list_alias: std::sync::Arc::default(),
            list_scopes: Vec::new(),
            addressed: rokc_mir::addressed(f),
            entry_poll: true,
            eager_poll: true,
            sret_into: None,
            tls: HashMap::new(),
            tls_arg: None,
            thunks: BTreeSet::new(),
        }
    }

    /// Address of TLS slot `slot` (803), off `tls_base`, computed once per
    /// function so LLVM keeps it in a register.
    fn tls_addr(&mut self, slot: Slot) -> String {
        self.needs.insert("rok_tls");
        let off = slot.offset(self.target.pointer_width / 8);
        if let Some(a) = self.tls.get(&off) {
            return a.clone();
        }
        let base = self.tls_base();
        let a = self.fresh_value();
        let text = format!(
            "    {a} = llvm.getelementptr inbounds {base}[{off}] : (!llvm.ptr) -> !llvm.ptr, i8\n"
        );
        self.tls_text(&text, off, &a);
        a
    }

    /// The address of `rok_tls` on this thread: the caller's (`tls_arg`),
    /// else one `threadlocal.address` in the entry block.
    fn tls_base(&mut self) -> String {
        self.needs.insert("rok_tls");
        if let Some(a) = &self.tls_arg {
            return a.clone();
        }
        if let Some(b) = self.tls.get(&u32::MAX) {
            return b.clone();
        }
        let [g, b] = std::array::from_fn(|_| self.fresh_value());
        let text = format!(
            "    {g} = llvm.mlir.addressof @rok_tls : !llvm.ptr
    {b} = \"llvm.intr.threadlocal.address\"({g}) : (!llvm.ptr) -> !llvm.ptr
"
        );
        self.tls_text(&text, u32::MAX, &b);
        b
    }

    /// `text` defining TLS address `a` (`tls` key `key`) in the entry
    /// block, remembered.
    fn tls_text(&mut self, text: &str, key: u32, a: &str) {
        self.entry_allocas.push_str(text);
        self.tls.insert(key, a.to_owned());
    }

    /// Whether a collection can run while this function is active
    /// (`stmt_safepoint`).
    fn reaches_safepoint(&self) -> bool {
        body_safepoints(self.f, &self.quiet, self.pkgs, self.target)
    }

    /// Scratch to reinterpret aggregate `ty` as its coerced C type, big
    /// enough for either (coerced types are at most 16 bytes or an HFA).
    fn coerce_slot(&mut self, ty: &MirTy) -> String {
        // Room for a coerced type rounded up to whole words.
        let size = self.target.size_align(ty).0.next_multiple_of(8).max(16);
        self.entry_alloca(&format!("!llvm.array<{size} x i8>"), 16)
    }

    /// Arguments of a rok-convention call and the address of a by-ref
    /// result. A by-ref argument is the operand's own slot (the callee
    /// copies it first), else a temporary.
    fn rok_call_args(
        &mut self,
        args: &[Operand],
        tys: &[MirTy],
        ret: &MirTy,
    ) -> (Vec<String>, Option<String>) {
        let mut vals = Vec::new();
        let into = self.sret_into.take();
        let sret = rok_by_ref(ret, self.target).then(|| {
            into.unwrap_or_else(|| {
                let ty = mlir_type(ret, self.target);
                let align = self.target.size_align(ret).1.max(8);
                self.entry_alloca(&ty, align)
            })
        });
        vals.extend(sret.clone());
        for (op, ty) in args.iter().zip(tys) {
            if !rok_by_ref(ty, self.target) {
                vals.push(self.materialize_operand(op, ty));
                continue;
            }
            if let Operand::Copy(id) = op {
                if self.local_ty(*id) == *ty {
                    if let Some(slot) = self.slot_of(*id) {
                        vals.push(slot);
                        continue;
                    }
                }
            }
            let t = mlir_type(ty, self.target);
            let align = self.target.size_align(ty).1.max(8);
            let tmp = self.entry_alloca(&t, align);
            if !self.memcpy_operand(&tmp, op, ty) {
                let v = self.materialize_operand(op, ty);
                let _ = writeln!(self.body, "    llvm.store {v}, {tmp} : {t}, !llvm.ptr");
            }
            vals.push(tmp);
        }
        (vals, sret)
    }

    /// A `str` argument for a runtime function codegen calls itself, and its
    /// type: Windows x64 and wasm pass the two-word struct by reference
    ///, so a pointer to a copy there.
    fn runtime_str(&mut self, v: &str) -> (String, String) {
        let ty = mlir_prim(PrimTy::Str, self.target.pointer_width);
        if !self.target.c_abi.pair_by_ref() {
            return (v.to_owned(), ty);
        }
        let slot = self.entry_alloca(&ty, self.target.pointer_width / 8);
        let _ = writeln!(self.body, "    llvm.store {v}, {slot} : {ty}, !llvm.ptr");
        (slot, "!llvm.ptr".to_owned())
    }

    fn entry_alloca(&mut self, ty: &str, align: u32) -> String {
        let [one, slot] = std::array::from_fn(|_| self.fresh_value());
        let _ = write!(
            self.entry_allocas,
            "    {one} = llvm.mlir.constant(1 : i32) : i32
    {slot} = llvm.alloca {one} x {ty} {{alignment = {align} : i64}} : (i32) -> !llvm.ptr
"
        );
        slot
    }

    /// Before touching a lazily initialized static: an acquire load of
    /// its state byte, and `@sym.init()` unless it reads 2 (ready).
    fn ensure_static_init(&mut self, name: &str) {
        if !self.pkgs.lazy_statics.contains_key(name) {
            return;
        }
        let sym = self.pkgs.static_sym(name);
        let n = self.next_check;
        self.next_check += 1;
        let [a, s, two, ready, at] = std::array::from_fn(|_| self.fresh_value());
        let str_ty = mlir_prim(PrimTy::Str, self.pw);
        let _ = write!(
            self.body,
            "    {a} = llvm.mlir.addressof @{sym}.state : !llvm.ptr
    {s} = llvm.load {a} atomic acquire {{alignment = 1 : i64}} : !llvm.ptr -> i8
    {two} = arith.constant 2 : i8
    {ready} = arith.cmpi eq, {s}, {two} : i8
    cf.cond_br {ready}, ^rok_sready{n}, ^rok_sinit{n}
  ^rok_sinit{n}:
"
        );
        self.str_literal_into(&at, &self.sources.location(self.at));
        let _ = write!(
            self.body,
            "    func.call @{sym}.init({at}) : ({str_ty}) -> ()
    cf.br ^rok_sready{n}
  ^rok_sready{n}:
"
        );
    }

    /// Write barrier before storing a reference
    /// into `obj` at `slot` (`len` bytes): the fast path tests the header's
    /// `UNLOGGED | CARDED` bits (old objects only). `UNLOGGED` calls
    /// `rok_gc_wb_slow(obj)`; `CARDED` (large old objects) marks the cards
    /// of the slot's first and last byte inline. Concurrent marking sets
    /// both: the slow path snapshots, the card remembers. Not a safepoint;
    /// emission continues in a fresh block.
    fn emit_write_barrier(&mut self, obj: &Operand, slot: &str, len: u32) {
        let pw = self.pw;
        let obj_v = self.materialize_operand(obj, &MirTy::GcRef);
        let n = self.next_check;
        self.next_check += 1;
        self.needs.insert("rok_gc_wb_slow");
        let hp = obj_v.clone();
        let [
            oi,
            fp,
            flags,
            bits,
            masked,
            zero,
            set,
            ubit,
            umask,
            unlogged,
        ] = std::array::from_fn(|_| self.fresh_value());
        let [
            cbit,
            cmask,
            carded,
            ctp,
            ct,
            si,
            off,
            last,
            sh,
            i0,
            e1,
            i1,
            c0,
            c1,
            one,
        ] = std::array::from_fn(|_| self.fresh_value());
        let mask = FLAG_UNLOGGED | FLAG_CARDED;
        let word = pw / 8;
        let tail = len.max(1) - 1;
        let _ = write!(
            self.body,
            "    {fp} = llvm.getelementptr {hp}[{GC_FLAGS_OFFSET}] : (!llvm.ptr) -> !llvm.ptr, i8
    {flags} = llvm.load {fp} atomic monotonic {{alignment = 4 : i64}} : !llvm.ptr -> i32
    {bits} = arith.constant {mask} : i32
    {masked} = arith.andi {flags}, {bits} : i32
    {zero} = arith.constant 0 : i32
    {set} = arith.cmpi ne, {masked}, {zero} : i32
    cf.cond_br {set}, ^rok_wb{n}, ^rok_wbok{n}
  ^rok_wb{n}:
    {ubit} = arith.constant {FLAG_UNLOGGED} : i32
    {umask} = arith.andi {flags}, {ubit} : i32
    {unlogged} = arith.cmpi ne, {umask}, {zero} : i32
    cf.cond_br {unlogged}, ^rok_wbslow{n}, ^rok_wbcq{n}
  ^rok_wbslow{n}:
    llvm.call @rok_gc_wb_slow({obj_v}) {{tbaa = [#rok_tbaa_ref]}} : (!llvm.ptr) -> ()
    cf.br ^rok_wbcq{n}
  ^rok_wbcq{n}:
    {cbit} = arith.constant {FLAG_CARDED} : i32
    {cmask} = arith.andi {flags}, {cbit} : i32
    {carded} = arith.cmpi ne, {cmask}, {zero} : i32
    cf.cond_br {carded}, ^rok_wbcard{n}, ^rok_wbok{n}
  ^rok_wbcard{n}:
    {ctp} = llvm.getelementptr {hp}[-{word}] : (!llvm.ptr) -> !llvm.ptr, i8
    {ct} = llvm.load {ctp} {{tbaa = [#rok_tbaa_card]}} : !llvm.ptr -> !llvm.ptr
    {si} = llvm.ptrtoint {slot} : !llvm.ptr to i{pw}
    {oi} = llvm.ptrtoint {hp} : !llvm.ptr to i{pw}
    {off} = arith.subi {si}, {oi} : i{pw}
    {last} = arith.constant {tail} : i{pw}
    {sh} = arith.constant {CARD_SHIFT} : i{pw}
    {i0} = arith.shrui {off}, {sh} : i{pw}
    {e1} = arith.addi {off}, {last} : i{pw}
    {i1} = arith.shrui {e1}, {sh} : i{pw}
    {c0} = llvm.getelementptr {ct}[{i0}] : (!llvm.ptr, i{pw}) -> !llvm.ptr, i8
    {c1} = llvm.getelementptr {ct}[{i1}] : (!llvm.ptr, i{pw}) -> !llvm.ptr, i8
    {one} = arith.constant 1 : i8
    llvm.store {one}, {c0} {{tbaa = [#rok_tbaa_card]}} : i8, !llvm.ptr
    llvm.store {one}, {c1} {{tbaa = [#rok_tbaa_card]}} : i8, !llvm.ptr
    cf.br ^rok_wbok{n}
  ^rok_wbok{n}:
"
        );
    }

    /// Safepoint poll: relaxed load of
    /// `rok_tls_yield_flag`, `rok_runtime_yield` when set; continues in a
    /// fresh block. `back`: the back-edge poll ending that block.
    fn emit_poll(&mut self, back: Option<u32>) {
        let n = self.emit_poll_split(back);
        let _ = writeln!(self.body, "    cf.br ^rok_run{n}\n  ^rok_run{n}:");
    }

    /// `emit_poll` with the yield block left open for the caller's
    /// terminator; returns `n` of the `^rok_run{n}` block to open after.
    ///
    /// A back-edge poll roots its `poll_live` locals and, where the frame
    /// is not linked, links it on the yield path only: the loop pays for
    /// neither.
    fn emit_poll_split(&mut self, back: Option<u32>) -> u32 {
        self.needs.insert("rok_runtime_yield");
        let a = self.tls_addr(tls_abi::YIELD_FLAG);
        let n = self.next_check;
        self.next_check += 1;
        let [flag, zero, set] = std::array::from_fn(|_| self.fresh_value());
        let _ = write!(
            self.body,
            "    {flag} = llvm.load {a} atomic monotonic {{alignment = 1 : i64}} : !llvm.ptr -> i8
    {zero} = arith.constant 0 : i8
    {set} = arith.cmpi ne, {flag}, {zero} : i8
    cf.cond_br {set}, ^rok_yield{n}, ^rok_run{n}
  ^rok_yield{n}:
"
        );
        // Every poll root's cell is written here, dead ones null: nothing
        // else touches them.
        if let Some(b) = back.filter(|_| self.poll_frame.is_some()) {
            let live = self.poll_live.get(&b).cloned().unwrap_or_default();
            let mut ids: Vec<u32> = self.poll_roots.iter().copied().collect();
            ids.sort_unstable();
            for id in ids {
                let l = LocalId(id);
                let cell = self.root_cells[&id].clone();
                if !live.contains(&l) {
                    self.emit_zero(&cell, &self.local_ty(l), "#rok_tbaa_gcroot");
                    continue;
                }
                let v = self.read_local(l);
                let ty = mlir_type(&self.local_ty(l), self.target);
                let _ = writeln!(
                    self.body,
                    "    llvm.store {v}, {cell} {{tbaa = [#rok_tbaa_gcroot]}} : {ty}, !llvm.ptr"
                );
            }
        }
        let link = back.is_some_and(|b| {
            self.gc_frame.is_some() && self.links.as_ref().is_some_and(|l| !l.linked.contains(&b))
        });
        let push = back.is_some() && self.poll_frame.is_some();
        let head = (link || push).then(|| {
            let fr = self.gc_frame.as_ref().or(self.poll_frame.as_ref()).unwrap();
            let (tls, h) = (fr.tls.clone(), self.fresh_value());
            let _ = writeln!(
                self.body,
                "    {h} = llvm.load {tls} : !llvm.ptr -> !llvm.ptr"
            );
            (tls, h)
        });
        if link {
            self.emit_link();
        }
        if push {
            let p = self.fresh_value();
            let fr = self.poll_frame.as_ref().unwrap();
            let _ = write!(
                self.body,
                "    {p} = llvm.load {} : !llvm.ptr -> !llvm.ptr\n    llvm.store {p}, {} : !llvm.ptr, !llvm.ptr\n{}",
                fr.tls, fr.prev, fr.link
            );
        }
        self.body
            .push_str("    llvm.call @rok_runtime_yield() {tbaa = [#rok_tbaa_ref]} : () -> ()\n");
        if let Some((tls, h)) = head {
            let _ = writeln!(
                self.body,
                "    llvm.store {h}, {tls} : !llvm.ptr, !llvm.ptr"
            );
        }
        n
    }

    /// Into (`enter`) or out of a C call: the inline fast path
    /// of `rok_enter_native` / `rok_leave_native`. Store the state, then
    /// read the yield flag (both `seq_cst`, since a stopper raises the flag
    /// and then reads the state); a raised flag takes the slow path.
    fn emit_native_edge(&mut self, enter: bool) {
        let (state, slow) = if enter {
            (2, "rok_enter_native")
        } else {
            (0, "rok_leave_native")
        };
        self.needs.insert(slow);
        let sa = self.tls_addr(tls_abi::MUTATOR_STATE);
        let fa = self.tls_addr(tls_abi::YIELD_FLAG);
        let n = self.next_check;
        self.next_check += 1;
        let [st, flag, zero, set] = std::array::from_fn(|_| self.fresh_value());
        let _ = write!(
            self.body,
            "    {st} = llvm.mlir.constant({state} : i8) : i8
    llvm.store {st}, {sa} atomic seq_cst {{alignment = 1 : i64}} : i8, !llvm.ptr
    {flag} = llvm.load {fa} atomic seq_cst {{alignment = 1 : i64}} : !llvm.ptr -> i8
    {zero} = arith.constant 0 : i8
    {set} = arith.cmpi ne, {flag}, {zero} : i8
    cf.cond_br {set}, ^rok_nslow{n}, ^rok_nfast{n}
  ^rok_nslow{n}:
    func.call @{slow}() : () -> ()
    cf.br ^rok_nfast{n}
  ^rok_nfast{n}:
"
        );
    }

    fn intern_string(&mut self, s: &str) -> String {
        self.intern_bytes(s.as_bytes())
    }

    /// The constant global holding `bytes` (and a NUL), shared by equal
    /// string literals and data.
    fn intern_bytes(&mut self, bytes: &[u8]) -> String {
        self.str_globals
            .entry(bytes.to_vec())
            .or_insert_with(|| crate::mangle::str_global(bytes))
            .clone()
    }

    fn is_block_param(&self, id: LocalId) -> bool {
        self.block_param_set.contains(&id.0)
    }

    /// Locals built directly in a by-address result (`%arg0`): the
    /// one every return returns, or a temporary only its block's return
    /// reads (`once`). Not parameters, rooted locals, address-taken ones or
    /// ones whose scope ends first: the caller's result is no root and only
    /// this function sees it.
    fn result_locals(&self, once: &[bool]) -> HashSet<LocalId> {
        let f = self.f;
        if !rok_by_ref(&f.ret_ty, self.target) {
            return HashSet::new();
        }
        let mut uses = vec![0u32; f.locals.len()];
        let mut dead = self.addressed.clone();
        for b in &f.blocks {
            for s in &b.stmts {
                if let Statement::StorageDead(l) = s {
                    dead.insert(*l);
                }
                for u in rokc_mir::stmt_uses(s).0 {
                    uses[u.0 as usize] += 1;
                }
            }
            for u in rokc_mir::term_uses(&b.terminator) {
                uses[u.0 as usize] += 1;
            }
        }
        let returned: Vec<Option<LocalId>> = f
            .blocks
            .iter()
            .filter_map(|b| match &b.terminator {
                Terminator::Return(Some(Operand::Copy(l))) => Some(Some(*l)),
                Terminator::Return(_) => Some(None),
                _ => None,
            })
            .collect();
        let only = match returned.first() {
            Some(Some(r)) if returned.iter().all(|x| *x == Some(*r)) => Some(*r),
            _ => None,
        };
        let temps = returned
            .iter()
            .flatten()
            .filter(|t| once[t.0 as usize] && uses[t.0 as usize] == 1);
        only.into_iter()
            .chain(temps.copied())
            .filter(|l| {
                !f.params.contains(l)
                    && !self.is_block_param(*l)
                    && !dead.contains(l)
                    && f.locals[l.0 as usize].ty == f.ret_ty
                    && (self.target.root_offsets(&f.ret_ty).is_empty() || !self.is_rooted(*l))
            })
            .collect()
    }

    /// Whether local `id` needs a GC root (`rooted`).
    fn is_rooted(&self, id: LocalId) -> bool {
        self.rooted.as_ref().is_none_or(|r| r.contains(&id))
    }

    /// Entry-block prelude: `llvm.alloca` slots for slotted locals, union
    /// scratch, the frame, then arguments stored into their slots.
    fn emit_prelude(&mut self) {
        let one = self.fresh_value();
        let _ = writeln!(self.body, "    {one} = llvm.mlir.constant(1 : i32) : i32");
        // A value read only later in its own block needs no slot, unless it
        // holds GC references, is an array, or `-g` describes it.
        let ssa = rokc_mir::block_local(self.f);
        // Link and poll lazily (`plan_links`), except for functions
        // entered from C or a pointer.
        if !(self.entry_poll && self.eager_poll) {
            let (quiet, pkgs, target) = (&*self.quiet, self.pkgs, self.target);
            let f = self.f;
            let skip: Vec<Option<usize>> = f.blocks.iter().map(rokc_mir::noreturn_call).collect();
            self.links = plan_links(
                f,
                |p, i| {
                    skip[p] != Some(i) && stmt_safepoint(&f.blocks[p].stmts[i], quiet, pkgs, target)
                },
                // A back-edge poll links on its yield path only.
                |_| false,
            );
        }
        // Where a lazy entry poll goes: before these statements, and at
        // the end of these blocks.
        let mut poll_stmts: HashSet<*const Statement> = HashSet::new();
        let mut poll_ends: HashSet<u32> = HashSet::new();
        if let (Some(links), true) = (&self.links, self.entry_poll) {
            for b in &self.f.blocks {
                match links.at.get(&b.id.0) {
                    Some(&usize::MAX) => {
                        poll_ends.insert(b.id.0);
                    }
                    Some(&i) => {
                        poll_stmts.insert(&raw const b.stmts[i]);
                    }
                    None => {}
                }
            }
        }
        // GC-holding locals no collection sees while live (`live_across`)
        // need no root and may stay SSA values. `back`: back-edge polls
        // count too.
        let roots = |back: bool| {
            let (quiet, pkgs, target) = (&*self.quiet, self.pkgs, self.target);
            rokc_mir::live_across(
                self.f,
                |l| {
                    !target
                        .root_offsets(&self.f.locals[l.0 as usize].ty)
                        .is_empty()
                },
                |s| {
                    // A poll before it holds its operands too.
                    if poll_stmts.contains(&std::ptr::from_ref(s)) {
                        return Some(true);
                    }
                    stmt_safepoint(s, quiet, pkgs, target).then(|| match s {
                        // A rok function roots the arguments it uses
                        // after a collection itself.
                        Statement::Assign {
                            rvalue: RValue::Call { callee, .. },
                            ..
                        } => self
                            .fns
                            .get(callee)
                            .is_none_or(|g| g.runtime || g.c.is_some()),
                        _ => true,
                    })
                },
                |b| back && self.back_edges.contains_key(&b.0) || poll_ends.contains(&b.0),
                self.entry_poll && self.links.is_none(),
            )
        };
        self.real_roots = Some(roots(false));
        self.rooted = Some(roots(true));
        let entry = self.f.blocks.first().map(|b| b.id.0);
        self.reentry = self.back_edges.values().flatten().copied().collect();
        self.reentry.retain(|h| Some(*h) != entry);
        let result = self.result_locals(&ssa);
        for local in &self.f.locals {
            if self.is_block_param(local.id) || !is_slotted(&local.ty) {
                continue;
            }
            if result.contains(&local.id) {
                self.slot_names[local.id.0 as usize] = Some("%arg0".to_owned());
                continue;
            }
            if ssa[local.id.0 as usize]
                && !matches!(local.ty, MirTy::Array(..))
                && !self.in_memory(&local.ty)
                && (self.target.root_offsets(&local.ty).is_empty() || !self.is_rooted(local.id))
                && (self.dbg.is_none()
                    || local
                        .name
                        .as_deref()
                        .is_none_or(|n| !rokc_mir::debug_visible(n)))
            {
                continue;
            }
            let ty = mlir_type(&local.ty, self.target);
            let slot = format!("%p{}", local.id.0);
            let align = slot_align(&local.ty, self.target);
            let _ = writeln!(
                self.body,
                "    {slot} = llvm.alloca {one} x {ty}{align} : (i32) -> !llvm.ptr"
            );
            self.slot_names[local.id.0 as usize] = Some(slot);
        }
        // Scratch for aggregate payload bitcasts, in the entry block so a
        // bitcast in a loop does not grow the stack.
        for b in &self.f.blocks {
            for st in &b.stmts {
                let Statement::Assign {
                    rvalue: RValue::Bitcast { from_ty, to_ty, .. },
                    ..
                } = st
                else {
                    continue;
                };
                let Some(union_ty) = [from_ty, to_ty]
                    .into_iter()
                    .find(|t| matches!(t, MirTy::Array(_, _) | MirTy::Union(..)))
                else {
                    continue;
                };
                let ty = mlir_type(union_ty, self.target);
                if self.union_scratch.contains_key(&ty) {
                    continue;
                }
                let slot = self.fresh_value();
                let align = self.target.size_align(union_ty).1;
                let _ = writeln!(
                    self.body,
                    "    {slot} = llvm.alloca {one} x {ty} {{alignment = {align} : i64}} : (i32) -> !llvm.ptr"
                );
                self.union_scratch.insert(ty, slot);
            }
        }
        for (l, var, at) in std::mem::take(&mut self.dbg_vars) {
            if let Some(slot) = self.slot_names[l as usize].clone() {
                let _ = writeln!(
                    self.body,
                    "    llvm.intr.dbg.declare {var} = {slot} : !llvm.ptr {at}"
                );
            }
        }
        self.emit_gc_frame();
        let first = usize::from(rok_by_ref(&self.f.ret_ty, self.target));
        for (i, p) in self.f.params.iter().enumerate() {
            let local_ty = self.f.locals[p.0 as usize].ty.clone();
            let mut arg = format!("%arg{}", i + first);
            if rok_by_ref(&local_ty, self.target) {
                // A copy of the caller's value, taken before anything
                // runs, slot to slot (see `Statement::Assign`).
                if let Some(slot) = self.slot_names[p.0 as usize].clone() {
                    let a = self.target.size_align(&local_ty).1;
                    self.emit_memcpy(&slot, &arg, &local_ty, a);
                    continue;
                }
                let v = self.fresh_value();
                let ty = mlir_type(&local_ty, self.target);
                let _ = writeln!(self.body, "    {v} = llvm.load {arg} : !llvm.ptr -> {ty}");
                arg = v;
            }
            let arg = self.buf_ref_facts(arg, &local_ty);
            let Some(slot) = self.slot_names[p.0 as usize].clone() else {
                self.local_names[p.0 as usize] = Some(arg);
                continue;
            };
            let ty = mlir_type(&local_ty, self.target);
            let tag = self.slot_tag(*p);
            let _ = writeln!(
                self.body,
                "    llvm.store {arg}, {slot} {{tbaa = [{tag}]}} : {ty}, !llvm.ptr"
            );
            if let Some(cell) = self
                .root_cells
                .get(&p.0)
                .filter(|_| !self.poll_roots.contains(&p.0))
            {
                let _ = writeln!(
                    self.body,
                    "    llvm.store {arg}, {cell} {{tbaa = [#rok_tbaa_gcroot]}} : {ty}, !llvm.ptr"
                );
            }
        }
        // Entry poll, once the frame roots the arguments: only where a call
        // cycle can re-enter (`reentrant_functions`) without a loop poll, and
        // only if a safepoint is reachable (recursion always reaches one).
        if self.entry_poll && self.links.is_none() && self.reaches_safepoint() {
            self.emit_poll(None);
        }
    }

    /// Link the frame where `links` says, before statement `i` of block `b`
    /// (`usize::MAX`: the terminator); reentrant functions poll there too.
    fn link_before(&mut self, b: u32, i: usize) {
        if self.links.as_ref().and_then(|l| l.at.get(&b)) == Some(&i) {
            self.emit_link();
            if self.entry_poll {
                self.emit_poll(None);
            }
        }
    }

    /// Shadow-stack frame for a function holding GC references
    /// that can reach a safepoint: `{prev, count, slots[count]}`, each slot
    /// the address of a word that may hold a reference
    /// (`Target::root_offsets`) in a local's or block param's slot. Slots
    /// are zeroed so the collector never reads garbage. A rooted local's
    /// slot is cleared after its last use (`rokc_mir::deaths`) so it stops
    /// retaining garbage.
    fn emit_gc_frame(&mut self) {
        // A loop's back-edge poll can stop this thread for a collection
        // mid-function, like a call.
        if self.back_edges.is_empty() && !self.reaches_safepoint() {
            return;
        }
        let one = self.fresh_value();
        let mut header = format!("    {one} = llvm.mlir.constant(1 : i32) : i32\n");
        let mut roots = Vec::new();
        let mut polled = Vec::new();
        let unassigned = match (&self.rooted, &self.real_roots) {
            (Some(all), Some(real)) => rokc_mir::maybe_unassigned(
                self.f,
                |l| all.contains(&l) && !real.contains(&l),
                |b| self.back_edges.contains_key(&b.0),
            ),
            _ => HashSet::new(),
        };
        let mut rooted = HashSet::new();
        let addressed = rokc_mir::addressed(self.f);
        // `#[track_caller]`'s location is always a static literal.
        let caller = self.f.params.last().filter(|_| self.f.facts.track_caller);
        for local in &self.f.locals {
            let offsets = self.target.root_offsets(&local.ty);
            if offsets.is_empty() || caller == Some(&local.id) || !self.is_rooted(local.id) {
                continue;
            }
            let id = local.id.0;
            let ty = mlir_type(&local.ty, self.target);
            let poll_only = self
                .real_roots
                .as_ref()
                .is_some_and(|r| !r.contains(&local.id));
            let slot = if self.is_block_param(local.id) {
                let s = format!("%gp{id}");
                let align = slot_align(&local.ty, self.target);
                let _ = writeln!(
                    header,
                    "    {s} = llvm.alloca {one} x {ty}{align} : (i32) -> !llvm.ptr"
                );
                // Rooted on every entry: on a yield path its SSA value
                // might not dominate.
                self.gc_param_slots.insert(id, s.clone());
                s
            } else if let Some(slot) = self.slot_names[id as usize].clone() {
                // An addressed local's slot is the one pointers reach.
                if self.rooted.is_some()
                    && !matches!(local.ty, MirTy::Array(..))
                    && !self.in_memory(&local.ty)
                    && !addressed.contains(&local.id)
                {
                    let cell = format!("%rc{id}");
                    let align = slot_align(&local.ty, self.target);
                    let _ = writeln!(
                        header,
                        "    {cell} = llvm.alloca {one} x {ty}{align} : (i32) -> !llvm.ptr"
                    );
                    self.root_cells.insert(id, cell.clone());
                    if poll_only {
                        self.poll_roots.insert(id);
                    }
                    cell
                } else {
                    self.root_slots.insert(id);
                    slot
                }
            } else {
                continue; // unslotted: never holds a value across a call
            };
            rooted.insert(id);
            // A poll root's cell is written only on a yield path, from its
            // own slot: one some path reaches a poll without assigning
            // starts null, so a yield path never roots an undefined word.
            let zero = if !self.poll_roots.contains(&id) {
                Some((slot.clone(), "#rok_tbaa_gcroot"))
            } else if unassigned.contains(&local.id) {
                Some((
                    self.slot_names[id as usize].clone().unwrap(),
                    self.slot_tag(local.id),
                ))
            } else {
                None
            };
            if let Some((zero, tag)) = zero {
                let body = std::mem::take(&mut self.body);
                self.emit_zero(&zero, &local.ty, tag);
                header.push_str(&std::mem::replace(&mut self.body, body));
            }
            let into = if self.poll_roots.contains(&id) {
                &mut polled
            } else {
                &mut roots
            };
            for off in offsets {
                if off == 0 {
                    into.push(slot.clone());
                    continue;
                }
                let a = self.fresh_value();
                let _ = writeln!(
                    header,
                    "    {a} = llvm.getelementptr {slot}[{off}] : (!llvm.ptr) -> !llvm.ptr, i8"
                );
                into.push(a);
            }
        }
        if roots.is_empty() && polled.is_empty() {
            return;
        }
        if !roots.is_empty() {
            self.gc_frame = Some(self.frame(&mut header, &one, &roots));
        }
        if !polled.is_empty() {
            self.poll_frame = Some(self.frame(&mut header, &one, &polled));
        }
        self.body.push_str(&header);
        let polled = &self.poll_roots;
        self.poll_live = rokc_mir::live_out_of(
            self.f,
            |l| polled.contains(&l.0),
            |b| self.back_edges.contains_key(&b.0),
        );
        self.deaths = rokc_mir::deaths(self.f, |l| {
            rooted.contains(&l.0) && !self.poll_roots.contains(&l.0)
        });
        if self.links.is_none() {
            self.emit_link();
        }
    }

    /// A frame for `roots` (addresses of root words), set up in `header`.
    fn frame(&mut self, header: &mut String, one: &str, roots: &[String]) -> GcFrame {
        let pw = self.pw;
        let tls = self.tls_addr(tls_abi::SHADOW_STACK_TOP);
        let n = roots.len();
        let frame_ty = format!("!llvm.struct<(ptr, i{pw}, array<{n} x ptr>)>");
        let [frame, count, f0, f1] = std::array::from_fn(|_| self.fresh_value());
        let _ = write!(
            header,
            "    {frame} = llvm.alloca {one} x {frame_ty} : (i32) -> !llvm.ptr
    {f0} = llvm.getelementptr {frame}[0, 0] : (!llvm.ptr) -> !llvm.ptr, {frame_ty}
    {count} = llvm.mlir.constant({n} : i{pw}) : i{pw}
    {f1} = llvm.getelementptr {frame}[0, 1] : (!llvm.ptr) -> !llvm.ptr, {frame_ty}
"
        );
        let mut link = format!("    llvm.store {count}, {f1} : i{pw}, !llvm.ptr\n");
        for (i, r) in roots.iter().enumerate() {
            let a = self.fresh_value();
            let _ = writeln!(
                header,
                "    {a} = llvm.getelementptr {frame}[0, 2, {i}] : (!llvm.ptr) -> !llvm.ptr, {frame_ty}"
            );
            let _ = writeln!(link, "    llvm.store {r}, {a} : !llvm.ptr, !llvm.ptr");
        }
        let _ = writeln!(link, "    llvm.store {frame}, {tls} : !llvm.ptr, !llvm.ptr");
        GcFrame {
            tls,
            prev: f0,
            link,
        }
    }

    /// Link the frame: its caller's frame into `prev`, then it at the
    /// top of the shadow stack.
    fn emit_link(&mut self) {
        if self.gc_frame.is_some() {
            let p = self.fresh_value();
            let fr = self.gc_frame.as_ref().unwrap();
            let (tls, prev, link) = (&fr.tls, &fr.prev, &fr.link);
            let _ = write!(
                self.body,
                "    {p} = llvm.load {tls} : !llvm.ptr -> !llvm.ptr
    llvm.store {p}, {prev} : !llvm.ptr, !llvm.ptr
{link}"
            );
        }
    }

    /// Null the root slot of dead local `l` (all its `GcRef` leaves).
    fn clear_root(&mut self, l: LocalId) {
        let slot = match (self.gc_param_slots.get(&l.0), self.root_cells.get(&l.0)) {
            (Some(s), _) | (None, Some(s)) => s.clone(),
            (None, None) => self.slot_names[l.0 as usize].clone().unwrap(),
        };
        self.emit_zero(&slot, &self.local_ty(l), "#rok_tbaa_gcroot");
    }

    /// Unlink this function's shadow-stack frame (before every return
    /// where it is linked).
    fn emit_gc_frame_pop(&mut self) {
        let b = self.edge_from.0;
        if self.gc_frame.is_some() && self.links.as_ref().is_none_or(|l| l.linked.contains(&b)) {
            let p = self.fresh_value();
            let fr = self.gc_frame.as_ref().unwrap();
            let _ = write!(
                self.body,
                "    {p} = llvm.load {} : !llvm.ptr -> !llvm.ptr
    llvm.store {p}, {} : !llvm.ptr, !llvm.ptr
",
                fr.prev, fr.tls
            );
        }
    }

    /// The stack slot of local `id`, if it has one.
    fn slot_of(&self, id: LocalId) -> Option<String> {
        if self.is_block_param(id) {
            return None;
        }
        self.slot_names[id.0 as usize].clone()
    }

    /// Read a local: a load of its slot, or its SSA value.
    fn read_local(&mut self, id: LocalId) -> String {
        // `()` has one value and no storage: every read is a fresh one
        // (a remapped SSA name could come from a non-dominating block).
        if self.local_ty(id) == MirTy::Unit {
            let v = self.fresh_value();
            let _ = writeln!(self.body, "    {v} = llvm.mlir.zero : {UNIT_TY}");
            return v;
        }
        if self.is_block_param(id) || self.slot_names[id.0 as usize].is_none() {
            return self
                .local_names
                .get(id.0 as usize)
                .and_then(Option::as_ref)
                .cloned()
                .unwrap_or_else(|| {
                    // Truly unknown - emit a typed zero so callers stay valid.
                    let v = self.fresh_value();
                    let ty = mlir_type(&self.local_ty(id), self.target);
                    let _ = writeln!(self.body, "    {v} = arith.constant 0 : {ty}");
                    v
                });
        }
        let slot = self.slot_names[id.0 as usize].clone().unwrap();
        let v = self.fresh_value();
        let ty = mlir_type(&self.local_ty(id), self.target);
        let tag = self.slot_tag(id);
        let _ = writeln!(
            self.body,
            "    {v} = llvm.load {slot} {{tbaa = [{tag}]}} : !llvm.ptr -> {ty}"
        );
        v
    }

    /// The TBAA tag of local `id`'s slot: `gcroot` where it is the root.
    fn slot_tag(&self, id: LocalId) -> &'static str {
        if self.root_slots.contains(&id.0) {
            "#rok_tbaa_gcroot"
        } else {
            "#rok_tbaa_slot"
        }
    }

    fn write_local(&mut self, id: LocalId, value: &str) {
        if self.is_block_param(id) || self.local_ty(id) == MirTy::Unit {
            return;
        }
        // Non-slotted locals: just remap the SSA name.
        if self.slot_names[id.0 as usize].is_none() {
            self.local_names[id.0 as usize] = Some(value.to_owned());
            return;
        }
        let slot = self.slot_names[id.0 as usize].clone().unwrap();
        let ty = mlir_type(&self.local_ty(id), self.target);
        let tag = self.slot_tag(id);
        let _ = writeln!(
            self.body,
            "    llvm.store {value}, {slot} {{tbaa = [{tag}]}} : {ty}, !llvm.ptr"
        );
        if let Some(cell) = self
            .root_cells
            .get(&id.0)
            .filter(|_| !self.poll_roots.contains(&id.0))
        {
            let _ = writeln!(
                self.body,
                "    llvm.store {value}, {cell} {{tbaa = [#rok_tbaa_gcroot]}} : {ty}, !llvm.ptr"
            );
        }
    }

    /// `v` of type `ty`, fresh from outside (argument, call result), with its
    /// `List` objects' facts given to LLVM: a `List` object always exists
    /// and holds length, capacity and buffer, so they may be loaded early
    /// (out of loops). Each goes through a no-op `ptrmask` that carries the
    /// facts. Other `GcBuf`s may be null or a `str`'s bytes.
    fn buf_ref_facts(&mut self, v: String, ty: &MirTy) -> String {
        let mut paths = Vec::new();
        buf_ref_paths(ty, self.target, &self.lists, &mut Vec::new(), &mut paths);
        if paths.is_empty() {
            return v;
        }
        let pw = self.pw;
        let deref = HEADER_SIZE + 3 * pw / 8;
        let ty_str = mlir_type(ty, self.target);
        let mask = self.fresh_value();
        let _ = writeln!(self.body, "    {mask} = arith.constant -8 : i{pw}");
        let mut v = v;
        for path in paths {
            let at = path
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let [p, q, out] = std::array::from_fn(|_| self.fresh_value());
            let _ = write!(
                self.body,
                "    {p} = llvm.extractvalue {v}[{at}] : {ty_str}
    {q} = llvm.call_intrinsic \"llvm.ptrmask\"({p}, {mask}) : (!llvm.ptr, i{pw}) -> (!llvm.ptr {{llvm.nonnull, llvm.dereferenceable = {deref} : i64, llvm.align = 8 : i64}})
    {out} = llvm.insertvalue {q}, {v}[{at}] : {ty_str}
"
            );
            v = out;
        }
        v
    }

    fn finish(self) -> String {
        let body = crate::compact::compact(&(self.entry_allocas + &self.body));
        match &self.dbg {
            Some(d) => crate::debug::locate(&body, d),
            None => body,
        }
    }

    fn fresh_value(&mut self) -> String {
        let n = self.next_ssa;
        self.next_ssa += 1;
        format!("%{n}")
    }

    fn emit_block(&mut self, b: &BasicBlock) {
        // Multi-block functions need labels; the entry block is unlabeled
        // per MLIR convention. Block parameters appear in the header.
        if b.id.0 != 0 {
            // Allocate SSA names for block params.
            let mut header_args: Vec<String> = Vec::new();
            for p in &b.params {
                let name = self.fresh_value();
                let ty = self.local_ty(*p);
                header_args.push(format!("{name}: {}", mlir_type(&ty, self.target)));
                self.local_names[p.0 as usize] = Some(name);
            }
            if self.reentry.contains(&b.id.0) {
                self.emit_reentry(b);
            }
            if header_args.is_empty() {
                let _ = writeln!(self.body, "  ^bb{}:", b.id.0);
            } else {
                let _ = writeln!(self.body, "  ^bb{}({}):", b.id.0, header_args.join(", "));
            }
            for p in &b.params {
                if let Some(slot) = self.gc_param_slots.get(&p.0) {
                    let v = self.local_names[p.0 as usize].clone().unwrap();
                    let ty = mlir_type(&self.local_ty(*p), self.target);
                    let _ = writeln!(
                        self.body,
                        "    llvm.store {v}, {slot} {{tbaa = [#rok_tbaa_gcroot]}} : {ty}, !llvm.ptr"
                    );
                }
            }
        }
        let pos = self.block_pos[&b.id.0];
        for l in self.deaths.on_entry.get(pos).cloned().unwrap_or_default() {
            self.clear_root(l);
        }
        let parts = rokc_mir::part_stores(b);
        for (i, s) in b.stmts.iter().enumerate() {
            self.link_before(b.id.0, i);
            match parts.get(&i) {
                Some(part) => self.emit_part_store(s, part),
                None => self.emit_stmt(s),
            }
            let dead = self
                .deaths
                .after_stmt
                .get(pos)
                .map(|d| d[i].clone())
                .unwrap_or_default();
            for l in dead {
                self.clear_root(l);
            }
        }
        self.link_before(b.id.0, usize::MAX);
        self.emit_terminator(b.id.0, &b.terminator);
    }

    /// A `BufStore` that writes one field of the element (`part_stores`).
    fn emit_part_store(&mut self, s: &Statement, part: &rokc_mir::PartStore) {
        let Statement::BufStore {
            buf,
            index,
            elem_ty,
            ..
        } = s
        else {
            unreachable!("a part store is a `BufStore`")
        };
        let (MirTy::Struct(_, fields) | MirTy::Tuple(fields)) = elem_ty else {
            unreachable!("a part store writes a field")
        };
        if self.target.explicit_layout(elem_ty) {
            return self.emit_stmt(s);
        }
        let field_ty = fields[part.field as usize].clone();
        let off = self.target.field_offsets(elem_ty)[part.field as usize];
        let p = self.buf_elem_addr(buf, index, elem_ty);
        let q = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {q} = llvm.getelementptr {p}[{off}] : (!llvm.ptr) -> !llvm.ptr, i8"
        );
        let v = self.materialize_operand(&part.value, &field_ty);
        if self.target.has_gc_ref(&field_ty) {
            let len = self.target.size_align(&field_ty).0;
            self.emit_write_barrier(buf, &q, len);
        }
        self.heap_store(&q, &v, &field_ty);
    }

    /// Does `ty` live in memory and move by `memcpy`: an aggregate over 64
    /// bytes (passed by address)? As LLVM values those cost
    /// optimizer time quadratic in their size.
    fn in_memory(&self, ty: &MirTy) -> bool {
        rok_by_ref(ty, self.target)
    }

    /// Copy a `ty` from `src` (aligned `src_align`) to `dst`. Addresses are
    /// otherwise aligned as the type; unknown alignment would make the copy
    /// bytewise where unaligned access is slow (RISC-V).
    fn emit_memcpy(&mut self, dst: &str, src: &str, ty: &MirTy, src_align: u32) {
        let pw = self.pw;
        let (size, align) = self.target.size_align(ty);
        let n = self.fresh_value();
        let _ = write!(
            self.body,
            "    {n} = llvm.mlir.constant({size} : i{pw}) : i{pw}
    \"llvm.intr.memcpy\"({dst}, {src}, {n}) <{{arg_attrs = [{}, {}, {{}}], isVolatile = false}}> : (!llvm.ptr, !llvm.ptr, i{pw}) -> ()
",
            align_attr(align),
            align_attr(src_align)
        );
    }

    /// Zero the `ty` at `dst` (TBAA `tag`).
    fn emit_zero(&mut self, dst: &str, ty: &MirTy, tag: &str) {
        let z = self.fresh_value();
        if self.in_memory(ty) {
            let pw = self.pw;
            let (size, align) = self.target.size_align(ty);
            let n = self.fresh_value();
            let _ = write!(
                self.body,
                "    {z} = llvm.mlir.constant(0 : i8) : i8
    {n} = llvm.mlir.constant({size} : i{pw}) : i{pw}
    \"llvm.intr.memset\"({dst}, {z}, {n}) <{{arg_attrs = [{}, {{}}, {{}}], isVolatile = false}}> : (!llvm.ptr, i8, i{pw}) -> ()
",
                align_attr(align)
            );
            return;
        }
        let ty = mlir_type(ty, self.target);
        let _ = write!(
            self.body,
            "    {z} = llvm.mlir.zero : {ty}
    llvm.store {z}, {dst} {{tbaa = [{tag}]}} : {ty}, !llvm.ptr
"
        );
    }

    /// Copy operand `op`, a `ty` in memory (`in_memory`), to `dst` from
    /// its slot; false (nothing emitted) for another.
    fn memcpy_operand(&mut self, dst: &str, op: &Operand, ty: &MirTy) -> bool {
        let Operand::Copy(id) = op else { return false };
        if !self.in_memory(ty) || self.local_ty(*id) != *ty {
            return false;
        }
        let Some(src) = self.slot_of(*id) else {
            return false;
        };
        if src != dst {
            self.emit_memcpy(dst, &src, ty, self.target.size_align(ty).1);
        }
        true
    }

    /// The address of field `index` of `target`, a struct or tuple in
    /// memory with a slot.
    fn field_addr(&mut self, target: &Operand, target_ty: &MirTy, index: u32) -> Option<String> {
        let Operand::Copy(id) = target else {
            return None;
        };
        if !self.in_memory(target_ty) || !matches!(target_ty, MirTy::Struct(..) | MirTy::Tuple(_)) {
            return None;
        }
        let slot = self.slot_of(*id)?;
        let off = self.target.field_offsets(target_ty)[index as usize];
        let p = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {p} = llvm.getelementptr {slot}[{off}] : (!llvm.ptr) -> !llvm.ptr, i8"
        );
        Some(p)
    }

    /// The address of static `name`: its symbol's, or (`dll_statics`) the
    /// one at its import address.
    fn static_ptr(&mut self, name: &str) -> String {
        let addr = self.fresh_value();
        if self.pkgs.dll_statics.contains(name) {
            let (imp, p) = (self.import_sym(name), self.fresh_value());
            let _ = writeln!(
                self.body,
                "    {p} = llvm.mlir.addressof @{imp} : !llvm.ptr\n    \
                 {addr} = llvm.load {p} invariant : !llvm.ptr -> !llvm.ptr"
            );
        } else {
            let sym = self.pkgs.static_sym(name);
            let _ = writeln!(
                self.body,
                "    {addr} = llvm.mlir.addressof @{sym} : !llvm.ptr"
            );
        }
        addr
    }

    fn import_sym(&self, name: &str) -> String {
        self.pkgs.import_sym(name, self.target.arch == "x86")
    }

    fn rvalue_addr(&mut self, r: &RValue, ty: &MirTy) -> Option<String> {
        match r {
            RValue::Use(Operand::Copy(id)) if self.local_ty(*id) == *ty => self.slot_of(*id),
            RValue::StaticLoad { name, .. } => {
                self.ensure_static_init(name);
                Some(self.static_ptr(name))
            }
            RValue::FieldRead { target, index, .. } => {
                let target_ty = match target {
                    Operand::Copy(id) => self.local_ty(*id),
                    Operand::Const(_) => return None,
                };
                self.field_addr(target, &target_ty, *index)
            }
            RValue::ArrayIndex {
                array,
                offset,
                index,
                elem_ty,
                len,
            } => {
                let slot = self.array_base(*array, *offset)?;
                let elem_str = mlir_type(elem_ty, self.target);
                let (idx_int, from_str) = self.checked_index(index, *len);
                let ptr = self.fresh_value();
                let _ = writeln!(
                    self.body,
                    "    {ptr} = llvm.getelementptr {slot}[0, {idx_int}] : (!llvm.ptr, {from_str}) -> !llvm.ptr, !llvm.array<{len} x {elem_str}>"
                );
                Some(ptr)
            }
            RValue::HeapField { ptr, ty, index } => Some(self.heap_field_addr(ptr, ty, *index)),
            RValue::HeapLoad { ptr, ty } if !self.target.any_inline(ty) => {
                let p = self.materialize_operand(ptr, &MirTy::GcRef);
                let addr = self.fresh_value();
                let _ = writeln!(
                    self.body,
                    "    {addr} = llvm.getelementptr {p}[{HEADER_SIZE}] : (!llvm.ptr) -> !llvm.ptr, i8"
                );
                Some(addr)
            }
            RValue::BufLoad {
                buf,
                index,
                elem_ty,
            } if *elem_ty != MirTy::Unit => Some(self.buf_elem_addr(buf, index, elem_ty)),
            _ => None,
        }
    }

    /// Assign `r` to the in-memory `ty` at `slot`: copied from where it's
    /// read or built in place; false (nothing emitted) for a value rvalue. A
    /// slot whose address is taken (`seen`) gets a call's result copied in
    /// afterwards, since the callee writes as it goes (`result_locals`).
    fn assign_in_memory(&mut self, slot: &str, ty: &MirTy, r: &RValue, seen: bool) -> bool {
        if let Some(src) = self.rvalue_addr(r, ty) {
            if src != slot {
                self.emit_memcpy(slot, &src, ty, self.target.size_align(ty).1);
            }
            return true;
        }
        if !self.in_memory(ty) {
            return false;
        }
        match r {
            RValue::StructInit { ty: sty, fields } => {
                let tys = struct_field_tys(sty);
                let offs = self.target.field_offsets(sty);
                let packed: u32 = tys.iter().map(|t| self.target.size_align(t).0).sum();
                if tys.len() != fields.len() || offs.len() != fields.len() {
                    return false;
                }
                // Padding is zero, as in a value built field by field.
                if packed != self.target.size_align(sty).0 {
                    self.emit_zero(slot, sty, "#rok_tbaa_slot");
                }
                for ((f, t), off) in fields.iter().zip(&tys).zip(offs) {
                    self.store_at(slot, off, f, t);
                }
                true
            }
            RValue::WithField {
                source,
                struct_ty,
                index,
                value,
            } if matches!(struct_ty, MirTy::Struct(..) | MirTy::Tuple(_)) => {
                if !self.memcpy_operand(slot, source, struct_ty) {
                    let v = self.materialize_operand(source, struct_ty);
                    let t = mlir_type(struct_ty, self.target);
                    let _ = writeln!(self.body, "    llvm.store {v}, {slot} : {t}, !llvm.ptr");
                }
                let t = struct_field_tys(struct_ty)[*index as usize].clone();
                let off = self.target.field_offsets(struct_ty)[*index as usize];
                self.store_at(slot, off, value, &t);
                true
            }
            RValue::Call { callee, args, .. }
                if self.fns[callee].c.is_none() && !self.fns[callee].runtime =>
            {
                let into = self.result_slot(slot, ty, seen);
                let dst = self.fresh_value();
                self.emit_call(&dst, *callee, args);
                if into != slot {
                    self.emit_memcpy(slot, &into, ty, self.target.size_align(ty).1);
                }
                true
            }
            RValue::CallIndirect {
                callee,
                args,
                arg_tys,
                ret_ty,
                abi: Abi::Rok,
            } => {
                let into = self.result_slot(slot, ty, seen);
                self.emit_call_indirect(callee, args, arg_tys, ret_ty);
                if into != slot {
                    self.emit_memcpy(slot, &into, ty, self.target.size_align(ty).1);
                }
                true
            }
            _ => false,
        }
    }

    /// Where the next call writes its by-address result bound for `slot`
    /// (`sret_into`): there, or a fresh slot when `seen`.
    fn result_slot(&mut self, slot: &str, ty: &MirTy, seen: bool) -> String {
        let into = if seen {
            let t = mlir_type(ty, self.target);
            self.entry_alloca(&t, self.target.size_align(ty).1.max(8))
        } else {
            slot.to_owned()
        };
        self.sret_into = Some(into.clone());
        into
    }

    /// Store operand `op`, a `ty`, `off` bytes into `base`.
    fn store_at(&mut self, base: &str, off: u32, op: &Operand, ty: &MirTy) {
        if *ty == MirTy::Unit {
            return;
        }
        let p = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {p} = llvm.getelementptr {base}[{off}] : (!llvm.ptr) -> !llvm.ptr, i8"
        );
        if self.memcpy_operand(&p, op, ty) {
            return;
        }
        let v = self.materialize_operand(op, ty);
        let t = mlir_type(ty, self.target);
        let _ = writeln!(self.body, "    llvm.store {v}, {p} : {t}, !llvm.ptr");
    }

    fn emit_stmt(&mut self, s: &Statement) {
        match s {
            Statement::Assign { target, rvalue } => {
                // `[a, b, c]` is stored straight into the target's slot.
                if let RValue::ArrayInit { elem_ty, elements } = rvalue {
                    self.emit_array_init(*target, elem_ty, elements);
                    return;
                }
                let ty = self.local_ty(*target);
                // `()` has no MLIR type and nothing to copy.
                if ty == MirTy::Unit && matches!(rvalue, RValue::Use(_)) {
                    return;
                }
                // Arrays and in-memory values are built in their slot and
                // copied slot to slot: as LLVM values big ones take the
                // optimizer minutes, or crash it.
                if let Some(slot) = self.slot_of(*target) {
                    let seen = self.addressed.contains(target);
                    if (matches!(ty, MirTy::Array(..)) || self.in_memory(&ty))
                        && self.assign_in_memory(&slot, &ty, rvalue, seen)
                    {
                        return;
                    }
                }
                let val = self.compute_rvalue(&ty, rvalue);
                self.write_local(*target, &val);
            }
            Statement::ArrayWrite {
                array,
                offset,
                index,
                value,
                elem_ty,
                len,
            } => {
                self.emit_array_write(*array, *offset, index, value, elem_ty, *len);
            }
            Statement::Loc(sp, scope) => {
                self.at = *sp;
                if self.dbg.is_some() {
                    let _ = writeln!(
                        self.body,
                        "{}{} {} {}",
                        crate::debug::MARK,
                        sp.line,
                        sp.col.max(1),
                        scope.id
                    );
                }
            }
            // A poll root's cell is null but on a yield path.
            Statement::StorageDead(l) if self.poll_roots.contains(&l.0) => {}
            Statement::StorageDead(l) => {
                // Only a frame-rooted slot matters; zero it (all its
                // `GcRef` leaves become null).
                let slot = self
                    .root_cells
                    .get(&l.0)
                    .or(self.slot_names[l.0 as usize].as_ref());
                if let (Some(_), Some(slot)) = (&self.gc_frame, slot.cloned()) {
                    let tag = if self.root_cells.contains_key(&l.0) {
                        "#rok_tbaa_gcroot"
                    } else {
                        self.slot_tag(*l)
                    };
                    self.emit_zero(&slot, &self.local_ty(*l), tag);
                }
            }
            Statement::RawStore {
                addr,
                value,
                ty,
                volatile,
            } => {
                let p = self.raw_addr(addr);
                if !*volatile && self.memcpy_operand(&p, value, ty) {
                    return;
                }
                let v = self.materialize_operand(value, ty);
                let t = mlir_type(ty, self.target);
                let vol = if *volatile { "volatile " } else { "" };
                let _ = writeln!(self.body, "    llvm.store {vol}{v}, {p} : {t}, !llvm.ptr");
            }
            // `()` elements take no memory (`GcBuf<()>`: a unit result).
            Statement::BufStore { elem_ty, .. } if *elem_ty == MirTy::Unit => {}
            Statement::BufStore {
                buf,
                index,
                value,
                elem_ty,
            } => {
                let p = self.buf_elem_addr(buf, index, elem_ty);
                if self.target.has_gc_ref(elem_ty) {
                    let len = self.target.size_align(elem_ty).0;
                    self.emit_write_barrier(buf, &p, len);
                }
                if !self.memcpy_operand(&p, value, elem_ty) {
                    let v = self.materialize_operand(value, elem_ty);
                    self.heap_store(&p, &v, elem_ty);
                }
            }
            Statement::HeapStore {
                ptr,
                ty,
                index,
                value,
            } => {
                let field = struct_field_tys(ty)[*index as usize].clone();
                let addr = self.heap_field_addr(ptr, ty, *index);
                if self.target.has_gc_ref(&field) {
                    let len = self.target.size_align(&field).0;
                    self.emit_write_barrier(ptr, &addr, len);
                }
                if !self.memcpy_operand(&addr, value, &field) {
                    let v = self.materialize_operand(value, &field);
                    self.heap_store(&addr, &v, &field);
                }
            }
            Statement::StaticStore { name, value, ty } => {
                self.ensure_static_init(name);
                let addr = self.static_ptr(name);
                if self.memcpy_operand(&addr, value, ty) {
                    return;
                }
                let val = self.materialize_operand(value, ty);
                let ty_str = mlir_type(ty, self.target);
                let _ = writeln!(
                    self.body,
                    "    llvm.store {val}, {addr} : {ty_str}, !llvm.ptr"
                );
            }
        }
    }

    /// The address of the array `offset` bytes into `array`'s slot.
    fn array_base(&mut self, array: LocalId, offset: u32) -> Option<String> {
        let slot = self.slot_names[array.0 as usize].clone()?;
        if offset == 0 {
            return Some(slot);
        }
        let p = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {p} = llvm.getelementptr {slot}[{offset}] : (!llvm.ptr) -> !llvm.ptr, i8"
        );
        Some(p)
    }

    fn emit_array_write(
        &mut self,
        array: LocalId,
        offset: u32,
        index: &Operand,
        value: &Operand,
        elem_ty: &MirTy,
        len: u32,
    ) {
        let Some(slot) = self.array_base(array, offset) else {
            return;
        };
        let elem_str = mlir_type(elem_ty, self.target);
        let (idx_int, from_str) = self.checked_index(index, len);
        let ptr = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {ptr} = llvm.getelementptr {slot}[0, {idx_int}] : (!llvm.ptr, {from_str}) -> !llvm.ptr, !llvm.array<{len} x {elem_str}>"
        );
        if self.memcpy_operand(&ptr, value, elem_ty) {
            return;
        }
        let val = self.materialize_operand(value, elem_ty);
        let _ = writeln!(
            self.body,
            "    llvm.store {val}, {ptr} : {elem_str}, !llvm.ptr"
        );
    }

    /// `[a, b, c]` stored element by element into the target's slot; a
    /// big one of constants (`b"..."`) copied from a constant global.
    fn emit_array_init(&mut self, target: LocalId, elem_ty: &MirTy, elements: &[Operand]) {
        let Some(slot) = self.slot_names[target.0 as usize].clone() else {
            return;
        };
        let len = elements.len();
        let ty = MirTy::Array(Box::new(elem_ty.clone()), len as u32);
        if self.in_memory(&ty) {
            if let Some(bytes) = const_bytes(elements, elem_ty, self.target) {
                let sym = self.intern_bytes(&bytes);
                let src = self.fresh_value();
                let _ = writeln!(
                    self.body,
                    "    {src} = llvm.mlir.addressof @{sym} : !llvm.ptr"
                );
                // Interned bytes are unaligned.
                self.emit_memcpy(&slot, &src, &ty, 1);
                return;
            }
        }
        let elem_str = mlir_type(elem_ty, self.target);
        for (i, e) in elements.iter().enumerate() {
            let ptr = self.fresh_value();
            let _ = writeln!(
                self.body,
                "    {ptr} = llvm.getelementptr {slot}[0, {i}] : (!llvm.ptr) -> !llvm.ptr, !llvm.array<{len} x {elem_str}>"
            );
            if self.memcpy_operand(&ptr, e, elem_ty) {
                continue;
            }
            let v = self.materialize_operand(e, elem_ty);
            let _ = writeln!(
                self.body,
                "    llvm.store {v}, {ptr} : {elem_str}, !llvm.ptr"
            );
        }
    }

    /// An array index at its own type, panicking unless `0 <= index < len`
    /// (one unsigned compare); in-range constants skip the check.
    fn checked_index(&mut self, index: &Operand, len: u32) -> (String, String) {
        let ty = match index {
            Operand::Copy(id) => self.local_ty(*id),
            Operand::Const(c) => const_ty(c),
        };
        let v = self.materialize_operand(index, &ty);
        let ty_str = mlir_type(&ty, self.target);
        let in_range = const_int(index).is_some_and(|c| (0..i128::from(len)).contains(&c));
        if !in_range {
            let fail = self.cmp_const("uge", &v, i128::from(len), &ty_str);
            self.emit_check(&fail, "index out of bounds");
        }
        (v, ty_str)
    }

    // Materialise an rvalue into a single SSA name and return it.
    fn compute_rvalue(&mut self, dst_ty: &MirTy, r: &RValue) -> String {
        match r {
            RValue::Use(op) => self.materialize_operand(op, dst_ty),
            RValue::BinOp {
                op,
                op_ty,
                lhs,
                rhs,
            } => {
                let dst = self.fresh_value();
                self.emit_bin_op(&dst, dst_ty, *op, op_ty, lhs, rhs);
                dst
            }
            RValue::UnaryOp { op, op_ty, operand } => {
                let dst = self.fresh_value();
                self.emit_unary_op(&dst, dst_ty, *op, op_ty, operand);
                dst
            }
            RValue::Call { callee, args, .. } => {
                let dst = self.fresh_value();
                self.emit_call(&dst, *callee, args);
                let sig = &self.fns[callee];
                if sig.runtime || sig.c.is_some() {
                    return dst;
                }
                let ret = sig.ret.clone();
                self.buf_ref_facts(dst, &ret)
            }
            RValue::StructInit { ty, fields } => self.emit_struct_init(ty, fields),
            RValue::FieldRead {
                target,
                index,
                field_ty,
            } => self.emit_field_read(target, *index, field_ty, dst_ty),
            RValue::Cast {
                source,
                from_ty,
                to_ty,
            } => self.emit_cast(source, from_ty, to_ty),
            RValue::Bitcast {
                source,
                from_ty,
                to_ty,
            } => {
                let v = self.materialize_operand(source, from_ty);
                self.bitcast_value(v, from_ty, to_ty)
            }
            RValue::WithField {
                source,
                struct_ty,
                index,
                value,
            } => self.emit_with_field(source, struct_ty, *index, value),
            RValue::ArrayInit { .. } => {
                // Array init doesn't return an SSA value; it stores
                // into the destination slot. Routed via emit_stmt.
                self.fresh_value()
            }
            RValue::ArrayIndex {
                array,
                offset,
                index,
                elem_ty,
                len,
            } => self.emit_array_index(*array, *offset, index, elem_ty, *len),
            RValue::FnAddr { callee, fn_ty } => self.emit_fn_addr(*callee, fn_ty),
            RValue::CallIndirect {
                callee,
                args,
                arg_tys,
                ret_ty,
                abi: Abi::Rok,
            } => self.emit_call_indirect(callee, args, arg_tys, ret_ty),
            RValue::CallIndirect {
                callee,
                args,
                arg_tys,
                ret_ty,
                abi: Abi::C { variadic, system },
            } => {
                // A call through an `extern "C"` / `extern "system"` fn
                // pointer.
                let fn_ty = MirTy::Fn(arg_tys.clone(), Box::new(ret_ty.clone()));
                let ptr = self.materialize_operand(callee, &fn_ty);
                let what = "a call through an `extern \"C\" fn` pointer";
                let dst = self.fresh_value();
                if let Some(abi) = Emitter::classify(
                    &mut self.errors,
                    self.target,
                    what,
                    arg_tys,
                    ret_ty,
                    *variadic,
                    *system,
                ) {
                    self.emit_c_call(&dst, arg_tys, ret_ty, &abi, Callee::Ptr(&ptr), args);
                }
                dst
            }
            RValue::HeapBox { value, ty } => self.emit_heap_box(value, ty),
            RValue::HeapAlloc { ty } => self.emit_gc_object(ty),
            RValue::HeapField { ptr, ty, index } => {
                let addr = self.heap_field_addr(ptr, ty, *index);
                let field = struct_field_tys(ty)[*index as usize].clone();
                self.heap_load(&addr, &field)
            }
            RValue::BufAlloc { elem_ty, len } => self.emit_buf_alloc(elem_ty, len),
            RValue::RawLoad { addr, ty, volatile } => {
                let p = self.raw_addr(addr);
                let t = mlir_type(ty, self.target);
                let v = self.fresh_value();
                let vol = if *volatile { "volatile " } else { "" };
                let _ = writeln!(self.body, "    {v} = llvm.load {vol}{p} : !llvm.ptr -> {t}");
                v
            }
            RValue::StaticAddr(name) => {
                self.ensure_static_init(name);
                let addr = self.static_ptr(name);
                let (pw, v) = (self.pw, self.fresh_value());
                let _ = writeln!(
                    self.body,
                    "    {v} = llvm.ptrtoint {addr} : !llvm.ptr to i{pw}"
                );
                v
            }
            RValue::AddrOf(l) => {
                let pw = self.pw;
                let v = self.fresh_value();
                // A `()` local has no storage: any aligned address does.
                match self.slot_of(*l) {
                    Some(slot) => {
                        let _ = writeln!(
                            self.body,
                            "    {v} = llvm.ptrtoint {slot} : !llvm.ptr to i{pw}"
                        );
                    }
                    None => {
                        let _ = writeln!(self.body, "    {v} = arith.constant 1 : i{pw}");
                    }
                }
                v
            }
            RValue::SizeOf { ty, align } => {
                let pw = self.pw;
                let (size, al) = self.target.size_align(ty);
                let n = if *align { al } else { size };
                let v = self.fresh_value();
                let _ = writeln!(self.body, "    {v} = arith.constant {n} : i{pw}");
                v
            }
            RValue::BufLoad { elem_ty, .. } if *elem_ty == MirTy::Unit => {
                let v = self.fresh_value();
                let _ = writeln!(self.body, "    {v} = arith.constant 0 : i1");
                v
            }
            RValue::BufLoad {
                buf,
                index,
                elem_ty,
            } => {
                let p = self.buf_elem_addr(buf, index, elem_ty);
                self.heap_load(&p, elem_ty)
            }
            RValue::BufAtomic {
                op,
                buf,
                index,
                args,
                elem_ty,
                order,
            } => self.emit_buf_atomic(op, buf, index, args, elem_ty, *order),
            RValue::HeapLoad { ptr, ty } => {
                let p = self.materialize_operand(ptr, &MirTy::GcRef);
                if self.target.any_inline(ty) {
                    return self.bitcast_value(p, &MirTy::GcRef, ty);
                }
                let addr = self.fresh_value();
                let _ = writeln!(
                    self.body,
                    "    {addr} = llvm.getelementptr {p}[{HEADER_SIZE}] : (!llvm.ptr) -> !llvm.ptr, i8"
                );
                self.heap_load(&addr, ty)
            }
            RValue::StaticLoad { name, ty } => {
                self.ensure_static_init(name);
                let ty_str = mlir_type(ty, self.target);
                let addr = self.static_ptr(name);
                let dst = self.fresh_value();
                let _ = writeln!(
                    self.body,
                    "    {dst} = llvm.load {addr} : !llvm.ptr -> {ty_str}"
                );
                dst
            }
        }
    }

    /// `rok_gc_alloc_slow(total_size, align, type_id)` and store the
    /// payload; returns the `GcRef`. Size computed here: MLIR folds the
    /// `gep null[1]` idiom under LLVM's default layout (`i64` 4-aligned),
    /// undersizing objects.
    fn emit_heap_box(&mut self, value: &Operand, ty: &MirTy) -> String {
        if self.target.any_inline(ty) {
            let v = self.materialize_operand(value, ty);
            return self.bitcast_value(v, ty, &MirTy::GcRef);
        }
        let obj = self.emit_gc_object(ty);
        let payload = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {payload} = llvm.getelementptr {obj}[{HEADER_SIZE}] : (!llvm.ptr) -> !llvm.ptr, i8"
        );
        if !self.memcpy_operand(&payload, value, ty) {
            let v = self.materialize_operand(value, ty);
            self.heap_store(&payload, &v, ty);
        }
        obj
    }

    /// A zeroed GC object of `ty`; returns the `GcRef`. Small objects bump
    /// the TLAB inline and write the header
    /// (`gc_word` = size, which the block heap parses by); else
    /// `rok_gc_alloc_slow`.
    fn emit_gc_object(&mut self, ty: &MirTy) -> String {
        let pw = self.pw;
        let tid = self.gc_tid(ty, false);
        let (size, align) = gc_object_size(ty, self.target);
        let size = size.max(MIN_OBJECT_SIZE);
        let [total, align_v] = std::array::from_fn(|_| self.fresh_value());
        self.needs.insert("rok_gc_alloc_slow");
        let _ = write!(
            self.body,
            "    {total} = arith.constant {size} : i{pw}
    {align_v} = arith.constant {align} : i{pw}
"
        );
        if size >= LOS_THRESHOLD || align > 8 {
            let obj = self.fresh_value();
            let _ = writeln!(
                self.body,
                "    {obj} = llvm.call @rok_gc_alloc_slow({total}, {align_v}, {tid}) {{tbaa = [#rok_tbaa_ref]}} : (i{pw}, i{pw}, i32) -> !llvm.ptr"
            );
            return obj;
        }
        self.emit_tlab_alloc(&total, &total, None, &align_v, &tid)
    }

    /// A small object from this thread's TLAB when it fits (and `ok`
    /// holds), else from the runtime (`alloc_fn`). The object's `GcRef`.
    fn emit_tlab_alloc(
        &mut self,
        size8: &str,
        size: &str,
        ok: Option<&str>,
        align_v: &str,
        tid: &str,
    ) -> String {
        let pw = self.pw;
        let cp = self.tls_addr(tls_abi::TLAB_CURSOR);
        let lp = self.tls_addr(tls_abi::TLAB_LIMIT);
        self.needs.insert("rok.alloc");
        let ok = ok.map_or_else(
            || {
                let t = self.fresh_value();
                let _ = writeln!(self.body, "    {t} = arith.constant true");
                t
            },
            ToOwned::to_owned,
        );
        let obj = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {obj} = llvm.call @rok.alloc({cp}, {lp}, {size8}, {size}, {align_v}, {tid}, {ok}) {{tbaa = [#rok_tbaa_ref]}} : (!llvm.ptr, !llvm.ptr, i{pw}, i{pw}, i{pw}, i32, i1) -> !llvm.ptr"
        );
        obj
    }

    /// Address of field `index` of the `ty` (tuple) payload of GC object
    /// `ptr`.
    fn heap_field_addr(&mut self, ptr: &Operand, ty: &MirTy, index: u32) -> String {
        let p = self.materialize_operand(ptr, &MirTy::GcRef);
        let ty_str = mlir_type(ty, self.target);
        let index = llvm_field(ty, index as usize, self.target);
        let [payload, addr] = std::array::from_fn(|_| self.fresh_value());
        let _ = write!(
            self.body,
            "    {payload} = llvm.getelementptr {p}[{HEADER_SIZE}] : (!llvm.ptr) -> !llvm.ptr, i8
    {addr} = llvm.getelementptr {payload}[0, {index}] : (!llvm.ptr) -> !llvm.ptr, {ty_str}
"
        );
        addr
    }

    /// GC type id of `ty` objects (`array`: buffers of `ty`) as an `i32`.
    /// Types with the same `ScanTable` share an id, so ids stay stable
    /// across REPL cells. Units read it from a glue-defined symbol
    /// (`units::gc_symbol`), so other units' types don't change their code.
    fn gc_tid(&mut self, ty: &MirTy, array: bool) -> String {
        let key = scan_table(ty, array, self.target);
        let index = self
            .gc_types
            .iter()
            .position(|t| *t == key)
            .unwrap_or_else(|| {
                self.gc_types.push(key);
                self.gc_types.len() - 1
            });
        let tid = self.fresh_value();
        if let Some(pkg) = &self.gc_pkg {
            let sym = crate::units::gc_symbol(pkg, &self.gc_types[index]);
            let a = self.fresh_value();
            let _ = write!(
                self.body,
                "    {a} = llvm.mlir.addressof @{sym} : !llvm.ptr
    {tid} = llvm.load {a} invariant : !llvm.ptr -> i32
"
            );
            self.gc_syms.insert(sym);
        } else {
            let id = self.gc_type_base + index as u32;
            let _ = writeln!(self.body, "    {tid} = arith.constant {id} : i32");
        }
        tid
    }

    /// `rok_gc_alloc_slow(header + len * size, align, type_id)` for a zeroed
    /// buffer of `len` `elem_ty`s: untraced unless elements hold references,
    /// then traced through an array `ScanTable`.
    fn emit_buf_alloc(&mut self, elem_ty: &MirTy, len: &Operand) -> String {
        let pw = self.pw;
        let usize_ty = MirTy::Prim(PrimTy::Usize);
        let n = self.materialize_operand(len, &usize_ty);
        let tid = if self.target.has_gc_ref(elem_ty) {
            self.gc_tid(elem_ty, true)
        } else {
            let t = self.fresh_value();
            let _ = writeln!(self.body, "    {t} = arith.constant {PLAIN_TYPE_ID} : i32");
            t
        };
        let (size, align) = self.target.size_align(elem_ty);
        let align = align.max(8);
        let [sz, hd, bytes, sum, max_n, ovf, all, total, align_v] =
            std::array::from_fn(|_| self.fresh_value());
        self.needs.insert("rok_gc_alloc_slow");
        // `n * size + header` overflowing asks for `usize::MAX`, which the
        // runtime rejects as too large. Constants print as signed `i{pw}`.
        let umax = u64::MAX >> (64 - pw);
        let limit = (umax - HEADER_SIZE as u64) / (size.max(1) as u64);
        let signed = |v: u64| ((v << (64 - pw)) as i64) >> (64 - pw);
        let (limit, all_v) = (signed(limit), signed(umax));
        let _ = write!(
            self.body,
            "    {sz} = arith.constant {size} : i{pw}
    {hd} = arith.constant {HEADER_SIZE} : i{pw}
    {bytes} = arith.muli {n}, {sz} : i{pw}
    {sum} = arith.addi {bytes}, {hd} : i{pw}
    {max_n} = arith.constant {limit} : i{pw}
    {ovf} = arith.cmpi ugt, {n}, {max_n} : i{pw}
    {all} = arith.constant {all_v} : i{pw}
    {total} = arith.select {ovf}, {all}, {sum} : i{pw}
    {align_v} = arith.constant {align} : i{pw}
"
        );
        if align > 8 {
            let obj = self.fresh_value();
            let _ = writeln!(
                self.body,
                "    {obj} = llvm.call @rok_gc_alloc_slow({total}, {align_v}, {tid}) {{tbaa = [#rok_tbaa_ref]}} : (i{pw}, i{pw}, i32) -> !llvm.ptr"
            );
            return obj;
        }
        // Small buffers bump the TLAB inline: size rounded up to 8 (no
        // overflow under `max_n`), at least `MIN_OBJECT_SIZE`, below the
        // large object space.
        let [seven, mask, padded, rounded, min, size8, los, small, inline] =
            std::array::from_fn(|_| self.fresh_value());
        let _ = write!(
            self.body,
            "    {seven} = arith.constant 7 : i{pw}
    {mask} = arith.constant -8 : i{pw}
    {padded} = arith.addi {sum}, {seven} : i{pw}
    {rounded} = arith.andi {padded}, {mask} : i{pw}
    {min} = arith.constant {MIN_OBJECT_SIZE} : i{pw}
    {size8} = arith.maxui {rounded}, {min} : i{pw}
    {los} = arith.constant {LOS_THRESHOLD} : i{pw}
    {small} = arith.cmpi ult, {size8}, {los} : i{pw}
    {inline} = arith.cmpi ule, {n}, {max_n} : i{pw}
"
        );
        let ok = self.fresh_value();
        let _ = writeln!(self.body, "    {ok} = arith.andi {small}, {inline} : i1");
        let obj = self.emit_tlab_alloc(&size8, &total, Some(&ok), &align_v, &tid);
        // An unused buffer is deleted with its allocation (`alloc_fn`);
        // its size still has to be one the runtime takes.
        let k = self.next_check;
        self.next_check += 1;
        let big = self.fresh_value();
        let _ = write!(
            self.body,
            "    cf.cond_br {ovf}, ^rok_abig{k}, ^rok_afit{k}
  ^rok_abig{k}:
    {big} = llvm.call @rok_gc_alloc_slow({all}, {align_v}, {tid}) {{tbaa = [#rok_tbaa_ref]}} : (i{pw}, i{pw}, i32) -> !llvm.ptr
    llvm.unreachable
  ^rok_afit{k}:
"
        );
        obj
    }

    /// Address of element `index` of buffer `buf` (header + index * size).
    /// `inttoptr` of a raw `usize` address operand.
    fn raw_addr(&mut self, addr: &Operand) -> String {
        let pw = self.pw;
        let a = self.materialize_operand(addr, &MirTy::Prim(PrimTy::Usize));
        let p = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {p} = llvm.inttoptr {a} : i{pw} to !llvm.ptr"
        );
        p
    }

    /// LLVM atomics on a buffer element; `bool` through its
    /// `i8` byte. `order`: the LLVM ordering (`cas`: success, failure).
    fn emit_buf_atomic(
        &mut self,
        op: &str,
        buf: &Operand,
        index: &Operand,
        args: &[Operand],
        elem_ty: &MirTy,
        order: [&str; 2],
    ) -> String {
        let p = self.buf_elem_addr(buf, index, elem_ty);
        let ty = mlir_type(elem_ty, self.target);
        let is_bool = ty == "i1";
        let at = if is_bool { "i8".to_owned() } else { ty };
        let align = self.target.size_align(elem_ty).0;
        let ops: Vec<String> = args
            .iter()
            .map(|a| {
                let v = self.materialize_operand(a, elem_ty);
                if !is_bool {
                    return v;
                }
                let w = self.fresh_value();
                let _ = writeln!(self.body, "    {w} = arith.extui {v} : i1 to i8");
                w
            })
            .collect();
        // A store yields the value it stored.
        let v = if op == "store" {
            ops[0].clone()
        } else {
            self.fresh_value()
        };
        match op {
            "store" => {
                let _ = writeln!(
                    self.body,
                    "    llvm.store {v}, {p} atomic {} {{alignment = {align} : i64}} : {at}, !llvm.ptr",
                    order[0]
                );
            }
            "load" => {
                let _ = writeln!(
                    self.body,
                    "    {v} = llvm.load {p} atomic {} {{alignment = {align} : i64}} : !llvm.ptr -> {at}",
                    order[0]
                );
            }
            "cas" => {
                let r = self.fresh_value();
                let _ = writeln!(
                    self.body,
                    "    {r} = llvm.cmpxchg {p}, {}, {} {} {} : !llvm.ptr, {at}\n    {v} = llvm.extractvalue {r}[0] : !llvm.struct<({at}, i1)>",
                    ops[0], ops[1], order[0], order[1]
                );
            }
            _ => {
                let kw = match op {
                    "and" | "or" | "xor" => format!("_{op}"),
                    _ => op.to_owned(),
                };
                let _ = writeln!(
                    self.body,
                    "    {v} = llvm.atomicrmw {kw} {p}, {} {} : !llvm.ptr, {at}",
                    ops[0], order[0]
                );
            }
        }
        if !is_bool {
            return v;
        }
        let b = self.fresh_value();
        let _ = writeln!(self.body, "    {b} = arith.trunci {v} : i8 to i1");
        b
    }

    fn buf_elem_addr(&mut self, buf: &Operand, index: &Operand, elem_ty: &MirTy) -> String {
        let pw = self.pw;
        let usize_ty = MirTy::Prim(PrimTy::Usize);
        let b = self.materialize_operand(buf, &MirTy::GcRef);
        let i = self.materialize_operand(index, &usize_ty);
        let size = self.target.size_align(elem_ty).0;
        let [sz, hd, off, at, p] = std::array::from_fn(|_| self.fresh_value());
        let _ = write!(
            self.body,
            "    {sz} = arith.constant {size} : i{pw}
    {hd} = arith.constant {HEADER_SIZE} : i{pw}
    {off} = arith.muli {i}, {sz} : i{pw}
    {at} = arith.addi {off}, {hd} : i{pw}
    {p} = llvm.getelementptr {b}[{at}] : (!llvm.ptr, i{pw}) -> !llvm.ptr, i8
"
        );
        p
    }

    /// Load a `ty` from GC memory at `p`, aggregates scalar by scalar with
    /// their TBAA tags so a store of another type can't clobber them.
    fn heap_load(&mut self, p: &str, ty: &MirTy) -> String {
        let ty_s = mlir_type(ty, self.target);
        let Some(leaves) = heap_leaves(ty, self.target) else {
            let tag = heap_tbaa(ty);
            let v = self.fresh_value();
            let _ = writeln!(
                self.body,
                "    {v} = llvm.load {p} {{tbaa = [{tag}]}} : !llvm.ptr -> {ty_s}"
            );
            return v;
        };
        let mut acc = self.fresh_value();
        let _ = writeln!(self.body, "    {acc} = llvm.mlir.undef : {ty_s}");
        for (path, off, leaf) in leaves {
            let [field, leaf_v, next] = std::array::from_fn(|_| self.fresh_value());
            let lt = mlir_type(&leaf, self.target);
            let at = join_path(&path);
            let _ = write!(
                self.body,
                "    {field} = llvm.getelementptr {p}[{off}] : (!llvm.ptr) -> !llvm.ptr, i8
    {leaf_v} = llvm.load {field} {{tbaa = [{}]}} : !llvm.ptr -> {lt}
    {next} = llvm.insertvalue {leaf_v}, {acc}[{at}] : {ty_s}
",
                heap_tbaa(&leaf)
            );
            acc = next;
        }
        acc
    }

    /// Store `v` (a `ty`) to GC object memory at `p` (`heap_load`).
    fn heap_store(&mut self, p: &str, v: &str, ty: &MirTy) {
        let ty_s = mlir_type(ty, self.target);
        let Some(leaves) = heap_leaves(ty, self.target) else {
            let tag = heap_tbaa(ty);
            let _ = writeln!(
                self.body,
                "    llvm.store {v}, {p} {{tbaa = [{tag}]}} : {ty_s}, !llvm.ptr"
            );
            return;
        };
        for (path, off, leaf) in leaves {
            let [field, leaf_v] = std::array::from_fn(|_| self.fresh_value());
            let lt = mlir_type(&leaf, self.target);
            let at = join_path(&path);
            let _ = write!(
                self.body,
                "    {leaf_v} = llvm.extractvalue {v}[{at}] : {ty_s}
    {field} = llvm.getelementptr {p}[{off}] : (!llvm.ptr) -> !llvm.ptr, i8
    llvm.store {leaf_v}, {field} {{tbaa = [{}]}} : {lt}, !llvm.ptr
",
                heap_tbaa(&leaf)
            );
        }
    }

    fn emit_fn_addr(&mut self, callee: FunctionId, fn_ty: &MirTy) -> String {
        let name = self.fns[&callee].name.clone();
        let f = &self.fns[&callee];
        if f.stdcall || f.c.as_ref().is_some_and(|a| a.variadic) {
            // An `llvm.func` (a definition's C entry, else the
            // declaration): no `func.constant` of it.
            let sym = f.c_entry.as_ref().map_or(name, |(entry, _)| entry.clone());
            let dst = self.fresh_value();
            let _ = writeln!(
                self.body,
                "    {dst} = llvm.mlir.addressof @{sym} : !llvm.ptr"
            );
            return dst;
        }
        let MirTy::Fn(args, ret) = fn_ty else {
            return self.unsupported("address of a function with a non-function type");
        };
        // `func.constant` takes a `func.func` symbol; cast to `!llvm.ptr`
        // for closure structs and llvm-dialect calls.
        let typed = self.fresh_value();
        let mut name = name;
        let sig = if let Some((entry, ty)) = &self.fns[&callee].c_entry {
            // An `extern "C" fn` handed to C: its C ABI entry.
            name.clone_from(entry);
            ty.clone()
        } else if let Some(abi) = &self.fns[&callee].c {
            // The C declaration's own (lowered) type; calling it back
            // through the pointer from rok does not redo the C ABI.
            let sig = &self.fns[&callee];
            let (params, ret) = c_signature(&sig.params, &sig.ret, abi, self.target);
            let params: Vec<String> = params.into_iter().map(|(t, _)| t).collect();
            format!(
                "({}) -> {}",
                params.join(", "),
                ret.unwrap_or_else(|| "()".to_owned())
            )
        } else {
            // A rok body taking `rok_tls`: its thunk (`emit_thunks`),
            // since a function value's callers pass none.
            if self.fns[&callee].tls {
                self.thunks.insert(callee);
                name.push_str(".v");
            }
            mlir_fn_signature(args, ret, self.target)
        };
        let _ = writeln!(self.body, "    {typed} = func.constant @{name} : {sig}");
        let dst = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {dst} = builtin.unrealized_conversion_cast {typed} : {sig} to !llvm.ptr"
        );
        dst
    }

    fn emit_call_indirect(
        &mut self,
        callee: &Operand,
        args: &[Operand],
        arg_tys: &[MirTy],
        ret_ty: &MirTy,
    ) -> String {
        let fn_ty = MirTy::Fn(arg_tys.to_vec(), Box::new(ret_ty.clone()));
        let callee_v = self.materialize_operand(callee, &fn_ty);
        let into = self.sret_into.is_some();
        let (arg_vals, sret) = self.rok_call_args(args, arg_tys, ret_ty);
        let arg_ty_list = rok_fn_types(arg_tys, ret_ty, self.target).0.join(", ");
        let arg_list = arg_vals.join(", ");
        let ret = if matches!(ret_ty, MirTy::Unit) {
            "()".to_owned()
        } else {
            mlir_type(ret_ty, self.target)
        };
        if let Some(r) = sret {
            let _ = writeln!(
                self.body,
                "    llvm.call {callee_v}({arg_list}) : !llvm.ptr, ({arg_ty_list}) -> ()"
            );
            let dst = self.fresh_value();
            if into {
                return dst;
            }
            let _ = writeln!(self.body, "    {dst} = llvm.load {r} : !llvm.ptr -> {ret}");
            self.buf_ref_facts(dst, ret_ty)
        } else if matches!(ret_ty, MirTy::Unit) {
            let _ = writeln!(
                self.body,
                "    llvm.call {callee_v}({arg_list}) : !llvm.ptr, ({arg_ty_list}) -> ()"
            );
            let dst = self.fresh_value();
            let _ = writeln!(self.body, "    {dst} = arith.constant 0 : i1");
            dst
        } else {
            let dst = self.fresh_value();
            let _ = writeln!(
                self.body,
                "    {dst} = llvm.call {callee_v}({arg_list}) : !llvm.ptr, ({arg_ty_list}) -> {ret}"
            );
            self.buf_ref_facts(dst, ret_ty)
        }
    }

    fn emit_array_index(
        &mut self,
        array: LocalId,
        offset: u32,
        index: &Operand,
        elem_ty: &MirTy,
        len: u32,
    ) -> String {
        let slot = self.array_base(array, offset).unwrap_or_default();
        let elem_str = mlir_type(elem_ty, self.target);
        let (idx_int, from_str) = self.checked_index(index, len);
        let [ptr, dst] = std::array::from_fn(|_| self.fresh_value());
        let _ = write!(
            self.body,
            "    {ptr} = llvm.getelementptr {slot}[0, {idx_int}] : (!llvm.ptr, {from_str}) -> !llvm.ptr, !llvm.array<{len} x {elem_str}>
    {dst} = llvm.load {ptr} : !llvm.ptr -> {elem_str}
"
        );
        dst
    }

    // `llvm.insertvalue` writes one element of an aggregate, returning
    // a new aggregate value (no in-place mutation).
    fn emit_with_field(
        &mut self,
        source: &Operand,
        struct_ty: &MirTy,
        index: u32,
        value: &Operand,
    ) -> String {
        let src_v = self.materialize_operand(source, struct_ty);
        let field_tys = struct_field_tys(struct_ty);
        let val_ty = field_tys.get(index as usize).cloned().unwrap_or(MirTy::Any);
        let v = self.materialize_operand(value, &val_ty);
        let ty_str = mlir_type(struct_ty, self.target);
        let index = llvm_field(struct_ty, index as usize, self.target);
        let dst = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {dst} = llvm.insertvalue {v}, {src_v}[{index}] : {ty_str}"
        );
        dst
    }

    // Integer width adjust. Wider via signed/unsigned extend, narrower
    // via truncate. Same-size acts as a no-op alias.
    fn emit_cast(&mut self, source: &Operand, from_ty: &MirTy, to_ty: &MirTy) -> String {
        let v = self.materialize_operand(source, from_ty);
        self.cast_value(v, from_ty, to_ty)
    }

    fn cast_value(&mut self, v: String, from_ty: &MirTy, to_ty: &MirTy) -> String {
        let from = mlir_type(from_ty, self.target);
        let to = mlir_type(to_ty, self.target);
        if from == to {
            return v;
        }
        // A C function pointer as an address (the null niche's test), or
        // a GC reference's (`buf_same`).
        if let (MirTy::Fn(..) | MirTy::GcRef, MirTy::Prim(_)) = (from_ty, to_ty) {
            let pw = self.pw;
            let addr = self.fresh_value();
            let _ = writeln!(
                self.body,
                "    {addr} = llvm.ptrtoint {v} : !llvm.ptr to i{pw}"
            );
            return self.cast_value(addr, &MirTy::Prim(PrimTy::Usize), to_ty);
        }
        let (MirTy::Prim(fp), MirTy::Prim(tp)) = (from_ty, to_ty) else {
            return self.unsupported(&format!("cast from `{from}` to `{to}`"));
        };
        if matches!(fp, PrimTy::Str) || matches!(tp, PrimTy::Str) {
            return self.unsupported(&format!("cast from `{from}` to `{to}`"));
        }
        let dst = self.fresh_value();
        // `bool` casts as a 1-bit unsigned int (`is_int` excludes it for
        // operator dispatch).
        let from_int_like = fp.is_int() || matches!(fp, PrimTy::Bool);
        let to_int_like = tp.is_int() || matches!(tp, PrimTy::Bool);
        // Pick the right arith op for each four-way (int/float) combination.
        match (from_int_like, to_int_like, fp.is_float(), tp.is_float()) {
            (true, true, _, _) => {
                // `bool`'s `bit_width` is its storage (8); the value is `i1`.
                let bool_aware = |p: PrimTy| -> u32 {
                    if matches!(p, PrimTy::Bool) {
                        1
                    } else {
                        p.bit_width(self.pw)
                    }
                };
                let from_w = bool_aware(*fp);
                let to_w = bool_aware(*tp);
                match to_w.cmp(&from_w) {
                    std::cmp::Ordering::Greater => {
                        // i1 / u-types zero-extend; signed-int sign-extend.
                        let mn = if matches!(fp, PrimTy::Bool) || !fp.is_signed_int() {
                            "arith.extui"
                        } else {
                            "arith.extsi"
                        };
                        let _ = writeln!(self.body, "    {dst} = {mn} {v} : {from} to {to}");
                    }
                    std::cmp::Ordering::Less => {
                        if matches!(tp, PrimTy::Bool) {
                            // To `i1`: compare with 0.
                            let zero = self.fresh_value();
                            let _ = writeln!(self.body, "    {zero} = arith.constant 0 : {from}");
                            let _ = writeln!(
                                self.body,
                                "    {dst} = arith.cmpi ne, {v}, {zero} : {from}"
                            );
                        } else {
                            let _ = writeln!(
                                self.body,
                                "    {dst} = arith.trunci {v} : {from} to {to}"
                            );
                        }
                    }
                    std::cmp::Ordering::Equal => {
                        return self.unsupported(&format!("cast from `{from}` to `{to}`"));
                    }
                }
            }
            (false, true, true, false) => {
                // float → int saturates, NaN → 0; plain
                // `fptosi` is poison out of range.
                let mn = if tp.is_signed_int() {
                    "fptosi"
                } else {
                    "fptoui"
                };
                let _ = writeln!(
                    self.body,
                    "    {dst} = llvm.call_intrinsic \"llvm.{mn}.sat.{to}.{from}\"({v}) : ({from}) -> {to}"
                );
            }
            (true, false, false, true) => {
                // int → float.
                let mn = if fp.is_signed_int() {
                    "arith.sitofp"
                } else {
                    "arith.uitofp"
                };
                let _ = writeln!(self.body, "    {dst} = {mn} {v} : {from} to {to}");
            }
            (false, false, true, true) => {
                // float → float.
                let from_w = fp.bit_width(self.pw);
                let to_w = tp.bit_width(self.pw);
                match to_w.cmp(&from_w) {
                    std::cmp::Ordering::Greater => {
                        let _ = writeln!(self.body, "    {dst} = arith.extf {v} : {from} to {to}");
                    }
                    std::cmp::Ordering::Less => {
                        let _ =
                            writeln!(self.body, "    {dst} = arith.truncf {v} : {from} to {to}");
                    }
                    std::cmp::Ordering::Equal => {
                        return self.unsupported(&format!("cast from `{from}` to `{to}`"));
                    }
                }
            }
            _ => return self.unsupported(&format!("cast from `{from}` to `{to}`")),
        }
        dst
    }

    /// Bit-preserving scalar move between a payload slot and a variant type:
    /// floats as same-width ints, ints zero-extended or truncated.
    fn bitcast_value(&mut self, v: String, from_ty: &MirTy, to_ty: &MirTy) -> String {
        let pw = self.pw;
        // `()` carries no bits: going in, the slot just reads zero; coming
        // out, it is the one unit value.
        if *from_ty == MirTy::Unit || *to_ty == MirTy::Unit {
            let n = self.fresh_value();
            let to = mlir_type(to_ty, self.target);
            let _ = writeln!(self.body, "    {n} = llvm.mlir.zero : {to}");
            return n;
        }
        // A reference word as a scalar: its address bits (an `any`'s
        // inline payload).
        let usize_ty = MirTy::Prim(PrimTy::Usize);
        match (from_ty, to_ty) {
            (MirTy::GcRef, MirTy::GcRef) => return v,
            (MirTy::GcRef, MirTy::Prim(_) | MirTy::Any) => {
                let w = self.ref_to_int(&v);
                return self.bitcast_value(w, &usize_ty, to_ty);
            }
            (MirTy::Prim(_) | MirTy::Any, MirTy::GcRef) => {
                let w = self.bitcast_value(v, from_ty, &usize_ty);
                return self.int_to_ref(&w);
            }
            (MirTy::Any, _) => return self.bitcast_value(v, &usize_ty, to_ty),
            (_, MirTy::Any) => return self.bitcast_value(v, from_ty, &usize_ty),
            _ => {}
        }
        let (MirTy::Prim(fp), MirTy::Prim(tp)) = (from_ty, to_ty) else {
            // Byte-array union: store as one type, load as the other.
            let (from, to) = (
                mlir_type(from_ty, self.target),
                mlir_type(to_ty, self.target),
            );
            let scratch = [(&from, from_ty), (&to, to_ty)]
                .into_iter()
                .find_map(|(k, t)| Some((self.union_scratch.get(k)?.clone(), t)));
            let Some((slot, slot_ty)) = scratch else {
                return self.unsupported(&format!("payload of `{from}` stored as `{to}`"));
            };
            let _ = writeln!(self.body, "    llvm.store {v}, {slot} : {from}, !llvm.ptr");
            // The tail a shorter payload leaves reads as zero, not stale
            // stack: a root word copied out of the union is null.
            let (fs, ss) = (
                self.target.size_align(from_ty).0,
                self.target.size_align(slot_ty).0,
            );
            if ss > fs {
                let [tail, z, n] = std::array::from_fn(|_| self.fresh_value());
                let _ = write!(
                    self.body,
                    "    {tail} = llvm.getelementptr {slot}[{fs}] : (!llvm.ptr) -> !llvm.ptr, i8
    {z} = llvm.mlir.constant(0 : i8) : i8
    {n} = llvm.mlir.constant({} : i{pw}) : i{pw}
    \"llvm.intr.memset\"({tail}, {z}, {n}) <{{isVolatile = false}}> : (!llvm.ptr, i8, i{pw}) -> ()
",
                    ss - fs
                );
            }
            let n = self.fresh_value();
            let _ = writeln!(self.body, "    {n} = llvm.load {slot} : !llvm.ptr -> {to}");
            return n;
        };
        let width = |p: &PrimTy| {
            if *p == PrimTy::Bool {
                1
            } else {
                p.bit_width(pw)
            }
        };
        let (fw, tw) = (width(fp), width(tp));
        let mut v = v;
        if fp.is_float() {
            let n = self.fresh_value();
            let _ = writeln!(
                self.body,
                "    {n} = arith.bitcast {v} : {} to i{fw}",
                mlir_prim(*fp, pw)
            );
            v = n;
        }
        if fw != tw {
            let n = self.fresh_value();
            let op = if tw > fw {
                "arith.extui"
            } else {
                "arith.trunci"
            };
            let _ = writeln!(self.body, "    {n} = {op} {v} : i{fw} to i{tw}");
            v = n;
        }
        if tp.is_float() {
            let n = self.fresh_value();
            let _ = writeln!(
                self.body,
                "    {n} = arith.bitcast {v} : i{tw} to {}",
                mlir_prim(*tp, pw)
            );
            v = n;
        }
        v
    }

    /// A reference's address bits.
    fn ref_to_int(&mut self, v: &str) -> String {
        let pw = self.pw;
        let n = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {n} = llvm.ptrtoint {v} : !llvm.ptr to i{pw}"
        );
        n
    }

    /// The reference whose address bits are `v` (a `usize`).
    fn int_to_ref(&mut self, v: &str) -> String {
        let pw = self.pw;
        let n = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {n} = llvm.inttoptr {v} : i{pw} to !llvm.ptr"
        );
        n
    }

    // Build a struct value by chaining `llvm.insertvalue` from zero (not
    // `undef`: a GC reference in an unset field must read as null).
    fn emit_struct_init(&mut self, ty: &MirTy, fields: &[Operand]) -> String {
        let ty_str = mlir_type(ty, self.target);
        let acc0 = self.fresh_value();
        let _ = writeln!(self.body, "    {acc0} = llvm.mlir.zero : {ty_str}");
        let field_tys = struct_field_tys(ty);
        let mut acc = acc0;
        for (i, f) in fields.iter().enumerate() {
            let f_ty = field_tys.get(i).cloned().unwrap_or_else(|| match f {
                Operand::Copy(id) => self.local_ty(*id),
                Operand::Const(c) => const_ty(c),
            });
            let v = self.materialize_operand(f, &f_ty);
            let next = self.fresh_value();
            let i = llvm_field(ty, i, self.target);
            let _ = writeln!(
                self.body,
                "    {next} = llvm.insertvalue {v}, {acc}[{i}] : {ty_str}"
            );
            acc = next;
        }
        acc
    }

    fn emit_field_read(
        &mut self,
        target: &Operand,
        index: u32,
        field_ty: &MirTy,
        _dst_ty: &MirTy,
    ) -> String {
        let target_ty = match target {
            Operand::Copy(id) => self.local_ty(*id),
            Operand::Const(c) => const_ty(c),
        };
        if let Some(p) = self.field_addr(target, &target_ty, index) {
            let ty = mlir_type(field_ty, self.target);
            let dst = self.fresh_value();
            let _ = writeln!(self.body, "    {dst} = llvm.load {p} : !llvm.ptr -> {ty}");
            return dst;
        }
        let target_v = self.materialize_operand(target, &target_ty);
        let target_ty_str = mlir_type(&target_ty, self.target);
        let index = llvm_field(&target_ty, index as usize, self.target);
        let dst = self.fresh_value();
        let _ = writeln!(
            self.body,
            "    {dst} = llvm.extractvalue {target_v}[{index}] : {target_ty_str}"
        );
        let _ = field_ty;
        dst
    }

    fn emit_call(&mut self, dst: &str, callee: FunctionId, args: &[Operand]) {
        let fns = self.fns;
        let sig = &fns[&callee];
        if sig.runtime && self.emit_inline_runtime(dst, &sig.name, args) {
            return;
        }
        // A debug rokc brackets a `#[no_safepoint]` call, so the runtime
        // can tell if one ever waits for a collection.
        // A runtime function is quiet only by `#[no_safepoint]`.
        let check = cfg!(debug_assertions) && sig.runtime && self.quiet.contains(&callee);
        if check {
            self.needs.insert("rok_nosp_enter");
            self.needs.insert("rok_nosp_leave");
            self.body
                .push_str("    func.call @rok_nosp_enter() : () -> ()\n");
        }
        self.emit_call_inner(dst, sig, callee, args);
        if check {
            self.body
                .push_str("    func.call @rok_nosp_leave() : () -> ()\n");
        }
    }

    fn emit_call_inner(&mut self, dst: &str, sig: &FnSig, callee: FunctionId, args: &[Operand]) {
        if let Some(abi) = &sig.c {
            self.emit_c_call(
                dst,
                &sig.params,
                &sig.ret,
                abi,
                Callee::Symbol(&sig.name),
                args,
            );
            return;
        }
        // Each argument at the callee's parameter type, so the operand
        // list matches the declaration.
        let into = self.sret_into.is_some();
        let (mut arg_vals, sret) = self.rok_call_args(args, &sig.params, &sig.ret);
        if sig.tls {
            arg_vals.push(self.tls_base());
        }
        let (arg_tys, _) = sig.rok_types(self.target);
        let (name, list, tys) = (&sig.name, arg_vals.join(", "), arg_tys.join(", "));
        let scopes = self.call_scopes(callee, args);
        if let Some(r) = sret {
            let _ = writeln!(
                self.body,
                "    func.call @{name}({list}){scopes} : ({tys}) -> ()"
            );
            if !into {
                let ret = mlir_type(&sig.ret, self.target);
                let _ = writeln!(self.body, "    {dst} = llvm.load {r} : !llvm.ptr -> {ret}");
            }
        } else if sig.ret == MirTy::Unit {
            let _ = write!(
                self.body,
                "    func.call @{name}({list}){scopes} : ({tys}) -> ()
    {dst} = arith.constant 0 : i1
"
            );
        } else {
            let ret = mlir_type(&sig.ret, self.target);
            let _ = writeln!(
                self.body,
                "    {dst} = func.call @{name}({list}){scopes} : ({tys}) -> {ret}"
            );
        }
    }

    /// Alias scopes of a call touching only its first argument's list, held
    /// by a scoped local (`crate::alias`): its scope, `noalias` for others.
    fn call_scopes(&self, callee: FunctionId, args: &[Operand]) -> String {
        let Some(Operand::Copy(l)) = args.first() else {
            return String::new();
        };
        if !self.list_alias.confined(callee) || !self.list_scopes.contains(&l.0) {
            return String::new();
        }
        let sym = &self.fns[&self.f.id].name;
        let scope = |x: u32| {
            format!(
                "#llvm.alias_scope<id = \"{sym}.{x}\", domain = #llvm.alias_scope_domain<id = \"{sym}\">>"
            )
        };
        let others: Vec<String> = self
            .list_scopes
            .iter()
            .filter(|&&x| x != l.0)
            .map(|&x| scope(x))
            .collect();
        format!(
            " {{alias_scopes = [{}], noalias_scopes = [{}]}}",
            scope(l.0),
            others.join(", ")
        )
    }

    /// Runtime function `name` done inline (a `str`'s length, one of its
    /// bytes, which the library reads in range); false for any other.
    fn emit_inline_runtime(&mut self, dst: &str, name: &str, args: &[Operand]) -> bool {
        let pw = self.pw;
        let str_ty = MirTy::Prim(PrimTy::Str);
        let sty = mlir_type(&str_ty, self.target);
        match (name, args) {
            ("rok_str_len", [s]) => {
                let s = self.materialize_operand(s, &str_ty);
                let _ = writeln!(self.body, "    {dst} = llvm.extractvalue {s}[1] : {sty}");
            }
            ("rok_str_byte", [s, i]) => {
                let s = self.materialize_operand(s, &str_ty);
                let i = self.materialize_operand(i, &MirTy::Prim(PrimTy::Usize));
                let [p, q] = std::array::from_fn(|_| self.fresh_value());
                let _ = write!(
                    self.body,
                    "    {p} = llvm.extractvalue {s}[0] : {sty}
    {q} = llvm.getelementptr {p}[{i}] : (!llvm.ptr, i{pw}) -> !llvm.ptr, i8
    {dst} = llvm.load {q} : !llvm.ptr -> i8
"
                );
            }
            // Up to 16 bytes: compare the first and last `w` bytes of each
            // side (`w` = 8, 4, 2, 1 covers `w..=2w`) instead of a call.
            ("rok_str_eq", [lhs, rhs]) => {
                let sa = self.materialize_operand(lhs, &str_ty);
                let sb = self.materialize_operand(rhs, &str_ty);
                let k = self.next_check;
                self.next_check += 1;
                let c16 = self.int_const(16);
                let (ra, ra_ty) = self.runtime_str(&sa);
                let (rb, rb_ty) = self.runtime_str(&sb);
                let [pa, la, pb, lb, same, small, no, yes, big] =
                    std::array::from_fn(|_| self.fresh_value());
                let _ = write!(
                    self.body,
                    "    {pa} = llvm.extractvalue {sa}[0] : {sty}
    {la} = llvm.extractvalue {sa}[1] : {sty}
    {pb} = llvm.extractvalue {sb}[0] : {sty}
    {lb} = llvm.extractvalue {sb}[1] : {sty}
    {no} = arith.constant 0 : i32
    {yes} = arith.constant 1 : i32
    {same} = arith.cmpi eq, {la}, {lb} : i{pw}
    cf.cond_br {same}, ^rok_eqn{k}, ^rok_eqend{k}({no} : i32)
  ^rok_eqn{k}:
    {small} = arith.cmpi ule, {la}, {c16} : i{pw}
    cf.cond_br {small}, ^rok_eqs8_{k}, ^rok_eqbig{k}
  ^rok_eqbig{k}:
    {big} = func.call @{name}({ra}, {rb}) : ({ra_ty}, {rb_ty}) -> i32
    cf.br ^rok_eqend{k}({big} : i32)
"
                );
                for (w, next) in [(8, "s4_"), (4, "s2_"), (2, "s1_"), (1, "z")] {
                    let wc = self.int_const(w);
                    let [ge, off, qa, qb, x0, y0, x1, y1, d0, d1, dx, ez, eq, ew] =
                        std::array::from_fn(|_| self.fresh_value());
                    let bits = w * 8;
                    let _ = write!(
                        self.body,
                        "  ^rok_eqs{w}_{k}:
    {ge} = arith.cmpi uge, {la}, {wc} : i{pw}
    cf.cond_br {ge}, ^rok_eqw{w}_{k}, ^rok_eq{next}{k}
  ^rok_eqw{w}_{k}:
    {off} = arith.subi {la}, {wc} : i{pw}
    {qa} = llvm.getelementptr {pa}[{off}] : (!llvm.ptr, i{pw}) -> !llvm.ptr, i8
    {qb} = llvm.getelementptr {pb}[{off}] : (!llvm.ptr, i{pw}) -> !llvm.ptr, i8
    {x0} = llvm.load {pa} {{alignment = 1 : i64}} : !llvm.ptr -> i{bits}
    {y0} = llvm.load {pb} {{alignment = 1 : i64}} : !llvm.ptr -> i{bits}
    {x1} = llvm.load {qa} {{alignment = 1 : i64}} : !llvm.ptr -> i{bits}
    {y1} = llvm.load {qb} {{alignment = 1 : i64}} : !llvm.ptr -> i{bits}
    {d0} = arith.xori {x0}, {y0} : i{bits}
    {d1} = arith.xori {x1}, {y1} : i{bits}
    {dx} = arith.ori {d0}, {d1} : i{bits}
    {ez} = arith.constant 0 : i{bits}
    {eq} = arith.cmpi eq, {dx}, {ez} : i{bits}
    {ew} = arith.extui {eq} : i1 to i32
    cf.br ^rok_eqend{k}({ew} : i32)
"
                    );
                }
                // Here the length is 0.
                let _ = write!(
                    self.body,
                    "  ^rok_eqz{k}:
    cf.br ^rok_eqend{k}({yes} : i32)
  ^rok_eqend{k}({dst}: i32):
"
                );
            }
            _ => return self.emit_float_op(dst, name, args),
        }
        true
    }

    /// The exact `rok_f64_*` / `rok_f32_*` math functions (rounding,
    /// `abs`, `copysign`, `min` / `max`, `mul_add`) as `math` / `arith`
    /// ops: one instruction or an LLVM intrinsic, not a runtime call. The
    /// transcendental ones stay libm calls.
    fn emit_float_op(&mut self, dst: &str, name: &str, args: &[Operand]) -> bool {
        let (prim, op) = match name.strip_prefix("rok_f64_") {
            Some(op) => (PrimTy::F64, op),
            None => match name.strip_prefix("rok_f32_") {
                Some(op) => (PrimTy::F32, op),
                None => return false,
            },
        };
        let mlir_op = match op {
            "floor" => "math.floor",
            "ceil" => "math.ceil",
            "round" => "math.round",
            "trunc" => "math.trunc",
            "abs" => "math.absf",
            "copysign" => "math.copysign",
            // `fmin` / `fmax`: a NaN operand is ignored.
            "min" => "arith.minnumf",
            "max" => "arith.maxnumf",
            "mul_add" => "math.fma",
            _ => return false,
        };
        let ty = MirTy::Prim(prim);
        let vs: Vec<String> = args
            .iter()
            .map(|a| self.materialize_operand(a, &ty))
            .collect();
        let t = mlir_type(&ty, self.target);
        let _ = writeln!(self.body, "    {dst} = {mlir_op} {} : {t}", vs.join(", "));
        true
    }

    /// A pointer-width integer constant.
    fn int_const(&mut self, v: i64) -> String {
        let c = self.fresh_value();
        let pw = self.pw;
        let _ = writeln!(self.body, "    {c} = arith.constant {v} : i{pw}");
        c
    }

    /// A C-ABI call (`abi::classify`), to a declaration or an `extern "C"
    /// fn` pointer. `str` goes as `rok_str_cstr`'s `char*` (the bytes, or a
    /// pinned copy of a slice released after) and comes back measured
    /// (`rok_cstr_len`); aggregates go through memory as their coerced type;
    /// arguments past `params` are C variadics.
    fn emit_c_call(
        &mut self,
        dst: &str,
        params: &[MirTy],
        ret: &MirTy,
        abi: &FnAbi,
        callee: Callee<'_>,
        args: &[Operand],
    ) {
        let pw = self.pw;
        let str_ty = MirTy::Prim(PrimTy::Str);
        let mut ops: Vec<String> = Vec::new();
        let mut op_tys: Vec<String> = Vec::new();
        let mut c_strs: Vec<(String, String)> = Vec::new();
        let ret_mlir = mlir_type(ret, self.target);
        let sret = if abi.ret == RetAbi::Sret {
            let slot = self.entry_alloca(&ret_mlir, self.target.size_align(ret).1);
            ops.push(slot.clone());
            op_tys.push("!llvm.ptr".to_owned());
            Some(slot)
        } else {
            None
        };
        for (i, op) in args.iter().enumerate() {
            let t = match (params.get(i), op) {
                (Some(t), _) => t.clone(),
                (None, Operand::Copy(id)) => self.local_ty(*id),
                (None, Operand::Const(c)) => const_ty(c),
            };
            let mut v = self.materialize_operand(op, &t);
            if t == str_ty && !abi.rok {
                let p = self.fresh_value();
                let (a, a_ty) = self.runtime_str(&v);
                let _ = writeln!(
                    self.body,
                    "    {p} = func.call @rok_str_cstr({a}) : ({a_ty}) -> !llvm.ptr"
                );
                self.needs.insert("rok_str_cstr");
                c_strs.push((v, p.clone()));
                v = p;
            }
            let ty = mlir_type(&t, self.target);
            match abi.args.get(i).unwrap_or(&ArgAbi::Direct) {
                ArgAbi::Direct => {
                    ops.push(v);
                    op_tys.push(c_type(&t, self.target));
                }
                ArgAbi::Coerce(c) => {
                    // Store as the aggregate, reload as the coerced type.
                    let slot = self.coerce_slot(&t);
                    let r = self.fresh_value();
                    let _ = write!(
                        self.body,
                        "    llvm.store {v}, {slot} : {ty}, !llvm.ptr
    {r} = llvm.load {slot} : !llvm.ptr -> {c}
"
                    );
                    ops.push(r);
                    op_tys.push(c.clone());
                }
                ArgAbi::Indirect { .. } => {
                    let slot = self.entry_alloca(&ty, self.target.size_align(&t).1.max(8));
                    let _ = writeln!(self.body, "    llvm.store {v}, {slot} : {ty}, !llvm.ptr");
                    ops.push(slot);
                    op_tys.push("!llvm.ptr".to_owned());
                }
            }
        }
        let call_ret = match &abi.ret {
            RetAbi::Direct if *ret != MirTy::Unit => Some(c_type(ret, self.target)),
            RetAbi::Coerce(c) => Some(c.clone()),
            _ => None,
        };
        let plain = abi.ret == RetAbi::Direct && *ret != str_ty;
        let res = call_ret.as_ref().map(|_| {
            if plain {
                dst.to_owned()
            } else {
                self.fresh_value()
            }
        });
        let lhs = res.as_ref().map(|r| format!("{r} = ")).unwrap_or_default();
        let rty = call_ret.clone().unwrap_or_else(|| "()".to_owned());
        let (list, tlist) = (ops.join(", "), op_tys.join(", "));
        // A C call may block: the thread is `PARKED` meanwhile,
        // its references rooted in the frame. Not for runtime
        // calls, nor `rok_*` symbols declared `extern "C"` (they may
        // allocate, which must not overlap another thread's collection).
        let parked = !abi.rok && !matches!(callee, Callee::Symbol(n) if n.starts_with("rok_"));
        if parked {
            self.emit_native_edge(true);
        }
        // Parameter attributes (`sret` in `x8` on arm64, `byval`, `signext`
        // / `zeroext`) go on the call too: LLVM reads some from the call
        // alone (a `tail` call reaches a caller `alloca` only via `byval`).
        let mut attrs: Vec<String> = c_signature(params, ret, abi, self.target)
            .0
            .into_iter()
            .map(|(_, a)| match a.trim() {
                "" => "{}".to_owned(),
                a => a.to_owned(),
            })
            .collect();
        attrs.resize(ops.len(), "{}".to_owned());
        let attrs = format!("arg_attrs = [{}]", attrs.join(", "));
        match callee {
            Callee::Symbol(name) if abi.variadic => {
                // The fixed part of the prototype: the hidden sret
                // pointer and the declared parameters.
                let fixed = usize::from(sret.is_some()) + params.len();
                let fn_ret = call_ret.unwrap_or_else(|| "!llvm.void".to_owned());
                let mut proto: Vec<&str> = op_tys[..fixed].iter().map(String::as_str).collect();
                proto.push("...");
                let _ = writeln!(
                    self.body,
                    "    {lhs}llvm.call @{name}({list}) vararg(!llvm.func<{fn_ret} ({})>) {{{attrs}}} : ({tlist}) -> {rty}",
                    proto.join(", ")
                );
            }
            Callee::Symbol(name) if abi.stdcall => {
                let _ = writeln!(
                    self.body,
                    "    {lhs}llvm.call x86_stdcallcc @{name}({list}) {{{attrs}}} : ({tlist}) -> {rty}"
                );
            }
            Callee::Symbol(name) if REF_ONLY_CALLS.contains(&name) => {
                let _ = writeln!(
                    self.body,
                    "    {lhs}llvm.call @{name}({list}) {{{attrs}, tbaa = [#rok_tbaa_ref]}} : ({tlist}) -> {rty}"
                );
            }
            Callee::Symbol(name) => {
                let _ = writeln!(
                    self.body,
                    "    {lhs}func.call @{name}({list}) {{{attrs}}} : ({tlist}) -> {rty}"
                );
            }
            Callee::Ptr(p) => {
                let cc = if abi.stdcall { "x86_stdcallcc " } else { "" };
                let _ = writeln!(
                    self.body,
                    "    {lhs}llvm.call {cc}{p}({list}) {{{attrs}}} : !llvm.ptr, ({tlist}) -> {rty}"
                );
            }
        }
        if parked {
            self.emit_native_edge(false);
        }
        for (v, p) in &c_strs {
            let (a, a_ty) = self.runtime_str(v);
            let _ = writeln!(
                self.body,
                "    func.call @rok_str_cstr_done({a}, {p}) : ({a_ty}, !llvm.ptr) -> ()"
            );
            self.needs.insert("rok_str_cstr_done");
        }
        match (&abi.ret, res) {
            (RetAbi::Sret, _) => {
                let slot = sret.expect("sret slot");
                let _ = writeln!(
                    self.body,
                    "    {dst} = llvm.load {slot} : !llvm.ptr -> {ret_mlir}"
                );
            }
            (RetAbi::Coerce(c), Some(r)) => {
                let slot = self.coerce_slot(ret);
                let _ = write!(
                    self.body,
                    "    llvm.store {r}, {slot} : {c}, !llvm.ptr
    {dst} = llvm.load {slot} : !llvm.ptr -> {ret_mlir}
"
                );
            }
            (_, Some(r)) if *ret == str_ty => {
                // A `char*` result becomes `(ptr, strlen)`; null reads as "".
                let [l, u, h] = std::array::from_fn(|_| self.fresh_value());
                let _ = write!(
                    self.body,
                    "    {l} = func.call @rok_cstr_len({r}) : (!llvm.ptr) -> i{pw}
    {u} = llvm.mlir.undef : {ret_mlir}
    {h} = llvm.insertvalue {r}, {u}[0] : {ret_mlir}
    {dst} = llvm.insertvalue {l}, {h}[1] : {ret_mlir}
"
                );
                self.needs.insert("rok_cstr_len");
            }
            (_, Some(_)) => {} // `dst` is the call's own result.
            (_, None) => {
                let _ = writeln!(self.body, "    {dst} = arith.constant 0 : i1");
            }
        }
    }

    /// `{dst} = str` value of a literal: its NUL-terminated global and
    /// byte length.
    fn str_literal_into(&mut self, dst: &str, text: &str) {
        let pw = self.pw;
        let sym = self.intern_string(text);
        let ty = mlir_prim(PrimTy::Str, pw);
        let [p, l, u, h] = std::array::from_fn(|_| self.fresh_value());
        let _ = write!(
            self.body,
            "    {p} = llvm.mlir.addressof @{sym} : !llvm.ptr
    {l} = arith.constant {} : i{pw}
    {u} = llvm.mlir.undef : {ty}
    {h} = llvm.insertvalue {p}, {u}[0] : {ty}
    {dst} = llvm.insertvalue {l}, {h}[1] : {ty}
",
            text.len()
        );
    }

    /// Record an unsupported construct; returns a dummy SSA name so
    /// emission can continue and collect further errors.
    fn unsupported(&mut self, what: &str) -> String {
        self.errors.push(format!("unsupported in codegen: {what}"));
        self.fresh_value()
    }

    fn unsupported_at(&mut self, what: &str) {
        self.errors.push(format!("unsupported in codegen: {what}"));
    }

    /// Branch to `rok_panic_at(msg, location)` when `fail` (`i1`); continues
    /// in a fresh block. The location is this statement's, or the one a
    /// `#[track_caller]` function received.
    fn emit_check(&mut self, fail: &str, msg: &str) {
        let n = self.next_check;
        self.next_check += 1;
        let _ = writeln!(
            self.body,
            "    cf.cond_br {fail}, ^rok_panic{n}, ^rok_ok{n}"
        );
        let _ = writeln!(self.body, "  ^rok_panic{n}:");
        let s = self.fresh_value();
        self.str_literal_into(&s, msg);
        let at = if self.f.facts.track_caller {
            let first = usize::from(rok_by_ref(&self.f.ret_ty, self.target));
            format!("%arg{}", self.f.params.len() - 1 + first)
        } else {
            let at = self.fresh_value();
            self.str_literal_into(&at, &self.sources.location(self.at));
            at
        };
        let (s, s_ty) = self.runtime_str(&s);
        let (at, at_ty) = self.runtime_str(&at);
        let _ = writeln!(
            self.body,
            "    func.call @rok_panic_at({s}, {at}) : ({s_ty}, {at_ty}) -> ()"
        );
        self.body.push_str("    llvm.unreachable\n");
        let _ = writeln!(self.body, "  ^rok_ok{n}:");
        self.needs.insert("rok_panic_at");
    }

    /// `fail = (v <pred> c)` for an integer constant `c`.
    fn cmp_const(&mut self, pred: &str, v: &str, c: i128, ty: &str) -> String {
        let k = self.fresh_value();
        let r = self.fresh_value();
        let _ = writeln!(self.body, "    {k} = arith.constant {c} : {ty}");
        let _ = writeln!(self.body, "    {r} = arith.cmpi {pred}, {v}, {k} : {ty}");
        r
    }

    /// Checks that panic instead of hitting LLVM poison / UB: division by
    /// zero, signed `MIN / -1`, shifts at or past the width; skipped for
    /// constants that are fine.
    fn emit_arith_checks(
        &mut self,
        op: PrimOp,
        prim: PrimTy,
        rhs: &Operand,
        lhs_v: &str,
        rhs_v: &str,
        ty: &str,
    ) {
        if !prim.is_int() {
            return;
        }
        let rhs_const = const_int(rhs);
        let bits = prim.bit_width(self.pw);
        match op {
            PrimOp::Div | PrimOp::Rem => {
                let (zero_msg, ovf_msg) = if op == PrimOp::Div {
                    (
                        "attempt to divide by zero",
                        "attempt to divide with overflow",
                    )
                } else {
                    (
                        "attempt to calculate the remainder with a divisor of zero",
                        "attempt to calculate the remainder with overflow",
                    )
                };
                if rhs_const.is_none_or(|c| c == 0) {
                    let fail = self.cmp_const("eq", rhs_v, 0, ty);
                    self.emit_check(&fail, zero_msg);
                }
                if prim.is_signed_int() && rhs_const.is_none_or(|c| c == -1) {
                    let is_min = self.cmp_const("eq", lhs_v, -(1i128 << (bits - 1)), ty);
                    let is_neg1 = self.cmp_const("eq", rhs_v, -1, ty);
                    let fail = self.fresh_value();
                    let _ = writeln!(
                        self.body,
                        "    {fail} = arith.andi {is_min}, {is_neg1} : i1"
                    );
                    self.emit_check(&fail, ovf_msg);
                }
            }
            PrimOp::Shl | PrimOp::Shr => {
                if rhs_const.is_some_and(|c| (0..i128::from(bits)).contains(&c)) {
                    return;
                }
                // Unsigned compare also rejects negative amounts.
                let fail = self.cmp_const("uge", rhs_v, i128::from(bits), ty);
                let msg = if op == PrimOp::Shl {
                    "attempt to shift left with overflow"
                } else {
                    "attempt to shift right with overflow"
                };
                self.emit_check(&fail, msg);
            }
            _ => {}
        }
    }

    /// Overflow check on integer `+ - *` without widening: sign tricks for
    /// add / sub, the extended product's high half for mul.
    fn emit_overflow_check(
        &mut self,
        op: PrimOp,
        prim: PrimTy,
        dst: &str,
        (lhs, rhs): (&str, &str),
        consts: (Option<i128>, Option<i128>),
        ty: &str,
    ) {
        let signed = prim.is_signed_int();
        if !self.overflow_checks
            || !(signed || prim.is_unsigned_int())
            || cannot_overflow(op, prim, prim.bit_width(self.pw), consts)
        {
            return;
        }
        let fail = match op {
            PrimOp::Add | PrimOp::Sub if signed => {
                // add: lhs and rhs share a sign that dst lacks.
                // sub: lhs and rhs differ in sign and dst's differs from lhs.
                let pairs = if op == PrimOp::Add {
                    [(lhs, dst), (rhs, dst)]
                } else {
                    [(lhs, rhs), (lhs, dst)]
                };
                let [x0, x1] = pairs.map(|(p, q)| {
                    let v = self.fresh_value();
                    let _ = writeln!(self.body, "    {v} = arith.xori {p}, {q} : {ty}");
                    v
                });
                let both = self.fresh_value();
                let _ = writeln!(self.body, "    {both} = arith.andi {x0}, {x1} : {ty}");
                self.cmp_const("slt", &both, 0, ty)
            }
            PrimOp::Add => self.cmp2("ult", dst, lhs, ty),
            PrimOp::Sub => self.cmp2("ult", lhs, rhs, ty),
            PrimOp::Mul => {
                let lo = self.fresh_value();
                let hi = self.fresh_value();
                let ext = if signed {
                    "mulsi_extended"
                } else {
                    "mului_extended"
                };
                let _ = writeln!(
                    self.body,
                    "    {lo}, {hi} = arith.{ext} {lhs}, {rhs} : {ty}"
                );
                if signed {
                    // No overflow iff the high half is the sign extension of lo.
                    let bits = prim.bit_width(self.pw);
                    let k = self.fresh_value();
                    let sign = self.fresh_value();
                    let _ = writeln!(self.body, "    {k} = arith.constant {} : {ty}", bits - 1);
                    let _ = writeln!(self.body, "    {sign} = arith.shrsi {lo}, {k} : {ty}");
                    self.cmp2("ne", &hi, &sign, ty)
                } else {
                    self.cmp_const("ne", &hi, 0, ty)
                }
            }
            _ => return,
        };
        let msg = match op {
            PrimOp::Add => "attempt to add with overflow",
            PrimOp::Sub => "attempt to subtract with overflow",
            _ => "attempt to multiply with overflow",
        };
        self.emit_check(&fail, msg);
    }

    /// `fail = (a <pred> b)`.
    fn cmp2(&mut self, pred: &str, a: &str, b: &str, ty: &str) -> String {
        let r = self.fresh_value();
        let _ = writeln!(self.body, "    {r} = arith.cmpi {pred}, {a}, {b} : {ty}");
        r
    }

    fn emit_bin_op(
        &mut self,
        dst: &str,
        _dst_ty: &MirTy,
        op: PrimOp,
        op_ty: &MirTy,
        lhs: &Operand,
        rhs: &Operand,
    ) {
        let lhs_v = self.materialize_operand(lhs, op_ty);
        let rhs_v = self.materialize_operand(rhs, op_ty);
        let ty = mlir_type(op_ty, self.target);
        let MirTy::Prim(prim) = op_ty else {
            self.unsupported_at(&format!("binary `{op:?}` on `{ty}`"));
            return;
        };
        // String literal patterns: content equality.
        if *prim == PrimTy::Str && matches!(op, PrimOp::Eq | PrimOp::Ne) {
            let c = self.fresh_value();
            let z = self.fresh_value();
            let pred = if op == PrimOp::Eq { "eq" } else { "ne" };
            let (l, l_ty) = self.runtime_str(&lhs_v);
            let (r, r_ty) = self.runtime_str(&rhs_v);
            let _ = write!(
                self.body,
                "    {c} = func.call @rok_str_cmp({l}, {r}) : ({l_ty}, {r_ty}) -> i32
    {z} = arith.constant 0 : i32
    {dst} = arith.cmpi {pred}, {c}, {z} : i32
"
            );
            self.needs.insert("rok_str_cmp");
            return;
        }
        self.emit_arith_checks(op, *prim, rhs, &lhs_v, &rhs_v, &ty);
        if let Some(line) = arith_binop(op, *prim, dst, &lhs_v, &rhs_v, &ty) {
            self.body.push_str(&line);
            let consts = (const_int(lhs), const_int(rhs));
            self.emit_overflow_check(op, *prim, dst, (&lhs_v, &rhs_v), consts, &ty);
            return;
        }
        self.unsupported_at(&format!("binary `{op:?}` on `{ty}`"));
    }

    fn emit_unary_op(
        &mut self,
        dst: &str,
        dst_ty: &MirTy,
        op: PrimOp,
        op_ty: &MirTy,
        operand: &Operand,
    ) {
        let v = self.materialize_operand(operand, op_ty);
        let ty = mlir_type(op_ty, self.target);
        let MirTy::Prim(prim) = op_ty else {
            let _ = dst_ty;
            self.unsupported_at(&format!("unary `{op:?}` on `{ty}`"));
            return;
        };
        match (op, prim) {
            (PrimOp::Neg, p) if p.is_signed_int() => {
                // arith has no `negi`; lower to `0 - x`.
                let zero = self.fresh_value();
                let _ = writeln!(self.body, "    {zero} = arith.constant 0 : {ty}");
                let _ = writeln!(self.body, "    {dst} = arith.subi {zero}, {v} : {ty}");
                let bits = p.bit_width(self.pw);
                if self.overflow_checks
                    && const_int(operand).is_none_or(|c| c == -(1i128 << (bits - 1)))
                {
                    let fail = self.cmp_const("eq", &v, -(1i128 << (bits - 1)), &ty);
                    self.emit_check(&fail, "attempt to negate with overflow");
                }
            }
            (PrimOp::Neg, p) if p.is_float() => {
                let _ = writeln!(self.body, "    {dst} = arith.negf {v} : {ty}");
            }
            (PrimOp::Sqrt, p) if p.is_float() => {
                let _ = writeln!(self.body, "    {dst} = math.sqrt {v} : {ty}");
            }
            (PrimOp::CountOnes | PrimOp::SwapBytes, p) if p.is_int() => {
                let name = if op == PrimOp::CountOnes {
                    "ctpop"
                } else {
                    "bswap"
                };
                let _ = writeln!(
                    self.body,
                    "    {dst} = llvm.intr.{name}({v}) : ({ty}) -> {ty}"
                );
            }
            (PrimOp::LeadingZeros | PrimOp::TrailingZeros, p) if p.is_int() => {
                let name = if op == PrimOp::LeadingZeros {
                    "ctlz"
                } else {
                    "cttz"
                };
                let _ = writeln!(
                    self.body,
                    "    {dst} = \"llvm.intr.{name}\"({v}) <{{is_zero_poison = false}}> : ({ty}) -> {ty}"
                );
            }
            (PrimOp::Not, p) if p.is_int() => {
                // Bitwise NOT = xor with all-ones.
                let ones = self.fresh_value();
                let _ = writeln!(self.body, "    {ones} = arith.constant -1 : {ty}");
                let _ = writeln!(self.body, "    {dst} = arith.xori {v}, {ones} : {ty}");
            }
            (PrimOp::LogicalNot, PrimTy::Bool) => {
                let one = self.fresh_value();
                let _ = writeln!(self.body, "    {one} = arith.constant 1 : i1");
                let _ = writeln!(self.body, "    {dst} = arith.xori {v}, {one} : i1");
            }
            _ => self.unsupported_at(&format!("unary `{op:?}` on `{ty}`")),
        }
    }

    fn emit_const_assign(&mut self, dst: &str, dst_ty: &MirTy, c: &ConstVal) {
        let ty = mlir_type(dst_ty, self.target);
        match c {
            // Numeric literals take a numeric destination's type, else keep
            // their own.
            ConstVal::Int { ty: prim, .. } | ConstVal::UInt { ty: prim, .. } => {
                let (value, as_f64) = match c {
                    ConstVal::Int { value, .. } => (value.to_string(), *value as f64),
                    ConstVal::UInt { value, .. } => (value.to_string(), *value as f64),
                    _ => unreachable!(),
                };
                let lit = match dst_ty {
                    MirTy::GcRef => {
                        let pw = self.pw;
                        let w = self.fresh_value();
                        let _ = write!(
                            self.body,
                            "    {w} = arith.constant {value} : i{pw}\n    {dst} = llvm.inttoptr {w} : i{pw} to !llvm.ptr\n"
                        );
                        return;
                    }
                    MirTy::Any => ty.clone(),
                    MirTy::Prim(p) if p.is_int() => ty.clone(),
                    MirTy::Prim(p) if p.is_float() => {
                        let f = format_float_decimal(as_f64);
                        let _ = writeln!(self.body, "    {dst} = arith.constant {f} : {ty}");
                        return;
                    }
                    _ => mlir_prim(*prim, self.pw),
                };
                let _ = writeln!(self.body, "    {dst} = arith.constant {value} : {lit}");
            }
            ConstVal::Bool(b) => {
                let _ = writeln!(
                    self.body,
                    "    {dst} = arith.constant {} : i1",
                    u32::from(*b)
                );
            }
            ConstVal::Float { bits, ty: prim } => {
                // `f32` / `f64` destination wins over the literal's width.
                let (prim, val) = match dst_ty {
                    MirTy::Prim(p) if p.is_float() && p != prim => {
                        let v = if *prim == PrimTy::F32 {
                            f64::from(f32::from_bits(*bits as u32))
                        } else {
                            f64::from_bits(*bits)
                        };
                        let b = if *p == PrimTy::F32 {
                            u64::from((v as f32).to_bits())
                        } else {
                            v.to_bits()
                        };
                        (*p, b)
                    }
                    _ => (*prim, *bits),
                };
                let lit = format_float(val, prim);
                let _ = writeln!(
                    self.body,
                    "    {dst} = arith.constant {lit} : {}",
                    mlir_prim(prim, self.pw)
                );
            }
            ConstVal::Unit | ConstVal::None if *dst_ty == MirTy::Unit => {
                let _ = writeln!(self.body, "    {dst} = llvm.mlir.zero : {UNIT_TY}");
            }
            ConstVal::Unit | ConstVal::None => {
                // Zero value of the destination: a zeroed aggregate (null
                // references) or a zero constant (also a `throws` carrier's
                // unused payload). Unit shouldn't get here; a placeholder
                // keeps the IR well-typed.
                if matches!(
                    dst_ty,
                    MirTy::Struct(_, _)
                        | MirTy::Tuple(_)
                        | MirTy::Array(_, _)
                        | MirTy::Union(..)
                        | MirTy::Prim(PrimTy::Str)
                ) {
                    let _ = writeln!(self.body, "    {dst} = llvm.mlir.zero : {ty}");
                } else if matches!(dst_ty, MirTy::Fn(_, _) | MirTy::GcRef) {
                    let _ = writeln!(self.body, "    {dst} = llvm.mlir.zero : !llvm.ptr");
                } else if matches!(dst_ty, MirTy::Prim(PrimTy::F32 | PrimTy::F64)) {
                    let _ = writeln!(self.body, "    {dst} = arith.constant 0.0 : {ty}");
                } else {
                    let _ = writeln!(self.body, "    {dst} = arith.constant 0 : {ty}");
                }
            }
            ConstVal::Char(c) => {
                let _ = writeln!(self.body, "    {dst} = arith.constant {c} : {ty}");
            }
            ConstVal::Str(s) => self.str_literal_into(dst, s),
            ConstVal::Bytes(_) => unreachable!("MIR lowering expands byte arrays"),
        }
    }

    /// `^rok_re<H>` forwards its arguments to header `H`; its
    /// `llvm.sideeffect` (no code, no memory) keeps it from being folded.
    fn emit_reentry(&mut self, b: &BasicBlock) {
        let params: Vec<(String, String)> = b
            .params
            .iter()
            .map(|p| {
                let ty = mlir_type(&self.local_ty(*p), self.target);
                (self.fresh_value(), ty)
            })
            .collect();
        let h = b.id.0;
        if params.is_empty() {
            let _ = writeln!(self.body, "  ^rok_re{h}:\n{KEEP}    cf.br ^bb{h}");
            return;
        }
        let decl: Vec<String> = params.iter().map(|(v, t)| format!("{v}: {t}")).collect();
        let vals: Vec<&str> = params.iter().map(|(v, _)| v.as_str()).collect();
        let tys: Vec<&str> = params.iter().map(|(_, t)| t.as_str()).collect();
        let _ = writeln!(
            self.body,
            "  ^rok_re{h}({}):\n{KEEP}    cf.br ^bb{h}({} : {})",
            decl.join(", "),
            vals.join(", "),
            tys.join(", ")
        );
    }

    fn emit_terminator(&mut self, b: u32, t: &Terminator) {
        self.edge_from = (b, false);
        let polled = matches!(t, Terminator::Goto { .. } | Terminator::If { .. })
            && self
                .back_edges
                .get(&b)
                .is_some_and(|hs| hs.iter().any(|h| self.reentry.contains(h)));
        if let Terminator::If {
            cond,
            then_bb,
            then_args,
            else_bb,
            else_args,
        } = t
        {
            // One edge goes back (a rotated loop's latch): test, then
            // poll on that edge only.
            let back = self.back_edges.get(&b);
            let to_then = back.is_some_and(|hs| hs.contains(&then_bb.0));
            let to_else = back.is_some_and(|hs| hs.contains(&else_bb.0));
            if polled && to_then != to_else {
                let v = self.materialize_operand(cond, &MirTy::Prim(PrimTy::Bool));
                let n = self.next_check;
                self.next_check += 1;
                let (edge, other, other_args) = if to_then {
                    ((then_bb, then_args), else_bb, else_args)
                } else {
                    ((else_bb, else_args), then_bb, then_args)
                };
                let out = self.format_block_target(other.0, other_args);
                let parts = if to_then {
                    format!("^rok_pe{n}, {out}")
                } else {
                    format!("{out}, ^rok_pe{n}")
                };
                let _ = writeln!(self.body, "    cf.cond_br {v}, {parts}\n  ^rok_pe{n}:");
                let goto = Terminator::Goto {
                    target: *edge.0,
                    args: edge.1.clone(),
                };
                return self.emit_terminator(b, &goto);
            }
        }
        if polled {
            // The yield path takes its own copy of the terminator, into
            // the re-entry blocks.
            let n = self.emit_poll_split(Some(b));
            self.edge_from = (b, true);
            self.emit_terminator_inner(t);
            self.edge_from = (b, false);
            let _ = writeln!(self.body, "  ^rok_run{n}:");
        } else if self.back_edges.contains_key(&b) {
            self.emit_poll(Some(b));
        }
        self.emit_terminator_inner(t);
    }

    fn emit_terminator_inner(&mut self, t: &Terminator) {
        match t {
            // A generic body returning a `T = ()` value returns nothing.
            Terminator::Return(None) => {
                self.emit_gc_frame_pop();
                self.body.push_str("    func.return\n");
            }
            Terminator::Return(Some(_)) if matches!(self.f.ret_ty, MirTy::Unit) => {
                self.emit_gc_frame_pop();
                self.body.push_str("    func.return\n");
            }
            Terminator::Return(Some(op)) if rok_by_ref(&self.f.ret_ty, self.target) => {
                let ret_ty = self.f.ret_ty.clone();
                if !self.memcpy_operand("%arg0", op, &ret_ty) {
                    let val = self.materialize_operand(op, &ret_ty);
                    let ty = mlir_type(&ret_ty, self.target);
                    let _ = writeln!(self.body, "    llvm.store {val}, %arg0 : {ty}, !llvm.ptr");
                }
                self.emit_gc_frame_pop();
                self.body.push_str("    func.return\n");
            }
            Terminator::Return(Some(op)) => {
                let val = self.materialize_operand(op, &self.f.ret_ty.clone());
                self.emit_gc_frame_pop();
                let ty = mlir_type(&self.f.ret_ty, self.target);
                let _ = writeln!(self.body, "    func.return {val} : {ty}");
            }
            Terminator::Goto { target, args } => {
                self.emit_branch("    cf.br", target.0, args);
            }
            Terminator::If {
                cond,
                then_bb,
                then_args,
                else_bb,
                else_args,
            } => {
                let v = self.materialize_operand(cond, &MirTy::Prim(PrimTy::Bool));
                let then_part = self.format_block_target(then_bb.0, then_args);
                let else_part = self.format_block_target(else_bb.0, else_args);
                let _ = writeln!(self.body, "    cf.cond_br {v}, {then_part}, {else_part}");
            }
            Terminator::Unreachable => {
                self.body.push_str("    llvm.unreachable\n");
            }
            Terminator::Suspend { .. } => {
                self.errors
                    .push("internal: a coroutine suspend point survived MIR".to_owned());
                self.body.push_str("    llvm.unreachable\n");
            }
        }
    }

    fn emit_branch(&mut self, op: &str, target: u32, args: &[Operand]) {
        let t = self.format_block_target(target, args);
        let _ = writeln!(self.body, "{op} {t}");
    }

    fn format_block_target(&mut self, target: u32, args: &[Operand]) -> String {
        let (from, yielded) = self.edge_from;
        let fast_back = !yielded
            && self
                .back_edges
                .get(&from)
                .is_some_and(|hs| hs.contains(&target));
        let label = if self.reentry.contains(&target) && !fast_back {
            format!("^rok_re{target}")
        } else {
            format!("^bb{target}")
        };
        if args.is_empty() {
            return label;
        }
        let parts: Vec<(String, String)> = args
            .iter()
            .map(|a| {
                let ty = match a {
                    Operand::Copy(id) => self.local_ty(*id),
                    Operand::Const(c) => const_ty(c),
                };
                let v = self.materialize_operand(a, &ty);
                (v, mlir_type(&ty, self.target))
            })
            .collect();
        let vals: Vec<String> = parts.iter().map(|(v, _)| v.clone()).collect();
        let tys: Vec<String> = parts.iter().map(|(_, t)| t.clone()).collect();
        format!("{label}({} : {})", vals.join(", "), tys.join(", "))
    }

    fn materialize_operand(&mut self, op: &Operand, ty: &MirTy) -> String {
        match op {
            Operand::Copy(id) => {
                let v = self.read_local(*id);
                // A primitive local used at another primitive type gets an
                // explicit conversion.
                let from = self.local_ty(*id);
                if mlir_type(&from, self.target) == mlir_type(ty, self.target) {
                    return v;
                }
                match (&from, ty) {
                    (MirTy::Prim(_), MirTy::Prim(_)) => self.cast_value(v, &from, ty),
                    // A reference word read as an address or `any` payload,
                    // and back.
                    (MirTy::GcRef, MirTy::Prim(_) | MirTy::Any)
                    | (MirTy::Prim(_) | MirTy::Any, MirTy::GcRef) => {
                        self.bitcast_value(v, &from, ty)
                    }
                    _ => v,
                }
            }
            Operand::Const(c) => {
                let v = self.fresh_value();
                self.emit_const_assign(&v, ty, c);
                v
            }
        }
    }

    fn local_ty(&self, id: rokc_ir_common::LocalId) -> MirTy {
        self.f
            .locals
            .get(id.0 as usize)
            .map_or(MirTy::Any, |l| l.ty.clone())
    }
}

/// An op that emits no code and touches no visible memory, so a block
/// holding it is not folded into its neighbours.
const KEEP: &str = "    llvm.call_intrinsic \"llvm.sideeffect\"() : () -> ()\n";

/// A `()` value (argument, field, element): an empty struct. A function
/// returning `()` returns nothing instead (`mlir_fn_signature`).
const UNIT_TY: &str = "!llvm.struct<()>";

fn mlir_type(ty: &MirTy, target: &Target) -> String {
    let pointer_width = target.pointer_width;
    match ty {
        MirTy::Unit => UNIT_TY.to_owned(),
        MirTy::Prim(p) => mlir_prim(*p, pointer_width),
        MirTy::Any => format!("i{pointer_width}"),
        MirTy::Tuple(parts) | MirTy::Struct(_, parts) => {
            let inner: Vec<String> = parts.iter().map(|t| mlir_type(t, target)).collect();
            if !target.explicit_layout(ty) {
                return format!("!llvm.struct<({})>", inner.join(", "));
            }
            // A packed LLVM struct with explicit `[k x i8]` padding, so
            // LLVM's offsets, size and array stride are ours.
            let (offsets, size) = (target.field_offsets(ty), target.size_align(ty).0);
            let mut items = Vec::new();
            let mut at = 0;
            for ((t, p), off) in inner.into_iter().zip(parts).zip(offsets) {
                if off > at {
                    items.push(format!("!llvm.array<{} x i8>", off - at));
                }
                items.push(t);
                at = off + target.size_align(p).0;
            }
            if size > at {
                items.push(format!("!llvm.array<{} x i8>", size - at));
            }
            format!("!llvm.struct<packed ({})>", items.join(", "))
        }
        MirTy::Array(elem, n) => {
            format!("!llvm.array<{n} x {}>", mlir_type(elem, target))
        }
        MirTy::Union(_, n, _) => format!("!llvm.array<{n} x i{pointer_width}>"),
        // Function values are opaque pointers inside aggregates; each
        // call site supplies the `(args) -> ret` shape.
        MirTy::GcRef | MirTy::Fn(_, _) => "!llvm.ptr".to_owned(),
    }
}

/// `{llvm.align = N}` of a pointer argument (`{}`: unknown).
fn align_attr(align: u32) -> String {
    if align > 1 {
        format!("{{llvm.align = {align} : i64}}")
    } else {
        "{}".to_owned()
    }
}

/// The alignment attribute of a stack slot of `ty`: an explicit layout
/// is a packed LLVM struct, whose own alignment is 1.
fn slot_align(ty: &MirTy, target: &Target) -> String {
    if target.explicit_layout(ty) {
        format!(" {{alignment = {} : i64}}", target.size_align(ty).1.max(8))
    } else {
        String::new()
    }
}

/// LLVM field index of field `i` of struct / tuple `ty`: an explicit
/// layout (`mlir_type`) puts padding fields in between.
fn llvm_field(ty: &MirTy, i: usize, target: &Target) -> usize {
    let (MirTy::Tuple(ps) | MirTy::Struct(_, ps)) = ty else {
        return i;
    };
    if !target.explicit_layout(ty) {
        return i;
    }
    let mut at = 0;
    let mut k = 0;
    for (j, (t, off)) in ps.iter().zip(target.field_offsets(ty)).enumerate() {
        if off > at {
            k += 1;
        }
        if j == i {
            break;
        }
        k += 1;
        at = off + target.size_align(t).0;
    }
    k
}

/// Scalars of aggregate `ty` as (field path, byte offset, type), nested
/// ones flattened; `None` for a scalar, an explicit layout (fields may be
/// misaligned) or a non-scalar part.
fn heap_leaves(ty: &MirTy, target: &Target) -> Option<Vec<(Vec<usize>, u32, MirTy)>> {
    fn go(
        ty: &MirTy,
        target: &Target,
        path: &mut Vec<usize>,
        base: u32,
        out: &mut Vec<(Vec<usize>, u32, MirTy)>,
    ) -> bool {
        let (MirTy::Tuple(ps) | MirTy::Struct(_, ps)) = ty else {
            return false;
        };
        for (i, (p, off)) in ps.iter().zip(target.field_offsets(ty)).enumerate() {
            path.push(i);
            let ok = match p {
                MirTy::Prim(_) | MirTy::GcRef | MirTy::Any | MirTy::Fn(..) => {
                    out.push((path.clone(), base + off, p.clone()));
                    true
                }
                MirTy::Tuple(_) | MirTy::Struct(..) => go(p, target, path, base + off, out),
                _ => false,
            };
            path.pop();
            if !ok {
                return false;
            }
        }
        true
    }
    if target.explicit_layout(ty) {
        return None;
    }
    let mut out = Vec::new();
    go(ty, target, &mut Vec::new(), 0, &mut out).then_some(out)
}

fn join_path(path: &[usize]) -> String {
    path.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Field paths of `List` buffers in `ty` (`buf_ref_facts`), through nested
/// structs but not tuples (a carrier holds an unset payload).
fn buf_ref_paths(
    ty: &MirTy,
    target: &Target,
    lists: &HashSet<u32>,
    path: &mut Vec<usize>,
    out: &mut Vec<Vec<usize>>,
) {
    let MirTy::Struct(id, parts) = ty else {
        return;
    };
    let list = lists.contains(&id.0);
    for (i, p) in parts.iter().enumerate() {
        path.push(llvm_field(ty, i, target));
        match p {
            MirTy::GcRef if list => out.push(path.clone()),
            MirTy::Struct(..) => buf_ref_paths(p, target, lists, path, out),
            _ => {}
        }
        path.pop();
    }
}

/// A C signature's lowered parameters `(MLIR type, attributes)` (a
/// leading `sret` pointer included) and result type.
fn c_signature(
    params: &[MirTy],
    ret: &MirTy,
    abi: &FnAbi,
    target: &Target,
) -> (Vec<(String, String)>, Option<String>) {
    let mut out = Vec::new();
    if abi.ret == RetAbi::Sret {
        out.push((
            "!llvm.ptr".to_owned(),
            format!(
                " {{llvm.sret = {}, llvm.align = {} : i64}}",
                mlir_type(ret, target),
                target.size_align(ret).1
            ),
        ));
    }
    for (t, a) in params.iter().zip(&abi.args) {
        out.push(match a {
            ArgAbi::Direct => (c_type(t, target), c_ext_attr(t, target).to_owned()),
            ArgAbi::Coerce(c) => (c.clone(), String::new()),
            ArgAbi::Indirect { byval: true } => (
                "!llvm.ptr".to_owned(),
                format!(
                    " {{llvm.byval = {}, llvm.align = {} : i64}}",
                    mlir_type(t, target),
                    crate::abi::byval_align(t, target)
                ),
            ),
            ArgAbi::Indirect { byval: false } => ("!llvm.ptr".to_owned(), String::new()),
        });
    }
    let ret = match &abi.ret {
        RetAbi::Direct if *ret != MirTy::Unit => Some(c_type(ret, target)),
        RetAbi::Coerce(c) => Some(c.clone()),
        _ => None,
    };
    (out, ret)
}

/// Extension of a narrow C integer parameter (caller) or result (callee),
/// as clang does: System V `x86_64`, Apple arm64 and others extend `i8` /
/// `i16` / `bool` (`c_char` signed); Windows x64 only `bool`; AAPCS64
/// elsewhere nothing; RISC-V LP64D also sign-extends 32-bit ints, `u32`
/// too.
fn c_ext_attr(ty: &MirTy, target: &Target) -> &'static str {
    let narrow = match target.c_abi {
        CAbi::Aarch64 => matches!(target.os.as_str(), "macos" | "ios"),
        CAbi::X86_64Win => false,
        CAbi::X86_64SysV
        | CAbi::X86SysV
        | CAbi::X86Win
        | CAbi::ArmHf
        | CAbi::Wasm32
        | CAbi::Riscv64
        | CAbi::Other => true,
    };
    match ty {
        // C's `char` is unsigned on 32-bit ARM and RISC-V Linux (rok's
        // `c_char` is signed: the same bits).
        MirTy::Prim(PrimTy::CChar) if matches!(target.c_abi, CAbi::ArmHf | CAbi::Riscv64) => {
            " {llvm.zeroext}"
        }
        MirTy::Prim(PrimTy::I32 | PrimTy::U32 | PrimTy::Char) if target.c_abi == CAbi::Riscv64 => {
            " {llvm.signext}"
        }
        MirTy::Prim(PrimTy::I8 | PrimTy::I16 | PrimTy::CChar) if narrow => " {llvm.signext}",
        MirTy::Prim(PrimTy::U8 | PrimTy::U16) if narrow => " {llvm.zeroext}",
        MirTy::Prim(PrimTy::Bool) if narrow || target.c_abi == CAbi::X86_64Win => " {llvm.zeroext}",
        _ => "",
    }
}

/// ` -> T` of a C declaration / definition, with the result's extension.
fn c_ret_arrow(ret: Option<&str>, ret_ty: &MirTy, abi: Option<&FnAbi>, target: &Target) -> String {
    let Some(r) = ret else {
        return String::new();
    };
    let direct = abi.is_none_or(|a| a.ret == RetAbi::Direct);
    match c_ext_attr(ret_ty, target) {
        a if direct && !a.is_empty() => format!(" -> ({r}{a})"),
        _ => format!(" -> {r}"),
    }
}

/// `mlir_type` as C sees it: `str` is a `char*`.
fn c_type(ty: &MirTy, target: &Target) -> String {
    if *ty == MirTy::Prim(PrimTy::Str) {
        return "!llvm.ptr".to_owned();
    }
    mlir_type(ty, target)
}

/// MLIR parameter and result types of a rok function: a by-address
/// argument is a pointer the callee copies from; a by-address result goes
/// through a leading pointer and the function returns nothing.
fn rok_fn_types(params: &[MirTy], ret: &MirTy, target: &Target) -> (Vec<String>, Option<String>) {
    let sret = rok_by_ref(ret, target);
    let mut out = Vec::new();
    if sret {
        out.push("!llvm.ptr".to_owned());
    }
    out.extend(params.iter().map(|t| {
        if rok_by_ref(t, target) {
            "!llvm.ptr".to_owned()
        } else {
            mlir_type(t, target)
        }
    }));
    let ret = (!sret && *ret != MirTy::Unit).then(|| mlir_type(ret, target));
    (out, ret)
}

/// `(args) -> ret` of a rok function value (`func.constant`).
fn mlir_fn_signature(args: &[MirTy], ret: &MirTy, target: &Target) -> String {
    let (params, ret) = rok_fn_types(args, ret, target);
    format!(
        "({}) -> {}",
        params.join(", "),
        ret.unwrap_or_else(|| "()".to_owned())
    )
}

fn mlir_prim(p: PrimTy, pointer_width: u32) -> String {
    match p {
        PrimTy::I8 | PrimTy::U8 | PrimTy::CChar => "i8".to_owned(),
        PrimTy::I16 | PrimTy::U16 => "i16".to_owned(),
        PrimTy::I32 | PrimTy::U32 | PrimTy::Char => "i32".to_owned(),
        PrimTy::I64 | PrimTy::U64 => "i64".to_owned(),
        PrimTy::Isize | PrimTy::Usize => format!("i{pointer_width}"),
        PrimTy::F32 => "f32".to_owned(),
        PrimTy::F64 => "f64".to_owned(),
        PrimTy::Bool => "i1".to_owned(),
        // `(ptr, len)` (808); a C function gets `rok_str_cstr`'s
        // NUL-terminated pointer instead.
        PrimTy::Str => format!("!llvm.struct<(ptr, i{pointer_width})>"),
    }
}

/// Build a single MLIR line for a primitive binary op. Returns `None` if
/// the (op, prim) combo is not handled.
#[allow(clippy::cognitive_complexity)] // flat (op, prim) dispatch table; splitting it hurts readability
fn arith_binop(
    op: PrimOp,
    prim: PrimTy,
    dst: &str,
    lhs: &str,
    rhs: &str,
    ty: &str,
) -> Option<String> {
    let int = prim.is_int();
    let signed = prim.is_signed_int();
    let float = prim.is_float();
    let bool_ = matches!(prim, PrimTy::Bool);

    let mnemonic: Option<String> = match op {
        // `Wrapping*` skip `emit_overflow_check` (it ignores them).
        PrimOp::Add | PrimOp::WrappingAdd if int => Some("arith.addi".into()),
        PrimOp::Add if float => Some("arith.addf".into()),
        PrimOp::Sub | PrimOp::WrappingSub if int => Some("arith.subi".into()),
        PrimOp::Sub if float => Some("arith.subf".into()),
        PrimOp::Mul | PrimOp::WrappingMul if int => Some("arith.muli".into()),
        PrimOp::Mul if float => Some("arith.mulf".into()),
        PrimOp::Div if signed => Some("arith.divsi".into()),
        PrimOp::Div if int => Some("arith.divui".into()),
        PrimOp::Div if float => Some("arith.divf".into()),
        PrimOp::Rem if signed => Some("arith.remsi".into()),
        PrimOp::Rem if int => Some("arith.remui".into()),
        PrimOp::Rem if float => Some("arith.remf".into()),
        PrimOp::BitAnd if int || bool_ => Some("arith.andi".into()),
        PrimOp::BitOr if int || bool_ => Some("arith.ori".into()),
        PrimOp::BitXor if int || bool_ => Some("arith.xori".into()),
        PrimOp::LogicalAnd if bool_ => Some("arith.andi".into()),
        PrimOp::LogicalOr if bool_ => Some("arith.ori".into()),
        PrimOp::Shl if int => Some("arith.shli".into()),
        PrimOp::Shr if signed => Some("arith.shrsi".into()),
        PrimOp::Shr if int => Some("arith.shrui".into()),
        _ => None,
    };
    if let Some(m) = mnemonic {
        return Some(format!("    {dst} = {m} {lhs}, {rhs} : {ty}\n"));
    }

    // Comparisons return i1; the predicate selector encodes signed-ness.
    let pred = match (op, prim) {
        (PrimOp::Eq, p) if p.is_int() || matches!(p, PrimTy::Bool) => Some(("arith.cmpi", "eq")),
        (PrimOp::Ne, p) if p.is_int() || matches!(p, PrimTy::Bool) => Some(("arith.cmpi", "ne")),
        (PrimOp::Lt, p) if p.is_signed_int() => Some(("arith.cmpi", "slt")),
        (PrimOp::Le, p) if p.is_signed_int() => Some(("arith.cmpi", "sle")),
        (PrimOp::Gt, p) if p.is_signed_int() => Some(("arith.cmpi", "sgt")),
        (PrimOp::Ge, p) if p.is_signed_int() => Some(("arith.cmpi", "sge")),
        (PrimOp::Lt, p) if p.is_int() => Some(("arith.cmpi", "ult")),
        (PrimOp::Le, p) if p.is_int() => Some(("arith.cmpi", "ule")),
        (PrimOp::Gt, p) if p.is_int() => Some(("arith.cmpi", "ugt")),
        (PrimOp::Ge, p) if p.is_int() => Some(("arith.cmpi", "uge")),
        (PrimOp::Eq, p) if p.is_float() => Some(("arith.cmpf", "oeq")),
        // `!=` is true when either side is NaN → unordered-or-not-equal.
        (PrimOp::Ne, p) if p.is_float() => Some(("arith.cmpf", "une")),
        (PrimOp::Lt, p) if p.is_float() => Some(("arith.cmpf", "olt")),
        (PrimOp::Le, p) if p.is_float() => Some(("arith.cmpf", "ole")),
        (PrimOp::Gt, p) if p.is_float() => Some(("arith.cmpf", "ogt")),
        (PrimOp::Ge, p) if p.is_float() => Some(("arith.cmpf", "oge")),
        _ => None,
    };
    if let Some((mn, pred)) = pred {
        return Some(format!("    {dst} = {mn} {pred}, {lhs}, {rhs} : {ty}\n"));
    }
    None
}

/// A local of type `ty` lives in an `llvm.alloca` slot; `()` and `any`
/// values stay SSA.
const fn is_slotted(ty: &MirTy) -> bool {
    !matches!(ty, MirTy::Unit | MirTy::Any)
}

/// `(total_size, align)` of a GC object with a `ty` payload.
pub fn gc_object_size(ty: &MirTy, target: &Target) -> (u32, u32) {
    let (size, align) = target.size_align(ty);
    let align = align.max(8);
    (HEADER_SIZE + size.next_multiple_of(align), align)
}

/// The `ScanTable` (802) of `ty` objects (`array`: buffers of `ty`).
///
/// `u32` words: header `{total_size u32, version u16 = 1, num_entries
/// u16}`, `{field_offset u32, field_kind u32}` entries, then sum
/// descriptors. An array table's size is the stride, offsets
/// element-relative.
pub fn scan_table(ty: &MirTy, array: bool, target: &Target) -> Vec<u32> {
    let mut slots = Vec::new();
    let (total, version) = if array {
        target.gc_offsets(ty, 0, &mut slots);
        (target.size_align(ty).0, 1 | TABLE_ARRAY)
    } else {
        target.gc_offsets(ty, HEADER_SIZE, &mut slots);
        (gc_object_size(ty, target).0, 1)
    };
    let mut words = vec![total, version | ((slots.len() as u32) << 16)];
    words.resize(2 + 2 * slots.len(), 0);
    encode_slots(&slots, 0, 2, &mut words);
    words
}

/// Write `slots` at word `at` (reserved), offsets relative to `base`. A
/// sum's entry is its tag offset and `SUM_PAYLOAD | desc << SUM_DESC_SHIFT`;
/// the appended descriptor is `n`, then `{tag, num_entries, entries}` per
/// data variant, offsets from the tag.
fn encode_slots(slots: &[GcSlot], base: u32, at: usize, words: &mut Vec<u32>) {
    for (i, slot) in slots.iter().enumerate() {
        let (off, kind) = match slot {
            GcSlot::Word(off, kind) => (*off, *kind),
            GcSlot::Sum(off, variants) => {
                let desc = words.len();
                words.push(variants.len() as u32);
                words.resize(desc + 1 + 3 * variants.len(), 0);
                for (k, (tag, v)) in variants.iter().enumerate() {
                    let entries = words.len();
                    words.resize(entries + 2 * v.len(), 0);
                    let head = desc + 1 + 3 * k;
                    words[head..head + 3].copy_from_slice(&[*tag, v.len() as u32, entries as u32]);
                    encode_slots(v, *off, entries, words);
                }
                (*off, SUM_PAYLOAD | ((desc as u32) << SUM_DESC_SHIFT))
            }
        };
        words[at + 2 * i] = off - base;
        words[at + 2 * i + 1] = kind;
    }
}

fn struct_field_tys(ty: &MirTy) -> Vec<MirTy> {
    match ty {
        MirTy::Struct(_, fs) | MirTy::Tuple(fs) => fs.clone(),
        _ => Vec::new(),
    }
}

/// `+ - *` whose constant operands rule out overflow: both constant and
/// fitting, or an identity / absorbing constant (`x + 0`, `x * 1`, `x * 0`).
pub fn cannot_overflow(
    op: PrimOp,
    prim: PrimTy,
    bits: u32,
    (l, r): (Option<i128>, Option<i128>),
) -> bool {
    if let (Some(a), Some(b)) = (l, r) {
        let v = match op {
            PrimOp::Add => a.checked_add(b),
            PrimOp::Sub => a.checked_sub(b),
            PrimOp::Mul => a.checked_mul(b),
            _ => None,
        };
        let (lo, hi) = if prim.is_signed_int() {
            (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
        } else {
            (
                0,
                if bits >= 127 {
                    i128::MAX
                } else {
                    (1i128 << bits) - 1
                },
            )
        };
        return v.is_some_and(|v| (lo..=hi).contains(&v));
    }
    match op {
        PrimOp::Add => l == Some(0) || r == Some(0),
        PrimOp::Sub => r == Some(0),
        PrimOp::Mul => matches!(l, Some(0 | 1)) || matches!(r, Some(0 | 1)),
        _ => false,
    }
}

/// Integer value of a constant operand (`Int` / in-range `UInt`).
pub fn const_int(op: &Operand) -> Option<i128> {
    match op {
        Operand::Const(ConstVal::Int { value, .. }) => Some(*value),
        Operand::Const(ConstVal::UInt { value, .. }) => i128::try_from(*value).ok(),
        _ => None,
    }
}

/// Best-effort MLIR type for a raw const operand.
const fn const_ty(c: &ConstVal) -> MirTy {
    match c {
        ConstVal::Int { ty, .. } | ConstVal::UInt { ty, .. } | ConstVal::Float { ty, .. } => {
            MirTy::Prim(*ty)
        }
        ConstVal::Bool(_) => MirTy::Prim(PrimTy::Bool),
        ConstVal::Str(_) => MirTy::Prim(PrimTy::Str),
        ConstVal::Char(_) => MirTy::Prim(PrimTy::Char),
        ConstVal::Unit | ConstVal::None | ConstVal::Bytes(_) => MirTy::Unit,
    }
}

fn format_float(bits: u64, prim: PrimTy) -> String {
    // A non-finite value has no decimal form: MLIR takes the bits of the
    // type's own width as a hex literal.
    if prim == PrimTy::F32 {
        let v = f32::from_bits(bits as u32);
        if v.is_finite() {
            format_float_decimal(f64::from(v))
        } else {
            format!("0x{:08X}", v.to_bits())
        }
    } else {
        let v = f64::from_bits(bits);
        if v.is_finite() {
            format_float_decimal(v)
        } else {
            format!("0x{bits:016X}")
        }
    }
}

/// A finite float as MLIR's literal grammar wants it: with a decimal
/// point (`3e0` is rejected; `{:?}` writes `1e300` for big values).
fn format_float_decimal(v: f64) -> String {
    let s = format!("{v:?}");
    match s.split_once('e') {
        Some((m, e)) if !m.contains('.') => format!("{m}.0e{e}"),
        _ => s,
    }
}

/// `bytes` as the body of an MLIR string attribute.
fn mlir_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::with_capacity(bytes.len());
    for &b in bytes {
        if b == b'\\' || b == b'"' {
            escaped.push('\\');
            escaped.push(b as char);
        } else if (0x20..0x7f).contains(&b) {
            escaped.push(b as char);
        } else {
            let _ = write!(escaped, "\\{b:02X}");
        }
    }
    escaped
}

/// Bytes of an array of scalar `elem_ty` constants as stored on `target`
/// (little-endian); `None` unless all are such constants.
#[must_use]
pub fn const_bytes(elements: &[Operand], elem_ty: &MirTy, target: &Target) -> Option<Vec<u8>> {
    let MirTy::Prim(p) = elem_ty else { return None };
    let size = target.size_align(elem_ty).0 as usize;
    let mut out = Vec::with_capacity(elements.len() * size);
    for e in elements {
        let Operand::Const(c) = e else { return None };
        let v: u128 = match c {
            ConstVal::Int { value, .. } if p.is_int() => *value as u128,
            ConstVal::UInt { value, .. } if p.is_int() => *value,
            ConstVal::Char(c) if p.is_int() => u128::from(*c),
            ConstVal::Bool(b) if *p == PrimTy::Bool => u128::from(*b),
            ConstVal::Float { bits, ty } if *ty == *p && p.is_float() => u128::from(*bits),
            _ => return None,
        };
        out.extend_from_slice(&v.to_le_bytes()[..size]);
    }
    Some(out)
}

fn format_const_attr(v: &ConstVal, ty: &MirTy, target: &Target) -> Option<String> {
    let ty_str = mlir_type(ty, target);
    if let (ConstVal::Bytes(b), MirTy::Array(..)) = (v, ty) {
        return Some(format!("\"{}\"", mlir_bytes(b)));
    }
    let MirTy::Prim(p) = ty else { return None };
    Some(match v {
        ConstVal::Int { value, .. } if p.is_int() => format!("{value} : {ty_str}"),
        ConstVal::UInt { value, .. } if p.is_int() => format!("{value} : {ty_str}"),
        ConstVal::Char(c) if p.is_int() => format!("{c} : {ty_str}"),
        ConstVal::Bool(b) if *p == PrimTy::Bool => format!("{} : i1", u8::from(*b)),
        ConstVal::Float { bits, ty: fp } if p.is_float() => {
            let v = if *fp == PrimTy::F32 {
                f64::from(f32::from_bits(*bits as u32))
            } else {
                f64::from_bits(*bits)
            };
            let b = if *p == PrimTy::F32 {
                u64::from((v as f32).to_bits())
            } else {
                v.to_bits()
            };
            format!("{} : {ty_str}", format_float(b, *p))
        }
        ConstVal::Int { value, .. } if p.is_float() => {
            format!("{} : {ty_str}", format_float_decimal(*value as f64))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod overflow_tests {
    use super::*;

    #[test]
    fn constant_operands_skip_the_check() {
        let i8_ = |op, l, r| cannot_overflow(op, PrimTy::I8, 8, (l, r));
        assert!(i8_(PrimOp::Add, Some(100), Some(27)));
        assert!(!i8_(PrimOp::Add, Some(100), Some(28)));
        assert!(i8_(PrimOp::Mul, None, Some(1)));
        assert!(i8_(PrimOp::Sub, None, Some(0)));
        assert!(!i8_(PrimOp::Sub, Some(0), None));
        assert!(!i8_(PrimOp::Add, None, Some(1)));
        assert!(!cannot_overflow(
            PrimOp::Sub,
            PrimTy::U8,
            8,
            (Some(1), Some(2))
        ));
        assert!(cannot_overflow(
            PrimOp::Mul,
            PrimTy::U64,
            64,
            (Some(1 << 40), Some(2))
        ));
    }

    #[test]
    fn non_finite_floats_take_their_own_width() {
        let f32_ = |v: f32| format_float(u64::from(v.to_bits()), PrimTy::F32);
        assert_eq!(f32_(f32::NEG_INFINITY), "0xFF800000");
        assert_eq!(f32_(f32::INFINITY), "0x7F800000");
        assert_eq!(f32_(f32::NAN), "0x7FC00000");
        assert_eq!(f32_(1.5), "1.5");
        let f64_ = |v: f64| format_float(v.to_bits(), PrimTy::F64);
        assert_eq!(f64_(f64::NEG_INFINITY), "0xFFF0000000000000");
        assert_eq!(f64_(1e300), "1.0e300");
    }
}
