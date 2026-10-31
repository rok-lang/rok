//! The back half of the pipeline (spec 902.11-902.14), in process: MLIR
//! passes, LLVM IR, LLVM optimization and codegen, and the lld link, all
//! statically linked (`rokc_llvm`). `ROK_CC` links instead when set.

use std::borrow::Cow;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use rokc_core::tmp::{TempEntry, private_dir};
use rokc_diagnostic::{ErrorKind, RokDiagnostic, SourceMap};
use rokc_mir::TargetSpec;
use rokc_target::{Linker, ObjectFormat};

use crate::CompiledModule;

/// Intermediate artifact selectable with `--emit=<kind>` (spec 901).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmitKind {
    /// `source` (old name `ast`): the package's whole AST after meta
    /// expansion and desugaring, as source (needs `keep_ast`);
    /// `expansions` shows meta expansions alone.
    Ast,
    Hir,
    Mir,
    Mlir,
    LlvmIr,
    Asm,
    Obj,
    /// `expansions` (old name `expanded`): meta expansions as source
    /// text (spec 6.9.9); needs the parsed
    /// sources (`Session::set_keep_ast`), so `CompileResult::emit` makes it.
    Expanded,
}

impl EmitKind {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "source" | "ast" => Self::Ast,
            "hir" => Self::Hir,
            "mir" => Self::Mir,
            "mlir" => Self::Mlir,
            "llvmir" | "llvm-ir" | "ll" => Self::LlvmIr,
            "asm" => Self::Asm,
            "obj" => Self::Obj,
            "expansions" | "expanded" => Self::Expanded,
            _ => return None,
        })
    }

    /// The new name of old kind name `s`, read until 0.2.0.
    #[must_use]
    pub fn renamed(s: &str) -> Option<&'static str> {
        match s {
            "ast" => Some("source"),
            "expanded" => Some("expansions"),
            _ => None,
        }
    }

    /// File extension used under `target/`.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Ast => "ast.rok",
            Self::Hir => "hir",
            Self::Mir => "mir",
            Self::Mlir => "mlir",
            Self::LlvmIr => "ll",
            Self::Asm => "s",
            Self::Obj => "o",
            Self::Expanded => "expanded.rok",
        }
    }
}

/// How to build native code; the target is the compiled module's.
#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// `0`..`3`, `s` or `z`: LLVM's `-O<level>`.
    pub opt_level: String,
    /// Runtime library: a path when it has an extension, else `-l<name>`.
    pub rokr: String,
    /// A shipped C library (`Libc::shipped`): the C runtime objects and C library
    /// for the target (`<sysroot>/lib/rok/targets/<target>/`).
    pub libc: Option<PathBuf>,
    /// Keep the scratch directory instead of deleting it.
    pub keep_temps: bool,
    /// `-g`: DWARF debug info (spec 9.9.6).
    pub debug: bool,
    /// Codegen a REPL cell (`cell_ir`).
    pub repl: Option<rokc_codegen_mlir::ReplCell>,
    /// `-C overflow-checks=yes|no`; `None` = on at `-O0` only.
    pub overflow_checks: Option<bool>,
    /// `--load-mlir-plugin`: MLIR dialect plugins (stdlib bare 1.10).
    pub mlir_dialect_plugins: Vec<PathBuf>,
    /// `--load-mlir-pass-plugin`: MLIR pass plugins.
    pub mlir_pass_plugins: Vec<PathBuf>,
    /// `--mlir-pass <pass>`: extra MLIR passes, run before the
    /// lowering to the LLVM dialect (a plugin dialect's own lowering).
    pub mlir_passes: Vec<String>,
    /// `-C backend=native|llvm` (else `ROK_BACKEND`): who generates the
    /// machine code (`native::object`).
    pub backend: Option<String>,
    /// `-L <dir>`: native library search directories, before the C
    /// driver's `LIBRARY_PATH`.
    pub lib_dirs: Vec<PathBuf>,
    /// `-l [static=|dylib=]<name>`: native libraries linked as if named by
    /// `#[link]` (`true`: static).
    pub libs: Vec<(String, bool)>,
    /// `--native-runtime <file>`: shared libraries copied beside the
    /// executable, which searches its own directory (`@loader_path` /
    /// `$ORIGIN`; Windows always does).
    pub runtime_files: Vec<PathBuf>,
    /// `-C subsystem=console|windows`: a Windows executable's subsystem.
    pub subsystem: crate::windows::Subsystem,
    /// `-C profile-generate` / `-C profile-use`: IR profile-guided
    /// optimization, by LLVM at every level.
    pub profile: Profile,
    /// The profile runtime an instrumented executable links
    /// (`<sysroot>/lib/rok/targets/<target>/`).
    pub profile_rt: Option<PathBuf>,
    /// Build by codegen units, reusing unchanged units' objects from here
    /// (spec 9.2.16).
    pub incremental: Option<PathBuf>,
}

