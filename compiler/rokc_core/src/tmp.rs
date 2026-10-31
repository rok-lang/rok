//! Temporary entries named `<prefix><pid>[-.]...` in the system temp
//! directory: removed on drop, at `exit` and by [`remove_live`], and
//! swept once their owner died without removing them (killed, crashed).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Entries not yet dropped, for exits that skip drops.
static LIVE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn track(path: &Path) {
    static AT_EXIT: std::sync::Once = std::sync::Once::new();
    AT_EXIT.call_once(|| {
        unsafe extern "C" {
            fn atexit(f: extern "C" fn()) -> i32;
        }
        extern "C" fn at_exit() {
            remove_live();
        }
        // SAFETY: registers a plain function.
        unsafe { atexit(at_exit) };
    });
    LIVE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(path.to_owned());
}

fn untrack(path: &Path) {
    LIVE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|p| p != path);
}

/// Run by [`remove_live`] first: ends what still holds files in live
/// entries (Windows can't delete a running executable's image).
static RELEASE: Mutex<Vec<fn()>> = Mutex::new(Vec::new());

/// Have [`remove_live`] call `f` before removing anything.
pub fn on_release(f: fn()) {
    let mut r = RELEASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !r.contains(&f) {
        r.push(f);
    }
}

/// Remove every live entry: for an exit that runs no drops (`exit`,
/// dying of a signal on purpose). Skips them if another thread holds the
/// list.
pub fn remove_live() {
    let released = RELEASE.try_lock().is_ok_and(|r| {
        r.iter().for_each(|f| f());
        !r.is_empty()
    });
    if let Ok(live) = LIVE.try_lock() {
        for p in live.iter() {
            remove(p);
            // Windows unmaps an ended process's image a moment after it
            // ends: until then its file can't be deleted.
            for _ in 0..if released && cfg!(windows) { 100 } else { 0 } {
                if !p.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
                remove(p);
            }
        }
    }
}

/// A file or directory in the temp directory, removed on drop.
pub struct TempEntry {
    path: PathBuf,
    keep: bool,
}

impl TempEntry {
    /// A new private directory `<temp>/<prefix><pid>-<n>`.
    pub fn dir(prefix: &str) -> std::io::Result<Self> {
        sweep_once(&[prefix]);
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        for attempt in 0..100u32 {
            let path = base.join(format!("{prefix}{pid}-{nanos:x}-{attempt}"));
            match private_dir(&path) {
                Ok(()) => return Ok(Self::adopt(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other(
            "cannot create a unique temporary directory",
        ))
    }

    /// Take over `path` (an entry this process named), removed on drop.
    #[must_use]
    pub fn adopt(path: PathBuf) -> Self {
        track(&path);
        Self { path, keep: false }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Leave the entry in place, and out of later sweeps.
    pub fn keep(&mut self) {
        self.keep = true;
        untrack(&self.path);
        if self.path.is_dir() {
            let _ = std::fs::write(self.path.join(KEEP), "");
        }
    }
}

impl Drop for TempEntry {
    fn drop(&mut self) {
        if !self.keep {
            untrack(&self.path);
            remove(&self.path);
        }
    }
}

/// Marks a directory kept on request (`--keep-temps`): never swept.
const KEEP: &str = ".rok-keep";

fn remove(p: &Path) {
    if std::fs::remove_dir_all(p).is_err() {
        let _ = std::fs::remove_file(p);
    }
}

/// Create `path` readable by this user alone (Windows: the user's temp
/// directory is private already).
pub fn private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(path);
    #[cfg(not(unix))]
    std::fs::create_dir(path)
}

/// Remove, once per process, temp entries of `prefixes` whose owner is
/// gone and that are an hour old: an hour keeps a reused pid's fresh
/// entries safe.
pub fn sweep_once(prefixes: &[&str]) {
    static DONE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut done = DONE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let todo: Vec<&str> = prefixes
        .iter()
        .copied()
        .filter(|p| !done.iter().any(|d| d == p))
        .collect();
    if todo.is_empty() {
        return;
    }
    done.extend(todo.iter().map(|p| (*p).to_owned()));
    drop(done);
    sweep(&std::env::temp_dir(), &todo, Duration::from_secs(3600));
}

/// Remove entries `<prefix><pid>[-.]...` of `dir` whose process `pid` is not
/// running, at least `min_age` since their last change.
pub fn sweep(dir: &Path, prefixes: &[&str], min_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let me = std::process::id();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|n| owner(n, prefixes)) else {
            continue;
        };
        let path = e.path();
        if pid == me || path.join(KEEP).exists() || pid_alive(pid) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|a| a >= min_age));
        if old {
            remove(&path);
        }
    }
}

/// `<temp>/<name>-<euid>`, made mode 0700, for a cache whose files are
/// trusted.
///
/// One already there must be a directory (not a link) of this user that
/// nobody else may use, else the error names its owner and mode.
/// Elsewhere `<temp>/<name>`: the Windows temp directory is the user's.
pub fn user_cache_dir(name: &str) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        // SAFETY: no arguments, cannot fail.
        let uid = unsafe { geteuid() };
        let dir = std::env::temp_dir().join(format!("{name}-{uid}"));
        let made = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir);
        let m = std::fs::symlink_metadata(&dir).map_err(|e| match made {
            Err(made) => format!("cannot create {}: {made}", dir.display()),
            Ok(()) => format!("{}: {e}", dir.display()),
        })?;
        let mode = m.permissions().mode() & 0o7777;
        if m.file_type().is_symlink() {
            return Err(format!(
                "{} is not private to you (a symbolic link)",
                dir.display()
            ));
        }
        if !m.is_dir() || m.uid() != uid || mode & 0o077 != 0 {
            return Err(format!(
                "{} is not private to you (owner uid {}, mode {mode:04o})",
                dir.display(),
                m.uid()
            ));
        }
        Ok(dir)
    }
    #[cfg(not(unix))]
    {
        Ok(std::env::temp_dir().join(name))
    }
}

