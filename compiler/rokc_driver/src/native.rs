//! Backend choice. rok's own (`rokc_codegen_native`) builds `-O0` for
//! `aarch64` and `x86_64`; LLVM builds everything else.

use rokc_diagnostic::{RokDiagnostic, SourceMap};

use crate::{BuildOptions, CompiledModule};

/// The native backend's object for `module`, or `None` when LLVM builds it
/// (also any Windows `-g` build: we write DWARF, Windows debuggers read
/// `CodeView`).
pub fn object(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
) -> Result<Option<rokc_codegen_native::Output>, RokDiagnostic> {
    if !chosen_for(module, opts) {
        return Ok(None);
    }
    let o = rokc_codegen_native::compile(&module.mir, &options(opts), sources)?;
    Ok(Some(o))
}

/// The native backend builds `module`'s code (else LLVM).
pub(crate) fn chosen_for(module: &CompiledModule, opts: &BuildOptions) -> bool {
    wanted(opts)
        && rokc_codegen_native::unsupported(&module.mir).is_none()
        && !(opts.debug && module.mir.target.object_format == rokc_target::ObjectFormat::Coff)
}

/// The native backend's options of a program build.
pub(crate) fn options(opts: &BuildOptions) -> rokc_codegen_native::Options {
    rokc_codegen_native::Options {
        overflow_checks: opts.overflow_checks,
        debug: opts.debug,
        repl: opts.repl.clone(),
        fresh_fns: None,
    }
}

/// The native backend generates `opts`' code (when the target and the
/// module allow; else LLVM).
#[must_use]
pub fn chosen(opts: &BuildOptions) -> bool {
    wanted(opts)
}

/// The native backend builds the REPL's cells (`cell_object`); else
/// LLVM does (`toolchain::cell_object`).
#[must_use]
pub fn jit(opts: &BuildOptions) -> bool {
    crate::repl_host() && wanted(opts)
}

/// A REPL cell as a native object, compiling only functions the session
/// lacks (decided on the MLIR text, as for LLVM).
pub fn cell_object(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
) -> Result<(Vec<u8>, rokc_codegen_mlir::MlirOutput), RokDiagnostic> {
    let mut out = crate::mlir_text(module, sources, opts)?;
    let fresh = out.fns.iter().map(|f| f.name.clone()).collect();
    let o = rokc_codegen_native::compile(
        &module.mir,
        &rokc_codegen_native::Options {
            overflow_checks: opts.overflow_checks,
            debug: opts.debug,
            repl: opts.repl.clone(),
            fresh_fns: Some(fresh),
        },
        sources,
    )?;
    out.text.clear();
    out.gc_types = o.gc_types;
    out.gc_tables = o.gc_tables;
    Ok((o.object, out))
}

/// `-O0` (unless `-C backend=llvm` / `ROK_BACKEND=llvm`) or
/// `-C backend=native`, and nothing LLVM-only (PGO).
fn wanted(opts: &BuildOptions) -> bool {
    let native = match opts.backend.as_deref() {
        Some("native") => true,
        Some(_) => false,
        None => opts.opt_level == "0",
    };
    native && opts.profile == crate::Profile::Off
}