/// IR profile-guided optimization of the program and its source
/// dependencies.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Profile {
    #[default]
    Off,
    /// Instrumented: the program writes its raw profile to this file
    /// name pattern (`LLVM_PROFILE_FILE` overrides it at run time).
    Generate(PathBuf),
    /// Optimized with this indexed profile (`rokc profdata merge`).
    Use(PathBuf),
    /// `-C cs-profile-generate`: use this profile, then instrument again
    /// after inlining; merged with the first, it is the profile to use.
    CsGenerate(PathBuf, PathBuf),
}

impl Profile {
    /// The profile the build optimizes with.
    #[must_use]
    pub fn used(&self) -> Option<&Path> {
        match self {
            Self::Use(p) | Self::CsGenerate(p, _) => Some(p),
            _ => None,
        }
    }

    /// The build is instrumented (links the profile runtime).
    #[must_use]
    pub const fn generates(&self) -> bool {
        matches!(self, Self::Generate(_) | Self::CsGenerate(..))
    }
}

impl BuildOptions {
    /// The `release` profile (spec 9.2.15): `-O2`, `-O3` or `-Os`.
    #[must_use]
    pub fn release(&self) -> bool {
        matches!(self.opt_level.as_str(), "2" | "3" | "s")
    }
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            opt_level: "0".to_owned(),
            rokr: "rokr".to_owned(),
            libc: None,
            keep_temps: false,
            debug: false,
            repl: None,
            overflow_checks: None,
            mlir_dialect_plugins: Vec::new(),
            mlir_pass_plugins: Vec::new(),
            mlir_passes: Vec::new(),
            backend: rokc_core::env_choice("ROK_BACKEND", "native|llvm").map(str::to_owned),
            lib_dirs: Vec::new(),
            libs: Vec::new(),
            runtime_files: Vec::new(),
            subsystem: crate::windows::Subsystem::Console,
            profile: Profile::Off,
            profile_rt: None,
            incremental: None,
        }
    }
}

/// Private scratch directory, created exclusively and removed on drop
/// unless `keep` is set.
pub struct TempDir {
    path: PathBuf,
    keep: bool,
    /// What `new` made, removed by its own drop; `beside`'s has none.
    entry: Option<TempEntry>,
}

impl TempDir {
    pub fn new(keep: bool) -> Result<Self, RokDiagnostic> {
        let mut e = TempEntry::dir("rokc-")
            .map_err(|e| err(format!("cannot create a temporary directory: {e}")))?;
        if keep {
            e.keep();
        }
        Ok(Self {
            path: e.path().to_owned(),
            keep,
            entry: Some(e),
        })
    }

