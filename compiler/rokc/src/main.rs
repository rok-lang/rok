//! `rokc`, the rok compiler (spec 901). Arguments are parsed by hand: the
//! workspace takes no external crates.
//!
//! Exit codes (901.6): 0 ok, 1 user error (compile error / failed test),
//! 2 usage error, 70 internal error. `run` exits with the program's own
//! code.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::time::Instant;

use rokc_interface::{
    BuildOptions, CompileResult, DepBuild, EmitKind, Fingerprint, Package, Profile, Session,
    SourceMap, TargetSpec, TempDir, print_rendered,
};
use rokc_tools::lint::Level;

use rokc_core::cli::{self, ROKC_COMMANDS, ROKC_USAGE as USAGE};
use rokc_core::term::{self, ColorChoice, Role, Stream};

// One flag per CLI switch; an enum per group would only add plumbing.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
struct Args {
    cmd: String,
    positional: Vec<String>,
    output: Option<PathBuf>,
    emit: Vec<EmitKind>,
    bootstrap: Vec<PathBuf>,
    externs: Vec<(String, PathBuf)>,
    /// `--extern-feature <name>=<f>`: `(code name, feature)`.
    extern_features: Vec<(String, String)>,
    /// `--name`: a library build's package name.
    name: Option<String>,
    target_dir: Option<PathBuf>,
    cfg: Vec<String>,
    /// `--print <what>` (`cfg`, `target-list`, `target-spec`), then exit.
    print: Option<String>,
    /// `--explain <code>`: print the code's explanation, then exit.
    explain: Option<String>,
    /// `--target`: a built-in target or a `<name>.rokon` file; `None` is
    /// the host.
    target: Option<String>,
    /// The resolved `--target`, for `target_cpu` / `target_features`.
    spec: Option<TargetSpec>,
    /// `-C target-cpu`.
    target_cpu: Option<String>,
    /// `-C target-feature`, each `+f` / `-f`.
    target_features: Vec<String>,
    opts: BuildOptions,
    /// After `--`: the program's arguments as given (any bytes), or
    /// bindgen's clang arguments.
    run_args: Vec<OsString>,
    verbose: u8,
    quiet: bool,
    json: bool,
    color: ColorChoice,
    check: bool,
    check_library: bool,
    /// `doc --std`: document the standard library tiers too.
    std: bool,
    /// `doc --std` without a path: the standard library alone.
    std_only: bool,
    diff: bool,
    threads: Option<usize>,
    show_output: bool,
    lint_levels: rokc_tools::lint::Levels,
    list: bool,
    /// `inspect --dump`: the whole ROKHIR as text.
    dump: bool,
    /// `profdata show --all-functions`.
    all_functions: bool,
    /// `profdata show --topn N`.
    topn: usize,
    /// `-C cs-profile-generate`: the raw profile pattern.
    cs_generate: Option<PathBuf>,
    /// `jupyter --connection-file`.
    connection_file: Option<PathBuf>,
    iters: Option<u64>,
    samples: Option<u64>,
    /// `bench --format=rokbench`: the machine lines, not the human ones.
    rokbench: bool,
    /// `test` / `bench --format=json`: JSON records on stdout, the human
    /// text on stderr.
    format_json: bool,
    /// `doc --open`.
    open: bool,
    /// `--emit-harness <file>`: write the test / bench harness manifest
    /// (built executables and their cases) to `<file>` instead of running;
    /// `rok` reads it and runs the cases (its runner, spec 9.1).
    emit_harness: Option<PathBuf>,
    /// An `-O` level or `--release` was given.
    opt_set: bool,
    /// `-C overflow-checks[=yes|no]`, which beats the manifest's profiles.
    overflow_checks: Option<bool>,
    /// From `rok`: the manifest's profiles that turn overflow checks off.
    no_overflow_checks: Vec<String>,
    /// The prebuilt standard library this build links (`prebuild`).
    std_prebuilt: Option<PathBuf>,
}

impl Args {
    const fn spec(&self) -> &TargetSpec {
        self.spec
            .as_ref()
            .expect("the target is resolved with the arguments")
    }
}

/// The version, and the commit and its date when built from git.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), env!("ROK_COMMIT_INFO"));

/// The command being run, once known, for usage errors.
static CMD: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// When the command started: `Finished` counts the standard library build.
static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn started() -> Instant {
    *STARTED.get_or_init(Instant::now)
}

/// `rokc <cmd> --help` (`rok <cmd> --help` under rok).
fn command_help(cmd: &str) -> String {
    let Some(h) = cli::find(ROKC_COMMANDS, cmd) else {
        usage_error(&format!("unknown command `{cmd}`{}", suggest_command(cmd)));
    };
    cli::render_help(cli::invoked_as(), h, &[])
}

/// "; did you mean ..?" for an unknown command: `run <file>` for a
/// script, else the closest command.
fn suggest_command(cmd: &str) -> String {
    if cli::looks_like_script(cmd) {
        return format!("; did you mean `{} run {cmd}`?", cli::invoked_as());
    }
    cli::did_you_mean(cmd, ROKC_COMMANDS.iter().map(|h| h.name), cli::CMD_SYNONYMS)
}

/// "; did you mean ..?" for an unknown option of `cmd`.
fn suggest_flag(cmd: &str, flag: &str) -> String {
    let spec = cli::rokc_spec();
    let sub = format!("{cmd} ");
    let own = spec
        .flags
        .iter()
        .filter(|(k, _)| cmd.is_empty() || *k == cmd || k.starts_with(&sub))
        .flat_map(|(_, v)| v);
    let cands: Vec<&str> = own.chain(&spec.global).map(String::as_str).collect();
    cli::did_you_mean(flag, cands, &[])
}

/// A usage error (exit 2), named as the program the user ran, and where
/// to read the usage.
fn usage_error(msg: &str) -> ! {
    let tool = cli::invoked_as();
    let msg = msg.replace("usage: rokc ", &format!("usage: {tool} "));
    let more = match CMD.get().filter(|c| cli::find(ROKC_COMMANDS, c).is_some()) {
        Some(c) => format!("{tool} help {c}"),
        None => format!("{tool} help"),
    };
    eprintln!("{tool}: {msg}\nrun `{more}` for usage");
    process::exit(2);
}

fn fail(msg: &str) -> ! {
    eprintln!(
        "{}: {} {msg}",
        cli::invoked_as(),
        term::label(Stream::Stderr, Role::Error)
    );
    process::exit(1);
}

