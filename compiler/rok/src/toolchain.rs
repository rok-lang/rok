//! Toolchains (spec 9.10.6, 9.2.19): self-contained
//! `$ROK_HOME/toolchains/<version>-<triple>/`. `$ROK_HOME/bin/{rok,rokc,
//! rokls}` are proxies that run the active toolchain's tool of the same
//! name, picked by `+<version>`, else `ROK_TOOLCHAIN`, else the nearest
//! `rok.rokon`'s `toolchain`, else `rok toolchain default`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use rokc_core::term::{self, Role, Stream};

use crate::{VERSION, home, manifest};

/// The triple this `rok` runs on.
pub const HOST: &str = env!("ROK_HOST");

const TOOLS: [&str; 3] = ["rok", "rokc", "rokls"];

fn exe_name(tool: &str) -> String {
    format!("{tool}{}", std::env::consts::EXE_SUFFIX)
}

/// `pin` (`"0.3"`) accepts `version` (`"0.3"`, `"0.3.1"`).
fn matches(pin: &str, version: &str) -> bool {
    version == pin
        || version
            .strip_prefix(pin)
            .is_some_and(|r| r.starts_with('.'))
}

fn root(home: &Path) -> PathBuf {
    home.join("toolchains")
}

/// Installed toolchains for this host, `(dir name, version)`, oldest first.
fn installed(home: &Path) -> Vec<(String, String)> {
    let suffix = format!("-{HOST}");
    let mut v: Vec<(String, String)> = std::fs::read_dir(root(home))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter_map(|n| Some((n.clone(), n.strip_suffix(&suffix)?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    // Semver order: 0.10 after 0.9, 0.1.0-rc.1 before 0.1.0.
    v.sort_by_key(|(_, ver)| crate::semver::Version::parse(ver).ok());
    v
}

/// The versions of the installed toolchains, oldest first.
pub fn versions(home: &Path) -> Vec<String> {
    installed(home).into_iter().map(|(_, v)| v).collect()
}

/// The newest installed toolchain matching `pin` (version prefix or dir
/// name).
fn find(home: &Path, pin: &str) -> Option<PathBuf> {
    installed(home)
        .into_iter()
        .rev()
        .find(|(name, ver)| name == pin || matches(pin, ver))
        .map(|(name, _)| root(home).join(name))
}

fn not_installed(pin: &str, why: &str) -> String {
    format!(
        "toolchain {pin} ({why}) is not installed; install it with `rok toolchain install {pin} \
         --from <dir | .tar.gz | .zip | url>`"
    )
}

/// The nearest `rok.rokon` and its `toolchain` (unparsable manifests pin
/// nothing).
fn manifest_pin() -> Option<(PathBuf, String)> {
    let cwd = std::env::current_dir().ok()?;
    let path = cwd.ancestors().find_map(manifest::find)?;
    let pin = manifest::toolchain_pin(&std::fs::read_to_string(&path).ok()?)?;
    Some((path, pin))
}

/// `toolchain` of `$ROK_HOME/config.rokon`.
fn default_pin(home: &Path) -> Result<Option<String>, String> {
    crate::config(home, "toolchain")
}

/// `+<version>`, else `ROK_TOOLCHAIN`, else the manifest's, else default.
fn active(home: &Path, plus: Option<&str>) -> Result<PathBuf, String> {
    chosen(home, plus).map(|(dir, _)| dir)
}

/// `active`'s toolchain and why it is the one.
fn chosen(home: &Path, plus: Option<&str>) -> Result<(PathBuf, String), String> {
    let env = std::env::var("ROK_TOOLCHAIN")
        .ok()
        .filter(|t| !t.is_empty());
    let (pin, why) = if let Some(p) = plus {
        (p.to_owned(), "`+` on the command line".to_owned())
    } else if let Some(p) = env {
        (p, "ROK_TOOLCHAIN".to_owned())
    } else if let Some((path, p)) = manifest_pin() {
        (p, format!("pinned by {}", path.display()))
    } else if let Some(p) = default_pin(home)? {
        (p, "the default".to_owned())
    } else {
        return Err(
            "no default toolchain; install one (`rok toolchain install`) or choose one with \
             `rok toolchain default <version>`"
                .to_owned(),
        );
    };
    match find(home, &pin) {
        Some(dir) => Ok((dir, why)),
        None => Err(not_installed(&pin, &why)),
    }
}

/// The first `tool` on `PATH`, as the shell finds it (on Windows, with
/// each `PATHEXT` extension).
fn first_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
        exts.split(';')
            .filter(|e| !e.is_empty())
            .map(|e| format!("{tool}{}", e.to_ascii_lowercase()))
            .collect()
    } else {
        vec![tool.to_owned()]
    };
    std::env::split_paths(&path)
        .filter(|d| !d.as_os_str().is_empty())
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| runnable(p))
}

