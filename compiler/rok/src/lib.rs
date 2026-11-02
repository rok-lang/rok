//! `rok`, the toolchain front: manifests and locks, dependency
//! resolution, stones and the git cache, environments, toolchains,
//! scaffolding and the Jupyter kernelspec. It runs the `rokc` beside it
//! with resolved deps as `--extern name=lib.rok`; a pin naming another
//! toolchain hands the whole command to that toolchain's `rok`.
//!
//! ```text
//! $ROK_HOME/bin/{rok,rokc,rokls}     proxies to the active toolchain, copies of the newest's rok
//! $ROK_HOME/bin/<name>               an executable `rok install` built
//! $ROK_HOME/installs.rokon           those executables and their sources
//! $ROK_HOME/toolchains/<version>-<triple>/
//!                                    a toolchain: bin/{rok,rokc,rokls}, lib/
//! $ROK_HOME/cache/git/               shared git cache (see `git`)
//! $ROK_HOME/cache/registry/          stone index clones (see `registry`)
//! $ROK_HOME/cache/native/            native packages, by SHA-256 (see `native`)
//! $ROK_HOME/config.rokon             `Config { toolchain: "<default>" }`
//! $ROK_HOME/tokens.rokon             publish tokens, per index
//! $ROK_HOME/sources.rokon            index mirrors (see `registry`)
//! $ROK_HOME/cache/scripts/<hash>/    a script's lock and prebuilt dependencies
//! $ROK_HOME/envs/<name>/             a named environment (rok.rokon, rok.lock, target/)
//! $ROK_HOME/envs/.current            `rok env use`'s choice
//! ```

mod env;
mod git;
mod help;
mod lock;
mod manifest;
mod native;
mod platform;
mod registry;
mod resolve;
mod semver;
mod toolchain;

pub use toolchain::{proxy, take_guard};

use std::path::{Path, PathBuf};
use std::process::Command;

use manifest::{Dep, GitRef, MANIFEST, Manifest, NativeSource, Source};
use resolve::{Context, Options, Resolved};
use rokc_core::cli::{self, looks_like_script};
use rokc_core::completions::{Spec, Values};
use rokc_core::term::{self, ColorChoice, Role, Stream};
use rokc_core::tmp::TempEntry;
use rokc_diagnostic::Json;
use rokc_rokon::{Kind, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// `VERSION`, and the commit and its date when built from git.
pub const VERSION_LINE: &str = concat!(env!("CARGO_PKG_VERSION"), env!("ROK_COMMIT_INFO"));

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // independent switches
struct Global {
    env: Option<String>,
    offline: bool,
    locked: bool,
    quiet: bool,
    verbose: bool,
    /// `+<version>`.
    toolchain: Option<String>,
    /// `--color=<when>`, passed on to rokc.
    color: Option<String>,
    /// The program's arguments after `--` (or a script's path) as given:
    /// the strings in `post` stand for them only lossily.
    tail: Vec<std::ffi::OsString>,
    /// The arguments and directory rok started with, for `handoff`.
    argv: Vec<std::ffi::OsString>,
    start: Option<PathBuf>,
}

/// `rok: warning: <msg>` on stderr, unless already printed.
pub(crate) fn warn_once(msg: &str) {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if seen.iter().any(|s| s == msg) {
        return;
    }
    seen.push(msg.to_owned());
    drop(seen);
    eprintln!("rok: {} {msg}", term::label(Stream::Stderr, Role::Warning));
}

/// The user's rok directory (`rokc_core::rok_home`).
pub fn home() -> PathBuf {
    rokc_core::rok_home().unwrap_or_else(|| PathBuf::from(".rok"))
}

/// Fields of `$ROK_HOME/config.rokon` (`Config { .. }`).
const CONFIG_FIELDS: [&str; 2] = ["registry", "toolchain"];

/// String field `field` of `$ROK_HOME/config.rokon`.
fn config(home: &Path, field: &str) -> Result<Option<String>, String> {
    let path = home.join("config.rokon");
    let text = config_text(&path)?;
    config_field(&text, &path.display().to_string(), field)
}

/// `path`'s text; an empty config when there is none. Any other read
/// error is fatal, so the file is never replaced by defaults.
fn config_text(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("Config {}\n".to_owned()),
        Err(e) => Err(format!(
            "cannot read {}: {e}\nhelp: fix its permissions or save it as UTF-8, \
             or move it aside to start from the defaults",
            path.display()
        )),
    }
}

/// String field `field` of the config text `text`, read from `file`.
/// Fields a newer rok wrote are skipped.
fn config_field(text: &str, file: &str, field: &str) -> Result<Option<String>, String> {
    let doc = rokc_rokon::Doc::parse(text, file)?;
    let c = doc.root().record("Config")?;
    skipped(file, &c.known(&CONFIG_FIELDS));
    c.opt(field).map(|r| r.string()).transpose()
}

/// `-v`, for readers too deep to be passed it.
static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Under `-v`, name the fields of shared file `file` this rok skipped;
/// once per file.
pub(crate) fn skipped(file: &str, fields: &[&str]) {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    if fields.is_empty() || !VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    {
        let mut seen = SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if seen.iter().any(|f| f == file) {
            return;
        }
        seen.push(file.to_owned());
    }
    let names: Vec<String> = fields.iter().map(|f| format!("`{f}`")).collect();
    term::note(&format!(
        "{file}: skipped {}, written by a newer rok (kept in the file)",
        names.join(", ")
    ));
}

/// Set string field `field` of `$ROK_HOME/config.rokon`, keeping the rest.
fn set_config(home: &Path, field: &str, value: &str) -> Result<(), String> {
    let path = home.join("config.rokon");
    let file = path.display().to_string();
    std::fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
    let old = config_text(&path)?;
    config_field(&old, &file, field)?;
    let step = [rokc_rokon::edit::Step::Field(field)];
    let text = rokc_rokon::edit::set(&old, &step, &rokc_rokon::quote(value))
        .map_err(|e| e.render(&old, &file))?;
    std::fs::write(&path, canonical_edit(&old, &text))
        .map_err(|e| format!("cannot write {file}: {e}"))
}

/// `new`, an edit of ROKON text `old`: in canonical form
/// when `old` was, else as edited (comments and layout kept).
fn canonical_edit(old: &str, new: &str) -> String {
    let canon = |t: &str| rokc_rokon::parse(t).ok().map(|v| rokc_rokon::to_string(&v));
    match (canon(old), canon(new)) {
        (Some(o), Some(n)) if o.trim() == old.trim() => n + "\n",
        _ => new.to_owned(),
    }
}

/// Run `rok` with `args` (without the program name); the exit code.
pub fn main_with(args: Vec<String>) -> i32 {
    main_with_os(args.into_iter().map(Into::into).collect())
}

/// [`main_with`] of the arguments as the system gives them: a program's
/// arguments (after `--`, or after a script's path) may be any bytes,
/// everything before them must be UTF-8.
pub fn main_with_os(args: Vec<std::ffi::OsString>) -> i32 {
    rokc_core::tmp::sweep_once(&["rok-install-", "rok-prebuild-"]);
    let mut g = Global {
        argv: args.clone(),
        start: std::env::current_dir().ok(),
        ..Global::default()
    };
    let mut rest = Vec::new();
    let total = args.len();
    let mut it = args.into_iter();
    // The next argument as a string; the error names its position.
    let next = |it: &mut std::vec::IntoIter<std::ffi::OsString>| {
        let a = it.next()?;
        Some(cli::utf8_arg(total - it.len(), a).map_err(|e| usage(&e)))
    };
    // The rest as given, and as strings for `post`.
    let take_tail = |g: &mut Global, it: &mut std::vec::IntoIter<std::ffi::OsString>| {
        g.tail = it.collect();
        g.tail
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<String>>()
    };
    // rok's own options, anywhere before `--`; the rest goes to rokc.
    while let Some(a) = next(&mut it) {
        let a = match a {
            Ok(a) => a,
            Err(code) => return code,
        };
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (a.clone(), None),
        };
        let value = |it: &mut std::vec::IntoIter<std::ffi::OsString>| match &inline {
            Some(v) => Ok(Some(v.clone())),
            None => next(it).transpose(),
        };
        match name.as_str() {
            "--" => {
                rest.push(a);
                rest.extend(take_tail(&mut g, &mut it));
            }
            "--env" => match value(&mut it) {
                Ok(Some(v)) => g.env = Some(v),
                Ok(None) => return usage("`--env` needs a name"),
                Err(code) => return code,
            },
            "--no-color" | "--color" => {
                let when = if name == "--color" {
                    match value(&mut it) {
                        Ok(v) => v,
                        Err(code) => return code,
                    }
                } else {
                    Some("never".to_owned())
                };
                let Some(when) = when else {
                    return usage("`--color` needs a value");
                };
                let Some(c) = ColorChoice::parse(&when) else {
                    return usage(&format!("`--color` takes auto|always|never, not `{when}`"));
                };
                term::set_choice(c);
                g.color = Some(format!("--color={when}"));
            }
            "--offline" => g.offline = true,
            "--locked" => g.locked = true,
            "--cwd" => {
                let dir = match value(&mut it) {
                    Ok(Some(dir)) => dir,
                    Ok(None) => return usage("`--cwd` needs a directory"),
                    Err(code) => return code,
                };
                if let Err(e) = std::env::set_current_dir(&dir) {
                    return usage(&format!("--cwd {dir}: {e}"));
                }
            }
            "-q" | "--quiet" => {
                g.quiet = true;
                rest.push(a);
            }
            "-v" | "--verbose" => {
                g.verbose = true;
                rest.push(a);
            }
            // `rok +<version> ..`, before the command.
            t if t.len() > 1 && t.starts_with('+') && rest.iter().all(|r| r.starts_with('-')) => {
                g.toolchain = Some(t[1..].to_owned());
            }
            // `rok <file> [args..]` is `rok run -q <file> -- args..`; a path
            // has a `/` or ends in `.rok`, which no command does.
            p if looks_like_script(p)
                && rest.iter().all(|r| r.starts_with('-'))
                && rest
                    .last()
                    .is_none_or(|l| !VALUE_OPTIONS.contains(&l.as_str())) =>
            {
                let mut post = vec![a, "-q".to_owned(), "--".to_owned()];
                post.extend(take_tail(&mut g, &mut it));
                return run(&g, "run", &rest, &post);
            }
            _ => rest.push(a),
        }
    }
    // `--explain <code>` takes no command.
    let explain = rest
        .iter()
        .take_while(|a| *a != "--")
        .enumerate()
        .find_map(|(j, a)| match a.strip_prefix("--explain") {
            Some("") => Some(rest.get(j + 1).cloned().unwrap_or_default()),
            Some(v) => v.strip_prefix('=').map(str::to_owned),
            None => None,
        });
    if let Some(code) = explain {
        return match rokc_diagnostic::explain(&code) {
            Ok(text) => {
                print!("{text}");
                0
            }
            Err(e) => usage(&e),
        };
    }
    // The command is the first non-option word; rokc options with a
    // separate value go after it. `--version --format json` has no
    // command, so `json` there is the value.
    let is_version = |a: &String| a == "-V" || a == "--version";
    let Some(i) = (0..rest.len()).find(|&j| {
        let a = &rest[j];
        (!a.starts_with('-') || a == "--")
            && !(j > 0 && rest[j - 1] == "--format" && rest[..j].iter().any(is_version))
    }) else {
        if rest.iter().any(is_version) {
            return match json_format(&rest, "version") {
                Ok(json) => {
                    print!("{}", version(&g, json));
                    0
                }
                Err(e) => usage(&e),
            };
        }
        if rest.iter().any(|a| a == "-h" || a == "--help") {
            print!("{}", help::short_usage());
            return 0;
        }
        // Bare `rok` (options only) is `rok repl`, like `python` / `julia`.
        return run(&g, "repl", &rest, &[]);
    };
    if rest[i] == "--" {
        return run(&g, "repl", &rest[..i], &rest[i..]);
    }
    let (cmd, post) = (&rest[i], &rest[i + 1..]);
    let before_dashes = post.iter().take_while(|a| *a != "--");
    if before_dashes
        .into_iter()
        .any(|a| a == "-h" || a == "--help")
    {
        return help(cmd);
    }
    // `-q` / `-v` were taken above; rokc takes them again, rok's own
    // commands do not.
    let to_rokc = DELEGATED.contains(&cmd.as_str())
        && !(cmd == "jupyter" && post.first().is_some_and(|p| p == "install"));
    let post: Vec<String> = if to_rokc {
        post.to_vec()
    } else {
        let end = post.iter().position(|a| a == "--").unwrap_or(post.len());
        let (opts, after) = post.split_at(end);
        opts.iter()
            .filter(|a| !matches!(a.as_str(), "-q" | "--quiet" | "-v" | "--verbose"))
            .chain(after)
            .cloned()
            .collect()
    };
    run(&g, cmd, &rest[..i], &post)
}

/// `rok --version`; with `-v`, which binary and toolchain that is, as
/// `key: value` lines like `rokc --version -v`. `json`: one object.
fn version(g: &Global, json: bool) -> String {
    let rows = if g.verbose {
        toolchain::describe(g.toolchain.as_deref()).to_vec()
    } else {
        Vec::new()
    };
    if json {
        let mut fields = vec![
            (
                "schema".to_owned(),
                Json::num(rokc_diagnostic::RECORD_SCHEMA),
            ),
            ("version".to_owned(), Json::str(env!("CARGO_PKG_VERSION"))),
            ("line".to_owned(), Json::str(VERSION_LINE)),
        ];
        fields.extend(rows.into_iter().map(|(k, v)| (k.to_owned(), Json::Str(v))));
        return format!("{}\n", Json::Obj(fields));
    }
    let rows: Vec<(String, String)> = rows
        .into_iter()
        .map(|(k, v)| (format!("{k}:"), v))
        .collect();
    format!("rok {VERSION_LINE}\n{}", term::list(&rows, None, ""))
}

