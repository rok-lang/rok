//! Interactive tools through the binaries: `rokc repl` fed by a pipe,
//! `rokls` / `rokc lsp` framing, `rokc check -`. (The language server's
//! queries are tested in `rokc_tools`.)

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Output, Stdio};

use common::{ROKC, scratch, setup, setup_runtime, text, workspace};

/// `exe args` with `input` on stdin.
fn piped(exe: &str, args: &[&str], input: &str) -> Output {
    piped_env(exe, args, input, &[])
}

/// `piped` with environment variables `env`.
fn piped_env(exe: &str, args: &[&str], input: &str, env: &[(&str, &str)]) -> Output {
    let mut child = Command::new(exe)
        .args(args)
        .env("ROK_HOME", workspace())
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §0.1.1 §0.1.2 §0.1.3 §4.16.10 §6.10.3 §9.1.2 §9.7.0 §9.7.1 §9.7.3 §9.7.4 §9.7.5 §9.7.7 §9.7.8 §9.9.9 / std §2.12.4
fn repl_keeps_state_across_cells() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
1 + 2
let x =
    40i32;
#[derive(Debug)]
struct P { x: i32; y: i32; }
fn add(p: P) -> i32 { p.x + p.y }
let p = P { x: x, y: 2 };
println(f\"sum {add(p)}\");
p
:type add(p)
let x = \"rebound\";
x
nope
fn add(p: P) -> i32 { p.x * p.y }
add(p)
panic(\"boom\");
[1, 2, 3]
let s: i32 | f64 = 7i32;
s
let a: any = 1.5;
a
:reset
:type x
:quit
1 + 1
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(
        stdout,
        "\
3
sum 42
P { x: 40, y: 2 }
i32
rebound
80
[1, 2, 3]
7
f64
",
        "stderr: {stderr}"
    );
    // A bad cell reports and is dropped; so is one that fails at run time.
    assert!(stderr.contains("cannot find value `nope`"), "{stderr}");
    assert!(stderr.contains("boom"), "{stderr}");
    // After `:reset` nothing is bound.
    assert!(stderr.contains("cannot find value `x`"), "{stderr}");
}

/// The REPL prints an expression cell with `Display`, else `Debug`, a
/// trailing `?` with `Debug`, only the type when the value has neither;
/// `println` uses `Display`; a `let` cell prints nothing.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.1 / std §2.10.11
fn repl_prints_debug_and_println_display() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
\"hello\"
struct Q { v: i32; }
impl Display for Q { fn fmt(self, ref f: Formatter) throws (FmtError) { try f.write_str(f\"Q={self.v}\") } }
let q = Q { v: 3 };
q
println(f\"{q}\");
#[derive(Debug)]
struct D { v: i32; }
let d = D { v: 4 };
println(f\"{d:?}\");
d
let x = 42;
x
let xs = [1, 2, 3];
xs?
\"hello\"?
d?
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(
        text(&out.stdout),
        "hello\nQ=3\nQ=3\nD { v: 4 }\nD { v: 4 }\n42\n[1, 2, 3]\n\"hello\"\nD { v: 4 }\n",
        "stderr: {stderr}"
    );
}

/// Piped input that ends inside a cell (9.7.8 continuation) is an error,
/// not a silently dropped cell.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.8
fn repl_reports_input_ending_inside_a_cell() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let out = piped(ROKC, &["repl", "--rokr", &rokr], "1 + 2\nlet x = f(40,");
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "3\n", "{stderr}");
    assert!(
        stderr.contains("error: input ended inside a cell; not run")
            && stderr.contains("let x = f(40,"),
        "{stderr}"
    );
    assert_eq!(out.status.code(), Some(1), "{stderr}");
}

/// `Rok <version> on <host>` as `rokc --version --verbose` reports them.
fn toolchain_line() -> String {
    let out = Command::new(ROKC)
        .args(["--version", "--verbose"])
        .output()
        .unwrap();
    let out = text(&out.stdout);
    let version = out.lines().next().unwrap().strip_prefix("rokc ").unwrap();
    let host = out.lines().find_map(|l| l.strip_prefix("host: ")).unwrap();
    format!("Rok {version} on {host}")
}

/// A pseudo-terminal: the master side, and the slave for a child's
/// stdio.
#[cfg(unix)]
fn pty() -> (std::fs::File, std::fs::File) {
    // SAFETY: plain libc calls on a pseudo-terminal the caller owns.
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0 && libc::grantpt(m) == 0 && libc::unlockpt(m) == 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(m)).to_owned();
        let master = <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(m);
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name.to_str().unwrap())
            .unwrap();
        (master, slave)
    }
}

/// `rokc args` on a pseudo-terminal with `ROK_HOME` at `home`: each of
/// `steps` typed at the next prompt. What it wrote; it must succeed.
#[cfg(unix)]
fn on_tty(args: &[&str], home: &std::path::Path, steps: &[&[u8]]) -> String {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let (master, slave) = pty();
    let mut child = Command::new(ROKC)
        .args(args)
        .env("ROK_HOME", home)
        .env("TERM", "xterm")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("COLORTERM")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    let mut keys = master.try_clone().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut master = master;
        let mut b = [0u8; 4096];
        // EIO once the child is gone.
        while let Ok(n @ 1..) = master.read(&mut b) {
            if tx.send(b[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut out = String::new();
    // Each prompt turns bracketed paste on: wait for the `n`th, as keys
    // typed before raw mode would meet the line discipline.
    let wait = |out: &mut String, n: usize| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while out.matches("\x1b[?2004h").count() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(b) = rx.recv_timeout(left) else {
                panic!("no prompt {n}: {out:?}");
            };
            out.push_str(&String::from_utf8_lossy(&b));
        }
    };
    for (i, keys_in) in steps.iter().enumerate() {
        wait(&mut out, i + 1);
        keys.write_all(keys_in).unwrap();
    }
    assert!(child.wait().unwrap().success(), "{out:?}");
    while let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
        out.push_str(&String::from_utf8_lossy(&b));
    }
    out
}

/// On a terminal: the session starts with the toolchain line, Tab
/// completes core names, a paste is one cell, Up and `C-r` recall whole
/// cells, the input is coloured, and the history file keeps the cells.
#[test]
#[cfg_attr(
    not(all(
        any(target_os = "macos", target_os = "linux"),
        any(target_arch = "aarch64", target_arch = "x86_64")
    )),
    ignore = "a pty test: the REPL host on macOS and Linux (arm64, x86_64)"
)]
#[cfg(unix)]
// spec: §9.7.8
fn repl_edits_lines_on_a_terminal() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let home = scratch("repl-line-edit");
    let steps: [&[u8]; 7] = [
        b"let x = 4\r",
        b"\x1b[200~fn f(a: i32) -> i32 {\r\n    a * 2\r\n}\r\n\x1b[201~\r",
        b"f(x)\r",
        b"prin\tln(f(1))\r",
        b"\x1b[A\r",
        b"\x12f(x\r",
        b":exit\r",
    ];
    let out = on_tty(&["repl", "--rokr", &rokr], &home, &steps);
    let banner = format!(
        "{}\r\nType :help for help, :exit to quit.\r\n",
        toolchain_line()
    );
    assert!(out.starts_with(&banner), "{out:?}");
    let lines: Vec<&str> = out.split("\r\n").collect();
    let shown = |v: &str| {
        let lines = lines
            .iter()
            .map(|l| l.strip_prefix("\x1b[?2004l").unwrap_or(l));
        lines.filter(|l| *l == v).count()
    };
    assert_eq!((shown("8"), shown("2")), (2, 2), "{out:?}");
    // The pasted cell is shown as its lines, coloured.
    assert!(out.contains("\n... }\x1b[K"), "{out:?}");
    assert!(out.contains("\x1b[95mfn\x1b[0m"), "{out:?}");
    let history = std::fs::read_to_string(home.join("repl_history")).unwrap();
    assert_eq!(
        history,
        "#rok repl_history 2\nlet x = 4\nfn f(a: i32) -> i32 {\\n    a * 2\\n}\nf(x)\n\
         println(f(1))\nf(x)\n:exit\n"
    );
}

/// `-q` leaves the toolchain line and the hint out.
#[test]
#[cfg_attr(
    not(all(
        any(target_os = "macos", target_os = "linux"),
        any(target_arch = "aarch64", target_arch = "x86_64")
    )),
    ignore = "a pty test: the REPL host on macOS and Linux (arm64, x86_64)"
)]
#[cfg(unix)]
// spec: §9.7.8
fn repl_quiet_starts_without_a_banner() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let home = scratch("repl-quiet");
    let out = on_tty(&["repl", "-q", "--rokr", &rokr], &home, &[b"quit\r"]);
    assert!(!out.contains("Rok ") && !out.contains(":help"), "{out:?}");
}

/// After output that does not end its line the prompt starts a fresh
/// line, with no marker drawn.
#[test]
#[cfg_attr(
    not(all(
        any(target_os = "macos", target_os = "linux"),
        any(target_arch = "aarch64", target_arch = "x86_64")
    )),
    ignore = "a pty test: the REPL host on macOS and Linux (arm64, x86_64)"
)]
#[cfg(unix)]
// spec: §9.7.8
fn repl_prompt_after_unended_output_has_no_marker() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let home = scratch("repl-fresh-line");
    let steps: [&[u8]; 2] = [b"print(\"ab\")\r", b"quit\r"];
    let out = on_tty(&["repl", "-q", "--rokr", &rokr], &home, &steps);
    assert!(out.contains("ab "), "{out:?}");
    assert!(!out.contains('%') && !out.contains("\x1b[7m"), "{out:?}");
}

