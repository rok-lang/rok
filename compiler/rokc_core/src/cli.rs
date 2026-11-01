//! The `rokc` command line as text: `rokc help`, the per-command help
//! and the shell completion scripts all read it. `rok` renders its own
//! commands' help with the same [`CmdHelp`] table.

use crate::completions::{Spec, Values, section_flags};
use crate::term::{self, Depth, Role, Stream};

/// `rokc help`.
pub const ROKC_USAGE: &str = "\
usage: rokc [global options] <command> [options] [args]

commands:
  build [path]           build the package: the executable
                         target/<profile>/<name> of `src/main.rok`, the
                         library target/<profile>/lib<name>.a + <name>.rokhir
                         of `src/lib.rok`, or both (the executable uses the
                         library as `<name>`); <profile> is debug, or release
                         at -O2+ / -Os; a --target build goes under
                         target/<target>/
  run [path] [-- args]   build, then run; args after `--` are forwarded
  test [path] [filter]   build the `#[test]` harness, the library's then the
                         executable's; `rok test` runs it (filter: regex with
                         ^ $ . * + ?, or a substring)
  bench [path] [filter]  build (-O2 unless -O given) the `#[bench]` harness;
                         `rok bench` runs it and prints the time per
                         iteration (+/- deviation)
  check [path | -]       front end, lints and codegen, no native build
                         (`-`: source on stdin)
  lint [path]            front end and lints; exit 1 if a `deny` lint fires
  repl [path]            interactive session, each cell compiled and run
                         (:type <expr>, :reset, :exit; -q: no banner); a
                         program path runs in the session first, its
                         bindings and items staying in scope; a library is
                         loaded and `use`d
  lsp                    language server on stdio (`rok lsp` / `rokls` run it)
  jupyter --connection-file <file>
                         Jupyter kernel (Jupyter starts it; `rok jupyter
                         install` writes its kernelspec)
  fmt [path]             format .rok / .rokon files in place (file or directory)
  doc [path]             HTML API docs of the package's library (else of its
                         executable) and its --extern dependencies in
                         target/doc/
  clean [path]           remove the package's target/
  bindgen <header> [-- <clang args>]
                         Rok declarations of a C header (stdout, or -o) for
                         the --target, through libclang (in the toolchain's
                         lib/); clang args such as -I <dir> or -D <name>
  demangle <symbol>...   decode `_Y` symbols
  inspect [--dump] <file>
                         a prebuilt library's package, exports, generic
                         items and archive members (--dump: its whole
                         ROKHIR); <file> is <name>.rokhir or the
                         lib<name>.a beside it
  emit [path]            write an intermediate artifact (see --emit)
  profdata merge [-o <file>] <file | dir>...
                         merge the .profraw / .profdata files (a directory:
                         those in it) into one profile (default:
                         default.profdata) for -C profile-use
  profdata show [--all-functions] [--topn N] <file>
                         summarize a profile (with every function; with
                         the N hottest)
  completions bash|zsh|fish|powershell
                         print the shell's completion script for rokc
  version                print the version
  help                   print this help

path is a package directory (default: the current one), a file inside a
package (the nearest directory upward with `src/main.rok` or `src/lib.rok`),
or a lone `.rok` file outside a package's `src/`, which is built as an
anonymous executable package.

global options:
  -v, --verbose                stage timings
  -vv                          also the files read and the generic
                               instances made
  -q, --quiet                  no progress output
  --color=auto|always|never    color output (--no-color = never)
  --cwd=<path>                 change directory first
  --message-format=human|json  diagnostics as JSON lines on stdout
  -A|-W|-D <lint|group>        allow / warn / deny a lint (repeatable, later
                               wins; `#[allow(..)]` etc. in the source
                               override)

