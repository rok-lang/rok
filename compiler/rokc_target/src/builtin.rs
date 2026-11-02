//! The built-in targets: Tier 1, then the targets without a guarantee.

use crate::{CAbi, Libc, Linker, ObjectFormat, StackProbes, TargetSpec};

/// Every built-in target's name.
pub const BUILTIN: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "wasm32-unknown-unknown",
    "wasm32-wasip2",
    "wasm32-wasip1-threads",
    "i686-unknown-linux-gnu",
    "i686-pc-windows-msvc",
    "armv7-unknown-linux-gnueabihf",
    "riscv64gc-unknown-linux-gnu",
    "powerpc64le-unknown-linux-gnu",
    "x86_64-unknown-freebsd",
    "riscv32imac-unknown-none-elf",
    "s390x-unknown-linux-gnu",
];

/// rok's thread stack: 8 MiB, as Linux's default rlimit and macOS give
/// the main thread (nested `await` runs on it).
const STACK: u64 = 8 << 20;

const X86_64_ELF: &str =
    "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128";
const AARCH64_ELF: &str =
    "e-m:e-p270:32:32-p271:32:32-p272:64:64-i8:8:32-i16:16:32-i64:64-i128:128-n32:64-S128-Fn32";
const WASM32: &str = "e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20";

fn s(v: &str) -> String {
    v.to_owned()
}

fn v(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| (*x).to_owned()).collect()
}

/// A Linux ELF target on glibc, `name` as its LLVM triple.
fn linux_gnu(name: &str, arch: &str, emulation: &str, loader: &str) -> TargetSpec {
    TargetSpec {
        name: s(name),
        llvm_target: s(name),
        deployment_target: s(""),
        arch: s(arch),
        os: s("linux"),
        env: s("gnu"),
        vendor: s("unknown"),
        family: s("unix"),
        endian: s("little"),
        pointer_width: 64,
        wide_align: 8,
        max_atomic_width: 64,
        c_abi: CAbi::Other,
        data_layout: s(""),
        cpu: s("generic"),
        features: s(""),
        object_format: ObjectFormat::Elf,
        linker: Linker::Ld,
        pre_link_args: v(&["-m", emulation]),
        crt_objects: v(&["Scrt1.o", "crti.o"]),
        // core's float methods call libm; glibc keeps it out of libc.
        late_link_args: v(&[
            "--as-needed",
            "-lm",
            "--no-as-needed",
            "-lc",
            "libc_nonshared.a",
            "crtn.o",
        ]),
        dynamic_linking: true,
        dynamic_loader: s(loader),
        libc: Libc::Glibc,
        rust_target: s(name),
        exe_suffix: s(""),
        staticlib_prefix: s("lib"),
        staticlib_suffix: s(".a"),
        dylib_prefix: s("lib"),
        dylib_suffix: s(".so"),
        c_symbol_prefix: s(""),
        stack_size: STACK,
        stack_probes: StackProbes::None,
        threads: true,
        native_backend: false,
        repl: false,
        profile_runtime: s("libclang_rt.profile.a"),
    }
}

/// The musl variant `name` of glibc target `t`: static executables.
fn musl(name: &str, t: TargetSpec) -> TargetSpec {
    TargetSpec {
        name: s(name),
        llvm_target: s(name),
        rust_target: s(name),
        env: s("musl"),
        crt_objects: v(&["crt1.o", "crti.o"]),
        late_link_args: v(&["libc.a", "crtn.o"]),
        dynamic_linking: false,
        dynamic_loader: s(""),
        libc: Libc::Musl,
        // Static: a session host has no dlsym to bind its cells.
        repl: false,
        ..t
    }
}

/// `x86_64` / `aarch64` Linux: rok's own backend, the REPL.
fn native(t: TargetSpec) -> TargetSpec {
    TargetSpec {
        native_backend: true,
        repl: true,
        stack_probes: StackProbes::Inline,
        ..t
    }
}

fn x86_64(t: TargetSpec) -> TargetSpec {
    native(TargetSpec {
        c_abi: CAbi::X86_64SysV,
        data_layout: s(X86_64_ELF),
        cpu: s("x86-64"),
        ..t
    })
}

fn aarch64(t: TargetSpec) -> TargetSpec {
    native(TargetSpec {
        c_abi: CAbi::Aarch64,
        data_layout: s(AARCH64_ELF),
        features: s("+neon"),
        ..t
    })
}

/// i686: SSE2.
fn i686(t: TargetSpec) -> TargetSpec {
    TargetSpec {
        arch: s("x86"),
        pointer_width: 32,
        cpu: s("pentium4"),
        stack_probes: StackProbes::Inline,
        ..t
    }
}

