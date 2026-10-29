//! `repl` (906): cells JIT-linked into one session, the implicit
//! `main.rok` (907.1).
//!
//! Each cell is compiled to an object (native backend at `-O0`, LLVM
//! above) and sent to a long-lived host (`repl_host.c`) that links and
//! runs it. Items are in every cell's program; statements are not.
//! Functions the host already has with the same text are only declared
//! (`ReplCell::known`). A top-level binding is an exported global slot (a
//! GC root) owned by its cell; later cells declare it `extern "C" static
//! mut` and copy it in on entry and out on exit. A cell that panics leaves
//! the session untouched: the host forks a snapshot per cell (Windows:
//! saves and restores memory).
//!
//! `repl <path>` loads a program first (`load_program`).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rokc_diagnostic::{Label, RokDiagnostic, Span};
use rokc_hir::{
    HirBlock, HirExpr, HirExprKind, HirFunction, HirModule, HirStaticDef, HirStmt, HirTy,
};
use rokc_interface::{
    BuildOptions, CompileResult, EmitKind, ReplCell, ReplFn, Session, SourceMap, TempDir, link_host,
};
use rokc_ir_common::{ConstVal, LocalId};

use crate::analysis::{Analysis, ty_name, ty_path};

const VALUE: &str = "__rok_repl_v";
/// The host's `__rok_repl_out`: a cell's shown value.
const SHOWN: &str = "__rok_repl_out";
/// `repl_host.c` compiled (`build.rs`, which sets `cfg(rok_repl_host)`);
/// empty where no host runs.
const HOST_OBJ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/repl_host.o"));

const HELP: &str = "  <expr>         evaluate and print its value (`Display`); `it` holds the
                 last value shown (a binding named `it` wins)
  <expr>?        evaluate and print its `Debug` form
  let x = ..     bind (a top-level `let` may rebind a name, 907.3; a
                 cell's last `let` or assignment needs no `;`)
  fn / struct / enum / impl / trait / use ..   define (a new `fn f` /
                 `struct S` replaces the old one)
  :type <expr>, :t <expr>   print the type of an expression
  :doc <name>    print its signature and doc comment
  :time <expr>   evaluate it; print the time of each phase and the total
  :vars          list the bindings (`name: type`) and items
  :load <file>   load a program into the session (as `rok repl <file>`)
  :reload        load the last loaded program again
  :edit          write cells in `$EDITOR`, then run them
  :save <file>   write the session as a script
  :opt [<level>] print or set the optimization level of later cells
                 (0, 1, 2, 3, s, z)
  :add <name>[@<req>] | <name> --path <dir> | <name> --git <url>
                 [--rev|--tag|--branch <r>]
                 add a dependency: `rok install` into the environment, or
                 in a package `rok add` (`rok repl`)
  :reset         forget every cell
  :help, :h      print this help
  :quit, :q, :exit, exit, quit
                 leave (also end of input; a binding named `exit` or
                 `quit` wins)
A line ends the cell (`>>>`) unless a bracket, string or comment is still
open, it ends in an operator, `,` or `=`, or it is an item header without
its body: then `...` asks for the next line.
";

/// The items of the session, as source.
#[derive(Clone, Default)]
struct State {
    /// Items with their replace key (`fn f`, `struct S`; `None` for impls).
    items: Vec<(Option<String>, String)>,
}

/// A program loaded into the session (`repl <path>`).
struct Program {
    /// The session source's path: beside the program's root file, so
    /// its `mod` files resolve.
    session: PathBuf,
    anonymous: bool,
    /// The program's path and text, where its diagnostics and panic
    /// locations point.
    path: String,
    text: Arc<str>,
    /// Its items as the session has them, with their offsets in `text`.
    items: Vec<(String, usize)>,
    /// The offset in `text` of the cell being built (`None`: a typed cell).
    cell: Option<usize>,
}

/// How a cell failed.
enum Fail {
    /// Reported.
    Reported,
    /// A binding later cells could not use (not reported): its name and
    /// why.
    Unkept(String, String),
}

enum Cell {
    Item(Option<String>),
    /// `let ..;`.
    Let,
    Stmt,
    Expr,
}

/// `ROK_REPL_TIME=1` or `:time`: each phase of a cell's build on stderr.
struct Lap(Option<std::time::Instant>);

impl Lap {
    fn start(on: bool) -> Self {
        Self((on || rokc_core::env_flag("ROK_REPL_TIME")).then(std::time::Instant::now))
    }

    fn note(&self, what: &str) {
        if self.0.is_some() {
            eprintln!("[repl] {what}");
        }
    }

    fn mark(&mut self, what: &str) {
        if let Some(t) = &mut self.0 {
            eprintln!("[repl] {what}: {:.1}ms", t.elapsed().as_secs_f64() * 1e3);
            *t = std::time::Instant::now();
        }
    }
}

/// The host's reply to a cell: its shown value, or `None` when it
/// failed (reported; the session is as before it).
type Loaded = Option<String>;

/// A top-level binding.
#[derive(Clone)]
struct Binding {
    name: String,
    /// The exported global holding its value.
    sym: String,
    /// Its type, as source.
    ty: String,
    /// Its type as `:vars` shows it.
    shown_ty: String,
    /// It holds an `any` value (whose type a redefinition may change).
    holds_any: bool,
    /// The session's `it`, not a user's binding.
    is_it: bool,
}

/// A cell `:save` writes: its statements, then the expression whose
/// value it showed (`it`: kept as `it`), and the bindings it made.
#[derive(Default)]
struct Saved {
    stmts: String,
    expr: Option<String>,
    it: bool,
    declares: Vec<(String, String)>,
}

/// The host and its channel. Field order matters: the channel closes
/// (host sees EOF) before the host is waited for.
struct Host {
    ctl: Ctl,
    _child: Reaped,
    _job: Option<Job>,
}

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.wait();
    }
}

/// Windows: a job that ends the host when its last handle, ours, closes:
/// when rokc exits, however it ends. POSIX hosts watch rokc themselves
/// (`repl_host.c`).
#[cfg(windows)]
struct Job(*mut std::ffi::c_void);

// SAFETY: a job handle may be used and closed from any thread.
#[cfg(windows)]
unsafe impl Send for Job {}

#[cfg(windows)]
impl Job {
    fn ending(child: &Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        type Handle = *mut std::ffi::c_void;
        unsafe extern "system" {
            fn CreateJobObjectW(security: *mut std::ffi::c_void, name: *const u16) -> Handle;
            fn SetInformationJobObject(job: Handle, class: i32, info: *const u8, len: u32) -> i32;
            fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        }
        #[repr(C)]
        #[derive(Default)]
        struct Limits {
            per_process_time: i64,
            per_job_time: i64,
            flags: u32,
            min_working_set: usize,
            max_working_set: usize,
            active_processes: u32,
            affinity: usize,
            priority_class: u32,
            scheduling_class: u32,
            io: [u64; 6],
            process_memory: usize,
            job_memory: usize,
            peak_process_memory: usize,
            peak_job_memory: usize,
        }
        const EXTENDED_LIMIT_INFORMATION: i32 = 9;
        const KILL_ON_JOB_CLOSE: u32 = 0x2000;
        // SAFETY: an unnamed job with default security, not inherited.
        let job = Self(unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) });
        if job.0.is_null() {
            return None;
        }
        let limits = Limits {
            flags: KILL_ON_JOB_CLOSE,
            ..Limits::default()
        };
        // SAFETY: `limits` has the layout of
        // JOBOBJECT_EXTENDED_LIMIT_INFORMATION; both handles are live.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                EXTENDED_LIMIT_INFORMATION,
                (&raw const limits).cast(),
                std::mem::size_of::<Limits>() as u32,
            ) != 0
                && AssignProcessToJobObject(job.0, child.as_raw_handle()) != 0
        };
        if !ok {
            return None;
        }
        JOBS.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(job.0 as usize);
        rokc_core::tmp::on_release(end_jobs);
        Some(job)
    }
}

/// Live jobs, for an exit that skips drops: the session directory can
/// only go once its host.exe has ended.
#[cfg(windows)]
static JOBS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

#[cfg(windows)]
fn end_jobs() {
    type Handle = *mut std::ffi::c_void;
    unsafe extern "system" {
        fn TerminateJobObject(job: Handle, code: u32) -> i32;
        fn QueryInformationJobObject(
            job: Handle,
            class: i32,
            info: *mut u8,
            len: u32,
            ret: *mut u32,
        ) -> i32;
    }
    #[repr(C)]
    #[derive(Default)]
    struct Accounting {
        times: [i64; 4],
        page_faults: u32,
        total_processes: u32,
        active_processes: u32,
        terminated_processes: u32,
    }
    const BASIC_ACCOUNTING_INFORMATION: i32 = 1;
    let Ok(jobs) = JOBS.try_lock() else { return };
    for &j in jobs.iter() {
        let job = j as Handle;
        // SAFETY: a job handle that is ours until `Job::drop`, which also
        // takes it out of `JOBS`.
        unsafe { TerminateJobObject(job, 1) };
        for _ in 0..200 {
            let mut a = Accounting::default();
            // SAFETY: `a` has the layout of
            // JOBOBJECT_BASIC_ACCOUNTING_INFORMATION.
            let ok = unsafe {
                QueryInformationJobObject(
                    job,
                    BASIC_ACCOUNTING_INFORMATION,
                    (&raw mut a).cast(),
                    std::mem::size_of::<Accounting>() as u32,
                    std::ptr::null_mut(),
                ) != 0
            };
            if !ok || a.active_processes == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe extern "system" {
            fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
        }
        JOBS.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|&j| j != self.0 as usize);
        // SAFETY: the job handle is ours.
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(not(windows))]
struct Job;

#[cfg(not(windows))]
impl Job {
    #[allow(clippy::unnecessary_wraps)]
    const fn ending(_: &Child) -> Option<Self> {
        None
    }
}

/// One stream both ways, the host's stdin. Unix: a socket pair. Windows:
/// a named pipe whose both ends this process opens.
#[cfg(unix)]
type Ctl = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Ctl = std::fs::File;

/// Our end of a new channel, and the host's as its stdin.
#[cfg(unix)]
fn channel() -> std::io::Result<(Ctl, Stdio)> {
    let (ours, theirs) = Ctl::pair()?;
    Ok((ours, Stdio::from(std::os::fd::OwnedFd::from(theirs))))
}

#[cfg(windows)]
fn channel() -> std::io::Result<(Ctl, Stdio)> {
    use std::os::windows::io::FromRawHandle;
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            instances: u32,
            out_size: u32,
            in_size: u32,
            timeout: u32,
            security: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
    }
    const PIPE_ACCESS_DUPLEX: u32 = 0x3;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x8;
    static N: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        r"\\.\pipe\rok-repl-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated name; one byte-mode instance, ours.
    let h = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_REJECT_REMOTE_CLIENTS,
            1,
            1 << 16,
            1 << 16,
            0,
            std::ptr::null_mut(),
        )
    };
    if h as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a handle just created and owned by nothing else.
    let ours = unsafe { std::fs::File::from_raw_handle(h) };
    let theirs = std::fs::File::options()
        .read(true)
        .write(true)
        .open(&name)?;
    Ok((ours, Stdio::from(theirs)))
}

/// End the cell running in host process `pid`: `SIGINT`, or on Windows
/// Ctrl-Break to the host's own process group (`CREATE_NEW_PROCESS_GROUP`).
pub fn interrupt(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status();
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
        }
        const CTRL_BREAK_EVENT: u32 = 1;
        // SAFETY: plain call; a gone group is an error return.
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    }
}