/// `raw` up to `--`, then the program's arguments `tail` (`cli::os_args`).
fn parse_args(raw: Vec<String>, mut tail: Vec<OsString>) -> Args {
    let mut it = raw.into_iter();
    let mut a = Args::default();
    // Value of `--name=value` or the next argument for `--name value`.
    let value = |name: &str, inline: Option<String>, it: &mut std::vec::IntoIter<String>| {
        inline
            .or_else(|| it.next())
            .unwrap_or_else(|| usage_error(&format!("`{name}` needs a value")))
    };
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        match name.as_str() {
            "--" => {
                a.run_args = it.by_ref().map(OsString::from).collect();
                a.run_args.append(&mut tail);
            }
            // Global options (901.5), accepted before or after the command.
            "-v" | "--verbose" => a.verbose += 1,
            "-vv" => a.verbose += 2,
            "-q" | "--quiet" => a.quiet = true,
            "--no-color" => a.color = ColorChoice::Never,
            "--color" => {
                let v = value(&name, inline, &mut it);
                a.color = ColorChoice::parse(&v).unwrap_or_else(|| {
                    usage_error(&format!("`--color` takes auto|always|never, not `{v}`"))
                });
            }
            "--cwd" => {
                let dir = value(&name, inline, &mut it);
                if let Err(e) = std::env::set_current_dir(&dir) {
                    usage_error(&format!("--cwd {dir}: {e}"));
                }
            }
            "--message-format" => {
                a.json = match value(&name, inline, &mut it).as_str() {
                    "json" => true,
                    "human" => false,
                    v => usage_error(&format!("unknown message format `{v}`")),
                };
            }
            "-o" | "--output" => a.output = Some(value(&name, inline, &mut it).into()),
            "--bootstrap" => a.bootstrap.push(value(&name, inline, &mut it).into()),
            "--cfg" => a.cfg.push(value(&name, inline, &mut it)),
            "--print" => a.print = Some(value(&name, inline, &mut it)),
            "--explain" => a.explain = Some(value(&name, inline, &mut it)),
            "--extern" => a.externs.push(parse_extern(&value(&name, inline, &mut it))),
            "--extern-feature" => {
                let v = value(&name, inline, &mut it);
                let Some((dep, f)) = v
                    .split_once('=')
                    .filter(|(d, f)| !d.is_empty() && !f.is_empty())
                else {
                    usage_error(&format!(
                        "`--extern-feature {v}`: expected <name>=<feature>"
                    ));
                };
                a.extern_features
                    .push((dep.replace('-', "_"), f.to_owned()));
            }
            "--name" => a.name = Some(value(&name, inline, &mut it)),
            "--target-dir" => a.target_dir = Some(value(&name, inline, &mut it).into()),
            "--emit-harness" => a.emit_harness = Some(value(&name, inline, &mut it).into()),
            "--target" => a.target = Some(value(&name, inline, &mut it)),
            "--rokr" => a.opts.rokr = value(&name, inline, &mut it),
            "-L" => a.opts.lib_dirs.push(value(&name, inline, &mut it).into()),
            s if s.starts_with("-L") => a.opts.lib_dirs.push(s[2..].into()),
            "-l" => a.opts.libs.push(parse_lib(&value(&name, inline, &mut it))),
            s if s.starts_with("-l") => a.opts.libs.push(parse_lib(&s[2..])),
            "--native-runtime" => a
                .opts
                .runtime_files
                .push(value(&name, inline, &mut it).into()),
            "--emit" => {
                for k in value(&name, inline, &mut it).split(',') {
                    a.emit.push(
                        EmitKind::parse(k)
                            .unwrap_or_else(|| usage_error(&format!("unknown emit kind `{k}`"))),
                    );
                }
            }
            "--release" => {
                "2".clone_into(&mut a.opts.opt_level);
                a.opt_set = true;
            }
            "-O0" | "-O1" | "-O2" | "-O3" | "-Os" => {
                name[2..].clone_into(&mut a.opts.opt_level);
                a.opt_set = true;
            }
            "-C" | "--codegen" => codegen_option(&mut a, &value(&name, inline, &mut it)),
            "--keep" => a.opts.keep_temps = true,
            "-g" => a.opts.debug = true,
            "--check" => a.check = true,
            "--check-library" => a.check_library = true,
            "--std" => a.std = true,
            "--diff" => a.diff = true,
            "--show-output" => a.show_output = true,
            "--list" => a.list = true,
            "--dump" => a.dump = true,
            "--all-functions" => a.all_functions = true,
            "--topn" => {
                let n = value(&name, inline, &mut it);
                a.topn = n
                    .parse()
                    .unwrap_or_else(|_| usage_error(&format!("bad --topn `{n}`")));
            }
            "--connection-file" => {
                a.connection_file = Some(value(&name, inline, &mut it).into());
            }
            "-A" | "-W" | "-D" | "--allow" | "--warn" | "--deny" => {
                let lvl = Level::from_flag(&name).expect("a lint level flag");
                a.lint_levels
                    .flags
                    .push((lvl, value(&name, inline, &mut it)));
            }
            // From `rok`: `profile: { "<p>": { overflow_checks: false } }`.
            "--no-overflow-checks-in" => {
                a.no_overflow_checks.push(value(&name, inline, &mut it));
            }
            // From `rok`: the levels so far are this manifest's `lints`.
            "--lints-from-manifest" => {
                let p = PathBuf::from(value(&name, inline, &mut it));
                let shown = std::env::current_dir()
                    .ok()
                    .and_then(|d| p.strip_prefix(d).ok().map(Path::to_path_buf))
                    .unwrap_or_else(|| p.clone());
                a.lint_levels.manifest =
                    Some((shown.display().to_string(), a.lint_levels.flags.len()));
            }
            "--iters" | "--samples" => {
                let n = value(&name, inline, &mut it);
                let n = n
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .unwrap_or_else(|| usage_error(&format!("bad {name} `{n}`")));
                if name == "--iters" {
                    a.iters = Some(n);
                } else {
                    a.samples = Some(n);
                }
            }
            "--format" => {
                (a.rokbench, a.format_json) = match value(&name, inline, &mut it).as_str() {
                    "rokbench" => (true, false),
                    "json" => (false, true),
                    "human" => (false, false),
                    v => usage_error(&format!(
                        "`--format` takes human, json or rokbench, not `{v}`"
                    )),
                };
            }
            "--open" => a.open = true,
            "--threads" => {
                let n = value(&name, inline, &mut it);
                a.threads = Some(
                    n.parse()
                        .unwrap_or_else(|_| usage_error(&format!("bad --threads `{n}`"))),
                );
            }
            "-h" | "--help" if a.cmd.is_empty() => "help".clone_into(&mut a.cmd),
            "-h" | "--help" => {
                print!("{}", command_help(&a.cmd));
                process::exit(0);
            }
            "--version" | "-V" if a.cmd.is_empty() => "version".clone_into(&mut a.cmd),
            // `-` alone is a positional (stdin, 901.9).
            s if s.starts_with('-') && s.len() > 1 => {
                usage_error(&format!("unknown option `{s}`{}", suggest_flag(&a.cmd, s)));
            }
            _ if a.cmd.is_empty() => {
                let _ = CMD.set(arg.clone());
                a.cmd = arg;
            }
            _ => a.positional.push(arg),
        }
    }
    if a.cmd.is_empty() && a.print.is_none() && a.explain.is_none() {
        usage_error("missing command (e.g. `build`)");
    }
    set_overflow_checks(&mut a);
    // Debug builds carry debug info; `-g` adds it to release (9.2.15).
    let output = matches!(a.cmd.as_str(), "build" | "run" | "test") && a.emit.is_empty();
    if output && !a.opts.release() {
        a.opts.debug = true;
    }
    let mut spec = TargetSpec::resolve(a.target.as_deref()).unwrap_or_else(|e| fail(&e));
    spec.set_cpu(
        a.target_cpu.as_deref(),
        &a.target_features,
        rokc_llvm::host_cpu,
    )
    .unwrap_or_else(|e| usage_error(&e));
    a.spec = Some(spec);
    if let Some(g) = a.cs_generate.take() {
        let Profile::Use(p) = &a.opts.profile else {
            usage_error("-C cs-profile-generate needs -C profile-use=<first stage's profile>");
        };
        a.opts.profile = Profile::CsGenerate(p.clone(), g);
    }
    if rokc_core::env_flag("ROK_VERBOSE") {
        a.verbose = a.verbose.max(1);
    }
    a
}

/// Overflow checks are on in every profile; `-C overflow-checks=no` or
/// the manifest's profile turns them off (spec 9.1.2).
fn set_overflow_checks(a: &mut Args) {
    let profile = if a.opts.release() { "release" } else { "debug" };
    a.opts.overflow_checks = a
        .overflow_checks
        .unwrap_or_else(|| !a.no_overflow_checks.iter().any(|p| p == profile));
}

/// `-C <v>`.
fn codegen_option(a: &mut Args, v: &str) {
    match v {
        "overflow-checks" | "overflow-checks=yes" | "overflow-checks=on" => {
            a.overflow_checks = Some(true);
        }
        "overflow-checks=no" | "overflow-checks=off" => {
            a.overflow_checks = Some(false);
        }
        "backend=native" | "backend=llvm" => a.opts.backend = Some(v[8..].to_owned()),
        _ if v.starts_with("target-cpu=") && v.len() > 11 => {
            a.target_cpu = Some(v[11..].to_owned());
        }
        _ if v.starts_with("target-feature=") => {
            a.target_features.extend(
                v[15..]
                    .split(',')
                    .filter(|f| !f.is_empty())
                    .map(str::to_owned),
            );
        }
        // clang's file name: `%m` merges the runs of one binary.
        "profile-generate" => {
            a.opts.profile = Profile::Generate("default_%m.profraw".into());
        }
        _ if v.starts_with("profile-generate=") => {
            a.opts.profile = Profile::Generate(Path::new(&v[17..]).join("default_%m.profraw"));
        }
        _ if v.starts_with("profile-use=") => {
            a.opts.profile = Profile::Use(v[12..].into());
        }
        "cs-profile-generate" => a.cs_generate = Some("default_%m.profraw".into()),
        _ if v.starts_with("cs-profile-generate=") => {
            a.cs_generate = Some(Path::new(&v[20..]).join("default_%m.profraw"));
        }
        _ => match v
            .strip_prefix("subsystem=")
            .and_then(rokc_interface::Subsystem::parse)
        {
            Some(s) => a.opts.subsystem = s,
            None => usage_error(&format!("unknown codegen option `{v}`")),
        },
    }
}

/// `-l [static=|dylib=]<name>`: `(name, static)`.
fn parse_lib(v: &str) -> (String, bool) {
    let (kind, name) = v.split_once('=').unwrap_or(("dylib", v));
    if name.is_empty() || !matches!(kind, "static" | "dylib") {
        usage_error(&format!("`-l {v}`: expected [static=|dylib=]<name>"));
    }
    (name.to_owned(), kind == "static")
}

/// `--extern name=path` (spec 902.3): a package directory, its
/// `src/lib.rok`, or a prebuilt `<name>.rokhir` / `lib<name>.a` (904).
fn parse_extern(v: &str) -> (String, PathBuf) {
    let Some((name, path)) = v.split_once('=') else {
        usage_error(&format!("`--extern {v}`: expected <name>=<path>"));
    };
    // A package name (`my-lib`) stands for its code name (`my_lib`).
    let name = &name.replace('-', "_");
    let ident = |s: &str| {
        s.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_alphanumeric() || c == '_')
    };
    if !ident(name) {
        usage_error(&format!("`--extern`: `{name}` is not a package name"));
    }
    if let Some(e) = rokc_core::reserved_package(name) {
        usage_error(&format!("`--extern {name}`: {e}"));
    }
    let path = PathBuf::from(path);
    let prebuilt = path.extension().is_some_and(|e| e == "a" || e == "rokhir");
    let root = if prebuilt {
        rokc_interface::rokhir::companions(&path).0
    } else if path.is_dir() {
        path.join("src/lib.rok")
    } else {
        path
    };
    if !root.is_file() {
        fail(&format!("`--extern {name}`: no `{}`", root.display()));
    }
    (name.to_owned(), root)
}

/// Every pass recurses over the tree (the parser caps depth), so run on a
/// thread sized for that cap in debug builds; it's only address space.
const COMPILER_STACK: usize = 1 << 30;

fn main() {
    // `wasm-component-ld` runs rokc as its `wasm-ld` (a `wasm32-wasip2` link).
    if std::env::var_os(rokc_interface::AS_WASM_LD).is_some() {
        process::exit(rokc_interface::wasm_ld_main());
    }
    // Spec 901.6: an internal error (rokc panic) exits 70, apart from a
    // program panic's 101.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        rokc_interface::end_if_output_closed(info);
        default_hook(info);
        eprintln!("{}: internal compiler error", cli::invoked_as());
        process::exit(rokc_core::ICE_EXIT);
    }));
    std::thread::Builder::new()
        .name("main".into())
        .stack_size(COMPILER_STACK)
        .spawn(real_main)
        .expect("spawn compiler thread")
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e));
}