/// `--format json` / `--format=json` in `args` (before `--`); an error
/// for any other format.
fn json_format(args: &[String], cmd: &str) -> Result<bool, String> {
    let mut it = args.iter().take_while(|a| *a != "--");
    let mut json = false;
    while let Some(a) = it.next() {
        let v = match a.strip_prefix("--format") {
            Some("") => it.next().map(String::as_str),
            Some(v) if v.starts_with('=') => Some(&v[1..]),
            _ => continue,
        };
        json = match v {
            Some("json") => true,
            Some("human") => false,
            Some(v) => {
                return Err(format!(
                    "`--format` takes human or json, not `{v}`: write `rok {cmd} --format json`"
                ));
            }
            None => return Err("`--format` needs a value".to_owned()),
        };
    }
    Ok(json)
}

/// The commands `rok` passes to `rokc`.
const DELEGATED: [&str; 15] = [
    "build", "check", "run", "test", "repl", "lsp", "fmt", "clean", "demangle", "emit", "bench",
    "doc", "lint", "jupyter", "profdata",
];

/// `rok <cmd> --help` / `rok help <cmd>`.
fn help(cmd: &str) -> i32 {
    match help::command(cmd) {
        Some(text) => {
            print!("{text}");
            0
        }
        None => usage(&unknown_command(cmd)),
    }
}

/// "unknown command" for `cmd`, with what to write instead.
fn unknown_command(cmd: &str) -> String {
    let names = help::all().map(|h| h.name).chain(["toolchain list"]);
    let hint = cli::did_you_mean(cmd, names, cli::CMD_SYNONYMS);
    format!("unknown command `{cmd}`{hint}")
}

/// Errors that are about the command line (exit code 2).
fn is_usage(e: &str) -> bool {
    [
        "usage:",
        "unknown option",
        "unexpected argument",
        "give at most one of",
    ]
    .iter()
    .any(|p| e.starts_with(p))
        || e.starts_with('`') && e.contains("` needs a value")
        || e.starts_with("`--format` takes ")
}

fn run(g: &Global, cmd: &str, pre: &[String], post: &[String]) -> i32 {
    VERBOSE.store(g.verbose && !g.quiet, std::sync::atomic::Ordering::Relaxed);
    // A pin naming another toolchain: its own rok runs the command, so rok
    // and rokc always come from one toolchain. Commands that
    // run rokc decide in `rokc_command`, by the package they work on.
    if !matches!(
        cmd,
        "toolchain" | "help" | "version" | "completions" | "__complete"
    ) && !DELEGATED.contains(&cmd)
    {
        match toolchain::handoff(g.toolchain.as_deref(), toolchain::project_pin(), false) {
            Ok(Some(h)) => return toolchain::run_handoff(&h, &g.argv, g.start.as_deref()),
            Ok(None) => {}
            Err(e) => {
                eprintln!("rok: {} {e}", term::label(Stream::Stderr, Role::Error));
                return 1;
            }
        }
    }
    let r = match cmd {
        "version" => json_format(post, cmd).map(|json| {
            print!("{}", version(g, json));
            0
        }),
        "help" if !post.is_empty() => return help(&post[0]),
        "help" => {
            print!("{}", help::short_usage());
            Ok(0)
        }
        "new" | "init" => cmd_new(g, cmd, post),
        "add" => cmd_add(g, post),
        "remove" => cmd_remove(g, post),
        "install" => cmd_install(g, post),
        "uninstall" => cmd_uninstall(g, post),
        "update" => cmd_update(g, post),
        "deps" => cmd_deps(g, post),
        "search" => cmd_search(g, post),
        "publish" => cmd_publish(g, post),
        "yank" => cmd_yank(g, post),
        "login" => cmd_login(g, post),
        "logout" => cmd_logout(g, post),
        "env" => env::cmd(g, post),
        "toolchain" => toolchain::cmd(post, g.quiet, g.verbose),
        "completions" => cmd_completions(post),
        "__complete" => complete(post),
        "jupyter" if post.first().is_some_and(|p| p == "install") => install_kernel(g, &post[1..]),
        "jupyter" if post.is_empty() => return usage_of(cmd, cli::JUPYTER_BARE),
        "test" | "bench" => harness(g, cmd, pre, post),
        c if DELEGATED.contains(&c) => delegate(g, cmd, pre, post),
        other => return usage(&unknown_command(other)),
    };
    r.unwrap_or_else(|e| {
        if is_usage(&e) {
            return usage_of(cmd, &e);
        }
        // A rendered diagnostic (`error[E..]: ..`) as it is.
        if e.starts_with("error[") {
            eprintln!("{e}");
        } else {
            eprintln!("rok: {} {e}", term::label(Stream::Stderr, Role::Error));
        }
        1
    })
}

fn usage(msg: &str) -> i32 {
    eprintln!("rok: {msg}\nrun `rok help` for usage");
    2
}

/// A usage error of command `cmd`: an unknown option gets the closest of
/// its options, and the pointer is to the command's help.
fn usage_of(cmd: &str, msg: &str) -> i32 {
    let mut msg = msg.to_owned();
    let unknown = msg
        .strip_prefix("unknown option `")
        .and_then(|r| r.strip_suffix('`'))
        .map(str::to_owned);
    if let Some(flag) = unknown {
        let spec = completion_spec();
        let sub = format!("{cmd} ");
        let cands: Vec<&str> = spec
            .flags
            .iter()
            .filter(|(k, _)| *k == cmd || k.starts_with(&sub))
            .flat_map(|(_, v)| v)
            .chain(&spec.global)
            .map(String::as_str)
            .collect();
        msg.push_str(&cli::did_you_mean(&flag, cands, &[]));
    }
    if help::find(cmd).is_none() {
        return usage(&msg);
    }
    eprintln!("rok: {msg}\nrun `rok help {cmd}` for usage");
    2
}

/// What rok's command line takes, read from its help; the commands rok
/// passes to rokc take rokc's flags too.
fn completion_spec() -> Spec {
    let cmds: Vec<&cli::CmdHelp> = help::all().collect();
    let mut s = Spec::from_commands("rok", &cmds, help::GLOBAL);
    let rokc = cli::rokc_spec();
    for c in DELEGATED {
        let mut flags = rokc.flags.get(c).cloned().unwrap_or_default();
        flags.extend(rokc.global.iter().cloned());
        s.add_flags(c, &flags);
    }
    let features: Vec<String> = help::FEATURES
        .iter()
        .filter_map(|(head, _)| head.split(' ').next())
        .map(str::to_owned)
        .collect();
    for c in help::WITH_FEATURES {
        s.add_flags(c, &features);
    }
    let run = |c: &str| Values::Run(c.to_owned());
    let toolchains = run("rok __complete toolchains");
    let envs = run("rok __complete envs");
    s.values = vec![("+".to_owned(), toolchains.clone())];
    for k in ["toolchain default", "toolchain remove"] {
        s.values.push((k.to_owned(), toolchains.clone()));
    }
    for k in ["env use", "env remove", "env shell", "env export", "--env"] {
        s.values.push((k.to_owned(), envs.clone()));
    }
    s.values.extend([
        ("--target".to_owned(), run("rokc --print target-list")),
        ("help".to_owned(), Values::Commands),
    ]);
    s
}

/// `rok completions <shell>`.
fn cmd_completions(args: &[String]) -> Result<i32, String> {
    let [shell] = args else {
        return Err("usage: rok completions bash|zsh|fish|powershell".to_owned());
    };
    let script = completion_spec().render(shell).ok_or_else(|| {
        format!(
            "usage: rok completions bash|zsh|fish|powershell (no completion script for `{shell}`)"
        )
    })?;
    print!("{script}");
    Ok(0)
}

/// `rok __complete toolchains | envs`: the names the completion scripts
/// offer, one per line.
fn complete(args: &[String]) -> Result<i32, String> {
    let names = match args {
        [w] if w == "toolchains" => toolchain::versions(&home()),
        [w] if w == "envs" => env::names(),
        _ => return Err("usage: rok __complete toolchains | envs".to_owned()),
    };
    for n in names {
        println!("{n}");
    }
    Ok(0)
}

/// The package directory `start` belongs to: the nearest directory at
/// or above it holding `src/main.rok`, `src/lib.rok` or `rok.rokon`.
fn package_dir(start: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(start).ok()?;
    let from = if abs.is_file() { abs.parent()? } else { &abs };
    from.ancestors()
        .find(|d| {
            d.join("src/main.rok").is_file()
                || d.join("src/lib.rok").is_file()
                || manifest::find(d).is_some()
        })
        .map(Path::to_path_buf)
}

/// What a command works on, and where its dependencies are prebuilt when
/// it is not a package (an environment's or a script's `target/`).
struct Target {
    ctx: Context,
    env: Option<String>,
    target_dir: Option<PathBuf>,
}

fn env_dir(name: &str) -> PathBuf {
    home().join("envs").join(name)
}

/// The environment used outside a project: `$ROK_ENV`, `rok env use`'s
/// choice, or `default`.
fn ambient_env() -> Result<String, String> {
    if let Some(e) = std::env::var("ROK_ENV").ok().filter(|e| !e.is_empty()) {
        env::name("ROK_ENV", &e)?;
        return Ok(e);
    }
    Ok(env::chosen()?.unwrap_or_else(|| "default".to_owned()))
}

fn load_dir(dir: &Path) -> Result<Context, String> {
    let manifest = match manifest::find(dir) {
        Some(p) => {
            let mut m = manifest::load(&p)?;
            m.lints = manifest::inherit_lints(&m, &p)?;
            m
        }
        None => Manifest::default(),
    };
    Ok(Context {
        manifest,
        base: dir.to_path_buf(),
        lock_path: dir.is_dir().then(|| dir.join(manifest::LOCK)),
        script: false,
    })
}

fn env_target(name: &str) -> Result<Target, String> {
    Ok(Target {
        ctx: load_dir(&env_dir(name))?,
        env: Some(name.to_owned()),
        target_dir: Some(env_dir(name).join("target")),
    })
}

/// The environment a command outside a package works in: `--env`
/// (which must exist, but for `default`), else `ambient_env`.
fn selected_env(g: &Global) -> Result<String, String> {
    match &g.env {
        Some(name) => {
            env::name("--env", name)?;
            if !env_dir(name).is_dir() && name != "default" {
                return Err(format!(
                    "no environment `{name}` (create it: rok env create {name})"
                ));
            }
            Ok(name.clone())
        }
        None => ambient_env(),
    }
}

/// The context for `path` (default: cwd). A package sees only its own
/// `rok.rokon`'s deps; anything else works in an environment.
fn target_for(g: &Global, path: Option<&str>) -> Result<Target, String> {
    let start = Path::new(path.unwrap_or("."));
    let package = package_dir(start);
    if let (Some(dir), None) = (&package, &g.env) {
        return Ok(Target {
            ctx: load_dir(dir)?,
            env: None,
            target_dir: None,
        });
    }
    let env = selected_env(g)?;
    if package.is_none() && start.is_file() {
        return script_target(start, &env);
    }
    env_target(&env)
}

/// `path` relative to cwd when under it.
fn shown(path: &Path) -> String {
    let cwd = std::env::current_dir().and_then(rokc_core::real_path);
    match cwd {
        Ok(c) if path.starts_with(&c) => relative(&c, path).display().to_string(),
        _ => path.display().to_string(),
    }
}

/// Script `root` and the files its `mod` declarations reach, as rokc
/// finds them, with their text.
fn script_files(root: &Path) -> Vec<(PathBuf, String)> {
    use rokc_lexer::TokenKind as T;
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    let dir = root.parent().map(Path::to_path_buf);
    let mut todo = vec![(root.to_path_buf(), dir)];
    while let Some((file, dir)) = todo.pop() {
        let Ok(src) = std::fs::read_to_string(&file) else {
            continue;
        };
        if out.iter().any(|(f, _)| *f == file) {
            continue;
        }
        // A token scan, not a parse: a `#[cfg]`-removed `mod` still counts.
        if let Some(dir) = dir.filter(|_| src.contains("mod")) {
            let (tokens, _) = rokc_lexer::Lexer::new(&src, "").lex_all();
            let kinds: Vec<&T> = tokens.iter().map(|t| &t.kind).collect();
            let mut path = None;
            for (i, k) in kinds.iter().enumerate() {
                match (k, kinds.get(i + 1..i + 5)) {
                    (T::HashLBracket, Some([T::Ident(a), T::LParen, T::StrLit(p), T::RParen]))
                        if a == "path" =>
                    {
                        path = Some(p.clone());
                    }
                    (T::Mod, Some([T::Ident(name), T::Semi, ..])) => {
                        let (body, sub) = match path.take() {
                            Some(p) => {
                                let body = dir.join(p);
                                let sub = body.with_extension("");
                                (body, sub)
                            }
                            None => (dir.join(format!("{name}.rok")), dir.join(name)),
                        };
                        todo.push((body, Some(sub)));
                    }
                    (T::Semi | T::LBrace | T::RBrace, _) => path = None,
                    _ => {}
                }
            }
        }
        out.push((file, src));
    }
    // The order rokc reads them in does not matter; the key's does.
    if let Some(rest) = out.get_mut(1..) {
        rest.sort_by(|a, b| a.0.cmp(&b.0));
    }
    out
}