    /// `.<name>.objs` beside the output `out`, made anew: the same
    /// path in every build of `out`.
    pub fn beside(out: &Path, keep: bool) -> Result<Self, RokDiagnostic> {
        let mut name = std::ffi::OsString::from(".");
        name.push(out.file_name().unwrap_or_default());
        name.push(".objs");
        let path = out.with_file_name(name);
        let _ = std::fs::remove_dir_all(&path);
        // The parent may be a shared cache another process just trimmed.
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        private_dir(&path).map_err(|e| err(format!("cannot create {}: {e}", path.display())))?;
        Ok(Self {
            path,
            keep,
            entry: None,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.keep {
            eprintln!(
                "{}: kept temporaries in {}",
                rokc_core::cli::invoked_as(),
                self.path.display()
            );
        } else if self.entry.is_none() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn err(msg: String) -> RokDiagnostic {
    RokDiagnostic::error(ErrorKind::Linkage, rokc_diagnostic::Span::DUMMY, msg)
}

/// Codegen a module to MLIR text for its target.
pub fn mlir_text(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
) -> Result<rokc_codegen_mlir::MlirOutput, RokDiagnostic> {
    codegen(
        module,
        sources,
        opts,
        Some(module.mir.target.data_layout.clone()),
    )
}

/// Codegen for its errors alone (`rokc check`).
pub fn check_codegen(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
) -> Result<(), RokDiagnostic> {
    codegen(module, sources, opts, None).map(drop)
}

fn codegen(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
    data_layout: Option<String>,
) -> Result<rokc_codegen_mlir::MlirOutput, RokDiagnostic> {
    rokc_diagnostic::time("mlir emit", || {
        rokc_codegen_mlir::codegen(&module.mir, &codegen_options(opts, data_layout), sources)
    })
}

/// The MLIR emitter's options of a build.
pub fn codegen_options(
    opts: &BuildOptions,
    data_layout: Option<String>,
) -> rokc_codegen_mlir::CodegenOptions {
    rokc_codegen_mlir::CodegenOptions {
        overflow_checks: opts.overflow_checks.unwrap_or(opts.opt_level == "0"),
        debug: opts.debug,
        optimized: opts.opt_level != "0",
        repl: opts.repl.clone(),
        data_layout,
    }
}

/// Produce one artifact's bytes.
pub fn emit(
    module: &CompiledModule,
    sources: &SourceMap,
    kind: EmitKind,
    opts: &BuildOptions,
) -> Result<Vec<u8>, RokDiagnostic> {
    if kind == EmitKind::Obj {
        if let Some(o) = crate::native::object(module, sources, opts)? {
            return Ok(o.object);
        }
    }
    match kind {
        EmitKind::Ast => Err(RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Internal,
            rokc_diagnostic::Span::DUMMY,
            "--emit=ast needs the parsed files (keep_ast)",
        )),
        EmitKind::Hir => Ok(rokc_hir::print_hir(&module.hir).into_bytes()),
        EmitKind::Mir => Ok(rokc_mir::print_mir(&module.mir).into_bytes()),
        EmitKind::Expanded => Err(err("`--emit=expanded` needs the parsed sources".to_owned())),
        EmitKind::Mlir => Ok(mlir_text(module, sources, opts)?.text.into_bytes()),
        EmitKind::LlvmIr => Ok(lower_to_llvm(module, sources, opts)?.0.into_bytes()),
        EmitKind::Asm => Ok(llvm(module, sources, opts, rokc_llvm::Emit::Asm, 1)?
            .0
            .remove(0)),
        EmitKind::Obj => Ok(llvm(module, sources, opts, rokc_llvm::Emit::Obj, 1)?
            .0
            .remove(0)),
    }
}

/// Build an executable at `out`; by codegen units with
/// `opts.incremental`, returning how many it compiled.
pub fn link_executable(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
    out: &Path,
) -> Result<Option<crate::units::UnitCounts>, RokDiagnostic> {
    let mut counts = None;
    let objects = if let Some(dir) = opts
        .incremental
        .as_deref()
        .filter(|_| crate::units::applies(module, opts))
    {
        let (objects, c, _) = crate::units::objects(module, sources, opts, dir)?;
        counts = Some(c);
        objects
    } else if let Some(o) = crate::native::object(module, sources, opts)? {
        vec![o.object]
    } else {
        llvm(module, sources, opts, rokc_llvm::Emit::Obj, 0)?.0
    };
    // A Mach-O debug map names the objects (dsymutil reads them before
    // they go): the same names in every build.
    let tmp = TempDir::beside(out, opts.keep_temps)?;
    let mut paths = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        let p = tmp.path().join(format!("module{i}.o"));
        std::fs::write(&p, o).map_err(|e| err(format!("cannot write {}: {e}", p.display())))?;
        paths.push(p);
    }
    link_object(module, &paths, opts, out, tmp.path())?;
    Ok(counts)
}

/// Library build (spec 902.13): `libfoo.a`.
///
/// By codegen units with `opts.incremental`, else as many members as LLVM
/// compiles in parallel. A prebuilt-library build writes `foo.rokhir`
/// beside it (904), a sibling rather than a member so no object-section
/// reader is needed.
pub fn build_archive(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
    archive_out: &Path,
) -> Result<Option<crate::units::UnitCounts>, RokDiagnostic> {
    let mut counts = None;
    let (objects, gc_types) = if let Some(dir) = opts
        .incremental
        .as_deref()
        .filter(|_| crate::units::applies(module, opts))
    {
        let (objects, c, gc_types) = crate::units::objects(module, sources, opts, dir)?;
        counts = Some(c);
        (objects, gc_types)
    } else if let Some(o) = crate::native::object(module, sources, opts)? {
        (vec![o.object], o.gc_types)
    } else {
        let (objs, mlir) = llvm(module, sources, opts, rokc_llvm::Emit::Obj, 0)?;
        (objs, mlir.gc_types)
    };
    let tmp = TempDir::new(opts.keep_temps)?;
    let mut paths = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        let p = tmp.path().join(format!("part{i}.o"));
        std::fs::write(&p, o).map_err(|e| err(format!("cannot write {}: {e}", p.display())))?;
        paths.push(p);
    }
    // A fresh archive, not members appended to an old one.
    let _ = std::fs::remove_file(archive_out);
    let t = &module.mir.target.spec;
    let members: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    rokc_llvm::write_archive(t, archive_out, &members)
        .map_err(|e| err(format!("cannot write {}: {e}", archive_out.display())))?;
    if let Some(r) = &module.interface {
        let mut r = r.clone();
        r.type_count = gc_types;
        let path = archive_out.with_file_name(format!("{}.rokhir", r.name));
        std::fs::write(&path, r.encode())
            .map_err(|e| err(format!("cannot write {}: {e}", path.display())))?;
    }
    Ok(counts)
}

/// A REPL cell (`opts.repl`) as an object for the session host, built by
/// LLVM, and what codegen says about it.
pub fn cell_object(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
) -> Result<(Vec<u8>, rokc_codegen_mlir::MlirOutput), RokDiagnostic> {
    let (mut objs, mlir) = llvm(module, sources, opts, rokc_llvm::Emit::Obj, 1)?;
    Ok((objs.remove(0), mlir))
}

/// The module's LLVM IR, as translated from MLIR (`--emit=llvm-ir`).
pub fn lower_to_llvm(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
) -> Result<(String, rokc_codegen_mlir::MlirOutput), RokDiagnostic> {
    let (mut ir, mlir) = llvm(module, sources, opts, rokc_llvm::Emit::LlvmIr, 1)?;
    let ir =
        String::from_utf8(ir.remove(0)).map_err(|_| err("LLVM printed non-UTF-8 IR".to_owned()))?;
    Ok((ir, mlir))
}

/// MLIR text below this size compiles as one object: splitting costs
/// more than the cores win back.
const SPLIT_MIN: usize = 1 << 20;

/// The module through LLVM in process: MLIR passes (plugins' first), LLVM
/// IR, `-O<level>` and codegen. `parts` 0: as many objects as the size
/// warrants (in-process `ThinLTO` above -O0, so inlining sees everything).
fn llvm(
    module: &CompiledModule,
    sources: &SourceMap,
    opts: &BuildOptions,
    emit: rokc_llvm::Emit,
    parts: usize,
) -> Result<(Vec<Vec<u8>>, rokc_codegen_mlir::MlirOutput), RokDiagnostic> {
    let mut mlir = mlir_text(module, sources, opts)?;
    let t = &module.mir.target.spec;
    let opt = rokc_llvm::OptLevel::parse(&opts.opt_level)
        .ok_or_else(|| err(format!("unknown optimization level `{}`", opts.opt_level)))?;
    let pipeline = rokc_llvm::mlir_pipeline(opts.opt_level != "0", &opts.mlir_passes);
    let dialects: Vec<&Path> = opts
        .mlir_dialect_plugins
        .iter()
        .map(PathBuf::as_path)
        .collect();
    let passes: Vec<&Path> = opts
        .mlir_pass_plugins
        .iter()
        .map(PathBuf::as_path)
        .collect();
    let c = rokc_llvm::Compile {
        target: t,
        opt,
        emit,
        // JIT'd Mach-O cells reach thread-locals through TLV
        // descriptors; the ELF loader resolves initial exec.
        tls_initial_exec: opts.repl.is_none() || t.object_format != ObjectFormat::MachO,
        mlir_pipeline: &pipeline,
        dialect_plugins: &dialects,
        pass_plugins: &passes,
        profile: match &opts.profile {
            Profile::Off => rokc_llvm::Profile::None,
            Profile::Generate(p) => rokc_llvm::Profile::Generate(p),
            Profile::Use(p) => rokc_llvm::Profile::Use(p),
            Profile::CsGenerate(u, g) => rokc_llvm::Profile::CsGenerate(u, g),
        },
    };
    let parts = if parts == 0 {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        let jobs = cores.min(mlir.text.len() / (SPLIT_MIN / 4)).min(16);
        if mlir.text.len() < SPLIT_MIN || jobs < 2 {
            1
        } else {
            jobs
        }
    } else {
        parts
    };
    let text = std::mem::take(&mut mlir.text);
    let (out, warnings) =
        rokc_diagnostic::time("llvm", || rokc_llvm::compile_mlir_split(&text, &c, parts))
            .map_err(|e| err(format!("LLVM back end failed:\n{e}")))?;
    for w in warnings.lines() {
        SourceMap::new().report(&RokDiagnostic::warning(
            ErrorKind::Linkage,
            rokc_diagnostic::Span::DUMMY,
            w,
        ));
    }
    mlir.text = text;
    Ok((out, mlir))
}

/// Link `objs` with the runtime and dependencies into `out`.
///
/// lld in process, or `ROK_CC`. Windows import libraries
/// (`windows::link_args`) go into `objs_dir` beside the objects, since the
/// PDB names their paths.
pub fn link_object(
    module: &CompiledModule,
    objs: &[PathBuf],
    opts: &BuildOptions,
    out: &Path,
    objs_dir: &Path,
) -> Result<(), RokDiagnostic> {
    let t = &*module.mir.target.spec;
    let apple = t.object_format == ObjectFormat::MachO;
    let windows = t.object_format == ObjectFormat::Coff;
    let search: Vec<OsString> = opts
        .lib_dirs
        .iter()
        .map(|d| {
            let mut l = OsString::from("-L");
            l.push(d);
            l
        })
        .collect();
    let mut libs = module.mir.link.native_libs.clone();
    for l in &opts.libs {
        if !libs.iter().any(|(n, _)| *n == l.0) {
            libs.push(l.clone());
        }
    }
    // The loader looks beside the executable for the shared libraries
    // copied there.
    let origin = if opts.runtime_files.is_empty() || windows {
        None
    } else if apple {
        Some("@loader_path")
    } else {
        Some("$ORIGIN")
    };
    let wit = module.mir.link.wit.as_ref();
    if let Some(cc) = c_compiler() {
        let tmp = TempDir::new(opts.keep_temps)?;
        let mut link = Command::new(cc);
        link.args(objs)
            .args(&module.archives)
            .arg(rokr_arg(opts, t));
        link.args(&search);
        link.args(cc_wit_args(wit, tmp.path())?);
        if let Some((rt, sym)) = profile_runtime(t, opts) {
            link.arg(rt).arg(format!("-Wl,-u,{sym}"));
        }
        for (name, stat) in &libs {
            if *stat && apple {
                link.arg(static_archive(name, opts)?);
            } else if *stat {
                link.args(["-Wl,-Bstatic", &format!("-l{name}"), "-Wl,-Bdynamic"]);
            } else {
                link.arg(format!("-l{name}"));
            }
        }
        if apple {
            link.arg("-Wl,-dead_strip");
        } else if !windows {
            link.arg("-Wl,--gc-sections");
        }
        if let Some(o) = origin {
            link.arg(format!("-Wl,-rpath,{o}"));
        }
        link.args(cc_profile_args(t, opts));
        // Apple's clang links for the machine it runs as (not rokc's,
        // under Rosetta) unless told.
        if !t.is_host() || apple {
            link.arg(format!("--target={}", cc_target(t)));
        }
        // core's float methods call libm; glibc keeps it out of libc.
        if !windows {
            link.arg("-lm");
        }
        link.arg("-o").arg(out);
        run(&mut link, "the link")?;
    } else if windows {
        let mut inputs: Vec<OsString> = objs.iter().map(|o| o.clone().into_os_string()).collect();
        inputs.extend(module.archives.iter().map(|a| a.clone().into_os_string()));
        inputs.push(rokr_arg(opts, t).into());
        if let Some((rt, sym)) = profile_runtime(t, opts) {
            inputs.extend([rt.into(), format!("/include:{sym}").into()]);
        }
        inputs.extend(search);
        inputs.extend(
            crate::windows::link_args(t, &libs, opts.subsystem, opts.debug, objs_dir)
                .map_err(|e| err(format!("the link failed:\n{e}")))?,
        );
        rokc_diagnostic::time("link", || {
            rokc_llvm::link_executable(t, &inputs, out, true, None)
        })
        .map_err(|e| err(format!("the link failed:\n{e}")))?;
    } else {
        let tmp = TempDir::new(opts.keep_temps)?;
        let mut inputs: Vec<OsString> = objs.iter().map(|o| o.clone().into_os_string()).collect();
        if t.object_format == ObjectFormat::Wasm && t.threads {
            inputs.push(wasm_tls_object(t, tmp.path())?.into_os_string());
        }
        if let Some(w) = wit {
            inputs.extend(wit_link_args(w, tmp.path())?);
        }
        inputs.extend(module.archives.iter().map(|a| a.clone().into_os_string()));
        inputs.push(rokr_arg(opts, t).into());
        if let Some((rt, sym)) = profile_runtime(t, opts) {
            // wasm-component-ld forwards only the long form.
            let u = if t.object_format == ObjectFormat::Wasm {
                "--undefined"
            } else {
                "-u"
            };
            inputs.extend([rt.into(), u.into(), sym.into()]);
        }
        inputs.extend(search);
        inputs.extend(native_lib_args(&libs, apple, opts)?);
        if let Some(o) = origin {
            inputs.extend(["-rpath".into(), o.into()]);
        }
        inputs.extend(release_link_args(t, opts).into_iter().map(OsString::from));
        let t = &*entry_model(t, wit);
        rokc_diagnostic::time("link", || {
            rokc_llvm::link_executable(t, &inputs, out, true, opts.libc.as_deref())
        })
        .map_err(|e| err(format!("the link failed:\n{e}")))?;
    }
    let dir = out.parent().unwrap_or_else(|| Path::new("."));
    for f in &opts.runtime_files {
        let to = dir.join(f.file_name().unwrap_or_default());
        // A new file: one a running program maps stays intact.
        let _ = std::fs::remove_file(&to);
        std::fs::copy(f, &to).map_err(|e| {
            err(format!(
                "cannot copy {} to {}: {e}",
                f.display(),
                to.display()
            ))
        })?;
    }
    // Mach-O keeps DWARF in the objects: gather it into a `.dSYM` while
    // they still exist.
    if opts.debug && apple {
        rokc_diagnostic::time("dsymutil", || rokc_llvm::dsymutil(&[out.as_os_str()]))
            .map_err(|e| err(format!("{}: {e}", out.display())))?;
    }
    Ok(())
}

/// lld arguments of a `release` link (9.2.15): safe identical code folding
/// and, without `-g`, only the symbols the loader needs. COFF folds
/// without `/debug` already; wasm keeps its name section.
fn release_link_args(t: &TargetSpec, opts: &BuildOptions) -> Vec<&'static str> {
    let strip = !opts.debug;
    match t.object_format {
        _ if !opts.release() => Vec::new(),
        ObjectFormat::MachO if strip => vec!["--icf=safe", "-x", "-no_exported_symbols"],
        ObjectFormat::Elf if strip => vec!["--icf=safe", "--strip-all"],
        ObjectFormat::MachO | ObjectFormat::Elf => vec!["--icf=safe"],
        _ => Vec::new(),
    }
}