#[cfg(unix)]
fn runnable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(p: &Path) -> bool {
    p.is_file()
}

/// Warn when the shell's `rok` is not `home`'s proxy: an update or a new
/// default would not take. Not with `-q`, as the installers run it.
fn path_warning(home: &Path, quiet: bool) {
    if quiet {
        return;
    }
    let ours = home.join("bin").join(exe_name("rok"));
    let Some(first) = first_on_path("rok") else {
        return;
    };
    let real = |p: &Path| rokc_core::real_path(p).unwrap_or_else(|_| p.to_path_buf());
    if real(&first) == real(&ours) {
        return;
    }
    let version = version_of(&first).map_or_else(String::new, |v| format!(" (rok {v})"));
    let bin = shown(&home.join("bin"));
    eprintln!(
        "rok: {} your shell runs {}{version} before {}; put {bin} first on PATH or remove the \
         other one",
        term::label(Stream::Stderr, Role::Warning),
        first.display(),
        shown(&ours),
    );
}

/// `<exe> --version`'s version, given a few seconds.
fn version_of(exe: &Path) -> Option<String> {
    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < std::time::Duration::from_secs(3) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    // `rok 0.1.4 (..)`.
    let mut words = out.lines().next()?.split_whitespace();
    (words.next() == Some("rok")).then(|| words.next().map(str::to_owned))?
}

/// `rok --version -v`: this binary, the toolchain it resolves and why,
/// the `rokc` it runs, and the host.
pub fn describe(plus: Option<&str>) -> String {
    let home = home();
    let toolchain = match chosen(&home, plus) {
        Ok((dir, why)) => format!(
            "{} ({why})",
            dir.file_name().unwrap_or_default().to_string_lossy()
        ),
        Err(_) => format!(
            "none (this rok is {VERSION}, not from {})",
            shown(&root(&home))
        ),
    };
    let pin = manifest_pin().map(|(_, p)| p);
    let rokc = rokc_for(plus, pin.as_deref()).map_or_else(|e| e, |p| p.display().to_string());
    let binary = rokc_core::current_exe()
        .or_else(|| std::env::current_exe().ok())
        .map_or_else(|| "unknown".to_owned(), |p| p.display().to_string());
    let rows = [
        ("binary", binary),
        ("toolchain", toolchain),
        ("rokc", rokc),
        ("host", HOST.to_owned()),
    ]
    .map(|(k, v)| (k.to_owned(), v));
    term::list(&rows, None, "")
}