/// Windows: the console sends Ctrl-C to rokc only (the host's group
/// ignores it); forward it to a running cell.
#[cfg(windows)]
fn forward_ctrl_c(running: Arc<AtomicU32>) {
    static RUNNING: std::sync::OnceLock<Arc<AtomicU32>> = std::sync::OnceLock::new();
    unsafe extern "system" fn handler(event: u32) -> i32 {
        const CTRL_C_EVENT: u32 = 0;
        if event != CTRL_C_EVENT {
            return 0;
        }
        let pid = RUNNING.get().map_or(0, |r| r.load(Ordering::Relaxed));
        if pid != 0 {
            interrupt(pid);
        }
        1
    }
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    if RUNNING.set(running).is_ok() {
        // SAFETY: a handler for the process's lifetime.
        unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
    }
}

/// Unix: Ctrl-C reaches rokc and the host alike; the host ends the cell.
/// A handler, unlike `SIG_IGN`, is reset by `exec`, so the host doesn't
/// inherit it.
#[cfg(unix)]
fn survive_ctrl_c() {
    const extern "C" fn ignore(_: i32) {}
    unsafe extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    const SIGINT: i32 = 2;
    // SAFETY: installs a handler that does nothing.
    unsafe { signal(SIGINT, ignore as extern "C" fn(i32) as usize) };
}

pub struct Repl {
    bootstrap: Vec<PathBuf>,
    /// Dependencies (`--extern name=<name>.rokhir`).
    pub externs: Vec<(String, PathBuf)>,
    /// Where `:add` prebuilds dependencies.
    pub deps_dir: PathBuf,
    /// The dependency archives the host has linked.
    linked: HashSet<PathBuf>,
    /// The program `repl <path>` loaded.
    program: Option<Program>,
    /// The first cell (`repl <lib>`: `use <lib>::*;`); `:reset`
    /// runs it again.
    pub prelude: Option<String>,
    opts: BuildOptions,
    state: State,
    host: Option<Host>,
    /// The host being built while the first cell compiles.
    starting: Option<std::thread::JoinHandle<Option<Host>>>,
    bindings: Vec<Binding>,
    /// Statics (MIR names) a loaded cell defined: later cells use its
    /// storage (`ReplCell::statics`), so their state carries over.
    statics: HashSet<String>,
    /// Types stored into `any` slots so far; every cell re-seeds them
    /// (`any_seeds`) so its `any` methods know all of them.
    any_types: std::collections::BTreeSet<String>,
    /// Cells built (slot names).
    built: u32,
    /// The `ScanTable`s loaded cells registered (`ReplCell::gc_tables`).
    gc_tables: Vec<Vec<u32>>,
    /// Symbol → (text hash, symbols used) of what the host has compiled;
    /// unchanged functions are declared, not recompiled.
    known: HashMap<String, (u64, Vec<String>)>,
    tmp: TempDir,
    /// The text the user typed for the cell being built: diagnostics
    /// point into it, not into the generated session source.
    input: String,
    /// The last compile failed only because an expression cell never
    /// yields a value (`panic(..)`).
    diverged: std::cell::Cell<bool>,
    /// The host process running the current cell (0: none). `SIGINT`
    /// to it ends the cell; the session goes on as before it.
    pub running: Arc<AtomicU32>,
    /// `:time`: the phases of the cell being built go to stderr.
    timing: bool,
    /// The last program loaded and whether it is a lone file (`:reload`).
    loaded: Option<(PathBuf, bool)>,
    /// The cells that ran, for `:save`.
    history: Vec<Saved>,
    /// The source directory of the package the session runs in without
    /// loading it: names it defines get a hint.
    pub package: Option<PathBuf>,
}

impl Drop for Repl {
    /// A host still being built writes into `tmp`: wait for it (and end
    /// it) before `tmp` goes.
    fn drop(&mut self) {
        if let Some(h) = self.starting.take() {
            drop(h.join());
        }
    }
}

impl Repl {
    pub fn new(bootstrap: Vec<PathBuf>, opts: BuildOptions) -> Result<Self, String> {
        Ok(Self {
            bootstrap,
            externs: Vec::new(),
            deps_dir: PathBuf::new(),
            linked: HashSet::new(),
            program: None,
            prelude: None,
            tmp: TempDir::new(opts.keep_temps).map_err(|d| d.message)?,
            opts,
            state: State::default(),
            host: None,
            starting: None,
            bindings: Vec::new(),
            statics: HashSet::new(),
            any_types: std::collections::BTreeSet::new(),
            built: 0,
            gc_tables: Vec::new(),
            known: HashMap::new(),
            input: String::new(),
            diverged: std::cell::Cell::new(false),
            running: Arc::default(),
            timing: false,
            loaded: None,
            history: Vec::new(),
            package: None,
        })
    }

    /// Read cells until `:quit` or EOF. `false` when input ends mid-cell.
    pub fn run(&mut self, input: impl BufRead, interactive: bool) -> bool {
        let prompt = |cont: bool| {
            if interactive {
                print!("{}", if cont { "... " } else { ">>> " });
                let _ = std::io::stdout().flush();
            }
        };
        let mut buf = String::new();
        prompt(false);
        for line in input.lines() {
            let Ok(line) = line else { break };
            buf.push_str(&line);
            buf.push('\n');
            if incomplete(&buf) {
                prompt(true);
                continue;
            }
            let cell = std::mem::take(&mut buf);
            if !self.cell(cell.trim()) {
                return true;
            }
            prompt(false);
        }
        if interactive {
            println!();
        }
        let rest = buf.trim();
        if !rest.is_empty() {
            eprintln!(
                "error: input ended inside a cell; not run (missing `;`, a closing bracket or \
                 a quote):\n{rest}"
            );
            return false;
        }
        true
    }

    /// Read cells from the terminal with line editing and history
    /// (`line_edit`) until `:quit` or end of input.
    pub fn run_tty(&mut self) {
        #[cfg(windows)]
        forward_ctrl_c(self.running.clone());
        #[cfg(unix)]
        survive_ctrl_c();
        let mut ed = crate::line_edit::Editor::new();
        let mut buf = String::new();
        loop {
            let prompt = if buf.is_empty() { ">>> " } else { "... " };
            match ed.read_line(prompt) {
                crate::line_edit::Input::Eof => return,
                crate::line_edit::Input::Interrupt => buf.clear(),
                crate::line_edit::Input::Line(line) => {
                    ed.add(&line);
                    buf.push_str(&line);
                    buf.push('\n');
                    if incomplete(&buf) {
                        continue;
                    }
                    let cell = std::mem::take(&mut buf);
                    if !self.cell(cell.trim()) {
                        return;
                    }
                }
            }
        }
    }

    /// One input; `false` to stop.
    pub fn cell(&mut self, cell: &str) -> bool {
        cell.clone_into(&mut self.input);
        match cell {
            _ if blank(cell) => {}
            // As in Python: the bare word ends the session.
            _ if EXIT.contains(&cell) && !self.defines(&cell[..4]) => return false,
            _ if cell.starts_with(':') => {
                let (cmd, arg) = cell
                    .split_once(char::is_whitespace)
                    .map_or((cell, ""), |(c, a)| (c, a.trim()));
                return self.command(cmd, arg);
            }
            _ => {
                if let Ok(Some(shown)) = self.eval(cell) {
                    println!("{shown}");
                }
            }
        }
        true
    }

    /// A `:` command; `false` to stop.
    fn command(&mut self, cmd: &str, arg: &str) -> bool {
        let needs = |what: &str, example: &str| {
            eprintln!("error: `{cmd}` takes {what}: `{cmd} {example}`");
        };
        match cmd {
            ":q" | ":quit" | ":exit" => return false,
            ":reset" => self.reset(),
            ":help" | ":h" => print!("{HELP}"),
            ":type" | ":t" if arg.is_empty() => needs("an expression", "1 + 2"),
            ":type" | ":t" => {
                arg.clone_into(&mut self.input);
                if let Some(ty) = self.type_of(arg, false) {
                    println!("{ty}");
                }
            }
            ":add" if arg.is_empty() => needs("a dependency", "<name> --path <dir>"),
            ":add" => self.add_dep(arg),
            ":doc" => {
                let start = arg
                    .trim_end_matches(|c: char| c.is_alphanumeric() || c == '_')
                    .len();
                if start == arg.len() {
                    needs("a name", "xs.len");
                } else if let Some(text) = self
                    .help(arg, start, arg.len())
                    // A method is found in a call.
                    .or_else(|| self.help(&format!("{arg}()"), start, arg.len()))
                {
                    println!("{text}");
                } else {
                    eprintln!("error: the session defines no `{arg}`; `:vars` lists what it does");
                }
            }
            ":time" if arg.is_empty() => needs("an expression", "fib(30)"),
            ":time" => {
                let t = std::time::Instant::now();
                self.timing = true;
                let r = self.eval(arg);
                self.timing = false;
                if let Ok(Some(shown)) = r {
                    println!("{shown}");
                }
                eprintln!("[repl] total: {:.1}ms", t.elapsed().as_secs_f64() * 1e3);
            }
            ":vars" => self.vars(),
            ":load" if arg.is_empty() => needs("a file", "prog.rok"),
            ":load" if std::path::Path::new(arg).is_dir() => eprintln!(
                "error: `:load` takes a `.rok` file; for a package directory start `rok repl {arg}`"
            ),
            ":load" => {
                self.load_program(std::path::Path::new(arg), true);
            }
            ":reload" => match self.loaded.clone() {
                Some((path, anonymous)) => {
                    self.load_program(&path, anonymous);
                }
                None => eprintln!("error: no program loaded yet; load one with `:load <file>`"),
            },
            ":edit" => return self.edit(),
            ":save" if arg.is_empty() => needs("a file", "session.rok"),
            ":save" => self.save(arg),
            ":opt" => match arg {
                "" => println!("-O{}", self.opts.opt_level),
                "0" | "1" | "2" | "3" | "s" | "z" => {
                    arg.clone_into(&mut self.opts.opt_level);
                    // Code built at the old level is built again.
                    self.known.clear();
                }
                _ => eprintln!(
                    "error: `:opt` takes 0, 1, 2, 3, s or z (as `-O`), not `{arg}`: `:opt 2`"
                ),
            },
            _ => eprintln!("unknown command `{cmd}` (:help lists them)"),
        }
        true
    }

    /// `:reset`: forget every cell, then run the prelude again.
    fn reset(&mut self) {
        self.state = State::default();
        self.bindings.clear();
        self.statics.clear();
        self.any_types.clear();
        self.history.clear();
        if let Some(p) = self.prelude.clone() {
            self.cell(&p);
        }
    }

    /// Whether the session's own code names `name`: a binding (not `it`),
    /// an item, or a `use` of it.
    fn defines(&self, name: &str) -> bool {
        self.bindings.iter().any(|b| b.name == name && !b.is_it)
            || self.state.items.iter().any(|(k, t)| match k {
                Some(k) => k.split(' ').nth(1) == Some(name),
                None => t.starts_with("use ") && names(t, name),
            })
    }

    /// Help for the name at `code[start..end]` (9.8.6): the language
    /// server's hover as text, from the cell checked in the session, else
    /// from the expression ending at the name alone.
    pub fn help(&self, code: &str, start: usize, end: usize) -> Option<String> {
        let at = |body: &str, off: usize| {
            let (src, base) = self.source(body);
            let (a, key) = self.analyze(&src);
            crate::lsp::hover(&a, &key, &src, base + off)
        };
        let from = crate::lsp::receiver_start(code, end);
        let md = at(code, start).or_else(|| at(&format!("{};", &code[from..end]), start - from))?;
        let lines: Vec<&str> = md.lines().filter(|l| !l.starts_with("```")).collect();
        Some(lines.join("\n"))
    }

    /// `:vars`: the bindings as `name: type`, then the named items'
    /// headers.
    fn vars(&self) {
        for b in &self.bindings {
            println!("{}: {}", b.name, b.shown_ty);
        }
        for (_, item) in self.state.items.iter().filter(|(k, _)| k.is_some()) {
            println!("{}", header(item));
        }
    }