/// A lone script: the merged `deps` / `native` of its and its modules'
/// `//! ```rokon` blocks (paths relative to each file); conflicting
/// requirements are an error. Lock and builds live in
/// `cache/scripts/<hash>/`, keyed by every file's path and block.
fn script_target(file: &Path, env: &str) -> Result<Target, String> {
    let abs = rokc_core::real_path(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut manifest = Manifest::default();
    // Per dependency: (name, file whose block has it, as written, merged).
    let mut by: Vec<(String, String, String, String)> = Vec::new();
    let mut key = abs.display().to_string();
    for (f, src) in script_files(&abs) {
        let Some(block) = manifest::script_block(&src) else {
            continue;
        };
        let name = shown(&f);
        key += &format!("\0{}\0{}", f.display(), block.text);
        let label = format!("{name} (//! rokon block)");
        let m = manifest::parse(&block.text, &label)?;
        for w in &m.warnings {
            warn_once(w);
        }
        if m.dev_deps.len() + m.lints.len() + m.features.len() > 0
            || m.lints_from.is_some()
            || !m.no_overflow_checks.is_empty()
            || m.name.is_some()
            || m.version.is_some()
        {
            return Err(format!(
                "{label}: a script block holds only `deps`, `native` and `toolchain`"
            ));
        }
        // The script run picks the toolchain (`toolchain::script_pin`).
        if m.toolchain.is_some() && f != abs {
            return Err(format!(
                "{label}: `toolchain` goes in the block of {}, the script that is run; remove \
                 it here",
                shown(&abs)
            ));
        }
        let dir = f.parent().unwrap_or_else(|| Path::new("/"));
        let local = |p: &str| {
            let joined = dir.join(p);
            rokc_core::real_path(&joined)
                .unwrap_or(joined)
                .display()
                .to_string()
                .replace('\\', "/")
        };
        let mut merge = |dep: &str, req: String, text: String| -> Result<bool, String> {
            match by.iter().find(|b| manifest::same(&b.0, dep)) {
                Some(b) if b.3 == text => Ok(false),
                Some((n, file1, req1, _)) => Err(format!(
                    "`{n}` is required as {req1} in {file1} and as {req} in {name}: use one \
                     version, the same entry in both blocks (or keep it in one of them)"
                )),
                None => {
                    by.push((dep.to_owned(), name.clone(), req, text));
                    Ok(true)
                }
            }
        };
        for mut d in m.deps {
            let req = d.source.to_rok();
            if let Source::Path(p) = &d.source {
                d.source = Source::Path(local(p));
            }
            let text = rokc_rokon::to_line(&d.to_value());
            if merge(&d.name, req, text)? {
                manifest.deps.push(d);
            }
        }
        for mut d in m.native {
            let req = rokc_rokon::to_line(&d.source.to_value());
            if let manifest::NativeSource::Direct { url, .. } = &mut d.source {
                if !url.contains("://") && Path::new(url.as_str()).is_relative() {
                    *url = local(url);
                }
            }
            let text = rokc_rokon::to_line(&d.source.to_value());
            if merge(&format!("native {}", d.name), req, text)? {
                manifest.native.push(d);
            }
        }
    }
    let cache = home()
        .join("cache/scripts")
        .join(format!("{:016x}", rokc_core::hash::fnv(key.as_bytes())));
    std::fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    Ok(Target {
        ctx: Context {
            manifest,
            base: abs.parent().unwrap_or_else(|| Path::new("/")).to_path_buf(),
            lock_path: Some(cache.join(manifest::LOCK)),
            script: true,
        },
        env: Some(env.to_owned()),
        target_dir: Some(cache.join("target")),
    })
}

fn options(g: &Global) -> Options {
    Options {
        offline: g.offline,
        locked: g.locked,
        quiet: g.quiet,
        ..Options::default()
    }
}

/// `args` without the feature options before `--`, which go to `o`.
fn feature_flags(args: &[String], o: &mut Options) -> Result<Vec<String>, String> {
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => {
                rest.push(a.clone());
                rest.extend(it.by_ref().cloned());
            }
            "--all-features" => o.all_features = true,
            "--no-default-features" => o.no_default_features = true,
            "--features" => {
                let v = it.next().ok_or("`--features` needs a value")?;
                o.features.extend(split_features(v));
            }
            _ => match a.strip_prefix("--features=") {
                Some(v) => o.features.extend(split_features(v)),
                None => rest.push(a.clone()),
            },
        }
    }
    Ok(rest)
}

/// `a,b c` -> `a`, `b`, `c`.
fn split_features(v: &str) -> impl Iterator<Item = String> {
    v.split([',', ' '])
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
}

/// Push `lints` as `-A/-W/-D` flags; where they end.
fn push_lints(extra: &mut Vec<String>, lints: &[(String, rokc_core::lint::Level)]) -> usize {
    for (name, level) in lints {
        extra.push(level.flag().to_owned());
        extra.push(name.clone());
    }
    extra.len()
}

/// Say that the levels before `at` are `ctx`'s manifest `lints`, so rokc
/// names rok.rokon as their source. The rokc is always this rok's own (a
/// pinned toolchain gets the whole command via `handoff`).
fn mark_manifest_lints(extra: &mut Vec<String>, at: usize, ctx: &resolve::Context) {
    if ctx.manifest.lints.is_empty() {
        return;
    }
    if let Some(path) = manifest::find(&ctx.base) {
        extra.insert(at, format!("--lints-from-manifest={}", path.display()));
    }
}

/// Run `rokc <pre> <cmd> [--extern..] [lint levels] [--target-dir] <post>`.
fn delegate(g: &Global, cmd: &str, pre: &[String], post: &[String]) -> Result<i32, String> {
    let (c, rokc) = rokc_command(g, cmd, pre, post, None)?;
    Ok(exec(c, &rokc))
}

/// `rok test` / `rok bench`: `rokc` builds the harness and writes a
/// manifest (the built executables and their cases); the runner here runs
/// each case, out of the compiler.
fn harness(g: &Global, cmd: &str, pre: &[String], post: &[String]) -> Result<i32, String> {
    let (mut c, rokc) = rokc_command(g, cmd, pre, post, None)?;
    let tmp = TempEntry::dir("rok-harness-").map_err(|e| e.to_string())?;
    let manifest = tmp.path().join("manifest");
    c.arg("--emit-harness").arg(&manifest);
    // Build the harness; its progress and diagnostics stream through.
    let status = c
        .status()
        .map_err(|e| format!("cannot run {}: {e}", rokc.display()))?;
    if !status.success() {
        return Ok(status.code().unwrap_or(1));
    }
    // No manifest means there was nothing to run (e.g. `--emit`).
    match std::fs::read_to_string(&manifest) {
        Ok(text) => {
            rokc_harness::check_version(&text)?;
            Ok(rokc_harness::run_manifest(
                &text,
                &mut std::io::stdout().lock(),
            ))
        }
        Err(_) => Ok(0),
    }
}

/// When `package` (the one a command works on), `script` or `+<version>`
/// / `ROK_TOOLCHAIN` pins another toolchain than this one, run the
/// command with that toolchain's rok and exit with its status.
fn hand_off_for(g: &Global, package: Option<&Path>, script: Option<&Path>) -> Result<(), String> {
    let pin = package.and_then(manifest::find).and_then(|p| {
        let pin = manifest::toolchain_pin(&std::fs::read_to_string(&p).ok()?)?;
        Some((pin, format!("pinned by {}", p.display())))
    });
    let pin = pin.or_else(|| script.and_then(toolchain::script_pin));
    if let Some(h) = toolchain::handoff(g.toolchain.as_deref(), pin, true)? {
        std::process::exit(toolchain::run_handoff(&h, &g.argv, g.start.as_deref()));
    }
    Ok(())
}

/// `delegate`'s rokc command, and the rokc it runs; with `in_env`, on
/// that environment's libraries (`prebuild_env`).
fn rokc_command(
    g: &Global,
    cmd: &str,
    pre: &[String],
    post: &[String],
    in_env: Option<&str>,
) -> Result<(Command, PathBuf), String> {
    let mut opts = options(g);
    opts.dev = matches!(cmd, "test" | "bench");
    let resolves = !matches!(cmd, "fmt" | "clean" | "demangle" | "profdata");
    let post = &if resolves {
        feature_flags(post, &mut opts)?
    } else {
        post.to_vec()
    };
    // The path is the first positional argument after the command.
    let first = positional(post);
    let path = first.filter(|p| *p != "-" && Path::new(p).exists());
    // Commands on a package say so here, before resolving anything.
    let start = Path::new(path.unwrap_or("."));
    let stdin = first == Some("-");
    let package = package_dir(start);
    // `doc --std` without a path documents the standard library alone and
    // needs no package.
    let std_doc = cmd == "doc" && first.is_none() && post.iter().any(|a| a == "--std");
    // Without a path, a REPL / kernel / language server may use an
    // environment from inside a package.
    let on_package = package.is_some()
        && !stdin
        && (path.is_some() || !matches!(cmd, "repl" | "jupyter" | "lsp"));
    if let Some(env) = g.env.as_ref().filter(|_| on_package && resolves) {
        return Err(format!(
            "`--env {env}` is for scripts and the REPL: a package builds with the dependencies \
             in its rok.rokon only (add one: `rok add <name>`)"
        ));
    }
    // A missing file is that, not a missing package; for test / bench a
    // word may be a filter.
    if let Some(p) = first.filter(|p| *p != "-" && path.is_none()) {
        let file = Path::new(p).extension().is_some_and(|x| x == "rok");
        if matches!(
            cmd,
            "build" | "check" | "run" | "doc" | "lint" | "emit" | "clean" | "repl" | "fmt"
        ) || file && matches!(cmd, "test" | "bench")
        {
            return Err(format!("`{p}` does not exist"));
        }
    }
    if matches!(
        cmd,
        "build" | "check" | "run" | "test" | "bench" | "doc" | "lint" | "emit" | "clean"
    ) && package.is_none()
        && !start.is_file()
        && !stdin
        && !std_doc
    {
        return Err(format!(
            "not inside a package: no src/main.rok or src/lib.rok in {} or above (make one with \
             `rok new <name>` or `rok init`, or name a .rok file)",
            std::path::absolute(start)
                .unwrap_or_else(|_| start.to_path_buf())
                .display()
        ));
    }
    // Before reading its manifest (which may have newer fields) or lock.
    hand_off_for(
        g,
        package
            .as_deref()
            .filter(|_| in_env.is_none() && g.env.is_none()),
        Some(start).filter(|s| package.is_none() && in_env.is_none() && s.is_file()),
    )?;
    let t = match in_env {
        Some(e) => env_target(e)?,
        None => target_for(g, path)?,
    };
    let mut extra: Vec<String> = Vec::new();
    let mut manifest_lints = None;
    // The manifest's `name` is the package's.
    let named = package.as_deref().filter(|_| t.env.is_none() && !stdin);
    if let (Some(name), Some(_)) = (&t.ctx.manifest.name, named) {
        if !matches!(
            cmd,
            "fmt" | "clean" | "demangle" | "profdata" | "lsp" | "jupyter"
        ) {
            extra.push(format!("--name={name}"));
        }
    }
    if resolves {
        opts.target = rokc_target::TargetSpec::resolve(target_arg(post)).ok();
        let graph = match resolve::resolve(&home(), &t.ctx, &opts) {
            Ok(r) => r,
            // The language server still starts (without dependencies).
            Err(e) if cmd == "lsp" => {
                eprintln!("rok: {} {e}", term::label(Stream::Stderr, Role::Warning));
                resolve::Graph {
                    packages: Vec::new(),
                    natives: Vec::new(),
                    features: Vec::new(),
                }
            }
            Err(e) => return Err(e),
        };
        for p in &graph.packages {
            extra.extend(extern_args(p));
        }
        // The package's own features.
        for f in &graph.features {
            extra.push(format!("--cfg=feature={f}"));
        }
        // Native packages a link (or the REPL session) needs for the target.
        if matches!(cmd, "build" | "run" | "test" | "bench" | "repl" | "jupyter") {
            let natives: Vec<&native::Resolved> = graph.natives.iter().collect();
            if !natives.is_empty() {
                let target = rokc_target::TargetSpec::resolve(target_arg(post))?;
                extra.extend(native::rokc_args(
                    &home(),
                    &natives,
                    &target,
                    g.offline,
                    g.quiet,
                )?);
            }
        }
        // rokc picks the profile; `-C overflow-checks` beats these.
        for p in &t.ctx.manifest.no_overflow_checks {
            extra.push(format!("--no-overflow-checks-in={p}"));
        }
        // `lints` go first: the command line's own `-A/-W/-D` win.
        manifest_lints = Some(push_lints(&mut extra, &t.ctx.manifest.lints));
        // `doc --std` writes ./target/doc/, not into an environment.
        if let Some(dir) = t.target_dir.as_ref().filter(|_| !on_package && !std_doc) {
            extra.push(format!("--target-dir={}", dir.display()));
        }
    }
    let rokc = toolchain::rokc_for()?;
    if let Some(at) = manifest_lints {
        mark_manifest_lints(&mut extra, at, &t.ctx);
    }
    let mut c = Command::new(&rokc);
    c.args(&g.color).args(pre).arg(cmd).args(&extra);
    post_args(&mut c, post, &g.tail);
    let me = std::env::current_exe().ok();
    handoff(&mut c, "ROK", me.as_deref().map(Path::as_os_str));
    // rokc's usage errors and help then speak of `rok`.
    handoff(&mut c, "ROK_CLI", Some("rok".as_ref()));
    // The REPL's `:add` installs into this environment (or, with none,
    // adds to the package).
    handoff(&mut c, "ROK_ENV", t.env.as_deref().map(AsRef::as_ref));
    // Tell rokc what the environment has, so a script missing one of
    // those gets a hint.
    let libs = t.env.as_deref().filter(|_| t.ctx.script).map(|e| {
        let libs: Vec<String> = load_dir(&env_dir(e))
            .map(|c| c.manifest.deps.into_iter().map(|d| d.name).collect())
            .unwrap_or_default();
        libs.join(",")
    });
    handoff(&mut c, "ROK_ENV_LIBS", libs.as_deref().map(AsRef::as_ref));
    Ok((c, rokc))
}

/// Hand `value` to rokc as `__<name>` (`None`: unset it).
fn handoff(c: &mut Command, name: &str, value: Option<&std::ffi::OsStr>) {
    let n = format!("__{name}");
    match value {
        Some(v) => c.env(n, v),
        None => c.env_remove(n),
    };
}

/// `post` for `c`, its program's arguments after `--` as given (`tail`).
fn post_args(c: &mut Command, post: &[String], tail: &[std::ffi::OsString]) {
    match post.iter().position(|a| a == "--") {
        Some(i) if post.len() - i - 1 == tail.len() => c.args(&post[..=i]).args(tail),
        _ => c.args(post),
    };
}

