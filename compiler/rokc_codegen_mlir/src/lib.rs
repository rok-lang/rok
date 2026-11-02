//! MIR → MLIR text in upstream dialects (`func`, `arith`, `math`, `cf`, `llvm`).
//! Passes, LLVM IR and codegen follow in `rokc_llvm`.

pub mod abi;
mod alias;
mod compact;
mod debug;
pub mod emit;
pub mod mangle;
mod repl;
#[cfg(test)]
mod tests;
pub mod units;

pub use mangle::{Demangled, demangle, pkg_hash};
pub use repl::ReplFn;

use rokc_diagnostic::{Code, RokDiagnostic};
use rokc_mir::MirModule;

#[derive(Clone, Debug, Default)]
pub struct MlirOutput {
    pub text: String,
    /// Number of GC type ids used.
    pub gc_types: u32,
    /// A REPL cell: every `ScanTable` of the session so far (the next
    /// cell's `ReplCell::gc_tables`).
    pub gc_tables: Vec<Vec<u32>>,
    /// A REPL cell: the exported symbols it defines anew (`repl::reuse`).
    pub fresh: Vec<String>,
    /// A REPL cell: the functions it defines anew.
    pub fns: Vec<ReplFn>,
    /// A REPL cell: how many functions it takes from earlier cells.
    pub reused: usize,
}

/// One REPL cell: code the host links into a process whose runtime is
/// up. Instead of a C `main`, `rok_repl_cell()` registers its GC types and
/// roots and runs the user `main`.
#[derive(Clone, Debug, Default)]
pub struct ReplCell {
    /// The `ScanTable`s earlier cells registered (type id
    /// `USER_TYPE_BASE + i`): the same type keeps its id, new ones follow.
    pub gc_tables: Vec<Vec<u32>>,
    /// Functions the session has (symbol → text hash, `repl::reuse`):
    /// declared, not defined, when this cell's text is the same.
    pub known: std::collections::HashMap<String, u64>,
    /// `extern` statics this cell defines (zeroed, exported, GC roots):
    /// the slots of the session's new bindings.
    pub define: Vec<String>,
    /// Statics (MIR names) an earlier cell defined: declared, not
    /// defined, so their state carries over.
    pub statics: Vec<String>,
}

/// How to generate code for a module (its target is `MirModule::target`).
#[derive(Clone, Debug, Default)]
pub struct CodegenOptions {
    /// Panic on integer overflow (unless `-C overflow-checks=no`).
    pub overflow_checks: bool,
    /// `-g`: DWARF debug info (spec 9.9.6).
    pub debug: bool,
    /// Above `-O0`: the debug info says so.
    pub optimized: bool,
    /// Generate a REPL cell instead of a program.
    pub repl: Option<ReplCell>,
    /// LLVM `target datalayout` (the toolchain's), so MLIR folds `sizeof`
    /// idioms with the real layout.
    pub data_layout: Option<String>,
}

/// One diagnostic for codegen's errors: the first at its function, the
/// rest as notes.
#[must_use]
pub fn codegen_error(errors: &[(rokc_diagnostic::Span, String)]) -> Option<RokDiagnostic> {
    let ((at, first), rest) = errors.split_first()?;
    let mut d = RokDiagnostic::error(Code::UNSUPPORTED, *at, first.clone());
    for (_, more) in rest {
        d = d.with_note(more.clone());
    }
    Some(d)
}

/// The MLIR text of `module`.
pub fn codegen(
    module: &MirModule,
    opts: &CodegenOptions,
    sources: &rokc_diagnostic::SourceMap,
) -> Result<MlirOutput, RokDiagnostic> {
    let target = module.target.clone();
    let mut e = emit::Emitter::new(target.clone());
    e.overflow_checks = opts.overflow_checks;
    e.data_layout.clone_from(&opts.data_layout);
    if opts.debug {
        e.debug = Some(debug::DebugInfo::new(target, opts.optimized, sources));
    }
    if let Some(r) = &opts.repl {
        e.gc_types.clone_from(&r.gc_tables);
    }
    e.repl.clone_from(&opts.repl);
    e.emit_module(module, sources);
    if let Some(d) = codegen_error(&e.errors) {
        return Err(d);
    }
    let gc_types = e.gc_type_count();
    let gc_tables = if opts.repl.is_some() {
        std::mem::take(&mut e.gc_types)
    } else {
        Vec::new()
    };
    let mut out = MlirOutput {
        gc_types,
        gc_tables,
        text: e.finish(),
        ..MlirOutput::default()
    };
    if let Some(r) = &opts.repl {
        (out.text, out.reused, out.fresh, out.fns) = repl::reuse(&out.text, &r.known);
    }
    Ok(out)
}

/// The MLIR text of unit `u` of `plan` (a program's, `units::plan`).
pub fn codegen_unit(
    module: &MirModule,
    opts: &CodegenOptions,
    sources: &rokc_diagnostic::SourceMap,
    plan: &units::Plan,
    u: usize,
) -> Result<String, RokDiagnostic> {
    let target = module.target.clone();
    let mut e = emit::Emitter::new(target.clone());
    e.overflow_checks = opts.overflow_checks;
    e.data_layout.clone_from(&opts.data_layout);
    if opts.debug {
        e.debug = Some(debug::DebugInfo::new(target, opts.optimized, sources));
    }
    e.gc_types.clone_from(&plan.gc_tables);
    e.unit = Some((std::sync::Arc::clone(&plan.core), u));
    e.glue = plan.units[u].glue;
    e.emit_module(module, sources);
    if e.gc_types.len() != plan.gc_tables.len() {
        e.errors.push((
            rokc_diagnostic::Span::DUMMY,
            format!(
                "internal: unit `{}` made a GC type its plan lacks",
                plan.units[u].name
            ),
        ));
    }
    if let Some(d) = codegen_error(&e.errors) {
        return Err(d);
    }
    Ok(e.finish())
}

/// A module of `rok.alloc` alone (what `units` builds link into each
/// optimized part, as the LLVM shim does for a split module).
#[must_use]
pub fn alloc_module(target: &rokc_mir::Target) -> String {
    emit::alloc_module(target)
}
