//! The compiler as a library: sessions, package discovery, prebuilt
//! dependencies and incremental-build records, for the `rokc` binary and
//! its embedded tools (REPL, Jupyter, LSP).

mod deps;
mod fresh;
mod package;
mod std_cache;

use std::path::{Path, PathBuf};

pub use deps::DepBuild;
pub use fresh::Fingerprint;
pub use package::{
    Package, STDIN, code_name, discover, package_root, root_file, source_files, source_files_with,
};
pub use std_cache::{StdKey, prebuilt_std};

use rokc_driver::{CompiledModule, CompilerConfig, compile_file, compile_source};

pub use rokc_diagnostic::{
    ErrorKind, Json, RokDiagnostic, SourceMap, capture_warnings, end_if_output_closed,
    print_rendered, set_report_depth, set_report_style,
};
pub use rokc_driver::native::chosen as native_backend;
pub use rokc_driver::units::UnitCounts;
pub use rokc_driver::{
    AS_WASM_LD, BuildOptions, Demangled, EmitKind, LLVM_VERSION, Profile, ReplCell, ReplFn,
    SourceFile, Subsystem, TargetSpec, TempDir, bootstrap_tiers, demangle, feature_of,
    find_mlir_plugin, link_host, profdata_merge, profdata_show, repl_host, rokhir, wasm_ld_main,
};
pub use rokc_target::{BUILTIN as BUILTIN_TARGETS, RENAMED as RENAMED_TARGETS};

#[derive(Default)]
pub struct Session {
    config: CompilerConfig,
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a `.rok` file to the bootstrap operator pre-pass; order only
    /// affects diagnostic order.
    pub fn add_bootstrap_file(&mut self, path: PathBuf) {
        self.config.bootstrap_files.push(path);
    }

    /// A dependency package (`--extern name=root`, spec 902.3): its
    /// `lib.rok` is compiled with this build and mounted at `name`.
    pub fn add_extern(&mut self, name: String, root: PathBuf) {
        self.config.externs.push((name, root));
    }

    /// Extern `name` is the package's own library, which the executable
    /// being built uses (spec 0.3.1).
    pub fn set_own_library(&mut self, name: Option<String>) {
        self.config.own_library = name;
    }

    /// Add a `--cfg` flag (either `"flag"` or `"key=value"`). Consulted
    /// by the `#[cfg(...)]` filter pass before HIR.
    pub fn add_cfg(&mut self, flag: String) {
        self.config.cfg_flags.push(flag);
    }

    /// The active cfg, one `flag` / `key="value"` per line
    /// (`rokc --print cfg`).
    #[must_use]
    pub fn cfg_lines(&self) -> Vec<String> {
        rokc_driver::cfg_env(&self.config).lines()
    }

    /// `rokc test` build: keep `#[test]` functions, generate the runner
    /// `main`, set `cfg(test)`.
    pub const fn set_test(&mut self, on: bool) {
        self.config.test = on;
    }

    /// `rokc bench` build: keep `#[bench]` functions, generate the
    /// timing `main`, set `cfg(test)`.
    pub const fn set_bench(&mut self, on: bool) {
        self.config.bench = on;
    }

    /// Keep each parsed file's AST in `CompileResult::sources` (lint,
    /// doc).
    pub const fn set_keep_ast(&mut self, on: bool) {
        self.config.keep_ast = on;
    }

    /// With `set_keep_ast`: a kept library stage may stand for the
    /// library's files (`CompileResult::library`).
    pub const fn set_library_items(&mut self, on: bool) {
        self.config.library_items = on;
    }

    /// Build the library package as prebuilt dependency `name`
    /// (`archive` then also writes `<name>.rokhir`, spec 904).
    pub fn set_lib_name(&mut self, name: Option<String>) {
        self.config.lib_name = name;
    }

    /// The input is a lone file outside any package (spec 4.16.8).
    pub const fn set_anonymous(&mut self, on: bool) {
        self.config.anonymous = on;
    }