fn real_main() {
    started();
    let (raw, tail) = cli::os_args().unwrap_or_else(|e| usage_error(&e));
    let a = parse_args(raw, tail);
    term::set_choice(a.color);
    rokc_interface::set_report_depth(a.json, term::depth(Stream::Stderr));
    if let Some(code) = &a.explain {
        match rokc_interface::explain(code) {
            Ok(text) => print!("{text}"),
            Err(e) => usage_error(&e),
        }
        return;
    }
    match a.print.as_deref() {
        None | Some("cfg") => {}
        Some("target-list") => {
            for t in rokc_interface::BUILTIN_TARGETS {
                println!("{t}");
            }
            return;
        }
        Some("target-spec") => {
            println!("{}", a.spec().to_rokon());
            return;
        }
        Some("lldbinit") => {
            match rokc_interface::sysroot().lldb {
                Ok(p) => println!("command script import '{}'", p.display()),
                Err(e) => fail(&e),
            }
            return;
        }
        Some(what) => usage_error(&format!(
            "`--print` takes `cfg`, `target-list`, `target-spec` or `lldbinit`, not `{what}`"
        )),
    }
    if a.print.is_some() {
        let mut session = Session::new();
        for c in &a.cfg {
            session.add_cfg(c.clone());
        }
        if a.opts.opt_level == "0" {
            session.add_cfg("debug_assertions".to_owned());
        }
        session.set_target(a.spec());
        for line in session.cfg_lines() {
            println!("{line}");
        }
        return;
    }
    // Commands of one `[path]` (`test` / `bench`: and a filter).
    let most = match a.cmd.as_str() {
        "build" | "emit" | "run" | "check" | "lint" | "doc" | "clean" | "repl" | "bindgen" => 1,
        "test" | "bench" => 2,
        _ => usize::MAX,
    };
    if let Some(extra) = a.positional.get(most) {
        let takes = if most == 1 {
            "one path"
        } else {
            "a path and a filter"
        };
        usage_error(&format!(
            "unexpected argument `{extra}`: `{}` takes {takes}",
            a.cmd
        ));
    }
    match a.cmd.as_str() {
        "build" => cmd_build(with_defaults(a)),
        "emit" => {
            let mut a = with_defaults(a);
            if a.emit.is_empty() {
                a.emit.push(EmitKind::Mlir);
            }
            cmd_build(a);
        }
        "check" => cmd_check(with_defaults(a), true),
        "completions" => match a.positional.as_slice() {
            [shell] => match rokc_core::cli::rokc_spec().render(shell) {
                Some(script) => print!("{script}"),
                None => usage_error(&format!(
                    "no completion script for `{shell}`; write `rokc completions bash`, \
                     `zsh`, `fish` or `powershell`"
                )),
            },
            _ => usage_error("usage: rokc completions bash|zsh|fish|powershell"),
        },
        "lint" if a.list => print!("{}", rokc_tools::lint::list()),
        "lint" => cmd_check(with_defaults(a), false),
        "lsp" => {
            let mut a = with_defaults(a);
            let pkg = rokc_interface::discover(Path::new(".")).ok();
            // The server starts without the dependencies it cannot build.
            if let Err(e) = prebuild(&mut a, pkg.as_ref()) {
                eprintln!(
                    "{}: {} {e}",
                    cli::invoked_as(),
                    term::label(Stream::Stderr, Role::Warning)
                );
                a.externs.clear();
            }
            let io = (
                std::io::BufReader::new(std::io::stdin()),
                std::io::stdout().lock(),
            );
            process::exit(rokc_tools::lsp::serve(
                io.0,
                io.1,
                a.bootstrap,
                a.externs,
                a.lint_levels,
            ));
        }
        "repl" => cmd_repl(with_defaults(a)),
        "jupyter" => {
            let mut a = a;
            let Some(file) = a.connection_file.take() else {
                if rokc_core::cli::invoked_as() == "rok" {
                    usage_error(rokc_core::cli::JUPYTER_BARE);
                }
                usage_error("usage: rokc jupyter --connection-file <file>");
            };
            let mut a = with_defaults(a);
            // The kernel loads the package it starts in (`rok repl .`).
            let pkg = rokc_interface::discover(Path::new("."))
                .ok()
                .filter(|p| !p.anonymous);
            let (prelude, program) = repl_load(&mut a, pkg.as_ref());
            let deps = prebuild(&mut a, pkg.as_ref()).unwrap_or_else(|e| fail(&e));
            let start = rokc_tools::repl::Start {
                prelude: prelude.as_deref(),
                program: program.as_ref().map(|(p, anon)| (p.as_path(), *anon)),
                package: None,
                banner: Some(banner()),
            };
            process::exit(rokc_tools::jupyter::serve(
                &file,
                a.bootstrap,
                a.externs,
                deps,
                a.opts,
                start,
            ));
        }
        "run" => cmd_run(with_defaults(a)),
        "test" => cmd_harness(with_defaults(a), Mode::Test),
        "bench" => cmd_harness(with_defaults(a), Mode::Bench),
        "doc" => cmd_doc(with_defaults(a)),
        "fmt" => cmd_fmt(&a),
        "clean" => {
            let pkg = discover_arg(&a);
            let target = a.target_dir.clone().unwrap_or_else(|| pkg.target_dir());
            match std::fs::remove_dir_all(&target) {
                Ok(()) if !a.quiet => term::status("Removed", &target.display().to_string()),
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => fail(&format!("cannot remove {}: {e}", target.display())),
            }
        }
        "bindgen" => cmd_bindgen(&a),
        "demangle" => cmd_demangle(&a),
        "profdata" => cmd_profdata(&a),
        "inspect" => cmd_inspect(&a),
        "version" => {
            println!("rokc {}", VERSION);
            if a.verbose > 0 {
                let rows = [
                    ("host:", env!("ROK_HOST")),
                    ("llvm:", rokc_interface::LLVM_VERSION.unwrap_or("none")),
                ]
                .map(|(k, v)| (k.to_owned(), v.to_owned()));
                print!("{}", term::list(&rows, None, ""));
            }
        }
        "help" => match a.positional.first() {
            Some(c) => print!("{}", command_help(c)),
            None => print!("{USAGE}"),
        },
        other => usage_error(&format!(
            "unknown command `{other}`{}",
            suggest_command(other)
        )),
    }
}

/// Find the library roots (`rok/core`, spec 0.5) and `librokr.a`
/// without flags.
fn with_defaults(mut a: Args) -> Args {
    let sys = rokc_interface::sysroot();
    if a.bootstrap.is_empty() {
        match sys.libs {
            Ok(roots) => a.bootstrap = roots,
            // The language server still starts.
            Err(e) if a.cmd == "lsp" => {
                eprintln!(
                    "{}: {} {e}",
                    cli::invoked_as(),
                    term::label(Stream::Stderr, Role::Warning)
                );
            }
            Err(e) => fail(&e),
        }
    }
    a.bootstrap = rokc_interface::bootstrap_tiers(&a.bootstrap);
    let links = matches!(
        a.cmd.as_str(),
        "run" | "test" | "bench" | "repl" | "jupyter"
    ) || (a.cmd == "build" && a.emit.is_empty());
    let t = a.spec().clone();
    let name = &t.name;
    // The host runtime is the sysroot's; other targets' (and their libc,
    // when shipped) are in their directory.
    let dir = sys.targets.join(name);
    let missing = || {
        format!(
            "no runtime for {name}: {} is missing (this toolchain does not ship the \
             target; in the repository, run targets/build.sh {name})",
            dir.display()
        )
    };
    if a.opts.rokr == "rokr" {
        let rokr = if t.is_host() {
            sys.rokr
        } else {
            Some(dir.join(t.runtime_lib()))
                .filter(|p| p.is_file())
                .ok_or_else(missing)
        };
        match rokr {
            Ok(lib) => a.opts.rokr = lib.display().to_string(),
            Err(e) if links => fail(&e),
            Err(_) => {}
        }
    }
    if t.libc.shipped() {
        if dir.is_dir() {
            a.opts.libc = Some(dir.clone());
        } else if links {
            fail(&missing());
        }
    }
    match &a.opts.profile {
        Profile::Off => {}
        _ if matches!(a.cmd.as_str(), "repl" | "jupyter") => fail(
            "-C profile-generate / -C profile-use: the REPL does not support profile-guided \
             optimization; build the program with `rokc build`",
        ),
        _ if a.opts.backend.as_deref() == Some("native") => usage_error(
            "-C profile-generate / -C profile-use need the LLVM backend, not the native one \
             (-C backend=native or ROK_BACKEND=native)",
        ),
        p if p.used().is_some_and(|p| !p.is_file()) => {
            fail(&format!(
                "-C profile-use: no profile {}",
                p.used().unwrap_or_else(|| Path::new("")).display()
            ));
        }
        p if p.generates() && links && t.profile_runtime.is_empty() => fail(&format!(
            "-C profile-generate is not supported for {name}: the target has no profile runtime"
        )),
        p if p.generates() && links => {
            let rt = dir.join(&t.profile_runtime);
            if !rt.is_file() {
                fail(&format!(
                    "-C profile-generate: no profile runtime for {name}: {} is missing (this \
                     toolchain does not ship it; in the repository, run targets/build.sh \
                     {name})",
                    rt.display()
                ));
            }
            a.opts.profile_rt = Some(rt);
        }
        _ => {}
    }
    let codegen = links || a.cmd == "build";
    if codegen
        && (a.target_cpu.is_some() || !a.target_features.is_empty())
        && rokc_interface::native_backend(&a.opts)
    {
        eprintln!(
            "{}: {} -C target-cpu / -C target-feature apply to the LLVM backend; the native \
             backend (-O0) builds for the target's baseline CPU",
            cli::invoked_as(),
            term::label(Stream::Stderr, Role::Warning)
        );
    }
    a
}

/// The package named by the first positional (default: `.`).
fn discover_arg(a: &Args) -> Package {
    discover_at(a, a.positional.first().map_or(".", String::as_str))
}

