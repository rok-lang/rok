//! Links the pinned LLVM / MLIR / lld static libraries and the C++ shim.
//!
//! The prefix is `ROK_LLVM_PREFIX`, else `llvm/build.sh`'s default
//! (`$ROK_LLVM_CACHE/llvm-<version>`, `~/.cache/rok`; `%LOCALAPPDATA%\rok`
//! on Windows). Without one, or with one for another machine than the
//! cargo target, the crate builds its stub (`cfg(rok_llvm)` unset) whose
//! entry points all fail. An unusable `ROK_LLVM_PREFIX` is a build error.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "build/exports.rs"]
mod exports;
use exports::Member;

/// Must match `VERSION` in `llvm/build.sh` and `llvm/build.ps1`.
const LLVM_VERSION: &str = "22.1.8";

fn main() {
    println!("cargo:rustc-check-cfg=cfg(rok_llvm)");
    println!("cargo:rerun-if-changed=src/shim.cpp");
    for v in [
        "ROK_LLVM_PREFIX",
        "ROK_LLVM_CACHE",
        "XDG_CACHE_HOME",
        "LOCALAPPDATA",
        "CXX",
    ] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    // A prefix holds libraries for the machine that built it: a foreign
    // target gets the stub.
    if env::var("TARGET") != env::var("HOST") {
        return;
    }
    let explicit = env::var_os("ROK_LLVM_PREFIX").filter(|p| !p.is_empty());
    let prefix = explicit.clone().map(PathBuf::from).or_else(default_prefix);
    // Rebuilt once the prefix appears: watch its nearest existing ancestor
    // (not `/`: cargo would walk the whole file system).
    if let Some(dir) = prefix
        .as_deref()
        .and_then(|p| p.ancestors().find(|a| a.exists() && a.parent().is_some()))
    {
        println!("cargo:rerun-if-changed={}", dir.display());
    }
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if let Some(p) = prefix
        .as_deref()
        .filter(|p| apple_host() && p.is_dir() && !x1_patched(p))
    {
        println!(
            "cargo:warning={} predates llvm/patches/arm64e-x1.patch; rebuild it with llvm/build.sh",
            p.display()
        );
    }
    let Some(prefix) = prefix.filter(|p| usable(p, msvc)) else {
        assert!(
            explicit.is_none(),
            "ROK_LLVM_PREFIX={} has no LLVM {LLVM_VERSION} with MLIR, lld and \
             llvm/patches (build one with llvm/build.sh or llvm/build.ps1)",
            Path::new(&explicit.unwrap_or_default()).display()
        );
        return;
    };
    // The prefix runs elsewhere: checking for another target.
    if !for_target(&prefix) {
        return;
    }
    println!("cargo:rustc-cfg=rok_llvm");
    println!("cargo:prefix={}", prefix.display());

    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let apple = env::var("CARGO_CFG_TARGET_VENDOR").as_deref() == Ok("apple");
    compile_shim(&prefix, &out, apple, msvc);
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=rokc_llvm_shim");

    // Users before what they use: lld, MLIR, LLVM. Not bundled into the rlib
    // (-bundle): the archives go to the final link as they are.
    let lib = prefix.join("lib");
    println!("cargo:rustc-link-search=native={}", lib.display());
    let mut names: Vec<String> = std::fs::read_dir(&lib)
        .unwrap()
        .filter_map(|e| {
            let n = e.ok()?.file_name().into_string().ok()?;
            let n = if msvc {
                n.strip_suffix(".lib")?
            } else {
                n.strip_prefix("lib")?.strip_suffix(".a")?
            }
            .to_owned();
            // `LLVM-C.lib` (MSVC) is the import library of LLVM-C.dll.
            (["lld", "MLIR", "LLVM"].iter().any(|p| n.starts_with(p)) && n != "LLVM-C").then_some(n)
        })
        .collect();
    names.sort_by_key(|n| {
        (
            ["lld", "MLIR", "LLVM"]
                .iter()
                .position(|p| n.starts_with(p)),
            n.clone(),
        )
    });
    // MLIR plugins bind to rokc's MLIR and LLVM support library (spec bare
    // 1.10.2), so rokc exports those and the rest is dead-stripped; Support
    // and Demangle go in whole. A static (musl) rokc loads no plugins and
    // exports nothing: it has no dynamic loader to bind them.
    let shared = env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("musl");
    if shared {
        let exports = out.join(if msvc { "exports.def" } else { "exports" });
        let shim = out.join(if msvc {
            "rokc_llvm_shim.lib"
        } else {
            "librokc_llvm_shim.a"
        });
        write_exports(&shim, &lib, &names, &exports, msvc);
        println!("cargo:exports={}", exports.display());
        names.retain(|n| !EXPORTED_LIBS.contains(&n.as_str()));
        for n in EXPORTED_LIBS {
            println!("cargo:rustc-link-lib=static:+whole-archive,-bundle={n}");
        }
        let dev = plugin_dev(&prefix, &out);
        println!("cargo:plugin_dev={}", dev.display());
        println!("cargo:rustc-env=ROK_LLVM_PLUGIN_DEV={}", dev.display());
    }
    if apple || msvc {
        for n in &names {
            println!("cargo:rustc-link-lib=static:-bundle={n}");
        }
    } else {
        // GNU ld reads each archive once, in order, and these reference each
        // other cyclically: a linker script groups them.
        let group: Vec<String> = names
            .iter()
            .map(|n| format!("\"{}\"", lib.join(format!("lib{n}.a")).display()))
            .collect();
        std::fs::write(
            out.join("librokc_llvm_libs.a"),
            format!("GROUP ( {} )\n", group.join(" ")),
        )
        .unwrap();
        println!("cargo:rustc-link-lib=static:-bundle=rokc_llvm_libs");
    }
    let system: &[&str] = if apple {
        // dsymutil writes the dSYM bundle's Info.plist with it.
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        &["c++"]
    } else if msvc {
        // LLVMSupport's `LLVM_SYSTEM_LIBS`; the C++ runtime is the CRT's
        // (/MD, as rustc links).
        &[
            "psapi", "shell32", "ole32", "uuid", "advapi32", "ws2_32", "ntdll",
        ]
    } else if libcxx(&lib) {
        &["c++", "m", "dl", "pthread", "rt"]
    } else {
        &["stdc++", "m", "dl", "pthread", "rt"]
    };
    // Static, the C++ runtime too (libstdc++.a; musl's libm.a, .. are
    // its libc.a's empty companions).
    let kind = if shared { "dylib" } else { "static:-bundle" };
    for l in system {
        println!("cargo:rustc-link-lib={kind}={l}");
    }
}