/// `ROK_CC` arguments for the profile: `-g`, or a release link's
/// stripping.
fn cc_profile_args(t: &TargetSpec, opts: &BuildOptions) -> &'static [&'static str] {
    match t.object_format {
        _ if opts.debug => &["-g"],
        _ if !opts.release() => &[],
        ObjectFormat::MachO => &["-Wl,-x,-no_exported_symbols"],
        ObjectFormat::Elf => &["-s"],
        _ => &[],
    }
}

/// `wasm-component-ld` arguments for `ffi::wit` (stdlib 1.8): the world in
/// a directory under `tmp`, its sources under `deps/`, and the core
/// exports backing the world's functions.
fn wit_link_args(w: &rokc_mir::WitLink, tmp: &Path) -> Result<Vec<OsString>, RokDiagnostic> {
    let io = |p: &Path, e: std::io::Error| err(format!("cannot write {}: {e}", p.display()));
    let dir = tmp.join("wit");
    let deps = dir.join("deps");
    std::fs::create_dir_all(&deps).map_err(|e| io(&deps, e))?;
    let world = dir.join("component.wit");
    std::fs::write(&world, &w.world).map_err(|e| io(&world, e))?;
    for (i, src) in w.sources.iter().enumerate() {
        let stem = src
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("wit");
        let to = deps.join(format!("{i}-{stem}"));
        if src.dir {
            std::fs::create_dir_all(&to).map_err(|e| io(&to, e))?;
        }
        for (path, text) in &src.files {
            let dst = if src.dir {
                to.join(path.file_name().unwrap_or_default())
            } else {
                deps.join(format!("{i}-{stem}.wit"))
            };
            std::fs::write(&dst, text).map_err(|e| io(&dst, e))?;
        }
    }
    let mut a: Vec<OsString> = vec!["--component-type".into(), dir.into_os_string()];
    a.extend(w.exports.iter().map(|s| format!("--export={s}").into()));
    Ok(a)
}