/// If this executable is a proxy (`$ROK_HOME/bin/<tool>` beside
/// `toolchains/`), run the active toolchain's `<tool>`; the exit code.
/// `None` for `rok toolchain ..`, which the proxy handles itself.
pub fn proxy(args: &[OsString]) -> Option<i32> {
    let exe = rokc_core::current_exe()?;
    let h = exe.parent()?.parent()?;
    if !h.join("toolchains").is_dir() {
        return None;
    }
    let tool = exe.file_stem()?.to_str()?;
    let tool = TOOLS.into_iter().find(|t| *t == tool)?;
    let (plus, rest) = match args.first().and_then(|a| a.to_str()) {
        Some(p) if p.len() > 1 && p.starts_with('+') => (Some(&p[1..]), &args[1..]),
        _ => (None, args),
    };
    let command = rest
        .iter()
        .find(|a| !a.to_str().is_some_and(|a| a.starts_with('-')));
    if tool == "rok" && plus.is_none() && command.is_some_and(|c| c == "toolchain") {
        return None;
    }
    let dir = match active(h, plus) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{tool}: {} {e}", term::label(Stream::Stderr, Role::Error));
            return Some(1);
        }
    };
    let target = dir.join("bin").join(exe_name(tool));
    let mut c = Command::new(&target);
    c.args(rest).env("ROK_HOME", h);
    if let Some(p) = plus {
        // The toolchain's `rok` runs the same toolchain's `rokc`.
        c.env("ROK_TOOLCHAIN", p);
    }
    Some(crate::exec(c, &target))
}

/// This `rok`'s own `rokc`, unless the active pin names another installed
/// version.
pub fn rokc_for(plus: Option<&str>, pin: Option<&str>) -> Result<PathBuf, String> {
    let env = std::env::var("ROK_TOOLCHAIN")
        .ok()
        .filter(|t| !t.is_empty());
    let chosen = plus
        .map(|p| (p.to_owned(), "`+` on the command line"))
        .or_else(|| env.map(|p| (p, "ROK_TOOLCHAIN")))
        .or_else(|| pin.map(|p| (p.to_owned(), "rok.rokon")));
    if let Some((p, why)) = chosen.filter(|(p, _)| !matches(p, VERSION)) {
        let dir = find(&home(), &p).ok_or_else(|| not_installed(&p, why))?;
        return Ok(dir.join("bin").join(exe_name("rokc")));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let rokc = exe.with_file_name(exe_name("rokc"));
    if !rokc.is_file() {
        return Err(format!("no `rokc` next to {}", exe.display()));
    }
    Ok(rokc)
}

pub fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for e in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            // `copy` keeps the permission bits (executables stay so).
            std::fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", src.display()))?;
        }
    }
    Ok(())
}