build options:
  -o, --output <path>          output file
  --emit=<kind>[,<kind>...]    source | hir | mir | mlir | llvmir | asm | obj |
                               expansions (source: the package's whole AST
                               after meta expansion and desugaring, as Rok
                               source; expansions: each meta expansion's
                               output) (written to
                               target/<profile>/build/<name>.<ext> unless -o)
  --target=<target>            a built-in target (--print target-list) or
                               a custom one's <name>.rokon file
  --release                    optimized build (same as -O2)
  -O0 | -O1 | -O2 | -O3 | -Os  optimization level (default -O0); -O2 / -O3 / -Os
                               build the release profile (no debug info, code
                               folded, symbols stripped), the others the
                               debug profile (debug info)
  -C overflow-checks[=yes|no]  integer overflow panics (default: on at -O0)
  -C backend=native|llvm       code generator (default: native at -O0, else LLVM)
  -C target-cpu=<cpu>|native   build for that CPU instead of the target's
                               baseline (native: this machine's); LLVM only
  -C target-feature=+f1,-f2,.. turn CPU features on / off (LLVM only); the
                               binary may not run on CPUs without them
  -C subsystem=console|windows Windows executable subsystem (default: console;
                               windows: a GUI program entered through WinMain)
  -C profile-generate[=<dir>]  instrument for profile-guided optimization: runs
                               write <dir>/default_%m.profraw (default dir: the
                               current one; LLVM_PROFILE_FILE overrides)
  -C profile-use=<file>        optimize with a profile from `rokc profdata merge`
                               (both select the LLVM backend at every level)
  -C cs-profile-generate[=<dir>]
                               with -C profile-use: instrument again after
                               inlining (context-sensitive); merge its runs
                               with that profile for the next -C profile-use
  --cfg <flag | key=value>     #[cfg] flag (repeatable)
  --print cfg                  print the active cfg (target keys, --cfg,
                               debug_assertions below -O1) and exit
  --print target-list          print the built-in targets and exit (an old
                               name: `<name> (deprecated: use <new>)`)
  --print target-spec          print the --target's spec as ROKON and exit
  --print lldbinit             print the lldb command that loads Rok's
                               formatters (sums, any) and exit
  --bootstrap <file>           library root to preload (repeatable; default:
                               <sysroot>/lib/rok/{core,std,bare}/src/lib.rok)
  --rokr <lib>                 runtime library: path, or name for -l<name>
                               (default: <sysroot>/lib/librokr.a)
  -L <dir>                     native library search directory, before
                               LIBRARY_PATH (repeatable)
  -l [static=|dylib=]<name>    link a native library, as `#[link]` does
                               (repeatable)
  --native-runtime <file>      a shared library the executable loads:
                               copied beside it, and the executable looks
                               in its own directory (repeatable)
  --extern <name>=<path>       dependency package: its directory or `src/lib.rok`
                               (prebuilt into target/<profile>/deps/ first,
                               each after the ones before it), or a prebuilt
                               <name>.rokhir / lib<name>.a; importable as
                               `use name::..`; another name for the same path
                               is an alias (repeatable; `rok` passes these)
  --extern-feature <name>=<f>  build source dependency <name> with
                               cfg feature=<f> (repeatable; `--cfg
                               feature=..` is the package's own)
  --name <name>                the package name (default: its directory's;
                               `rok` passes rok.rokon's `name`); a
                               library's is in its symbols
  --target-dir <dir>           instead of the package's target/ (without a
                               package: the system temp directory's
                               rokc-target-<uid>/, private to the user, a
                               cache trimmed to what was built in the last
                               week)
  -g                           debug info in a release build too (a debug
                               build always has it; macOS: a .dSYM next to
                               the executable)
  --keep                       keep the temporary build directory

check options:  --check-library (check every library body, reached or not)
test options:   --threads=N, --show-output,
                --format=human|json (json: one JSON line per test and a
                summary on stdout, the human text on stderr)
bench options:  --iters=N (calls per sample; default: calibrated to ~100ms),
                --samples=N (timed samples, default 10),
                --format=human|json|rokbench (json: as for tests;
                rokbench: one `#rokbench` line per benchmark; default
                human, piped or not)
harness:        --emit-harness <file> (write the built harness and its cases
                to <file> for `rok` to run, instead of naming it)
doc options:    --std (document the standard library too; without a path,
                only it, into ./target/doc/),
                --open (open the index in $BROWSER or the system's browser)
lint options:   --list
fmt options:    --check (exit 1 if a file would change), --diff

The sysroot is the directory above rokc's own bin/ (a toolchain:
$ROK_HOME/toolchains/<version>-<triple>/); a build in the repository uses
its library/ and the librokr.a beside rokc.

environment:
  ROK_CC            C compiler driver for the final link (default: rokc's
                    own lld)
  ROK_BACKEND       native | llvm: code generator, as -C backend
  ROK_TARGET_DIR    target/ location (default: inside the package)
  ROK_VERBOSE       1: same as --verbose
  ROK_TIME_PASSES   1: the wall time of each compiler pass on stderr
  ROK_TEST_THREADS  default for --threads (default: the CPU count)
  ROK_INCREMENTAL   where the front end keeps parses and lowered bodies
                    (default: target/<profile>/incremental/; 0: nowhere)
  ROK_INCREMENTAL_VERIFY
                    1: lower every body again and fail on a difference
  ROK_LLVM_CACHE    where bindgen also looks for libclang
  ROK_HOME          the toolchains, environments and caches (default:
                    ~/.rok)
  LIBRARY_PATH      native library search directories, after -L
  BROWSER           the browser doc --open starts
  ROK_REPL_TIME     1: the time of each phase of a REPL cell on stderr
  NO_COLOR          non-empty: --color=auto uses no color
  CLICOLOR_FORCE    non-empty and not 0: --color=auto uses color even when
                    the stream is not a terminal, unless NO_COLOR is set
  COLORTERM         truecolor or 24bit: 24-bit color
  TERM              dumb: --color=auto uses no color; *256color*: the
                    256-color palette
  LLVM_PROFILE_FILE read by a -C profile-generate program: where its raw
                    profile goes
  __ROK_CLI         rok: messages name `rok`, not rokc (`rok` sets it)
  __ROK_ENV, __ROK_ENV_LIBS
                    the environment `rok` runs a script in, and its
                    libraries (`rok` sets them)
  __ROK             the `rok` that REPL :add and the language server run
                    (`rok` sets it)
                    (run, test and bench keep these four out of the
                    program's environment)

Programs read ROKR_WORKERS, ROKR_THREAD_PER_CORE, ROKR_DYNAMIC_WORKERS,
ROKR_GC_* and ROKR_HEAP_DUMP (spec 7.6, 9.9.8).
";

/// One command's help. `{tool}` in `usage`, `about` and `examples`
/// reads as the program the user ran (`rok` or `rokc`).
#[derive(Clone, Copy, Debug)]
pub struct CmdHelp {
    pub name: &'static str,
    /// One line for the command list.
    pub summary: &'static str,
    /// Forms after the program name: `build [path] [options]`.
    pub usage: &'static [&'static str],
    /// A paragraph, wrapped.
    pub about: &'static str,
    /// Said only under `rokc`.
    pub rokc_note: &'static str,
    /// `(command line, what it does)`.
    pub examples: &'static [(&'static str, &'static str)],
    /// The command's own options: `(head, description)`.
    pub flags: &'static [(&'static str, &'static str)],
    /// Option sections of `ROKC_USAGE` that apply too.
    pub sections: &'static [&'static str],
}

impl CmdHelp {
    pub const EMPTY: Self = Self {
        name: "",
        summary: "",
        usage: &[],
        about: "",
        rokc_note: "",
        examples: &[],
        flags: &[],
        sections: &[],
    };
}

/// The program the user ran: `rok` when it runs rokc for them (it sets
/// `__ROK_CLI=rok`), else `rokc`.
#[must_use]
pub fn invoked_as() -> &'static str {
    if crate::handoff_var("ROK_CLI").is_some_and(|v| v == "rok") {
        "rok"
    } else {
        "rokc"
    }
}

