//! Named environments, `$ROK_HOME/envs/<name>/`: dependency sets used
//! outside a project, picked by `--env`, `$ROK_ENV`, `rok env use`, else
//! `default` (no activation step).

use std::path::{Path, PathBuf};
use std::process::Command;

use rokc_core::term;

use crate::manifest::{self, LOCK, MANIFEST, Source};
use crate::{Global, ambient_env, env_dir, home};

/// The environments' names, sorted.
pub fn names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(home().join("envs"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// `rok env use`'s choice, in `envs/`; `CURRENT` too for older rok. No
/// environment can be called either (`check`).
const CHOICE: &str = ".current";
const OLD_CHOICE: &str = "CURRENT";

/// Why `name` can't be a new environment.
fn check(name: &str) -> Result<(), String> {
    rokc_core::check_os_name("environment", name)?;
    if [CHOICE, OLD_CHOICE]
        .iter()
        .any(|r| r.trim_start_matches('.').eq_ignore_ascii_case(name))
    {
        return Err(format!(
            "`{name}` is reserved for `rok env use`'s choice; name the environment something \
             else, e.g. `my_{name}`"
        ));
    }
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    if name.chars().all(ok) {
        return Ok(());
    }
    let fixed: String = name.chars().map(|c| if ok(c) { c } else { '_' }).collect();
    Err(format!(
        "`{name}` is not a valid environment name: ASCII letters, digits, `_`, `-` and `.`; try \
         `{fixed}`"
    ))
}

/// Check environment `name` taken from `source` (`--env`, `ROK_ENV`, the
/// choice file; empty for an argument). A path is an error; a name
/// `check` refuses only warns when that environment exists (an older rok
/// made it).
pub fn name(source: &str, name: &str) -> Result<(), String> {
    let at = |e: String| {
        if source.is_empty() {
            e
        } else {
            format!("{source}: {e}")
        }
    };
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':', '\0']) {
        return Err(at(format!(
            "`{name}` is a path, not an environment name; use a name from `rok env list`, e.g. \
             `default`"
        )));
    }
    match check(name) {
        Err(e) if env_dir(name).is_dir() => {
            crate::warn_once(&at(e));
            Ok(())
        }
        r => r.map_err(at),
    }
}

/// `rok env use`'s choice, if any. `CURRENT` alone is an older rok's;
/// when both files are there the newer wins, and `.current` without
/// `CURRENT` means an older rok cleared the choice.
pub fn chosen() -> Result<Option<String>, String> {
    let envs = home().join("envs");
    let (new, old) = (envs.join(CHOICE), envs.join(OLD_CHOICE));
    let time = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let file = match (time(&new), old.is_dir(), time(&old)) {
        (None, _, _) => old,
        (Some(_), true, _) => new,
        (Some(_), false, None) => return Ok(None),
        (Some(n), false, Some(o)) => {
            if o > n {
                old
            } else {
                new
            }
        }
    };
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(None);
    };
    // The first line: a newer rok may add more.
    let chosen = text.lines().next().unwrap_or("").trim();
    if chosen.is_empty() {
        return Ok(None);
    }
    name(&file.display().to_string(), chosen)?;
    Ok(Some(chosen.to_owned()))
}

/// Make `name` `rok env use`'s choice.
fn choose(name: &str) -> Result<(), String> {
    let envs = home().join("envs");
    std::fs::create_dir_all(&envs).map_err(|e| format!("{}: {e}", envs.display()))?;
    let write = |f: &str| {
        let file = envs.join(f);
        std::fs::write(&file, format!("{name}\n")).map_err(|e| format!("{}: {e}", file.display()))
    };
    // `CURRENT` first, so `.current` is never the older; skipped when an
    // environment's directory has that name (a case-insensitive `current`).
    if !envs.join(OLD_CHOICE).is_dir() {
        write(OLD_CHOICE)?;
    }
    write(CHOICE)
}

pub fn create(name: &str) -> Result<PathBuf, String> {
    check(name)?;
    if let Some(other) = names().into_iter().find(|n| n.eq_ignore_ascii_case(name)) {
        return Err(if other == name {
            format!("environment `{name}` already exists")
        } else {
            format!(
                "environment `{other}` already exists, and Windows and macOS see `{name}` as the \
                 same name; pick another, or use `{other}`"
            )
        });
    }
    let dir = env_dir(name);
    if dir.exists() {
        return Err(format!("environment `{name}` already exists"));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(
        dir.join(MANIFEST),
        format!("// rok environment `{name}`\n{}", manifest::EMPTY),
    )
    .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

fn existing(name: &str) -> Result<PathBuf, String> {
    self::name("", name)?;
    let dir = env_dir(name);
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(format!("no environment `{name}`"))
    }
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to)
        .map(drop)
        .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))
}