/// The package at `path`, named `--name` when given.
fn discover_at(a: &Args, path: &str) -> Package {
    let mut pkg = rokc_interface::discover(Path::new(path)).unwrap_or_else(|e| fail(&e));
    if let Some(n) = a.name.as_ref().filter(|_| !pkg.anonymous) {
        pkg.name.clone_from(n);
    }
    // Code names a library by its package name (an executable beside it
    // too): that name must be one code can spell.
    if pkg.lib || pkg.library.is_some() {
        if let Some(e) = rokc_core::reserved_package(&rokc_interface::code_name(&pkg.name)) {
            fail(&format!("{}: {e}", pkg.root.display()));
        }
    }
    pkg
}

fn create_dir(dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        fail(&format!("cannot create {}: {e}", dir.display()));
    }
}

/// Prebuild source `--extern`s into `<out>/deps` and use their ROKHIRs.
fn prebuild(a: &mut Args, pkg: Option<&Package>) -> Result<PathBuf, String> {
    let dir = out_dir(a, pkg).join("deps");
    // A native build links the standard library prebuilt.
    if matches!(a.cmd.as_str(), "build" | "run" | "test" | "bench") && a.emit.is_empty() {
        let level = format!("-O{}", a.opts.opt_level);
        let quiet = a.quiet;
        let announce = move || {
            if !quiet {
                term::status(
                    "Compiling",
                    &format!("std ({level}, cached for next builds)"),
                );
            }
        };
        a.std_prebuilt = rokc_interface::prebuilt_std(
            &rokc_interface::StdKey {
                bootstrap: &a.bootstrap,
                cfg: &a.cfg,
                opts: &a.opts,
                target: a.spec(),
                test: matches!(a.cmd.as_str(), "test" | "bench"),
            },
            &announce,
        );
        if a.verbose > 0 && !a.quiet {
            match &a.std_prebuilt {
                Some(p) => term::status("Std", &format!("prebuilt ({})", p.display())),
                None => term::status("Std", "from source"),
            }
        }
    }
    let build = DepBuild {
        bootstrap: &a.bootstrap,
        cfg: &a.cfg,
        features: &a.extern_features,
        opts: &a.opts,
        target: a.spec(),
        dir: &dir,
        quiet: a.quiet,
        std: a.std_prebuilt.as_deref(),
    };
    a.externs = build.prebuild(&a.externs)?;
    Ok(dir)
}

/// What `compile` builds for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Build,
    Test,
    Bench,
    /// Front end only, ASTs kept.
    Doc,
}

/// A session for `pkg` with the command line's libraries and flags.
fn session(a: &Args, pkg: &Package) -> Session {
    let mut session = Session::new();
    for b in &a.bootstrap {
        session.add_bootstrap_file(b.clone());
    }
    for c in &a.cfg {
        session.add_cfg(c.clone());
    }
    if a.opts.opt_level == "0" {
        session.add_cfg("debug_assertions".to_owned());
    } else {
        session.set_inline_prebuilt(true);
    }
    for (n, p) in &a.externs {
        session.add_extern(n.clone(), p.clone());
    }
    session.set_anonymous(pkg.anonymous);
    session.set_own_library(pkg.library.as_ref().map(|_| lib_name(pkg)));
    session.set_target(a.spec());
    session.set_std_prebuilt(a.std_prebuilt.clone());
    session.set_incremental(incremental_dir(a, pkg));
    session
}

/// `-v`: what the front end took from the incremental directory.
fn reused(a: &Args, result: &CompileResult) {
    if let Some(r) = result.reuse.filter(|_| a.verbose > 0 && !a.quiet) {
        eprintln!(
            "      Reused: {} of {} files parsed, {} of {} bodies lowered{}",
            r.files.0,
            r.files.1,
            r.bodies.0,
            r.bodies.1,
            if r.library {
                ", the library's items collected"
            } else {
                ""
            }
        );
    }
}

fn progress(a: &Args, verb: &str, pkg: &Package) {
    if a.quiet {
        return;
    }
    // `doc --std` alone documents a stand-in package in a temporary
    // directory: name what is documented instead.
    let what = if a.std_only {
        "the standard library".to_owned()
    } else {
        format!("{} ({})", pkg.name, pkg.dir.display())
    };
    term::status(verb, &what);
}

/// Front end for `pkg`: reports warnings (also returned, rendered), exits
/// 1 on errors, prints the 901.8 progress line unless `-q`. The instant
/// is the command's start, for `finished`.
fn compile(a: &Args, pkg: &Package, mode: Mode) -> (CompileResult, String, Instant) {
    let t = Instant::now();
    progress(
        a,
        if mode == Mode::Doc {
            "Documenting"
        } else {
            "Compiling"
        },
        pkg,
    );
    let mut session = session(a, pkg);
    // A library's `build` is a prebuilt library: `lib<name>.a` + ROKHIR.
    if pkg.lib && mode == Mode::Build {
        session.set_lib_name(Some(lib_name(pkg)));
    }
    session.set_test(mode == Mode::Test);
    session.set_bench(mode == Mode::Bench);
    // A build lints as `check` does (6.2.9).
    let lints = mode != Mode::Doc;
    let library_files = a.emit.contains(&EmitKind::Expanded) || a.emit.contains(&EmitKind::Ast);
    session.set_keep_ast(true);
    session.set_library_items(lints && !library_files);
    let result = session.compile_file(&pkg.root);
    if result.has_errors() {
        print_rendered(
            &result
                .warnings
                .iter()
                .map(|w| result.render(w))
                .collect::<String>(),
        );
        result.report_errors();
        // A failed compile's list holds its warnings too.
        let e = result.errors.iter().filter(|d| d.is_error()).count();
        let w = result.warnings.len() + result.errors.len() - e;
        could_not_build(a, pkg, e, w);
    }
    let diags = if lints {
        rokc_tools::lint::run(&result, &a.lint_levels)
    } else {
        result.warnings.clone()
    };
    let (errors, warnings): (Vec<_>, Vec<_>) = diags.iter().partition(|d| d.is_error());
    let n = warnings.len();
    let warnings: String = warnings.iter().map(|w| result.render(w)).collect();
    print_rendered(&warnings);
    if !errors.is_empty() {
        errors.iter().for_each(|d| result.report(d));
        could_not_build(a, pkg, errors.len(), n);
    }
    stage(a, "front end", t);
    reused(a, &result);
    if a.verbose > 1 && !a.quiet {
        eprint!("{}", result.trace());
    }
    (result, warnings, started())
}

/// `error: could not build <name> (1 error, 2 warnings)`, then exit 1.
fn could_not_build(a: &Args, pkg: &Package, errors: usize, warnings: usize) -> ! {
    if !a.quiet && !a.json {
        eprintln!(
            "{} could not build {} ({})",
            term::label(Stream::Stderr, Role::Error),
            pkg.name,
            counts(errors, warnings)
        );
    }
    process::exit(1);
}

/// `1 error, 2 warnings`.
fn counts(errors: usize, warnings: usize) -> String {
    let s = |n: usize| if n == 1 { "" } else { "s" };
    format!(
        "{errors} error{}, {warnings} warning{}",
        s(errors),
        s(warnings)
    )
}

/// Incremental directory (spec 9.2.16): `<out>/incremental/<name>-<hash>`,
/// or under `$ROK_INCREMENTAL`. A lone file gets one only there;
/// `ROK_INCREMENTAL=0` turns it off.
fn incremental_dir(a: &Args, pkg: &Package) -> Option<PathBuf> {
    let env = std::env::var_os("ROK_INCREMENTAL").filter(|v| !v.is_empty());
    let root = match env {
        Some(v) if v == "0" => return None,
        Some(v) => PathBuf::from(v),
        None if pkg.anonymous => return None,
        None => out_dir(a, Some(pkg)).join("incremental"),
    };
    let id = std::path::absolute(&pkg.root).unwrap_or_else(|_| pkg.root.clone());
    let h = rokc_core::hash::fnv(id.to_string_lossy().as_bytes());
    Some(root.join(format!("{}-{h:016x}", pkg.name)))
}

/// The library's package name as code spells it.
fn lib_name(pkg: &Package) -> String {
    rokc_interface::code_name(&pkg.name)
}

/// Codegen-unit name: `lib<name>` for a library, to keep it apart from a
/// package's executable.
fn unit_name(pkg: &Package) -> String {
    if pkg.lib {
        format!("lib{}", pkg.name)
    } else {
        pkg.name.clone()
    }
}

/// Package with both root files: build the library and mount it as dep
/// `<name>` of the executable (spec 0.3.1).
fn mount_library(a: &mut Args, pkg: &Package) {
    let Some(lib) = pkg.library_target() else {
        return;
    };
    let archive = build_lib(a, &lib, None);
    a.externs.push((
        lib_name(&lib),
        rokc_interface::rokhir::companions(&archive).0,
    ));
}

/// Build library `pkg` into `out` (default `target/<profile>/lib<name>.a`
/// plus `.rokhir`) unless up to date; the archive.
fn build_lib(a: &Args, pkg: &Package, out: Option<PathBuf>) -> PathBuf {
    let out = out.unwrap_or_else(|| {
        let target = out_dir(a, Some(pkg));
        create_dir(&target);
        target.join(format!("lib{}.a", lib_name(pkg)))
    });
    build_fresh(a, pkg, fingerprint(a, pkg, &out), |a, result| {
        if !a.quiet {
            term::status("Building", &out.display().to_string());
        }
        let opts = unit_opts(a, pkg, &unit_name(pkg));
        let counts = fail_on(result.archive(&opts, &out), &result.source_map);
        report_units(a, counts);
    });
    out
}

/// `-v`: per-stage timing (909.5).
fn stage(a: &Args, what: &str, since: Instant) {
    if a.verbose > 0 && !a.quiet {
        eprintln!("   Stage: {what} ({}ms)", since.elapsed().as_millis());
    }
}