/// Remove files under `dir` not changed for `age`, then directories that
/// emptied and are that old themselves: a build that just made one (other
/// processes share the cache) keeps it.
pub fn evict(dir: &Path, age: Duration) -> bool {
    let old = |m: std::io::Result<std::fs::Metadata>| {
        m.and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|a| a > age))
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut empty = true;
    for e in entries.flatten() {
        let p = e.path();
        let gone = match e.file_type() {
            Ok(t) if t.is_dir() => {
                let aged = old(e.metadata());
                evict(&p, age) && aged && std::fs::remove_dir(&p).is_ok()
            }
            Ok(_) => old(e.metadata()) && std::fs::remove_file(&p).is_ok(),
            Err(_) => false,
        };
        empty &= gone;
    }
    empty
}

/// The pid in `name` = `<prefix><digits>` followed by nothing, `-` or `.`.
fn owner(name: &str, prefixes: &[&str]) -> Option<u32> {
    prefixes.iter().find_map(|p| {
        let rest = name.strip_prefix(p)?;
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if end == 0 || !matches!(rest[end..].chars().next(), None | Some('-' | '.')) {
            return None;
        }
        rest[..end].parse().ok()
    })
}

/// Whether process `pid` exists (one of another user counts).
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let Ok(p) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { kill(p, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(3) // ESRCH
    }
    #[cfg(windows)]
    {
        type Handle = *mut std::ffi::c_void;
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
            fn GetExitCodeProcess(h: Handle, code: *mut u32) -> i32;
            fn CloseHandle(h: Handle) -> i32;
        }
        const QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        const ERROR_INVALID_PARAMETER: i32 = 87;
        // SAFETY: a query-only handle, closed below.
        let h = unsafe { OpenProcess(QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return std::io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER);
        }
        let mut code = 0u32;
        // SAFETY: `h` is a live process handle.
        let ok = unsafe { GetExitCodeProcess(h, &raw mut code) } != 0;
        // SAFETY: `h` is ours.
        unsafe { CloseHandle(h) };
        !ok || code == STILL_ACTIVE
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// This test process's scratch root, `$ROK_TEST_TMP` (else the temp
/// directory) `/rok-test-<pid>`: removed at exit unless
/// `ROK_KEEP_TEST_TMP` is set. Roots of test processes that died are
/// removed first.
pub fn test_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let base = std::env::var_os("ROK_TEST_TMP")
            .filter(|b| !b.is_empty())
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let _ = std::fs::create_dir_all(&base);
        // Also the per-test directories of older harnesses.
        sweep(&base, &["rok-test-", "rokc-test-"], Duration::ZERO);
        let root = base.join(format!("rok-test-{}", std::process::id()));
        remove(&root);
        std::fs::create_dir_all(&root).expect("cannot create the test scratch root");
        let root = crate::real_path(&root).unwrap_or(root);
        let _ = TEST_ROOT_PATH.set(root.clone());
        if std::env::var_os("ROK_KEEP_TEST_TMP").is_none() {
            unsafe extern "C" {
                fn atexit(f: extern "C" fn()) -> i32;
            }
            // SAFETY: registers a plain function.
            unsafe { atexit(remove_test_root) };
        }
        root
    })
}

extern "C" fn remove_test_root() {
    if let Some(root) = TEST_ROOT_PATH.get() {
        remove(root);
    }
}

static TEST_ROOT_PATH: OnceLock<PathBuf> = OnceLock::new();

/// A fresh, empty directory `name` in [`test_root`].
pub fn test_dir(name: &str) -> PathBuf {
    let dir = test_root().join(name);
    remove(&dir);
    std::fs::create_dir_all(&dir).expect("cannot create a test scratch directory");
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_reads_the_pid_after_a_prefix() {
        let p = ["rokc-", "rok-install-"];
        assert_eq!(owner("rokc-12-ab-0", &p), Some(12));
        assert_eq!(owner("rok-install-7-tool", &p), Some(7));
        assert_eq!(owner("rokc-9.rok", &p), Some(9));
        assert_eq!(owner("rokc-target", &p), None);
        assert_eq!(owner("rokc-12x", &p), None);
        assert_eq!(owner("other-12", &p), None);
    }

    #[test]
    fn sweep_removes_only_dead_owners() {
        let dir = test_dir("sweep");
        let dead = dir.join("rokc-4000000000-0");
        let live = dir.join(format!("rokc-{}-1", std::process::id()));
        let kept = dir.join("rokc-4000000001-2");
        for d in [&dead, &live, &kept] {
            std::fs::create_dir(d).unwrap();
        }
        std::fs::write(kept.join(KEEP), "").unwrap();
        sweep(&dir, &["rokc-"], Duration::ZERO);
        assert!(!dead.exists());
        assert!(live.exists() && kept.exists());
    }

    #[test]
    fn evict_keeps_a_directory_a_build_just_made() {
        let dir = test_dir("evict");
        let stale = dir.join("debug/old.o");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let fresh = dir.join("scripts/0123");
        std::fs::create_dir_all(&fresh).unwrap();
        evict(&dir, Duration::from_millis(200));
        assert!(fresh.exists(), "an empty directory made just now stays");
        assert!(!stale.exists() && !dir.join("debug").exists());
    }
}