/// rokc's options that take a separate value (`--target x86_64-..`).
const VALUE_OPTIONS: [&str; 33] = [
    "--color",
    "--cwd",
    "--message-format",
    "-o",
    "--output",
    "--bootstrap",
    "--cfg",
    "--print",
    "--explain",
    "--extern",
    "--extern-feature",
    "--name",
    "--target-dir",
    "--target",
    "--rokr",
    "-L",
    "-l",
    "--native-runtime",
    "--emit",
    "-C",
    "--codegen",
    "--topn",
    "--connection-file",
    "-A",
    "-W",
    "-D",
    "--allow",
    "--warn",
    "--deny",
    "--iters",
    "--samples",
    "--format",
    "--threads",
];

/// The first argument before `--` that is neither an option nor its value
/// (`-` counts: stdin).
fn positional(args: &[String]) -> Option<&str> {
    let mut it = args.iter().take_while(|a| *a != "--");
    while let Some(a) = it.next() {
        if VALUE_OPTIONS.contains(&a.as_str()) {
            it.next();
        } else if a == "-" || !a.starts_with('-') {
            return Some(a);
        }
    }
    None
}

/// `--extern=<name>=<lib.rok>`, one more per other name its dependents
/// give it (rokc's aliases), and its `--extern-feature=<name>=<f>`s.
fn extern_args(p: &Resolved) -> Vec<String> {
    let name = manifest::code_name(&p.code);
    let mut out: Vec<String> = std::iter::once(&p.code)
        .chain(&p.aliases)
        .map(|n| format!("--extern={}={}", manifest::code_name(n), p.root.display()))
        .collect();
    out.extend(
        p.features
            .iter()
            .map(|f| format!("--extern-feature={name}={f}")),
    );
    out
}

/// `--target <t>` / `--target=<t>` among rokc's arguments.
fn target_arg(post: &[String]) -> Option<&str> {
    let mut it = post.iter().take_while(|a| *a != "--");
    while let Some(a) = it.next() {
        if a == "--target" {
            return it.next().map(String::as_str);
        }
        if let Some(t) = a.strip_prefix("--target=") {
            return Some(t);
        }
    }
    None
}

/// Replace this process with `c` (unix), else wait for it.
pub(crate) fn exec(mut c: Command, what: &Path) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = c.exec();
        eprintln!(
            "rok: {} cannot run {}: {e}",
            term::label(Stream::Stderr, Role::Error),
            what.display()
        );
        1
    }
    #[cfg(windows)]
    {
        // Ctrl-C / Ctrl-Break reach every process on the console: let the
        // child decide and wait. A handler, unlike `NULL`, isn't inherited.
        unsafe extern "system" {
            fn SetConsoleCtrlHandler(
                handler: Option<extern "system" fn(u32) -> i32>,
                add: i32,
            ) -> i32;
        }
        const extern "system" fn ignore(_: u32) -> i32 {
            1
        }
        // SAFETY: registers a handler that only returns.
        unsafe { SetConsoleCtrlHandler(Some(ignore), 1) };
    }
    #[cfg(not(unix))]
    match c.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!(
                "rok: {} cannot run {}: {e}",
                term::label(Stream::Stderr, Role::Error),
                what.display()
            );
            1
        }
    }
}

const MAIN_TEMPLATE: &str = "println(\"Hello, world!\");\n";

const LIB_TEMPLATE: &str = "\
pub fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

#[test]
fn adds() {
    if add(2, 2) != 4 {
        panic(\"add(2, 2) != 4\");
    }
}
";

/// `rok new [--lib] [--bin] <name>`: a directory with `src/main.rok`
/// (default), `src/lib.rok`, or both, and `rok.rokon`. `init`: the same in
/// an existing directory without those files.
fn cmd_new(g: &Global, cmd: &str, args: &[String]) -> Result<i32, String> {
    let (mut lib, mut bin) = (false, false);
    let mut pos = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--lib" => lib = true,
            "--bin" => bin = true,
            "--script" if cmd == "init" && args.len() == 2 => {
                let file = it.next().ok_or("`--script` needs a value")?;
                return init_script(g, Path::new(file));
            }
            "--script" => return Err("usage: rok init --script <file.rok>".into()),
            f if f.starts_with('-') => return Err(format!("unknown option `{f}`")),
            p => pos.push(p),
        }
    }
    let bin = bin || !lib;
    let init = cmd == "init";
    let dir = match (init, pos.as_slice()) {
        (false, [name]) => PathBuf::from(name),
        (true, []) => PathBuf::from("."),
        (true, [d]) => PathBuf::from(d),
        _ => return Err(format!("usage: rok {cmd} [--lib] [--bin] <name>")),
    };
    // Check the name as typed, before touching the file system: on
    // Windows `nul` names the device, which exists and has no file name.
    if !init {
        if let Some(n) = dir.file_name().and_then(|n| n.to_str()) {
            manifest::check_new_name(n)?;
        }
    }
    if init {
        for f in ["src/main.rok", "src/lib.rok"] {
            if dir.join(f).exists() {
                return Err(format!("`{}` already exists", dir.join(f).display()));
            }
        }
    } else if dir.exists() {
        return Err(format!("`{}` already exists", dir.display()));
    }
    // The package name is the directory's.
    let abs = std::path::absolute(&dir).map_err(|e| e.to_string())?;
    let dir_name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("app");
    let name = if init {
        let mut n: String = dir_name
            .to_ascii_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        if !n.starts_with(|c: char| c.is_ascii_lowercase())
            || rokc_core::check_os_name("package", &n).is_err()
        {
            n.insert_str(0, "pkg-");
        }
        n.truncate(rokc_core::MAX_NAME);
        if n != dir_name && !g.quiet {
            eprintln!(
                "{} the package name is `{n}` (`{dir_name}` is not a package name)",
                term::label(Stream::Stderr, Role::Note)
            );
        }
        n
    } else {
        manifest::check_new_name(dir_name)?;
        dir_name.to_owned()
    };
    let src = dir.join("src");
    std::fs::create_dir_all(&src).map_err(|e| format!("{}: {e}", src.display()))?;
    // With both, the executable uses the library by the package's name.
    let main = if lib {
        format!(
            "use {}::add;\n\nprintln(f\"2 + 2 = {{add(2, 2)}}\");\n",
            manifest::code_name(&name)
        )
    } else {
        MAIN_TEMPLATE.to_owned()
    };
    for (file, text, on) in [
        ("src/lib.rok", LIB_TEMPLATE, lib),
        ("src/main.rok", main.as_str(), bin),
    ] {
        let root = dir.join(file);
        if on {
            std::fs::write(&root, text).map_err(|e| format!("{}: {e}", root.display()))?;
        }
    }
    if manifest::find(&dir).is_none() {
        let value = Value::record(
            Some("Package"),
            vec![
                ("name", Value::str(&name)),
                ("version", Value::str("0.1.0")),
                // Stays on this series when a newer one is the default.
                (
                    "toolchain",
                    Value::str(toolchain::series(VERSION).unwrap_or_else(|| VERSION.to_owned())),
                ),
                ("deps", Value::new(Kind::Dict(Vec::new()))),
            ],
        );
        let path = dir.join(MANIFEST);
        std::fs::write(&path, rokc_rokon::to_string(&value) + "\n")
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "target/\n").map_err(|e| format!("{}: {e}", ignore.display()))?;
    }
    // Its own git repository, unless already inside one or git is missing.
    let in_repo = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(&dir)
        .output();
    if in_repo.is_ok_and(|o| !o.status.success()) {
        let _ = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status();
    }
    if !g.quiet {
        let what = match (lib, bin) {
            (true, true) => "library and executable",
            (true, false) => "library",
            _ => "executable",
        };
        term::status("Created", &format!("{what} package ({})", dir.display()));
        // What to type next, under the message. Windows PowerShell 5.1
        // has no `&&`: one command per line there.
        let then = if bin { "rok run" } else { "rok test" };
        let d = dir.display().to_string();
        let cd = match d.as_str() {
            "." => None,
            d if d.contains(char::is_whitespace) => Some(format!("cd \"{d}\"")),
            d => Some(format!("cd {d}")),
        };
        let lines = match cd {
            None => vec![then.to_owned()],
            Some(cd) if cfg!(windows) => vec![cd, then.to_owned()],
            Some(cd) => vec![format!("{cd} && {then}")],
        };
        for l in lines {
            eprintln!("{:>12} {l}", "");
        }
    }
    Ok(0)
}

/// `rok init --script <file.rok>`: turn the script's directory into a
/// package named after it; the merged blocks go into `rok.rokon` and the
/// files move under `src/` (the script as `main.rok`) without their blocks.
fn init_script(g: &Global, file: &Path) -> Result<i32, String> {
    if package_dir(file).is_some() || !file.is_file() {
        return Err(format!(
            "`{}` is not a script outside a package",
            file.display()
        ));
    }
    let t = script_target(file, &ambient_env()?)?;
    let root = rokc_core::real_path(file).map_err(|e| e.to_string())?;
    let dir = root.parent().unwrap_or_else(|| Path::new("/"));
    let stem = root.file_stem().and_then(|s| s.to_str()).unwrap_or("app");
    let name = stem.to_ascii_lowercase();
    manifest::check_new_name(&name)?;
    let files = script_files(&root);
    let mut moves = Vec::new();
    for (f, src) in files {
        let to = if f == root {
            dir.join("src/main.rok")
        } else {
            let rel = f.strip_prefix(dir).map_err(|_| {
                format!(
                    "{} is outside {}: move it there first",
                    f.display(),
                    dir.display()
                )
            })?;
            dir.join("src").join(rel)
        };
        if to.exists() {
            return Err(format!("`{}` already exists", to.display()));
        }
        moves.push((f, to, manifest::set_script_block(&src, None)));
    }
    if dir.join(MANIFEST).exists() {
        return Err(format!("`{}` already exists", dir.join(MANIFEST).display()));
    }
    let deps = t.ctx.manifest.deps.iter().map(|d| {
        let mut d = d.clone();
        if let Source::Path(p) = &d.source {
            let rel = relative(dir, Path::new(p)).display().to_string();
            d.source = Source::Path(rel.replace('\\', "/"));
        }
        (Value::str(&d.name), d.to_value())
    });
    let mut fields = vec![
        ("name", Value::str(&name)),
        ("version", Value::str("0.1.0")),
        ("deps", Value::new(Kind::Dict(deps.collect()))),
    ];
    if !t.ctx.manifest.native.is_empty() {
        let natives = t.ctx.manifest.native.iter().map(|d| {
            let mut s = d.source.clone();
            if let NativeSource::Direct { url, .. } = &mut s {
                if Path::new(url.as_str()).is_absolute() {
                    *url = relative(dir, Path::new(url.as_str())).display().to_string();
                }
            }
            (Value::str(&d.name), s.to_value())
        });
        fields.push(("native", Value::new(Kind::Dict(natives.collect()))));
    }
    let manifest = rokc_rokon::to_string(&Value::record(Some("Package"), fields)) + "\n";
    let write = |p: &Path, t: &str| {
        std::fs::create_dir_all(p.parent().unwrap_or(dir))
            .and_then(|()| std::fs::write(p, t))
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    write(&dir.join(MANIFEST), &manifest)?;
    for (from, to, text) in &moves {
        write(to, text)?;
        std::fs::remove_file(from).map_err(|e| format!("{}: {e}", from.display()))?;
        // A submodule folder left empty goes too.
        if let Some(p) = from.parent().filter(|p| *p != dir) {
            let _ = std::fs::remove_dir(p);
        }
    }
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        write(&ignore, "target/\n")?;
    }
    if !g.quiet {
        term::status(
            "Created",
            &format!(
                "executable package `{name}` from {} ({})",
                file.display(),
                dir.display()
            ),
        );
    }
    Ok(0)
}

/// `rok jupyter install`: `kernels/<name>/kernel.json` under
/// `$JUPYTER_DATA_DIR` (else the per-user default), launching this `rok`.
fn install_kernel(g: &Global, args: &[String]) -> Result<i32, String> {
    if let Some(a) = args.first() {
        return Err(format!("`rok jupyter install` takes no `{a}`"));
    }
    if g.env.is_some() {
        selected_env(g)?;
    }
    // The proxy, when installed: the kernel follows the default toolchain.
    let proxy = home()
        .join("bin")
        .join(format!("rok{}", std::env::consts::EXE_SUFFIX));
    let exe = if proxy.is_file() {
        proxy
    } else {
        std::env::current_exe().map_err(|e| e.to_string())?
    };
    let mut argv = vec![exe.display().to_string()];
    if let Some(e) = &g.env {
        argv.extend(["--env".to_owned(), e.clone()]);
    }
    argv.extend(["jupyter", "--connection-file", "{connection_file}"].map(str::to_owned));
    let (name, display) = match &g.env {
        Some(e) => (format!("rok-{e}"), format!("rok ({e})")),
        None => ("rok".to_owned(), "rok".to_owned()),
    };
    let user_home = rokc_core::user_home;
    let data = std::env::var_os("JUPYTER_DATA_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            if cfg!(target_vendor = "apple") {
                Some(user_home()?.join("Library/Jupyter"))
            } else if cfg!(windows) {
                std::env::var_os("APPDATA")
                    .filter(|d| !d.is_empty())
                    .map(|d| PathBuf::from(d).join("jupyter"))
            } else {
                std::env::var_os("XDG_DATA_HOME")
                    .filter(|d| !d.is_empty())
                    .map(|d| PathBuf::from(d).join("jupyter"))
                    .or_else(|| Some(user_home()?.join(".local/share/jupyter")))
            }
        })
        .ok_or("cannot find the Jupyter data directory (set JUPYTER_DATA_DIR)")?;
    let dir = data.join("kernels").join(&name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut spec = vec![
        (
            "argv".to_owned(),
            Json::Arr(argv.into_iter().map(Json::Str).collect()),
        ),
        ("display_name".to_owned(), Json::Str(display)),
        ("language".to_owned(), Json::str("rok")),
        ("interrupt_mode".to_owned(), Json::str("message")),
    ];
    // A GUI-started Jupyter has a bare PATH; keep this shell's (LLVM tools).
    let mut env = vec![(
        "ROK_HOME".to_owned(),
        Json::Str(home().display().to_string()),
    )];
    if let Ok(path) = std::env::var("PATH") {
        env.push(("PATH".to_owned(), Json::Str(path)));
    }
    spec.push(("env".to_owned(), Json::Obj(env)));
    let file = dir.join("kernel.json");
    std::fs::write(&file, format!("{}\n", Json::Obj(spec)))
        .map_err(|e| format!("{}: {e}", file.display()))?;
    for (logo, png) in [
        (
            "logo-32x32.png",
            &include_bytes!("../assets/logo-32x32.png")[..],
        ),
        (
            "logo-64x64.png",
            &include_bytes!("../assets/logo-64x64.png")[..],
        ),
    ] {
        let logo = dir.join(logo);
        std::fs::write(&logo, png).map_err(|e| format!("{}: {e}", logo.display()))?;
    }
    if !g.quiet {
        term::status(
            "Installed",
            &format!("kernel `{name}` ({})", file.display()),
        );
    }
    Ok(0)
}