/// A cell ends by tokens: brackets in char literals and comments do not
/// count, a balanced line ends the cell (a last `let` needs no `;`), and
/// an item header or a trailing operator asks for the next line.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.8
fn repl_cells_end_by_tokens() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
'('
'\"'
/* ( */ 1
1 + 1 /* c */
fn g(x: i32)
    -> i32 { x * 2 }
g(4)
let x = 1
2 + 2
x
let y = 1 +
    2 *
    3 // no `;`
y
y += 2
y
let zs = [1, 2]
zs[0] -= 1
zs?
{ let q = 2; q * 3 }
if y > 1 { 1 } else { 2 }
match y { 9 => 10, _ => 0 }
// done
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(
        text(&out.stdout),
        "(\n\"\n1\n2\n8\n4\n1\n7\n9\n[0, 2]\n6\n1\n10\n",
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("error"), "{stderr}");
}

/// Errors quote a cell as typed, without the `;` the REPL supplies.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.5 §9.7.8
fn repl_quotes_cells_as_typed() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let out = piped(ROKC, &["repl", "--rokr", &rokr], "let a = 1 a\nk += 1\n");
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("expected `;`, found identifier `a`") && stderr.contains("| let a = 1 a\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains("cannot find value `k`") && stderr.contains("| k += 1\n"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("1 a;") && !stderr.contains("+= 1;"),
        "{stderr}"
    );
}

/// A cell of several statements shows its last one's value.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.1 §9.7.7 §9.7.8
fn repl_cell_value_is_its_last_statement() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
println(\"x\"); while true { break; }
let a = 1; a + 1
println(\"y\"); let b = a + 2;
b
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(text(&out.stdout), "x\n2\ny\n3\n", "stderr: {stderr}");
}

/// `repl <lib>` (9.1.2): the library is loaded and its items
/// are in scope from the first cell, also after `:reset`.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.1.2 §9.7.10 §9.9.9
fn repl_loads_and_uses_a_library() {
    // The library is archived by LLVM.
    let Some(env) = setup() else { return };
    let rokr = env.rokr.display().to_string();
    let dir = scratch("repl-pkg");
    let lib = dir.join("geom");
    std::fs::create_dir_all(lib.join("src")).unwrap();
    std::fs::write(
        lib.join("src/lib.rok"),
        "pub fn twice(x: i32) -> i32 { x * 2 }\n",
    )
    .unwrap();
    let pkg = lib.display().to_string();
    let out = piped(
        ROKC,
        &["repl", "--rokr", &rokr, &pkg],
        "twice(21)\n:reset\ntwice(2)\n",
    );
    let stdout = text(&out.stdout);
    assert_eq!(stdout, "42\n4\n", "{}", text(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A static library's `dllimport` call (through `__imp_<name>`) reaches
/// the DLL function.
#[test]
#[cfg(windows)]
// spec: §9.7.10
fn repl_static_library_calls_a_dll_import() {
    let Some(env) = setup() else { return };
    let rokr = env.rokr.display().to_string();
    let dir = scratch("repl_imp");
    common::put(
        dir.join("imp.c"),
        "__declspec(dllimport) unsigned long GetCurrentProcessId(void);\n\
         int imp_pid(void) { return GetCurrentProcessId() != 0; }\n",
    )
    .unwrap();
    for (exe, args) in [
        ("clang", &["-c", "imp.c", "-o", "imp.o"][..]),
        ("lib", &["/nologo", "/OUT:imp.lib", "imp.o"]),
    ] {
        let out = Command::new(exe)
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{exe}: {}", text(&out.stderr));
    }
    let lib_dir = dir.display().to_string();
    let out = piped(
        ROKC,
        &["repl", "--rokr", &rokr, "-L", &lib_dir, "-l", "static=imp"],
        "extern \"C\" { fn imp_pid() -> i32; }\nunsafe { imp_pid() }\n",
    );
    assert_eq!(text(&out.stdout), "1\n", "{}", text(&out.stderr));
}

/// A cell runs once, in one process: its side effects
/// do not repeat, its bindings / items stay, and a cell that dies
/// leaves the session as it was.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.6.5 §9.7.0 §9.7.2
fn repl_runs_each_cell_once() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let log = scratch("repl-once").join("log");
    let log_s = log.display().to_string().replace('\\', "/");
    let _ = std::fs::remove_file(&log);
    // Cell 1 appends to `log`: a rerun would append again.
    let input = format!(
        "\
println(\"once\"); let old = try fs::read_to_str(\"{log_s}\") catch _ {{ \"\" }}; try fs::write(\"{log_s}\", old + \"x\") catch _ {{ }};
let n = 1i32;
#[derive(Debug)]
struct P {{ x: i32; }}
fn twice(p: P) -> i32 {{ p.x * 2 }}
let p = P {{ x: 21 }};
n += 1;
n += 100; panic(\"boom\");
n
twice(p)
p
"
    );
    let out = piped(
        ROKC,
        &["repl", "--rokr", &rokr],
        &format!("use rok::std::fs;\n{input}"),
    );
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    assert_eq!(stdout, "once\n2\n42\nP { x: 21 }\n", "stderr: {stderr}");
    assert!(stderr.contains("boom"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&log).ok().as_deref(), Some("x"));
    let _ = std::fs::remove_file(&log);
}

/// A binding a later cell could not use is refused in the cell that
/// makes it; one a later cell can name is kept,
/// `any` values too (their methods and type tests work in later cells).
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.0 §9.7.3
fn repl_refuses_bindings_later_cells_cannot_use() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
mod geo { struct Hid { x: i32; } pub fn mk() -> Hid { Hid { x: 1 } } pub struct Pt { pub x: i32; } }
let h = geo::mk();
let d: any Debug = 5i32;
let pt = geo::Pt { x: 3 };
pt.x
#[derive(Debug)]
struct Q { v: i32; }
let q: any Debug = Q { v: 9 };
d
q
match q { x: Q => x.v, _ => 0 }
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    // An `any Debug` shows the `Debug` of what it holds (6.10.3).
    assert_eq!(stdout, "3\n5\nQ { v: 9 }\n9\n", "stderr: {stderr}");
    assert!(
        stderr.contains("`h: Hid` cannot be kept for later cells (`geo::Hid` is private"),
        "{stderr}"
    );
}

/// `static` items keep their state across cells (a redefinition starts
/// it over); cells are JIT-compiled.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.6.1 §9.6.2 §9.7.0
fn repl_statics_persist() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
static N: Atomic<i32> = Atomic::new(0);
fn bump() -> i32 { return N.fetch_add(1) + 1; }
bump()
bump()
N.load()
static N: Atomic<i32> = Atomic::new(100);
bump()
bump()
";
    let out = piped_env(
        ROKC,
        &["repl", "--rokr", &rokr],
        input,
        &[("ROK_REPL_TIME", "1")],
    );
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "1\n2\n2\n101\n102\n", "stderr: {stderr}");
    assert!(stderr.contains("[repl] jit + run"), "{stderr}");
}

/// Library code and unchanged items compile once per session; a
/// redefinition takes effect from the next cell, also in older code.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.6.3 §9.6.4 §9.7.6
fn repl_compiles_library_code_once() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
let l = [1, 2, 3];
l.len()
fn f() -> i32 { 1 }
fn g() -> i32 { f() + 10 }
g()
fn f() -> i32 { 2 }
f()
g()
";
    let out = piped_env(
        ROKC,
        &["repl", "--rokr", &rokr],
        input,
        &[("ROK_REPL_TIME", "1")],
    );
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "3\n11\n2\n12\n", "stderr: {stderr}");
    let counts: Vec<(usize, usize, usize)> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("[repl] functions: "))
        .filter_map(|l| {
            let (new, old) = l.split_once(" compiled, ")?;
            let (old, bytes) = old.split_once(" from earlier cells; object: ")?;
            let bytes = bytes.strip_suffix(" bytes")?;
            Some((new.parse().ok()?, old.parse().ok()?, bytes.parse().ok()?))
        })
        .collect();
    assert_eq!(counts.len(), 5, "{stderr}");
    // `l.len()`: `List`'s methods come from the first cell, and its
    // object holds only its own code (no copies of what it does not
    // call, no symbols it does not use).
    assert!(
        counts[0].0 > 10 && counts[1].0 <= 3 && counts[1].1 > 10,
        "{stderr}"
    );
    assert!(counts[1].2 < 4096, "{stderr}");
}

/// The library is collected (or its stage read back) once per session:
/// later cells take the stage the first one kept, whatever their text.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.2.16 §9.7.1
fn repl_collects_the_library_once() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let out = piped_env(
        ROKC,
        &["repl", "--rokr", &rokr],
        "let a = 1;\na + 1\nlet b = a * 3;\nb\n",
        &[("ROK_TIME_PASSES", "1")],
    );
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "2\n3\n", "stderr: {stderr}");
    // Looked up in the store by the first cell only.
    let reads = stderr
        .lines()
        .filter(|l| l.ends_with("hir library read"))
        .count();
    assert_eq!(reads, 1, "{stderr}");
}