    /// `:edit`: cells written in `$EDITOR` run as typed; `false` to stop.
    /// The text of a run that failed opens again.
    fn edit(&mut self) -> bool {
        let file = self.tmp.path().join("edit.rok");
        if !file.exists() {
            if let Err(e) = std::fs::write(&file, "") {
                eprintln!("error: cannot write {}: {e}", file.display());
                return true;
            }
        }
        let editor = ["VISUAL", "EDITOR"]
            .iter()
            .filter_map(std::env::var_os)
            .find(|e| !e.is_empty())
            .and_then(|e| e.into_string().ok())
            .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "vi" }.to_owned());
        let mut words = editor.split_whitespace();
        let Some(exe) = words.next() else { return true };
        match Command::new(exe).args(words).arg(&file).status() {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("error: `{editor}` failed ({s}); nothing ran");
                return true;
            }
            Err(e) => {
                eprintln!("error: cannot run `{editor}`: {e}; set `EDITOR` to your editor");
                return true;
            }
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for c in split_cells(&text) {
            let r = if c.starts_with(':') || EXIT.contains(&c.as_str()) {
                if !self.cell(&c) {
                    return false;
                }
                Ok(None)
            } else {
                self.eval(&c)
            };
            match r {
                Ok(Some(shown)) => println!("{shown}"),
                Ok(None) => {}
                Err(()) => return true,
            }
        }
        let _ = std::fs::write(&file, "");
        true
    }

    /// `:save <file>`: the session's items, then the cells that ran, as a
    /// script. An expression cell is a statement, or `let it = ..` where a
    /// later cell reads `it`.
    fn save(&self, path: &str) {
        match std::fs::write(path, self.script()) {
            Ok(()) => eprintln!("wrote {path}"),
            Err(e) => eprintln!("error: cannot write {path}: {e}"),
        }
    }

    /// The session as a script (`:save`). A file may not declare a name
    /// twice in one scope (5.1.4): a cell that declares one again with
    /// the same type assigns it; otherwise it opens a block that lasts
    /// to the end.
    fn script(&self) -> String {
        let mut out = String::new();
        for (_, item) in &self.state.items {
            out.push_str(item);
            out.push('\n');
        }
        let mut scope: HashMap<&str, &str> = HashMap::new();
        let mut open = 0;
        for (i, s) in self.history.iter().enumerate() {
            let mut read = false;
            for later in &self.history[i + 1..] {
                let text = format!("{}{}", later.stmts, later.expr.as_deref().unwrap_or(""));
                read = s.it && names(&text, "it");
                if read || later.it {
                    break;
                }
            }
            // `it` is declared only where a later cell reads it.
            let declares: Vec<(&str, &str)> = s
                .declares
                .iter()
                .map(|(n, t)| (n.as_str(), t.as_str()))
                .filter(|(n, _)| read || !(s.it && *n == "it"))
                .collect();
            let stmts = match s.stmts.strip_suffix("\n;") {
                Some(t) => format!("{t};"),
                None => s.stmts.clone(),
            };
            let mut text = stmts.clone();
            let (mut assign_it, mut block) = (false, false);
            for &(n, t) in &declares {
                match scope.get(n) {
                    None => {}
                    Some(&old) if old == t && s.it && n == "it" => assign_it = true,
                    Some(&old) if old == t => match assign_instead(&text, n) {
                        Some(a) => text = a,
                        None => block = true,
                    },
                    Some(_) => block = true,
                }
            }
            if block {
                out.push_str("{\n");
                open += 1;
                scope.clear();
                (text, assign_it) = (stmts, false);
            }
            scope.extend(declares);
            out.push_str(&text);
            match &s.expr {
                Some(e) if assign_it => {
                    let _ = write!(out, "it = {e};");
                }
                Some(e) if read => {
                    let _ = write!(out, "let it = {e};");
                }
                Some(e) => {
                    let _ = write!(out, "{e};");
                }
                None => {}
            }
            out.push('\n');
        }
        out.push_str(&"}\n".repeat(open));
        out
    }

    /// The bindings (name, type) the last cell built made.
    fn declared(&self) -> Vec<(String, String)> {
        let made = format!("__rok_repl_{}_", self.built);
        let mine = self.bindings.iter().filter(|b| b.sym.starts_with(&made));
        mine.map(|b| (b.name.clone(), b.ty.clone())).collect()
    }

    /// One cell: what it shows (the value, or its type), or `Err`
    /// after its errors went to stderr.
    #[allow(clippy::result_unit_err)]
    pub fn eval(&mut self, cell: &str) -> Result<Option<String>, ()> {
        // Diagnostics quote the cell as typed (`regions`).
        cell.clone_into(&mut self.input);
        let cell = &close_stmt(cell);
        if let Cell::Item(key) = classify(cell) {
            return self.item(cell, key).then_some(None).ok_or(());
        }
        // The cell's value is its last top-level statement's (907.7).
        let (stmts, last) = split_last(cell);
        let saved = |stmts: &str, expr: Option<&str>, it: bool| Saved {
            stmts: stmts.to_owned(),
            expr: expr.map(str::to_owned),
            it,
            declares: Vec::new(),
        };
        let (r, ran) = match classify(last) {
            // A statement (a `let` too) shows nothing (907.1).
            Cell::Item(_) | Cell::Let | Cell::Stmt => {
                (self.run_once(cell, None, false), saved(cell, None, false))
            }
            Cell::Expr => {
                // A trailing `?` on a bare top-level expression asks for the
                // `Debug` form: error propagation needs a `throws` context,
                // which a top-level expression has not, so the slot is free.
                let (expr, debug) = match last.trim_end().strip_suffix('?') {
                    Some(e) => (e.trim_end(), true),
                    None => (last, false),
                };
                let before = self.it().map(str::to_owned);
                let r = self.run_once(
                    &format!("{stmts}let {VALUE} = ({expr});"),
                    Some(VALUE),
                    debug,
                );
                if r.is_err() && self.diverged.take() {
                    let text = format!("{stmts}{expr};");
                    (self.run_once(&text, None, false), saved(&text, None, false))
                } else {
                    let it = self.it().is_some() && self.it() != before.as_deref();
                    (r, saved(stmts, Some(expr), it))
                }
            }
        };
        if r.is_ok() {
            let declares = self.declared();
            self.history.push(Saved { declares, ..ran });
        }
        r
    }

    /// The slot of the session's `it`.
    fn it(&self) -> Option<&str> {
        let b = self.bindings.iter().find(|b| b.is_it)?;
        Some(&b.sym)
    }

    /// The type of `expr` in the session, without running it; `quiet`
    /// keeps its errors unreported.
    fn type_of(&self, expr: &str, quiet: bool) -> Option<String> {
        let src = self.session(&self.state, &format!("let {VALUE} = ({expr});"), None);
        self.compile(&src, quiet)
            .and_then(|r| binding_ty(&r, VALUE))
    }

    /// The names the session defines: bindings, then items.
    pub fn names(&self) -> Vec<&str> {
        let items = self.state.items.iter();
        let items = items.filter_map(|(k, _)| k.as_deref()?.split(' ').nth(1));
        self.bindings
            .iter()
            .map(|b| b.name.as_str())
            .chain(items)
            .collect()
    }

    /// `:add <args>`: `rok install --env` in an environment's session, else
    /// `rok add`; then prebuild `rok deps`.
    fn add_dep(&mut self, args: &str) {
        let Some(rok) = std::env::var_os("ROK") else {
            eprintln!("`:add` needs a session started by `rok repl`");
            return;
        };
        let env: Vec<String> = std::env::var("ROK_ENV")
            .ok()
            .filter(|e| !e.is_empty())
            .map(|e| vec!["--env".to_owned(), e])
            .unwrap_or_default();
        let run = |cmd: &str, extra: &[&str]| {
            Command::new(&rok)
                .arg(cmd)
                .args(&env)
                .args(extra)
                .stderr(std::process::Stdio::inherit())
                .output()
                .ok()
                .filter(|o| o.status.success())
        };
        let words: Vec<&str> = args.split_whitespace().collect();
        if run(if env.is_empty() { "add" } else { "install" }, &words).is_none() {
            return;
        }
        let Some(out) = run("deps", &[]) else { return };
        // `name=<lib.rok>` lines, each followed by `--extern-feature` lines,
        // then native rokc arguments under `# native ..` lines.
        let text = String::from_utf8_lossy(&out.stdout);
        let o = &mut self.opts;
        let mut deps: Vec<(String, PathBuf)> = Vec::new();
        let mut features: Vec<(String, String)> = Vec::new();
        for l in text.lines().filter(|l| !l.starts_with('#')) {
            if let Some((n, f)) = l
                .strip_prefix("--extern-feature=")
                .and_then(|v| v.split_once('='))
            {
                features.push((n.to_owned(), f.to_owned()));
            } else if let Some(d) = l.strip_prefix("-L") {
                push_new(&mut o.lib_dirs, PathBuf::from(d));
            } else if let Some(f) = l.strip_prefix("--native-runtime=") {
                push_new(&mut o.runtime_files, PathBuf::from(f));
            } else if let Some((kind, name)) = l.strip_prefix("-l").and_then(|k| k.split_once('='))
            {
                push_new(&mut o.libs, (name.to_owned(), kind == "static"));
            } else if let Some((n, p)) = l.split_once('=') {
                deps.push((n.to_owned(), PathBuf::from(p)));
            }
        }
        let build = rokc_interface::DepBuild {
            bootstrap: &self.bootstrap,
            cfg: &[],
            features: &features,
            opts: &self.opts,
            target: rokc_interface::TargetSpec::host(),
            dir: &self.deps_dir,
            quiet: true,
            std: None,
        };
        match build.prebuild(&deps) {
            Ok(externs) => self.externs = externs,
            Err(e) => eprintln!("error: {e}"),
        }
    }

    fn item(&mut self, cell: &str, key: Option<String>) -> bool {
        // A new definition replaces the old one of the same name.
        let mut next = self.state.clone();
        next.items.retain(|(k, _)| key.is_none() || *k != key);
        next.items.push((key.clone(), cell.to_owned()));
        let Some(r) = self.compile(&self.session(&next, "", None), false) else {
            return false;
        };
        // Codegen too, like `check`: it rejects what cannot be lowered.
        if let Err(d) = r.emit(EmitKind::Mlir, &self.opts) {
            r.report(&d);
            return false;
        }
        // A binding of a redefined type would be read with the new
        // layout: forget it.
        let old = key.as_deref().filter(|k| {
            !k.starts_with("fn ")
                && self
                    .state
                    .items
                    .iter()
                    .any(|(o, _)| o.as_deref() == Some(k))
        });
        if let Some(ty) = old.and_then(|k| k.split(' ').nth(1)) {
            self.bindings.retain(|b| {
                let keep = !b
                    .ty
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|w| w == ty);
                if !keep {
                    eprintln!("note: `{}` is gone: its type `{ty}` was redefined", b.name);
                } else if b.holds_any {
                    // It may hold an old `{ty}` the new tag would claim;
                    // telling them apart needs the value's tag.
                    eprintln!(
                        "note: `{}` is gone: it holds an `any` value and `{ty}` was redefined",
                        b.name
                    );
                    return false;
                }
                keep
            });
        }
        // A redefined static starts over: the next cell defines it anew.
        if let Some(name) = key.as_deref().and_then(|k| k.strip_prefix("static ")) {
            let lib = |s: &str| {
                s.starts_with("rok::")
                    || self
                        .externs
                        .iter()
                        .any(|(n, _)| s.starts_with(&format!("{n}::")))
            };
            self.statics
                .retain(|s| lib(s) || s.rsplit("::").next() != Some(name));
        }
        // A user's `it` wins over the session's.
        if key.as_deref().and_then(|k| k.split(' ').nth(1)) == Some("it") {
            self.bindings.retain(|b| !b.is_it);
        }
        self.state = next;
        true
    }

    /// Cell source: items, binding slots, `body`, and the store of `show`
    /// to the host. `keep` adds the slot stores to the HIR.
    fn session(&self, state: &State, body: &str, show: Option<&str>) -> String {
        let mut src = String::new();
        for (_, item) in &state.items {
            src.push_str(item);
            src.push('\n');
        }
        // Every type earlier cells stored in `any` slots: stored here too.
        for (i, t) in self.any_types.iter().enumerate() {
            let _ = writeln!(src, "fn __rok_repl_any_{i}(x: {t}) -> any {{ return x; }}");
        }
        let _ = writeln!(src, "extern \"C\" {{\n    static mut {SHOWN}: str;");
        for b in &self.bindings {
            let _ = writeln!(src, "    static mut {}: {};", b.sym, b.ty);
        }
        src.push_str("}\n");
        for b in &self.bindings {
            let _ = writeln!(src, "let {} = {};", b.name, b.sym);
        }
        // Its own block: a `let` may rebind a name (907.3, 5.1.4). None
        // without a body: a loaded program's `fn main` is the entry then.
        if body.is_empty() && show.is_none() {
            return src;
        }
        src.push_str("{\n");
        src.push_str(body);
        src.push('\n');
        if let Some(text) = show {
            let _ = writeln!(src, "{SHOWN} = {text};");
        }
        src.push_str("}\n");
        src
    }

    /// Build `cell` as a library, run it in the host, keep its top-level
    /// bindings; the `Display` (or `Debug`) of `show` (`eval`).
    fn run_once(
        &mut self,
        cell: &str,
        show: Option<&str>,
        debug: bool,
    ) -> Result<Option<String>, ()> {
        self.run_cell(cell, show, debug, &[]).map_err(|f| {
            if let Fail::Unkept(_, e) = f {
                eprintln!("error: {e}");
            }
        })
    }

    /// `run_once`; a binding later cells can't use fails the cell before it
    /// runs, unless it is in `may_drop` (dropped with a note).
    fn run_cell(
        &mut self,
        cell: &str,
        show: Option<&str>,
        debug: bool,
        may_drop: &[String],
    ) -> Result<Option<String>, Fail> {
        self.start_host();
        let mut lap = Lap::start(self.timing);
        // A bare expression prints its `Display`; a trailing `?` (`debug`)
        // prints its `Debug`. Either falls back to the type name alone when
        // the value does not implement that trait; the plain build reports
        // real errors.
        let texts: Vec<String> = show
            .map(|n| {
                let value = if debug {
                    format!("f\"{{{n}:?}}\"")
                } else {
                    format!("f\"{{{n}}}\"")
                };
                vec![value, format!("{n}.type_name_of()")]
            })
            .unwrap_or_default();
        let mut tries = texts.iter().map(Some).chain([None]);
        let (mut r, shown) = loop {
            let text = tries.next().flatten();
            let src = self.session(&self.state, cell, text.map(String::as_str));
            match self.compile(&src, text.is_some()) {
                Some(r) => break (r, text.is_some()),
                None if text.is_some() => {}
                None => return Err(Fail::Reported),
            }
        };
        lap.mark("front end");
        let it = show.is_some() && !self.defines("it");
        let new = self
            .keep(&mut r, may_drop, it)
            .map_err(|(n, e)| Fail::Unkept(n, e))?;
        let edited = new.is_some();
        let new = new.unwrap_or_default();
        lap.mark("bindings");
        self.built += 1;
        let mut opts = self.opts.clone();
        // `-g` cells compile everything (their text carries
        // locations, which differ from cell to cell anyway).
        let known = if opts.debug {
            HashMap::new()
        } else {
            self.known
                .iter()
                .map(|(k, (h, _))| (k.clone(), *h))
                .collect()
        };
        opts.repl = Some(ReplCell {
            gc_tables: self.gc_tables.clone(),
            known,
            define: new.iter().map(|b| b.sym.clone()).collect(),
            statics: self.statics.iter().cloned().collect(),
        });
        // MIR lowering reads the front end's per-thread state: on the
        // compile thread too.
        let code = crate::analysis::isolate(move || {
            if edited {
                r.relower();
            }
            let code = r.cell_code(&opts);
            (r, code)
        });
        let (r, code) = code.map_err(|e| {
            SourceMap::new().report(&crate::analysis::ice(&e));
            Fail::Reported
        })?;
        let (ir, out) = match code {
            Ok(x) => x,
            Err(d) => {
                r.report(&d);
                return Err(Fail::Reported);
            }
        };
        lap.mark("codegen");
        lap.note(&format!(
            "functions: {} compiled, {} from earlier cells; object: {} bytes",
            out.fns.len(),
            out.reused,
            ir.len()
        ));
        self.link_archives(&r);
        let value = self.load(&ir);
        lap.mark("jit + run");
        let value = value.ok_or(Fail::Reported)?;
        self.gc_tables = out.gc_tables;
        self.learn(out.fresh, out.fns);
        self.bindings
            .retain(|b| !new.iter().any(|n| n.name == b.name));
        self.bindings.extend(new);
        if let Some(m) = r.modules.first() {
            let defined = m.mir.statics.iter().filter(|s| !s.is_extern);
            self.statics.extend(defined.map(|s| s.name.clone()));
            // Names a later cell writes at its top level: visible there.
            let visible = |t: &&String| {
                t.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                    .all(|p| p.is_empty() || rokc_hir::type_visible(&m.hir, p, ""))
            };
            let stored = m.hir.any_types.iter().filter(visible);
            self.any_types.extend(stored.cloned());
        }
        Ok(show
            .and_then(|n| binding_ty(&r, n))
            .filter(|ty| ty != "()")
            .map(|ty| if shown { value } else { ty }))
    }

    /// Adds slots for the cell's new top-level `let`s (last of each name)
    /// and, at the block's end, stores of every binding back to its slot.
    /// An unusable binding is an error unless in `may_drop`. `it`: the
    /// shown value becomes `it`, unless the cell binds `it` itself. `None`:
    /// HIR unchanged; else MIR must be lowered again.
    fn keep(
        &self,
        r: &mut CompileResult,
        may_drop: &[String],
        it: bool,
    ) -> Result<Option<Vec<Binding>>, (String, String)> {
        let Some(hir) = r.modules.first_mut().map(|m| &mut m.hir) else {
            return Ok(None);
        };
        // Where the cell's block starts in the session source.
        let at = u32::try_from(self.session(&self.state, "", None).len()).unwrap_or(u32::MAX);
        let Some((f, outer, block)) = top_fns(hir)
            .into_iter()
            .find_map(|f| cell_scope(&hir.functions[f], at).map(|(o, b)| (f, o, b)))
        else {
            return Ok(None);
        };
        // The `let`s before the cell's block load the old bindings.
        let outer: Vec<LocalId> = outer
            .iter()
            .filter_map(|s| match s {
                HirStmt::Let { local, .. } => Some(*local),
                _ => None,
            })
            .collect();
        let locals = &hir.functions[f].locals;
        let local = |id: LocalId| locals.iter().find(|l| l.id == id);
        let named = |id: LocalId| {
            local(id)?
                .name
                .as_deref()
                .filter(|n| !n.starts_with("__") && *n != "_")
        };
        let lets = block.stmts.iter().filter_map(|s| match s {
            HirStmt::Let { local, .. } => Some(*local),
            _ => None,
        });
        let mut new: Vec<(Binding, LocalId)> = Vec::new();
        for (i, id) in lets.enumerate() {
            let Some(l) = local(id) else { continue };
            let shown = it && l.name.as_deref() == Some(VALUE);
            let name = match named(id) {
                Some(n) => n,
                // A unit or unkeepable value leaves `it` as it was.
                None if shown
                    && !matches!(l.ty, HirTy::Unit | HirTy::Never)
                    && !new.iter().any(|(b, _)| b.name == "it") =>
                {
                    "it"
                }
                None => continue,
            };
            // Full paths, so later cells name the type without a `use`.
            let ty = ty_path(hir, &l.ty);
            // Closures, unresolved types, and types the session's top level
            // can't see (spec 5.6) can't be kept.
            let why = if ty.contains('{') || ty.starts_with("closure") {
                Some("its type has no name".to_owned())
            } else {
                ty.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                    .find(|p| !rokc_hir::type_visible(hir, p, ""))
                    .map(|p| format!("`{p}` is private to its module"))
            };
            if let Some(why) = why {
                if shown {
                    continue;
                }
                let msg = format!(
                    "`{name}: {}` cannot be kept for later cells ({why})",
                    ty_name(hir, &l.ty)
                );
                if !may_drop.iter().any(|n| n == name) {
                    return Err((name.to_owned(), msg));
                }
                eprintln!("note: {msg}");
                continue;
            }
            let holds_any = holds_any(hir, &l.ty, &mut Vec::new());
            new.retain(|(b, _)| b.name != name);
            let sym = format!("__rok_repl_{}_{i}", self.built + 1);
            let name = name.to_owned();
            new.push((
                Binding {
                    name,
                    sym,
                    ty,
                    shown_ty: ty_name(hir, &l.ty),
                    holds_any,
                    is_it: shown,
                },
                id,
            ));
        }
        // Every binding goes back (a `let` can be assigned);
        // only the ones the cell assigns would need to.
        let old = self
            .bindings
            .iter()
            .filter(|b| !new.iter().any(|(n, _)| n.name == b.name))
            .filter_map(|b| {
                let id = outer.iter().rev().find(|&&id| named(id) == Some(&b.name))?;
                Some((b.sym.clone(), *id))
            });
        let stores: Vec<HirStmt> = old
            .chain(new.iter().map(|(b, id)| (b.sym.clone(), *id)))
            .filter_map(|(name, id)| {
                let ty = local(id)?.ty.clone();
                let kind = HirExprKind::Local(id);
                let span = rokc_diagnostic::Span::DUMMY;
                Some(HirStmt::StaticAssign {
                    name,
                    value: HirExpr { kind, ty, span },
                })
            })
            .collect();
        let slots: Vec<HirStaticDef> = new
            .iter()
            .filter_map(|(b, id)| {
                Some(HirStaticDef {
                    name: b.sym.clone(),
                    ty: local(*id)?.ty.clone(),
                    init: ConstVal::Unit,
                    is_mut: true,
                    is_extern: true,
                    dll_import: false,
                    init_fn: None,
                    span: rokc_diagnostic::Span::DUMMY,
                })
            })
            .collect();
        hir.statics.extend(slots);
        let mut stores = Some(stores);
        let mut store = |scope: &mut HirBlock| {
            let Some(HirStmt::Expr(e)) = scope.stmts.iter_mut().find(|s| is_cell(s, at)) else {
                return;
            };
            let (HirExprKind::Block(b), Some(stores)) = (&mut e.kind, stores.take()) else {
                return;
            };
            if let Some(t) = b.trailing.take() {
                b.stmts.push(HirStmt::Expr(t));
                e.ty = HirTy::Unit;
            }
            b.stmts.extend(stores);
        };
        if let Some(body) = hir.functions[f].body.as_mut() {
            store(body);
            rokc_hir::each_block_mut(body, &mut |e| {
                if let HirExprKind::Block(b) = &mut e.kind {
                    store(b);
                }
            });
        }
        Ok(Some(new.into_iter().map(|(b, _)| b).collect()))
    }

    /// Build and start the host in the background (the first cell
    /// compiles meanwhile).
    fn start_host(&mut self) {
        if self.host.is_none() && self.starting.is_none() {
            let (dir, opts) = (self.tmp.path().to_owned(), self.opts.clone());
            self.starting = Some(std::thread::spawn(move || start_host(&dir, &opts)));
        }
    }

    /// Record `fns`. `fresh` replaced earlier definitions, so everything
    /// compiled against those (transitively) is stale.
    fn learn(&mut self, fresh: Vec<String>, fns: Vec<ReplFn>) {
        let mut gone: HashSet<String> = fresh.into_iter().collect();
        loop {
            let stale: Vec<String> = self
                .known
                .iter()
                .filter(|(_, (_, refs))| refs.iter().any(|r| gone.contains(r)))
                .map(|(k, _)| k.clone())
                .collect();
            if stale.is_empty() {
                break;
            }
            for k in stale {
                self.known.remove(&k);
                gone.insert(k);
            }
        }
        for f in fns {
            self.known.insert(f.name, (f.hash, f.refs));
        }
    }

    /// The host, started on first use.
    fn host(&mut self) -> Option<&mut Host> {
        if self.host.is_none() {
            self.host = match self.starting.take() {
                Some(h) => h.join().ok().flatten(),
                None => start_host(self.tmp.path(), &self.opts),
            };
        }
        self.host.as_mut()
    }

    /// Link the native packages' libraries, then the prebuilt
    /// dependencies `r` uses, into the host, once each.
    fn link_archives(&mut self, r: &CompileResult) {
        let mut files = native_files(&self.opts);
        files.extend(
            r.modules
                .first()
                .into_iter()
                .flat_map(|m| m.archives.clone()),
        );
        for a in &files {
            if self.linked.contains(a) {
                continue;
            }
            let Some(host) = self.host() else { return };
            let path = a.display().to_string();
            let reply = (|| {
                writeln!(host.ctl, "lib {}", path.len())?;
                host.ctl.write_all(path.as_bytes())?;
                let mut line = String::new();
                BufReader::new(&host.ctl).read_line(&mut line)?;
                Ok::<_, std::io::Error>(line)
            })();
            if reply.is_ok_and(|l| l.starts_with("ok")) {
                self.linked.insert(a.clone());
            } else {
                eprintln!("error: cannot link {path} into the session");
            }
        }
    }

    /// Send the host (started on first use) `cell <len>` and the
    /// cell's LLVM IR; its shown value.
    fn load(&mut self, ir: &[u8]) -> Loaded {
        self.host()?;
        let host = self.host.as_mut()?;
        let _ = std::io::stdout().flush();
        let reply = (|| {
            writeln!(host.ctl, "cell {}", ir.len())?;
            host.ctl.write_all(ir)?;
            let mut rd = BufReader::new(&host.ctl);
            let mut line = String::new();
            // `pid <n>`: the process the cell runs in.
            rd.read_line(&mut line)?;
            let pid = line
                .strip_prefix("pid ")
                .and_then(|p| p.trim().parse().ok());
            self.running.store(pid.unwrap_or(0), Ordering::Relaxed);
            line.clear();
            rd.read_line(&mut line)?;
            self.running.store(0, Ordering::Relaxed);
            let Some(n) = line.strip_prefix("ok ") else {
                return Ok(line);
            };
            let mut value = vec![0; n.trim().parse().unwrap_or(0)];
            rd.read_exact(&mut value)?;
            Ok::<_, std::io::Error>(format!("ok {}", String::from_utf8_lossy(&value)))
        })();
        match reply {
            Ok(r) if r.starts_with("ok ") => Some(r[3..].to_owned()),
            Ok(r) if r == "err\n" => {
                eprintln!("error: the cell failed; the session is as it was before it");
                None
            }
            _ => {
                eprintln!("error: the session host is gone; the bindings are lost");
                self.host = None;
                self.linked.clear();
                self.bindings.clear();
                self.statics.clear();
                self.any_types.clear();
                self.gc_tables.clear();
                self.known.clear();
                None
            }
        }
    }

    /// Front end; errors unless `quiet`. Locations map to `<repl>:line:col`
    /// or the loaded program's own.
    fn compile(&self, src: &str, quiet: bool) -> Option<CompileResult> {
        let (s, path) = self.compiler();
        // Added first so the views are in place while HIR lowering
        // writes caller locations (`#[track_caller]`).
        let mut map = SourceMap::new();
        let session = map.add(path.display().to_string(), src.to_owned());
        let regions = self.regions(src, &mut map);
        for r in &regions {
            let (line, col) = line_col(src, r.off);
            let last =
                line + u32::try_from(src[r.off..r.off + r.len].matches('\n').count()).ok()?;
            let at = line_col(&r.text, r.at);
            map.view(
                session,
                r.path.clone(),
                (line, col + 1),
                last,
                (at.0, at.1 + 1),
            );
        }
        let (text, at) = (src.to_owned(), path.clone());
        let r = match crate::analysis::isolate(move || s.compile_source(&text, &at, map)) {
            Ok(r) => r,
            Err(e) => {
                SourceMap::new().report(&crate::analysis::ice(&e));
                return None;
            }
        };
        if !r.has_errors() {
            return Some(r);
        }
        // A diverging expression cell (`panic(..)`) has no value to show:
        // `eval` runs it again as a statement.
        let never = format!("`{VALUE}`: the initializer never produces a value");
        if let [d] = r.errors.as_slice() {
            if d.message.contains(&never) {
                self.diverged.set(true);
                return None;
            }
        }
        if !quiet {
            let path = path.display().to_string();
            for d in &r.errors {
                r.report(&self.package_hint(localize(d, session, &path, &regions)));
            }
        }
        None
    }

    /// `d`, with a hint when the name it cannot find is one the package
    /// the session runs in (not loaded) defines.
    fn package_hint(&self, d: RokDiagnostic) -> RokDiagnostic {
        let Some(dir) = &self.package else { return d };
        let name = d
            .message
            .strip_prefix("cannot find ")
            .filter(|m| m.ends_with("` in this scope"))
            .and_then(|m| m.split('`').nth(1))
            .map(str::to_owned);
        match name {
            Some(n) if defined_in(dir, &n) => d.with_help(format!(
                "`{n}` is defined in this package; load this package: `rok repl .`"
            )),
            _ => d,
        }
    }

    /// A compiler session for session sources, and the path they stand at.
    fn compiler(&self) -> (Session, PathBuf) {
        let mut s = Session::new();
        for b in &self.bootstrap {
            s.add_bootstrap_file(b.clone());
        }
        for (n, p) in &self.externs {
            s.add_extern(n.clone(), p.clone());
        }
        let path = self
            .program
            .as_ref()
            .map_or_else(|| self.tmp.path().join("repl.rok"), |p| p.session.clone());
        s.set_anonymous(self.program.as_ref().is_none_or(|p| p.anonymous));
        s.set_cache_library(true);
        (s, path)
    }

    /// The session source with `body` as the cell, and where `body`
    /// starts in it.
    pub fn source(&self, body: &str) -> (String, usize) {
        let src = self.session(&self.state, body, None);
        let at = if body.is_empty() {
            src.len()
        } else {
            src.len() - body.len() - "\n}\n".len()
        };
        (src, at)
    }

    /// Check a session source (`source`) for the tools, reporting
    /// nothing: the analysis and the path its spans name.
    pub fn analyze(&self, src: &str) -> (Analysis, String) {
        let (s, path) = self.compiler();
        let a = crate::analysis::analyze(s, src, &path, &[]);
        (a, path.display().to_string())
    }

    /// Where the user's text is in the session source `src`: the cell
    /// being built, and the loaded program's items.
    fn regions(&self, src: &str, map: &mut SourceMap) -> Vec<Region> {
        let program = self.program.as_ref();
        let shown = |map: &mut SourceMap, p: &Program, off: usize, len: usize, at: usize| Region {
            off,
            len,
            file: map.add(p.path.clone(), p.text.to_string()),
            path: p.path.clone(),
            text: Arc::clone(&p.text),
            at,
        };
        let mut regions = Vec::new();
        // An expression cell is `{stmts}let VALUE = ({last});`.
        let (stmts, last) = split_last(&self.input);
        let last_off = src.rfind(last).filter(|_| !last.is_empty());
        let stmts_off = last_off
            .and_then(|o| src[..o].rfind(stmts))
            .filter(|_| !stmts.is_empty());
        for (off, part, at) in [(stmts_off, stmts, 0), (last_off, last, stmts.len())] {
            let Some(off) = off else { continue };
            let len = part.len();
            regions.push(match program.and_then(|p| Some((p, p.cell?))) {
                Some((p, cell)) => shown(map, p, off, len, cell + at),
                None => Region {
                    off,
                    len,
                    file: map.add(REPL_FILE.to_owned(), self.input.clone()),
                    path: REPL_FILE.to_owned(),
                    text: Arc::from(self.input.as_str()),
                    at,
                },
            });
        }
        if let Some(p) = program {
            for (item, at) in &p.items {
                if let Some(off) = src.find(item.as_str()) {
                    regions.push(shown(map, p, off, item.len(), *at));
                }
            }
        }
        regions
    }

    /// `repl <path>`: load `path` (`anonymous`: a lone file); `false` on
    /// failure. Checked whole first. Items join the session; top-level
    /// statements run as cells split after each top-level `let`, so a
    /// failure keeps earlier bindings. An unusable binding is dropped if
    /// nothing later names it, else the statements up to its last use run
    /// as one cell. A `fn main` program runs as one cell and `main` then
    /// leaves the session.
    pub fn load_program(&mut self, path: &std::path::Path, anonymous: bool) -> bool {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: cannot read {}: {e}", path.display());
                return false;
            }
        };
        self.loaded = Some((path.to_owned(), anonymous));
        let shown = path.display().to_string();
        let (tokens, _) = rokc_lexer::Lexer::new(&text, shown.clone()).lex_all();
        let tops = rokc_parser::parse_program(&tokens, shown.clone()).0.tops;
        let session = if anonymous {
            self.tmp.path().join("repl.rok")
        } else {
            path.with_file_name("repl.rok")
        };
        // Top-level elements: (start, end, is_item).
        let tops: Vec<(usize, usize, bool)> = tops
            .iter()
            .enumerate()
            .map(|(i, &(start, item))| {
                let end = tops.get(i + 1).map_or(text.len(), |t| t.0 as usize);
                let start = start as usize;
                (start, start + text[start..end].trim_end().len(), item)
            })
            .collect();
        let mut items = Vec::new();
        // The statements' text with the items blanked out (newlines
        // kept): offsets in it are offsets in the file.
        let mut body = text.clone().into_bytes();
        for &(start, end, _) in tops.iter().filter(|t| t.2) {
            let item = text[start..end].to_owned();
            let key = match classify(&item) {
                Cell::Item(k) => k,
                _ => None,
            };
            items.push((key, item, start));
            for b in &mut body[start..end] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
        let body = String::from_utf8_lossy(&body).into_owned();
        let stmts: Vec<(usize, usize)> = tops
            .iter()
            .filter(|t| !t.2)
            .map(|&(start, end, _)| (start, end))
            .collect();
        // One cell per stretch ending at a `let` (or the last statement).
        let mut cells: Vec<(usize, usize)> = Vec::new();
        let mut from = None;
        for (i, &(start, end)) in stmts.iter().enumerate() {
            let from_here = *from.get_or_insert(start);
            if matches!(classify(&text[start..end]), Cell::Let) || i + 1 == stmts.len() {
                cells.push((from_here, end));
                from = None;
            }
        }
        self.program = Some(Program {
            session,
            anonymous,
            path: shown,
            text: Arc::from(text.as_str()),
            items: items.iter().map(|(_, t, at)| (t.clone(), *at)).collect(),
            cell: None,
        });
        let mut next = self.state.clone();
        // Its items replace the session's of the same name (`:reload`).
        next.items
            .retain(|(k, _)| k.is_none() || !items.iter().any(|(n, ..)| n == k));
        next.items.extend(items.into_iter().map(|(k, t, _)| (k, t)));
        let all = match (cells.first(), cells.last()) {
            (Some(f), Some(l)) => &body[f.0..l.1],
            _ => "",
        };
        self.set_cell(all, cells.first().map(|c| c.0));
        let checked = self
            .compile(&self.session(&next, all, None), false)
            .is_some();
        self.set_cell("", None);
        if !checked {
            if let Some(p) = &mut self.program {
                p.items.clear();
            }
            return false;
        }
        let main = Some("fn main".to_owned());
        if next.items.iter().any(|(k, _)| *k == main) {
            self.state = next.clone();
            next.items.retain(|(k, _)| *k != main);
            let ok = self.run_cell("", None, false, &[]).is_ok();
            self.state = next;
            return ok;
        }
        self.state = next;
        let mut ok = true;
        let mut i = 0;
        while ok && i < cells.len() {
            // Cells `i..=j` run as one; `may_drop`: bindings in them that
            // later cells cannot use and need not.
            let (mut j, mut may_drop) = (i, Vec::new());
            loop {
                let text = &body[cells[i].0..cells[j].1];
                self.set_cell(text, Some(cells[i].0));
                match self.run_cell(text, None, false, &may_drop) {
                    Ok(_) => {
                        self.history.push(Saved {
                            stmts: text.trim().to_owned(),
                            declares: self.declared(),
                            ..Saved::default()
                        });
                        break;
                    }
                    // Merged through the last cell that names it.
                    Err(Fail::Unkept(name, _)) => {
                        match (j + 1..cells.len())
                            .rev()
                            .find(|&k| names(&body[cells[k].0..cells[k].1], &name))
                        {
                            Some(k) => j = k,
                            None => may_drop.push(name),
                        }
                    }
                    Err(Fail::Reported) => {
                        ok = false;
                        if j + 1 < cells.len() {
                            eprintln!("note: the rest of the program did not run");
                        }
                        break;
                    }
                }
            }
            i = j + 1;
        }
        self.set_cell("", None);
        ok
    }

    /// The cell being built is `text`, at `at` in the loaded program.
    fn set_cell(&mut self, text: &str, at: Option<usize>) {
        text.clone_into(&mut self.input);
        if let Some(p) = &mut self.program {
            p.cell = at;
        }
    }
}