/// The LLVM libraries rokc exports whole besides MLIR.
const EXPORTED_LIBS: [&str; 2] = ["LLVMSupport", "LLVMDemangle"];

/// The members of `archive`: read as COFF (MSVC), else `nm -P -g` plus
/// `flags` (ELF's weak definitions are `W` / `V`).
fn members(archive: &Path, flags: &[&str], msvc: bool) -> Vec<Member> {
    if msvc {
        let bytes = std::fs::read(archive)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", archive.display()));
        return exports::coff_members(&bytes);
    }
    let out = Command::new("nm")
        .args(["-P", "-g"])
        .args(flags)
        .arg(archive)
        .output()
        .unwrap_or_else(|e| panic!("cannot run nm: {e}"));
    assert!(out.status.success(), "nm failed on {}", archive.display());
    let mut v: Vec<Member> = Vec::new();
    // `name type [value size]`; a member starts with `<name>:`.
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        let mut f = l.split_whitespace();
        let Some(n) = f.next() else { continue };
        let ty = f.next();
        if ty.is_none() && n.ends_with(':') {
            v.push(Member::default());
            continue;
        }
        let (Some(ty), Some(m)) = (ty, v.last_mut()) else {
            continue;
        };
        let n = n.to_owned();
        match ty {
            "U" => m.undefs.push(n),
            "W" => m.weak.push(n),
            "V" => {
                m.data.push(n.clone());
                m.weak.push(n);
            }
            "w" | "v" => {}
            _ => m.defs.push(n),
        }
    }
    v
}

/// The non-weak, non-hidden global definitions of Mach-O `archive`.
fn macho_exportable(archive: &Path) -> std::collections::HashSet<String> {
    let out = Command::new("nm")
        .args(["-m", "--defined-only"])
        .arg(archive)
        .output()
        .unwrap_or_else(|e| panic!("cannot run nm: {e}"));
    assert!(out.status.success(), "nm failed on {}", archive.display());
    // `<value> (<segment>,<section>) [weak] [private] external [..] <name>`
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (attrs, name) = l.rsplit_once(' ')?;
            let a: Vec<&str> = attrs.split_whitespace().collect();
            (a.contains(&"external") && !a.contains(&"weak") && !a.contains(&"private"))
                .then(|| name.to_owned())
        })
        .collect()
}