/// `rok add` arguments and `--dev`. A stone without `@req` gets an empty
/// `req` for the caller to fill.
pub fn parse_dep_args(args: &[String]) -> Result<(Dep, bool), String> {
    let mut name = None;
    let (mut path, mut git, mut reference, mut dev) = (None, None, GitRef::Default, false);
    let mut registry = None;
    let (mut features, mut optional, mut default_features) = (Vec::new(), false, true);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (a.as_str(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .ok_or_else(|| format!("`{flag}` needs a value"))
        };
        match flag {
            "--path" => path = Some(value()?),
            "--git" => git = Some(value()?),
            "--rev" | "--tag" | "--branch" if reference != GitRef::Default => {
                return Err("give at most one of --rev, --tag, --branch".into());
            }
            "--rev" => reference = GitRef::Rev(value()?),
            "--tag" => reference = GitRef::Tag(value()?),
            "--branch" => reference = GitRef::Branch(value()?),
            "--dev" => dev = true,
            "--registry" => registry = Some(value()?),
            "--features" => {
                for f in split_features(&value()?) {
                    manifest::check_feature(&f)?;
                    features.push(f);
                }
            }
            "--no-default-features" => default_features = false,
            "--optional" => optional = true,
            f if f.starts_with('-') => return Err(format!("unknown option `{f}`")),
            n if name.is_none() => name = Some(n.to_owned()),
            n => return Err(format!("unexpected argument `{n}`")),
        }
    }
    let usage = "usage: rok add <name>[@<req>] [--registry <url>] | <name> --path <dir> | \
                 <name> --git <url> [--rev|--tag|--branch <r>]  [--dev] [--features <f,..>] \
                 [--no-default-features] [--optional]";
    if optional && dev {
        return Err("a dev dependency cannot be --optional".into());
    }
    // `rok install --path / --git` may leave the name to the package.
    let name = match name {
        Some(n) => n,
        None if path.is_some() || git.is_some() => String::new(),
        None => return Err(usage.into()),
    };
    let (name, req) = match name.split_once('@') {
        Some((n, r)) => (n.to_owned(), Some(r.to_owned())),
        None => (name, None),
    };
    if !name.is_empty() {
        manifest::check_new_name(&name)?;
    }
    let source = match (path, git) {
        (None, None) if reference == GitRef::Default => {
            let req = req.unwrap_or_default();
            if !req.is_empty() {
                semver::Req::parse(&req)?;
            }
            Source::Registry { req, registry }
        }
        _ if req.is_some() || registry.is_some() => {
            return Err("`@<req>` / --registry go with a stone, not --path / --git".into());
        }
        (Some(p), None) if reference == GitRef::Default => Source::Path(p),
        (None, Some(url)) => Source::Git { url, reference },
        (_, None) => return Err("--rev / --tag / --branch go with --git".into()),
        _ => return Err(usage.into()),
    };
    Ok((
        Dep {
            optional,
            features,
            default_features,
            ..Dep::new(name, source)
        },
        dev,
    ))
}

/// The manifest to edit, created if missing: the package's for `add` /
/// `remove` (`env: None`), environment `env`'s for `install` / `uninstall`.
fn manifest_to_edit(env: Option<&str>) -> Result<(PathBuf, bool), String> {
    if let Some(name) = env {
        let dir = env_dir(name);
        if !dir.is_dir() {
            env::create(name)?;
        }
        return Ok((dir.join(MANIFEST), true));
    }
    let dir = package_dir(Path::new(".")).ok_or(
        "not inside a package (no src/main.rok, src/lib.rok or rok.rokon here or above): make \
         one with `rok init`; for a script, name it: `rok add <name> <file.rok>`",
    )?;
    Ok((
        manifest::find(&dir).unwrap_or_else(|| dir.join(MANIFEST)),
        false,
    ))
}

/// `a` relative to directory `from` (both absolute), with `..` steps.
fn relative(from: &Path, a: &Path) -> PathBuf {
    let f: Vec<_> = from.components().collect();
    let t: Vec<_> = a.components().collect();
    let common = f.iter().zip(&t).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..f.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Write `text` and resolve; restore `old` if resolution fails.
fn write_and_resolve(
    g: &Global,
    path: &Path,
    old: Option<&str>,
    text: &str,
) -> Result<resolve::Graph, String> {
    let text = canonical_edit(old.unwrap_or(manifest::EMPTY), text);
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let r = load_dir(dir).and_then(|ctx| resolve::resolve(&home(), &ctx, &options(g)));
    r.or_else(|e| {
        match old {
            Some(o) => std::fs::write(path, o),
            None => std::fs::remove_file(path),
        }
        .map_err(|e2| format!("{e}\n(and cannot restore {}: {e2})", path.display()))?;
        Err(e)
    })
}

/// The `version` in the rok.rokon of resolved dependency `name`.
fn dep_version(graph: &resolve::Graph, name: &str) -> Option<String> {
    let p = graph
        .packages
        .iter()
        .find(|p| manifest::same(&p.name, name))?;
    // `<dir>/src/lib.rok`
    let dir = p.root.parent()?.parent()?;
    manifest::load(&manifest::find(dir)?).ok()?.version
}

/// `--native <name>[@<req>] [--registry <url>]` into the package's,
/// environment `env`'s or `script`'s `native`.
fn add_native(
    g: &Global,
    args: &[String],
    env: Option<&str>,
    script: Option<&Path>,
) -> Result<i32, String> {
    let (vals, _, pos) = flags(args, &["--registry"], &["--native"])?;
    let [spec] = pos.as_slice() else {
        return Err("usage: rok add --native <name>[@<req>] [--registry <url>]".into());
    };
    let (name, req) = spec.split_once('@').unwrap_or((spec, ""));
    manifest::check_new_name(name)?;
    let registry = val(&vals, "--registry").map(str::to_owned);
    let req = if req.is_empty() {
        let url = match &registry {
            Some(r) => r.clone(),
            None => registry::default_url(&home())?,
        };
        let index = registry::open(&home(), &url, g.offline, g.quiet)?;
        let all = native::entries(&index, &url, name)?;
        let e = native::best(&all, &semver::Req::parse("*")?)
            .ok_or_else(|| native::no_match(&index, &all, name, "*", &url))?;
        format!("^{}", e.version)
    } else {
        semver::Req::parse(req)?;
        req.to_owned()
    };
    let source = NativeSource::Registry { req, registry };
    if let Some(file) = script {
        let value = rokc_rokon::to_line(&source.to_value());
        edit_script(g, file, |old, label| {
            manifest::set_dep(old, label, "native", name, &value).map(Some)
        })?;
        if !g.quiet {
            term::status("Added", &format!("{name} to native of {}", file.display()));
        }
        return Ok(0);
    }
    let (path, _) = manifest_to_edit(env)?;
    let old = std::fs::read_to_string(&path).ok();
    let text = manifest::set_dep(
        old.as_deref().unwrap_or(manifest::EMPTY),
        &path.display().to_string(),
        "native",
        name,
        &rokc_rokon::to_line(&source.to_value()),
    )?;
    write_and_resolve(g, &path, old.as_deref(), &text)?;
    if !g.quiet {
        term::status("Added", &format!("{name} to native of {}", path.display()));
    }
    Ok(0)
}

/// `rok add` / `remove` work on a package; `--env` is `rok install`'s.
fn package_only(g: &Global, cmd: &str, instead: &str) -> Result<(), String> {
    match &g.env {
        Some(e) => Err(format!(
            "`rok {cmd}` edits a package's rok.rokon or a script's block; for the libraries the \
             REPL / Jupyter see in environment `{e}`, run `rok {instead} --env {e} <name>`"
        )),
        None => Ok(()),
    }
}

fn cmd_add(g: &Global, args: &[String]) -> Result<i32, String> {
    package_only(g, "add", "install")?;
    let (script, args) = script_arg(args)?;
    if args.iter().any(|a| a == "--native") {
        return add_native(g, &args, None, script.as_deref());
    }
    let (dep, dev) = parse_dep_args(&args)?;
    if dep.name.is_empty() {
        return Err("usage: rok add <name> --path <dir> | <name> --git <url> ..".into());
    }
    match script {
        Some(file) => add_to_script(g, dep, dev, &file),
        None => add_dep(g, dep, dev, None),
    }
}

/// The options of `rok add` / `remove` that take a value.
const DEP_VALUE_OPTIONS: [&str; 7] = [
    "--path",
    "--git",
    "--rev",
    "--tag",
    "--branch",
    "--registry",
    "--features",
];

/// Split off the script argument (a last `.rok` that isn't an option
/// value).
fn script_arg(args: &[String]) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let Some((last, rest)) = args.split_last().filter(|(l, r)| {
        Path::new(l.as_str())
            .extension()
            .is_some_and(|x| x == "rok")
            && r.last()
                .is_none_or(|p| !DEP_VALUE_OPTIONS.contains(&p.as_str()))
    }) else {
        return Ok((None, args.to_vec()));
    };
    let file = PathBuf::from(last);
    if !file.is_file() {
        return Err(format!("`{last}` does not exist"));
    }
    if let Some(dir) = package_dir(&file) {
        return Err(format!(
            "`{last}` belongs to the package in {}, which uses its rok.rokon: run the command \
             there without `{last}`",
            dir.display(),
        ));
    }
    Ok((Some(file), rest.to_vec()))
}

/// Edit `file`'s `//! ```rokon` block with `edit` (`None`: no change) and
/// resolve, restoring the file on failure; an emptied block is removed.
/// Whether it changed.
fn edit_script(
    g: &Global,
    file: &Path,
    edit: impl FnOnce(&str, &str) -> Result<Option<String>, String>,
) -> Result<bool, String> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let old = manifest::script_block(&src).map(|b| b.text);
    let old = old.as_deref().unwrap_or(manifest::EMPTY);
    let label = format!("{} (//! rokon block)", file.display());
    let Some(text) = edit(old, &label)? else {
        return Ok(false);
    };
    let text = canonical_edit(old, &text);
    let m = manifest::parse(&text, &label)?;
    let keep = !(m.deps.is_empty() && m.native.is_empty());
    let new = manifest::set_script_block(&src, keep.then_some(text.as_str()));
    let write = |t: &str| {
        std::fs::write(file, t).map_err(|e| format!("cannot write {}: {e}", file.display()))
    };
    write(&new)?;
    let r = script_target(file, &ambient_env()?)
        .and_then(|t| resolve::resolve(&home(), &t.ctx, &options(g)));
    if let Err(e) = r {
        write(&src).map_err(|e2| format!("{e}\n(and {e2})"))?;
        return Err(e);
    }
    Ok(true)
}

/// `rok add <dep> <file.rok>`; a path dep is made relative to the script.
fn add_to_script(g: &Global, mut dep: Dep, dev: bool, file: &Path) -> Result<i32, String> {
    if dev || dep.optional {
        return Err(
            "a script's block holds only `deps`: no --dev, and no --optional (it has no \
             features)"
                .into(),
        );
    }
    if let Source::Path(p) = &dep.source {
        let abs = rokc_core::real_path(p).map_err(|e| format!("--path {p}: {e}"))?;
        let script = rokc_core::real_path(file).map_err(|e| e.to_string())?;
        let dir = script.parent().unwrap_or_else(|| Path::new("/"));
        let rel = relative(dir, &abs).display().to_string();
        dep.source = Source::Path(rel.replace('\\', "/"));
    }
    pin_newest(g, &mut dep)?;
    edit_script(g, file, |old, label| {
        manifest::add_dep(old, label, "deps", &dep).map(Some)
    })?;
    if !g.quiet {
        term::status("Added", &format!("{} to {}", dep.name, file.display()));
    }
    Ok(0)
}

/// A stone without a requirement: `^` its newest version.
fn pin_newest(g: &Global, dep: &mut Dep) -> Result<(), String> {
    if let Source::Registry { req, registry } = &mut dep.source {
        if req.is_empty() {
            let url = match registry {
                Some(r) => r.clone(),
                None => registry::default_url(&home())?,
            };
            let index = registry::open(&home(), &url, g.offline, g.quiet)?;
            let all = registry::entries(&index, &url, &dep.name)?;
            let star = semver::Req::parse("*")?;
            let (e, _) = registry::best_here(&all, &star);
            let e = e.ok_or_else(|| registry::no_match(&index, &all, &dep.name, "*", &url))?;
            *req = format!("^{}", e.version);
        }
    }
    Ok(())
}

