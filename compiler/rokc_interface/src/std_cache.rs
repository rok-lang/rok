//! The standard library prebuilt once per toolchain, options and target as
//! one library `rok` (`librok.a` + `rok.rokhir`), linked instead of
//! compiled every build.
//!
//! Kept in `rokc_driver::cache_dir()`, one folder per
//! `<toolchain>-<options>-<sources>` hash, built in scratch and renamed
//! into place so an existing folder is complete. A new folder evicts other
//! toolchains', older sources' and all but the `KEEP` most recent option
//! sets, sparing anything used within `IN_USE` (each use stamps `used`).
//! A failed build leaves `<folder>.failed`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, SystemTime};

use rokc_core::hash::{FNV_OFFSET, fnv_add};

use crate::{BuildOptions, Session, SourceMap, TargetSpec, rokhir, source_files};

/// What the library build depends on besides its sources.
pub struct StdKey<'a> {
    pub bootstrap: &'a [PathBuf],
    pub cfg: &'a [String],
    pub opts: &'a BuildOptions,
    pub target: &'a TargetSpec,
    /// `cfg(test)` (a test or bench build).
    pub test: bool,
}

/// The lock holder touches the lock this often while it builds.
const HEARTBEAT: Duration = Duration::from_secs(2);

/// A lock untouched for this long belongs to a build that died.
const STALE_LOCK: Duration = Duration::from_secs(30);

/// Option sets kept per toolchain (a folder is ~8 MB).
const KEEP: usize = 6;

/// A folder used this recently may still be linked from.
const IN_USE: Duration = Duration::from_secs(3600);

/// Mark `dir` as just used.
fn stamp(dir: &Path) {
    let _ = std::fs::write(dir.join("used"), "");
}

/// When `dir` was used last.
fn last_use(dir: &Path) -> SystemTime {
    std::fs::metadata(dir.join("used"))
        .or_else(|_| std::fs::metadata(dir))
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// `rok.rokhir` of the prebuilt std for `key`, built now if missing; a
/// build in another process is waited for, so fingerprints stay put.
/// `on_build` runs just before this process starts a build.
///
/// `None` (compile from source) when ruled out by options, the cache is
/// read-only, or the build fails.
#[must_use]
pub fn prebuilt_std(key: &StdKey<'_>, on_build: &dyn Fn()) -> Option<PathBuf> {
    let o = key.opts;
    let plain = o.repl.is_none()
        && o.mlir_dialect_plugins.is_empty()
        && o.mlir_pass_plugins.is_empty()
        && o.mlir_passes.is_empty()
        // The standard library is profiled, or at -O3 optimized, with
        // the program.
        && o.profile == crate::Profile::Off
        && o.opt_level != "3";
    // No features in the library; other `--cfg`s and cross builds are rare
    // enough that a separate library build costs more than it saves.
    let cfg: Vec<&String> = key
        .cfg
        .iter()
        .filter(|c| crate::feature_of(c).is_none())
        .collect();
    if !plain || key.bootstrap.is_empty() || !cfg.is_empty() || !key.target.is_host() {
        return None;
    }
    let root = rokc_driver::cache_dir()?;
    let hash = |words: &[&[u8]]| {
        let h = words
            .iter()
            .fold(FNV_OFFSET, |h, w| fnv_add(fnv_add(h, w), b"\0"));
        format!("{h:016x}")
    };
    let toolchain = hash(&[rokhir::producer().as_bytes()]);
    let options = hash(&[
        o.opt_level.as_bytes(),
        format!(
            "{} {:?} {:016x}",
            o.debug,
            o.overflow_checks,
            key.target.hash()
        )
        .as_bytes(),
        // Either backend's objects link with the other's, but a cached
        // library is the backend's own build.
        if rokc_driver::native::chosen(o) {
            b"native".as_slice()
        } else {
            b"llvm".as_slice()
        },
        format!("{} {cfg:?} {:?}", key.test, key.bootstrap).as_bytes(),
    ]);
    let mut sources: Vec<Vec<u8>> = Vec::new();
    for b in key.bootstrap {
        for f in source_files(b.parent().unwrap_or(b)) {
            sources.push(f.display().to_string().into_bytes());
            sources.push(std::fs::read(&f).unwrap_or_default());
        }
    }
    let sources = hash(&sources.iter().map(Vec::as_slice).collect::<Vec<_>>());
    let family = format!("{toolchain}-{options}-");
    let dir = root.join(format!("{family}{sources}"));
    let rokhir = dir.join(format!("{}.rokhir", rokc_driver::STD_PKG));
    let failed = dir.with_extension("failed");
    let lock = root.join(format!("{family}lock"));
    loop {
        if rokhir.is_file() {
            stamp(&dir);
            return Some(rokhir);
        }
        if failed.exists() {
            return None;
        }
        std::fs::create_dir_all(&root).ok()?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            // Windows: a just-removed lock still open elsewhere is pending
            // deletion, and recreating it is denied meanwhile.
            Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(_) => return None,
        }
        let stale = std::fs::metadata(&lock)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > STALE_LOCK));
        if stale {
            let _ = std::fs::remove_file(&lock);
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    // Finished between the check and the lock.
    if rokhir.is_file() {
        let _ = std::fs::remove_file(&lock);
        stamp(&dir);
        return Some(rokhir);
    }
    let (stop, beat) = std::sync::mpsc::channel::<()>();
    let held = lock.clone();
    let heartbeat = std::thread::spawn(move || {
        while beat.recv_timeout(HEARTBEAT) == Err(RecvTimeoutError::Timeout) {
            if let Ok(f) = std::fs::File::options().write(true).open(&held) {
                let _ = f.set_modified(SystemTime::now());
            }
        }
    });
    on_build();
    let built = build(key, &root, &dir);
    drop(stop);
    let _ = heartbeat.join();
    if !built {
        // Not tried again until the sources or the toolchain change.
        let _ = std::fs::write(&failed, "");
    }
    if built {
        stamp(&dir);
    }
    prune(&root, &toolchain, &family, &sources);
    let _ = std::fs::remove_file(&lock);
    built.then_some(rokhir)
}