fn apple(name: &str, arch: &str, llvm_arch: &str) -> TargetSpec {
    TargetSpec {
        llvm_target: format!("{llvm_arch}-apple-macosx"),
        // What the runtime library is built for.
        deployment_target: s("11.0"),
        os: s("macos"),
        env: s(""),
        vendor: s("apple"),
        object_format: ObjectFormat::MachO,
        linker: Linker::Ld64,
        pre_link_args: v(&["-arch", llvm_arch]),
        crt_objects: Vec::new(),
        late_link_args: v(&["-lSystem"]),
        dynamic_loader: s(""),
        libc: Libc::LibSystem,
        dylib_suffix: s(".dylib"),
        c_symbol_prefix: s("_"),
        ..linux_gnu(name, arch, "", "")
    }
}

fn windows(name: &str, arch: &str, machine: &str) -> TargetSpec {
    TargetSpec {
        os: s("windows"),
        env: s("msvc"),
        vendor: s("pc"),
        family: s("windows"),
        object_format: ObjectFormat::Coff,
        linker: Linker::LldLink,
        pre_link_args: vec![format!("/machine:{machine}")],
        crt_objects: Vec::new(),
        late_link_args: Vec::new(),
        dynamic_loader: s(""),
        libc: Libc::Ucrt,
        exe_suffix: s(".exe"),
        staticlib_prefix: s(""),
        staticlib_suffix: s(".lib"),
        dylib_prefix: s(""),
        dylib_suffix: s(".dll"),
        profile_runtime: s("clang_rt.profile.lib"),
        ..linux_gnu(name, arch, "", "")
    }
}

/// wasm32 against wasi-libc: a command module.
fn wasm32(name: &str, rust_target: &str) -> TargetSpec {
    TargetSpec {
        os: s("wasi"),
        env: s(""),
        family: s("wasm"),
        pointer_width: 32,
        c_abi: CAbi::Wasm32,
        data_layout: s(WASM32),
        // SIMD128.
        features: s("+simd128"),
        object_format: ObjectFormat::Wasm,
        linker: Linker::WasmLd,
        pre_link_args: Vec::new(),
        crt_objects: v(&["crt1-command.o"]),
        late_link_args: v(&["libc.a"]),
        dynamic_linking: false,
        dynamic_loader: s(""),
        libc: Libc::WasiLibc,
        rust_target: s(rust_target),
        exe_suffix: s(".wasm"),
        threads: false,
        ..linux_gnu(name, "wasm32", "", "")
    }
}