    /// Compile for `target` (`--target`; the host by default).
    pub fn set_target(&mut self, target: &TargetSpec) {
        self.config.target = rokc_driver::Target::new(target.clone());
    }

    /// Keep the library's parse across compiles (a long-lived session
    /// over a library that does not change: the REPL).
    pub const fn set_cache_library(&mut self, on: bool) {
        self.config.cache_library = on;
    }

    /// Build the bootstrap tiers as the prebuilt library `rok`.
    pub const fn set_std_lib(&mut self, on: bool) {
        self.config.std_lib = on;
    }

    /// Link the bootstrap tiers prebuilt (`rok.rokhir`, `prebuilt_std`).
    pub fn set_std_prebuilt(&mut self, rokhir: Option<PathBuf>) {
        self.config.std_prebuilt = rokhir;
    }

    /// Read `path` (absolute) as `text` instead of from the disk.
    pub fn add_buffer(&mut self, path: PathBuf, text: String) {
        self.config.buffers.push((path, text));
    }

    /// Lower (and so check) every library body, reached or not.
    pub const fn set_check_library(&mut self, on: bool) {
        self.config.check_library = on;
    }

    /// Compile the small prebuilt standard library functions reached
    /// too, for the optimizer to inline (an optimized build).
    pub const fn set_inline_prebuilt(&mut self, on: bool) {
        self.config.inline_prebuilt = on;
    }

    /// Keep parses and lowered bodies in `dir` for the next compile of
    /// the package (spec 9.2.16).
    pub fn set_incremental(&mut self, dir: Option<PathBuf>) {
        self.config.incremental = dir;
    }

    pub fn compile_file(&self, path: &Path) -> CompileResult {
        let mut map = SourceMap::new();
        let r = compile_file(path, &self.config, &mut map);
        Self::result(r, map)
    }

    /// `compile_file` with the root's text given (stdin, an editor, the
    /// REPL); `path` still locates `mod foo;` files, `map` holds known views.
    pub fn compile_source(&self, source: &str, path: &Path, mut map: SourceMap) -> CompileResult {
        let r = compile_source(source, path, &self.config, &mut map);
        Self::result(r, map)
    }