/// The target to link for: a component without `main` is a reactor
/// (`crt1-reactor.o`, no entry; the host calls `_initialize`).
fn entry_model<'t>(t: &'t TargetSpec, wit: Option<&rokc_mir::WitLink>) -> Cow<'t, TargetSpec> {
    if wit.is_some_and(|w| w.reactor) {
        Cow::Owned(TargetSpec {
            crt_objects: vec!["crt1-reactor.o".to_owned()],
            pre_link_args: [&t.pre_link_args[..], &["--no-entry".to_owned()]].concat(),
            ..t.clone()
        })
    } else {
        Cow::Borrowed(t)
    }
}

/// The C compiler driver's name of target `t`: clang calls a WASI
/// preview 2 component by its Rust name (`wasm32-wasip2`).
fn cc_target(t: &TargetSpec) -> String {
    if t.linker == Linker::WasmComponentLd {
        t.rust_target.clone()
    } else {
        t.llvm_triple()
    }
}

/// `wit_link_args` for the C compiler driver (`ROK_CC`), which hands
/// them to `wasm-component-ld`; a reactor's exec model.
fn cc_wit_args(w: Option<&rokc_mir::WitLink>, tmp: &Path) -> Result<Vec<OsString>, RokDiagnostic> {
    let Some(w) = w else {
        return Ok(Vec::new());
    };
    let args = wit_link_args(w, tmp)?;
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut wl = OsString::from("-Wl,");
        wl.push(a);
        if a == "--component-type" {
            wl.push(",");
            wl.push(it.next().map_or_else(OsString::new, Clone::clone));
        }
        out.push(wl);
    }
    if w.reactor {
        out.push("-mexec-model=reactor".into());
    }
    Ok(out)
}