/// Symbols rokc exports (`cargo:exports`): the non-weak globals of every
/// MLIR member the link loads (found as the linker would) and of all of
/// [`EXPORTED_LIBS`]. Weak definitions stay internal (a plugin has its own,
/// and dyld would bind each exported one at launch), except MSVC's in the
/// whole libraries and explicit `TypeID`s, and ELF's weak objects: GCC
/// puts every vtable and typeinfo in a COMDAT, and a plugin deriving from
/// `mlir::Pass` must bind to rokc's. Mach-O takes a name list, ELF a
/// dynamic list, PE a module definition file.
fn write_exports(shim: &Path, lib: &Path, names: &[String], to: &Path, msvc: bool) {
    use std::collections::{HashMap, HashSet};
    // (archive, exported, loaded whole)
    let archives: Vec<(PathBuf, bool, bool)> = std::iter::once((shim.to_owned(), false, true))
        .chain(names.iter().map(|n| {
            let whole = EXPORTED_LIBS.contains(&n.as_str());
            (
                archive(lib.parent().unwrap(), n, msvc),
                whole || n.starts_with("MLIR"),
                whole,
            )
        }))
        .collect();
    let all: Vec<_> = archives
        .iter()
        .map(|(a, ..)| members(a, &[], msvc))
        .collect();
    let mut definer: HashMap<&str, (usize, usize)> = HashMap::new();
    for (a, ms) in all.iter().enumerate() {
        for (m, member) in ms.iter().enumerate() {
            for d in member.defs.iter().chain(&member.weak) {
                definer.entry(d).or_insert((a, m));
            }
        }
    }
    let mut work: Vec<(usize, usize)> = (0..all.len())
        .filter(|&a| archives[a].2)
        .flat_map(|a| (0..all[a].len()).map(move |m| (a, m)))
        .collect();
    let mut loaded: HashSet<(usize, usize)> = work.iter().copied().collect();
    while let Some((a, m)) = work.pop() {
        for u in &all[a][m].undefs {
            // A dllimport reference binds the definition too.
            let u = u.strip_prefix("__imp_").unwrap_or(u);
            if let Some(&d) = definer.get(u) {
                if loaded.insert(d) {
                    work.push(d);
                }
            }
        }
    }
    let apple = env::var("CARGO_CFG_TARGET_VENDOR").as_deref() == Ok("apple");
    let mut syms: Vec<(String, bool)> = Vec::new();
    for (a, (path, exported, whole)) in archives.iter().enumerate() {
        if !exported || !(0..all[a].len()).any(|m| loaded.contains(&(a, m))) {
            continue;
        }
        // Mach-O's nm -P types weak and hidden (private extern)
        // definitions as others: -m spells them out.
        let strong: Option<HashSet<String>> = apple.then(|| macho_exportable(path));
        for (m, member) in all[a].iter().enumerate() {
            if !loaded.contains(&(a, m)) {
                continue;
            }
            // MSVC makes explicit template instantiations COMDATs, and
            // clang-cl explicit TypeIDs' `id`s; plugins' `extern template`
            // uses (`SmallVectorBase<uint32_t>::grow_pod`) and TypeIDs
            // (rok/mlir_typeid.h) must bind to rokc's, so those are exported.
            let weak = member.weak.iter().filter(|w| {
                if msvc {
                    *whole || w.starts_with("?id@?$TypeIDResolver@")
                } else {
                    !apple && member.data.contains(*w)
                }
            });
            syms.extend(
                (member.defs.iter().chain(weak))
                    .filter(|d| strong.as_ref().is_none_or(|s| s.contains(*d)))
                    .map(|d| (d.clone(), member.data.contains(d))),
            );
        }
    }
    syms.sort();
    syms.dedup();
    let text = if msvc {
        let x86 = env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86");
        exports::def_file(&syms, x86).unwrap_or_else(|e| panic!("{e}"))
    } else if apple {
        let list: Vec<&str> = syms.iter().map(|(s, _)| s.as_str()).collect();
        list.join("\n") + "\n"
    } else {
        let list: Vec<String> = syms.iter().map(|(s, _)| format!("  {s};")).collect();
        format!("{{\n{}\n}};\n", list.join("\n"))
    };
    std::fs::write(to, text).unwrap();
}