/// `dep` into the package's (`env: None`) or environment's `deps` /
/// `dev_deps`, then resolve.
fn add_dep(g: &Global, mut dep: Dep, dev: bool, env: Option<&str>) -> Result<i32, String> {
    let (path, is_env) = manifest_to_edit(env)?;
    // Path deps: relative in a project (it moves as a whole), absolute in
    // an environment.
    if let Source::Path(p) = &dep.source {
        let abs = rokc_core::real_path(p).map_err(|e| format!("--path {p}: {e}"))?;
        let dir = rokc_core::real_path(path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(|e| e.to_string())?;
        let shown = if is_env { abs } else { relative(&dir, &abs) };
        dep.source = Source::Path(shown.display().to_string().replace('\\', "/"));
    }
    pin_newest(g, &mut dep)?;
    let old = std::fs::read_to_string(&path).ok();
    let section = if dev { "dev_deps" } else { "deps" };
    let text = manifest::add_dep(
        old.as_deref().unwrap_or(manifest::EMPTY),
        &path.display().to_string(),
        section,
        &dep,
    )?;
    let graph = write_and_resolve(g, &path, old.as_deref(), &text)?;
    if !g.quiet {
        if let Some(e) = env {
            term::status(
                "Installed",
                &format!(
                    "{n} into environment `{e}` (for `rok repl` / `rok jupyter`; a script gets \
                     it with `rok add {n} <file.rok>`)",
                    n = dep.name
                ),
            );
        } else {
            // A dev dependency is not in the resolved graph: its pin, or
            // its directory's manifest.
            let pinned = || match &dep.source {
                Source::Registry { req, .. } => req.strip_prefix('^').map(str::to_owned),
                Source::Path(p) => {
                    let dir = path.parent()?.join(p);
                    manifest::load(&manifest::find(&dir)?).ok()?.version
                }
                Source::Git { .. } => None,
            };
            let v = dep_version(&graph, &dep.name)
                .or_else(pinned)
                .map_or_else(String::new, |v| format!(" v{v}"));
            let dev = if dev { " (dev_deps)" } else { "" };
            term::status("Added", &format!("{}{v}{dev}", dep.name));
        }
    }
    Ok(0)
}

fn cmd_remove(g: &Global, args: &[String]) -> Result<i32, String> {
    package_only(g, "remove", "uninstall")?;
    let (script, args) = script_arg(args)?;
    let (native, name) = match args.as_slice() {
        [name] => (false, name),
        [f, name] if f == "--native" => (true, name),
        _ => return Err("usage: rok remove [--native] <name> [<file.rok>]".into()),
    };
    if let Some(file) = script {
        let section = if native { "native" } else { "deps" };
        let removed = edit_script(g, &file, |old, label| {
            manifest::remove_dep(old, label, section, name)
        })?;
        if !removed {
            return Err(format!(
                "`{name}` is not a dependency in the block of {}",
                file.display()
            ));
        }
        if !g.quiet {
            term::status("Removed", &format!("{name} from {}", file.display()));
        }
        return Ok(0);
    }
    let (path, _) = manifest_to_edit(None)?;
    if !remove_from(g, &path, native, name)? {
        return Err(format!(
            "`{name}` is not a dependency in {}",
            path.display()
        ));
    }
    if !g.quiet {
        term::status("Removed", &format!("{name} from {}", path.display()));
    }
    Ok(0)
}

/// Remove `name` from `path`'s deps (with `native`, its `native`) and
/// resolve; whether it was there.
fn remove_from(g: &Global, path: &Path, native: bool, name: &str) -> Result<bool, String> {
    let sections: &[&str] = if native {
        &["native"]
    } else {
        &["deps", "dev_deps"]
    };
    let Ok(old) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let file = path.display().to_string();
    let mut text: Option<String> = None;
    for section in sections {
        let now = text.as_deref().unwrap_or(&old);
        if let Some(t) = manifest::remove_dep(now, &file, section, name)? {
            text = Some(t);
        }
    }
    let Some(text) = text else {
        return Ok(false);
    };
    write_and_resolve(g, path, Some(&old), &text)?;
    Ok(true)
}

/// Manifest of what `rok install` put in `$ROK_HOME/bin`.
fn installs() -> PathBuf {
    home().join("installs.rokon")
}

/// `installs.rokon`'s entries; under `-v`, what a newer rok wrote that
/// this one skipped.
fn read_installs(text: &str, file: &str) -> Result<Vec<Dep>, String> {
    let (deps, newer) = manifest::parse_installs(text, file)?;
    skipped(file, &newer.iter().map(String::as_str).collect::<Vec<_>>());
    Ok(deps)
}

const INSTALLS_HEADER: &str = "// rok install: the executables in bin/ and where they came from\n";

/// The toolchain's executables in `$ROK_HOME/bin`.
const PROXIES: [&str; 3] = ["rok", "rokc", "rokls"];

const INSTALL_USAGE: &str = "usage: rok install <name>[@<req>] [--registry <url>] | [<name>] --path \
                             <dir | file.rok> | [<name>] --git <url> [--rev|--tag|--branch <r>] \
                             [--features <f,..>] [--no-default-features] [--lib | --bin] [--force] | \
                             --native <name>[@<req>] | --list";

/// `rok install`: an executable (package with `src/main.rok`, or a script)
/// built `--release` into `$ROK_HOME/bin`, or a library (`src/lib.rok`
/// only, or `--lib`) into the active environment; a package with both
/// is an error without `--lib` or `--bin`. An executable already
/// installed from the same version or commit and features is kept unless
/// `--force`; path installs always rebuild.
fn cmd_install(g: &Global, args: &[String]) -> Result<i32, String> {
    if args.iter().any(|a| a == "--list") {
        return match args {
            [_] => list_installs(g),
            _ => Err(INSTALL_USAGE.into()),
        };
    }
    let env = selected_env(g)?;
    if args.iter().any(|a| a == "--native") {
        return add_native(g, args, Some(&env), None);
    }
    let lib = args.iter().any(|a| a == "--lib");
    let bin = args.iter().any(|a| a == "--bin");
    if lib && bin {
        return Err(
            "`--lib` and `--bin` go apart: `--lib` installs the library, `--bin` the executable"
                .into(),
        );
    }
    let force = args.iter().any(|a| a == "--force");
    let args: Vec<String> = args
        .iter()
        .filter(|a| *a != "--lib" && *a != "--bin" && *a != "--force")
        .cloned()
        .collect();
    if args.is_empty() {
        return Err(INSTALL_USAGE.into());
    }
    let again = (!force).then(|| format!("rok install {} --force", args.join(" ")));
    let (mut dep, dev) = parse_dep_args(&args)?;
    if dev || dep.optional {
        return Err(
            "`rok install` takes no --dev / --optional: they are for a package's rok.rokon (`rok \
             add`)"
                .into(),
        );
    }
    let named = !dep.name.is_empty();
    if let Source::Path(p) = &dep.source {
        let file = Path::new(p);
        if file.is_file() {
            if lib {
                return Err(format!(
                    "`{p}` is a script: --lib installs a package's library (src/lib.rok)"
                ));
            }
            if !named {
                file.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .clone_into(&mut dep.name);
            }
            manifest::check_name(&dep.name)?;
            let abs = rokc_core::real_path(file).map_err(|e| format!("--path {p}: {e}"))?;
            dep.source = Source::Path(abs.display().to_string().replace('\\', "/"));
            return install_bin(g, &dep, &abs, false, again.as_deref());
        }
    }
    if !named {
        dep.name = match &dep.source {
            Source::Path(p) => std::path::absolute(p)
                .ok()
                .and_then(|a| Some(a.file_name()?.to_str()?.to_owned())),
            Source::Git { url, .. } => url
                .trim_end_matches('/')
                .rsplit(['/', ':', '\\'])
                .next()
                .map(|n| n.trim_end_matches(".git").to_owned()),
            Source::Registry { .. } => None,
        }
        .unwrap_or_default();
    }
    pin_newest(g, &mut dep)?;
    let (dir, exact) = resolve::fetch_package(&home(), &dep, Path::new("."), &options(g))?;
    if !named {
        if let Some(n) = manifest::find(&dir)
            .map(|m| manifest::load(&m))
            .transpose()?
            .and_then(|m| m.name)
        {
            dep.name = n;
        }
    }
    manifest::check_name(&dep.name)?;
    let has_lib = dir.join("src/lib.rok").is_file();
    let has_bin = dir.join("src/main.rok").is_file();
    if has_lib && has_bin && !lib && !bin {
        return Err(format!(
            "`{}` has an executable and a library (src/lib.rok): write `rok install --bin ..` \
             for the executable, or `rok install --lib ..` to put the library into environment \
             `{env}` (for `rok repl` / `rok jupyter`)",
            dep.name
        ));
    }
    if has_bin && !lib {
        if let Source::Path(p) = &dep.source {
            let abs = rokc_core::real_path(p).map_err(|e| format!("--path {p}: {e}"))?;
            dep.source = Source::Path(abs.display().to_string().replace('\\', "/"));
        } else {
            dep.source = exact;
        }
        let copy = !matches!(dep.source, Source::Path(_));
        return install_bin(g, &dep, &dir, copy, again.as_deref());
    }
    if bin {
        return Err(format!(
            "`{}` has no executable (src/main.rok) for --bin{}",
            dep.name,
            if has_lib {
                format!("; without --bin, its library goes into environment `{env}`")
            } else {
                String::new()
            }
        ));
    }
    if !has_lib {
        return Err(if lib {
            format!(
                "`{}` has no library (src/lib.rok) for --lib; without --lib, its executable \
                 (src/main.rok) goes into {}",
                dep.name,
                home().join("bin").display()
            )
        } else {
            format!(
                "`{}` is not a package: no src/lib.rok (a library) or src/main.rok (an \
                 executable)",
                dir.display()
            )
        });
    }
    let (path, _) = manifest_to_edit(Some(&env))?;
    let old = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = dep.name.clone();
    add_dep(g, dep, false, Some(&env))?;
    if !prebuild_env(g, &env)? {
        let added = std::fs::read_to_string(&path).ok();
        write_and_resolve(g, &path, added.as_deref(), &old)?;
        return Err(format!(
            "`{name}` did not build in environment `{env}`; nothing was installed"
        ));
    }
    Ok(0)
}

/// Build `src` `--release` into `$ROK_HOME/bin` (from a scratch copy when
/// `copy`, so a cached checkout isn't written) and record `dep`. `again`:
/// skip when `dep` is already installed (not a path).
fn install_bin(
    g: &Global,
    dep: &Dep,
    src: &Path,
    copy: bool,
    again: Option<&str>,
) -> Result<i32, String> {
    let name = &dep.name;
    if PROXIES.contains(&name.as_str()) {
        return Err(format!(
            "`{name}` is the toolchain's own executable; install it under another name: `rok \
             install <name> ..`"
        ));
    }
    let bin = home().join("bin");
    let exe = bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let record = installs();
    let file = record.display().to_string();
    let old = std::fs::read_to_string(&record)
        .unwrap_or_else(|_| format!("{INSTALLS_HEADER}{}", manifest::EMPTY));
    let prev = read_installs(&old, &file)?
        .into_iter()
        .find(|d| d.name == *name);
    if exe.exists() && prev.is_none() {
        return Err(format!(
            "{} is there and `rok install` did not put it there; remove it first",
            exe.display()
        ));
    }
    if let Some(again) = again {
        if prev.as_ref() == Some(dep) && exe.exists() && !matches!(dep.source, Source::Path(_)) {
            if !g.quiet {
                eprintln!(
                    "`{name}` {} is installed already ({}); to rebuild it, run `{again}`",
                    installed_as(&dep.source),
                    exe.display()
                );
            }
            return Ok(0);
        }
    }
    std::fs::create_dir_all(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let id = std::process::id();
    let work = TempEntry::adopt(std::env::temp_dir().join(format!("rok-install-{id}-{name}")));
    let from = if copy {
        let _ = std::fs::remove_dir_all(work.path());
        toolchain::copy_tree(src, work.path())?;
        work.path()
    } else {
        src
    };
    let tmp = TempEntry::adopt(bin.join(format!(".{name}.{id}{}", std::env::consts::EXE_SUFFIX)));
    let mut c = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    c.args(&g.color);
    for (on, flag) in [
        (g.offline, "--offline"),
        (g.locked, "--locked"),
        (g.quiet, "-q"),
    ] {
        if on {
            c.arg(flag);
        }
    }
    c.arg("build")
        .arg(from)
        .args(["--release", "-o"])
        .arg(tmp.path());
    if !dep.features.is_empty() {
        c.arg(format!("--features={}", dep.features.join(",")));
    }
    if !dep.default_features {
        c.arg("--no-default-features");
    }
    let built = c.status().map_err(|e| format!("cannot run rok: {e}"));
    drop(work);
    let built = built?;
    if built.code() == Some(rokc_core::ICE_EXIT) {
        return Err(format!(
            "`{name}` did not build: internal compiler error; nothing was installed"
        ));
    }
    if !built.success() {
        return Err(format!("`{name}` did not build; nothing was installed"));
    }
    std::fs::rename(tmp.path(), &exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let text = manifest::set_install(&old, &file, dep)?;
    std::fs::write(&record, text).map_err(|e| format!("cannot write {file}: {e}"))?;
    if !g.quiet {
        let new = installed_as(&dep.source);
        let (verb, what) = match prev.map(|p| installed_as(&p.source)) {
            None => ("Installed", format!("{name} {new}")),
            Some(old) if old == new => ("Reinstalled", format!("{name} {new}")),
            Some(old) => match (semver::Version::parse(&old), semver::Version::parse(&new)) {
                (Ok(a), Ok(b)) if a < b => ("Upgraded", format!("{name} from {old} to {new}")),
                (Ok(a), Ok(b)) if a > b => ("Downgraded", format!("{name} from {old} to {new}")),
                _ => ("Replaced", format!("{name} {old} with {new}")),
            },
        };
        term::status(verb, &format!("{what} ({})", exe.display()));
    }
    Ok(0)
}

/// An executable's source as `rok install` shows it.
fn installed_as(source: &Source) -> String {
    match source {
        Source::Registry { req, .. } => req.trim_start_matches('=').to_owned(),
        Source::Git { url, reference } => match reference.pair() {
            Some((_, r)) => format!("{url} at {}", &r[..r.len().min(12)]),
            None => url.clone(),
        },
        Source::Path(p) => p.clone(),
    }
}

/// Prebuild environment `env`'s libraries into its `target/` by building
/// an empty file, as its REPL would. Whether that worked.
fn prebuild_env(g: &Global, env: &str) -> Result<bool, String> {
    let dir = TempEntry::dir("rok-prebuild-").map_err(|e| format!("temporary directory: {e}"))?;
    let probe = dir.path().join("probe.rok");
    let out = probe.with_extension(std::env::consts::EXE_EXTENSION);
    std::fs::write(&probe, "").map_err(|e| format!("{}: {e}", probe.display()))?;
    let args = [
        probe.display().to_string(),
        "-o".to_owned(),
        out.display().to_string(),
    ];
    let built = rokc_command(g, "build", &["-q".to_owned()], &args, Some(env))
        .and_then(|(mut c, _)| c.status().map_err(|e| format!("cannot run rokc: {e}")));
    drop(dir);
    Ok(built?.success())
}

/// `rok install --list`: environment libraries, then installed
/// executables, `<name> <source>` each.
fn list_installs(g: &Global) -> Result<i32, String> {
    let env = selected_env(g)?;
    let libs = load_dir(&env_dir(&env))?.manifest;
    println!("libraries (environment `{env}`):");
    for d in &libs.deps {
        println!("  {} {}", d.name, d.source.to_rok());
    }
    for d in &libs.native {
        println!(
            "  {} native {}",
            d.name,
            rokc_rokon::to_line(&d.source.to_value())
        );
    }
    println!("executables ({}):", home().join("bin").display());
    if let Ok(text) = std::fs::read_to_string(installs()) {
        for d in read_installs(&text, &installs().display().to_string())? {
            println!("  {} {}", d.name, d.source.to_rok());
        }
    }
    Ok(0)
}

/// `rok uninstall [--native] <name>..`: from the active environment, else
/// from installed executables.
fn cmd_uninstall(g: &Global, args: &[String]) -> Result<i32, String> {
    let native = args.first().is_some_and(|a| a == "--native");
    let names = &args[usize::from(native)..];
    if names.is_empty() || names.iter().any(|n| n.starts_with('-')) {
        return Err("usage: rok uninstall [--native] <name>..".into());
    }
    let env = selected_env(g)?;
    let manifest = env_dir(&env).join(MANIFEST);
    for name in names {
        if remove_from(g, &manifest, native, name)? {
            if !g.quiet {
                term::status("Removed", &format!("{name} from environment `{env}`"));
            }
            continue;
        }
        let record = installs();
        let file = record.display().to_string();
        let removed = match std::fs::read_to_string(&record) {
            Ok(old) if !native => manifest::remove_install(&old, &file, name)?,
            _ => None,
        };
        let Some(text) = removed else {
            return Err(format!(
                "`{name}` is not installed: environment `{env}` has no library `{name}`, and \
                 `rok install` put no `{name}` in {} (see `rok install --list`)",
                home().join("bin").display()
            ));
        };
        let exe = home()
            .join("bin")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if exe.exists() {
            std::fs::remove_file(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
        }
        std::fs::write(&record, text).map_err(|e| format!("cannot write {file}: {e}"))?;
        if !g.quiet {
            term::status("Removed", &format!("{}", exe.display()));
        }
    }
    Ok(0)
}

fn cmd_update(g: &Global, names: &[String]) -> Result<i32, String> {
    let in_package = g.env.is_none() && package_dir(Path::new(".")).is_some();
    let t = target_for(g, None)?;
    let manifest = &t.ctx.manifest;
    let has_deps =
        !(manifest.deps.is_empty() && manifest.dev_deps.is_empty() && manifest.native.is_empty());
    // Outside a package, the ambient environment only when it has
    // libraries; else this is a mistake, not an empty update.
    if !in_package && g.env.is_none() && !has_deps {
        return Err(
            "not inside a package (no src/main.rok, src/lib.rok or rok.rokon here or above): \
             make one with `rok init`; for an environment's libraries: `rok update --env <name>`"
                .to_owned(),
        );
    }
    for n in names {
        if !manifest
            .deps
            .iter()
            .chain(&manifest.dev_deps)
            .any(|d| manifest::same(&d.name, n))
            && !manifest.native.iter().any(|d| manifest::same(&d.name, n))
        {
            let lock = t
                .ctx
                .lock_path
                .as_deref()
                .and_then(|p| lock::Lock::read(p).ok().flatten());
            if lock
                .as_ref()
                .is_none_or(|l| l.find(n).is_none() && l.find_native(n).is_none())
            {
                return Err(format!("`{n}` is not a dependency"));
            }
        }
    }
    let read = || {
        t.ctx
            .lock_path
            .as_deref()
            .and_then(|p| lock::Lock::read(p).ok().flatten())
            .unwrap_or_default()
    };
    let before = read();
    let mut opts = options(g);
    opts.update = Some(names.to_vec());
    opts.dev = true;
    resolve::resolve(&home(), &t.ctx, &opts)?;
    if g.quiet {
        return Ok(0);
    }
    let changes = lock_changes(&before, &read());
    for (verb, what) in &changes {
        term::status(verb, what);
    }
    let what = match &t.env {
        Some(e) => format!(" in environment `{e}`"),
        None => String::new(),
    };
    if !has_deps {
        let add = if t.env.is_some() {
            "rok install <name>"
        } else {
            "rok add <name>"
        };
        term::note(&format!(
            "no dependencies{what} to update; add one: `{add}`"
        ));
    } else if changes.is_empty() {
        term::note(&format!("all dependencies{what} are up to date"));
    } else if let Some(e) = &t.env {
        term::status("Updated", &format!("environment `{e}`"));
    }
    Ok(0)
}

/// What `rok update` changed in the lock: (status verb, line) per
/// package, as `json v0.1.0 -> v0.1.1`.
fn lock_changes(old: &lock::Lock, new: &lock::Lock) -> Vec<(&'static str, String)> {
    let short = |c: &str| c.chars().take(7).collect::<String>();
    let label = |p: &lock::Locked| {
        p.version
            .as_ref()
            .map_or_else(|| short(&p.commit), |v| format!("v{v}"))
    };
    let mut out = Vec::new();
    for p in &new.packages {
        match old.find(&p.name) {
            None => out.push(("Locked", format!("{} {}", p.name, label(p)))),
            Some(o) if o.commit == p.commit && o.version == p.version => {}
            Some(o) if label(o) == label(p) => out.push((
                "Updated",
                format!("{} {} -> {}", p.name, short(&o.commit), short(&p.commit)),
            )),
            Some(o) => out.push((
                "Updated",
                format!("{} {} -> {}", p.name, label(o), label(p)),
            )),
        }
    }
    for p in &new.natives {
        match old.find_native(&p.name) {
            None => out.push(("Locked", format!("{} v{}", p.name, p.version))),
            Some(o) if o.version == p.version => {}
            Some(o) => out.push((
                "Updated",
                format!("{} v{} -> v{}", p.name, o.version, p.version),
            )),
        }
    }
    for p in &old.packages {
        if new.find(&p.name).is_none() {
            out.push(("Removed", format!("{} {}", p.name, label(p))));
        }
    }
    for p in &old.natives {
        if new.find_native(&p.name).is_none() {
            out.push(("Removed", format!("{} v{}", p.name, p.version)));
        }
    }
    out
}

/// `rok deps [path] [--dev] [--format json]`: `name=<lib.rok>` per
/// dependency in build order, then per native package a `# native <name>
/// <vers>` line and its rokc arguments (read by the REPL's `:add`).
fn cmd_deps(g: &Global, args: &[String]) -> Result<i32, String> {
    let mut opts = options(g);
    let args = feature_flags(args, &mut opts)?;
    let json = json_format(&args, "deps")?;
    let mut path = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--format" {
            it.next();
        } else if !a.starts_with('-') {
            path = Some(a.as_str());
            break;
        }
    }
    let t = target_for(g, path)?;
    opts.dev = args.iter().any(|a| a == "--dev");
    let graph = resolve::resolve(&home(), &t.ctx, &opts)?;
    if json {
        return deps_json(g, &graph);
    }
    for p in &graph.packages {
        // `name=<lib.rok>` per name, then the features.
        for l in extern_args(p) {
            println!("{}", l.strip_prefix("--extern=").unwrap_or(&l));
        }
    }
    for n in &graph.natives {
        println!("# native {} {}", n.entry.name, n.entry.version);
        for a in native::rokc_args(
            &home(),
            &[n],
            rokc_target::TargetSpec::host(),
            g.offline,
            g.quiet,
        )? {
            println!("{a}");
        }
    }
    Ok(0)
}

/// `rok deps --format json`: a `package` record per dependency in build
/// order, a `native` record per native package, then a `summary`.
fn deps_json(g: &Global, graph: &resolve::Graph) -> Result<i32, String> {
    let record = |ty: &str, fields: Vec<(&str, Json)>| {
        let mut all = vec![
            (
                "schema".to_owned(),
                Json::num(rokc_diagnostic::RECORD_SCHEMA),
            ),
            ("type".to_owned(), Json::str(ty)),
        ];
        all.extend(fields.into_iter().map(|(k, v)| (k.to_owned(), v)));
        println!("{}", Json::Obj(all));
    };
    let strs = |v: &[String]| Json::Arr(v.iter().map(Json::str).collect());
    for p in &graph.packages {
        record(
            "package",
            vec![
                ("name", Json::str(&p.name)),
                (
                    "version",
                    p.version.as_deref().map_or(Json::Null, Json::str),
                ),
                ("source", Json::str(&p.source)),
                ("path", Json::str(p.root.display().to_string())),
                ("features", strs(&p.features)),
            ],
        );
    }
    for n in &graph.natives {
        let args = native::rokc_args(
            &home(),
            &[n],
            rokc_target::TargetSpec::host(),
            g.offline,
            g.quiet,
        )?;
        record(
            "native",
            vec![
                ("name", Json::str(&n.entry.name)),
                ("version", Json::str(n.entry.version.to_string())),
                ("args", strs(&args)),
            ],
        );
    }
    let count = |n: usize| Json::num(u32::try_from(n).unwrap_or(u32::MAX));
    record(
        "summary",
        vec![
            ("packages", count(graph.packages.len())),
            ("natives", count(graph.natives.len())),
        ],
    );
    Ok(0)
}

/// `rok search [--native] <query>`: `name newest - description` for each
/// name containing the query (any case).
fn cmd_search(g: &Global, args: &[String]) -> Result<i32, String> {
    let (q, native) = match args {
        [q] if !q.starts_with('-') => (q, false),
        [f, q] if f == "--native" => (q, true),
        _ => return Err("usage: rok search [--native] <query>".into()),
    };
    let url = registry::default_url(&home())?;
    let index = registry::open(&home(), &url, g.offline, g.quiet)?;
    // `-` and `_` are one; file names are lowercase.
    let q = q.to_ascii_lowercase().replace('_', "-");
    let mut names: Vec<String> = registry::names(&index, &url, native)?
        .into_iter()
        .filter(|n| n.replace('_', "-").contains(&q))
        .collect();
    names.sort();
    let star = semver::Req::parse("*")?;
    // Descriptions live in the registry API, not the index.
    let api = if g.offline {
        None
    } else {
        registry::api(&home(), &url, true).ok()
    };
    for n in names {
        let newest = if native {
            native::best(&native::entries(&index, &url, &n)?, &star)
                .map(|e| (e.name.clone(), e.version.clone()))
        } else {
            registry::best(&registry::entries(&index, &url, &n)?, &star)
                .map(|e| (e.name.clone(), e.version.clone()))
        };
        if let Some((name, version)) = newest {
            let v = version.to_string();
            match api
                .as_deref()
                .and_then(|a| registry::description(a, &name, &v, native))
            {
                Some(d) => println!("{name} {v} - {d}"),
                None => println!("{name} {v}"),
            }
        }
    }
    Ok(0)
}

/// The current package's directory and manifest.
fn this_package() -> Result<(PathBuf, Manifest), String> {
    let dir = package_dir(Path::new("."))
        .filter(|d| manifest::find(d).is_some())
        .ok_or("not inside a package with a rok.rokon")?;
    let m = manifest::load(&dir.join(MANIFEST))?;
    Ok((dir, m))
}

/// Options with a value (`--f v` / `--f=v`), switches given, positionals.
type Flags = (Vec<(String, String)>, Vec<String>, Vec<String>);

/// `args` split by `valued` options and `switches`; others are errors.
fn flags(args: &[String], valued: &[&str], switches: &[&str]) -> Result<Flags, String> {
    let (mut vals, mut sw, mut pos) = (Vec::new(), Vec::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (f, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (a.as_str(), None),
        };
        if valued.contains(&f) {
            let v = inline
                .or_else(|| it.next().cloned())
                .ok_or_else(|| format!("`{f}` needs a value"))?;
            vals.push((f.to_owned(), v));
        } else if switches.contains(&f) {
            sw.push(f.to_owned());
        } else if f.starts_with('-') {
            return Err(format!("unknown option `{f}`"));
        } else {
            pos.push(a.clone());
        }
    }
    Ok((vals, sw, pos))
}

fn val<'a>(vals: &'a [(String, String)], f: &str) -> Option<&'a str> {
    vals.iter()
        .rev()
        .find(|(k, _)| k == f)
        .map(|(_, v)| v.as_str())
}