/// The built-in target `name`.
pub fn spec(name: &str) -> Option<TargetSpec> {
    Some(match name {
        "x86_64-unknown-linux-gnu" => x86_64(linux_gnu(
            name,
            "x86_64",
            "elf_x86_64",
            "/lib64/ld-linux-x86-64.so.2",
        )),
        "aarch64-unknown-linux-gnu" => aarch64(linux_gnu(
            name,
            "aarch64",
            "aarch64linux",
            "/lib/ld-linux-aarch64.so.1",
        )),
        "x86_64-unknown-linux-musl" => musl(name, spec("x86_64-unknown-linux-gnu")?),
        "aarch64-unknown-linux-musl" => musl(name, spec("aarch64-unknown-linux-gnu")?),
        "i686-unknown-linux-gnu" => i686(TargetSpec {
            wide_align: 4,
            c_abi: CAbi::X86SysV,
            data_layout: s(
                "e-m:e-p:32:32-p270:32:32-p271:32:32-p272:64:64-i128:128-f64:32:64-f80:32-n8:16:32-S128",
            ),
            ..linux_gnu(name, "x86", "elf_i386", "/lib/ld-linux.so.2")
        }),
        "armv7-unknown-linux-gnueabihf" => TargetSpec {
            pointer_width: 32,
            c_abi: CAbi::ArmHf,
            data_layout: s("e-m:e-p:32:32-Fi8-i64:64-v128:64:128-a:0:32-n32-S64"),
            // NEON; the thread pointer from TPIDRURO, not
            // glibc's `__aeabi_read_tp` (in ld.so).
            features: s("+vfp3,+d32,+neon,+read-tp-tpidruro"),
            ..linux_gnu(name, "arm", "armelf_linux_eabi", "/lib/ld-linux-armhf.so.3")
        },
        "x86_64-apple-darwin" => TargetSpec {
            data_layout: s(
                "e-m:o-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128",
            ),
            cpu: s("core2"),
            ..x86_64(apple(name, "x86_64", "x86_64"))
        },
        "aarch64-apple-darwin" => {
            let t = aarch64(apple(name, "aarch64", "arm64"));
            TargetSpec {
                data_layout: s(
                    "e-m:o-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-n32:64-S128-Fn32",
                ),
                cpu: s("apple-m1"),
                features: s(""),
                ..t
            }
        }
        "x86_64-pc-windows-msvc" => TargetSpec {
            c_abi: CAbi::X86_64Win,
            data_layout: s(
                "e-m:w-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128",
            ),
            cpu: s("x86-64"),
            stack_probes: StackProbes::Call,
            native_backend: true,
            repl: true,
            ..windows(name, "x86_64", "x64")
        },
        "aarch64-pc-windows-msvc" => TargetSpec {
            c_abi: CAbi::Aarch64,
            data_layout: s(
                "e-m:w-p270:32:32-p271:32:32-p272:64:64-p:64:64-i32:32-i64:64-i128:128-n32:64-S128-Fn32",
            ),
            features: s("+neon"),
            stack_probes: StackProbes::Call,
            native_backend: true,
            repl: true,
            ..windows(name, "aarch64", "arm64")
        },
        "i686-pc-windows-msvc" => {
            let mut t = i686(windows(name, "x86", "x86"));
            // 4 GiB address space on 64-bit Windows (the heap),
            // no SafeSEH: rok raises no SEH and the runtime's handler is
            // vectored.
            t.pre_link_args
                .extend(v(&["/largeaddressaware", "/safeseh:no"]));
            TargetSpec {
                c_abi: CAbi::X86Win,
                data_layout: s(
                    "e-m:x-p:32:32-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32-a:0:32-S32",
                ),
                c_symbol_prefix: s("_"),
                stack_probes: StackProbes::Call,
                ..t
            }
        }
        // A WASI preview 2 component.
        "wasm32-wasip2" => TargetSpec {
            // LLVM sees plain wasi; the linker makes the component.
            llvm_target: s("wasm32-wasi"),
            linker: Linker::WasmComponentLd,
            ..wasm32(name, "wasm32-wasip2")
        },
        // Preview 1 with wasi-threads: host-provided shared memory and
        // `wasi_thread_start` as each thread's entry.
        "wasm32-wasip1-threads" => TargetSpec {
            llvm_target: s("wasm32-wasi-threads"),
            features: s("+simd128,+atomics,+bulk-memory,+mutable-globals"),
            pre_link_args: v(&[
                "--shared-memory",
                "--import-memory",
                "--export-memory",
                "--export=wasi_thread_start",
            ]),
            threads: true,
            ..wasm32(name, "wasm32-wasip1-threads")
        },
        // The browser's: a reactor. The host calls `_initialize`, then
        // exports, flushing stdout (rok's and C's buffers) after each, since
        // no `exit` does.
        "wasm32-unknown-unknown" => TargetSpec {
            os: s("unknown"),
            pre_link_args: v(&[
                "--no-entry",
                "--export-if-defined=rok_stdio_flush",
                "--export-if-defined=fflush",
            ]),
            crt_objects: v(&["crt1-reactor.o"]),
            // No file system to write a profile to.
            profile_runtime: s(""),
            ..wasm32(name, "wasm32-wasip1")
        },
        // Tier 1.
        "riscv64gc-unknown-linux-gnu" => TargetSpec {
            llvm_target: s("riscv64-unknown-linux-gnu"),
            c_abi: CAbi::Riscv64,
            data_layout: s("e-m:e-p:64:64-i64:64-i128:128-n32:64-S128"),
            cpu: s("generic-rv64"),
            features: s("+m,+a,+f,+d,+c"),
            stack_probes: StackProbes::Inline,
            ..linux_gnu(
                name,
                "riscv64",
                "elf64lriscv",
                "/lib/ld-linux-riscv64-lp64d.so.1",
            )
        },
        // No guarantee.
        "powerpc64le-unknown-linux-gnu" => TargetSpec {
            data_layout: s("e-m:e-Fn32-i64:64-i128:128-n32:64-S128-v256:256:256-v512:512:512"),
            cpu: s("ppc64le"),
            ..linux_gnu(name, "powerpc64", "elf64lppc", "/lib64/ld64.so.2")
        },
        "s390x-unknown-linux-gnu" => TargetSpec {
            endian: s("big"),
            data_layout: s("E-m:e-i1:8:16-i8:8:16-i64:64-f128:64-v128:64-a:8:16-n32:64"),
            cpu: s("z10"),
            ..linux_gnu(name, "s390x", "elf64_s390", "/lib/ld64.so.1")
        },
        "x86_64-unknown-freebsd" => TargetSpec {
            os: s("freebsd"),
            env: s(""),
            late_link_args: v(&["-lc", "crtn.o"]),
            crt_objects: v(&["crt1.o", "crti.o"]),
            dynamic_loader: s("/libexec/ld-elf.so.1"),
            libc: Libc::FreeBsd,
            native_backend: false,
            repl: false,
            ..x86_64(TargetSpec {
                pre_link_args: v(&["-m", "elf_x86_64_fbsd"]),
                ..linux_gnu(name, "x86_64", "", "")
            })
        },
        "riscv32imac-unknown-none-elf" => TargetSpec {
            llvm_target: s("riscv32"),
            os: s("none"),
            env: s(""),
            family: s(""),
            pointer_width: 32,
            max_atomic_width: 32,
            data_layout: s("e-m:e-p:32:32-i64:64-n32-S128"),
            cpu: s("generic-rv32"),
            features: s("+m,+a,+c"),
            crt_objects: Vec::new(),
            late_link_args: Vec::new(),
            dynamic_linking: false,
            dynamic_loader: s(""),
            libc: Libc::None,
            rust_target: s("riscv32imac-unknown-none-elf"),
            threads: false,
            profile_runtime: s(""),
            ..linux_gnu(name, "riscv32", "elf32lriscv", "")
        },
        _ => return None,
    })
}