/// The plugin development tree (`target/<profile>/mlir-plugin-dev`,
/// shipped as `rok-mlir-dev`, docs/mlir-plugins.md): the prefix's LLVM and
/// MLIR headers, `mlir/Support/TypeID.h` including `rok/mlir_typeid.h`
/// before its first explicit `TypeID`, plugin entry points
/// `LLVM_ALWAYS_EXPORT`, and on MSVC rokc's import library in `lib/`.
fn plugin_dev(prefix: &Path, out: &Path) -> PathBuf {
    let dev = out.ancestors().nth(3).unwrap().join("mlir-plugin-dev");
    let inc = dev.join("include");
    // The headers are copied once per prefix.
    let stamp = dev.join("prefix");
    let from = prefix.display().to_string();
    if std::fs::read_to_string(&stamp).ok().as_deref() != Some(from.as_str()) {
        // Refresh only the headers; leaving lib/ in place avoids churning
        // rokc's import library while a concurrent rokc link writes it (a
        // Windows LNK1104 on rokc.exp).
        let _ = std::fs::remove_dir_all(&inc);
        for d in ["llvm", "llvm-c", "mlir", "mlir-c"] {
            copy_tree(&prefix.join("include").join(d), &inc.join(d));
        }
        std::fs::write(&stamp, &from).unwrap();
    }
    std::fs::create_dir_all(dev.join("lib")).unwrap();
    let first = "\nMLIR_DECLARE_EXPLICIT_SELF_OWNING_TYPE_ID(void)";
    let typeid = format!("\n#include \"rok/mlir_typeid.h\"{first}");
    hook(prefix, &inc, "mlir/Support/TypeID.h", first, &typeid);
    // The entry points declared exported, as plugins define them (MSVC
    // refuses a `dllexport` definition of a plain declaration).
    for (h, f) in [
        ("DialectPlugin.h", "mlirGetDialectPluginInfo"),
        ("PassPlugin.h", "mlirGetPassPluginInfo"),
    ] {
        let decl = format!("LLVM_ATTRIBUTE_WEAK\n{f}();");
        let exported = format!("LLVM_ATTRIBUTE_WEAK LLVM_ALWAYS_EXPORT\n{f}();");
        hook(
            prefix,
            &inc,
            &format!("mlir/Tools/Plugins/{h}"),
            &decl,
            &exported,
        );
    }
    std::fs::create_dir_all(inc.join("rok")).unwrap();
    std::fs::copy("plugin/mlir_typeid.h", inc.join("rok/mlir_typeid.h")).unwrap();
    println!("cargo:rerun-if-changed=plugin/mlir_typeid.h");
    dev
}

