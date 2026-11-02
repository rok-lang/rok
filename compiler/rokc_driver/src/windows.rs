//! Windows (`*-pc-windows-msvc`) without MSVC or the Windows SDK: import
//! libraries built in process from embedded mingw-w64 module definitions
//! (`windows/<arch>/*.def`, `gen.sh`), rok's own CRT startup over UCRT,
//! and lld-link options. No MSVC static library is linked.

use std::ffi::OsString;
use std::path::Path;

/// `(link name, module definition)`, the UCRT first: `ntdll` repeats some
/// C functions, and the first import library that has a symbol wins.
type Defs = [(&'static str, &'static str); 11];

macro_rules! defs {
    ($arch:literal) => {
        defs!($arch; "ucrtbase" "kernel32" "synchronization" "ws2_32" "advapi32" "bcrypt"
            "userenv" "shell32" "ole32" "user32" "ntdll")
    };
    ($arch:literal; $($name:literal)*) => {
        [$(($name, include_str!(concat!("../windows/", $arch, "/", $name, ".def")))),*]
    };
}

const X86_64: Defs = defs!("x86_64");
const AARCH64: Defs = defs!("aarch64");
const I686: Defs = defs!("i686");

/// `-C subsystem=console|windows`: entry `mainCRTStartup`, or
/// `WinMainCRTStartup` calling `WinMain`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Subsystem {
    #[default]
    Console,
    Windows,
}

impl Subsystem {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "console" => Some(Self::Console),
            "windows" => Some(Self::Windows),
            _ => None,
        }
    }
}

fn defs(t: &rokc_llvm::Target) -> Result<&'static Defs, String> {
    match t.arch.as_str() {
        "x86_64" => Ok(&X86_64),
        "aarch64" => Ok(&AARCH64),
        "x86" => Ok(&I686),
        _ => Err(format!("no Windows libraries for {}", t.name)),
    }
}

/// lld-link arguments besides objects and archives: startup object and
/// import libraries (written to `dir`), `#[link]` libraries the embedded
/// set lacks, subsystem and entry, and `/debug` with `debug`.
pub fn link_args(
    t: &rokc_llvm::Target,
    native_libs: &[(String, bool)],
    subsystem: Subsystem,
    debug: bool,
    dir: &Path,
) -> Result<Vec<OsString>, String> {
    let defs = defs(t)?;
    let startup = dir.join("rok_startup.obj");
    let obj = rokc_llvm::compile_llvm_ir(
        &startup_ir(t, subsystem),
        &rokc_llvm::Compile {
            target: t,
            opt: rokc_llvm::OptLevel::O2,
            emit: rokc_llvm::Emit::Obj,
            tls_initial_exec: false,
            mlir_pipeline: "",
            profile: rokc_llvm::Profile::None,
        },
    )?;
    std::fs::write(&startup, obj).map_err(|e| format!("{}: {e}", startup.display()))?;
    let mut a = vec![startup.into_os_string()];
    for (name, _) in native_libs {
        if !defs.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            a.push(format!("{name}.lib").into());
        }
    }
    for (name, def) in defs {
        let lib = dir.join(format!("{name}.lib"));
        rokc_llvm::import_library(t, def, &lib)?;
        a.push(lib.into_os_string());
    }
    let (sub, entry) = match subsystem {
        Subsystem::Console => ("console", "mainCRTStartup"),
        Subsystem::Windows => ("windows", "WinMainCRTStartup"),
    };
    a.push(format!("/subsystem:{sub}").into());
    a.push(format!("/entry:{entry}").into());
    // 8 MiB main stack like Linux, macOS and the runtime's own threads
    // (nested `await` runs on it); Windows' default is 1 MiB.
    a.push(format!("/stack:{}", t.stack_size).into());
    // Nothing refers to the TLS directory but the loader. (`/include`
    // takes the symbol, C names `_`-prefixed on x86.)
    a.push(format!("/include:{}_tls_used", t.c_symbol_prefix).into());
    // The PE timestamp and PDB signature: a hash of the output, so equal
    // inputs link to equal files.
    a.push("/Brepro".into());
    if debug {
        a.push("/debug".into());
    }
    Ok(a)
}