/// The name a cell's text goes by in locations.
const REPL_FILE: &str = "<repl>";

/// Cells that end the session unless the session defines the name.
const EXIT: [&str; 4] = ["exit", "quit", "exit()", "quit()"];

/// The cells of a multi-line text: lines until the input is complete
/// (`incomplete`), as the terminal REPL reads them.
pub(crate) fn split_cells(code: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut buf = String::new();
    for line in code.lines() {
        buf.push_str(line);
        buf.push('\n');
        if !incomplete(&buf) {
            let cell = std::mem::take(&mut buf);
            if !blank(&cell) {
                cells.push(cell.trim().to_owned());
            }
        }
    }
    if !blank(&buf) {
        cells.push(buf.trim().to_owned());
    }
    cells
}

/// An item's text before its body or `;`, on one line, without its
/// attributes and comments: `fn f(x: i32) -> i32`, `const N: i32`.
fn header(item: &str) -> String {
    let rest = past_attrs(item);
    let mut depth = 0i32;
    let mut end = rest.len();
    let bytes = rest.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'{' | b';' if depth == 0 => {
                end = i;
                break;
            }
            // `=` alone, not `==`, `=>`, `<=`, `>=` or `!=`.
            b'=' if depth == 0
                && !matches!(bytes.get(i + 1), Some(b'=' | b'>'))
                && !matches!(
                    i.checked_sub(1).map(|j| bytes[j]),
                    Some(b'=' | b'<' | b'>' | b'!')
                ) =>
            {
                end = i;
                break;
            }
            _ => {}
        }
    }
    rest[..end].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `stmts` with its top-level `let name =` (or `let mut name =`) turned
