//! Definitions shared by the compiler and the `rok` tool.

pub mod cli;
pub mod codec;
pub mod completions;
pub mod def;
pub mod hash;
pub mod lint;
pub mod local;
pub mod suggest;
pub mod term;
pub mod tmp;

use std::path::{Path, PathBuf};

pub use def::{DefId, DefInfo, DefKind, DefTable, Visibility};

/// `$ROK_HOME`, else the home the running executable sits in, else
/// `~/.rok` (`%USERPROFILE%\.rok`). Spec 9.2.19.
#[must_use]
pub fn rok_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("ROK_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    if let Some(h) = std::env::current_exe().ok().and_then(|e| home_of(&e)) {
        return Some(h);
    }
    Some(user_home()?.join(".rok"))
}

/// Env knob `name`; `None` if unset or empty. An invalid value warns once
/// (naming `expected`) and reads as unset.
pub fn env_knob<T>(
    name: &'static str,
    expected: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Option<T> {
    static WARNED: std::sync::Mutex<std::collections::BTreeSet<&str>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let v = std::env::var_os(name).filter(|v| !v.is_empty())?;
    let r = v.to_str().and_then(parse);
    if r.is_none()
        && WARNED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name)
    {
        let v = v.to_string_lossy();
        eprintln!("rokc: warning: ignoring {name}={v} (expected {expected})");
    }
    r
}

/// Knob `name` set to one of `choices` (`a|b|c`): that choice.
pub fn env_choice(name: &'static str, choices: &'static str) -> Option<&'static str> {
    env_knob(name, choices, |v| choices.split('|').find(|&c| c == v))
}

/// Knob `name` = `0|1`.
#[must_use]
pub fn env_flag(name: &'static str) -> bool {
    env_choice(name, "0|1") == Some("1")
}

/// The user's home directory: `%USERPROFILE%` on Windows, else `$HOME`.
#[must_use]
pub fn user_home() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `fs::canonicalize` without Windows' `\\?\` / `\\?\UNC\` prefixes.
pub fn real_path(p: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let real = std::fs::canonicalize(p)?;
    if cfg!(windows) {
        if let Some(plain) = real.to_str().and_then(unverbatim) {
            return Ok(PathBuf::from(plain));
        }
    }
    Ok(real)
}

/// An exclusive lock on the file at `path` (created, with its directory,
/// if missing), held until the returned file is dropped. Blocks while
/// another process holds it.
pub fn lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        unsafe extern "C" {
            fn flock(fd: i32, op: i32) -> i32;
        }
        const LOCK_EX: i32 = 2;
        loop {
            // SAFETY: `f` is an open file.
            if unsafe { flock(f.as_raw_fd(), LOCK_EX) } == 0 {
                break;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        #[repr(C)]
        struct Overlapped {
            internal: usize,
            internal_high: usize,
            offset: u32,
            offset_high: u32,
            event: *mut std::ffi::c_void,
        }
        unsafe extern "system" {
            fn LockFileEx(
                file: *mut std::ffi::c_void,
                flags: u32,
                reserved: u32,
                low: u32,
                high: u32,
                ov: *mut Overlapped,
            ) -> i32;
        }
        const LOCKFILE_EXCLUSIVE_LOCK: u32 = 2;
        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: std::ptr::null_mut(),
        };
        // SAFETY: `f` is an open synchronous handle; `ov` outlives the call.
        let ok = unsafe {
            LockFileEx(
                f.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                u32::MAX,
                u32::MAX,
                &raw mut ov,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(f)
}

/// A stamp that changes whenever the file at `p` is written or replaced.
///
/// Size, mtime, inode and ctime, without reading the file (Windows: file
/// id and creation time; the id because NTFS gives a recreated name the
/// old creation time). `-` when missing.
#[must_use]
pub fn file_stamp(p: &Path) -> String {
    let Ok(m) = std::fs::metadata(p) else {
        return "-".to_owned();
    };
    let nanos = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos())
    };
    #[cfg(unix)]
    let made = {
        use std::os::unix::fs::MetadataExt;
        format!("{:x}-{:x}.{:x}", m.ino(), m.ctime(), m.ctime_nsec())
    };
    #[cfg(windows)]
    let made = format!("{}-{:x}", windows_file_id(p), nanos(m.created()));
    #[cfg(not(any(unix, windows)))]
    let made = format!("{:x}", nanos(m.created()));
    format!("{:x}-{:x}-{made}", m.len(), nanos(m.modified()))
}