/// rok's CRT startup instead of MSVC's static parts: the entry (UCRT setup,
/// `.CRT$XC*` initializers, `main`, `exit`), the loader's TLS directory
/// (`_tls_used`), `_fltused`, and `__chkstk` (LLVM's probe for frames of a
/// page or more). LLVM and lld-link mangle C names (`_` prefix on x86).
fn startup_ir(t: &rokc_llvm::Target, subsystem: Subsystem) -> String {
    let chkstk = if t.arch == "x86" {
        // eax: frame size. Touch each page below the caller's esp, then
        // lower esp by eax (MSVC's x86 `_chkstk` / `_alloca_probe`); other
        // registers kept. `_tls_array`: the TEB offset of the TLS array
        // that thread-local accesses read through %fs.
        r#"module asm ".globl __tls_array"
module asm "__tls_array = 0x2c"
module asm ".text"
module asm ".globl __chkstk"
module asm ".p2align 4"
module asm "__chkstk:"
module asm "  push %ecx"
module asm "  cmp $0x1000, %eax"
module asm "  lea 8(%esp), %ecx"
module asm "  jb 1f"
module asm "2:"
module asm "  sub $0x1000, %ecx"
module asm "  test %ecx, (%ecx)"
module asm "  sub $0x1000, %eax"
module asm "  cmp $0x1000, %eax"
module asm "  ja 2b"
module asm "1:"
module asm "  sub %eax, %ecx"
module asm "  test %ecx, (%ecx)"
module asm "  lea 4(%esp), %eax"
module asm "  mov %ecx, %esp"
module asm "  mov -4(%eax), %ecx"
module asm "  push (%eax)"
module asm "  sub %esp, %eax"
module asm "  ret"
"#
    } else if t.arch == "aarch64" {
        // x15: the frame size / 16. Touches each page below sp; keeps
        // every register but x16 / x17.
        r#"module asm ".text"
module asm ".globl __chkstk"
module asm ".p2align 2"
module asm "__chkstk:"
module asm "  lsl x16, x15, #4"
module asm "  mov x17, sp"
module asm "1:"
module asm "  sub x17, x17, #4096"
module asm "  subs x16, x16, #4096"
module asm "  ldr xzr, [x17]"
module asm "  b.gt 1b"
module asm "  ret"
"#
    } else {
        // rax: the frame size. Touches each page below the caller's rsp;
        // keeps every register.
        r#"module asm ".text"
module asm ".globl __chkstk"
module asm ".p2align 4"
module asm "__chkstk:"
module asm "  push %rcx"
module asm "  push %rax"
module asm "  cmp $0x1000, %rax"
module asm "  lea 24(%rsp), %rcx"
module asm "  jb 1f"
module asm "2:"
module asm "  sub $0x1000, %rcx"
module asm "  test %rcx, (%rcx)"
module asm "  sub $0x1000, %rax"
module asm "  cmp $0x1000, %rax"
module asm "  ja 2b"
module asm "1:"
module asm "  sub %rax, %rcx"
module asm "  test %rcx, (%rcx)"
module asm "  pop %rax"
module asm "  pop %rcx"
module asm "  ret"
"#
    };
    // `_set_app_type`: 1 console, 2 GUI.
    let entry = match subsystem {
        Subsystem::Console => {
            r"define void @mainCRTStartup() {
  call void @_set_app_type(i32 1)
  %a = call i32 @_configure_narrow_argv(i32 1)
  %e = call i32 @_initialize_narrow_environment()
  call void @_initterm(ptr @__xc_a, ptr @__xc_z)
  %r = call i32 @main()
  call void @exit(i32 %r)
  unreachable
}
"
        }
        // rok's WinMain ignores lpCmdLine (the runtime reads
        // the command line itself), so it gets "".
        Subsystem::Windows => {
            r"@__ImageBase = external global i8
@empty = private constant [1 x i8] zeroinitializer

define void @WinMainCRTStartup() {
  call void @_set_app_type(i32 2)
  %a = call i32 @_configure_narrow_argv(i32 1)
  %e = call i32 @_initialize_narrow_environment()
  call void @_initterm(ptr @__xc_a, ptr @__xc_z)
  %r = call i32 @WinMain(ptr @__ImageBase, ptr null, ptr @empty, i32 10)
  call void @exit(i32 %r)
  unreachable
}

define i32 @WinMain(ptr %instance, ptr %prev, ptr %cmd, i32 %show) {
  %r = call i32 @main()
  ret i32 %r
}
"
        }
    };
    format!(
        r#"{chkstk}
@_tls_index = global i32 0
@_tls_start = global i8 0, section ".tls"
@_tls_end = global i8 0, section ".tls$ZZZ"
@__xl_a = global ptr null, section ".CRT$XLA"
@__xl_z = global ptr null, section ".CRT$XLZ"
@_tls_used = constant {{ ptr, ptr, ptr, ptr, i32, i32 }} {{ ptr @_tls_start, ptr @_tls_end, ptr @_tls_index, ptr getelementptr (ptr, ptr @__xl_a, i32 1), i32 0, i32 0 }}, section ".rdata$T"
@_fltused = global i32 39029
@__xc_a = constant ptr null, section ".CRT$XCA"
@__xc_z = constant ptr null, section ".CRT$XCZ"

declare void @_set_app_type(i32)
declare i32 @_configure_narrow_argv(i32)
declare i32 @_initialize_narrow_environment()
declare void @_initterm(ptr, ptr)
declare void @exit(i32) noreturn
declare i32 @main()

{entry}
{}"#,
        stdio_ir(t)
    )
}

