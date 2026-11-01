//! Incremental builds by codegen units (spec 9.2.16): a package's objects
//! from its units (`rokc_codegen_mlir::units`), each recompiled only when
//! its key changed. Directory layout:
//!
//! - `stamp`: compiler and options the rest is for (a change empties it);
//! - `<key>.o`: a unit's object (native backend, LLVM `-O0`);
//! - `<key>.bc`: a unit's `ThinLTO` bitcode above `-O0`, with LLVM's
//!   object cache in `thin/`.
//!
//! Units use a placeholder for the package hash, patched before the link.
//! Unused entries are deleted.

use std::path::{Path, PathBuf};

use rokc_codegen_mlir::units::{self, Plan, PlanOptions};
use rokc_diagnostic::{ErrorKind, RokDiagnostic, SourceMap};

use crate::{BuildOptions, CompiledModule};

/// What units a build compiled, and how many it reused.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UnitCounts {
    /// The names of the units compiled (their home modules).
    pub compiled: Vec<String>,
    pub reused: usize,
    /// Above `-O0`: the units whose optimized object LLVM's cache had.
    pub cached: Option<usize>,
}

/// Whether `module` builds by units: a program or library (not a REPL
/// cell) without whole-module features (PGO).
pub fn applies(_module: &CompiledModule, opts: &BuildOptions) -> bool {
    opts.incremental.is_some() && opts.repl.is_none() && opts.profile == crate::Profile::Off
}

/// The objects of `module` (`applies`), from `dir` where its units are
/// unchanged, and how many GC types it has.
pub fn objects(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
    dir: &Path,
) -> Result<(Vec<Vec<u8>>, UnitCounts, u32), RokDiagnostic> {
    let mir = &module.mir;
    let native = crate::native::chosen_for(module, opts);
    let t = &*mir.target.spec;
    let data_layout = (!native).then(|| t.data_layout.clone());
    let salt = salt(opts, native, t, data_layout.as_deref());
    open(dir, &salt)?;
    let plan = rokc_diagnostic::time("units", || {
        units::plan(
            mir,
            &PlanOptions {
                salt: &salt,
                debug: opts.debug,
                sources,
            },
        )
    });
    let (objects, counts, keep) = if native {
        rokc_diagnostic::time("native", || by_native(module, sources, opts, &plan, dir))?
    } else if opts.opt_level == "0" {
        by_llvm_o0(module, sources, opts, &plan, dir, t, data_layout)?
    } else {
        by_thin_link(module, sources, opts, &plan, dir, t, data_layout)?
    };
    prune(dir, &keep);
    let real = mir
        .link
        .packages
        .iter()
        .find(|(p, _)| p == units::own_package(&mir.link))
        .map_or("", |(_, h)| h.as_str());
    let objects = rokc_diagnostic::time("rehash", || {
        objects
            .into_iter()
            .map(|o| rehash(o, plan.placeholder(), real))
            .collect()
    });
    Ok((objects, counts, plan.gc_tables.len() as u32))
}

/// What units' entries are for: compiler, target, code-changing options;
/// under `-g` also the base directory of source paths (debug info is
/// absolute, unit keys hold their own spans' paths).
fn salt(
    opts: &BuildOptions,
    native: bool,
    t: &rokc_llvm::Target,
    data_layout: Option<&str>,
) -> String {
    let mut s = format!(
        "{}\nnative {native}\nopt {}\noverflow {:?}\ndebug {}\ntarget {:016x}\n{data_layout:?}\n",
        crate::rokhir::producer(),
        opts.opt_level,
        opts.overflow_checks,
        opts.debug,
        t.hash(),
    );
    if opts.debug {
        let cwd = std::env::current_dir().unwrap_or_default();
        s.push_str(&format!("{}\n", cwd.display()));
    }
    s
}

/// `dir`, emptied when its stamp is not `salt`'s.
fn open(dir: &Path, salt: &str) -> Result<(), RokDiagnostic> {
    let stamp = dir.join("stamp");
    if std::fs::read_to_string(&stamp).ok().as_deref() == Some(salt) {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)
        .map_err(|e| err(format!("cannot create {}: {e}", dir.display())))?;
    write(&stamp, salt.as_bytes())
}

/// `path` holds `bytes`, or what it held: written beside it, then
/// renamed over it.
fn write(path: &Path, bytes: &[u8]) -> Result<(), RokDiagnostic> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let put = || std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    // Another build may have emptied the directory meanwhile.
    put()
        .or_else(|e| match path.parent() {
            Some(dir) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(dir).and_then(|()| put())
            }
            _ => Err(e),
        })
        .map_err(|e| err(format!("cannot write {}: {e}", path.display())))
}