/// The volume serial number and file index of `p` (`0.0` when it cannot
/// be opened): what an inode is on Unix.
#[cfg(windows)]
fn windows_file_id(p: &Path) -> String {
    use std::os::windows::io::AsRawHandle;
    unsafe extern "system" {
        // BY_HANDLE_FILE_INFORMATION is 13 DWORDs.
        fn GetFileInformationByHandle(file: *mut std::ffi::c_void, info: *mut [u32; 13]) -> i32;
    }
    let mut info = [0u32; 13];
    let ok = std::fs::File::open(p).is_ok_and(
        |f| unsafe { GetFileInformationByHandle(f.as_raw_handle(), &raw mut info) } != 0,
    );
    if !ok {
        return "0.0".to_owned();
    }
    // dwVolumeSerialNumber, nFileIndexHigh, nFileIndexLow.
    format!(
        "{:x}.{:x}",
        info[7],
        (u64::from(info[11]) << 32) | u64::from(info[12])
    )
}

/// A Windows verbatim path without its `\\?\` prefix, when it has an
/// ordinary spelling.
fn unverbatim(s: &str) -> Option<String> {
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    let rest = s.strip_prefix(r"\\?\")?;
    let drive = rest.as_bytes();
    (drive.len() >= 3 && drive[0].is_ascii_alphabetic() && &drive[1..3] == b":\\")
        .then(|| rest.to_owned())
}

/// The running executable, through symlinks ([`real_path`]).
#[must_use]
pub fn current_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(real_path(&exe).unwrap_or(exe))
}

/// The rok home an executable is installed in: `<home>/bin/<exe>` (a
/// proxy: `<home>/toolchains/` exists) or `<home>/toolchains/<name>/bin/<exe>`.
#[must_use]
pub fn home_of(exe: &Path) -> Option<PathBuf> {
    let exe = real_path(exe).unwrap_or_else(|_| exe.to_path_buf());
    let up = exe.parent()?.parent()?;
    if up.join("toolchains").is_dir() {
        return Some(up.to_path_buf());
    }
    let tcs = up.parent()?;
    (tcs.file_name()? == "toolchains").then(|| tcs.parent().map(Path::to_path_buf))?
}

/// The standard library's package name.
pub const STD_PKG: &str = "rok";

/// Reserved keywords (spec 1.5); the lexer's list.
pub const KEYWORDS: &[&str] = &[
    "let", "fn", "if", "else", "match", "while", "loop", "for", "return", "break", "continue",
    "try", "throw", "async", "await", "yield", "unsafe", "using", "struct", "enum", "trait",
    "impl", "pub", "mod", "use", "as", "is", "ref", "true", "false", "none", "self", "Self",
];

/// Why `name` (code name: `-` spelled `_`) can't be a package name: code
/// could not name it, or it would mount into `rok::` beside the standard
/// library.
#[must_use]
pub fn reserved_package(name: &str) -> Option<String> {
    let why = if name == STD_PKG {
        "is reserved for the standard library".to_owned()
    } else if name == "pkg" {
        "already names a package's own root (`pkg::`), so code can't name a package called that"
            .to_owned()
    } else if name == "sup" {
        "already names the parent module (`sup::`), so code can't name a package called that"
            .to_owned()
    } else if KEYWORDS.contains(&name) {
        "is a keyword, so code can't name a package called that".to_owned()
    } else {
        return None;
    };
    Some(format!(
        "`{name}` {why}; name the package something else, e.g. `{name}_util`"
    ))
}