    fn result(
        r: Result<rokc_driver::CompiledProject, Vec<RokDiagnostic>>,
        source_map: SourceMap,
    ) -> CompileResult {
        match r {
            Ok(p) => CompileResult {
                modules: p.modules,
                errors: Vec::new(),
                warnings: p.warnings,
                tests: p.tests,
                sources: p.sources,
                library: p.library,
                files: p.files,
                source_map,
                reuse: p.reuse,
            },
            Err(errors) => CompileResult {
                modules: Vec::new(),
                errors,
                warnings: Vec::new(),
                tests: Vec::new(),
                sources: Vec::new(),
                library: None,
                files: Vec::new(),
                source_map,
                reuse: None,
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CompileResult {
    pub modules: Vec<CompiledModule>,
    pub errors: Vec<RokDiagnostic>,
    pub warnings: Vec<RokDiagnostic>,
    /// Test names of a `set_test` build (bench names of `set_bench`).
    pub tests: Vec<String>,
    /// Parsed files of a `set_keep_ast` build (user package first).
    pub sources: Vec<SourceFile>,
    /// The library's items when a library stage stood for its files
    /// (`set_library_items`).
    pub library: Option<&'static rokc_ast::Program>,
    /// The source files read from disk.
    pub files: Vec<PathBuf>,
    /// The compile's source files by span id: what its diagnostics and
    /// codegen's locations name.
    pub source_map: SourceMap,
    /// What was taken from the incremental directory.
    pub reuse: Option<rokc_driver::Reuse>,
}

impl CompileResult {
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn report_errors(&self) {
        for d in &self.errors {
            self.source_map.report(d);
        }
    }

    /// Print a diagnostic of this compile (`SourceMap::report`).
    pub fn report(&self, d: &RokDiagnostic) {
        self.source_map.report(d);
    }

    /// A diagnostic of this compile as `report` prints it.
    #[must_use]
    pub fn render(&self, d: &RokDiagnostic) -> String {
        self.source_map.render(d)
    }

    fn module(&self) -> Result<&CompiledModule, RokDiagnostic> {
        self.modules.first().ok_or_else(|| {
            RokDiagnostic::error(
                ErrorKind::Internal,
                rokc_diagnostic::Span::DUMMY,
                "no module produced",
            )
        })
    }

    /// One intermediate artifact (`--emit=<kind>`).
    pub fn emit(&self, kind: EmitKind, opts: &BuildOptions) -> Result<Vec<u8>, RokDiagnostic> {
        if kind == EmitKind::Expanded {
            // The user package's files (a `set_keep_ast` build).
            let text: Vec<&str> = self
                .sources
                .iter()
                .filter(|s| s.package.is_empty())
                .flat_map(|s| &s.expanded)
                .map(String::as_str)
                .collect();
            return Ok(text.join("\n").into_bytes());
        }
        if kind == EmitKind::Ast && !self.sources.is_empty() {
            return Ok(self.ast_source().into_bytes());
        }
        rokc_driver::emit(self.module()?, &self.source_map, kind, opts)
    }

    /// `-vv` (909.5): the source files read, then the generic instances
    /// made, one progress line each.
    #[must_use]
    pub fn trace(&self) -> String {
        let files = self
            .source_map
            .files()
            .iter()
            .map(|(p, _)| ("Reading", p.clone()));
        let instances = self
            .modules
            .iter()
            .flat_map(|m| &m.hir.functions)
            .filter_map(rokc_driver::instance)
            .map(|sig| ("Specialized", sig));
        let depth = rokc_core::term::depth(rokc_core::term::Stream::Stderr);
        let mut out = String::new();
        for (verb, what) in files.chain(instances) {
            out.push_str(&rokc_core::term::status_in(depth, verb, &what));
            out.push('\n');
        }
        out
    }

    /// Codegen for its errors alone (`check`), without the toolchain.
    pub fn check_codegen(&self, opts: &BuildOptions) -> Result<(), RokDiagnostic> {
        rokc_driver::check_codegen(self.module()?, &self.source_map, opts)
    }

    /// The package's files after meta expansion as source, each under a
    /// `// <file> (<module>)` line (`--emit=ast`; needs `set_keep_ast`).
    #[must_use]
    pub fn ast_source(&self) -> String {
        let mut out = String::new();
        for f in self.sources.iter().filter(|f| f.package.is_empty()) {
            let module = if f.module.is_empty() {
                "pkg".to_owned()
            } else {
                f.module.join("::")
            };
            out.push_str(&format!("// {} ({module})\n", f.path.display()));
            out.push_str(&rokc_ast::print::print_file(&f.attrs, &f.items, &f.stmts));
            out.push('\n');
        }
        out
    }

    /// Lower MIR again after editing `modules[0].hir`.
    pub fn relower(&mut self) {
        if let Some(m) = self.modules.first_mut() {
            rokc_driver::relower(m);
        }
    }

    /// Build an executable at `out`; by codegen units with
    /// `opts.incremental`, and then how many it compiled.
    pub fn link(
        &self,
        opts: &BuildOptions,
        out: &Path,
    ) -> Result<Option<UnitCounts>, RokDiagnostic> {
        rokc_driver::link_executable(self.module()?, &self.source_map, opts, out)
    }

    /// A REPL cell as an object for the host, from the native backend or
    /// LLVM, with what codegen says about it (GC tables, new symbols).
    pub fn cell_code(
        &self,
        opts: &BuildOptions,
    ) -> Result<(Vec<u8>, rokc_driver::MlirOutput), RokDiagnostic> {
        if rokc_driver::native::jit(opts) {
            rokc_driver::native::cell_object(self.module()?, &self.source_map, opts)
        } else {
            rokc_driver::cell_object(self.module()?, &self.source_map, opts)
        }
    }

    /// Build a library archive at `out` (spec 902.13); with
    /// `opts.incremental` by codegen units, returning how many compiled.
    pub fn archive(
        &self,
        opts: &BuildOptions,
        out: &Path,
    ) -> Result<Option<UnitCounts>, RokDiagnostic> {
        rokc_driver::build_archive(self.module()?, &self.source_map, opts, out)
    }
}

/// The standard library and runtime of this `rokc` (spec 9.2.19).
///
/// Under its sysroot (`lib/rok/{core,std,bare}/src/`, `lib/librokr.a`,
/// `lib\rokr.lib`), else in a repository build the `library/` and the
/// runtime beside `rokc`. `ROK_HOME` plays no part; each `Err` lists where
/// it looked.
pub struct Sysroot {
    /// The library roots to preload (`core`, then `std` / `bare`).
    pub libs: Result<Vec<PathBuf>, String>,
    pub rokr: Result<PathBuf, String>,
    /// `lib/rok/targets/<target>/`: what `--target` links (its runtime;
    /// Linux: C runtime objects and libc). In the repository:
    /// `target/lib/rok/targets/` (`targets/build.sh`).
    pub targets: PathBuf,
    /// `lib/rok/lldb/rok_lldb.py`: lldb formatters for `-g` programs.
    pub lldb: Result<PathBuf, String>,
}

/// The sysroot's library directory.
pub const SYSROOT_LIBS: &str = "lib/rok";
/// The repository's `library/` (a dev build).
const DEV_LIBS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../library");

#[must_use]
pub fn sysroot() -> Sysroot {
    let exe = rokc_core::current_exe().unwrap_or_default();
    let bin = exe.parent().unwrap_or_else(|| Path::new("."));
    let root = bin.parent().unwrap_or(bin);
    let dev = Path::new(DEV_LIBS);
    let dev = rokc_core::real_path(dev).unwrap_or_else(|_| dev.to_path_buf());
    let lib_dirs = [root.join(SYSROOT_LIBS), dev];
    let libs = lib_dirs
        .iter()
        .find(|d| d.join("core/src/lib.rok").is_file())
        .map(|d| bootstrap_roots(d))
        .ok_or_else(|| {
            format!(
                "the standard library is missing: no {} (the sysroot of {}) nor {} (a build in \
                 the repository); reinstall the toolchain, or pass --bootstrap <core/src/lib.rok>",
                lib_dirs[0].join("core/src/lib.rok").display(),
                exe.display(),
                lib_dirs[1].join("core/src/lib.rok").display(),
            )
        });
    let lldb = lib_dirs
        .iter()
        .map(|d| d.join("lldb/rok_lldb.py"))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            format!(
                "the lldb formatters are missing: no {} nor {}",
                lib_dirs[0].join("lldb/rok_lldb.py").display(),
                lib_dirs[1].join("lldb/rok_lldb.py").display(),
            )
        });
    let rt_lib = TargetSpec::host().runtime_lib();
    let rt = [root.join("lib").join(&rt_lib), bin.join(&rt_lib)];
    let rokr = rt.iter().find(|p| p.is_file()).cloned().ok_or_else(|| {
        format!(
            "the runtime library is missing: no {} (the sysroot of {}) nor {} (a build in the \
             repository: `cargo build -p rokr`); reinstall the toolchain, or pass --rokr <lib>",
            rt[0].display(),
            exe.display(),
            rt[1].display(),
        )
    });
    Sysroot {
        libs,
        rokr,
        targets: root.join("lib/rok/targets"),
        lldb,
    }
}

/// Library roots to preload: `core/src/lib.rok`, then `std` / `bare` if
/// present (no implicit glob: user code names them).
fn bootstrap_roots(libs: &Path) -> Vec<PathBuf> {
    let mut out = vec![libs.join("core/src/lib.rok")];
    out.extend(
        ["std", "bare"]
            .iter()
            .map(|tier| libs.join(tier).join("src/lib.rok"))
            .filter(|p| p.is_file()),
    );
    out
}