fn run(c: &mut Command, what: &str) -> Result<String, String> {
    let out = c.output().map_err(|e| format!("cannot run {what}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what} failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Check `archive` against `sum` (hex digest, optionally the file name).
fn check_sum(archive: &Path, sum: &Path, what: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(sum).map_err(|e| format!("{}: {e}", sum.display()))?;
    let want = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{}: not a SHA-256 digest", sum.display()));
    }
    let bytes = std::fs::read(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let got = rokc_core::hash::hex(&rokc_core::hash::sha256(&bytes));
    if got != want {
        return Err(format!(
            "{what} has SHA-256 {got}, but its .sha256 says {want} (a damaged or altered download)"
        ));
    }
    Ok(())
}

/// Unpack `from` (a directory, `.tar[.gz]`, `.zip`, or a URL of one) into
/// `tmp`; returns the toolchain's top directory. Downloads must match the
/// `.sha256` beside them; local archives are checked when one is there.
fn unpack(from: &str, tmp: &Path) -> Result<PathBuf, String> {
    let mut archive = PathBuf::from(from);
    let curl = |url: &str, to: &Path| {
        run(
            Command::new("curl")
                .args(["-fsSL", "--proto", "=https,http", "-o"])
                .arg(to)
                .arg(url),
            &format!("curl {url}"),
        )
    };
    if from.starts_with("https://") || from.starts_with("http://") {
        // `tar` tells the format by the content.
        archive = tmp.join("download");
        let sum = tmp.join("download.sha256");
        curl(from, &archive)?;
        curl(&format!("{from}.sha256"), &sum)?;
        check_sum(&archive, &sum, from)?;
    } else {
        let mut sum = archive.clone().into_os_string();
        sum.push(".sha256");
        let sum = PathBuf::from(sum);
        if archive.is_file() && sum.is_file() {
            check_sum(&archive, &sum, from)?;
        }
    }
    let files = tmp.join("files");
    if archive.is_dir() {
        copy_tree(&archive, &files)?;
    } else if archive.is_file() {
        std::fs::create_dir_all(&files).map_err(|e| e.to_string())?;
        run(
            Command::new("tar")
                .arg("-xf")
                .arg(&archive)
                .arg("-C")
                .arg(&files),
            &format!("tar -xf {}", archive.display()),
        )?;
    } else {
        return Err(format!("{from} does not exist"));
    }
    // An archive holding one top directory: that directory.
    if !files.join("bin").is_dir() {
        let entries: Vec<PathBuf> = std::fs::read_dir(&files)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .collect();
        if let [only] = entries.as_slice() {
            return Ok(only.clone());
        }
    }
    Ok(files)
}

/// Check `top` is complete and is `version` for this host; its full
/// version.
fn verify(top: &Path, version: &str) -> Result<String, String> {
    let rokc = top.join("bin").join(exe_name("rokc"));
    if !rokc.is_file() {
        return Err("not a rok toolchain: no bin/rokc".to_owned());
    }
    for need in [
        format!("bin/{}", exe_name("rok")),
        format!("lib/{}", rokc_target::TargetSpec::host().runtime_lib()),
        "lib/rok/core/src/lib.rok".to_owned(),
    ] {
        if !top.join(&need).exists() {
            return Err(format!("incomplete toolchain: no {need}"));
        }
    }
    let out = run(
        Command::new(&rokc).args(["--version", "--verbose"]),
        &rokc.display().to_string(),
    )?;
    let full = out
        .lines()
        .find_map(|l| l.strip_prefix("rokc "))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| format!("unexpected `rokc --version` output: {out:?}"))?;
    if !matches(version, full) {
        return Err(format!(
            "the toolchain is rokc {full}, not {version} (its `bin/rokc --version`)"
        ));
    }
    if let Some(h) = out.lines().find_map(|l| l.strip_prefix("host: ")) {
        if h.trim() != HOST {
            return Err(format!(
                "the toolchain is built for {}, but this machine is {HOST}",
                h.trim()
            ));
        }
    }
    Ok(full.to_owned())
}

/// A scratch directory under `toolchains/`, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Unpack and verify `from` in scratch: (scratch, top dir, full version).
fn stage(home: &Path, version: &str, from: &str) -> Result<(Scratch, PathBuf, String), String> {
    std::fs::create_dir_all(root(home)).map_err(|e| e.to_string())?;
    let tmp = Scratch(root(home).join(format!(".tmp-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&tmp.0);
    std::fs::create_dir_all(&tmp.0).map_err(|e| format!("{}: {e}", tmp.0.display()))?;
    let top = unpack(from, &tmp.0)?;
    let full = verify(&top, version)?;
    Ok((tmp, top, full))
}

/// Move `top` to `toolchains/<full>-<host>`, moving an existing one aside
/// into `tmp` and back on failure; its name.
fn place(home: &Path, tmp: &Scratch, top: &Path, full: &str) -> Result<String, String> {
    let name = format!("{full}-{HOST}");
    let dest = root(home).join(&name);
    let old = tmp.0.join("old");
    let had = dest.exists();
    if had {
        std::fs::rename(&dest, &old).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    if let Err(e) = std::fs::rename(top, &dest) {
        if had {
            let _ = std::fs::rename(&old, &dest);
        }
        return Err(format!("{}: {e}", dest.display()));
    }
    Ok(name)
}

fn install(home: &Path, version: &str, from: &str) -> Result<String, String> {
    let (tmp, top, full) = stage(home, version, from)?;
    place(home, &tmp, &top, &full)
}

/// `$ROK_HOME/bin/{rok,rokc,rokls}`: copies of `toolchain`'s `rok`.
fn write_proxies(home: &Path, toolchain: &Path) -> Result<(), String> {
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let src = toolchain.join("bin").join(exe_name("rok"));
    for t in TOOLS {
        let dst = bin.join(exe_name(t));
        // Copy beside, then rename: a running proxy keeps its file.
        let tmp = bin.join(format!(".{t}.tmp"));
        std::fs::copy(&src, &tmp).map_err(|e| format!("{}: {e}", src.display()))?;
        if cfg!(windows) {
            set_aside(&bin, &dst);
        }
        std::fs::rename(&tmp, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    }
    Ok(())
}

/// Windows can rename a running executable but not replace it: move `exe`
/// to `bin/.old-<n>` and delete older ones no longer running.
fn set_aside(bin: &Path, exe: &Path) {
    for e in std::fs::read_dir(bin).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with(".old-") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    if exe.exists() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let _ = std::fs::rename(exe, bin.join(format!(".old-{nanos:x}")));
    }
}

fn set_default(home: &Path, pin: &str) -> Result<PathBuf, String> {
    let dir = find(home, pin).ok_or_else(|| not_installed(pin, "the default"))?;
    crate::set_config(home, "toolchain", pin)?;
    write_proxies(home, &dir)?;
    Ok(dir)
}

/// The completion scripts install.sh and install.ps1 write: file, shell
/// and the tools whose scripts it holds.
fn completion_files(home: &Path) -> Vec<(PathBuf, &'static str, &'static [&'static str])> {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let user = rokc_core::user_home();
    let data = var("XDG_DATA_HOME").or_else(|| user.as_ref().map(|h| h.join(".local/share")));
    let config = var("XDG_CONFIG_HOME").or_else(|| user.as_ref().map(|h| h.join(".config")));
    let mut files = Vec::new();
    for (t, tools) in [("rok", &["rok"]), ("rokc", &["rokc"])] {
        if let Some(d) = &data {
            files.push((
                d.join("bash-completion/completions").join(t),
                "bash",
                &tools[..],
            ));
        }
        files.push((home.join("zsh").join(format!("_{t}")), "zsh", &tools[..]));
        if let Some(c) = &config {
            let f = c.join("fish/completions").join(format!("{t}.fish"));
            files.push((f, "fish", &tools[..]));
        }
    }
    files.push((
        home.join("completions/rok.ps1"),
        "powershell",
        &["rok", "rokc"],
    ));
    files
}

/// Rewrite the completion scripts that exist from `toolchain`'s tools;
/// what was rewritten, or why not. Never creates one, and a toolchain
/// without `completions` leaves them as they are.
fn refresh_completions(home: &Path, toolchain: &Path) -> Vec<Result<String, String>> {
    let script = |tool: &str, shell: &str| {
        let exe = toolchain.join("bin").join(exe_name(tool));
        let out = Command::new(exe)
            .args(["completions", shell])
            .env("ROK_HOME", home)
            .env_remove("ROK_TOOLCHAIN")
            .current_dir(home)
            .output()
            .ok()?;
        let mut text = out.stdout;
        if !out.status.success() || text.is_empty() {
            return None;
        }
        if !text.ends_with(b"\n") {
            text.push(b'\n');
        }
        Some(text)
    };
    let mut done = Vec::new();
    for (file, shell, tools) in completion_files(home) {
        if !file.is_file() {
            continue;
        }
        let Some(parts) = tools
            .iter()
            .map(|t| script(t, shell))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let mut tmp = file.clone().into_os_string();
        tmp.push(".new");
        let tmp = PathBuf::from(tmp);
        let r = std::fs::write(&tmp, parts.concat()).and_then(|()| std::fs::rename(&tmp, &file));
        done.push(match r {
            Ok(()) => Ok(format!(
                "{shell} completions for {} in {}",
                tools.join(" and "),
                shown(&file)
            )),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(format!("cannot refresh {}: {e}", file.display()))
            }
        });
    }
    done
}

/// Report `refresh_completions`: failures always, the files under `-v`.
fn report_completions(done: &[Result<String, String>], verbose: bool) {
    for r in done {
        match r {
            Ok(m) if verbose => term::note(&format!("refreshed {m}")),
            Ok(_) => {}
            Err(e) => eprintln!("rok: {} {e}", term::label(Stream::Stderr, Role::Warning)),
        }
    }
}

/// Release server in GitHub releases' layout:
/// `download/v<version>/rok-<version>-<triple>.<ext>` (+ `.sha256`) and
/// `latest/download/rok-version`. `ROK_DIST` overrides it.
fn dist() -> String {
    std::env::var("ROK_DIST")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "https://github.com/rok-lang/rok/releases".to_owned())
        .trim_end_matches('/')
        .to_owned()
}

const ARCHIVE: &str = if cfg!(windows) { "zip" } else { "tar.gz" };

/// `v` if it can be a release's full version.
fn release_version(v: &str, from: &str) -> Result<String, String> {
    let ok = v.starts_with(|c: char| c.is_ascii_digit())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    if ok {
        Ok(v.to_owned())
    } else {
        Err(format!("{from}: `{v}` is not a version"))
    }
}

/// The newest release's version, from `<dist>/latest/download/rok-version`.
fn latest(dist: &str) -> Result<String, String> {
    let url = format!("{dist}/latest/download/rok-version");
    let out = run(
        Command::new("curl").args(["-fsSL", "--proto", "=https,http", &url]),
        &format!("curl {url}"),
    )?;
    release_version(out.trim(), &url)
}

/// Check the proxies run `full` as the default (no `ROK_TOOLCHAIN`, no
/// project).
fn check_proxies(home: &Path, full: &str) -> Result<(), String> {
    let version = |tool: &str| {
        let exe = home.join("bin").join(exe_name(tool));
        run(
            Command::new(&exe)
                .arg("--version")
                .env("ROK_HOME", home)
                .env_remove("ROK_TOOLCHAIN")
                .current_dir(home),
            &exe.display().to_string(),
        )
    };
    version("rok")?;
    let out = version("rokc")?;
    let first = out.lines().next().unwrap_or("");
    let want = format!("rokc {full}");
    if first != want && !first.starts_with(&format!("{want} ")) {
        return Err(format!("the rokc proxy says {first:?}, not {want}"));
    }
    Ok(())
}

/// Make `full` the default, write and check its proxies; on failure
/// restore config and proxies, and remove `dir` if `fresh`.
fn switch(home: &Path, full: &str, dir: &Path, fresh: bool) -> Result<(), String> {
    let bin = home.join("bin");
    let config = home.join("config.rokon");
    let old_config = std::fs::read(&config).ok();
    let mut kept = Vec::new();
    for t in TOOLS {
        let p = bin.join(exe_name(t));
        if p.is_file() {
            let prev = bin.join(format!(".{t}.prev"));
            std::fs::copy(&p, &prev).map_err(|e| format!("{}: {e}", p.display()))?;
            kept.push((p, prev));
        }
    }
    let Err(e) = set_default(home, full).and_then(|_| check_proxies(home, full)) else {
        for (_, prev) in &kept {
            let _ = std::fs::remove_file(prev);
        }
        return Ok(());
    };
    let mut lost = Vec::new();
    let restored = match &old_config {
        Some(c) => std::fs::write(&config, c),
        None => std::fs::remove_file(&config),
    };
    if let Err(e) = restored {
        lost.push(format!("{}: {e}", config.display()));
    }
    for t in TOOLS {
        let p = bin.join(exe_name(t));
        let r = match kept.iter().find(|(q, _)| *q == p) {
            Some((_, prev)) => {
                if cfg!(windows) {
                    set_aside(&bin, &p);
                }
                std::fs::rename(prev, &p)
            }
            None if p.exists() => std::fs::remove_file(&p),
            None => Ok(()),
        };
        if let Err(e) = r {
            lost.push(format!("{}: {e}", p.display()));
        }
    }
    if fresh {
        let _ = std::fs::remove_dir_all(dir);
    }
    if lost.is_empty() {
        Err(format!(
            "{e}\nrolled back: the default toolchain and the proxies are as they were"
        ))
    } else {
        Err(format!("{e}\nrolling back failed: {}", lost.join("; ")))
    }
}

/// `rok toolchain update`: install `version` (default: newest), make it
/// the default, update the proxies.
fn update(home: &Path, version: Option<&str>, quiet: bool, verbose: bool) -> Result<(), String> {
    let dist = dist();
    let full = match version {
        Some(v) => release_version(v, "rok toolchain update")?,
        None => latest(&dist)?,
    };
    let name = format!("{full}-{HOST}");
    let dir = root(home).join(&name);
    let current = default_pin(home)?.and_then(|p| find(home, &p));
    if current.as_ref() == Some(&dir) && check_proxies(home, &full).is_ok() {
        if !quiet {
            eprintln!("rok {full} is up to date");
        }
        return Ok(());
    }
    let fresh = !dir.is_dir();
    if fresh {
        let url = format!("{dist}/download/v{full}/rok-{full}-{HOST}.{ARCHIVE}");
        if !quiet {
            term::status("Downloading", &format!("rok {full} ({HOST})"));
        }
        let (tmp, top, _) = stage(home, &full, &url)?;
        place(home, &tmp, &top, &full)?;
    }
    switch(home, &full, &dir, fresh)?;
    let refreshed = refresh_completions(home, &dir);
    if quiet {
        return Ok(());
    }
    let suffix = format!("-{HOST}");
    let old = current
        .as_deref()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned());
    match &old {
        // The default already, with broken proxies.
        Some(n) if *n == name => term::status("Repaired", &format!("rok {full} proxies")),
        Some(n) => {
            let from = n.strip_suffix(&suffix).unwrap_or(n);
            term::status("Updated", &format!("rok {from} -> {full}"));
        }
        None => term::status("Installed", &format!("rok {full} to {}", shown(home))),
    }
    if let Some(old) = old.filter(|n| *n != name) {
        term::note(&format!(
            "{old} stays installed (`rok toolchain remove {old}`)"
        ));
    }
    report_completions(&refreshed, verbose);
    Ok(())
}

/// `path` as `~/..` when under the user's home, for display only.
fn shown(path: &Path) -> String {
    let user = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|h| !h.is_empty());
    match user.and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) if !rest.as_os_str().is_empty() => {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
        }
        _ => path.display().to_string(),
    }
}

/// `rok toolchain clean`: remove every installed toolchain but the
/// current default. Opt-in tidy-up; a version a project pins is removed
/// too, so it is never run implicitly.
fn clean(home: &Path, quiet: bool) -> Result<(), String> {
    let def = default_pin(home)?
        .and_then(|p| find(home, &p))
        .ok_or_else(|| {
            "no default toolchain to keep; set one with `rok toolchain default <version>`, \
         or remove a toolchain by name (`rok toolchain remove <version>`)"
                .to_owned()
        })?;
    let mut removed = 0;
    for (name, _) in installed(home) {
        let dir = root(home).join(&name);
        if dir == def {
            continue;
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        removed += 1;
        if !quiet {
            term::status("Removed", &format!("toolchain {name}"));
        }
    }
    if !quiet {
        let kept = def.file_name().unwrap_or_default().to_string_lossy();
        if removed == 0 {
            eprintln!("nothing to clean; only the default ({kept}) is installed");
        } else {
            term::status("Kept", &format!("the default toolchain {kept}"));
        }
    }
    Ok(())
}

pub const USAGE: &str = "\
usage: rok toolchain list
       rok toolchain install <version> --from <dir | .tar[.gz] | .zip | url>
       rok toolchain default [<version>]
       rok toolchain remove <version>
       rok toolchain update [<version>]
       rok toolchain clean";

pub fn cmd(args: &[String], quiet: bool, verbose: bool) -> Result<i32, String> {
    let h = home();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["list"] => {
            let def = default_pin(&h)?.and_then(|p| find(&h, &p));
            let all = installed(&h);
            if all.is_empty() {
                println!("no toolchains installed (this rok is {VERSION})");
            }
            let current = all
                .iter()
                .position(|(name, _)| def.as_ref().is_some_and(|d| d.ends_with(name)));
            let rows: Vec<(String, String)> =
                all.into_iter().map(|(n, _)| (n, String::new())).collect();
            print!("{}", term::list(&rows, current, "(default)"));
        }
        ["install", version, rest @ ..] => {
            let from = match rest {
                ["--from", f] => *f,
                [f] if f.starts_with("--from=") => &f[7..],
                _ => return Err(USAGE.into()),
            };
            let name = install(&h, version, from)?;
            if !quiet {
                term::status("Installed", &format!("toolchain {name}"));
            }
            // The first toolchain becomes the default; proxies once.
            let def = default_pin(&h)?;
            if def.is_none() {
                set_default(&h, &name)?;
                if !quiet {
                    term::status("Default", &format!("toolchain {name}"));
                }
            } else if !h.join("bin").join(exe_name("rok")).is_file() {
                let d = def.as_deref().and_then(|p| find(&h, p));
                write_proxies(&h, &d.unwrap_or_else(|| root(&h).join(&name)))?;
            }
        }
        ["update"] => {
            update(&h, None, quiet, verbose)?;
            path_warning(&h, quiet);
        }
        ["update", v] if !v.starts_with('-') => {
            update(&h, Some(v), quiet, verbose)?;
            path_warning(&h, quiet);
        }
        ["clean"] => clean(&h, quiet)?,
        ["default"] => match default_pin(&h)? {
            Some(p) => println!("{p}"),
            None => return Err("no default toolchain".into()),
        },
        ["default", pin] => {
            let dir = set_default(&h, pin)?;
            let refreshed = refresh_completions(&h, &dir);
            if !quiet {
                let name = dir.file_name().unwrap_or_default().to_string_lossy();
                term::status("Default", &format!("toolchain {pin} ({name})"));
                report_completions(&refreshed, verbose);
            }
            path_warning(&h, quiet);
        }
        ["remove", name] if !name.contains(['/', '\\']) && !name.starts_with('.') => {
            let dir = [root(&h).join(name), root(&h).join(format!("{name}-{HOST}"))]
                .into_iter()
                .find(|d| d.is_dir())
                .ok_or_else(|| format!("toolchain {name} is not installed"))?;
            std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        _ => return Err(USAGE.into()),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn pins_match_prefixes() {
        assert!(super::matches("0.3", "0.3.1"));
        assert!(super::matches("0.3.1", "0.3.1"));
        assert!(!super::matches("0.3", "0.30.0"));
        assert!(super::matches("0.1", "0.1.0-rc.1"));
        assert!(!super::matches("0.1.0", "0.1.0-rc.1"));
        assert!(!super::matches("0.3.1", "0.3"));
    }

    #[test]
    fn installed_in_semver_order() {
        let home = rokc_core::tmp::test_dir("tc-order");
        for v in ["0.10.0", "0.1.0", "0.1.0-rc.1", "0.9.0"] {
            std::fs::create_dir_all(super::root(&home).join(format!("{v}-{}", super::HOST)))
                .unwrap();
        }
        let got: Vec<String> = super::installed(&home)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        // A pin takes the newest match: the release over its candidate.
        let pick = super::find(&home, "0.1")
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(got, ["0.1.0-rc.1", "0.1.0", "0.9.0", "0.10.0"]);
        assert_eq!(pick, Some(format!("0.1.0-{}", super::HOST)));
    }
}
