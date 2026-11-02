//! In-process backend: MLIR → LLVM IR → object → executable with the
//! statically linked LLVM, MLIR and lld; no subprocesses or textual
//! hand-offs. Without an LLVM prefix [`AVAILABLE`] is false and every entry
//! point fails.

use std::ffi::{CString, OsStr, OsString};
use std::path::Path;

/// The LLVM this crate links (`llvm/build.sh`).
pub const LLVM_VERSION: &str = "22.1.8";

/// Whether this build links LLVM.
pub const AVAILABLE: bool = cfg!(rok_llvm);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    Os,
    Oz,
}

impl OptLevel {
    /// `0`..`3`, `s`, `z` (rokc's `-O<level>`).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "0" => Self::O0,
            "1" => Self::O1,
            "2" => Self::O2,
            "3" => Self::O3,
            "s" => Self::Os,
            "z" => Self::Oz,
            _ => return None,
        })
    }

    #[cfg_attr(not(rok_llvm), allow(dead_code))]
    const fn code(self) -> i32 {
        match self {
            Self::O0 => 0,
            Self::O1 => 1,
            Self::O2 => 2,
            Self::O3 => 3,
            Self::Os => b's' as i32,
            Self::Oz => b'z' as i32,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Emit {
    /// LLVM IR text as translated from MLIR, before any LLVM pass.
    LlvmIr,
    Asm,
    Obj,
}

pub use rokc_target::TargetSpec as Target;
use rokc_target::{Linker, ObjectFormat};

/// What to compile and how.
#[derive(Clone, Debug)]
pub struct Compile<'a> {
    pub target: &'a Target,
    pub opt: OptLevel,
    pub emit: Emit,
    /// `rok_tls` uses the initial-exec TLS model;
    /// off for JIT'd code.
    pub tls_initial_exec: bool,
    /// The MLIR pass pipeline ([`mlir_pipeline`]).
    pub mlir_pipeline: &'a str,
    pub profile: Profile<'a>,
}

/// IR profile-guided optimization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Profile<'a> {
    None,
    /// Instrument: the program writes its raw profile to this file name
    /// pattern (`%m`, `%p`; `LLVM_PROFILE_FILE` overrides it at run time).
    Generate(&'a Path),
    /// Optimize with this indexed profile.
    Use(&'a Path),
    /// Context-sensitive: optimize with this indexed profile, then
    /// instrument again after inlining, writing to this pattern.
    CsGenerate(&'a Path, &'a Path),
}

/// MLIR passes run before translation to LLVM IR.
///
/// The cleanup when optimizing, then conversion to the LLVM dialect.
/// Op-agnostic passes run nested (`any(..)`) per function and global, in
/// parallel.
#[must_use]
pub fn mlir_pipeline(optimize: bool) -> String {
    [
        if optimize {
            "any(canonicalize,cse,convert-scf-to-cf)"
        } else {
            "any(convert-scf-to-cf)"
        },
        // `math` ops: LLVM intrinsics where there is one, libm calls
        // (declared as `func`s) for the rest.
        "any(convert-math-to-llvm)",
        "convert-math-to-libm",
        "convert-func-to-llvm",
        "any(convert-index-to-llvm)",
        "any(convert-arith-to-llvm)",
        "convert-cf-to-llvm",
        "any(reconcile-unrealized-casts)",
    ]
    .join(",")
}

/// MLIR text in the standard dialects → `c.emit`.
pub fn compile_mlir(src: &str, c: &Compile<'_>) -> Result<Vec<u8>, String> {
    ffi::compile(true, src, c, 1).map(|(mut v, _)| v.remove(0))
}

/// MLIR text → `c.emit`; an object split into `parts` objects compiled
/// in parallel (some may be empty). Also what LLVM warned about.
pub fn compile_mlir_split(
    src: &str,
    c: &Compile<'_>,
    parts: usize,
) -> Result<(Vec<Vec<u8>>, String), String> {
    ffi::compile(true, src, c, parts.max(1))
}

/// One codegen unit of [`compile_units`].
pub enum UnitInput<'a> {
    /// MLIR text to compile.
    Mlir(&'a str),
    /// Its `ThinLTO` bitcode from an earlier build
    /// (`UnitsOutput::bitcode`).
    Bitcode(&'a [u8]),
}

/// What [`compile_units`] made.
pub struct UnitsOutput {
    /// An object per unit.
    pub objects: Vec<Vec<u8>>,
    /// The `ThinLTO` bitcode of each unit given as MLIR.
    pub bitcode: Vec<Option<Vec<u8>>>,
    /// How many objects the cache had.
    pub cached: usize,
    /// What LLVM warned about.
    pub warnings: String,
}

/// A package's codegen units optimized at `c.opt` (above 0) to an object
/// each.
///
/// In-process `ThinLTO`: pre-link pipeline per unit (`(name, input)`, names
/// stable across builds), then the thin link, reusing unchanged units'
/// objects from `cache`. `alloc` (`rok.alloc`) is linked into each unit at
/// the end. A program's `_Y` symbols may be internalized; a `library`'s
/// only those users can't name (`.copy_Y`, `_gctype.`).
pub fn compile_units(
    units: &[(&str, UnitInput<'_>)],
    alloc: &str,
    c: &Compile<'_>,
    cache: &Path,
    library: bool,
) -> Result<UnitsOutput, String> {
    ffi::compile_units(units, alloc, c, cache, library)
}

/// LLVM IR text → `c.emit` (`LlvmIr` only applies the TLS model).
pub fn compile_llvm_ir(src: &str, c: &Compile<'_>) -> Result<Vec<u8>, String> {
    ffi::compile(false, src, c, 1).map(|(mut v, _)| v.remove(0))
}

/// Merge raw, indexed or text IR profiles into indexed `out`, as
/// `llvm-profdata merge`; returns its warnings, one per line.
pub fn profdata_merge(inputs: &[&Path], out: &Path) -> Result<String, String> {
    let inputs = inputs
        .iter()
        .map(|p| cstr(p.as_os_str()))
        .collect::<Result<Vec<_>, _>>()?;
    ffi::profdata_merge(&inputs, &cstr(out.as_os_str())?)
}

/// `llvm-profdata show` of the IR profile `file`: a summary; with `all`,
/// every function; with `topn`, the hottest ones.
pub fn profdata_show(file: &Path, all: bool, topn: usize) -> Result<String, String> {
    ffi::profdata_show(&cstr(file.as_os_str())?, all, topn)
}

/// The data layout LLVM gives `t` (what clang writes as `target
/// datalayout`), to check a spec's against.
pub fn data_layout(t: &Target) -> Result<String, String> {
    ffi::data_layout(t)
}

/// The build machine's CPU and features (`+f,-g,..`), as LLVM detects
/// them: `-C target-cpu=native`.
pub fn host_cpu() -> Result<(String, String), String> {
    ffi::host_cpu()
}

/// Runs lld; `args[0]` names the flavor (`ld64.lld`, `ld.lld`, `lld-link`,
/// `wasm-ld`). Returns what it printed; an error carries it.
pub fn lld<S: AsRef<OsStr>>(args: &[S]) -> Result<String, String> {
    let args = args
        .iter()
        .map(|a| cstr(a.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    ffi::lld(&args)
}

/// Link executable `out` with lld against the platform C library.
///
/// `inputs`: objects, archives, `-l`, `-L` and flavor options; `strip` drops
/// unreferenced code. Mach-O: the SDK from `SDKROOT`, else the developer
/// directory. A shipped `libc` brings the toolchain's C runtime and library
/// (`targets/build.sh`). COFF: no default libraries; `inputs` bring the
/// CRT (via [`import_library`]) and entry point.
pub fn link_executable<S: AsRef<OsStr>>(
    t: &Target,
    inputs: &[S],
    out: &Path,
    strip: bool,
    libc: Option<&Path>,
) -> Result<String, String> {
    // `-L<dir>` inputs, then `LIBRARY_PATH` directories, as by cc.
    let search = std::env::var_os("LIBRARY_PATH").unwrap_or_default();
    let search = std::env::split_paths(&search)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| {
            let mut l = OsString::from("-L");
            l.push(d);
            l
        });
    let (dirs, rest): (Vec<OsString>, Vec<OsString>) = inputs
        .iter()
        .map(|a| a.as_ref().to_owned())
        .partition(|a| a.to_str().is_some_and(|s| s.starts_with("-L")));
    let inputs: Vec<OsString> = dirs.into_iter().chain(search).chain(rest).collect();
    let libc = if t.libc.shipped() {
        let dir = libc.ok_or_else(|| format!("no C library for {}", t.name))?;
        if t.crt_objects.iter().any(|o| !dir.join(o).is_file()) {
            return Err(format!("no C library for {} in {}", t.name, dir.display()));
        }
        Some(dir)
    } else {
        None
    };
    // A file of the target's directory, or an argument.
    let late = |a: &String| -> OsString {
        match libc {
            Some(dir) if !a.starts_with('-') => dir.join(a).into_os_string(),
            _ => a.into(),
        }
    };
    let mut a: Vec<OsString> = vec![t.linker.flavor().into()];
    match t.object_format {
        ObjectFormat::MachO => a.extend(macho_args(t)?),
        ObjectFormat::Elf => {
            // Debug info of `--gc-sections`-dropped code points at the DWARF
            // tombstone (all ones), not 0, the start of the address space.
            a.extend(t.pre_link_args.iter().map(OsString::from));
            a.extend(
                [
                    "--eh-frame-hdr",
                    "--build-id",
                    "-z",
                    "dead-reloc-in-nonalloc=.debug_info=0xffffffffffffffff",
                ]
                .map(OsString::from),
            );
            // A dynamic executable is a PIE run by the system's loader.
            if t.dynamic_linking {
                a.extend(["-pie", "--hash-style=gnu", "-dynamic-linker"].map(OsString::from));
                a.push((&t.dynamic_loader).into());
            } else {
                a.push("-static".into());
            }
        }
        ObjectFormat::Coff => {
            a.extend(["/nologo", "/nodefaultlib", "/dynamicbase", "/nxcompat"].map(OsString::from));
            a.extend(t.pre_link_args.iter().map(OsString::from));
            let mut o = OsString::from("/out:");
            o.push(out);
            a.push(o);
            // `-L<dir>` (LIBRARY_PATH) in lld-link's spelling.
            a.extend(inputs.into_iter().map(
                |i| match i.to_str().and_then(|s| s.strip_prefix("-L")) {
                    Some(d) => format!("/libpath:{d}").into(),
                    None => i,
                },
            ));
            a.push(if strip { "/opt:ref" } else { "/opt:noref" }.into());
            a.extend(t.late_link_args.iter().map(late));
            return lld(&a);
        }
        // Stack first: an overflow runs past address 0 and wraps to the top
        // of memory, which is never grown into, so the store traps.
        // wasm-ld drops unreferenced sections by default.
        ObjectFormat::Wasm => {
            a.extend(["--stack-first", "-z"].map(OsString::from));
            a.push(format!("stack-size={}", t.stack_size).into());
            a.push("--max-memory=4294901760".into());
            a.extend(t.pre_link_args.iter().map(OsString::from));
        }
    }
    a.extend(["-o".into(), out.as_os_str().to_owned()]);
    if let Some(dir) = libc {
        a.extend(t.crt_objects.iter().map(|o| dir.join(o).into_os_string()));
        let mut l = OsString::from("-L");
        l.push(dir);
        a.push(l);
    }
    a.extend(inputs);
    if strip {
        match t.object_format {
            ObjectFormat::MachO => a.push("-dead_strip".into()),
            ObjectFormat::Elf => a.push("--gc-sections".into()),
            _ => {}
        }
    }
    a.extend(t.late_link_args.iter().map(late));
    match (t.linker, libc) {
        (Linker::WasmComponentLd, Some(dir)) => link_component(&a, dir),
        _ => lld(&a),
    }
}

/// ld64's arguments before the output: the target's, the platform
/// version (`deployment_target`) and the SDK.
fn macho_args(t: &Target) -> Result<Vec<OsString>, String> {
    let sdk = apple_sdk()?;
    let min = &t.deployment_target;
    let sdk_version = sdk_version(&sdk).unwrap_or_else(|| min.clone());
    let mut a: Vec<OsString> = t.pre_link_args.iter().map(OsString::from).collect();
    a.extend(["-platform_version", &t.os, min, &sdk_version].map(OsString::from));
    a.extend(["-syslibroot".into(), sdk.into_os_string()]);
    // ld64 also searches the real /usr/local/lib.
    if Path::new("/usr/local/lib").is_dir() {
        a.push("-L/usr/local/lib".into());
    }
    Ok(a)
}

/// Set in the environment of `wasm-component-ld`, which runs rokc as its
/// `wasm-ld`: rokc then only links ([`wasm_ld_main`]).
pub const AS_WASM_LD: &str = "ROK_AS_WASM_LD";

/// rokc run as `wasm-ld` ([`AS_WASM_LD`]): lld's wasm flavor over the
/// arguments. The exit code.
#[must_use]
pub fn wasm_ld_main() -> i32 {
    let mut args: Vec<OsString> = vec!["wasm-ld".into()];
    args.extend(std::env::args_os().skip(1));
    match lld(&args) {
        Ok(text) => {
            eprint!("{text}");
            0
        }
        Err(text) => {
            eprint!("{text}");
            1
        }
    }
}

/// `wasm32-wasip2` is a preview 2 component: `wasm-component-ld` (shipped
/// with wasi-libc) links with rokc as its `wasm-ld`, then wraps the module
/// with its embedded preview 1 adapter. Returns what it printed; errors
/// carry it.
fn link_component(args: &[OsString], libc: &Path) -> Result<String, String> {
    let rokc = std::env::current_exe().map_err(|e| format!("cannot find rokc: {e}"))?;
    let tool = libc.join(format!("wasm-component-ld{}", std::env::consts::EXE_SUFFIX));
    let run = std::process::Command::new(&tool)
        .arg("--wasm-ld-path")
        .arg(&rokc)
        .args(&args[1..])
        .env(AS_WASM_LD, "1")
        .output()
        .map_err(|e| format!("{}: {e}", tool.display()))?;
    let text =
        String::from_utf8_lossy(&run.stderr).into_owned() + &String::from_utf8_lossy(&run.stdout);
    if run.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// `dsymutil <args>`: the DWARF of a Mach-O executable's objects into a
/// `.dSYM` bundle. It reports on stderr.
pub fn dsymutil<S: AsRef<OsStr>>(args: &[S]) -> Result<(), String> {
    let mut v = vec![cstr(OsStr::new("dsymutil"))?];
    for a in args {
        v.push(cstr(a.as_ref())?);
    }
    ffi::dsymutil(&v)
}

/// An archive of `members` with a symbol table: BSD format for Apple
/// targets, COFF for Windows, else GNU.
pub fn write_archive(t: &Target, out: &Path, members: &[&Path]) -> Result<(), String> {
    let members = members
        .iter()
        .map(|m| cstr(m.as_os_str()))
        .collect::<Result<Vec<_>, _>>()?;
    let kind = match t.object_format {
        ObjectFormat::MachO => 1,
        ObjectFormat::Coff => 2,
        _ => 0,
    };
    ffi::write_archive(&cstr(out.as_os_str())?, &members, kind)
}

/// The import library `out` of the module definition `def` (a `.def`
/// file's text, mingw-w64 syntax) for a Windows target.
pub fn import_library(t: &Target, def: &str, out: &Path) -> Result<(), String> {
    let machine = t
        .coff_machine()
        .ok_or_else(|| format!("{} has no COFF machine type", t.name))?;
    ffi::import_library(def, machine, &cstr(out.as_os_str())?)
}

/// The macOS SDK as `xcrun` would find it, without running it: `SDKROOT`,
/// else `DEVELOPER_DIR`'s, else `xcode-select`'s, else the CLT.
pub fn apple_sdk() -> Result<std::path::PathBuf, String> {
    if let Some(p) = std::env::var_os("SDKROOT").filter(|p| !p.is_empty()) {
        return Ok(p.into());
    }
    let dev = std::env::var_os("DEVELOPER_DIR")
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::fs::read_link("/var/db/xcode_select_link").ok())
        .unwrap_or_else(|| "/Library/Developer/CommandLineTools".into());
    // Xcode keeps it in the platform, the command line tools at the top.
    [
        "Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk",
        "SDKs/MacOSX.sdk",
    ]
    .iter()
    .map(|s| dev.join(s))
    .find(|s| s.is_dir())
    .ok_or_else(|| {
        format!(
            "no macOS SDK in {} (install the Xcode command line tools, or set SDKROOT)",
            dev.display()
        )
    })
}

/// `"Version":"26.0"` of the SDK's `SDKSettings.json`.
fn sdk_version(sdk: &Path) -> Option<String> {
    let json = std::fs::read_to_string(sdk.join("SDKSettings.json")).ok()?;
    let rest = &json[json.find("\"Version\"")? + 9..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_owned())
}

fn cstr(s: &OsStr) -> Result<CString, String> {
    CString::new(s.as_encoded_bytes())
        .map_err(|_| format!("{} contains NUL", Path::new(s).display()))
}

#[cfg(rok_llvm)]
mod ffi {
    use std::ffi::{CString, c_char, c_int};
    use std::path::Path;

    use super::{Compile, Emit, Profile, Target, UnitInput, UnitsOutput};

    #[repr(C)]
    struct RokBuf {
        ptr: *mut c_char,
        len: usize,
    }

    impl RokBuf {
        const fn new() -> Self {
            Self {
                ptr: std::ptr::null_mut(),
                len: 0,
            }
        }

        fn take(mut self) -> Vec<u8> {
            let v = if self.ptr.is_null() {
                Vec::new()
            } else {
                // SAFETY: the shim wrote `len` bytes at `ptr`.
                unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }.to_vec()
            };
            // SAFETY: a buffer of the shim.
            unsafe { rok_llvm_free(&raw mut self) };
            v
        }

        fn text(self) -> String {
            String::from_utf8_lossy(&self.take()).into_owned()
        }
    }

    #[repr(C)]
    struct RokCompile {
        triple: *const c_char,
        cpu: *const c_char,
        features: *const c_char,
        opt: c_int,
        emit: c_int,
        tls_initial_exec: c_int,
        mlir_pipeline: *const c_char,
        parts: usize,
        profile_generate: *const c_char,
        profile_use: *const c_char,
    }

    unsafe extern "C" {
        fn rok_llvm_free(b: *mut RokBuf);
        fn rok_llvm_compile_mlir(
            src: *const c_char,
            len: usize,
            o: *const RokCompile,
            out: *mut RokBuf,
            err: *mut RokBuf,
            warn: *mut RokBuf,
        ) -> c_int;
        fn rok_llvm_compile_ir(
            src: *const c_char,
            len: usize,
            o: *const RokCompile,
            out: *mut RokBuf,
            err: *mut RokBuf,
            warn: *mut RokBuf,
        ) -> c_int;
        fn rok_llvm_compile_units(
            units: *const RokUnit,
            n: usize,
            alloc: *const c_char,
            alloc_len: usize,
            o: *const RokCompile,
            cache: *const c_char,
            library: c_int,
            objects: *mut RokBuf,
            bitcode: *mut RokBuf,
            hits: *mut usize,
            err: *mut RokBuf,
            warn: *mut RokBuf,
        ) -> c_int;
        fn rok_llvm_data_layout(o: *const RokCompile, out: *mut RokBuf, err: *mut RokBuf) -> c_int;
        fn rok_llvm_host_cpu(cpu: *mut RokBuf, features: *mut RokBuf) -> c_int;
        fn rok_llvm_dsymutil(argv: *const *const c_char, argc: usize) -> c_int;
        fn rok_llvm_lld(argv: *const *const c_char, argc: usize, log: *mut RokBuf) -> c_int;
        fn rok_llvm_write_archive(
            path: *const c_char,
            members: *const *const c_char,
            n: usize,
            kind: c_int,
            err: *mut RokBuf,
        ) -> c_int;
        fn rok_llvm_profdata_merge(
            inputs: *const *const c_char,
            n: usize,
            path: *const c_char,
            warn: *mut RokBuf,
            err: *mut RokBuf,
        ) -> c_int;
        fn rok_llvm_profdata_show(
            path: *const c_char,
            all: c_int,
            topn: usize,
            out: *mut RokBuf,
            err: *mut RokBuf,
        ) -> c_int;
        fn rok_llvm_import_lib(
            def: *const c_char,
            len: usize,
            machine: u16,
            path: *const c_char,
            err: *mut RokBuf,
        ) -> c_int;
    }

    fn c(s: &str) -> Result<CString, String> {
        CString::new(s).map_err(|_| format!("`{s}` contains NUL"))
    }

    /// `f` of the shim's form of `o`.
    fn with_opts<R>(
        o: &Compile<'_>,
        parts: usize,
        f: impl FnOnce(&RokCompile) -> R,
    ) -> Result<R, String> {
        let (triple, cpu, features) = (
            c(&o.target.llvm_triple())?,
            c(&o.target.cpu)?,
            c(&o.target.features)?,
        );
        let pipeline = c(o.mlir_pipeline)?;
        let (generate, used) = match o.profile {
            Profile::None => (None, None),
            Profile::Generate(p) => (Some(p), None),
            Profile::Use(p) => (None, Some(p)),
            Profile::CsGenerate(u, g) => (Some(g), Some(u)),
        };
        let path = |p: Option<&Path>| p.map_or_else(|| c(""), |p| super::cstr(p.as_os_str()));
        let (generate, used) = (path(generate)?, path(used)?);
        let opts = RokCompile {
            triple: triple.as_ptr(),
            cpu: cpu.as_ptr(),
            features: features.as_ptr(),
            opt: o.opt.code(),
            emit: match o.emit {
                Emit::LlvmIr => 0,
                Emit::Asm => 1,
                Emit::Obj => 2,
            },
            tls_initial_exec: c_int::from(o.tls_initial_exec),
            mlir_pipeline: pipeline.as_ptr(),
            parts,
            profile_generate: generate.as_ptr(),
            profile_use: used.as_ptr(),
        };
        Ok(f(&opts))
    }

    pub fn compile(
        mlir: bool,
        src: &str,
        o: &Compile<'_>,
        parts: usize,
    ) -> Result<(Vec<Vec<u8>>, String), String> {
        let mut out: Vec<RokBuf> = (0..parts).map(|_| RokBuf::new()).collect();
        let (mut err, mut warn) = (RokBuf::new(), RokBuf::new());
        let f = if mlir {
            rok_llvm_compile_mlir
        } else {
            rok_llvm_compile_ir
        };
        // SAFETY: every pointer outlives the call; the shim fills the buffers.
        let rc = with_opts(o, parts, |opts| unsafe {
            f(
                src.as_ptr().cast(),
                src.len(),
                opts,
                out.as_mut_ptr(),
                &raw mut err,
                &raw mut warn,
            )
        })?;
        let out: Vec<Vec<u8>> = out.into_iter().map(RokBuf::take).collect();
        let warn = warn.text();
        if rc == 0 {
            drop(err.take());
            Ok((out, warn))
        } else {
            Err(err.text())
        }
    }

    #[repr(C)]
    struct RokUnit {
        name: *const c_char,
        src: *const c_char,
        len: usize,
        bitcode: c_int,
    }

    pub fn compile_units(
        units: &[(&str, UnitInput<'_>)],
        alloc: &str,
        o: &Compile<'_>,
        cache: &Path,
        library: bool,
    ) -> Result<UnitsOutput, String> {
        let names = units
            .iter()
            .map(|(n, _)| c(n))
            .collect::<Result<Vec<_>, _>>()?;
        let raw: Vec<RokUnit> = units
            .iter()
            .zip(&names)
            .map(|((_, u), name)| {
                let (src, bitcode): (&[u8], bool) = match u {
                    UnitInput::Mlir(t) => (t.as_bytes(), false),
                    UnitInput::Bitcode(b) => (b, true),
                };
                RokUnit {
                    name: name.as_ptr(),
                    src: src.as_ptr().cast(),
                    len: src.len(),
                    bitcode: c_int::from(bitcode),
                }
            })
            .collect();
        let cache = super::cstr(cache.as_os_str())?;
        let n = units.len();
        let mut objects: Vec<RokBuf> = (0..n).map(|_| RokBuf::new()).collect();
        let mut bitcode: Vec<RokBuf> = (0..n).map(|_| RokBuf::new()).collect();
        let (mut err, mut warn, mut hits) = (RokBuf::new(), RokBuf::new(), 0usize);
        // SAFETY: every pointer outlives the call; the shim fills the buffers.
        let rc = with_opts(o, 1, |opts| unsafe {
            rok_llvm_compile_units(
                raw.as_ptr(),
                n,
                alloc.as_ptr().cast(),
                alloc.len(),
                opts,
                cache.as_ptr(),
                c_int::from(library),
                objects.as_mut_ptr(),
                bitcode.as_mut_ptr(),
                &raw mut hits,
                &raw mut err,
                &raw mut warn,
            )
        })?;
        let objects: Vec<Vec<u8>> = objects.into_iter().map(RokBuf::take).collect();
        let bitcode: Vec<Option<Vec<u8>>> = bitcode
            .into_iter()
            .zip(units)
            .map(|(b, (_, u))| {
                let b = b.take();
                matches!(u, UnitInput::Mlir(_)).then_some(b)
            })
            .collect();
        let warnings = warn.text();
        if rc == 0 {
            drop(err.take());
            Ok(UnitsOutput {
                objects,
                bitcode,
                cached: hits,
                warnings,
            })
        } else {
            Err(err.text())
        }
    }

    pub fn host_cpu() -> Result<(String, String), String> {
        let (mut cpu, mut features) = (RokBuf::new(), RokBuf::new());
        // SAFETY: the shim fills both buffers when it returns 0.
        if unsafe { rok_llvm_host_cpu(&raw mut cpu, &raw mut features) } != 0 {
            return Err("LLVM cannot tell this machine's CPU".to_owned());
        }
        Ok((cpu.text(), features.text()))
    }

    pub fn data_layout(t: &Target) -> Result<String, String> {
        let (triple, cpu, features, none) =
            (c(&t.llvm_triple())?, c(&t.cpu)?, c(&t.features)?, c("")?);
        let opts = RokCompile {
            triple: triple.as_ptr(),
            cpu: cpu.as_ptr(),
            features: features.as_ptr(),
            opt: 0,
            emit: 2,
            tls_initial_exec: 0,
            mlir_pipeline: none.as_ptr(),
            parts: 1,
            profile_generate: none.as_ptr(),
            profile_use: none.as_ptr(),
        };
        let (mut out, mut err) = (RokBuf::new(), RokBuf::new());
        // SAFETY: as in `compile`.
        let rc = unsafe { rok_llvm_data_layout(&raw const opts, &raw mut out, &raw mut err) };
        if rc == 0 {
            drop(err.take());
            Ok(out.text())
        } else {
            drop(out.take());
            Err(err.text())
        }
    }

    pub fn dsymutil(args: &[CString]) -> Result<(), String> {
        let ptrs: Vec<*const c_char> = args.iter().map(|s| s.as_ptr()).collect();
        // SAFETY: `ptrs` points into `args`, alive for the call.
        let rc = unsafe { rok_llvm_dsymutil(ptrs.as_ptr(), ptrs.len()) };
        if rc == 0 {
            Ok(())
        } else {
            Err("dsymutil failed".to_owned())
        }
    }

    pub fn lld(args: &[CString]) -> Result<String, String> {
        let ptrs: Vec<*const c_char> = args.iter().map(|s| s.as_ptr()).collect();
        let mut log = RokBuf::new();
        // SAFETY: `ptrs` points into `args`, alive for the call.
        let rc = unsafe { rok_llvm_lld(ptrs.as_ptr(), ptrs.len(), &raw mut log) };
        let log = log.text();
        if rc == 0 { Ok(log) } else { Err(log) }
    }

    pub fn write_archive(out: &CString, members: &[CString], kind: c_int) -> Result<(), String> {
        let ptrs: Vec<*const c_char> = members.iter().map(|s| s.as_ptr()).collect();
        let mut err = RokBuf::new();
        // SAFETY: as in `lld`.
        let rc = unsafe {
            rok_llvm_write_archive(out.as_ptr(), ptrs.as_ptr(), ptrs.len(), kind, &raw mut err)
        };
        if rc == 0 {
            drop(err.take());
            Ok(())
        } else {
            Err(err.text())
        }
    }

    pub fn profdata_merge(inputs: &[CString], out: &CString) -> Result<String, String> {
        let ptrs: Vec<*const c_char> = inputs.iter().map(|s| s.as_ptr()).collect();
        let (mut warn, mut err) = (RokBuf::new(), RokBuf::new());
        // SAFETY: as in `lld`.
        let rc = unsafe {
            rok_llvm_profdata_merge(
                ptrs.as_ptr(),
                ptrs.len(),
                out.as_ptr(),
                &raw mut warn,
                &raw mut err,
            )
        };
        if rc == 0 {
            drop(err.take());
            Ok(warn.text())
        } else {
            drop(warn.take());
            Err(err.text())
        }
    }

    pub fn profdata_show(file: &CString, all: bool, topn: usize) -> Result<String, String> {
        let (mut out, mut err) = (RokBuf::new(), RokBuf::new());
        // SAFETY: as in `lld`.
        let rc = unsafe {
            rok_llvm_profdata_show(
                file.as_ptr(),
                c_int::from(all),
                topn,
                &raw mut out,
                &raw mut err,
            )
        };
        if rc == 0 {
            drop(err.take());
            Ok(out.text())
        } else {
            drop(out.take());
            Err(err.text())
        }
    }

    pub fn import_library(def: &str, machine: u16, out: &CString) -> Result<(), String> {
        let mut err = RokBuf::new();
        // SAFETY: as in `lld`.
        let rc = unsafe {
            rok_llvm_import_lib(
                def.as_ptr().cast(),
                def.len(),
                machine,
                out.as_ptr(),
                &raw mut err,
            )
        };
        if rc == 0 {
            drop(err.take());
            Ok(())
        } else {
            Err(err.text())
        }
    }
}

#[cfg(not(rok_llvm))]
mod ffi {
    use std::ffi::CString;

    use super::{Compile, Target};

    fn off<T>() -> Result<T, String> {
        Err(format!(
            "this rokc was built without LLVM {} (set ROK_LLVM_PREFIX, see llvm/build.sh)",
            super::LLVM_VERSION
        ))
    }

    pub fn compile(
        _: bool,
        _: &str,
        _: &Compile<'_>,
        _: usize,
    ) -> Result<(Vec<Vec<u8>>, String), String> {
        off()
    }

    pub fn compile_units(
        _: &[(&str, super::UnitInput<'_>)],
        _: &str,
        _: &Compile<'_>,
        _: &std::path::Path,
        _: bool,
    ) -> Result<super::UnitsOutput, String> {
        off()
    }

    pub fn profdata_merge(_: &[CString], _: &CString) -> Result<String, String> {
        off()
    }

    pub fn profdata_show(_: &CString, _: bool, _: usize) -> Result<String, String> {
        off()
    }

    pub fn data_layout(_: &Target) -> Result<String, String> {
        off()
    }

    pub fn host_cpu() -> Result<(String, String), String> {
        off()
    }

    pub fn lld(_: &[CString]) -> Result<String, String> {
        off()
    }

    pub fn dsymutil(_: &[CString]) -> Result<(), String> {
        off()
    }

    pub fn write_archive(_: &CString, _: &[CString], _: i32) -> Result<(), String> {
        off()
    }

    pub fn import_library(_: &str, _: u16, _: &CString) -> Result<(), String> {
        off()
    }
}