/// into the assignment `name =`; `None` without one.
fn assign_instead(stmts: &str, name: &str) -> Option<String> {
    use rokc_lexer::TokenKind as T;
    let (tokens, _) = rokc_lexer::Lexer::new(stmts, REPL_FILE).lex_all();
    let kind = |i: usize| tokens.get(i).map(|t| &t.kind);
    let mut depth = 0i32;
    for (i, t) in tokens.iter().enumerate() {
        match &t.kind {
            T::LParen | T::LBracket | T::LBrace | T::HashLBracket | T::HashBangLBracket => {
                depth += 1;
            }
            T::RParen | T::RBracket | T::RBrace => depth -= 1,
            T::Let if depth == 0 => {
                let j = i + 1 + usize::from(matches!(kind(i + 1), Some(T::Ident(m)) if m == "mut"));
                if matches!(kind(j), Some(T::Ident(n)) if n == name) && kind(j + 1) == Some(&T::Eq)
                {
                    let (from, to) = (t.span.start as usize, tokens[j].span.start as usize);
                    return Some(format!("{}{}", &stmts[..from], &stmts[to..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether a `.rok` file under `dir` defines an item `name`.
fn defined_in(dir: &std::path::Path, name: &str) -> bool {
    use rokc_lexer::TokenKind as T;
    let files = rokc_interface::source_files(dir);
    files
        .iter()
        .filter(|f| f.extension().is_some_and(|x| x == "rok"))
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .any(|text| {
            let (tokens, _) = rokc_lexer::Lexer::new(&text, "").lex_all();
            tokens.windows(2).any(|w| {
                let item = match &w[0].kind {
                    T::Fn | T::Struct | T::Enum | T::Trait => true,
                    T::Ident(k) => matches!(k.as_str(), "const" | "static" | "type" | "union"),
                    _ => false,
                };
                item && matches!(&w[1].kind, T::Ident(n) if n == name)
            })
        })
}

/// Whether `text` has the identifier `name`.
fn names(text: &str, name: &str) -> bool {
    let (tokens, _) = rokc_lexer::Lexer::new(text, "").lex_all();
    tokens
        .iter()
        .any(|t| matches!(&t.kind, rokc_lexer::TokenKind::Ident(n) if n == name))
}

/// User text inside the generated session source, and where it shows.
struct Region {
    /// Byte offset and length in the session source.
    off: usize,
    len: usize,
    /// The file it shows in (registered id, path, text) and its offset there.
    file: u32,
    path: String,
    text: Arc<str>,
    at: usize,
}

/// 1-based line of byte `off` in `src`, and how many chars precede it
/// on that line.
fn line_col(src: &str, off: usize) -> (u32, u32) {
    let before = &src[..off];
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    (
        u32::try_from(before.matches('\n').count() + 1).unwrap_or(0),
        u32::try_from(before[start..].chars().count()).unwrap_or(0),
    )
}

/// `d` as the user sees it: spans in the user's text move to their file
/// (`regions`); spans in the generated wrapper are dropped.
fn localize(d: &RokDiagnostic, session: u32, path: &str, regions: &[Region]) -> RokDiagnostic {
    let mut d = d.clone();
    // `None`: in the wrapper. A span touching a region's edge (the `(`
    // around an expression cell) snaps into it.
    let map = |s: Span| -> Option<Span> {
        if s.file != session {
            return Some(s);
        }
        let (start, end) = (s.start as usize, (s.end as usize).max(s.start as usize));
        let r = regions
            .iter()
            .find(|r| (r.off..r.off + r.len).contains(&start))
            .or_else(|| {
                regions
                    .iter()
                    .find(|r| start + 1 >= r.off && start <= r.off + r.len)
            })?;
        let shown = |b: usize| r.at + b.saturating_sub(r.off).min(r.len);
        let (start, end) = (shown(start), shown(end));
        let (line, col) = line_col(&r.text, start);
        Some(Span {
            start: u32::try_from(start).ok()?,
            end: u32::try_from(end).ok()?,
            line,
            col: col + 1,
            file: r.file,
        })
    };
    if d.file.as_deref() == Some(path) {
        d.file = None;
    }
    if let Some(p) = d.primary.take() {
        d.primary = map(p.span).map(|span| Box::new(Label { span, ..*p }));
    }
    d.secondary = std::mem::take(&mut d.secondary)
        .into_iter()
        .filter_map(|l| {
            Some(Label {
                span: map(l.span)?,
                ..l
            })
        })
        .collect();
    d.message = d
        .message
        .replace(&format!("`let {VALUE}`"), "the expression");
    d
}

/// Native libraries for the host, in load order: shared ones (runtime
/// files; off Windows also `dylib`s in `-L` dirs), then static ones after
/// those they use.
fn native_files(opts: &BuildOptions) -> Vec<PathBuf> {
    let find = |f: String| {
        opts.lib_dirs
            .iter()
            .map(|d| d.join(&f))
            .find(|p| p.is_file())
    };
    let dylib = |n: &str| {
        let ext = if cfg!(target_vendor = "apple") {
            "dylib"
        } else {
            "so"
        };
        (!cfg!(windows)).then(|| format!("lib{n}.{ext}"))
    };
    let archive = |n: &str| {
        Some(if cfg!(windows) {
            format!("{n}.lib")
        } else {
            format!("lib{n}.a")
        })
    };
    let mut out: Vec<PathBuf> = opts.runtime_files.iter().rev().cloned().collect();
    for want_static in [false, true] {
        for (n, _) in opts.libs.iter().rev().filter(|l| l.1 == want_static) {
            let file = if want_static { archive(n) } else { dylib(n) };
            if let Some(p) = file.and_then(find) {
                push_new(&mut out, p);
            }
        }
    }
    out
}

fn push_new<T: PartialEq>(to: &mut Vec<T>, x: T) {
    if !to.contains(&x) {
        to.push(x);
    }
}

/// Build and start the session host.
fn start_host(dir: &std::path::Path, opts: &BuildOptions) -> Option<Host> {
    if !rokc_interface::repl_host() {
        eprintln!(
            "error: the REPL runs on macOS, Linux glibc and Windows (arm64, x86_64) only \
             (a musl rokc is static: its session host cannot bind cells)"
        );
        return None;
    }
    let obj = dir.join("host.o");
    let exe = dir.join(format!("host{}", std::env::consts::EXE_SUFFIX));
    if let Err(e) = std::fs::write(&obj, HOST_OBJ) {
        eprintln!("error: cannot write {}: {e}", obj.display());
        return None;
    }
    if let Err(d) = link_host(&obj, opts, &exe) {
        SourceMap::new().report(&d);
        return None;
    }
    let (ctl, theirs) = match channel() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot make the host channel: {e}");
            return None;
        }
    };
    let mut cmd = Command::new(&exe);
    cmd.stdin(theirs);
    #[cfg(windows)]
    {
        // Its own group: `interrupt`'s Ctrl-Break reaches it alone.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NEW_PROCESS_GROUP);
    }
    match cmd.spawn() {
        Ok(child) => Some(Host {
            ctl,
            _job: Job::ending(&child),
            _child: Reaped(child),
        }),
        Err(e) => {
            eprintln!("error: cannot run {}: {e}", exe.display());
            None
        }
    }
}

/// Whether a value of `ty` can hold an `any` value (`seen` cuts
/// recursive types).
fn holds_any(hir: &HirModule, ty: &HirTy, seen: &mut Vec<HirTy>) -> bool {
    if seen.contains(ty) {
        return false;
    }
    seen.push(ty.clone());
    let mut any = |ts: &[HirTy]| ts.iter().any(|t| holds_any(hir, t, seen));
    match ty {
        HirTy::Any | HirTy::AnyTrait(_) => true,
        HirTy::Sum(ts) | HirTy::Tuple(ts) => any(ts),
        HirTy::Array(t, _) | HirTy::GcBuf(t) => any(std::slice::from_ref(t)),
        HirTy::Throws { ok, err } => any(&[(**ok).clone(), (**err).clone()]),
        HirTy::Struct(id) => hir.structs.iter().filter(|s| s.id == *id).any(|s| {
            let fields: Vec<HirTy> = s.fields.iter().map(|f| f.ty.clone()).collect();
            any(&fields)
        }),
        HirTy::Enum(id) => hir.enums.iter().filter(|e| e.id == *id).any(|e| {
            let payloads: Vec<HirTy> = e
                .variants
                .iter()
                .filter_map(|v| v.payload_ty.clone())
                .collect();
            any(&payloads)
        }),
        _ => false,
    }
}

/// The functions that may hold the session's top level: `main`, then the
/// coroutine bodies it starts (a top-level `await` makes `main` run its
/// body on the executor, 4.16.5).
fn top_fns(hir: &HirModule) -> Vec<usize> {
    let Some(main) = hir.functions.iter().position(|f| f.name == "main") else {
        return Vec::new();
    };
    let mut out = vec![main];
    if let Some(body) = &hir.functions[main].body {
        rokc_hir::each_block(body, &mut |e| {
            if let HirExprKind::CoroNew { body_fn, .. } = &e.kind {
                out.extend(hir.functions.iter().position(|f| f.id == *body_fn));
            }
        });
    }
    out
}

/// Whether `s` is the cell's block: the block expression at offset `at`
/// of the session source.
const fn is_cell(s: &HirStmt, at: u32) -> bool {
    matches!(s, HirStmt::Expr(e) if e.span.start == at && matches!(e.kind, HirExprKind::Block(_)))
}

/// In `f`, the statements before the cell's block (`is_cell`) in the
/// block holding it, and the cell's block.
fn cell_scope(f: &HirFunction, at: u32) -> Option<(&[HirStmt], &HirBlock)> {
    fn split(scope: &HirBlock, at: u32) -> Option<(&[HirStmt], &HirBlock)> {
        let i = scope.stmts.iter().position(|s| is_cell(s, at))?;
        match &scope.stmts[i] {
            HirStmt::Expr(HirExpr {
                kind: HirExprKind::Block(b),
                ..
            }) => Some((&scope.stmts[..i], b)),
            _ => None,
        }
    }
    let body = f.body.as_ref()?;
    let mut found = split(body, at);
    rokc_hir::each_block(body, &mut |e| {
        if let (None, HirExprKind::Block(b)) = (&found, &e.kind) {
            found = split(b, at);
        }
    });
    found
}

/// The type of the last top-level binding `name` of the session (`main`
/// or its coroutine body), as written.
fn binding_ty(r: &CompileResult, name: &str) -> Option<String> {
    let hir: &HirModule = &r.modules.first()?.hir;
    let local = top_fns(hir).into_iter().find_map(|f| {
        hir.functions[f]
            .locals
            .iter()
            .rev()
            .find(|l| l.name.as_deref() == Some(name))
    })?;
    Some(match &local.ty {
        HirTy::Unit => "()".to_owned(),
        t => ty_name(hir, t),
    })
}

/// `text` past its leading attributes and line comments, which belong to
/// the item after them.
fn past_attrs(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        if rest.starts_with("#[") {
            let Some(end) = matching(rest, 1) else { break };
            rest = rest[end + 1..].trim_start();
        } else if rest.starts_with("//") {
            rest = rest.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else {
            return rest;
        }
    }
    rest
}

fn classify(cell: &str) -> Cell {
    let rest = past_attrs(cell);
    let words: Vec<&str> = rest
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(4)
        .collect();
    // A block's first word is inside it: `{ let q = 2; q * 3 }` is a value.
    let first = if rest.starts_with('{') {
        ""
    } else {
        words.first().copied().unwrap_or("")
    };
    let item_kw = [
        "fn", "struct", "enum", "union", "trait", "impl", "use", "mod", "type", "const", "static",
        "extern", "pub", "async", "unsafe",
    ];
    if item_kw.contains(&first) && !(first == "unsafe" && rest[6..].trim_start().starts_with('{')) {
        // `fn f` / `struct S` / `pub fn f` name what a redefinition replaces.
        let named = words.windows(2).find(|w| {
            [
                "fn", "struct", "enum", "union", "trait", "type", "const", "static",
            ]
            .contains(&w[0])
        });
        return Cell::Item(named.map(|w| format!("{} {}", w[0], w[1])));
    }
    if first == "let" {
        return Cell::Let;
    }
    if cell.ends_with(';') {
        Cell::Stmt
    } else {
        Cell::Expr
    }
}

/// `cell` split before its last top-level element.
fn split_last(cell: &str) -> (&str, &str) {
    let (tokens, _) = rokc_lexer::Lexer::new(cell, REPL_FILE).lex_all();
    let tops = rokc_parser::parse_program(&tokens, REPL_FILE).0.tops;
    cell.split_at(tops.last().map_or(0, |t| t.0 as usize))
}

/// Byte index of the bracket closing the one at `open`.
fn matching(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices().skip(open) {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The input needs more lines (907.8 `...` continuation): an open
/// bracket, string or block comment, a last token that cannot end a
/// statement, or an item header still waiting for its body.
pub(crate) fn incomplete(src: &str) -> bool {
    use rokc_lexer::TokenKind as T;
    // A command is one line (`:load .`).
    if src.trim_start().starts_with(':') {
        return false;
    }
    let (tokens, errors) = rokc_lexer::Lexer::new(src, REPL_FILE).lex_all();
    if errors
        .iter()
        .any(|e| e.message.starts_with("unterminated") && !e.message.contains("char"))
    {
        return true;
    }
    // A doc comment waits for its item.
    let last = src.lines().rev().map(str::trim).find(|l| !l.is_empty());
    if last.is_some_and(|l| l.starts_with("///") && !l.starts_with("////")) {
        return true;
    }
    let tokens = tokens.split_last().map_or(&[][..], |(_, t)| t);
    let mut depth = 0i32;
    // The last top-level statement starts after a `;` or a closing `}`.
    let mut start = 0;
    for (i, t) in tokens.iter().enumerate() {
        match t.kind {
            T::LParen | T::LBracket | T::LBrace | T::HashLBracket | T::HashBangLBracket => {
                depth += 1;
            }
            T::RParen | T::RBracket | T::RBrace => {
                depth -= 1;
                if depth == 0 && t.kind == T::RBrace {
                    start = i + 1;
                }
            }
            T::Semi if depth == 0 => start = i + 1,
            _ => {}
        }
    }
    if depth > 0 {
        return true;
    }
    tokens.last().is_some_and(|t| continues(&t.kind)) || item_header(&tokens[start..])
}

/// `cell` with the `;` its last top-level `let` or assignment may lack
/// (907.8).
fn close_stmt(cell: &str) -> String {
    use rokc_lexer::TokenKind as T;
    let last = split_last(cell).1;
    let (tokens, _) = rokc_lexer::Lexer::new(last, REPL_FILE).lex_all();
    // A place starts an assignment; `using g = ..`, `if let p = ..` and
    // `static S: T = ..` hold an `=` without being one.
    let place = match tokens.first().map(|t| &t.kind) {
        Some(T::Ident(w)) => !matches!(w.as_str(), "static" | "const" | "type" | "extern"),
        Some(T::SelfLower | T::Star | T::LParen) => true,
        _ => false,
    };
    let mut depth = 0i32;
    let mut assigns = false;
    let scan = if place { &tokens[..] } else { &[] };
    for t in scan {
        match t.kind {
            T::LParen | T::LBracket | T::LBrace | T::HashLBracket | T::HashBangLBracket => {
                depth += 1;
            }
            T::RParen | T::RBracket | T::RBrace => depth -= 1,
            T::Eq
            | T::PlusEq
            | T::MinusEq
            | T::StarEq
            | T::SlashEq
            | T::PercentEq
            | T::PercentPercentEq
            | T::AmpEq
            | T::PipeEq
            | T::CaretEq
            | T::ShlEq
            | T::ShrEq
                if depth == 0 =>
            {
                assigns = true;
            }
            _ => {}
        }
    }
    let open = tokens
        .iter()
        .rev()
        .nth(1)
        .is_some_and(|t| t.kind != T::Semi);
    if open && (assigns || matches!(classify(last), Cell::Let)) {
        // On a line of its own, outside the text diagnostics quote.
        format!("{cell}\n;")
    } else {
        cell.to_owned()
    }
}

/// `src` is only whitespace and comments.
pub(crate) fn blank(src: &str) -> bool {
    rokc_lexer::Lexer::new(src, REPL_FILE).lex_all().0.len() <= 1
}

/// `kind` needs a token after it: a binary operator, `.`, `,`, `=`,
/// `->`, `|`, or a keyword that leads into more.
fn continues(kind: &rokc_lexer::TokenKind) -> bool {
    use rokc_lexer::TokenKind as T;
    match kind {
        T::Ident(w) => matches!(w.as_str(), "in" | "mut" | "where"),
        // Not `>` / `>>`: they also close generics (`x as List<i32>`).
        T::Plus
        | T::Minus
        | T::Star
        | T::Slash
        | T::Percent
        | T::PercentPercent
        | T::EqEq
        | T::NotEq
        | T::Lt
        | T::LtEq
        | T::GtEq
        | T::AmpAmp
        | T::PipePipe
        | T::Bang
        | T::Amp
        | T::Pipe
        | T::Caret
        | T::Tilde
        | T::Shl
        | T::MathOp(_)
        | T::ExtOp(_)
        | T::DotPlus
        | T::DotMinus
        | T::DotStar
        | T::DotSlash
        | T::DotPercent
        | T::DotPercentPercent
        | T::DotEqEq
        | T::DotNotEq
        | T::DotLt
        | T::DotGt
        | T::DotLtEq
        | T::DotGtEq
        | T::DotAmp
        | T::DotPipe
        | T::DotAmpAmp
        | T::DotPipePipe
        | T::DotCaret
        | T::DotShl
        | T::DotShr
        | T::Eq
        | T::PlusEq
        | T::MinusEq
        | T::StarEq
        | T::SlashEq
        | T::PercentEq
        | T::PercentPercentEq
        | T::AmpEq
        | T::PipeEq
        | T::CaretEq
        | T::ShlEq
        | T::ShrEq
        | T::DotDotEq
        | T::Colon
        | T::ColonColon
        | T::Dot
        | T::QuestionDot
        | T::QuestionQuestion
        | T::Arrow
        | T::FatArrow
        | T::Comma
        | T::At
        | T::Let
        | T::If
        | T::Else
        | T::Match
        | T::While
        | T::Loop
        | T::For
        | T::As
        | T::Is
        | T::Ref
        | T::Try
        | T::Throw
        | T::Using => true,
        _ => false,
    }
}

/// `stmt` (balanced, no `;` and no block) is only attributes or an
/// item's header: `fn g(x: i32)` waits for `-> i32 {` on the next line.
fn item_header(stmt: &[rokc_lexer::Token]) -> bool {
    use rokc_lexer::TokenKind as T;
    let kind = |i: usize| stmt.get(i).map(|t| &t.kind);
    let mut i = 0;
    loop {
        match kind(i) {
            None => return i > 0,
            Some(T::HashLBracket | T::HashBangLBracket) => i = past_group(stmt, i),
            Some(T::Pub) if kind(i + 1) == Some(&T::LParen) => i = past_group(stmt, i + 1),
            Some(T::Pub | T::Async | T::Unsafe) => i += 1,
            Some(T::Ident(w)) if w == "extern" => {
                i += if matches!(kind(i + 1), Some(T::StrLit(_))) {
                    2
                } else {
                    1
                };
            }
            Some(T::Ident(w)) if w == "const" && kind(i + 1) == Some(&T::Fn) => i += 1,
            Some(T::Fn | T::Struct | T::Enum | T::Trait | T::Impl) => return true,
            Some(T::Ident(w)) => return w == "union" && matches!(kind(i + 1), Some(T::Ident(_))),
            Some(_) => return false,
        }
    }
}

/// Index past the bracket closing the one at `open`.
fn past_group(tokens: &[rokc_lexer::Token], open: usize) -> usize {
    use rokc_lexer::TokenKind as T;
    let mut depth = 0i32;
    for (i, t) in tokens.iter().enumerate().skip(open) {
        match t.kind {
            T::LParen | T::LBracket | T::HashLBracket | T::HashBangLBracket => depth += 1,
            T::RParen | T::RBracket => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return i + 1;
        }
    }
    tokens.len()
}

/// What a session loads before its first cell.
#[derive(Default)]
pub struct Start<'a> {
    /// The first cell (`repl <lib>`: `use <lib>::*;`).
    pub prelude: Option<&'a str>,
    /// A program (its root file, whether it is a lone file): `repl <path>`.
    pub program: Option<(&'a std::path::Path, bool)>,
    /// The source directory of the package the session runs in, not
    /// loaded (`Repl::package`).
    pub package: Option<PathBuf>,
}

impl Repl {
    /// Run `s`'s prelude, then load its program.
    pub fn start(&mut self, s: Start<'_>) {
        self.package = s.package;
        if let Some(p) = s.prelude {
            self.prelude = Some(p.to_owned());
            self.cell(p);
        }
        if let Some((path, anonymous)) = s.program {
            self.load_program(path, anonymous);
        }
    }
}

/// `rok repl` with the stdin it was started on, after `start`.
pub fn main(
    bootstrap: Vec<PathBuf>,
    externs: Vec<(String, PathBuf)>,
    deps_dir: PathBuf,
    opts: BuildOptions,
    interactive: bool,
    start: Start<'_>,
) -> i32 {
    match Repl::new(bootstrap, opts) {
        Ok(mut r) => {
            r.externs = externs;
            r.deps_dir = deps_dir;
            r.start(start);
            if interactive {
                r.run_tty();
                0
            } else {
                i32::from(!r.run(std::io::stdin().lock(), false))
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_classify_and_continue() {
        assert!(matches!(classify("1 + 2"), Cell::Expr));
        assert!(matches!(classify("println(\"x\");"), Cell::Stmt));
        assert!(matches!(classify("let mut x = 1;"), Cell::Let));
        assert!(matches!(classify("let (a, b) = (1, 2);"), Cell::Let));
        assert!(
            matches!(classify("#[derive(Debug)]\nstruct P { x: i32; }"), Cell::Item(Some(k)) if k == "struct P")
        );
        assert!(matches!(classify("pub fn f() {}"), Cell::Item(Some(k)) if k == "fn f"));
        assert!(matches!(classify("impl P {}"), Cell::Item(None)));
        assert!(incomplete("let x =\n"));
        assert!(incomplete("fn f() {\n"));
        assert!(incomplete("#[derive(Debug)]\n"));
        assert!(incomplete("let s = \"a\n"));
        assert!(!incomplete("let x =\n42i32;\n"));
        assert!(!incomplete("f(\"(\")\n"));
    }

    #[test]
    fn cells_end_by_tokens() {
        let complete = [
            // Brackets in literals and comments do not count.
            "'('",
            "'\"'",
            "/* ( */ 1",
            "1 + 1 /* c */",
            "f(x) // (",
            "r\"(\"",
            // A balanced line ends the cell; `;` is the compiler's to ask for.
            "let x = 1",
            "f(); let x = 1",
            "x as List<i32>",
            "0..",
            "x?",
            "return",
            "fn g(x: i32)\n-> i32 { x }",
            "struct P(i32);",
            "const N: i32 = 1",
            "union",
            "(1, 2)",
            "\"s\"",
        ];
        let open = [
            "1 /* (",
            "f\"{x}",
            // A last token that needs more.
            "1 +",
            "xs.",
            "f(a,",
            "a ->",
            "x ==",
            "if a {} else",
            "for x in",
            "|x|",
            "a &&",
            // Item headers wait for their body.
            "fn g(x: i32)",
            "pub(crate) async fn g<T>(x: T) -> T",
            "struct P",
            "#[derive(Debug)]\nenum E",
            "impl Display for P",
            "trait T",
            "fn f() {}\nfn g()",
        ];
        for t in complete {
            assert!(!incomplete(&format!("{t}\n")), "{t}");
        }
        for t in open {
            assert!(incomplete(&format!("{t}\n")), "{t}");
        }
        assert!(blank("// c\n/* d */\n"));
        assert!(!blank("1 // c\n"));
    }

    #[test]
    fn a_last_let_or_assignment_gets_its_semicolon() {
        for (cell, closed) in [
            ("let x = 1", true),
            ("f(); let x = 1 // c", true),
            ("let x = if c { 1 } else { 2 }", true),
            ("k += 2", true),
            ("v = 5", true),
            ("xs[0] -= 1", true),
            ("let x = 1;", false),
            ("let x = 1; x", false),
            ("f(1)", false),
            ("f(a = 1)", false),
            ("x == 1", false),
            ("while true { let y = 1 }", false),
            ("{ let q = 2; q * 3 }", false),
            ("using g = m.lock() { g.get() }", false),
            ("if let Some(x) = o { x } else { 0 }", false),
            ("(a, b) = (b, a)", true),
        ] {
            let want = if closed {
                format!("{cell}\n;")
            } else {
                cell.to_owned()
            };
            assert_eq!(close_stmt(cell), want);
        }
        assert!(matches!(classify("{ let q = 2; q * 3 }"), Cell::Expr));
        assert!(matches!(classify("{ fn f() {} f() }"), Cell::Expr));
    }

    #[test]
    fn several_statements_end_at_the_last() {
        assert!(!incomplete("let a = 1; a + 1\n"));
        assert!(!incomplete("println(\"x\"); while true { break; }\n"));
        assert_eq!(
            split_last("println(\"x\"); while true { break; }").1,
            "while true { break; }"
        );
        assert_eq!(split_last("let a = 1; a + 1").1, "a + 1");
    }

    fn repl() -> Repl {
        Repl::new(Vec::new(), BuildOptions::default()).unwrap()
    }

    fn binding(name: &str, is_it: bool) -> Binding {
        Binding {
            name: name.to_owned(),
            sym: format!("__rok_repl_1_{name}"),
            ty: "i32".to_owned(),
            shown_ty: "i32".to_owned(),
            holds_any: false,
            is_it,
        }
    }

    fn saved(stmts: &str, expr: Option<&str>, it: bool, declares: &[(&str, &str)]) -> Saved {
        Saved {
            stmts: stmts.to_owned(),
            expr: expr.map(str::to_owned),
            it,
            declares: declares
                .iter()
                .map(|(n, t)| ((*n).to_owned(), (*t).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn doc_comments_and_commands_end_cells() {
        // A doc comment waits for its item, which it belongs to.
        assert!(incomplete("/// Doubles it.\n"));
        assert!(!incomplete(
            "/// Doubles it.\nfn twice(x: i32) -> i32 { x * 2 }\n"
        ));
        assert!(!incomplete("//// not a doc comment\n"));
        assert!(
            matches!(classify("/// Doubles it.\nfn twice(x: i32) -> i32 { x * 2 }"), Cell::Item(Some(k)) if k == "fn twice")
        );
        // A command is its line, even ending in `.` or an operator.
        assert!(!incomplete(":load .\n"));
        assert!(!incomplete(":type 1 +\n"));
        assert_eq!(
            split_cells("/// d\nfn f() {}\n:vars\nf()"),
            ["/// d\nfn f() {}", ":vars", "f()"]
        );
    }

    #[test]
    fn vars_shows_item_headers() {
        assert_eq!(
            header("/// Doubles it.\n#[inline]\nfn twice(x: i32) -> i32 { x * 2 }"),
            "fn twice(x: i32) -> i32"
        );
        assert_eq!(
            header("fn f(a: [i32; 3]) -> bool { a[0] == 1 }"),
            "fn f(a: [i32; 3]) -> bool"
        );
        assert_eq!(header("const N: i32 = 1;"), "const N: i32");
        assert_eq!(header("struct P(i32);"), "struct P(i32)");
        assert_eq!(
            header("#[derive(Debug)]\nstruct Q {\n  x: i32;\n}"),
            "struct Q"
        );
        assert_eq!(
            header("fn g(x: i32)\n    -> i32 where i32: Copy { x }"),
            "fn g(x: i32) -> i32 where i32: Copy"
        );
    }

    #[test]
    fn exit_and_it_yield_to_user_names() {
        let mut r = repl();
        assert!(!r.defines("exit") && !r.defines("it"));
        // The session's `it` is not the user's.
        r.bindings.push(binding("it", true));
        assert!(!r.defines("it"));
        r.bindings.push(binding("exit", false));
        assert!(r.defines("exit") && !r.defines("quit"));
        r.state
            .items
            .push((Some("fn quit".to_owned()), "fn quit() {}".to_owned()));
        assert!(r.defines("quit"));
        r.bindings.clear();
        r.state
            .items
            .push((None, "use rok::std::env::exit;".to_owned()));
        assert!(r.defines("exit"));
        // Without one the bare word ends the session, as `:q` does.
        let mut r = repl();
        assert!(!r.cell("exit"));
        assert!(!r.cell("quit()"));
        assert!(!r.cell(":q"));
        assert!(r.cell(":frob"));
        assert!(r.cell(":opt 9"));
        assert_eq!(r.opts.opt_level, BuildOptions::default().opt_level);
        assert!(r.cell(":opt 2"));
        assert_eq!(r.opts.opt_level, "2");
        assert!(r.cell(":reload"));
    }

    #[test]
    fn save_writes_a_script_that_declares_each_name_once() {
        let mut r = repl();
        r.state.items.push((
            Some("fn twice".to_owned()),
            "fn twice(x: i32) -> i32 { x * 2 }".to_owned(),
        ));
        r.history = vec![
            saved("let a = 1\n;", None, false, &[("a", "i32")]),
            // Read by the next cell: kept as `it`.
            saved("", Some("twice(a)"), true, &[("it", "i32")]),
            saved("", Some("it + 1"), true, &[("it", "i32")]),
            // Not read again: a statement.
            saved("", Some("it * 2"), true, &[("it", "i32")]),
            // Declared again with its type: an assignment.
            saved("let a = 5;", None, false, &[("a", "i32")]),
            // With another type: a block of its own.
            saved("let a = \"s\"; println(a);", None, false, &[("a", "str")]),
            saved("", Some("a"), true, &[("it", "str")]),
        ];
        assert_eq!(
            r.script(),
            "\
fn twice(x: i32) -> i32 { x * 2 }
let a = 1;
let it = twice(a);
it = it + 1;
it * 2;
a = 5;
{
let a = \"s\"; println(a);
a;
}
"
        );
        let path = r.tmp.path().join("s.rok");
        r.save(&path.display().to_string());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r.script());
        assert_eq!(
            assign_instead("f(); let b = 2; b", "b").as_deref(),
            Some("f(); b = 2; b")
        );
        assert_eq!(assign_instead("{ let b = 2; }", "b"), None);
        assert_eq!(assign_instead("let b: i32 = 2;", "b"), None);
    }

    #[test]
    fn package_names_get_a_hint() {
        let dir = rokc_core::tmp::test_dir("repl-hint");
        std::fs::create_dir_all(dir.join("src/geo")).unwrap();
        std::fs::write(
            dir.join("src/lib.rok"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/geo.rok"), "pub struct Point { x: i32; }\n").unwrap();
        let src = dir.join("src");
        assert!(defined_in(&src, "add") && defined_in(&src, "Point"));
        assert!(!defined_in(&src, "x"));
        let missing = |what: &str| {
            RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Resolve,
                Span::DUMMY,
                format!("cannot find {what} in this scope"),
            )
        };
        let mut r = repl();
        assert!(r.package_hint(missing("function `add`")).notes.is_empty());
        r.package = Some(src);
        assert_eq!(
            r.package_hint(missing("function `add`")).notes,
            ["help: `add` is defined in this package; load this package: `rok repl .`"]
        );
        assert!(r.package_hint(missing("value `nope`")).notes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