/// The prefix's header `rel` into `inc` with its one `needle` replaced.
fn hook(prefix: &Path, inc: &Path, rel: &str, needle: &str, with: &str) {
    let from = prefix.join("include").join(rel);
    let text = std::fs::read_to_string(&from).unwrap();
    assert!(
        text.matches(needle).count() == 1,
        "{} has no single `{}` to hook",
        from.display(),
        needle.trim()
    );
    std::fs::write(inc.join(rel), text.replacen(needle, with, 1)).unwrap();
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

fn default_prefix() -> Option<PathBuf> {
    let var = |v: &str| env::var_os(v).filter(|p| !p.is_empty()).map(PathBuf::from);
    // The build machine's convention (`build.sh` / `build.ps1`).
    let cache = var("ROK_LLVM_CACHE").or_else(|| {
        if cfg!(windows) {
            Some(var("LOCALAPPDATA")?.join("rok"))
        } else {
            var("XDG_CACHE_HOME")
                .map(|c| c.join("rok"))
                .or_else(|| Some(var("HOME")?.join(".cache/rok")))
        }
    })?;
    Some(cache.join(format!("llvm-{LLVM_VERSION}")))
}

/// The static archive of library `name`.
fn archive(prefix: &Path, name: &str, msvc: bool) -> PathBuf {
    prefix.join("lib").join(if msvc {
        format!("{name}.lib")
    } else {
        format!("lib{name}.a")
    })
}

/// A prefix of the pinned LLVM with MLIR and lld static libraries.
fn usable(prefix: &Path, msvc: bool) -> bool {
    let version = std::fs::read_to_string(prefix.join("include/llvm/Config/llvm-config.h"))
        .unwrap_or_default();
    version.contains(&format!("#define LLVM_VERSION_STRING \"{LLVM_VERSION}"))
        && archive(prefix, "MLIRIR", msvc).is_file()
        && archive(prefix, "lldMachO", msvc).is_file()
        && prefix.join("include/lld/Common/Driver.h").is_file()
        && prefix.join("share/rok/dsymutil/Options.inc").is_file()
        && (!apple_host() || x1_patched(prefix))
}

fn apple_host() -> bool {
    env::var("CARGO_CFG_TARGET_VENDOR").as_deref() == Ok("apple")
}

/// llvm/patches/arm64e-x1.patch: without it ld64.lld rejects the macOS 27
/// SDK, and Apple targets only link on macOS.
fn x1_patched(prefix: &Path) -> bool {
    std::fs::read_to_string(prefix.join("include/llvm/TextAPI/Architecture.def"))
        .is_ok_and(|d| d.contains("arm64e_x1"))
}

/// Does the prefix run on the cargo target (`LLVM_HOST_TRIPLE`'s arch and
/// OS)?
fn for_target(prefix: &Path) -> bool {
    let config = std::fs::read_to_string(prefix.join("include/llvm/Config/llvm-config.h"))
        .unwrap_or_default();
    let Some(host) = config.lines().find_map(|l| {
        l.strip_prefix("#define LLVM_HOST_TRIPLE \"")?
            .strip_suffix('"')
    }) else {
        return false;
    };
    let arch = match host.split('-').next().unwrap_or("") {
        "arm64" => "aarch64",
        "amd64" => "x86_64",
        a => a,
    };
    let os = ["darwin", "macos", "windows", "linux"]
        .into_iter()
        .find(|o| host.contains(&format!("-{o}")))
        .map_or("", |o| if o == "darwin" { "macos" } else { o });
    env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok(arch)
        && env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok(os)
}

/// The prefix's LLVM was built against libc++ (`std::__1`), not
/// libstdc++ (Linux).
fn libcxx(lib: &Path) -> bool {
    let support = std::fs::read(lib.join("libLLVMSupport.a")).unwrap_or_default();
    support.windows(7).any(|w| w == b"St3__1E")
}

/// Compile the shim and dsymutil's sources (`share/rok/dsymutil`) in
/// parallel into one archive with `$CXX` (`c++`; MSVC: `cl` / `clang-cl`)
/// and `ar` (`lib`).
fn compile_shim(prefix: &Path, out: &Path, apple: bool, msvc: bool) {
    let dsym = prefix.join("share/rok/dsymutil");
    let mut sources = vec![PathBuf::from("src/shim.cpp")];
    sources.extend(
        std::fs::read_dir(&dsym)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "cpp")),
    );
    println!("cargo:rerun-if-changed={}", dsym.display());
    let (obj_ext, default_cxx) = if msvc { ("obj", "cl") } else { ("o", "c++") };
    let objs: Vec<PathBuf> = std::thread::scope(|s| {
        // Collect first so every compile starts before any join.
        #[allow(clippy::needless_collect)]
        let jobs: Vec<_> = sources
            .iter()
            .map(|src| {
                let obj = out.join(src.file_stem().unwrap()).with_extension(obj_ext);
                let dsym = &dsym;
                s.spawn(move || {
                    let cxx = env::var_os("CXX").unwrap_or_else(|| default_cxx.into());
                    let mut cmd = Command::new(cxx);
                    if msvc {
                        // LLVM's own flags, as cl / clang-cl take them.
                        cmd.args([
                            "/nologo",
                            "/std:c++17",
                            "/O2",
                            "/DNDEBUG",
                            "/GR-",
                            "/MD",
                            "/EHs-c-",
                            "/D_HAS_EXCEPTIONS=0",
                            "/Zc:__cplusplus",
                            // The option tables' variadic macros.
                            "/Zc:preprocessor",
                            "/bigobj",
                            "/w",
                            "/c",
                        ])
                        .arg(src)
                        .arg(format!("/I{}", prefix.join("include").display()))
                        .arg(format!("/I{}", dsym.display()))
                        .arg(format!("/Fo{}", obj.display()));
                    } else {
                        cmd.args([
                            "-std=c++17",
                            "-O2",
                            "-DNDEBUG",
                            "-fno-rtti",
                            "-fno-exceptions",
                            "-fPIC",
                            "-fvisibility-inlines-hidden",
                            "-w",
                            "-c",
                        ])
                        .arg(src)
                        .arg(format!("-I{}", prefix.join("include").display()))
                        .arg(format!("-I{}", dsym.display()))
                        .arg("-o")
                        .arg(&obj);
                    }
                    if apple {
                        cmd.arg("-mmacosx-version-min=11.0");
                    }
                    run(&mut cmd);
                    obj
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    if msvc {
        let archive = out.join("rokc_llvm_shim.lib");
        run(Command::new("lib")
            .arg("/nologo")
            .arg(format!("/out:{}", archive.display()))
            .args(&objs));
    } else {
        let archive = out.join("librokc_llvm_shim.a");
        let _ = std::fs::remove_file(&archive);
        run(Command::new("ar").arg("crs").arg(&archive).args(&objs));
    }
}

fn run(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("cannot run {cmd:?}: {e}"));
    assert!(status.success(), "{cmd:?} failed");
}