/// Deletes the unit entries of `dir` other than `keep`, and scratch
/// files left for a minute.
fn prune(dir: &Path, keep: &[PathBuf]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        // Another build of this directory may be writing a scratch file.
        let stale = || {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() > 60)
        };
        let unit = p.extension().is_some_and(|x| {
            x == "o" || x == "bc" || x.to_string_lossy().starts_with("tmp") && stale()
        });
        if unit && !keep.contains(&p) {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// `object` with the placeholder package hash in its symbols replaced by
/// the real one (the same length: `_Y<8 hex>_`).
fn rehash(mut object: Vec<u8>, placeholder: &str, real: &str) -> Vec<u8> {
    let from = format!("_Y{placeholder}_");
    let to = format!("_Y{real}_");
    if from == to || from.len() != to.len() {
        return object;
    }
    let (from, to) = (from.as_bytes(), to.as_bytes());
    let mut i = 0;
    while let Some(at) = object[i..].windows(from.len()).position(|w| w == from) {
        let at = i + at;
        object[at..at + to.len()].copy_from_slice(to);
        i = at + to.len();
    }
    object
}

/// Runs `job` for each index below `n` on a thread per core (each with
/// a deep stack, as the compile thread has).
fn in_parallel<T: Send>(
    n: usize,
    job: impl Fn(usize) -> Result<T, RokDiagnostic> + Sync,
) -> Result<Vec<T>, RokDiagnostic> {
    crate::on_cores(n, 256 << 20, job)
        .map_err(|e| err(format!("cannot start a codegen thread: {e}")))?
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err(err("a codegen thread failed".to_owned()))))
        .collect()
}

type Built = (Vec<Vec<u8>>, UnitCounts, Vec<PathBuf>);

/// Objects by the native backend; a unit's object is its entry.
fn by_native(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
    plan: &Plan,
    dir: &Path,
) -> Result<Built, RokDiagnostic> {
    let nopts = crate::native::options(opts);
    let paths: Vec<PathBuf> = plan
        .units
        .iter()
        .map(|u| dir.join(format!("{}.o", u.key)))
        .collect();
    let fresh: Vec<Option<Vec<u8>>> = paths.iter().map(|p| std::fs::read(p).ok()).collect();
    let objects = in_parallel(plan.units.len(), |u| {
        if let Some(o) = &fresh[u] {
            return Ok((o.clone(), false));
        }
        let o = rokc_codegen_native::compile_unit(&module.mir, &nopts, sources, plan, u)?.object;
        write(&paths[u], &o)?;
        Ok((o, true))
    })?;
    Ok(finish_counts(plan, objects, None, paths))
}

fn finish_counts(
    plan: &Plan,
    objects: Vec<(Vec<u8>, bool)>,
    cached: Option<usize>,
    keep: Vec<PathBuf>,
) -> Built {
    let compiled: Vec<String> = objects
        .iter()
        .zip(&plan.units)
        .filter(|((_, c), _)| *c)
        .map(|(_, u)| u.name.clone())
        .collect();
    let counts = UnitCounts {
        reused: objects.len() - compiled.len(),
        compiled,
        cached,
    };
    (objects.into_iter().map(|(o, _)| o).collect(), counts, keep)
}

/// The MLIR text of each unit `want` says.
fn mlir(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
    plan: &Plan,
    data_layout: Option<String>,
    want: &[bool],
) -> Result<Vec<Option<String>>, RokDiagnostic> {
    let copts = crate::toolchain::codegen_options(opts, data_layout);
    rokc_diagnostic::time("mlir emit", || {
        (0..plan.units.len())
            .map(|u| {
                want[u]
                    .then(|| rokc_codegen_mlir::codegen_unit(&module.mir, &copts, sources, plan, u))
                    .transpose()
            })
            .collect()
    })
}

