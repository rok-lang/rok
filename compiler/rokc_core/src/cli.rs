//! The `rokc` command line as text: `rokc help`, the per-command help
//! and the shell completion scripts all read it.

use crate::completions::{Spec, Values, section_flags};

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
                         `rok bench` runs it and prints ns/iter (+/- deviation)
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
                         rok declarations of a C header (stdout, or -o) for
                         the --target, through libclang (in the toolchain's
                         lib/); clang args such as -I <dir> or -D <name>
  demangle <symbol>...   decode `_Y` symbols
  rokhir-text <file>       dump a prebuilt library's ROKHIR (<name>.rokhir or
                         the lib<name>.a beside it)
  inspect <file>         a prebuilt library's package, exports, generic
                         items and archive members
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
  --color=auto|always|never    color diagnostics (--no-color = never)
  --cwd=<path>                 change directory first
  --message-format=human|json  diagnostics as JSON lines on stdout
  -A|-W|-D <lint|group>        allow / warn / deny a lint (repeatable, later
                               wins; `#[allow(..)]` etc. in the source
                               override)

build options:
  -o, --out, --output <path>   output file
  --emit=<kind>[,<kind>...]    ast | hir | mir | mlir | llvmir | asm | obj | expanded
                               (ast: the package's whole AST after meta
                               expansion and desugaring, as rok source;
                               expanded: each meta expansion's output)
                               (written to target/<profile>/build/<name>.<ext>
                               unless -o)
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
  --print target-list          print the built-in targets and exit
  --print target-spec          print the --target's spec as ROKON and exit
  --print lldbinit             print the lldb command that loads rok's
                               formatters (sums, any) and exit
  --load-mlir-plugin <path|name>
                               MLIR dialect plugin (a name is searched
                               in $ROK_MLIR_PLUGIN_PATH; repeatable)
  --load-mlir-pass-plugin <path|name>
                               MLIR pass plugin
  --mlir-pass <pass>           extra MLIR pass, before the LLVM
                               lowering (repeatable)
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
                               `use name::..` (repeatable; `rok` passes these)
  --extern-feature <name>=<f>  build source dependency <name> with
                               cfg feature=<f> (repeatable; `--cfg
                               feature=..` is the package's own)
  --name <name>                the package name (default: its directory's;
                               `rok` passes rok.rokon's `name`); a
                               library's is in its symbols
  --target-dir <dir>           instead of the package's target/ (without a
                               package: the system temp directory's
                               rokc-target/, a cache trimmed to what was
                               built in the last week)
  -g                           debug info in a release build too (a debug
                               build always has it; macOS: a .dSYM next to
                               the executable)
  --keep                       keep the temporary build directory

check options:  --check-library (check every library body, reached or not)
test options:   --threads=N, --show-output
bench options:  --iters=N (calls per sample; default: calibrated to ~10ms),
                --samples=N (timed samples, default 10)
harness:        --emit-harness <file> (write the built harness and its cases
                to <file> for `rok` to run, instead of naming it)
doc options:    --std (document the standard library too; without a path,
                only it, into ./target/doc/)
lint options:   --list
fmt options:    --check (exit 1 if a file would change), --diff

The sysroot is the directory above rokc's own bin/ (a toolchain:
$ROK_HOME/toolchains/<version>-<triple>/); a build in the repository uses
its library/ and the librokr.a beside rokc.

environment:
  ROK_CC            C compiler driver for the final link (default: rokc's
                    own lld)
  ROK_BACKEND       native | llvm: code generator, as -C backend
  ROKC_TARGET_DIR   target/ location (default: inside the package)
  ROKC_VERBOSE      same as --verbose
  ROKC_NUM_JOBS     default for --threads
  ROKC_INCREMENTAL  where the front end keeps parses and lowered bodies
                    (default: target/<profile>/incremental/; 0: nowhere)
  ROKC_INCR_VERIFY  1: lower every body again and fail on a difference
";

/// The option sections of `ROKC_USAGE` that apply to `cmd`.
#[must_use]
pub fn rokc_sections(cmd: &str) -> &'static [&'static str] {
    match cmd {
        "build" | "run" | "emit" => &["build options"],
        "test" => &["build options", "test options", "harness"],
        "bench" => &["build options", "bench options", "harness"],
        "check" => &["check options"],
        "lint" => &["lint options"],
        "fmt" => &["fmt options"],
        _ => &[],
    }
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
    let names: Vec<String> = s.commands.iter().map(|(n, _)| n.clone()).collect();
    for c in names {
        for sec in rokc_sections(&c) {
            s.add_flags(&c, &section_flags(ROKC_USAGE, sec));
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