/// compiler-rt's profile runtime and the symbol that pulls in its
/// registration (which writes the profile at exit).
fn profile_runtime(t: &TargetSpec, opts: &BuildOptions) -> Option<(PathBuf, String)> {
    if !opts.profile.generates() {
        return None;
    }
    let rt = opts.profile_rt.clone()?;
    Some((rt, format!("{}__llvm_profile_runtime", t.c_symbol_prefix)))
}

/// lld's arguments for the `#[link]` libraries `libs` (name, static).
fn native_lib_args(
    libs: &[(String, bool)],
    apple: bool,
    opts: &BuildOptions,
) -> Result<Vec<OsString>, RokDiagnostic> {
    let mut a: Vec<OsString> = Vec::new();
    for (name, stat) in libs {
        if *stat && apple {
            a.push(static_archive(name, opts)?.into_os_string());
        } else if *stat {
            a.extend([
                "-Bstatic".into(),
                format!("-l{name}").into(),
                "-Bdynamic".into(),
            ]);
        } else {
            a.push(format!("-l{name}").into());
        }
    }
    Ok(a)
}

/// `rok_tls` (803 §8.3.1) for `wasm32-wasi-threads` in wasm TLS, with the
/// accessor the runtime uses (`rokr_common::tls`; stable Rust can't define
/// it there), written to `dir`.
fn wasm_tls_object(t: &TargetSpec, dir: &Path) -> Result<PathBuf, RokDiagnostic> {
    let size = rokc_mir::tls_abi::SIZE.offset(t.pointer_width / 8);
    let align = rokc_mir::tls_abi::ALIGN;
    let ir = format!(
        "declare ptr @llvm.threadlocal.address.p0(ptr)\n\
         @rok_tls = thread_local global [{size} x i8] zeroinitializer, align {align}\n\
         define hidden ptr @rok_tls_addr() nounwind {{\n\
         \x20 %p = call ptr @llvm.threadlocal.address.p0(ptr @rok_tls)\n\
         \x20 ret ptr %p\n}}\n"
    );
    let obj = rokc_llvm::compile_llvm_ir(
        &ir,
        &rokc_llvm::Compile {
            target: t,
            opt: rokc_llvm::OptLevel::O2,
            emit: rokc_llvm::Emit::Obj,
            tls_initial_exec: false,
            mlir_pipeline: "",
            dialect_plugins: &[],
            pass_plugins: &[],
            profile: rokc_llvm::Profile::None,
        },
    )
    .map_err(|e| err(format!("the TLS object: {e}")))?;
    let path = dir.join("rok_tls.o");
    std::fs::write(&path, obj).map_err(|e| err(format!("cannot write {}: {e}", path.display())))?;
    Ok(path)
}