/// The index `--registry` names, else rokstones.io's.
fn index_url(registry: Option<&str>) -> Result<String, String> {
    match registry {
        Some(u) => Ok(u.to_owned()),
        None => registry::default_url(&home()),
    }
}

/// The registry API of the index `--registry` names (else
/// rokstones.io's), and the token for it.
fn api_token(g: &Global, registry: Option<&str>) -> Result<(String, String), String> {
    let home = home();
    let url = index_url(registry)?;
    let login = match registry {
        Some(u) if !registry::same_index(u, registry::DEFAULT) => {
            format!("rok login --registry {u}")
        }
        _ => "rok login".to_owned(),
    };
    let token = registry::token(&home, &url, &login)?;
    let api = registry::api(&home, &url, g.quiet)?;
    Ok((api, token))
}

/// `--index-dir <dir>`; a stray `--index <dir>` says what to write.
fn index_dir<'a>(vals: &'a [(String, String)], cmd: &str) -> Result<Option<&'a str>, String> {
    if let Some(d) = val(vals, "--index") {
        return Err(format!(
            "`rok {cmd}` has no `--index`: write `--index-dir {d}`"
        ));
    }
    Ok(val(vals, "--index-dir"))
}

/// `rok publish [--native <file>] [--index-dir <dir> | --registry <url>]
/// [--dry-run]`: print the index line and commit it to `--index-dir` or
/// publish it through the API; `--dry-run` only checks.
fn cmd_publish(g: &Global, args: &[String]) -> Result<i32, String> {
    let (vals, sw, pos) = flags(
        args,
        &["--index", "--index-dir", "--registry", "--native"],
        &["--commit", "--dry-run"],
    )?;
    if let Some(p) = pos.first() {
        return Err(format!("unexpected argument `{p}`"));
    }
    if sw.iter().any(|s| s == "--commit") {
        return Err(
            "`rok publish` has no `--commit`: a bare `rok publish` publishes, `rok publish \
             --dry-run` only checks"
                .into(),
        );
    }
    let write = !sw.iter().any(|s| s == "--dry-run");
    let index = index_dir(&vals, "publish")?.map(Path::new);
    let reg = val(&vals, "--registry");
    if index.is_some() && reg.is_some() {
        return Err(
            "`--index-dir` and `--registry` go apart: `--index-dir <dir>` commits to a checkout \
             of the index, `--registry <url>` publishes through that index's API"
                .into(),
        );
    }
    let remote = match index {
        None if write => Some(api_token(g, reg)?),
        _ => None,
    };
    let (line, meta, mut name, mut vers, rel, what) = if let Some(file) = val(&vals, "--native") {
        let e = native::publish(&home(), Path::new(file), index, write)?.0;
        let rel = native::path(&e.name);
        let meta = registry::native_meta(&e);
        (
            e.to_line(),
            meta,
            e.name,
            e.version.to_string(),
            rel,
            "native ",
        )
    } else {
        let (dir, m) = this_package()?;
        let e = registry::entry_for(&dir, &m)?;
        // Checked on a dry run too.
        let meta = registry::meta(&dir, &m)?;
        if let Some(i) = index {
            registry::publish(i, &e, write)?;
        }
        let rel = registry::path(&e.name);
        (e.to_line(), meta, e.name, e.version.to_string(), rel, "")
    };
    let to = match (index, remote) {
        (Some(i), _) => i.display().to_string(),
        (None, Some((api, token))) => {
            let p = registry::publish_api(&api, &token, &line, meta)?;
            (name, vers) = (p.name, p.version);
            for w in &p.warnings {
                warn_once(&format!("{api}: {w}"));
            }
            api
        }
        (None, None) => index_url(reg)?,
    };
    println!("{line}");
    if !g.quiet {
        if write {
            term::status("Published", &format!("{what}{name} {vers} to {to}"));
        } else if index.is_some() {
            eprintln!(
                "{} dry run; `rok publish` commits it to {to} as {rel} (not pushed)",
                term::label(Stream::Stderr, Role::Note)
            );
        } else {
            eprintln!(
                "{} dry run; `rok publish` sends it to the registry API of {to} (needs `rok \
                 login`), which adds it as {rel}",
                term::label(Stream::Stderr, Role::Note)
            );
        }
    }
    Ok(0)
}