/// The entry of `cmd` in a usage text: its lines (`  cmd ...`, or
/// `  a | cmd | b ...`) and the more indented lines after each.
#[must_use]
pub fn command_help(usage: &str, cmd: &str) -> Option<String> {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut out = String::new();
    let mut entry: Option<usize> = None;
    for line in usage.lines() {
        let head = line.trim_start();
        let names = head
            .split("  ")
            .next()
            .unwrap_or("")
            .split(|c: char| c == '|' || c.is_whitespace())
            .take_while(|w| !w.starts_with(['<', '[', '-']))
            .any(|w| w == cmd);
        let starts = indent(line) > 0 && names;
        let continues = entry.is_some_and(|e| indent(line) > e) && !head.is_empty();
        if starts && entry.is_none_or(|e| indent(line) <= e) {
            entry = Some(indent(line));
        } else if !continues {
            entry = None;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    (!out.is_empty()).then_some(out)
}

/// Length of the shebang line (spec 1.2.3), terminator excluded; 0 if
/// none. `#!` followed (past whitespace and comments) by `[` is an inner
/// attribute, not a shebang.
#[must_use]
pub fn shebang_len(src: &str) -> usize {
    let Some(mut rest) = src.strip_prefix("#!") else {
        return 0;
    };
    let b = |s: &str, i: usize| s.as_bytes().get(i).copied();
    loop {
        rest = rest.trim_start_matches(['\t', '\n', '\u{b}', '\u{c}', '\r', ' ']);
        if rest.starts_with("//") && !rest.starts_with("///") && !rest.starts_with("//!")
            || rest.starts_with("////")
        {
            rest = rest.find(['\n', '\r']).map_or("", |i| &rest[i..]);
        } else if rest.starts_with("/*")
            && b(rest, 2) != Some(b'!')
            && (b(rest, 2) != Some(b'*') || matches!(b(rest, 3), Some(b'*' | b'/')))
        {
            let mut depth = 0usize;
            let mut i = 0;
            while i + 1 < rest.len() {
                match &rest.as_bytes()[i..i + 2] {
                    b"/*" => (depth, i) = (depth + 1, i + 2),
                    b"*/" => (depth, i) = (depth - 1, i + 2),
                    _ => i += 1,
                }
                if depth == 0 {
                    break;
                }
            }
            if depth > 0 {
                break;
            }
            rest = &rest[i..];
        } else {
            break;
        }
    }
    if rest.starts_with('[') {
        return 0;
    }
    src.find(['\n', '\r']).unwrap_or(src.len())
}

#[cfg(test)]
mod tests {
    #[test]
    fn shebang_lines() {
        let s = super::shebang_len;
        assert_eq!(s("#!/usr/bin/env rok\nprintln(1);"), 18);
        assert_eq!(s("#!/usr/bin/env rok"), 18);
        assert_eq!(s("#!rok\r\nx"), 5);
        assert_eq!(s("#![allow(unused)]\n"), 0);
        assert_eq!(s("#! [allow(unused)]\n"), 0);
        assert_eq!(s("#!\n// c\n/* d /* e */ */ [x]"), 0);
        // A doc comment is a token: not `[`.
        assert_eq!(s("#!/// d\n[x]"), 7);
        assert_eq!(s("#!/** d */[x]"), 13);
        assert_eq!(s("#!/**/[x]"), 0);
        assert_eq!(s("#!/* open"), 9);
        assert_eq!(s(" #!/bin/rok"), 0);
        assert_eq!(s("x\n#!/bin/rok"), 0);
    }

    #[test]
    fn verbatim_windows_paths_get_their_plain_spelling() {
        let u = super::unverbatim;
        assert_eq!(
            u(r"\\?\C:\Users\é\a.rok").as_deref(),
            Some(r"C:\Users\é\a.rok")
        );
        assert_eq!(u(r"\\?\UNC\srv\share\x").as_deref(), Some(r"\\srv\share\x"));
        assert_eq!(u(r"\\?\Volume{1}\x"), None);
        assert_eq!(u(r"C:\x"), None);
    }

    #[test]
    fn command_help_takes_the_entry_and_its_continuations() {
        let usage = "usage: x\n\ncmds:\n  add <n>      add\n               more\n  add <n> --p  \
                     path\n  rm <n>       rm\n  a | b [x]    ab\n";
        assert_eq!(
            super::command_help(usage, "add").unwrap(),
            "  add <n>      add\n               more\n  add <n> --p  path\n"
        );
        assert_eq!(
            super::command_help(usage, "rm").unwrap(),
            "  rm <n>       rm\n"
        );
        assert_eq!(
            super::command_help(usage, "b").unwrap(),
            "  a | b [x]    ab\n"
        );
        assert!(super::command_help(usage, "x").is_none());
        assert!(super::command_help(usage, "n").is_none());
    }
}