/// `repl <path>` runs the program in the session; a failure keeps the
/// bindings made before the failing cell.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §0.1.4 §9.7.5 §9.7.10
fn repl_path_keeps_the_programs_bindings() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let dir = scratch("run_i");
    let prog = dir.join("prog.rok");
    std::fs::write(
        &prog,
        "\
mod geo { struct Hid { x: i32; } pub fn mk() -> Hid { Hid { x: 1 } } pub fn get(h: Hid) -> i32 { h.x } }
#[derive(Debug)]
struct P { x: i32; }
let n = 1i32;
let h = geo::mk();
let m = geo::get(h) + n;
fn twice(p: P) -> i32 { p.x * 2 }
let p = P { x: m };
let xs = [1, 2, 3];
println(f\"ran {twice(p)}\");
n += 100;
let bad = xs[m + 5];
let never = 0;
",
    )
    .unwrap();
    let path = prog.display().to_string();
    let input = "n\n:type p\ntwice(p) + m\nxs\nnever\n";
    let out = piped(ROKC, &["repl", "--rokr", &rokr, &path], input);
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    // `n += 100` ran in the cell that failed: `n` is as before it.
    assert_eq!(stdout, "ran 4\n1\nP\n6\n[1, 2, 3]\n", "stderr: {stderr}");
    assert!(stderr.contains(&format!("{path}:12:11")), "{stderr}");
    assert!(
        stderr.contains("the rest of the program did not run"),
        "{stderr}"
    );
    assert!(stderr.contains("cannot find value `never`"), "{stderr}");
    // `h`'s type is private to `geo`: not kept, and nothing after names it.
    assert!(stderr.contains("`h: Hid` cannot be kept"), "{stderr}");

    // `fn main`: it runs; the items stay, its locals do not.
    std::fs::write(
        &prog,
        "fn helper(x: i32) -> i32 { x * 2 }\nfn main() {\n    let local = helper(21);\n    println(f\"main {local}\");\n}\n",
    )
    .unwrap();
    let out = piped(
        ROKC,
        &["repl", "--rokr", &rokr, &path],
        "helper(5)\nlocal\n",
    );
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "main 42\n10\n", "stderr: {stderr}");
    assert!(stderr.contains("cannot find value `local`"), "{stderr}");

    // A program that does not compile runs nothing; its errors point
    // into it, and the session starts empty.
    std::fs::write(&prog, "println(\"no\");\nlet y = nope + 1;\n").unwrap();
    let out = piped(ROKC, &["repl", "--rokr", &rokr, &path], "1 + 1\n");
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "2\n", "stderr: {stderr}");
    assert!(stderr.contains(&format!("{path}:2:9")), "{stderr}");

    // A package: its `mod` files resolve.
    let pkg = dir.join("pkg");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("src/main.rok"),
        "mod util;\nlet k = util::three();\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/util.rok"), "pub fn three() -> i32 { 3 }\n").unwrap();
    let pkg_s = pkg.display().to_string();
    let out = piped(
        ROKC,
        &["repl", "--rokr", &rokr, &pkg_s],
        "k + util::three()\n",
    );
    assert_eq!(text(&out.stdout), "6\n", "{}", text(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A failing cell is undone: its definitions, its writes to earlier
/// objects and statics, and the thread it started are gone.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.6.5 §9.7.0 §9.7.5
fn repl_survives_failed_cells() {
    let Some(env) = setup() else { return };
    let rokr = env.rokr.display().to_string();
    let input = "\
use rok::bare::ptr::Ptr;
let kept = 7;
fn peek() -> i64 { let p: Ptr<i64> = Ptr::dangling(); unsafe { p.read() } }
peek()
kept + 1
fn down(n: i32) -> i32 { if n == 0 { panic(\"deep\") } else { down(n - 1) + 1 } }
down(50)
kept + 2
rok::std::process::exit(3);
fn f() -> i32 { 1 }
f()
fn f() -> i32 { panic(\"v2\") }
f()
fn f() -> i32 { 1 }
f()
kept + 3
use rok::std::thread;
static C: Atomic<i64> = Atomic::new(0);
let xs = [1, 2, 3];
C.store(1);
xs.push(4); C.store(5); thread::spawn(|| { while true { C.fetch_add(1); } }); thread::sleep(Duration::ms(50)); panic(\"after\");
xs
C.load()
thread::sleep(Duration::ms(200));
C.load()
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stderr = text(&out.stderr);
    assert_eq!(
        text(&out.stdout),
        "8\n9\n1\n1\n10\n[1, 2, 3]\n1\n1\n",
        "stderr: {stderr}"
    );
    let failed = |why: &str| {
        stderr
            .matches(&format!("error: {why}; the session is unchanged"))
            .count()
    };
    assert_eq!(failed("the cell failed"), 1, "{stderr}");
    assert_eq!(failed("the cell panicked"), 3, "{stderr}");
    assert_eq!(failed("the cell called exit(3)"), 1, "{stderr}");
    assert!(stderr.contains("deep") && stderr.contains("v2"), "{stderr}");
}

/// Executor threads survive a failed cell; a thread running when it
/// failed is gone.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.0
fn repl_threads_after_a_failed_cell() {
    let Some(env) = setup() else { return };
    let rokr = env.rokr.display().to_string();
    let input = "\
use rok::core::task;
use rok::std::thread;
task::block_on(async { let t = task::spawn(async { 20 + 1 }); await t })
task::block_on(async { await task::spawn_blocking(|| 7) })
task::block_on(async { let t = task::spawn(async { 5 }); await t }); panic(\"in the middle\");
task::block_on(async { let t = task::spawn(async { 40 + 2 }); await t })
task::block_on(async { await task::spawn_blocking(|| 8) })
let h = thread::spawn(|| { let n: i64 = 0; while n >= 0 { n += 1; } n });
thread::sleep(Duration::ms(100)); panic(\"busy\");
let j = try h.join() catch e { println(f\"{e}\"); -1i64 };
j
task::block_on(async { let ts = [task::spawn(async { 1 }), task::spawn(async { 2 })]; let s = 0; for t in ts { s += await t; } s })
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    assert_eq!(
        stdout,
        "21\n7\n42\n8\nthread join failed: the thread ended with a failed REPL cell\n-1\n3\n",
        "stderr: {stderr}"
    );
    assert_eq!(
        stderr.matches("error: the cell panicked").count(),
        2,
        "{stderr}"
    );
}

/// A failed cell loses nothing that ran on the session's threads, and the
/// locks of a thread gone afterwards are released.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.0
fn repl_failed_cell_loses_no_work_and_no_lock() {
    let Some(env) = setup() else { return };
    let rokr = env.rokr.display().to_string();
    let input = "\
use rok::core::sync::Once;
use rok::core::task;
use rok::core::time::Instant;
use rok::std::thread;
static POLLED: Atomic<i64> = Atomic::new(0);
static JOB: Atomic<i64> = Atomic::new(0);
fn spin(ms: i64) { let t = Instant::now(); while t.elapsed().as_ms() < ms {} }
let t = task::block_on(async { task::spawn(async { spin(1500); POLLED.store(1); 1 }) }); let b = task::block_on(async { task::spawn_blocking(|| { spin(1500); JOB.store(1); 2 }) });
panic(\"at once\");
(POLLED.load(), JOB.load())
task::block_on(async { (await t) + (await b) })
static IN: Atomic<i64> = Atomic::new(0);
static GO: Atomic<i64> = Atomic::new(0);
let m = Mutex::new(5); let rw = RwLock::new(6); let once = Once::new();
let h = thread::spawn(|| { using g = m.lock() { using r = rw.read() { once.call_once(|| { IN.store(1); while GO.load() == 0 { thread::park(); } }); } } });
while IN.load() == 0 { thread::sleep(Duration::ms(5)); } thread::sleep(Duration::ms(100));
panic(\"holder gone\");
GO.store(1); h.thread().unpark();
using g = m.lock() { g.get() }
using w = rw.write() { w.get() }
once.call_once(|| { println(\"ran again\"); });
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    // The holder sits in `call_once` with both locks until `GO`, parked
    // in one wait so a rollback finds it where the checkpoint did (a
    // thread that ran since counts as gone). Windows keeps that thread,
    // which then lets them go; a fork host has none.
    let again = if cfg!(windows) { "" } else { "ran again\n" };
    assert_eq!(
        stdout,
        format!("(1, 1)\n3\n5\n6\n{again}"),
        "stderr: {stderr}"
    );
    assert_eq!(
        stderr.matches("error: the cell panicked").count(),
        2,
        "{stderr}"
    );
}

fn frame(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

// spec: §9.1.2 §9.9.13
#[test]
fn rokc_lsp_answers_over_stdio() {
    let input = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#,
        r#"{"jsonrpc":"2.0","method":"exit"}"#,
    ]
    .map(frame)
    .concat();
    // (`rokls` is `rok lsp`: the `rok` crate tests it.)
    {
        let out = piped(ROKC, &["lsp"], &input);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        let stdout = text(&out.stdout);
        assert!(stdout.starts_with("Content-Length: "), "{stdout}");
        assert!(stdout.contains(r#""hoverProvider":true"#), "{stdout}");
        assert!(stdout.contains(r#""id":2,"result":null"#), "{stdout}");
    }
    // `exit` without `shutdown` is exit code 1 (LSP spec).
    let out = piped(
        ROKC,
        &["lsp"],
        &frame(r#"{"jsonrpc":"2.0","method":"exit"}"#),
    );
    assert_eq!(out.status.code(), Some(1));
}

/// Input that ends before `initialize` (the server started by hand) is
/// named on stderr, not a bare exit 1.
// spec: §9.9.13
#[test]
fn rokls_eof_before_initialize_says_so() {
    let out = piped(ROKC, &["lsp"], "");
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(
        err.contains("input ended before the editor sent `initialize`"),
        "{err}"
    );
}

/// The `rok.add` command of a quick fix runs `$ROK add <name>` in the
/// package directory; any other command, or a flag, is refused.
// spec: §9.9.13
#[cfg(unix)]
#[test]
fn lsp_rok_add_command_runs_rok() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("lsp-rok-add");
    let app = dir.join("app");
    std::fs::create_dir_all(app.join("src")).unwrap();
    let main = app.join("src/main.rok");
    std::fs::write(&main, "use json::parse;\n").unwrap();
    let log = dir.join("log");
    let stub = dir.join("rok");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\necho \"$PWD $*\" > '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let uri = rokc_tools::lsp::path_to_uri(&main.display().to_string(), cfg!(windows));
    let run = |id: u32, command: &str, args: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"workspace/executeCommand","params":{{"command":"{command}","arguments":["{uri}"{args}]}}}}"#
        )
    };
    let input = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#.to_owned(),
        run(2, "rok.add", r#","json""#),
        run(3, "rok.remove", r#","json""#),
        run(4, "rok.add", r#","--path""#),
        r#"{"jsonrpc":"2.0","id":5,"method":"shutdown"}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"exit"}"#.to_owned(),
    ]
    .map(|m| frame(&m))
    .concat();
    let stub = stub.display().to_string();
    let out = piped_env(ROKC, &["lsp"], &input, &[("__ROK", &stub)]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains(r#""id":2,"result":null"#), "{stdout}");
    assert!(stdout.contains(r#""id":3,"error""#), "{stdout}");
    assert!(stdout.contains(r#""id":4,"error""#), "{stdout}");
    let ran = std::fs::read_to_string(&log).unwrap();
    let real = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
    let (cwd, args) = ran.trim_end().split_once(' ').unwrap();
    assert_eq!(real(cwd.as_ref()), real(&app), "{ran}");
    assert_eq!(args, "add json");
}

/// The server builds its dependencies first and answers from their ROKHIR:
/// hover shows a dependency function's doc comment, definition goes to
/// its source.
// spec: §9.9.13
#[test]
fn lsp_sees_prebuilt_dependencies() {
    let Some(env) = setup() else { return };
    let dir = scratch("lsp-deps");
    let (util, app) = (dir.join("util"), dir.join("app"));
    std::fs::create_dir_all(util.join("src")).unwrap();
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::write(
        util.join("src/lib.rok"),
        "/// Says hi.\npub fn hello() -> i32 {\n    return 1;\n}\n",
    )
    .unwrap();
    let main = app.join("src/main.rok");
    let src = "use util::hello;\nlet x = hello();\nprintln(f\"{x}\");\n";
    std::fs::write(&main, src).unwrap();
    let uri = rokc_tools::lsp::path_to_uri(&main.display().to_string(), cfg!(windows));
    let doc = format!(r#"{{"uri":"{uri}","languageId":"rok","version":1,"text":{src:?}}}"#);
    let at =
        format!(r#"{{"textDocument":{{"uri":"{uri}"}},"position":{{"line":1,"character":10}}}}"#);
    let input = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#.to_owned(),
        format!(r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{doc}}}}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{at}}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":3,"method":"textDocument/definition","params":{at}}}"#),
        r#"{"jsonrpc":"2.0","id":4,"method":"shutdown"}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"exit"}"#.to_owned(),
    ]
    .map(|m| frame(&m))
    .concat();
    let ext = format!("util={}", util.display());
    let mut child = Command::new(ROKC)
        .current_dir(&app)
        .env("ROK_HOME", workspace())
        .args(["lsp", "--extern", &ext, "--rokr"])
        .arg(&env.rokr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(app.join("target/debug/deps/util.rokhir").is_file());
    let stdout = text(&out.stdout);
    let reply = |id: u32| {
        let at = stdout
            .find(&format!(r#""id":{id},"#))
            .unwrap_or_else(|| panic!("{stdout}"));
        stdout[at..]
            .split("Content-Length")
            .next()
            .unwrap()
            .to_owned()
    };
    assert!(reply(2).contains("Says hi."), "{stdout}");
    let def = reply(3);
    let want = rokc_tools::lsp::path_to_uri(
        &util.join("src/lib.rok").display().to_string(),
        cfg!(windows),
    );
    assert!(def.contains(&want), "{def}");
    assert!(def.contains(r#""start":{"line":1,"#), "{def}");
}

/// An internal compiler error (`ROK_TEST_ICE`, a debug rokc's) in an
/// analysis is that buffer's diagnostic; the server keeps serving.
// spec: §9.1.6 §9.7.5
#[test]
fn lsp_survives_a_compiler_panic() {
    let dir = scratch("lsp-ice");
    let main = dir.join("main.rok");
    let uri = rokc_tools::lsp::path_to_uri(&main.display().to_string(), cfg!(windows));
    let mut child = Command::new(ROKC)
        .current_dir(&dir)
        .env("ROK_HOME", workspace())
        .env("ROK_TEST_ICE", "ice-here")
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let send = |stdin: &mut std::process::ChildStdin, m: String| {
        stdin.write_all(frame(&m).as_bytes()).unwrap();
    };
    // The next message whose text contains `want`.
    let mut next = |want: &str| loop {
        let mut len = 0;
        loop {
            let mut line = String::new();
            assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "server gone");
            match line.trim_end().strip_prefix("Content-Length: ") {
                Some(n) => len = n.parse().unwrap(),
                None if line.trim_end().is_empty() => break,
                None => {}
            }
        }
        let mut body = vec![0; len];
        stdout.read_exact(&mut body).unwrap();
        let body = String::from_utf8(body).unwrap();
        if body.contains(want) {
            break body;
        }
    };
    let good = "let x = 1;\nprintln(f\"{x}\");\n";
    let bad = format!("{good}// ice-here\n");
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#.into(),
    );
    send(
        &mut stdin,
        format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{uri}","languageId":"rok","version":1,"text":{bad:?}}}}}}}"#
        ),
    );
    let diags = next("publishDiagnostics");
    assert!(
        diags.contains("internal compiler error") && diags.contains("ROK_TEST_ICE"),
        "{diags}"
    );
    send(
        &mut stdin,
        format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{uri}","version":2}},"contentChanges":[{{"text":{good:?}}}]}}}}"#
        ),
    );
    let diags = next("publishDiagnostics");
    assert!(diags.contains(r#""diagnostics":[]"#), "{diags}");
    send(
        &mut stdin,
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{{"textDocument":{{"uri":"{uri}"}},"position":{{"line":1,"character":11}}}}}}"#
        ),
    );
    let hover = next(r#""id":2"#);
    assert!(hover.contains("i32"), "{hover}");
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":3,"method":"shutdown"}"#.into(),
    );
    send(&mut stdin, r#"{"jsonrpc":"2.0","method":"exit"}"#.into());
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

/// A compiler panic on a cell fails that cell; the session goes on.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.1.6 §9.7.5
fn repl_survives_a_compiler_panic() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let out = piped_env(
        ROKC,
        &["repl", "--rokr", &rokr],
        "let x = 40;\nx + 1 // ice-here\nx + 2\n",
        &[("ROK_TEST_ICE", "ice-here")],
    );
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "42\n", "{stderr}");
    assert!(
        stderr.contains("internal compiler error") && stderr.contains("ROK_TEST_ICE"),
        "{stderr}"
    );
    assert_eq!(out.status.code(), Some(0), "{stderr}");
}

// spec: §9.1.9
#[test]
fn check_reads_stdin() {
    let out = piped(
        ROKC,
        &["-q", "check", "-"],
        "let x: i32 = 1;\nprintln(f\"{x}\");\n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = piped(ROKC, &["-q", "check", "-"], "let x: i32 = true;\n");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("<stdin>:1:"),
        "{}",
        text(&out.stderr)
    );
}

/// How rokc ended once its stdout (`out`) or stderr lost its reader
/// before `input` reached its stdin, and what it wrote to the other one
/// (for a failure's message).
///
/// Unix: the closed stream is a pipe made in the child with its read end
/// closed there. A pipe made here could have its read end inherited by a
/// process another test spawns meanwhile (macOS std sets `FD_CLOEXEC`
/// after `pipe`), and rokc's writes would then succeed.
fn closed_output(args: &[&str], out: bool, input: &str) -> Closed {
    let mut cmd = Command::new(ROKC);
    cmd.args(args)
        .env("ROK_HOME", workspace())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        let fd = if out { 1 } else { 2 };
        if out {
            cmd.stdout(Stdio::null());
        } else {
            cmd.stderr(Stdio::null());
        }
        // SAFETY: pipe / close / dup2 are async-signal-safe.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, move || {
                let mut p = [0; 2];
                if libc::pipe(p.as_mut_ptr()) != 0 || libc::dup2(p[1], fd) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(p[0]);
                libc::close(p[1]);
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().unwrap();
    let kept = if out {
        drop(child.stdout.take());
        child.stderr.take().map(read_all)
    } else {
        drop(child.stderr.take());
        child.stdout.take().map(read_all)
    };
    // rokc may be gone before it read everything.
    drop(child.stdin.take().unwrap().write_all(input.as_bytes()));
    let status = child.wait().unwrap();
    let other = kept.map(|t| t.join().unwrap()).unwrap_or_default();
    Closed { status, other }
}

/// `closed_output`'s result; shown as the status and the other stream.
struct Closed {
    status: std::process::ExitStatus,
    other: String,
}

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}, other stream: {:?}", self.status, self.other)
    }
}

fn read_all(mut r: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    })
}

/// Whether `status` is how a program ends writing to a closed pipe:
/// killed by SIGPIPE (Windows: std drops the writes, and these `check`s
/// end with their errors' exit 1), not a panic or abort.
fn ended_by_closed_pipe(c: &Closed) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::process::ExitStatusExt::signal(&c.status) == Some(13)
    }
    #[cfg(not(unix))]
    {
        c.status.code() == Some(1)
    }
}

/// A closed stdout / stderr ends rokc quietly (`rokc .. | head`), not
/// as an internal error (101) or an abort in the panic hook.
#[test]
// spec: §9.1.6 §9.1.9
fn closed_output_ends_rokc_quietly() {
    let errors = "let x: i32 = true;\n".repeat(2000);
    let st = closed_output(&["check", "-"], false, &errors);
    assert!(ended_by_closed_pipe(&st), "check, stderr closed: {st}");
    let st = closed_output(&["--message-format=json", "check", "-"], true, &errors);
    assert!(ended_by_closed_pipe(&st), "check json, stdout closed: {st}");
}

#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.1.6 §9.7.5
fn repl_closed_output_ends_quietly() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let args = ["repl", "--rokr", &rokr];
    let st = closed_output(&args, false, "let a = 1; let b: str = 2;\n");
    assert!(ended_by_closed_pipe(&st), "stderr closed: {st}");
    let st = closed_output(&args, true, "1 + 2\n");
    assert!(ended_by_closed_pipe(&st), "stdout closed: {st}");
}

/// Errors in a cell point into its text.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.1.8 §9.7.5 §9.7.7
fn repl_errors_point_into_the_cell() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
let a = 1; let b: str = 2;
let é = \"é\"; let w = 3; w + true
fn f() -> i64 { 1 }
fn f() -> str { \"x\" }
let p = (1 +
  2); println(p); f() + p
a + 1
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(text(&out.stdout), "", "{stderr}");
    assert_eq!(
        stderr,
        "\
error[E0040]: mismatched types: `let b` expects `str`, found `i32`
  --> <repl>:1:25
   |
 1 | let a = 1; let b: str = 2;
   |                         ^
   |
   = help: to make text from it, write `f\"{2}\"`
   = help: or, to keep the `i32`, drop `: str`
error[E0054]: operator `+` cannot be applied to `i32` and `bool`
  --> <repl>:1:25
   |
 1 | let é = \"é\"; let w = 3; w + true
   |                         ^^^^^^^^
error[E0040]: mismatched types: the right operand of `+` expects `str`, found `i32`
  --> <repl>:2:25
   |
 2 |   2); println(p); f() + p
   |                         ^
   |
   = help: `+` joins two `str`s; put the value in an f-string: `f\"{f()}{p}\"`
error[E0027]: cannot find value `a` in this scope
  --> <repl>:1:1
   |
 1 | a + 1
   | ^
"
    );
}

/// A cell with a top-level `await` keeps its bindings and shows its
/// value; `it` holds the last value shown, and the user's `it` wins.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §4.16.5 §9.7.0 §9.7.1 §9.7.7
fn repl_await_cells_and_it() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
async fn af() -> i32 { return 3; }
let w = 10;
let v = await af(); println(f\"got {v}\");
v + w
await af()
it + 1
it
xs
let it = 7;
5
it
:reset
fn it() -> i32 { 9 }
2
it()
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(
        stdout, "got 3\n13\n3\n4\n4\n5\n7\n2\n9\n",
        "stderr: {stderr}"
    );
    // A failed cell leaves `it` alone.
    assert!(stderr.contains("cannot find value `xs`"), "{stderr}");
}

/// `exit` / `quit` alone end the session unless the session defines
/// the name.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.9.9
fn repl_exit_words_end_the_session() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let args = ["repl", "--rokr", &rokr];
    for (input, want) in [
        ("1\nexit\n2\n", "1\n"),
        ("quit()\n2\n", ""),
        ("let exit = 5;\nexit\nquit\n2\n", "5\n"),
        ("fn quit() -> i32 { 6 }\nquit()\n:q\n2\n", "6\n"),
    ] {
        let out = piped(ROKC, &args, input);
        let stderr = text(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{stderr}");
        assert_eq!(text(&out.stdout), want, "{input}: {stderr}");
    }
}

/// `:doc`, `:time`, `:vars`, `:load`, `:reload`, `:opt`, `:save` (read back
/// with `repl <file>`) and the commands' usage errors.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.7 §9.7.10 §9.9.9
fn repl_session_commands() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let dir = scratch("repl-commands");
    std::fs::create_dir_all(&dir).unwrap();
    let prog = dir.join("p.rok");
    std::fs::write(
        &prog,
        "let base = 40i32;\nfn add(a: i32, b: i32) -> i32 { a + b }\nprintln(\"loaded\");\n",
    )
    .unwrap();
    let saved = dir.join("s.rok");
    let (prog_s, saved_s) = (prog.display().to_string(), saved.display().to_string());
    let input = format!(
        "\
/// Doubles it.
fn twice(x: i32) -> i32 {{ x * 2 }}
:doc twice
:time twice(4)
:load {prog_s}
add(base, 2)
:reload
let xs = [1, 2];
xs.push(3)
it
it * 2
let xs = \"s\"
:vars
:opt
:opt 9
:opt 0
:save {saved_s}
:doc
:load
:load {dir}
:frob
:help
",
        dir = dir.display()
    );
    let out = piped(ROKC, &["repl", "--rokr", &rokr], &input);
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let help = stdout.find("  <expr> ").unwrap_or(stdout.len());
    assert_eq!(
        &stdout[..help],
        "\
fn twice(x: i32) -> i32

Doubles it.
8
loaded
42
loaded
42
84
base: i32
it:   i32
xs:   str
fn twice(x: i32) -> i32
fn add(a: i32, b: i32) -> i32
-O0
",
        "stderr: {stderr}"
    );
    for cmd in [
        ":doc <name>",
        ":time <expr>",
        ":vars",
        ":load <file>",
        ":reload",
        ":edit",
        ":save <file>",
        ":opt",
        ":add <name>[@<req>]",
        ":type <expr>, :t <expr>",
        ":help, :h",
        ":exit, :quit, :q, exit, quit",
    ] {
        assert!(stdout.contains(cmd), "{cmd}: {stdout}");
    }
    for e in [
        "[repl] front end: ",
        "[repl] total: ",
        "`:opt` takes 0, 1, 2, 3 or s",
        "`:doc` takes a name: `:doc xs.len`",
        "`:load` takes a file: `:load prog.rok`",
        "`:load` takes a `.rok` file; for a package directory start `rok repl",
        "unknown command `:frob`",
    ] {
        assert!(stderr.contains(e), "{e}: {stderr}");
    }
    // The script declares each name once per scope; read back, it runs
    // the cells again, one by one: `xs` is the last one declared.
    let script = std::fs::read_to_string(&saved).unwrap();
    assert!(script.contains("\nbase = 40i32;\n"), "{script}");
    assert!(script.contains("\nlet it = add(base, 2);\n"), "{script}");
    assert!(
        script.contains("\n{\n// :cell\nlet xs = \"s\";\n}\n"),
        "{script}"
    );
    let out = piped(
        ROKC,
        &["repl", "--rokr", &rokr, &saved_s],
        "base + twice(1)\nit\nxs\n",
    );
    let stderr = text(&out.stderr);
    assert_eq!(
        text(&out.stdout),
        "loaded\nloaded\n42\n42\ns\n",
        "{script}\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `:load` of a `:save`d session replays its cells: a name declared
/// again with another type keeps its last binding.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.9.9
fn load_of_saved_session_keeps_redeclared_binding() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let dir = scratch("repl-save-load");
    std::fs::create_dir_all(&dir).unwrap();
    let saved = dir.join("s.rok").display().to_string();
    let input = format!("let a = 1\nlet a = \"s\"\n:save {saved}\n:reset\n:load {saved}\na\n");
    let out = piped(ROKC, &["repl", "--rokr", &rokr], &input);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(text(&out.stdout), "s\n", "{stderr}");
    // `rok run` of the script still works: a block holds the new `a`.
    let out = piped(ROKC, &["run", "-q", "--rokr", &rokr, &saved], "");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `:edit` runs what the editor leaves in its file.
#[test]
#[cfg_attr(
    not(all(
        any(target_os = "macos", target_os = "linux"),
        any(target_arch = "aarch64", target_arch = "x86_64")
    )),
    ignore = "the editor is `cp`: macOS and Linux, arm64 and x86_64"
)]
// spec: §9.9.9
fn repl_edit_runs_the_editors_cells() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let dir = scratch("repl-edit");
    std::fs::create_dir_all(&dir).unwrap();
    let cells = dir.join("cells.rok");
    std::fs::write(&cells, "fn g() -> i32 { 5 }\nlet b = g() + 1\nb * 2\n").unwrap();
    // An editor that writes the cells into the file it is given.
    let editor = format!("cp {}", cells.display());
    let out = piped_env(
        ROKC,
        &["repl", "--rokr", &rokr],
        ":edit\nb + g()\n",
        &[("VISUAL", ""), ("EDITOR", &editor)],
    );
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "12\n11\n", "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// In a package the session does not load, a name the package defines
/// gets a hint; `repl .` names the package's directory.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.1.2 §9.7.10
fn repl_hints_at_the_package_it_runs_in() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let dir = scratch("repl-here").join("rokpk");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rok"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let run = |args: &[&str], input: &str| {
        let mut child = Command::new(ROKC)
            .args(args)
            .args(["--rokr", &rokr])
            .current_dir(&dir)
            .env("ROK_HOME", workspace())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let out = run(&["repl"], "add(1, 2)\nnope(1)\n");
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "", "{stderr}");
    assert_eq!(
        stderr
            .matches("help: `add` is defined in this package; load this package: `rok repl .`")
            .count(),
        1,
        "{stderr}"
    );
    // The library is archived by LLVM.
    if setup().is_some() {
        let out = run(&["repl", "."], "add(1, 2)\n");
        let stderr = text(&out.stderr);
        assert_eq!(text(&out.stdout), "3\n", "{stderr}");
        assert!(stderr.contains("Compiling rokpk (.)"), "{stderr}");
    }
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}

/// A value shows with `Display`, else `Debug`, else its type name.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.0 §9.7.7 / std §2.12.4
fn repl_shows_values_in_full() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
let xs = [1, 2, 3]
xs
(1, \"a\", [true])
enum E { A, B(i32) }
E::B(3)
#[derive(Debug)]
enum F { A, B(i32) }
F::B(3)
struct S { a: i32 }
S { a: 1 }
S { a: 1 }?
#[derive(Debug)]
struct D { a: i32 }
D { a: 1 }
struct T { a: i32 }
impl Display for T { fn fmt(self, ref f: Formatter) throws (FmtError) { try f.write_str(f\"T={self.a}\") } }
T { a: 2 }
T { a: 2 }?
nope?
nope(); S { a: 1 }
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stderr = text(&out.stderr);
    assert_eq!(
        text(&out.stdout),
        "[1, 2, 3]\n(1, \"a\", [true])\nE\nB(3)\nS\nS\nD { a: 1 }\nT=2\nT\n",
        "stderr: {stderr}"
    );
    // A trailing `?` keeps the error's location; a value that cannot show
    // adds no error to the cell's.
    assert_eq!(
        stderr,
        "\
error[E0027]: cannot find value `nope` in this scope
  --> <repl>:1:1
   |
 1 | nope?
   | ^^^^
error[E0027]: cannot find function `nope` in this scope
  --> <repl>:1:1
   |
 1 | nope(); S { a: 1 }
   | ^^^^^^
"
    );
}

/// A cell's failure says what happened: a panic in an earlier cell's
/// function names that cell, a throw and `exit` say so, and the generated
/// `main` is never named.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.0 §9.7.5
fn repl_reports_how_a_cell_failed() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
let kept = 1
fn boom(n: i32) {
    panic(f\"boom {n}\");
}
boom(5)
fn t() throws { throw ValueError::new(\"bad\"); }
kept += 1; try t()
kept += 1; rok::std::process::exit(3);
kept
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "1\n", "stderr: {stderr}");
    assert_eq!(
        stderr,
        "\
thread 'main' panicked at <repl cell 2>:2:5:
boom 5
error: the cell panicked; the session is unchanged
error: the cell threw `bad`; the session is unchanged
  --> <repl cell 5>:1:16
error: the cell called exit(3); the session is unchanged (`exit` or `:quit` ends the session)
"
    );
}

/// A redefinition that breaks an earlier item names it, its cell and its
/// text, and says how to go on; redefining both in one cell works.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.0 §9.7.5
fn repl_redefinition_names_broken_dependents() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "\
struct P { x: i32 }
impl P { fn get(self) -> i32 { self.x } }
impl P { fn two(self) -> i32 { 2 } }
struct P { y: i32 }
struct P { y: i32 } impl P { fn get(self) -> i32 { self.y } }
P { y: 4 }.get() + P { y: 0 }.two()
fn f() -> i32 { 1 }
fn g() -> i32 { f() + 1 }
fn f() -> str { \"x\" } fn g() -> str { f() }
g()
";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    let stderr = text(&out.stderr);
    assert_eq!(text(&out.stdout), "6\nx\n", "stderr: {stderr}");
    assert_eq!(
        stderr,
        "\
error[E0043]: struct `P` has no field `x`
  --> <repl cell 2>:1:32
   |
 1 | impl P { fn get(self) -> i32 { self.x } }
   |                                ^^^^^^
   |
   = help: available fields: `y`
   = help: `impl P` from cell 2 no longer compiles with this definition: redefine it in this cell too, or `:reset` to start over
"
    );
}

/// Ctrl-C ends a running cell; the session goes on as before it.
#[test]
#[cfg_attr(
    not(all(
        any(target_os = "macos", target_os = "linux"),
        any(target_arch = "aarch64", target_arch = "x86_64")
    )),
    ignore = "the REPL host runs on macOS and Linux, arm64 and x86_64"
)]
// spec: §9.7.5
fn repl_ctrl_c_interrupts_the_cell() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let mut child = Command::new(ROKC)
        .args(["repl", "--rokr", &rokr])
        .env("ROK_HOME", workspace())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    stdin
        .write_all(b"let n = 1\neprintln(\"spinning\"); n += 1; while true {}\n")
        .unwrap();
    let mut line = String::new();
    stderr.read_line(&mut line).unwrap();
    assert_eq!(line, "spinning\n");
    // The session host is rokc's child; the cell runs in it.
    let host = Command::new("pgrep")
        .args(["-P", &child.id().to_string()])
        .output()
        .unwrap();
    let host = text(&host.stdout);
    let host = host.lines().next().unwrap_or_default();
    assert!(
        Command::new("kill")
            .args(["-INT", host])
            .status()
            .unwrap()
            .success()
    );
    stdin.write_all(b"n\n").unwrap();
    drop(stdin);
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).unwrap();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        rest,
        "error: interrupted (Ctrl-C); the session is unchanged\n"
    );
    assert_eq!(stdout, "1\n");
}

/// Piped input is the REPL's own: a cell reads the end of input, and
/// the lines after it are still cells.
#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.10
fn repl_piped_cells_read_no_input() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let input = "((try io::read_line() catch _ { \"failed\" }) ?? \"\").len()\n\
                 40 + 2\n";
    let out = piped(ROKC, &["repl", "--rokr", &rokr], input);
    assert_eq!(text(&out.stdout), "0\n42\n", "{}", text(&out.stderr));
}

/// On a terminal a cell reads the terminal, and the line editor has it
/// back after the cell.
#[cfg(unix)]
#[test]
#[cfg_attr(
    not(all(
        any(target_os = "macos", target_os = "linux"),
        any(target_arch = "aarch64", target_arch = "x86_64")
    )),
    ignore = "a pseudo-terminal: the REPL host on macOS and Linux, arm64 and x86_64"
)]
// spec: §9.7.10
fn repl_cells_read_the_terminal() {
    use std::os::unix::process::CommandExt;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let (mut master, slave) = pty();
    // Its own home: the line editor keeps its history there.
    let home = scratch("repl-tty");
    let mut cmd = Command::new(ROKC);
    cmd.args(["repl", "--rokr", &rokr])
        .env("ROK_HOME", &home)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    // SAFETY: async-signal-safe calls only: the terminal becomes the
    // child's controlling one (`/dev/tty`).
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    drop(cmd);
    let seen = Arc::new(Mutex::new(String::new()));
    let mut reader = master.try_clone().unwrap();
    let s = seen.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            s.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    // Until `what` has been written `times` times.
    let wait_for = |what: &str, times: usize| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while seen.lock().unwrap().matches(what).count() < times {
            assert!(
                Instant::now() < deadline,
                "no {what:?} in {:?}",
                seen.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait_for(">>> ", 1);
    master
        .write_all(b"println(\"name?\"); let name = (try io::read_line() catch _ { \"failed\" }) ?? \"none\";\r")
        .unwrap();
    wait_for("name?\r\n", 1);
    master.write_all(b"rok\r").unwrap();
    wait_for(">>> ", 2);
    master.write_all(b"\"[\" + name + \"]\"\r").unwrap();
    wait_for("[rok]", 1);
    master.write_all(b":quit\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "{:?}", seen.lock().unwrap());
        std::thread::sleep(Duration::from_millis(20));
    }
    // The cell read `rok`, not the line editor.
    let history = std::fs::read_to_string(home.join("repl_history")).unwrap();
    assert!(
        history.ends_with("\"[\" + name + \"]\"\n:quit\n"),
        "{history}"
    );
    assert!(!history.lines().any(|l| l == "rok"), "{history}");
    let _ = std::fs::remove_dir_all(&home);
}

/// `rokc jupyter` driven by a hand-written ZMTP client (no Jupyter
/// needed): heartbeat, `kernel_info`, cells that bind, show, print and
/// panic, completion, and `shutdown`. Every message must be signed.
#[test]
#[cfg_attr(
    not(all(
        any(target_os = "macos", target_os = "linux"),
        any(target_arch = "aarch64", target_arch = "x86_64")
    )),
    ignore = "the REPL host runs on macOS and Linux, arm64 and x86_64"
)]
// spec: §0.1.2 §9.1.2 §9.7.9 §9.8.1 §9.8.2 §9.8.4 §9.8.5 §9.8.6 §9.8.7
fn jupyter_kernel_speaks_the_wire_protocol() {
    use rokc_interface::Json;
    use rokc_tools::jupyter::{encode, handshake, parse_msg, read_msg, write_msg};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    let env = setup_runtime();
    let key = b"secret-key";
    let dir = scratch("jupyter");
    // The kernel loads the package it starts in.
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/main.rok"),
        "fn add(a: i32, b: i32) -> i32 { a + b }\nlet base = 40i32;\n",
    )
    .unwrap();
    let file = dir.join("kernel.json");
    // The kernel says `ready` on stderr once every socket accepts. A
    // probed port can be taken again before the kernel binds it (it then
    // exits instead): start over on fresh ports.
    let mut attempts = 0;
    let (kernel, ports, mut stderr) = loop {
        let listeners: Vec<_> = (0..5)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let ports: Vec<u16> = listeners
            .iter()
            .map(|l| l.local_addr().unwrap().port())
            .collect();
        drop(listeners);
        std::fs::write(
            &file,
            format!(
                r#"{{"transport":"tcp","ip":"127.0.0.1","shell_port":{},"iopub_port":{},
"stdin_port":{},"control_port":{},"hb_port":{},"key":"secret-key",
"signature_scheme":"hmac-sha256","kernel_name":"rok"}}"#,
                ports[0], ports[1], ports[2], ports[3], ports[4]
            ),
        )
        .unwrap();
        let mut kernel = Command::new(ROKC)
            .args(["jupyter", "--connection-file"])
            .arg(&file)
            .args(["--rokr", &env.rokr.display().to_string()])
            .env("ROK_HOME", workspace())
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(kernel.stderr.take().unwrap());
        let mut said = String::new();
        stderr.read_line(&mut said).unwrap();
        if said == "rokc jupyter: ready\n" {
            break (kernel, ports, stderr);
        }
        kernel.wait().unwrap();
        attempts += 1;
        assert!(attempts < 5, "the kernel exits: {said}");
    };
    let connect = |port: u16, kind: &str| {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
        handshake(&mut s, kind).unwrap();
        s
    };
    let mut iopub = connect(ports[1], "SUB");
    write_msg(&mut iopub, &[b"\x01".to_vec()]).unwrap();
    let mut shell = connect(ports[0], "DEALER");
    let mut control = connect(ports[3], "DEALER");
    let mut hb = connect(ports[4], "REQ");
    write_msg(&mut hb, &[Vec::new(), b"ping".to_vec()]).unwrap();
    assert_eq!(read_msg(&mut hb).unwrap(), [Vec::new(), b"ping".to_vec()]);

    // ZeroMQ's slow joiner: iopub gets the subscriber only after the
    // handshake, so probe with `kernel_info_request` until it answers.
    iopub
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let probe = encode(
            key,
            "test",
            &[],
            "kernel_info_request",
            &Json::obj([]),
            &Json::obj([]),
        );
        write_msg(&mut shell, &probe).unwrap();
        read_msg(&mut shell).unwrap();
        if iopub.peek(&mut [0u8]).is_ok_and(|n| n > 0) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nothing on iopub after 60s"
        );
    }
    iopub
        .set_read_timeout(Some(Duration::from_secs(120)))
        .unwrap();
    let mut stdin = connect(ports[2], "DEALER");
    streams_and_asks(key, &mut shell, &mut iopub, &mut stdin);

    // Send a request: its reply (type, content) and the iopub messages
    // it caused, up to its `idle`.
    let mut request = |s: &mut TcpStream, kind: &str, content: Json| {
        let frames = encode(key, "test", &[], kind, &Json::obj([]), &content);
        let mine = parse_msg(key, frames.clone()).unwrap().header;
        let id = mine.get("msg_id").clone();
        write_msg(s, &frames).unwrap();
        let reply = parse_msg(key, read_msg(s).unwrap()).expect("a signed reply");
        assert!(reply.ids.is_empty());
        assert_eq!(reply.parent.get("msg_id"), &id);
        let mut published = Vec::new();
        loop {
            let m = parse_msg(key, read_msg(&mut iopub).unwrap()).expect("signed iopub");
            if m.parent.get("msg_id") != &id {
                continue;
            }
            if m.content.get("execution_state").as_str() == Some("idle") {
                break;
            }
            published.push((m.kind().to_owned(), m.content));
        }
        (reply.kind().to_owned(), reply.content, published)
    };
    let exec = |code: &str| Json::obj([("code", Json::str(code)), ("silent", Json::Bool(false))]);
    let shows = |published: &[(String, Json)], text: &str| {
        published.iter().any(|(k, c)| {
            k == "execute_result" && c.get("data").get("text/plain").as_str() == Some(text)
        })
    };

    let (kind, info, _) = request(&mut shell, "kernel_info_request", Json::obj([]));
    assert_eq!(kind, "kernel_info_reply");
    assert_eq!(info.get("language_info").get("name").as_str(), Some("rok"));
    assert_eq!(info.get("banner").as_str(), Some(toolchain_line().as_str()));

    let (_, reply, _) = request(&mut shell, "execute_request", exec("let x = 40i32;"));
    assert_eq!(reply.get("status").as_str(), Some("ok"), "{reply}");
    let (_, reply, published) = request(&mut shell, "execute_request", exec("x + 2"));
    assert_eq!(reply.get("execution_count"), &Json::num(2));
    assert!(shows(&published, "42"), "{published:?}");
    assert!(
        published
            .iter()
            .any(|(k, c)| k == "execute_input" && c.get("code").as_str() == Some("x + 2"))
    );
    cells_follow_the_count(&mut shell, &mut request);

    let (_, _, published) = request(&mut shell, "execute_request", exec("println(f\"hi {x}\");"));
    assert!(
        published.iter().any(|(k, c)| k == "stream"
            && c.get("name").as_str() == Some("stdout")
            && c.get("text").as_str() == Some("hi 40\n")),
        "{published:?}"
    );

    let (_, reply, published) = request(&mut shell, "execute_request", exec("panic(\"boom\");"));
    assert_eq!(reply.get("status").as_str(), Some("error"), "{reply}");
    panic_streams_then_fails(&published);
    // The session goes on, as it was before the failed cell.
    let (_, _, published) = request(&mut shell, "execute_request", exec("x"));
    assert!(shows(&published, "40"), "{published:?}");
    completion_inspect_history(&mut shell, &mut request);
    // The package's items and bindings, `await` and `it`, as in the REPL.
    let (_, _, published) = request(&mut shell, "execute_request", exec("add(base, 2)"));
    assert!(shows(&published, "42"), "{published:?}");
    let code = "async fn af() -> i32 { return 3; }\nlet v = await af();\nv + it";
    let (_, _, published) = request(&mut shell, "execute_request", exec(code));
    assert!(shows(&published, "45"), "{published:?}");
    let (_, _, published) = request(&mut shell, "execute_request", exec("await af()"));
    assert!(shows(&published, "3"), "{published:?}");
    let (_, _, published) = request(&mut shell, "execute_request", exec("v * it"));
    assert!(shows(&published, "9"), "{published:?}");

    // Values and failures show as in the REPL.
    let (_, _, published) = request(&mut shell, "execute_request", exec("[1, 2, 3]"));
    assert!(shows(&published, "[1, 2, 3]"), "{published:?}");
    let (_, reply, _) = request(
        &mut shell,
        "execute_request",
        exec("rok::std::process::exit(3);"),
    );
    assert_eq!(
        reply.get("evalue").as_str(),
        Some(
            "error: the cell called exit(3); the session is unchanged (`exit` or `:quit` ends the session)"
        ),
        "{reply}"
    );

    let (kind, _, _) = request(
        &mut control,
        "shutdown_request",
        Json::obj([("restart", Json::Bool(false))]),
    );
    assert_eq!(kind, "shutdown_reply");
    let out = kernel.wait_with_output().unwrap();
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).unwrap();
    assert!(out.status.success(), "{rest}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The panic message of a failed cell streams as it is written; `error`
/// then says what became of the cell.
fn panic_streams_then_fails(published: &[(String, rokc_interface::Json)]) {
    let at = |kind: &str| published.iter().position(|(k, _)| k == kind);
    let (said, error) = (at("stream").expect("stream"), at("error").expect("error"));
    assert!(said < error, "{published:?}");
    assert_eq!(published[said].1.get("name").as_str(), Some("stderr"));
    assert!(
        published[said].1.to_string().contains("boom"),
        "{published:?}"
    );
    assert_eq!(
        published[error].1.get("evalue").as_str(),
        Some("error: the cell panicked; the session is unchanged")
    );
}

/// Output streamed while a cell runs, and stdin through `input_request`,
/// on the kernel of `jupyter_kernel_speaks_the_wire_protocol` (cells kept
/// out of its history).
fn streams_and_asks(
    key: &[u8],
    shell: &mut std::net::TcpStream,
    iopub: &mut std::net::TcpStream,
    stdin: &mut std::net::TcpStream,
) {
    use rokc_interface::Json;
    use rokc_tools::jupyter::{encode, parse_msg, read_msg, write_msg};

    let execute = |shell: &mut std::net::TcpStream, code: &str, allow_stdin: bool| {
        let content = Json::obj([
            ("code", Json::str(code)),
            ("silent", Json::Bool(false)),
            ("store_history", Json::Bool(false)),
            ("allow_stdin", Json::Bool(allow_stdin)),
        ]);
        let frames = encode(
            key,
            "test",
            &[],
            "execute_request",
            &Json::obj([]),
            &content,
        );
        let header = parse_msg(key, frames.clone()).unwrap().header;
        write_msg(shell, &frames).unwrap();
        header
    };
    // The stdout streamed for `id` until it holds `text`.
    let streamed = |iopub: &mut std::net::TcpStream, id: &Json, text: &str| {
        let mut got = String::new();
        while !got.contains(text) {
            let m = parse_msg(key, read_msg(iopub).unwrap()).expect("signed iopub");
            if m.parent.get("msg_id") == id
                && m.kind() == "stream"
                && m.content.get("name").as_str() == Some("stdout")
            {
                got += m.content.get("text").as_str().unwrap_or("");
            }
        }
        got
    };
    let reply = |shell: &mut std::net::TcpStream, id: &Json| {
        let m = parse_msg(key, read_msg(shell).unwrap()).expect("a signed reply");
        assert_eq!(m.parent.get("msg_id"), id);
        m.content
    };

    // `early` arrives while the cell still sleeps.
    let code = "use rok::std::thread;\nprintln(\"early\");\n\
                thread::sleep(Duration::ms(1500));\nprintln(\"late\");";
    let id = execute(shell, code, true).get("msg_id").clone();
    assert_eq!(streamed(iopub, &id, "early\n"), "early\n");
    shell.set_nonblocking(true).unwrap();
    let pending = shell.peek(&mut [0u8]);
    shell.set_nonblocking(false).unwrap();
    assert!(
        pending
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock),
        "the reply came before the output: {pending:?}"
    );
    assert_eq!(reply(shell, &id).get("status").as_str(), Some("ok"));
    assert_eq!(streamed(iopub, &id, "late\n"), "late\n");

    // A read asks the client, after the prompt the cell printed.
    let code = "print(\"name? \");\n\
                let name = (try io::read_line() catch _ { \"failed\" }) ?? \"none\";\n\
                println(\"hello [\" + name + \"]\");";
    let header = execute(shell, code, true);
    let id = header.get("msg_id").clone();
    let ask = parse_msg(key, read_msg(stdin).unwrap()).expect("a signed input_request");
    assert_eq!(ask.kind(), "input_request");
    assert_eq!(ask.parent.get("msg_id"), &id);
    assert_eq!(ask.content.get("password"), &Json::Bool(false));
    assert_eq!(streamed(iopub, &id, "name? "), "name? ");
    let answer = Json::obj([("value", Json::str("rok"))]);
    let frames = encode(key, "test", &[], "input_reply", &ask.header, &answer);
    write_msg(stdin, &frames).unwrap();
    assert_eq!(reply(shell, &id).get("status").as_str(), Some("ok"));
    assert_eq!(streamed(iopub, &id, "hello [rok]\n"), "hello [rok]\n");

    // Without `allow_stdin` a read is the end of input.
    let code = "let line = (try io::read_line() catch _ { \"failed\" }) ?? \"\";\n\
                println(line.len());";
    let id = execute(shell, code, false).get("msg_id").clone();
    assert_eq!(reply(shell, &id).get("status").as_str(), Some("ok"));
    assert_eq!(streamed(iopub, &id, "0\n"), "0\n");
}

/// A REPL message: (reply kind, reply content, iopub messages).
type Reply = (
    String,
    rokc_interface::Json,
    Vec<(String, rokc_interface::Json)>,
);

/// `complete` / `is_complete` / `inspect` / `history` against the
/// session of `jupyter_kernel_speaks_the_wire_protocol` (`x: i32`, five
/// cells run).
/// Cells are numbered after the execution count (2 so far): `N.k` when
/// one execute splits; a silent execute leaves the count as it is.
fn cells_follow_the_count(
    shell: &mut std::net::TcpStream,
    request: &mut impl FnMut(&mut std::net::TcpStream, &str, rokc_interface::Json) -> Reply,
) {
    use rokc_interface::Json;
    let exec = |code: &str| Json::obj([("code", Json::str(code)), ("silent", Json::Bool(false))]);
    let panicked_in = |published: &[(String, Json)], cell: &str| {
        published
            .iter()
            .any(|(k, c)| k == "stream" && c.to_string().contains(&format!("panicked at {cell}:")))
    };
    let code = "let y = 1;\npanic(\"two\");";
    let (_, reply, published) = request(shell, "execute_request", exec(code));
    assert_eq!(reply.get("execution_count"), &Json::num(3));
    assert!(panicked_in(&published, "<repl cell 3.2>"), "{published:?}");
    let silent = Json::obj([("code", Json::str("x;")), ("silent", Json::Bool(true))]);
    request(shell, "execute_request", silent);
    let (_, reply, published) = request(shell, "execute_request", exec("panic(\"four\");"));
    assert_eq!(reply.get("execution_count"), &Json::num(4));
    assert!(panicked_in(&published, "<repl cell 4>"), "{published:?}");
}

fn completion_inspect_history(
    shell: &mut std::net::TcpStream,
    request: &mut impl FnMut(&mut std::net::TcpStream, &str, rokc_interface::Json) -> Reply,
) {
    use rokc_interface::Json;
    let (_, reply, _) = request(
        shell,
        "is_complete_request",
        Json::obj([("code", Json::str("fn f() {"))]),
    );
    assert_eq!(reply.get("status").as_str(), Some("incomplete"));
    let (_, reply, _) = request(
        shell,
        "complete_request",
        Json::obj([("code", Json::str("1 + x")), ("cursor_pos", Json::num(5))]),
    );
    assert_eq!(
        reply.get("matches"),
        &Json::Arr(vec![Json::str("x")]),
        "{reply}"
    );
    assert_eq!(reply.get("cursor_start"), &Json::num(4));
    let (_, reply, _) = request(
        shell,
        "complete_request",
        Json::obj([("code", Json::str("whi")), ("cursor_pos", Json::num(3))]),
    );
    assert_eq!(
        reply.get("matches"),
        &Json::Arr(vec![Json::str("while")]),
        "{reply}"
    );
    let inspect = |pos: u32| {
        Json::obj([
            ("code", Json::str("1 + x + nope")),
            ("cursor_pos", Json::num(pos)),
            ("detail_level", Json::num(0)),
        ])
    };
    let (kind, reply, _) = request(shell, "inspect_request", inspect(4));
    assert_eq!(kind, "inspect_reply");
    assert_eq!(reply.get("found"), &Json::Bool(true), "{reply}");
    assert_eq!(reply.get("data").get("text/plain").as_str(), Some("x: i32"));
    let (_, reply, _) = request(shell, "inspect_request", inspect(10));
    assert_eq!(reply.get("found"), &Json::Bool(false), "{reply}");
    let (_, reply, _) = request(
        shell,
        "history_request",
        Json::obj([
            ("hist_access_type", Json::str("tail")),
            ("n", Json::num(2)),
            ("output", Json::Bool(false)),
            ("raw", Json::Bool(true)),
        ]),
    );
    assert_eq!(
        reply.get("history"),
        &Json::Arr(vec![
            Json::Arr(vec![
                Json::num(0),
                Json::num(6),
                Json::str("panic(\"boom\");")
            ]),
            Json::Arr(vec![Json::num(0), Json::num(7), Json::str("x")]),
        ]),
        "{reply}"
    );

    // Members, paths and doc comments (9.8.5 / 9.8.6).
    let code = "struct P { x: i32; y: i32; }\n\
                impl P {\n    /// Sum of the coordinates.\n    fn sum(self) -> i32 { self.x + self.y }\n}";
    let exec = Json::obj([("code", Json::str(code)), ("silent", Json::Bool(false))]);
    let (_, reply, _) = request(shell, "execute_request", exec);
    assert_eq!(reply.get("status").as_str(), Some("ok"), "{reply}");
    let complete = |code: &str| {
        Json::obj([
            ("code", Json::str(code)),
            ("cursor_pos", Json::num(code.chars().count() as u32)),
        ])
    };
    let (_, reply, _) = request(
        shell,
        "complete_request",
        complete("let p = P { x: 1, y: 2 };\np.s"),
    );
    assert_eq!(
        reply.get("matches"),
        &Json::Arr(vec![Json::str("sum")]),
        "{reply}"
    );
    assert_eq!(reply.get("cursor_start"), &Json::num(28));
    let (_, reply, _) = request(
        shell,
        "complete_request",
        complete("let p = P { x: 1, y: 2 };\np."),
    );
    for m in ["x", "y", "sum"] {
        assert!(reply.get("matches").to_string().contains(m), "{reply}");
    }
    let (_, reply, _) = request(shell, "complete_request", complete("P::"));
    assert!(
        reply.get("matches").to_string().contains("\"sum\""),
        "{reply}"
    );
    let code = "let p = P { x: 1, y: 2 };\np.sum()";
    let (_, reply, _) = request(
        shell,
        "inspect_request",
        Json::obj([
            ("code", Json::str(code)),
            ("cursor_pos", Json::num(28)),
            ("detail_level", Json::num(0)),
        ]),
    );
    assert_eq!(
        reply.get("data").get("text/plain").as_str(),
        Some("fn sum(self: P) -> i32\n\nSum of the coordinates."),
        "{reply}"
    );
}

/// Entries in the temp directory `tmp` other than the lone-file cache.
fn leftovers(tmp: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(tmp)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("rokc-target"))
        .collect();
    v.sort();
    v
}

/// Processes whose executable or command line names `dir`.
fn processes_in(dir: &std::path::Path) -> Vec<String> {
    let dir = dir.display().to_string();
    let out = if cfg!(windows) {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-CimInstance Win32_Process | ForEach-Object { $_.ExecutablePath }",
            ])
            .output()
    } else {
        Command::new("ps").args(["-axo", "command"]).output()
    };
    text(&out.unwrap().stdout)
        .lines()
        .filter(|l| l.contains(&dir))
        .map(str::to_owned)
        .collect()
}