/// The most bytes of a yank's `--message`.
const MAX_YANK_MESSAGE: usize = 512;

/// `rok yank [--native] <name>@<version> [--undo] [--message <why>]
/// [--index-dir <dir> | --registry <url>]`: into the `--index-dir`
/// checkout, else via the API.
fn cmd_yank(g: &Global, args: &[String]) -> Result<i32, String> {
    let (vals, sw, pos) = flags(
        args,
        &[
            "--vers",
            "--index",
            "--index-dir",
            "--registry",
            "--message",
        ],
        &["--undo", "--native"],
    )?;
    let usage = "usage: rok yank [--native] <name>@<version> [--undo] [--message <why>] \
                 [--index-dir <dir> | --registry <url>]";
    let native = sw.iter().any(|s| s == "--native");
    if let Some(v) = val(&vals, "--vers") {
        let n = match pos.as_slice() {
            [n] if !n.contains('@') => n.as_str(),
            _ => "<name>",
        };
        return Err(format!(
            "`rok yank` has no `--vers`: write `rok yank {n}@{v}`"
        ));
    }
    let (name, vers) = match pos.as_slice() {
        [at] => match at.split_once('@') {
            Some((n, v)) if !n.is_empty() && !v.is_empty() => (n.to_owned(), v.to_owned()),
            _ => {
                // A bare version is likely this package's.
                let n = at.trim_end_matches('@');
                let fix = if n.starts_with(|c: char| c.is_ascii_digit()) {
                    format!("<name>@{n}")
                } else {
                    format!("{n}@1.2.0")
                };
                return Err(format!(
                    "`{at}`: give the version as `<name>@<version>`, e.g. `rok yank {fix}`\n{usage}"
                ));
            }
        },
        _ => return Err(usage.into()),
    };
    let undo = sw.iter().any(|s| s == "--undo");
    let message = val(&vals, "--message").filter(|m| !m.is_empty());
    if let Some(m) = message {
        if undo {
            return Err(
                "`--message` says why a version is yanked; `--undo` takes none: drop one of them"
                    .into(),
            );
        }
        if m.len() > MAX_YANK_MESSAGE {
            return Err(format!(
                "`--message` is {} bytes; keep it to {MAX_YANK_MESSAGE}",
                m.len()
            ));
        }
    }
    let index = index_dir(&vals, "yank")?;
    let reg = val(&vals, "--registry");
    if index.is_some() && reg.is_some() {
        return Err(
            "`--index-dir` and `--registry` go apart: `--index-dir <dir>` commits to a checkout \
             of the index, `--registry <url>` goes through that index's API"
                .into(),
        );
    }
    let to = if let Some(i) = index {
        if message.is_some() {
            return Err(
                "`--message` goes to the registry API, which keeps it; an `--index-dir` \
                 checkout has no place for it: drop `--message`"
                    .into(),
            );
        }
        registry::yank(Path::new(i), &name, &vers, !undo, native)?;
        i.to_owned()
    } else {
        let (api, token) = api_token(g, reg)?;
        registry::yank_api(&api, &token, &name, &vers, undo, native, message)?;
        api
    };
    if !g.quiet {
        let verb = if undo { "Unyanked" } else { "Yanked" };
        eprintln!("   {verb} {name} {vers} in {to}");
    }
    Ok(0)
}

/// `rok login [--registry <url>] [<token>]`: store a token whose account
/// may publish, for that index (default: rokstones.io's).
fn cmd_login(g: &Global, args: &[String]) -> Result<i32, String> {
    let (vals, _, pos) = flags(args, &["--registry"], &[])?;
    if pos.len() > 1 {
        return Err("usage: rok login [--registry <url>] [<token>]".into());
    }
    let home = home();
    let default = registry::default_url(&home)?;
    let url = val(&vals, "--registry").unwrap_or(&default);
    let api = registry::api(&home, url, g.quiet)?;
    let token = match pos.first() {
        Some(t) => t.clone(),
        None => read_secret(&format!(
            "a token from {api}/me (not shown; Ctrl-C to cancel): "
        ))?,
    };
    registry::check_token(&token)?;
    let (login, publish) = registry::me(&api, &token)?;
    if !publish {
        return Err(
            "this account cannot publish yet: publishing is open to rokstones.io maintainers only"
                .into(),
        );
    }
    let file = registry::save_token(&home, url, &token)?;
    if !g.quiet {
        term::status(
            "Logged in",
            &format!("as {login} at {api} (token in {})", file.display()),
        );
    }
    Ok(0)
}

/// `rok logout [--registry <url> | --all]`: remove `rok login`'s token
/// for that index (default: rokstones.io's), or every one.
fn cmd_logout(g: &Global, args: &[String]) -> Result<i32, String> {
    let (vals, sw, pos) = flags(args, &["--registry"], &["--all"])?;
    if let Some(a) = pos.first() {
        return Err(format!("unexpected argument `{a}`"));
    }
    let all = sw.iter().any(|s| s == "--all");
    let reg = val(&vals, "--registry");
    if all && reg.is_some() {
        return Err(
            "`--all` logs out of every index; give `--all` or `--registry <url>`, not both".into(),
        );
    }
    let home = home();
    let had = if all {
        registry::logout(&home, None)?
    } else {
        let default = registry::default_url(&home)?;
        registry::logout(&home, Some(reg.unwrap_or(&default)))?
    };
    if !g.quiet {
        if had {
            term::status("Logged out", "");
        } else {
            eprintln!("{} not logged in", term::label(Stream::Stderr, Role::Note));
        }
    }
    Ok(0)
}

/// A trimmed stdin line, unechoed on a terminal (echo returns on Ctrl-C).
fn read_secret(prompt: &str) -> Result<String, String> {
    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal();
    let hidden = tty && {
        eprint!("{prompt}");
        echo::off()
    };
    let mut line = String::new();
    let r = std::io::stdin().read_line(&mut line);
    if hidden {
        echo::on();
    }
    if tty {
        eprintln!();
    }
    r.map_err(|e| format!("cannot read stdin: {e}"))?;
    Ok(line.trim().to_owned())
}

/// Turn terminal echo off; restore it on `on` or Ctrl-C (which then ends
/// the process as usual).
#[cfg(unix)]
mod echo {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // `struct termios` starts with `c_iflag`, `c_oflag`, `c_cflag`,
    // `c_lflag` (`tcflag_t`); the rest is copied as is.
    #[cfg(target_vendor = "apple")]
    type Flag = u64;
    #[cfg(not(target_vendor = "apple"))]
    type Flag = u32;
    const LFLAG: usize = 3 * size_of::<Flag>();
    const ECHO: Flag = 8;
    const SIGINT: i32 = 2;
    const TCSANOW: i32 = 0;

    /// The saved `termios`, larger than any.
    static mut SAVED: [u64; 32] = [0; 32];
    static HANDLER: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" {
        fn tcgetattr(fd: i32, t: *mut u8) -> i32;
        fn tcsetattr(fd: i32, when: i32, t: *const u8) -> i32;
        fn signal(sig: i32, handler: usize) -> usize;
        fn raise(sig: i32) -> i32;
    }

    // Async-signal-safe calls only.
    extern "C" fn interrupted(_: i32) {
        // SAFETY: `SAVED` is written before this handler is installed.
        unsafe {
            tcsetattr(0, TCSANOW, (&raw const SAVED).cast());
            signal(SIGINT, 0);
            raise(SIGINT);
        }
    }

    pub fn off() -> bool {
        // SAFETY: `SAVED` is only touched here, before the handler is
        // installed, and by `on` / the handler afterwards.
        unsafe {
            if tcgetattr(0, (&raw mut SAVED).cast()) != 0 {
                return false;
            }
            let mut t = SAVED;
            let lflag = (&raw mut t).cast::<u8>().add(LFLAG).cast::<Flag>();
            lflag.write_unaligned(lflag.read_unaligned() & !ECHO);
            let h = interrupted as extern "C" fn(i32) as usize;
            HANDLER.store(signal(SIGINT, h), Ordering::Relaxed);
            tcsetattr(0, TCSANOW, (&raw const t).cast());
        }
        true
    }

    pub fn on() {
        // SAFETY: as in `off`.
        unsafe {
            tcsetattr(0, TCSANOW, (&raw const SAVED).cast());
            signal(SIGINT, HANDLER.load(Ordering::Relaxed));
        }
    }
}

#[cfg(windows)]
mod echo {
    use std::sync::atomic::{AtomicU32, Ordering};

    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> isize;
        fn GetConsoleMode(h: isize, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: isize, mode: u32) -> i32;
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
    const ENABLE_ECHO_INPUT: u32 = 4;
    static SAVED: AtomicU32 = AtomicU32::new(0);

    fn restore() {
        // SAFETY: the console input handle.
        unsafe {
            SetConsoleMode(
                GetStdHandle(STD_INPUT_HANDLE),
                SAVED.load(Ordering::Relaxed),
            )
        };
    }

    /// Restore, then let the next handler (the default one) end the
    /// process.
    extern "system" fn interrupted(_: u32) -> i32 {
        restore();
        0
    }

    pub fn off() -> bool {
        // SAFETY: the console input handle and a local out-parameter.
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            let mut m = 0;
            if GetConsoleMode(h, &mut m) == 0 {
                return false;
            }
            SAVED.store(m, Ordering::Relaxed);
            SetConsoleCtrlHandler(Some(interrupted), 1);
            SetConsoleMode(h, m & !ENABLE_ECHO_INPUT);
        }
        true
    }

    pub fn on() {
        restore();
        // SAFETY: removes the handler `off` added.
        unsafe { SetConsoleCtrlHandler(Some(interrupted), 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_updates_keep_other_fields() {
        let home = rokc_core::tmp::test_dir("config-fields");
        assert_eq!(config(&home, "toolchain").unwrap(), None);
        set_config(&home, "registry", "https://example.test/index").unwrap();
        set_config(&home, "toolchain", "0.1.1").unwrap();
        assert_eq!(
            config(&home, "toolchain").unwrap().as_deref(),
            Some("0.1.1")
        );
        assert_eq!(
            config(&home, "registry").unwrap().as_deref(),
            Some("https://example.test/index")
        );
    }

    #[test]
    fn config_read_errors_preserve_existing_bytes() {
        let home = rokc_core::tmp::test_dir("config-read-error");
        let path = home.join("config.rokon");
        let bytes = b"Config { registry: \"index\" }\n\xff";
        std::fs::write(&path, bytes).unwrap();
        let error = config(&home, "toolchain").unwrap_err();
        assert!(error.contains("cannot read"));
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("help: fix its permissions or save it as UTF-8"));
        assert_eq!(set_config(&home, "toolchain", "0.1.1").unwrap_err(), error);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(config(&home, "toolchain").is_err());
        assert!(set_config(&home, "toolchain", "0.1.1").is_err());
        assert!(path.is_dir());
    }
}