fn finished(a: &Args, started: Instant) {
    if !a.quiet {
        term::status(
            "Finished",
            &format!("in {:.2}s", started.elapsed().as_secs_f64()),
        );
    }
}

/// `r`'s value, or report its error and exit 1.
fn fail_on<T>(r: Result<T, rokc_interface::RokDiagnostic>, sources: &SourceMap) -> T {
    r.unwrap_or_else(|d| {
        sources.report(&d);
        process::exit(1)
    })
}

/// `<target>/[<triple>/]<profile>/`: `release` at `-O2`+ or `-Os`, else
/// `debug`. Without a package, `<target>` is `lone_target_dir`.
fn out_dir(a: &Args, pkg: Option<&Package>) -> PathBuf {
    let mut dir = a
        .target_dir
        .clone()
        .or_else(|| pkg.map(Package::target_dir))
        .or_else(|| std::env::var_os("ROK_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(lone_target_dir);
    if a.target.is_some() {
        dir.push(&a.spec().name);
    }
    dir.join(if a.opts.release() { "release" } else { "debug" })
}

/// `$TMPDIR/rokc-target-<uid>` (Windows: `%TEMP%\rokc-target`), the
/// target directory of lone files: a cache, private to the user, as it
/// runs what it finds. Once a day, files nobody wrote for a week go
/// (`evict`).
fn lone_target_dir() -> PathBuf {
    let dir = rokc_core::tmp::user_cache_dir("rokc-target")
        .unwrap_or_else(|e| fail(&format!("{e}; remove it or set ROK_TARGET_DIR")));
    let stamp = dir.join(".evicted");
    let day = std::time::Duration::from_secs(86_400);
    let due = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .map_or(true, |t| t.elapsed().map_or(true, |a| a > day));
    // Rewriting an empty file leaves its time on Windows: set it.
    let stamped =
        || std::fs::File::create(&stamp).and_then(|f| f.set_modified(std::time::SystemTime::now()));
    if due && std::fs::create_dir_all(&dir).is_ok() && stamped().is_ok() {
        rokc_core::tmp::evict(&dir, 7 * day);
    }
    dir
}

/// The file name of executable `name` for the build's target.
fn exe_file(a: &Args, name: &str) -> String {
    format!("{name}{}", a.spec().exe_suffix)
}

/// `target/<profile>/<name>` (`.exe`, `.wasm`), `lib<name>.a` + `.rokhir`
/// for a library (a package with both builds both; `-o` names the
/// executable), and `build/<name>.<ext>` for `--emit`.
fn cmd_build(mut a: Args) {
    let pkg = discover_arg(&a);
    prebuild(&mut a, Some(&pkg)).unwrap_or_else(|e| fail(&e));
    let target = out_dir(&a, Some(&pkg));
    if pkg.lib && a.emit.is_empty() {
        build_lib(&a, &pkg, a.output.clone());
        return;
    }
    mount_library(&mut a, &pkg);
    if !a.emit.is_empty() {
        let (result, _, started) = compile(&a, &pkg, Mode::Build);
        if a.output.is_some() && a.emit.len() > 1 {
            usage_error("`-o` with several `--emit` kinds");
        }
        for &kind in &a.emit {
            let t = Instant::now();
            let bytes = fail_on(result.emit(kind, &a.opts), &result.source_map);
            let out = a.output.clone().unwrap_or_else(|| {
                let build = target.join("build");
                create_dir(&build);
                build.join(format!("{}.{}", pkg.name, kind.extension()))
            });
            if let Err(e) = std::fs::write(&out, bytes) {
                fail(&format!("cannot write {}: {e}", out.display()));
            }
            stage(&a, &format!("emit {}", kind.extension()), t);
            if !a.quiet {
                term::status("Wrote", &out.display().to_string());
            }
        }
        finished(&a, started);
        return;
    }
    // A lone file's executable goes beside it, built once with `run`'s.
    if pkg.anonymous && a.output.is_none() {
        let exe = script_exe(&a, &pkg);
        let dest = pkg.root.with_file_name(exe_file(&a, &pkg.name));
        place(&a, &pkg, &exe, &dest);
        for f in also(&a, &pkg, &exe) {
            place(
                &a,
                &pkg,
                &f,
                &dest.with_file_name(f.file_name().unwrap_or_default()),
            );
        }
        if !a.quiet {
            term::status(
                "Built",
                &format!("{} (use -o to put it elsewhere)", shown(&dest)),
            );
        }
        return;
    }
    let out = a.output.clone().unwrap_or_else(|| {
        create_dir(&target);
        target.join(exe_file(&a, &pkg.name))
    });
    build_fresh(&a, &pkg, fingerprint(&a, &pkg, &out), |a, result| {
        if !a.quiet {
            term::status("Building", &out.display().to_string());
        }
        link_exe(a, &pkg, result, &out, &pkg.name);
    });
}

/// Build unless up to date; a fresh build reprints its warnings.
fn build_fresh(a: &Args, pkg: &Package, fp: Fingerprint, link: impl FnOnce(&Args, &CompileResult)) {
    if let Some(warnings) = fp.fresh() {
        if !a.quiet {
            term::status("Fresh", &format!("{} ({})", pkg.name, pkg.dir.display()));
        }
        print_rendered(&warnings);
        return;
    }
    fp.clear();
    let (result, warnings, started) = compile(a, pkg, Mode::Build);
    let t = Instant::now();
    // The back end's warnings (a stale profile) are replayed too.
    let ((), more) = rokc_interface::capture_warnings(|| link(a, &result));
    fp.save(&result.files, &(warnings + &more));
    stage(a, "codegen + link", t);
    finished(a, started);
}

/// Codegen-unit build options reusing unchanged units' objects (902.16);
/// a lone file gets its own directory under the target dir.
fn unit_opts(a: &Args, pkg: &Package, unit: &str) -> BuildOptions {
    let mut opts = a.opts.clone();
    opts.incremental = Some(if pkg.anonymous {
        let file = pkg.root.to_string_lossy();
        let hash = rokc_core::hash::fnv(file.as_bytes());
        out_dir(a, None)
            .join("incremental")
            .join(format!("{unit}-{hash:016x}"))
    } else {
        out_dir(a, Some(pkg)).join("incremental").join(unit)
    });
    opts
}

/// `-v`: how many units a build reused and compiled; `-vv`: which.
fn report_units(a: &Args, counts: Option<rokc_interface::UnitCounts>) {
    let Some(c) = counts.filter(|_| a.verbose > 0 && !a.quiet) else {
        return;
    };
    let all = c.compiled.len() + c.reused;
    let cached = c.cached.map_or_else(String::new, |k| {
        format!(" ({k} of {all} optimized objects cached)")
    });
    term::status(
        "Units",
        &format!("{} reused, {} compiled{cached}", c.reused, c.compiled.len()),
    );
    if a.verbose > 1 {
        for u in &c.compiled {
            term::status("Compiled", &format!("unit {u}"));
        }
    }
}

/// Link `result`'s executable at `out`, by codegen units (`unit_opts`).
fn link_exe(a: &Args, pkg: &Package, result: &CompileResult, out: &Path, unit: &str) {
    let opts = unit_opts(a, pkg, unit);
    let counts = fail_on(result.link(&opts, out), &result.source_map);
    report_units(a, counts);
}

/// The incremental-build key of `pkg` built into `out` (see `Fingerprint`).
fn fingerprint(a: &Args, pkg: &Package, out: &Path) -> Fingerprint {
    let mut words = vec![
        format!("{:?}", a.opts),
        format!("lints {:?}", a.lint_levels),
        format!("target {:016x}", a.spec().hash()),
        out.display().to_string(),
        pkg.root.display().to_string(),
        std::env::var("PATH").unwrap_or_default(),
        std::env::var("ROK_CC").unwrap_or_default(),
    ];
    words.extend(a.cfg.iter().cloned());
    words.extend(pkg.lib.then(|| lib_name(pkg)));
    // A package's sources are its `src/`.
    let mut inputs = vec![if pkg.anonymous {
        pkg.root.clone()
    } else {
        pkg.root
            .parent()
            .map_or_else(|| pkg.dir.clone(), Path::to_path_buf)
    }];
    let mut stamped = vec![PathBuf::from(&a.opts.rokr)];
    // Shipped C runtime objects, libc and linker for the target.
    if let Some(dir) = &a.opts.libc {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        stamped.extend(files);
    }
    stamped.extend(a.opts.profile_rt.clone());
    stamped.extend(a.opts.profile.used().map(Path::to_path_buf));
    for b in &a.bootstrap {
        words.push(b.display().to_string());
        inputs.push(b.parent().map_or_else(|| b.clone(), Path::to_path_buf));
    }
    for (n, p) in &a.externs {
        words.push(format!("{n}={}", p.display()));
        let (rokhir, archive) = rokc_interface::rokhir::companions(p);
        inputs.push(rokhir);
        stamped.push(archive);
    }
    // Shared libraries are copied beside the executable; a missing copy
    // rebuilds.
    stamped.extend(a.opts.runtime_files.iter().cloned());
    Fingerprint::new(out, also(a, pkg, out), &words, &inputs, &stamped)
}

/// Files a build makes besides `out` (ROKHIR, copied shared libraries).
fn also(a: &Args, pkg: &Package, out: &Path) -> Vec<PathBuf> {
    if pkg.lib {
        vec![rokc_interface::rokhir::companions(out).0]
    } else {
        let dir = out.parent().unwrap_or_else(|| Path::new("."));
        a.opts
            .runtime_files
            .iter()
            .map(|f| dir.join(f.file_name().unwrap_or_default()))
            .collect()
    }
}

/// Lone-file executables unused this long go when another is built.
const SCRIPT_KEPT: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Lone file `pkg`'s executable, cached (spec 9.10.7) at
/// `scripts/<key>/<name>`, `<key>` hashing everything the build reads. A
/// hit compiles nothing and reprints the warnings. Racing runs build one
/// at a time, under a lock named after the file's units, and a waiter finds
/// the build fresh; linking to a scratch name and renaming keeps a
/// running executable safe.
fn script_exe(a: &Args, pkg: &Package) -> PathBuf {
    let root = out_dir(a, None).join("scripts");
    let name = exe_file(a, &pkg.name);
    let fp = fingerprint(a, pkg, Path::new(&name));
    let dir = root.join(fp.key());
    let exe = dir.join(&name);
    let fp = fp.moved_to(&exe, also(a, pkg, &exe));
    let units = unit_opts(a, pkg, &pkg.name).incremental.unwrap_or_default();
    let mut lock = units.file_name().unwrap_or_default().to_os_string();
    lock.push(".lock");
    let lock = out_dir(a, None).join("locks").join(lock);
    let _held = rokc_core::lock_file(&lock)
        .unwrap_or_else(|e| fail(&format!("cannot lock {}: {e}", lock.display())));
    let mut built = false;
    build_fresh(a, pkg, fp, |a, result| {
        create_dir(&dir);
        let tmp = dir.join(format!(".{name}.{}", process::id()));
        link_exe(a, pkg, result, &tmp, &pkg.name);
        // Windows can't replace a running executable; if one is there, it
        // is this one.
        if std::fs::rename(&tmp, &exe).is_err() && exe.is_file() {
            let _ = std::fs::remove_file(&tmp);
        }
        // Another run's `.dSYM` is as good.
        let dsym = |p: &Path| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            p.with_file_name(format!("{name}.dSYM"))
        };
        if dsym(&tmp).exists() && std::fs::rename(dsym(&tmp), dsym(&exe)).is_err() {
            let _ = std::fs::remove_dir_all(dsym(&tmp));
        }
        built = true;
    });
    let _ = std::fs::write(dir.join("used"), "");
    if built {
        sweep_scripts(&root, &dir);
    }
    exe
}

/// Copy `from` to `to` for a lone file's build. A file at `to` that an
/// earlier build did not put there stays: the build fails and names `-o`.
/// A record per destination in the target directory's `placed/` holds
/// the size and time of what was written.
fn place(a: &Args, pkg: &Package, from: &Path, to: &Path) {
    let root = a
        .target_dir
        .clone()
        .or_else(|| std::env::var_os("ROK_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(lone_target_dir);
    let abs = std::path::absolute(to).unwrap_or_else(|_| to.to_path_buf());
    let h = rokc_core::hash::fnv(abs.to_string_lossy().as_bytes());
    let record = root.join("placed").join(format!("{h:016x}"));
    let stamp = |p: &Path| {
        let m = std::fs::symlink_metadata(p)
            .ok()
            .filter(std::fs::Metadata::is_file)?;
        Some(format!("{} {:?}", m.len(), m.modified().ok()?))
    };
    if std::fs::symlink_metadata(to).is_ok() && std::fs::read_to_string(&record).ok() != stamp(to) {
        fail(&format!(
            "{} is in the way: it was not built from {}; to keep it, write the executable \
             elsewhere: `{} build {} -o <file>`",
            shown(to),
            shown(&pkg.root),
            cli::invoked_as(),
            shown(&pkg.root),
        ));
    }
    // A scratch copy renamed into place: a running executable stays.
    let name = to.file_name().unwrap_or_default().to_string_lossy();
    let tmp = to.with_file_name(format!(".{name}.{}", process::id()));
    if let Err(e) = std::fs::copy(from, &tmp).and_then(|_| std::fs::rename(&tmp, to)) {
        let _ = std::fs::remove_file(&tmp);
        fail(&format!("cannot write {}: {e}", shown(to)));
    }
    if let Some(s) = stamp(to) {
        create_dir(&root.join("placed"));
        let _ = std::fs::write(&record, s);
    }
}

/// `p` relative to the current directory when under it.
fn shown(p: &Path) -> String {
    let rel = std::env::current_dir().ok().and_then(|cwd| {
        let cwd = rokc_core::real_path(cwd).ok()?;
        let p = rokc_core::real_path(p).ok()?;
        p.strip_prefix(&cwd).ok().map(Path::to_path_buf)
    });
    rel.filter(|r| !r.as_os_str().is_empty())
        .unwrap_or_else(|| p.to_path_buf())
        .display()
        .to_string()
}

/// Remove the entries of `root` but `keep` unused for `SCRIPT_KEPT`.
fn sweep_scripts(root: &Path, keep: &Path) {
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let p = e.path();
        let old = std::fs::metadata(p.join("used"))
            .or_else(|_| e.metadata())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > SCRIPT_KEPT);
        if p != keep && old {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// `rokc check` (codegen) / `rokc lint` (none); exit 1 on an error or a
/// `deny` lint. A package with both roots checks the library, then the
/// executable on top of it.
fn cmd_check(mut a: Args, codegen: bool) {
    // `check -`: the source is stdin (901.9), an anonymous package.
    let stdin = a.positional.first().is_some_and(|p| p == "-");
    let pkg = if stdin {
        Package::stdin()
    } else {
        discover_arg(&a)
    };
    prebuild(&mut a, Some(&pkg)).unwrap_or_else(|e| fail(&e));
    a.lint_levels.library = a.check_library;
    let started = started();
    progress(&a, "Checking", &pkg);
    let source = stdin.then(|| {
        let mut src = String::new();
        if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut src) {
            fail(&format!("cannot read stdin: {e}"));
        }
        src
    });
    let (mut errors, mut warnings) = (0, 0);
    for target in pkg.library_target().into_iter().chain([pkg.clone()]) {
        if errors > 0 {
            break;
        }
        if let Some(lib) = target.library.as_ref() {
            a.externs.push((lib_name(&target), lib.clone()));
        }
        let mut session = session(&a, &target);
        session.set_check_library(a.check_library);
        let (result, diags) = rokc_tools::analysis::check(
            &mut session,
            source.as_deref(),
            &target.root,
            &a.lint_levels,
            codegen.then_some(&a.opts),
        );
        reused(&a, &result);
        diags.iter().for_each(|d| result.report(d));
        let e = diags.iter().filter(|d| d.is_error()).count();
        errors += e;
        warnings += diags.len() - e;
    }
    if !a.quiet && !a.json {
        term::status(
            "Checked",
            &format!("{}: {}", pkg.name, counts(errors, warnings)),
        );
    }
    finished(&a, started);
    process::exit(i32::from(errors > 0));
}

fn cmd_repl(mut a: Args) {
    // Standard input is the cell input (901.2).
    if a.positional.first().is_some_and(|p| p == "-") {
        usage_error("`rokc repl -`: standard input is the cell input; give a path or none");
    }
    let pkg = a.positional.first().map(|p| discover_at(&a, p));
    let (prelude, program) = repl_load(&mut a, pkg.as_ref());
    let here = rokc_interface::discover(Path::new(".")).ok();
    let deps = prebuild(&mut a, pkg.as_ref().or(here.as_ref())).unwrap_or_else(|e| fail(&e));
    // In a package it does not load, the session hints at its names.
    let package = here
        .filter(|_| pkg.is_none())
        .and_then(|h| h.root.parent().map(Path::to_path_buf));
    let interactive = std::io::stdin().is_terminal();
    let start = rokc_tools::repl::Start {
        prelude: prelude.as_deref(),
        program: program.as_ref().map(|(p, anon)| (p.as_path(), *anon)),
        package,
        banner: (interactive && std::io::stdout().is_terminal() && !a.quiet).then(banner),
    };
    process::exit(rokc_tools::repl::main(
        a.bootstrap,
        a.externs,
        deps,
        a.opts,
        interactive,
        start,
    ));
}

/// The REPL's and kernel's first line: the `--version` text and the host.
fn banner() -> String {
    format!("Rok {VERSION} on {}", env!("ROK_HOST"))
}

/// What a session on `pkg` loads: a library becomes a session dependency
/// with `use <name>::*;` as the first cell (901.2); a program is loaded,
/// with its package's library (0.3.1) as a dependency.
fn repl_load(a: &mut Args, pkg: Option<&Package>) -> (Option<String>, Option<(PathBuf, bool)>) {
    let Some(p) = pkg else {
        return (None, None);
    };
    if let Some(lib) = p.library.clone() {
        a.externs.push((lib_name(p), lib));
    }
    if p.lib {
        let name = lib_name(p);
        a.externs.push((name.clone(), p.root.clone()));
        return (Some(format!("use {name}::*;")), None);
    }
    (None, Some((p.root.clone(), p.anonymous)))
}

fn cmd_run(mut a: Args) {
    let pkg = discover_arg(&a);
    if pkg.lib {
        fail(&format!(
            "`{}` is a library package (`src/lib.rok`); there is nothing to run",
            pkg.dir.display()
        ));
    }
    prebuild(&mut a, Some(&pkg)).unwrap_or_else(|e| fail(&e));
    mount_library(&mut a, &pkg);
    let exe = match &a.output {
        None if pkg.anonymous => script_exe(&a, &pkg),
        o => {
            let exe = o.clone().unwrap_or_else(|| {
                let target = out_dir(&a, Some(&pkg));
                create_dir(&target);
                target.join(exe_file(&a, &pkg.name))
            });
            let fp = fingerprint(&a, &pkg, &exe);
            build_fresh(&a, &pkg, fp, |a, result| {
                link_exe(a, &pkg, result, &exe, &pkg.name);
            });
            exe
        }
    };
    if !a.quiet {
        // A lone file runs from the cache: name the file, `-v` the copy.
        let what = match (pkg.anonymous && a.output.is_none(), a.verbose) {
            (true, 0) => shown(&pkg.root),
            (true, _) => format!("`{}`", exe.display()),
            _ => format!("`{}`", shown(&exe)),
        };
        term::status("Running", &what);
    }
    let mut c = Command::new(&exe);
    c.args(&a.run_args);
    rokc_core::strip_handoffs(&mut c);
    let status = c
        .status()
        .unwrap_or_else(|e| fail(&format!("cannot run {}: {e}", exe.display())));
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(&status) {
        process::exit(128 + sig);
    }
    process::exit(status.code().unwrap_or(1));
}

/// `[path] [filter]`: a lone argument that isn't an existing path is the
/// filter.
fn path_and_filter(a: &Args) -> (&str, Option<&str>) {
    match a.positional.as_slice() {
        [] => (".", None),
        [p] if Path::new(p).exists() => (p.as_str(), None),
        [f] => (".", Some(f.as_str())),
        [p, f] => (p.as_str(), Some(f.as_str())),
        _ => usage_error(&format!("usage: rokc {} [path] [filter]", a.cmd)),
    }
}

/// `rokc test` / `rokc bench [path] [filter]` (`-O2` unless given): build
/// the harness and emit the manifest `rok` runs (spec 9.1). `rok` runs
/// the cases; `--emit-harness` names the manifest file.
fn cmd_harness(mut a: Args, mode: Mode) {
    if mode != Mode::Bench && a.rokbench {
        usage_error("`--format` for test takes human or json; `rokbench` is for bench");
    }
    if mode == Mode::Bench && !a.opt_set {
        "2".clone_into(&mut a.opts.opt_level);
        set_overflow_checks(&mut a);
    }
    let (path, filter) = path_and_filter(&a);
    let filter = filter.map(str::to_owned);
    let pkg = discover_at(&a, path);
    prebuild(&mut a, Some(&pkg)).unwrap_or_else(|e| fail(&e));
    // `rokc` builds the harness; the `rok` runner (spec 9.1) runs it. A
    // package with both roots builds the library's first, the
    // executable's second. Each goes into the target directory so it
    // survives this process, and its path and cases go to the manifest.
    let mut harnesses: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for target in pkg.library_target().into_iter().chain([pkg.clone()]) {
        mount_library(&mut a, &target);
        harnesses.push(build_harness(&a, &target, mode));
    }
    write_harness_manifest(&a, mode, filter.as_deref(), &harnesses);
}

/// Build `pkg`'s test or bench harness into the target directory; its
/// executable and the cases in it.
fn build_harness(a: &Args, pkg: &Package, mode: Mode) -> (PathBuf, Vec<String>) {
    let kind = if mode == Mode::Bench { "bench" } else { "test" };
    let (result, _, started) = compile(a, pkg, mode);
    let unit = format!("{}-{kind}", unit_name(pkg));
    let dir = out_dir(a, Some(pkg)).join("harness");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        fail(&format!("{}: {e}", dir.display()));
    }
    let exe = dir.join(exe_file(a, &unit));
    let t = Instant::now();
    link_exe(a, pkg, &result, &exe, &unit);
    stage(a, "codegen + link", t);
    finished(a, started);
    (exe, result.tests)
}

/// The default test thread count: `--threads`, else `ROK_TEST_THREADS`,
/// else the available parallelism. `rok`'s runner reads it from the manifest.
fn test_threads(a: &Args) -> usize {
    a.threads
        .or_else(|| {
            rokc_core::env_knob("ROK_TEST_THREADS", "a positive number", |v| {
                v.parse().ok().filter(|&n| n > 0)
            })
        })
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from))
}