/// This process's arguments without the program name, split at the first
/// `--`: those up to it (with it) as strings, the program's after it as
/// given. A non-UTF-8 argument before `--` is an error message.
pub fn os_args() -> Result<(Vec<String>, Vec<std::ffi::OsString>), String> {
    split_os_args(std::env::args_os().skip(1))
}

/// [`os_args`] of `args`.
pub fn split_os_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<(Vec<String>, Vec<std::ffi::OsString>), String> {
    let mut it = args.into_iter();
    let mut head = Vec::new();
    for a in it.by_ref() {
        let a = utf8_arg(head.len() + 1, a)?;
        let end = a == "--";
        head.push(a);
        if end {
            break;
        }
    }
    Ok((head, it.collect()))
}

/// Argument `n` (from 1) as a string: options, commands and paths must
/// be UTF-8; a program's arguments, after `--`, need not.
pub fn utf8_arg(n: usize, a: std::ffi::OsString) -> Result<String, String> {
    a.into_string().map_err(|a| {
        format!(
            "argument {n} (`{}`) is not valid UTF-8; only a program's arguments, after `--`, \
             may be other bytes",
            a.to_string_lossy()
        )
    })
}

/// Whether command-line word `w` names a script, not a command: it has a
/// `/` (Windows: or `\`) or ends in `.rok`, which no command does.
#[must_use]
pub fn looks_like_script(w: &str) -> bool {
    std::path::Path::new(w)
        .extension()
        .is_some_and(|x| x == "rok")
        || w.contains('/')
        || cfg!(windows) && w.contains('\\')
}

