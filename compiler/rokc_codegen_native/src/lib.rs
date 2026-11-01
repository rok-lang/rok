//! rok's own backend: MIR straight to machine code and an object file, no
//! LLVM, for fast `-O0` builds and REPL cells. Both backends follow one
//! runtime ABI, so their objects link together.
//!
//! MIR → LIR (`lower`: layouts, conventions, GC frames, barriers, checks)
//! → machine code (one module per ISA) → object (one module per format).

// Instruction selection names registers and encodings tersely; its
// dispatch matches are long by nature.
#![allow(clippy::many_single_char_names, clippy::cognitive_complexity)]

mod a64;
mod cc;
mod coff;
mod dwarf;
mod elf;
mod lir;
mod lower;
mod macho;
mod module;
mod obj;
mod wrap;
mod x64;

use rokc_diagnostic::{ErrorKind, RokDiagnostic};
use rokc_mir::MirModule;

/// How to generate code for a module (its target is `MirModule::target`).
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Panic on integer overflow.
    pub overflow_checks: bool,
    /// `-g`: DWARF debug info (spec 9.9.6).
    pub debug: bool,
    /// Generate a REPL cell instead of a program.
    pub repl: Option<rokc_codegen_mlir::ReplCell>,
    /// A REPL cell: the exported functions it defines anew (the rest the
    /// session has: declared only, `rokc_codegen_mlir::ReplFn`).
    pub fresh_fns: Option<std::collections::HashSet<String>>,
}

/// A compiled module.
pub struct Output {
    /// The relocatable object file.
    pub object: Vec<u8>,
    /// Number of GC type ids used.
    pub gc_types: u32,
    /// Every `ScanTable` so far (a REPL cell: the next one's
    /// `ReplCell::gc_tables`).
    pub gc_tables: Vec<Vec<u32>>,
}

/// Why this backend can't compile `module` (LLVM then does): no
/// instruction selector or object writer for the target.
#[must_use]
pub fn unsupported(module: &MirModule) -> Option<String> {
    let t = &module.target;
    (!t.native_backend).then(|| format!("no native backend for {}", t.name))
}

/// Compile `module` to an object file.
pub fn compile(
    module: &MirModule,
    opts: &Options,
    sources: &rokc_diagnostic::SourceMap,
) -> Result<Output, RokDiagnostic> {
    if let Some(why) = unsupported(module) {
        return Err(RokDiagnostic::error(
            ErrorKind::Unsupported,
            rokc_diagnostic::Span::DUMMY,
            why,
        ));
    }
    finish(module, opts, sources, None)
}

/// Compile unit `u` of `plan` (`rokc_codegen_mlir::units`) to an object
/// file (the module must be one `unsupported` accepts).
pub fn compile_unit(
    module: &MirModule,
    opts: &Options,
    sources: &rokc_diagnostic::SourceMap,
    plan: &rokc_codegen_mlir::units::Plan,
    u: usize,
) -> Result<Output, RokDiagnostic> {
    let unit = module::UnitArgs {
        core: &plan.core,
        u,
        glue: plan.units[u].glue,
        tables: &plan.gc_tables,
    };
    let o = finish(module, opts, sources, Some(&unit))?;
    if o.gc_tables.len() != plan.gc_tables.len() {
        return Err(RokDiagnostic::error(
            ErrorKind::Internal,
            rokc_diagnostic::Span::DUMMY,
            format!(
                "unit `{}` made a GC type its plan lacks",
                plan.units[u].name
            ),
        ));
    }
    Ok(o)
}

fn finish(
    module: &MirModule,
    opts: &Options,
    sources: &rokc_diagnostic::SourceMap,
    unit: Option<&module::UnitArgs<'_>>,
) -> Result<Output, RokDiagnostic> {
    let emit = if module.target.arch == "x86_64" {
        x64::emit
    } else {
        a64::emit
    };
    let mut c = module::compile(module, opts, sources, unit, &mut { emit });
    // COFF: no DWARF (Windows debuggers read CodeView; `-g` builds for
    // Windows go through LLVM).
    if opts.debug && !c.obj.coff {
        dwarf::write(&mut c.obj, module, sources);
    }
    if let Some(d) = rokc_codegen_mlir::codegen_error(&c.errors) {
        return Err(d);
    }
    c.obj.drop_unreferenced();
    Ok(Output {
        object: if c.obj.coff {
            coff::write(&c.obj)
        } else if c.obj.elf {
            elf::write(&c.obj)
        } else {
            macho::write(&c.obj)
        },
        gc_types: c.gc_types.len() as u32,
        gc_tables: c.gc_types,
    })
}