/// Write the harness manifest `rok` runs (spec 9.1): the run options
/// `rokc` resolved, then each executable with its cases. Without
/// `--emit-harness`, name the built executables for a human instead.
fn write_harness_manifest(
    a: &Args,
    mode: Mode,
    filter: Option<&str>,
    harnesses: &[(PathBuf, Vec<String>)],
) {
    // Resolve the thread count for a test run whether or not a manifest is
    // emitted, so `ROK_TEST_THREADS` is validated (and a bad value warned)
    // on every `rokc test`.
    let threads = (mode == Mode::Test).then(|| test_threads(a));
    let Some(path) = a.emit_harness.as_ref() else {
        if !a.quiet {
            for (exe, _) in harnesses {
                term::status("Built", &exe.display().to_string());
            }
            eprintln!(
                "{} run the harness with `rok {}`",
                term::label(Stream::Stderr, Role::Note),
                a.cmd
            );
        }
        return;
    };
    // `rokc_harness::VERSION`; a newer runner reads older formats. `format
    // json` needs 2, so a runner that cannot write it refuses the run.
    let mut m = String::from(if a.format_json {
        "version 2\n"
    } else {
        "version 1\n"
    });
    m.push_str(if mode == Mode::Bench {
        "kind bench\n"
    } else {
        "kind test\n"
    });
    if mode == Mode::Bench {
        if let Some(n) = a.iters {
            m.push_str(&format!("iters {n}\n"));
        }
        m.push_str(&format!("samples {}\n", a.samples.unwrap_or(10)));
        if a.rokbench {
            m.push_str("format rokbench\n");
        }
    }
    if a.format_json {
        m.push_str("format json\n");
    }
    if mode != Mode::Bench {
        m.push_str(&format!("threads {}\n", threads.unwrap_or(1)));
        if a.show_output {
            m.push_str("show_output\n");
        }
    }
    // The filter is raw to end of line, so it may hold spaces.
    if let Some(f) = filter {
        m.push_str(&format!("filter {f}\n"));
    }
    for (exe, cases) in harnesses {
        // Absolute, so the runner finds it from any working directory.
        let exe = std::path::absolute(exe).unwrap_or_else(|_| exe.clone());
        m.push_str(&format!("exe {}\n", exe.display()));
        for c in cases {
            m.push_str(&format!("case {c}\n"));
        }
    }
    if let Err(e) = std::fs::write(path, m) {
        fail(&format!("{}: {e}", path.display()));
    }
}