/// Objects by LLVM at `-O0`, a unit at a time; a unit's object is its
/// entry.
fn by_llvm_o0(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
    plan: &Plan,
    dir: &Path,
    t: &rokc_llvm::Target,
    data_layout: Option<String>,
) -> Result<Built, RokDiagnostic> {
    let paths: Vec<PathBuf> = plan
        .units
        .iter()
        .map(|u| dir.join(format!("{}.o", u.key)))
        .collect();
    let fresh: Vec<Option<Vec<u8>>> = paths.iter().map(|p| std::fs::read(p).ok()).collect();
    let want: Vec<bool> = fresh.iter().map(Option::is_none).collect();
    let texts = mlir(module, sources, opts, plan, data_layout, &want)?;
    let pipeline = rokc_llvm::mlir_pipeline(false);
    let c = llvm_options(t, &pipeline, rokc_llvm::OptLevel::O0);
    let objects = rokc_diagnostic::time("llvm", || {
        in_parallel(plan.units.len(), |u| match (&fresh[u], &texts[u]) {
            (Some(o), _) => Ok((o.clone(), false)),
            (None, Some(text)) => {
                let o = rokc_llvm::compile_mlir(text, &c)
                    .map_err(|e| err(format!("LLVM back end failed:\n{e}")))?;
                write(&paths[u], &o)?;
                Ok((o, true))
            }
            (None, None) => Err(err("internal: a unit without code".to_owned())),
        })
    })?;
    Ok(finish_counts(plan, objects, None, paths))
}

/// Objects above `-O0`: the units' bitcode through the thin link, whose
/// `thin/` cache spares unchanged units.
fn by_thin_link(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
    plan: &Plan,
    dir: &Path,
    t: &rokc_llvm::Target,
    data_layout: Option<String>,
) -> Result<Built, RokDiagnostic> {
    let paths: Vec<PathBuf> = plan
        .units
        .iter()
        .map(|u| dir.join(format!("{}.bc", u.key)))
        .collect();
    let fresh: Vec<Option<Vec<u8>>> = paths.iter().map(|p| std::fs::read(p).ok()).collect();
    let want: Vec<bool> = fresh.iter().map(Option::is_none).collect();
    let texts = mlir(module, sources, opts, plan, data_layout, &want)?;
    // Named by package too (a library's units may hold the same code as
    // another package's).
    let own = units::own_package(&module.mir.link);
    let names: Vec<String> = plan
        .units
        .iter()
        .map(|u| format!("{own}/{}", u.name))
        .collect();
    let inputs: Vec<(&str, rokc_llvm::UnitInput<'_>)> = names
        .iter()
        .enumerate()
        .map(|(u, name)| {
            let input = match (&fresh[u], &texts[u]) {
                (Some(b), _) => rokc_llvm::UnitInput::Bitcode(b),
                (None, Some(text)) => rokc_llvm::UnitInput::Mlir(text),
                (None, None) => rokc_llvm::UnitInput::Mlir(""),
            };
            (name.as_str(), input)
        })
        .collect();
    let opt = rokc_llvm::OptLevel::parse(&opts.opt_level)
        .ok_or_else(|| err(format!("unknown optimization level `{}`", opts.opt_level)))?;
    let pipeline = rokc_llvm::mlir_pipeline(true);
    let c = llvm_options(t, &pipeline, opt);
    let alloc = rokc_codegen_mlir::alloc_module(&module.mir.target);
    let cache = dir.join("thin");
    let out = rokc_diagnostic::time("llvm", || {
        rokc_llvm::compile_units(&inputs, &alloc, &c, &cache, module.mir.link.lib.is_some())
    })
    .map_err(|e| err(format!("LLVM back end failed:\n{e}")))?;
    for w in out.warnings.lines() {
        sources.report(&RokDiagnostic::warning(
            ErrorKind::Linkage,
            rokc_diagnostic::Span::DUMMY,
            w,
        ));
    }
    for (u, b) in out.bitcode.iter().enumerate() {
        if let Some(b) = b {
            write(&paths[u], b)?;
        }
    }
    let objects = out
        .objects
        .into_iter()
        .zip(&want)
        .map(|(o, w)| (o, *w))
        .collect();
    Ok(finish_counts(plan, objects, Some(out.cached), paths))
}

/// LLVM's options for a unit: no profile (`applies`).
const fn llvm_options<'a>(
    t: &'a rokc_llvm::Target,
    pipeline: &'a str,
    opt: rokc_llvm::OptLevel,
) -> rokc_llvm::Compile<'a> {
    rokc_llvm::Compile {
        target: t,
        opt,
        emit: rokc_llvm::Emit::Obj,
        tls_initial_exec: true,
        mlir_pipeline: pipeline,
        profile: rokc_llvm::Profile::None,
    }
}

fn err(msg: String) -> RokDiagnostic {
    RokDiagnostic::error(ErrorKind::Linkage, rokc_diagnostic::Span::DUMMY, msg)
}