/// Words people type for a command: `(command, [synonyms])`.
pub const CMD_SYNONYMS: &[(&str, &[&str])] = &[
    ("build", &["compile", "make"]),
    ("run", &["exec", "start"]),
    ("fmt", &["format", "fix"]),
    ("install", &["i"]),
    ("remove", &["rm", "del", "delete"]),
    ("update", &["upgrade", "up"]),
    ("new", &["create"]),
    ("toolchain list", &["ls", "list"]),
    ("version", &["ver"]),
];

/// "; did you mean `x`?" for `name` among `cands` (synonyms first), or
/// nothing.
#[must_use]
pub fn did_you_mean<'a>(
    name: &str,
    cands: impl IntoIterator<Item = &'a str>,
    syn: &[(&str, &[&str])],
) -> String {
    crate::suggest::suggest_or_synonym(name, cands, syn)
        .map(|s| format!("; did you mean `{s}`?"))
        .unwrap_or_default()
}

/// rokc's commands, in `ROKC_USAGE` order.
pub const ROKC_COMMANDS: &[CmdHelp] = &[
    CmdHelp {
        name: "build",
        summary: "compile the package",
        usage: &["build [path] [options]"],
        about: "\
Build the package at <path> (default: the current directory). src/main.rok
becomes the executable target/<profile>/<name>, src/lib.rok the library
target/<profile>/lib<name>.a and <name>.rokhir. <profile> is debug, or
release with --release, -O2, -O3 or -Os. A lone .rok file outside a
package's src/ builds as a one-file program, its executable beside it
(hello.rok: hello) unless -o names another file.",
        examples: &[
            ("{tool} build", "debug build of the package here"),
            ("{tool} build --release", "optimized build"),
            (
                "{tool} build hello.rok -o hello",
                "one file, the executable named hello",
            ),
        ],
        sections: &["build options"],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "run",
        summary: "build, then run the program",
        usage: &["run [path] [options] [-- args]"],
        about: "\
Build the executable, then run it. Arguments after `--` go to the program,
and the exit code is the program's.",
        examples: &[
            ("{tool} run", "run the package here"),
            ("{tool} run hello.rok", "run one file"),
            (
                "{tool} run --release -- in.txt",
                "optimized, with in.txt as argument",
            ),
        ],
        sections: &["build options"],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "test",
        summary: "run the tests",
        usage: &["test [path] [filter] [options]"],
        about: "\
Build the `#[test]` functions of the library, then of the executable, and
run them. A filter keeps the tests whose name matches: a regex with
^ $ . * + ?, or a substring.",
        rokc_note: "rokc only builds the harness; `rok test` runs it.",
        examples: &[
            ("{tool} test", "every test"),
            ("{tool} test parse", "the tests whose name contains parse"),
            (
                "{tool} test --threads=1 --show-output",
                "one at a time, with their output",
            ),
        ],
        sections: &["build options", "test options"],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "bench",
        summary: "run the benchmarks",
        usage: &["bench [path] [filter] [options]"],
        about: "\
Build the `#[bench]` functions (at -O2 unless an -O level is given) and run
them; each prints its time per iteration and the deviation. A filter works
as for tests.",
        rokc_note: "rokc only builds the harness; `rok bench` runs it.",
        examples: &[
            ("{tool} bench", "every benchmark"),
            (
                "{tool} bench sort --samples=20",
                "those matching sort, 20 samples each",
            ),
        ],
        sections: &["build options", "bench options"],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "check",
        summary: "find errors without building",
        usage: &["check [path | -] [options]"],
        about: "\
Run the front end, the lints and code generation, without the native
build. `-` reads the source from standard input.",
        examples: &[
            ("{tool} check", "check the package here"),
            (
                "{tool} check src/main.rok",
                "the package that holds this file",
            ),
        ],
        sections: &["check options"],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "lint",
        summary: "run the lints",
        usage: &["lint [path] [options]"],
        about: "Run the front end and the lints. Exits with 1 if a `deny` lint fires.",
        examples: &[
            ("{tool} lint", "lint the package here"),
            ("{tool} lint -D unused", "make the `unused` lints errors"),
            ("{tool} lint --list", "every lint and its level"),
        ],
        sections: &["lint options"],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "repl",
        summary: "start an interactive session",
        usage: &["repl [path] [options]"],
        about: "\
Each cell is compiled and run. In the session, :type <expr> shows a type,
:reset starts over and :exit leaves. A program path runs in the session
first, its bindings and items staying in scope; a library is loaded and
`use`d.",
        examples: &[
            ("{tool} repl", "an empty session"),
            (
                "{tool} repl src/main.rok",
                "start from a program's bindings",
            ),
        ],
        flags: &[("-q, --quiet", "no banner")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "lsp",
        summary: "run the language server",
        usage: &["lsp"],
        about: "\
The language server, on standard input and output. Editors start it;
`rokls` is the same server.",
        examples: &[("{tool} lsp", "serve on stdio")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "jupyter",
        summary: "run the Jupyter kernel",
        usage: &["jupyter --connection-file <file>"],
        about: "\
The Jupyter kernel. Jupyter starts it with the connection file it wrote;
`rok jupyter install` writes the kernelspec that tells it how.",
        examples: &[(
            "{tool} jupyter --connection-file kernel.json",
            "what Jupyter runs",
        )],
        flags: &[("--connection-file <file>", "the file Jupyter wrote")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "fmt",
        summary: "format source files",
        usage: &["fmt [path] [options]"],
        about: "\
Format .rok and .rokon files in place: <path> is one file, or a directory
whose files are all formatted (default: the current one).",
        examples: &[
            ("{tool} fmt", "format everything here"),
            ("{tool} fmt --check", "exit 1 if a file would change (CI)"),
            (
                "{tool} fmt --diff src/main.rok",
                "show the change, write nothing",
            ),
        ],
        sections: &["fmt options"],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "doc",
        summary: "build the API docs",
        usage: &["doc [path] [options]"],
        about: "\
HTML docs of the package's library (else of its executable) and of its
dependencies, in target/doc/.",
        examples: &[
            ("{tool} doc", "docs of the package here"),
            ("{tool} doc . --std", "with the standard library"),
            ("{tool} doc --std", "the standard library alone"),
            ("{tool} doc --open", "then open them in a browser"),
        ],
        sections: &["doc options"],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "clean",
        summary: "remove target/",
        usage: &["clean [path]"],
        about: "Remove the package's target/ directory.",
        examples: &[("{tool} clean", "remove ./target")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "bindgen",
        summary: "Rok declarations of a C header",
        usage: &["bindgen <header> [options] [-- <clang args>]"],
        about: "\
Write the Rok declarations of a C header for the --target, read through
libclang (in the toolchain's lib/). Clang arguments such as -I <dir> or
-D <name> go after `--`.",
        examples: &[
            (
                "{tool} bindgen zlib.h -o src/zlib.rok",
                "declarations into a file",
            ),
            (
                "{tool} bindgen app.h -- -I include",
                "with an include directory",
            ),
        ],
        flags: &[
            ("-o <file>", "write to <file> (default: stdout)"),
            ("--target=<target>", "the target the declarations are for"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "demangle",
        summary: "decode `_Y` symbols",
        usage: &["demangle <symbol>..."],
        about: "Print the package, path, name and generics each `_Y` symbol encodes.",
        examples: &[(
            "{tool} demangle _Y3f8a1c2b_std_vec_push_a7c91d8e44b2f190",
            "",
        )],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "inspect",
        summary: "describe a prebuilt library",
        usage: &["inspect [--dump] <file>"],
        about: "\
Print a prebuilt library's package, exports, generic items and archive
members; with --dump, its whole ROKHIR as text. <file> is <name>.rokhir
or the lib<name>.a beside it.",
        examples: &[
            ("{tool} inspect target/debug/libgeom.a", ""),
            (
                "{tool} inspect --dump target/debug/libgeom.a",
                "the whole ROKHIR",
            ),
        ],
        flags: &[("--dump", "the whole ROKHIR as text")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "emit",
        summary: "write an intermediate form",
        usage: &["emit [path] [--emit=<kind>[,<kind>...]] [options]"],
        about: "\
Write intermediate forms of the package to target/<profile>/build/ (or
-o): source, hir, mir, mlir (the default), llvmir, asm, obj or
expansions.",
        examples: &[
            ("{tool} emit", "the MLIR"),
            ("{tool} emit --emit=llvmir,asm", "LLVM IR and assembly"),
            (
                "{tool} emit --emit=asm -o out.s --release",
                "optimized assembly",
            ),
        ],
        sections: &["build options"],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "profdata",
        summary: "merge and show PGO profiles",
        usage: &[
            "profdata merge [-o <file>] <file | dir>...",
            "profdata show [--all-functions] [--topn N] <file>",
        ],
        about: "\
merge: combine .profraw / .profdata files (a directory: those in it) into
one profile for -C profile-use. show: summarize a profile.",
        examples: &[
            (
                "{tool} profdata merge -o app.profdata .",
                "merge the profiles here",
            ),
            (
                "{tool} profdata show --topn 10 app.profdata",
                "the 10 hottest functions",
            ),
        ],
        flags: &[
            ("-o <file>", "merge: the output (default: default.profdata)"),
            ("--all-functions", "show: every function"),
            ("--topn N", "show: the N hottest functions"),
        ],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "completions",
        summary: "print a shell completion script",
        usage: &["completions bash|zsh|fish|powershell"],
        about: "Print the completion script for that shell on standard output.",
        examples: &[("{tool} completions zsh > _{tool}", "save the zsh script")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "version",
        summary: "print the version",
        usage: &["version [-v]"],
        about: "Print the version; with -v, also the host and the LLVM version.",
        examples: &[("{tool} version -v", "")],
        ..CmdHelp::EMPTY
    },
    CmdHelp {
        name: "help",
        summary: "print help",
        usage: &["help [command]"],
        about: "\
With a command: its usage, examples and options (`{tool} <command> --help`
is the same). Without one: every command and the global options.",
        examples: &[("{tool} help build", "")],
        ..CmdHelp::EMPTY
    },
];

/// The entry for `cmd` in `table`.
#[must_use]
pub fn find<'a>(table: &'a [CmdHelp], cmd: &str) -> Option<&'a CmdHelp> {
    table.iter().find(|h| h.name == cmd)
}

/// Section `name` of `ROKC_USAGE`: its first line and the indented ones
/// after it.
fn usage_section(name: &str) -> Vec<&'static str> {
    let mut lines = ROKC_USAGE
        .lines()
        .skip_while(|l| !l.starts_with(&format!("{name}:")));
    let mut out: Vec<&str> = lines.next().into_iter().collect();
    out.extend(lines.take_while(|l| l.starts_with(' ') && !l.trim().is_empty()));
    out
}

/// `rows` as `  name  description`, descriptions aligned; a long name
/// puts its description on the next line.
#[must_use]
pub fn rows(d: Depth, rows: &[(String, &str)]) -> String {
    rows_from(d, rows, 0, 26)
}

/// [`rows`], the descriptions at least `min` columns in; a name longer
/// than `wide` goes on a line of its own.
#[must_use]
pub fn rows_from(d: Depth, rows: &[(String, &str)], min: usize, wide: usize) -> String {
    let width = rows
        .iter()
        .map(|r| r.0.chars().count())
        .filter(|&n| n <= wide)
        .max()
        .unwrap_or(0)
        .max(min);
    let mut out = String::new();
    for (name, desc) in rows {
        let n = name.chars().count();
        out.push_str("  ");
        out.push_str(&term::paint_in(d, Role::Emph, name));
        let mut lines = desc.lines();
        match lines.next() {
            None => {}
            Some(first) if n <= wide => {
                out.push_str(&" ".repeat(width - n + 2));
                out.push_str(first);
            }
            Some(first) => {
                out.push('\n');
                out.push_str(&" ".repeat(width + 4));
                out.push_str(first);
            }
        }
        out.push('\n');
        for l in lines {
            out.push_str(&" ".repeat(width + 4));
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// `title:` as a help heading.
#[must_use]
pub fn heading(d: Depth, title: &str) -> String {
    term::paint_in(d, Role::Status, &format!("{title}:"))
}

/// Option sections a caller adds to a command's help: `(title, rows)`.
pub type Extra<'a> = &'a [(&'a str, &'a [(&'a str, &'a str)])];

/// The help of `h` for `tool`, with `extra` option sections after its
/// own.
#[must_use]
pub fn render_help(tool: &str, h: &CmdHelp, extra: Extra<'_>) -> String {
    render_help_in(term::depth(Stream::Stdout), tool, h, extra)
}

#[must_use]
pub fn render_help_in(d: Depth, tool: &str, h: &CmdHelp, extra: Extra<'_>) -> String {
    use std::fmt::Write as _;
    #[allow(clippy::literal_string_with_formatting_args)] // a placeholder
    let t = |s: &str| s.replace("{tool}", tool);
    let mut o = String::new();
    for (i, u) in h.usage.iter().enumerate() {
        let lead = if i == 0 {
            heading(d, "usage")
        } else {
            " ".repeat(6)
        };
        let _ = writeln!(o, "{lead} {tool} {}", t(u));
    }
    let _ = writeln!(o, "\n{}", t(h.about));
    if tool == "rokc" && !h.rokc_note.is_empty() {
        let _ = writeln!(o, "\n{}", h.rokc_note);
    }
    if !h.examples.is_empty() {
        let ex: Vec<(String, &str)> = h.examples.iter().map(|(c, w)| (t(c), *w)).collect();
        let _ = write!(
            o,
            "\n{}\n{}",
            heading(d, "examples"),
            rows_from(d, &ex, 0, 40)
        );
    }
    let own: [(&str, &[(&str, &str)]); 1] = [("options", h.flags)];
    for (title, flags) in own.iter().chain(extra).filter(|(_, f)| !f.is_empty()) {
        let fl: Vec<(String, &str)> = flags.iter().map(|(f, w)| ((*f).to_owned(), *w)).collect();
        let _ = write!(o, "\n{}\n{}", heading(d, title), rows(d, &fl));
    }
    for s in h.sections {
        let lines = usage_section(s);
        let Some((first, rest)) = lines.split_first() else {
            continue;
        };
        let _ = writeln!(o, "\n{}{}", heading(d, s), &first[s.len() + 1..]);
        for l in rest {
            let _ = writeln!(o, "{l}");
        }
    }
    if tool != "rokc" {
        o = o.replace("`rokc ", &format!("`{tool} "));
    }
    let _ = writeln!(o, "\nglobal options (-q, -v, --color, ...): `{tool} help`");
    o
}

/// What `rokc`'s command line takes, for its completion scripts (and
/// those of `rok`, which passes these commands to it).
#[must_use]
pub fn rokc_spec() -> Spec {
    let mut s = Spec::parse(
        "rokc",
        ROKC_USAGE,
        |n| n == "commands",
        |n| n == "global options",
    );
    for h in ROKC_COMMANDS {
        for sec in h.sections {
            s.add_flags(h.name, &section_flags(ROKC_USAGE, sec));
        }
    }
    s.values = vec![
        ("+".into(), Values::Run("rok __complete toolchains".into())),
        (
            "--target".into(),
            Values::Run("rokc --print target-list".into()),
        ),
        ("help".into(), Values::Commands),
    ];
    s
}

/// `rok jupyter` with no connection file: the user wants the kernel
/// added, not run.
pub const JUPYTER_BARE: &str =
    "Jupyter starts the kernel itself; to add it, run `rok jupyter install`";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rokc_command_has_help() {
        let spec = rokc_spec();
        for (name, _) in &spec.commands {
            let h = find(ROKC_COMMANDS, name).unwrap_or_else(|| panic!("no help for {name}"));
            assert!(h.usage[0].starts_with(name.as_str()), "{name}");
            assert!(!h.examples.is_empty(), "{name}");
        }
        assert_eq!(spec.commands.len(), ROKC_COMMANDS.len());
    }

    #[test]
    fn own_flags_complete() {
        let spec = rokc_spec();
        for h in ROKC_COMMANDS {
            let sub = format!("{} ", h.name);
            let known: Vec<&String> = spec
                .flags
                .iter()
                .filter(|(k, _)| *k == h.name || k.starts_with(&sub))
                .flat_map(|(_, v)| v)
                .collect();
            for (head, _) in h.flags {
                for f in head.split([',', ' ', '=']).filter(|w| w.starts_with('-')) {
                    assert!(
                        known.iter().any(|k| *k == f) || spec.global.iter().any(|k| k == f),
                        "{} {f}",
                        h.name
                    );
                }
            }
        }
    }

    #[test]
    fn renders_for_either_tool() {
        let h = find(ROKC_COMMANDS, "build").unwrap();
        let rok = render_help_in(Depth::Off, "rok", h, &[]);
        assert!(
            rok.starts_with("usage: rok build [path] [options]\n\n"),
            "{rok}"
        );
        assert!(rok.contains("examples:\n  rok build "), "{rok}");
        assert!(rok.contains("\nbuild options:\n  -o, --out"), "{rok}");
        assert!(!rok.contains("rokc "), "{rok}");
        let rokc = render_help_in(Depth::Off, "rokc", h, &[]);
        assert!(rokc.contains("`rokc profdata merge`"), "{rokc}");
        let t = find(ROKC_COMMANDS, "test").unwrap();
        assert!(render_help_in(Depth::Off, "rokc", t, &[]).contains("only builds the harness"));
        assert!(!render_help_in(Depth::Off, "rok", t, &[]).contains("harness"));
        let painted = render_help_in(Depth::Ansi16, "rok", h, &[]);
        assert!(
            painted.starts_with("\x1b[1;96musage:\x1b[0m rok build"),
            "{painted:?}"
        );
    }

    #[test]
    fn suggestions() {
        let names = || ROKC_COMMANDS.iter().map(|h| h.name);
        let dym = |w| did_you_mean(w, names(), CMD_SYNONYMS);
        assert_eq!(dym("biuld"), "; did you mean `build`?");
        assert_eq!(dym("compile"), "; did you mean `build`?");
        assert_eq!(dym("zzzz"), "");
        assert!(looks_like_script("fc.rok") && looks_like_script("a/b"));
        assert!(!looks_like_script("build"));
    }
}