/// `#[link(name, kind = "static")]` for ld64, which has no `-Bstatic` and
/// prefers `.dylib`: the first `lib<name>.a` in `-L` dirs, then
/// `LIBRARY_PATH`.
fn static_archive(name: &str, opts: &BuildOptions) -> Result<PathBuf, RokDiagnostic> {
    let env = std::env::var_os("LIBRARY_PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = opts
        .lib_dirs
        .iter()
        .cloned()
        .chain(std::env::split_paths(&env).filter(|d| !d.as_os_str().is_empty()))
        .collect();
    let file = format!("lib{name}.a");
    dirs.iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            err(format!(
                "`#[link(name = \"{name}\", kind = \"static\")]`: no {file} in the -L directories \
             or LIBRARY_PATH"
            ))
        })
}

/// `--rokr`: a library path when it has an extension, else `-l<name>`
/// (COFF: the library's file name).
fn rokr_arg(opts: &BuildOptions, t: &TargetSpec) -> String {
    if Path::new(&opts.rokr).extension().is_some() {
        opts.rokr.clone()
    } else if t.object_format == ObjectFormat::Coff {
        t.staticlib(&opts.rokr)
    } else {
        format!("-l{}", opts.rokr)
    }
}

/// `--load-mlir-plugin <path | name>` (stdlib bare 1.10.3): a path must
/// exist; a bare name is looked up ([`mlir_plugin_files`]) in
/// `ROK_MLIR_PLUGIN_PATH`.
///
/// # Errors
/// The plugin is not there.
pub fn find_mlir_plugin(spec: &str) -> Result<PathBuf, String> {
    let p = Path::new(spec);
    if p.components().count() > 1 || p.extension().is_some() {
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(format!("MLIR plugin `{spec}`: no such file"))
        };
    }
    let dirs = std::env::var_os("ROK_MLIR_PLUGIN_PATH").unwrap_or_default();
    let names = mlir_plugin_files(spec, std::env::consts::OS);
    std::env::split_paths(&dirs)
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|c| c.is_file())
        .ok_or_else(|| {
            format!(
                "MLIR plugin `{spec}` not found (looked for {} in ROK_MLIR_PLUGIN_PATH)",
                names.join(" / ")
            )
        })
}

/// File names of plugin `name` on `os`: `<name>.dll`, `lib<name>.so`, and
/// on macOS `lib<name>.dylib` first (cmake modules are `.so` there too).
fn mlir_plugin_files(name: &str, os: &str) -> Vec<String> {
    match os {
        "windows" => vec![format!("{name}.dll")],
        "macos" => vec![format!("lib{name}.dylib"), format!("lib{name}.so")],
        _ => vec![format!("lib{name}.so")],
    }
}