#[test]
#[cfg_attr(
    not(any(
        all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        windows
    )),
    ignore = "the REPL host runs on macOS, Linux and Windows (arm64, x86_64)"
)]
// spec: §9.7.0
fn rokc_leaves_no_temporaries() {
    let env = setup_runtime();
    let rokr = env.rokr.display().to_string();
    let root = scratch("no-temporaries");
    let tmp = root.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let rokc = |args: &[&str]| {
        let mut c = Command::new(ROKC);
        c.args(args).arg("--rokr").arg(&rokr).current_dir(&root);
        for v in ["TMPDIR", "TMP", "TEMP"] {
            c.env(v, &tmp);
        }
        c.env("ROK_HOME", workspace());
        c
    };
    std::fs::write(root.join("hi.rok"), "println(\"hi\");\n").unwrap();
    std::fs::write(root.join("bad.rok"), "let x: i32 = \"no\";\n").unwrap();
    let out = rokc(&["run", "-q", "hi.rok"]).output().unwrap();
    assert_eq!(text(&out.stdout), "hi\n", "{}", text(&out.stderr));
    let out = rokc(&["build", "-q", "bad.rok"]).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(leftovers(&tmp), Vec::<String>::new());

    // A REPL whose stdout lost its reader exits without running drops,
    // its host still alive: the host ends before the session goes.
    let mut child = rokc(&["repl"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    drop(child.stdin.take().unwrap().write_all(b"1 + 2\n"));
    child.wait().unwrap();
    assert_eq!(leftovers(&tmp), Vec::<String>::new());

    // rokc killed while a cell runs: its host ends too, and nothing stays.
    let mark = root.join("started");
    let cell = format!(
        "use rok::std::fs;\ntry! fs::write(\"{}\", \"\"); loop {{}}\n",
        mark.display().to_string().replace('\\', "/")
    );
    let mut child = rokc(&["repl"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(cell.as_bytes())
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !mark.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(mark.exists(), "the cell did not start");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (left, procs) = (leftovers(&tmp), processes_in(&tmp));
        // Windows: the job ends the host; its directory, still holding the
        // host's image as it ends, is left to the sweep.
        if procs.is_empty() && (left.is_empty() || cfg!(windows)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "left behind: {left:?}, running: {procs:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