pub fn cmd(g: &Global, args: &[String]) -> Result<i32, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["create", name] => {
            let dir = create(name)?;
            if !g.quiet {
                term::status(
                    "Created",
                    &format!("environment `{name}` ({})", dir.display()),
                );
            }
        }
        ["list"] => {
            let active = ambient_env()?;
            let names = names();
            if names.is_empty() {
                term::note("no environments; make one with `rok env create <name>`");
                return Ok(0);
            }
            let current = names.iter().position(|n| *n == active);
            let rows: Vec<(String, String)> =
                names.into_iter().map(|n| (n, String::new())).collect();
            print!("{}", term::list(&rows, current, "(active)"));
        }
        ["remove", name] => {
            let dir = existing(name)?;
            std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            if chosen().ok().flatten().as_deref() == Some(*name) {
                for f in [CHOICE, OLD_CHOICE] {
                    let _ = std::fs::remove_file(home().join("envs").join(f));
                }
            }
        }
        ["use", name] => {
            if *name != "default" {
                existing(name)?;
            }
            choose(name)?;
        }
        ["export", name, dir] => {
            let src = existing(name)?;
            let dir = Path::new(dir);
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            copy(&src.join(MANIFEST), &dir.join(MANIFEST))?;
            if src.join(LOCK).is_file() {
                copy(&src.join(LOCK), &dir.join(LOCK))?;
            }
        }
        ["import", name, from] => import(g, name, Path::new(from))?,
        ["shell"] => return shell(&ambient_env()?),
        ["shell", name] => {
            existing(name)?;
            return shell(name);
        }
        _ => {
            return Err(
                "usage: rok env create|remove|use <name> | list | export <name> <dir> | \
                        import <name> <dir|rok.rokon> | shell [name]"
                    .into(),
            );
        }
    }
    Ok(0)
}

/// Copy a project's `rok.rokon` / `rok.lock` into a new environment (path
/// deps made absolute) and resolve; the lock keeps its commits.
fn import(g: &Global, name: &str, from: &Path) -> Result<(), String> {
    let (src_dir, path) = if from.is_dir() {
        let m =
            manifest::find(from).ok_or_else(|| format!("no {MANIFEST} in {}", from.display()))?;
        (from.to_path_buf(), m)
    } else {
        (
            from.parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            from.to_path_buf(),
        )
    };
    let mut text =
        std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file = path.display().to_string();
    let m = manifest::parse(&text, &file)?;
    for (section, deps) in [("deps", &m.deps), ("dev_deps", &m.dev_deps)] {
        for d in deps {
            if let Source::Path(p) = &d.source {
                if Path::new(p).is_relative() {
                    let abs = rokc_core::real_path(src_dir.join(p))
                        .map_err(|e| format!("dependency `{}`: {p}: {e}", d.name))?;
                    let s = Source::Path(abs.display().to_string());
                    text = manifest::set_dep(&text, &file, section, &d.name, &s.to_rok())?;
                }
            }
        }
    }
    if let Some(from) = m
        .lints_from
        .as_deref()
        .filter(|p| Path::new(p).is_relative())
    {
        let abs = rokc_core::real_path(src_dir.join(from))
            .map_err(|e| format!("`lints_from`: {from}: {e}"))?;
        let value = rokc_rokon::to_line(&rokc_rokon::Value::str(abs.display().to_string()));
        text = rokc_rokon::edit::set(
            &text,
            &[rokc_rokon::edit::Step::Field("lints_from")],
            &value,
        )
        .map_err(|e| e.render(&text, &file))?;
    }
    let dir = create(name)?;
    let done = std::fs::write(dir.join(MANIFEST), &text)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            if src_dir.join(LOCK).is_file() {
                copy(&src_dir.join(LOCK), &dir.join(LOCK))?;
            }
            let ctx = crate::load_dir(&dir)?;
            crate::resolve::resolve(&home(), &ctx, &crate::options(g)).map(drop)
        });
    if done.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    done
}

/// `$SHELL` (`%COMSPEC%`) with `ROK_ENV=<name>` and this toolchain first
/// on `PATH`.
fn shell(name: &str) -> Result<i32, String> {
    let (var, default) = if cfg!(windows) {
        ("COMSPEC", "cmd.exe")
    } else {
        ("SHELL", "/bin/sh")
    };
    let sh = std::env::var_os(var).unwrap_or_else(|| default.into());
    let mut path: Vec<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
        .into_iter()
        .collect();
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path).map_err(|e| e.to_string())?;
    let status = Command::new(&sh)
        .env("ROK_ENV", name)
        .env("PATH", path)
        .status()
        .map_err(|e| format!("cannot run {}: {e}", sh.to_string_lossy()))?;
    Ok(status.code().unwrap_or(1))
}