/// `rokc doc [path]`: HTML into `target/doc/` (see `rokc_tools::doc`).
fn cmd_doc(mut a: Args) {
    // `--std` alone: an empty package that only mounts the library.
    let std_only = a.std && a.positional.is_empty();
    a.std_only = std_only;
    let tmp = std_only.then(|| fail_on(TempDir::new(a.opts.keep_temps), &SourceMap::new()));
    if let Some(t) = &tmp {
        let main = t.path().join("rok/src/main.rok");
        std::fs::create_dir_all(t.path().join("rok/src"))
            .and_then(|()| std::fs::write(&main, ""))
            .unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", main.display())));
        a.positional.push(main.display().to_string());
        a.name.get_or_insert_with(|| "rok".to_owned());
    }
    // A package with both root files: its library (0.3.1).
    let pkg = discover_arg(&a);
    let pkg = pkg.library_target().unwrap_or(pkg);
    prebuild(&mut a, Some(&pkg)).unwrap_or_else(|e| fail(&e));
    let (result, _, started) = compile(&a, &pkg, Mode::Doc);
    let out = a
        .target_dir
        .clone()
        .unwrap_or_else(|| {
            if std_only {
                PathBuf::from("target")
            } else {
                pkg.target_dir()
            }
        })
        .join("doc");
    let docs = rokc_tools::doc::Scope {
        user: !std_only,
        std: a.std,
    };
    let index = rokc_tools::doc::generate(&result, &pkg.name, &out, docs)
        .unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", out.display())));
    drop(tmp);
    finished(&a, started);
    if !a.quiet {
        term::status("Generated", &index.display().to_string());
    }
    if a.open {
        open_in_browser(&index);
    }
}

/// `doc --open`: `$BROWSER`, else the system's opener. A failure warns
/// and leaves the path to open by hand.
fn open_in_browser(page: &Path) {
    let page = std::path::absolute(page).unwrap_or_else(|_| page.to_path_buf());
    let browser = std::env::var_os("BROWSER").filter(|b| !b.is_empty());
    let r = if let Some(b) = browser {
        // A browser may run until closed: do not wait for it.
        Command::new(&b).arg(&page).spawn().map(|_| ())
    } else {
        let mut c = if cfg!(windows) {
            // `start`'s first quoted argument is a window title.
            let mut c = Command::new("cmd");
            c.args(["/C", "start", ""]);
            c
        } else if cfg!(target_os = "macos") {
            Command::new("open")
        } else {
            Command::new("xdg-open")
        };
        c.arg(&page)
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null())
            .status()
            .and_then(|s| {
                if s.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!("{s}")))
                }
            })
    };
    if let Err(e) = r {
        eprintln!(
            "{}: {} cannot open a browser ({e}); open {} in one, or set BROWSER",
            cli::invoked_as(),
            term::label(Stream::Stderr, Role::Warning),
            page.display()
        );
    }
}

/// A walk's progress on a terminal, once it has taken a second.
struct Scanning {
    on: bool,
    start: Instant,
    last: Option<Instant>,
    width: usize,
}

impl Scanning {
    fn new(quiet: bool) -> Self {
        Self {
            on: !quiet && std::io::stderr().is_terminal(),
            start: Instant::now(),
            last: None,
            width: 0,
        }
    }

    fn tick(&mut self, n: usize) {
        let now = Instant::now();
        if !self.on
            || now - self.start < std::time::Duration::from_secs(1)
            || self
                .last
                .is_some_and(|l| now - l < std::time::Duration::from_millis(100))
        {
            return;
        }
        self.last = Some(now);
        let msg = format!("{n} files…");
        // The verb takes 12 columns.
        self.width = self.width.max(13 + msg.chars().count());
        let d = term::depth(Stream::Stderr);
        eprint!("\r{}", term::status_in(d, "Scanning", &msg));
    }

    fn clear(&mut self) {
        if self.width > 0 {
            eprint!("\r{}\r", " ".repeat(self.width));
            self.width = 0;
        }
    }
}

/// `n file(s)`.
fn files_n(n: usize) -> String {
    format!("{n} file{}", if n == 1 { "" } else { "s" })
}