/// The REPL host: `repl_host.c` with the whole runtime linked in and
/// exported, for the cells it links itself.
pub fn link_host(host_object: &Path, opts: &BuildOptions, out: &Path) -> Result<(), RokDiagnostic> {
    let t = TargetSpec::host();
    if t.object_format == ObjectFormat::Coff {
        return link_windows_host(t, host_object, opts, out);
    }
    let cc = c_compiler();
    let flag = |f: &str| -> OsString {
        if cc.is_some() {
            format!("-Wl,{f}").into()
        } else {
            f.into()
        }
    };
    let obj = host_object.as_os_str().to_owned();
    let rokr = OsString::from(rokr_arg(opts, t));
    // ELF: the runtime in the dynamic symbol table, for `dlsym`.
    let inputs = if t.object_format == ObjectFormat::MachO {
        let mut v = vec![obj, flag("-all_load"), rokr];
        if cc.is_some() {
            v.push(format!("--target={}", t.llvm_triple()).into());
        }
        v
    } else {
        vec![
            flag("--export-dynamic"),
            obj,
            flag("--whole-archive"),
            rokr,
            flag("--no-whole-archive"),
        ]
    };
    if let Some(cc) = cc {
        let mut link = Command::new(cc);
        link.args(&inputs).arg("-lm").arg("-o").arg(out);
        return run(&mut link, "the REPL host link");
    }
    rokc_llvm::link_executable(t, &inputs, out, false, opts.libc.as_deref())
        .map(drop)
        .map_err(|e| err(format!("cannot link the REPL host:\n{e}")))
}

/// Windows host: every runtime member, every symbol exported (found with
/// `GetProcAddress`), linked like a program.
fn link_windows_host(
    t: &TargetSpec,
    host_object: &Path,
    opts: &BuildOptions,
    out: &Path,
) -> Result<(), RokDiagnostic> {
    let tmp = TempDir::new(opts.keep_temps)?;
    let mut inputs: Vec<OsString> = vec![
        host_object.into(),
        format!("/wholearchive:{}", rokr_arg(opts, t)).into(),
        // lld-link honors it in its MinGW mode only.
        "-lldmingw".into(),
        "/export-all-symbols".into(),
    ];
    let fail = |e: String| err(format!("cannot link the REPL host:\n{e}"));
    inputs.extend(
        crate::windows::link_args(t, &[], crate::Subsystem::Console, false, tmp.path())
            .map_err(fail)?,
    );
    rokc_llvm::link_executable(t, &inputs, out, false, None)
        .map(drop)
        .map_err(fail)
}

/// The C compiler driver that links instead of lld: `ROK_CC`, else the
/// system's `cc` for a rokc built without LLVM (no lld either).
fn c_compiler() -> Option<OsString> {
    std::env::var_os("ROK_CC")
        .filter(|c| !c.is_empty())
        .or_else(|| (!rokc_llvm::AVAILABLE).then(|| "cc".into()))
}

/// rokc's build cache, beside the toolchain when writable.
///
/// `<sysroot>/lib/rok-cache` when installed, else `rok-cache/` beside
/// `rokc`. A read-only install (a Nix store path, a system package) uses
/// `rok/rokc-<hash of the sysroot>` in the user's cache directory.
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let exe = rokc_core::current_exe()?;
        let bin = exe.parent()?;
        let sysroot = bin.parent()?;
        let own = if sysroot.join("lib/rok/core/src").is_dir() {
            sysroot.join("lib/rok-cache")
        } else {
            bin.join("rok-cache")
        };
        if writable(&own) {
            return Some(own);
        }
        let key = rokc_core::hash::fnv(sysroot.as_os_str().as_encoded_bytes());
        Some(user_cache()?.join(format!("rokc-{key:016x}")))
    })
    .clone()
}

/// `dir` exists or can be made, and takes a new file.
fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".probe-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// `rok/` in the user's cache directory.
fn user_cache() -> Option<PathBuf> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        env("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Caches"))
    } else {
        env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|h| h.join(".cache")))
    };
    Some(base?.join("rok"))
}

fn run(cmd: &mut Command, what: &str) -> Result<(), RokDiagnostic> {
    let out = rokc_diagnostic::time(what, || cmd.output())
        .map_err(|e| err(format!("cannot run {what}: {e}")))?;
    if !out.status.success() {
        return Err(err(format!(
            "{what} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn mlir_plugins_are_found_by_path_or_name() {
        assert!(super::find_mlir_plugin("/no/such/libx.so").is_err());
        assert!(super::find_mlir_plugin("no_such_plugin_xyz").is_err());
    }

    #[test]
    fn mlir_plugin_names_follow_the_host() {
        use super::mlir_plugin_files as f;
        assert_eq!(f("d", "windows"), ["d.dll"]);
        assert_eq!(f("d", "macos"), ["libd.dylib", "libd.so"]);
        assert_eq!(f("d", "linux"), ["libd.so"]);
    }
}