/// `printf` / `fprintf` / `sprintf` / `snprintf`: UCRT exports none (its
/// headers inline them over `__stdio_common_v*printf`), so C variadic
/// calls reach these. `va_list` is a pointer on every
/// Windows arch. Selectany COMDATs, like a C object's inline copies.
fn stdio_ir(t: &rokc_llvm::Target) -> String {
    let n = format!("i{}", t.pointer_width);
    // (signature, setup, call) per function; `%va` is the va_list.
    let fns = [
        (
            "printf(ptr %fmt, ...)",
            "  %f = call ptr @__acrt_iob_func(i32 1)\n",
            "call i32 @__stdio_common_vfprintf(i64 0, ptr %f, ptr %fmt, ptr null, ptr %va)",
        ),
        (
            "fprintf(ptr %f, ptr %fmt, ...)",
            "",
            "call i32 @__stdio_common_vfprintf(i64 0, ptr %f, ptr %fmt, ptr null, ptr %va)",
        ),
        // _CRT_INTERNAL_PRINTF_LEGACY_VSPRINTF_NULL_TERMINATION
        (
            "sprintf(ptr %b, ptr %fmt, ...)",
            "",
            &format!(
                "call i32 @__stdio_common_vsprintf(i64 1, ptr %b, {n} -1, ptr %fmt, ptr null, ptr %va)"
            ),
        ),
        // _CRT_INTERNAL_PRINTF_STANDARD_SNPRINTF_BEHAVIOR
        (
            &format!("snprintf(ptr %b, {n} %n, ptr %fmt, ...)"),
            "",
            &format!(
                "call i32 @__stdio_common_vsprintf(i64 2, ptr %b, {n} %n, ptr %fmt, ptr null, ptr %va)"
            ),
        ),
    ];
    let mut s = format!(
        "declare ptr @__acrt_iob_func(i32)
declare i32 @__stdio_common_vfprintf(i64, ptr, ptr, ptr, ptr)
declare i32 @__stdio_common_vsprintf(i64, ptr, {n}, ptr, ptr, ptr)
declare void @llvm.va_start.p0(ptr)
declare void @llvm.va_end.p0(ptr)
"
    );
    for (sig, setup, call) in fns {
        let name = &sig[..sig.find('(').unwrap()];
        s += &format!(
            "
${name} = comdat any
define weak_odr i32 @{sig} comdat {{
  %ap = alloca ptr
  call void @llvm.va_start.p0(ptr %ap)
  %va = load ptr, ptr %ap
{setup}  %r = {call}
  call void @llvm.va_end.p0(ptr %ap)
  %neg = icmp slt i32 %r, 0
  %out = select i1 %neg, i32 -1, i32 %r
  ret i32 %out
}}
"
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(machine, subsystem)` of a PE image.
    fn pe_header(exe: &[u8]) -> (u16, u16) {
        assert_eq!(&exe[..2], b"MZ");
        let pe = u32::from_le_bytes(exe[0x3c..0x40].try_into().unwrap()) as usize;
        assert_eq!(&exe[pe..pe + 4], b"PE\0\0");
        let machine = u16::from_le_bytes([exe[pe + 4], exe[pe + 5]]);
        // The optional header's `Subsystem`.
        let sub = pe + 24 + 68;
        (machine, u16::from_le_bytes([exe[sub], exe[sub + 1]]))
    }

    /// A `main` alone links into a console / GUI PE with nothing but
    /// what rokc carries: no MSVC, no Windows SDK.
    #[test]
    fn links_a_pe_from_embedded_libraries() {
        if !rokc_llvm::AVAILABLE {
            return;
        }
        let dir = crate::TempDir::new(false).unwrap();
        for (triple, machine) in [
            ("x86_64-pc-windows-msvc", 0x8664),
            ("aarch64-pc-windows-msvc", 0xaa64),
            ("i686-pc-windows-msvc", 0x14c),
        ] {
            let t = rokc_llvm::Target::builtin(triple).unwrap();
            let obj = rokc_llvm::compile_llvm_ir(
                "define i32 @main() {\n  ret i32 0\n}\n",
                &rokc_llvm::Compile {
                    target: &t,
                    opt: rokc_llvm::OptLevel::O0,
                    emit: rokc_llvm::Emit::Obj,
                    tls_initial_exec: false,
                    mlir_pipeline: "",
                    profile: rokc_llvm::Profile::None,
                },
            )
            .unwrap();
            let main = dir.path().join(format!("{triple}.obj"));
            std::fs::write(&main, obj).unwrap();
            for (sub, code) in [(Subsystem::Console, 3), (Subsystem::Windows, 2)] {
                let exe = dir.path().join(format!("{triple}-{code}.exe"));
                let libs = [("ws2_32".to_owned(), false)];
                let mut args = vec![main.clone().into_os_string()];
                args.extend(link_args(&t, &libs, sub, false, dir.path()).unwrap());
                rokc_llvm::link_executable(&t, &args, &exe, true, None).unwrap();
                assert_eq!(pe_header(&std::fs::read(&exe).unwrap()), (machine, code));
            }
        }
    }
}