/// What `rokc fmt` formats: each path given, a package's (`true`) or
/// not; without a path, the package the working directory is in.
fn fmt_roots(a: &Args, tool: &str) -> Vec<(PathBuf, bool)> {
    if !a.positional.is_empty() {
        return a
            .positional
            .iter()
            .map(|p| {
                (
                    PathBuf::from(p),
                    rokc_interface::root_file(Path::new(p)).is_some(),
                )
            })
            .collect();
    }
    let Some(pkg) = rokc_interface::package_root(Path::new(".")) else {
        fail(&format!(
            "not inside a package; pass a file or directory: `{tool} fmt <path>`"
        ));
    };
    // `..` steps up to it, so paths print short.
    let cwd = std::path::absolute(".").unwrap_or_default();
    let up = cwd.strip_prefix(&pkg).map_or(0, |r| r.components().count());
    let dir = if up == 0 {
        PathBuf::from(".")
    } else {
        std::iter::repeat_n("..", up).collect()
    };
    vec![(dir, true)]
}

/// The files under `roots`: a package's `src/`, `tests/`, `benches/`,
/// `examples/` and manifest, a directory's sources, or the file.
fn fmt_files(a: &Args, roots: &[(PathBuf, bool)]) -> Vec<PathBuf> {
    let mut scan = Scanning::new(a.quiet);
    let mut files = Vec::new();
    for (r, package) in roots {
        if *package || r.is_dir() {
            let mut found = Vec::new();
            if *package {
                for d in ["src", "tests", "benches", "examples"] {
                    found.extend(rokc_interface::source_files_with(&r.join(d), &mut |n| {
                        scan.tick(n);
                    }));
                }
                found.extend(Some(r.join("rok.rokon")).filter(|m| m.is_file()));
            } else {
                found = rokc_interface::source_files_with(r, &mut |n| scan.tick(n));
            }
            scan.clear();
            if found.is_empty() && !a.quiet {
                term::note(&format!("no .rok or .rokon files under {}", r.display()));
            }
            files.extend(found);
        } else if r.is_file() {
            files.push(r.clone());
        } else {
            scan.clear();
            fail(&format!("`{}` does not exist", r.display()));
        }
    }
    scan.clear();
    files
}

/// `rokc fmt`'s last line: what changed, or that nothing had to.
fn fmt_summary(a: &Args, tool: &str, total: usize, changed: usize, errors: usize) {
    let same = total - changed - errors;
    if a.quiet || total == 0 {
        return;
    }
    if changed > 0 && (a.check || a.diff) {
        let flag = if a.check { "--check" } else { "--diff" };
        let (need, them) = if changed == 1 {
            ("needs", "it")
        } else {
            ("need", "them")
        };
        term::note(&format!(
            "{} {need} formatting; `{tool} fmt` without `{flag}` rewrites {them}",
            files_n(changed)
        ));
    } else if changed > 0 {
        let rest = if same > 0 {
            format!(", {same} already formatted")
        } else {
            String::new()
        };
        term::status("Formatted", &format!("{}{rest}", files_n(changed)));
    } else if errors == 0 {
        term::note(&format!("{} already formatted", files_n(same)));
    }
}

/// `rokc fmt [path..]`: see `fmt_roots` and `fmt_files`.
fn cmd_fmt(a: &Args) {
    let tool = rokc_core::cli::invoked_as();
    let files = fmt_files(a, &fmt_roots(a, tool));
    let (mut changed, mut errors) = (0, 0);
    for f in &files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => fail(&format!("{}: {e}", f.display())),
        };
        let formatted = match rokc_fmt::format_file(f, &src) {
            Ok(s) => s,
            Err(e) if e.starts_with("error[") => {
                eprintln!("{e}");
                errors += 1;
                continue;
            }
            Err(e) => {
                eprintln!("{}: cannot format {}: {e}", cli::invoked_as(), f.display());
                errors += 1;
                continue;
            }
        };
        if formatted == src {
            continue;
        }
        changed += 1;
        if a.diff || a.check {
            println!("Diff in {}:", f.display());
            if a.diff || !a.quiet {
                print!("{}", rokc_fmt::line_diff(&src, &formatted));
            }
        }
        if !a.check && !a.diff {
            if let Err(e) = std::fs::write(f, formatted) {
                fail(&format!("cannot write {}: {e}", f.display()));
            }
            if a.verbose > 0 {
                term::status("Formatted", &f.display().to_string());
            }
        }
    }
    fmt_summary(a, tool, files.len(), changed, errors);
    if errors > 0 || (a.check && changed > 0) {
        process::exit(1);
    }
}

/// `rokc profdata merge [-o <file>] <file | dir>...`: merge raw and
/// indexed profiles (a directory's `.profraw` / `.profdata`) for
/// `-C profile-use`.
fn cmd_profdata(a: &Args) {
    let usage = "usage: rokc profdata merge [-o <file>] <file | dir>... | profdata show \
                 [--all-functions] [--topn N] <file>";
    let [sub, inputs @ ..] = a.positional.as_slice() else {
        usage_error(usage);
    };
    if sub == "show" {
        let [file] = inputs else { usage_error(usage) };
        let text = rokc_interface::profdata_show(Path::new(file), a.all_functions, a.topn)
            .unwrap_or_else(|e| fail(&format!("profdata show: {}", e.trim_end())));
        print!("{text}");
        return;
    }
    if sub != "merge" || inputs.is_empty() {
        usage_error(usage);
    }
    let mut files = Vec::new();
    for i in inputs {
        let p = Path::new(i);
        if !p.is_dir() {
            files.push(p.to_path_buf());
            continue;
        }
        let entries = std::fs::read_dir(p).unwrap_or_else(|e| fail(&format!("{i}: {e}")));
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|f| {
                f.extension()
                    .is_some_and(|x| x == "profraw" || x == "profdata")
            })
            .collect();
        if found.is_empty() {
            fail(&format!(
                "profdata merge: no .profraw or .profdata file in {i}"
            ));
        }
        found.sort();
        files.extend(found);
    }
    let out = a
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("default.profdata"));
    let refs: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    let warnings = rokc_interface::profdata_merge(&refs, &out)
        .unwrap_or_else(|e| fail(&format!("profdata merge: {}", e.trim_end())));
    for w in warnings.lines() {
        eprintln!(
            "{}: {} profdata merge: {w}",
            cli::invoked_as(),
            term::label(Stream::Stderr, Role::Warning)
        );
    }
    if !a.quiet {
        term::status(
            "Wrote",
            &format!(
                "{} ({} input{})",
                out.display(),
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            ),
        );
    }
}

/// `rokc bindgen <header>` (std 1.7.6, spec 9.1.3): one block per
/// symbol.
fn cmd_bindgen(a: &Args) {
    let [header] = a.positional.as_slice() else {
        usage_error("usage: rokc bindgen <header> [--target <t>] [-o <file>] [-- <clang args>]");
    };
    let spec = a.spec();
    let libclang =
        rokc_tools::bindgen::find_libclang(rokc_llvm::LLVM_VERSION).unwrap_or_else(|e| fail(&e));
    let sdk = (spec.vendor == "apple")
        .then(rokc_llvm::apple_sdk)
        .transpose()
        .unwrap_or_else(|e| fail(&e));
    let clang_args: Vec<String> = a
        .run_args
        .iter()
        .enumerate()
        .map(|(i, x)| match x.to_str() {
            Some(x) => x.to_owned(),
            None => usage_error(&format!(
                "clang argument {} (`{}`) is not valid UTF-8; libclang takes UTF-8 only",
                i + 1,
                x.to_string_lossy()
            )),
        })
        .collect();
    let opts = rokc_tools::bindgen::Options {
        header: Path::new(header),
        target: &spec.name,
        triple: &spec.llvm_triple(),
        stdcall: spec.stdcall_system(),
        sdk,
        clang_args: &clang_args,
        // Host headers serve the host, every Apple target (the SDK has
        // them all) and every MSVC target on an MSVC host; others come
        // from clang arguments.
        system_headers: spec.is_host()
            || spec.vendor == "apple"
            || spec.env == "msvc" && TargetSpec::host().env == "msvc"
            || clang_args.iter().any(|x| x.starts_with("--sysroot")),
    };
    let out = rokc_tools::bindgen::generate(&libclang, &opts).unwrap_or_else(|e| fail(&e));
    match &a.output {
        Some(p) => {
            std::fs::write(p, out).unwrap_or_else(|e| fail(&format!("{}: {e}", p.display())));
        }
        None => print!("{out}"),
    }
}

/// `inspect [--dump] <file>`.
fn cmd_inspect(a: &Args) {
    let [file] = a.positional.as_slice() else {
        usage_error("usage: rokc inspect [--dump] <file.rokhir | lib<name>.a>");
    };
    let path = Path::new(file);
    let out = if a.dump {
        let rokhir = rokc_interface::rokhir::companions(path).0;
        std::fs::read(&rokhir)
            .map_err(|e| format!("{}: {e}", rokhir.display()))
            .and_then(|b| rokc_interface::rokhir::Rokhir::decode(&b))
            .map(|r| r.text())
    } else {
        rokc_interface::rokhir::inspect(path)
    };
    print!("{}", out.unwrap_or_else(|e| fail(&e)));
}

fn cmd_demangle(a: &Args) {
    if a.positional.is_empty() {
        usage_error("usage: rokc demangle <symbol>...");
    }
    let mut bad = false;
    for (i, sym) in a.positional.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let Some(d) = rokc_interface::demangle(sym) else {
            eprintln!("{}: `{sym}` is not a rok symbol", cli::invoked_as());
            bad = true;
            continue;
        };
        let path = d.qualified_path();
        println!("package:  {}", d.pkg);
        println!(
            "path:     {}",
            if path.is_empty() { "(root)" } else { &path }
        );
        println!("name:     {}", d.name);
        match &d.typehash {
            Some(h) => println!("generics: {h} (type tuple hash)"),
            None => println!("generics: (none)"),
        }
    }
    if bad {
        process::exit(1);
    }
}