/// Remove other toolchains' builds, this one's of older sources, and
/// all but the `KEEP` option sets used last; none used within `IN_USE`.
fn prune(root: &Path, toolchain: &str, family: &str, sources: &str) {
    let idle = |t: &SystemTime| t.elapsed().is_ok_and(|age| age > IN_USE);
    let mut kept = Vec::new();
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = e.path();
        // Scratch folders, and the library's collected stages (swept
        // on their own).
        if name.starts_with('.') || name == rokc_driver::LIBRARY_STAGES {
            continue;
        }
        let used = last_use(&p);
        let old =
            !name.starts_with(toolchain) || (name.starts_with(family) && !name.contains(sources));
        if old {
            if idle(&used) {
                let _ = std::fs::remove_dir_all(&p).or_else(|_| std::fs::remove_file(&p));
            }
        } else if p.is_dir() {
            kept.push((used, p));
        }
    }
    kept.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in kept.into_iter().skip(KEEP).filter(|(t, _)| idle(t)) {
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::remove_file(p.with_extension("failed"));
    }
}

/// Build into a scratch folder of `root`, then rename it to `dir`.
fn build(key: &StdKey<'_>, root: &Path, dir: &Path) -> bool {
    let tmp = root.join(format!(".build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    if std::fs::create_dir_all(&tmp).is_err() {
        return false;
    }
    let mut s = Session::new();
    for b in key.bootstrap {
        s.add_bootstrap_file(b.clone());
    }
    for c in key.cfg {
        s.add_cfg(c.clone());
    }
    if key.test {
        s.add_cfg("test".to_owned());
    }
    s.set_target(key.target);
    s.set_std_lib(true);
    let stub = tmp.join("rok.prebuilt").join("lib.rok");
    let r = s.compile_source(
        "// the prebuilt standard library\n",
        &stub,
        SourceMap::new(),
    );
    let archive = tmp.join(format!("lib{}.a", rokc_driver::STD_PKG));
    let ok = !r.has_errors() && r.archive(key.opts, &archive).is_ok();
    if ok && std::fs::rename(&tmp, dir).is_ok() {
        return true;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    // Another process may have put it there first.
    dir.join(format!("{}.rokhir", rokc_driver::STD_PKG))
        .is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_the_option_sets_used_last() {
        let root = rokc_core::tmp::test_dir("std-cache");
        for i in 0..KEEP + 2 {
            let dir = root.join(format!("tc-opt{i}-src"));
            std::fs::create_dir_all(&dir).unwrap();
            stamp(&dir);
            let used = std::fs::File::options()
                .write(true)
                .open(dir.join("used"))
                .unwrap();
            let secs = 1_000_000 + u64::try_from(i).unwrap();
            used.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        }
        for stale in ["other-opt0-src", "tc-opt0-stale"] {
            let dir = root.join(stale);
            std::fs::create_dir_all(&dir).unwrap();
            let used = std::fs::File::create(dir.join("used")).unwrap();
            used.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        }
        // In use: kept though of another toolchain.
        std::fs::create_dir_all(root.join("busy-opt0-src")).unwrap();
        // Used just now: kept beyond `KEEP`.
        for i in 0..2 {
            let dir = root.join(format!("tc-now{i}-src"));
            std::fs::create_dir_all(&dir).unwrap();
            stamp(&dir);
        }
        prune(&root, "tc", "tc-opt0-", "src");
        let mut left: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut want: Vec<String> = (KEEP - 2..KEEP + 2)
            .map(|i| format!("tc-opt{i}-src"))
            .collect();
        want.splice(
            0..0,
            ["busy-opt0-src", "tc-now0-src", "tc-now1-src"].map(str::to_owned),
        );
        assert_eq!(left, want);
        let _ = std::fs::remove_dir_all(&root);
    }
}
