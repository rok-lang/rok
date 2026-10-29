//! Native packages: prebuilt C / C++ libraries, one archive per target,
//! described by `Native { .. }`. rok compiles and runs nothing from them:
//! it checks hashes, unpacks, and tells rokc where the libraries are.
//!
//! ```rokon
//! Native { name: "zlib", vers: "1.3.1", deps: { "other": Dep::native("^1") }, targets: { "aarch64-apple-darwin": Artifact { url: "https://example.com/zlib-1.3.1-aarch64-apple-darwin.tar.gz", sha256: "<hex>", links: { "z": "static" }, runtime: [] } }, yanked: false, license: "Zlib" }
//! ```
//!
//! An index has one line per version in `native/<shard>`; a
//! `Dep::native(url, sha256)` names a file with one value (artifact urls
//! may be relative to it). An archive (anything `tar` reads) holds `lib/`,
//! the `runtime` files copied beside executables (`bin/<name>.dll`) and
//! licenses.
//!
//! ```text
//! $ROK_HOME/cache/native/<sha256>/          an archive, unpacked after its hash matched
//! $ROK_HOME/cache/native/meta/<sha256>.rokon  a `Dep::native(url: ..)` file
//! ```

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use rokc_core::hash::{hex, sha256};
use rokc_rokon::{Decoder, Doc, Kind, Value};
use rokc_target::{ObjectFormat, TargetSpec};

use crate::manifest::{self, NativeDep, NativeSource};
use crate::registry;
use crate::semver::{Req, Version};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    /// `(name, static)`: what executables link (`-l`).
    pub links: Vec<(String, bool)>,
    /// Archive paths of the shared libraries executables load.
    pub runtime: Vec<String>,
}

/// One version of a native package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub vers: Version,
    pub deps: Vec<NativeDep>,
    /// Target triple -> its build.
    pub targets: Vec<(String, Artifact)>,
    pub yanked: bool,
    pub meta: Vec<(&'static str, String)>,
}

const META: [&str; 3] = ["description", "license", "repository"];

/// A native package of a build.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub name: String,
    pub entry: Entry,
    /// What relative artifact urls are relative to (a file's directory).
    pub base: Option<String>,
    /// The packages that need it (by key; `""` the root).
    pub users: Vec<String>,
}

/// A native package's file in an index.
pub fn path(name: &str) -> String {
    format!("native/{}", registry::path(name))
}

fn hash_hex(d: Decoder<'_>) -> Result<String, String> {
    let s = d.string()?;
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(d.error("a SHA-256 is 64 hex digits"));
    }
    Ok(s.to_ascii_lowercase())
}

/// A path inside an archive: relative, without `..`.
fn inner(d: Decoder<'_>) -> Result<String, String> {
    let s = d.string()?;
    let ok = !s.is_empty()
        && Path::new(&s)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if !ok {
        return Err(d.error("a path inside the archive (relative, no `..`)"));
    }
    Ok(s)
}

fn artifact(d: Decoder<'_>, strict: bool) -> Result<Artifact, String> {
    let r = d.record("Artifact")?;
    if strict {
        r.only(&["url", "sha256", "links", "runtime"])?;
    }
    let mut links = Vec::new();
    for (k, v) in r
        .opt("links")
        .map(|l| l.dict())
        .transpose()?
        .unwrap_or_default()
    {
        let stat = match v.string()?.as_str() {
            "static" => true,
            "dylib" => false,
            _ => return Err(v.error("a link kind is \"static\" or \"dylib\"")),
        };
        links.push((k.string()?, stat));
    }
    Ok(Artifact {
        url: r.req("url")?.string()?,
        sha256: hash_hex(r.req("sha256")?)?,
        links,
        runtime: r
            .opt("runtime")
            .map(|l| l.list())
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .map(inner)
            .collect::<Result<_, _>>()?,
    })
}

// `strict`: reject unknown fields. Index lines are read with it off, so
// fields a newer rok adds do not hide a package's other versions; a
// single file (`parse_one`) is someone's own and keeps it on.
fn entry(d: Decoder<'_>, strict: bool) -> Result<Entry, String> {
    let r = d.record("Native")?;
    if strict {
        r.only(&[
            "name",
            "vers",
            "deps",
            "targets",
            "yanked",
            "description",
            "license",
            "repository",
        ])?;
    }
    let name = r.req("name")?.string()?;
    manifest::check_name(&name).map_err(|e| r.field_error("name", &e))?;
    let vers = Version::parse(&r.req("vers")?.string()?).map_err(|e| r.field_error("vers", &e))?;
    let mut targets = Vec::new();
    for (k, v) in r.req("targets")?.dict()? {
        targets.push((k.string()?, artifact(v, strict)?));
    }
    Ok(Entry {
        name,
        vers,
        deps: r.opt("deps").map_or(Ok(Vec::new()), manifest::natives)?,
        targets,
        yanked: r
            .opt("yanked")
            .map(|y| y.bool())
            .transpose()?
            .unwrap_or(false),
        meta: META
            .iter()
            .filter_map(|k| Some(r.opt(k)?.string().map(|v| (*k, v))))
            .collect::<Result<_, _>>()?,
    })
}

/// The entries of an index file, one per line.
pub fn parse_file(text: &str, file: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if !line.trim().is_empty() {
            let at = format!("{file} (line {})", i + 1);
            out.push(entry(Doc::parse(line, &at)?.root(), false)?);
        }
    }
    Ok(out)
}

/// A file holding one `Native { .. }`.
pub fn parse_one(text: &str, file: &str) -> Result<Entry, String> {
    entry(Doc::parse(text, file)?.root(), true)
}

impl Entry {
    /// The index line (canonical ROKON on one line).
    pub fn to_line(&self) -> String {
        let deps = self
            .deps
            .iter()
            .map(|d| (Value::str(&d.name), d.source.to_value()))
            .collect();
        let targets = self
            .targets
            .iter()
            .map(|(t, a)| {
                let links = a
                    .links
                    .iter()
                    .map(|(n, s)| {
                        (
                            Value::str(n),
                            Value::str(if *s { "static" } else { "dylib" }),
                        )
                    })
                    .collect();
                let runtime = a.runtime.iter().map(Value::str).collect();
                let a = Value::record(
                    Some("Artifact"),
                    vec![
                        ("url", Value::str(&a.url)),
                        ("sha256", Value::str(&a.sha256)),
                        ("links", Value::new(Kind::Dict(links))),
                        ("runtime", Value::new(Kind::List(runtime))),
                    ],
                );
                (Value::str(t), a)
            })
            .collect();
        rokc_rokon::to_line(&Value::record(
            Some("Native"),
            [
                ("name", Value::str(&self.name)),
                ("vers", Value::str(self.vers.to_string())),
                ("deps", Value::new(Kind::Dict(deps))),
                ("targets", Value::new(Kind::Dict(targets))),
                ("yanked", Value::new(Kind::Bool(self.yanked))),
            ]
            .into_iter()
            .chain(self.meta.iter().map(|(k, v)| (*k, Value::str(v))))
            .collect(),
        ))
    }

    /// What `rok.lock` pins: the SHA-256 of the line (yanking aside).
    pub fn checksum(&self) -> String {
        let line = Self {
            yanked: false,
            ..self.clone()
        }
        .to_line();
        hex(&sha256(line.as_bytes()))
    }
}

/// The versions of native package `name` in the cached index `url`.
pub fn entries(cache: &crate::git::Cache, url: &str, name: &str) -> Result<Vec<Entry>, String> {
    let Some((file, text)) = registry::index_file(cache, url, "native/", name)? else {
        return Ok(Vec::new());
    };
    let mut all = parse_file(&text, &format!("{url}: {file}"))?;
    all.retain(|e| manifest::same(&e.name, name));
    Ok(all)
}

pub fn best<'a>(entries: &'a [Entry], req: &Req) -> Option<&'a Entry> {
    entries
        .iter()
        .filter(|e| !e.yanked && req.matches(&e.vers))
        .max_by(|a, b| a.vers.cmp(&b.vers))
}

pub fn no_match(
    cache: &crate::git::Cache,
    entries: &[Entry],
    name: &str,
    req: &str,
    url: &str,
) -> String {
    if entries.is_empty() {
        let help = registry::missing(cache, url, name, true);
        return format!("no native package named `{name}` in {url}{help}");
    }
    let vs: Vec<String> = entries
        .iter()
        .map(|e| format!("{}{}", e.vers, if e.yanked { " (yanked)" } else { "" }))
        .collect();
    format!(
        "no version of native package `{name}` matches `{req}` (in {url}: {})",
        vs.join(", ")
    )
}

fn remote(loc: &str) -> bool {
    loc.starts_with("https://") || loc.starts_with("http://")
}

/// `rel` (a url or a path) against `base` (a directory or url prefix).
pub fn locate(base: Option<&str>, rel: &str) -> Result<String, String> {
    if rel.contains("://") || Path::new(rel).is_absolute() {
        return Ok(rel.to_owned());
    }
    match base {
        Some(b) if b.contains("://") => Ok(format!("{}/{rel}", b.trim_end_matches('/'))),
        Some(b) => Ok(Path::new(b).join(rel).display().to_string()),
        None => Err(format!(
            "`{rel}` is relative, but an index entry's urls must be absolute"
        )),
    }
}

/// The directory (or url prefix) holding `loc`.
pub fn dir_of(loc: &str) -> String {
    match loc.rsplit_once('/') {
        Some((d, _)) if loc.contains("://") => d.to_owned(),
        _ => Path::new(loc)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .display()
            .to_string(),
    }
}

/// Bytes at `loc`: `http(s)://` via curl (into `tmp`), else a file.
fn fetch(loc: &str, tmp: &Path) -> Result<Vec<u8>, String> {
    let file = if remote(loc) {
        let out = Command::new("curl")
            .args(["-fsSL", "--proto", "=https,http", "-o"])
            .arg(tmp)
            .arg(loc)
            .output()
            .map_err(|e| format!("cannot run `curl` (is it installed?): {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "cannot download {loc}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        tmp.to_path_buf()
    } else {
        let p = loc.strip_prefix("file://").unwrap_or(loc);
        // `file:///C:/x` is `C:/x`.
        let drive = p.len() > 2 && p.starts_with('/') && p.as_bytes()[2] == b':';
        PathBuf::from(if cfg!(windows) && drive { &p[1..] } else { p })
    };
    std::fs::read(&file).map_err(|e| format!("cannot read {loc}: {e}"))
}

fn check_hash(bytes: &[u8], want: &str, what: &str, loc: &str) -> Result<(), String> {
    let got = hex(&sha256(bytes));
    if got == want {
        return Ok(());
    }
    Err(format!(
        "{what}: {loc} has SHA-256 {got}, but {want} is expected (the file changed, or the \
         hash is wrong)"
    ))
}

fn cache(home: &Path) -> PathBuf {
    home.join("cache/native")
}

/// A scratch path in the cache, unique to this process.
fn scratch(home: &Path, what: &str) -> Result<PathBuf, String> {
    let dir = cache(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir.join(format!(".tmp-{what}-{}", std::process::id())))
}

/// The `Native { .. }` file a direct dep names: cached, else fetched and
/// checked.
pub fn direct(
    home: &Path,
    name: &str,
    loc: &str,
    want: &str,
    offline: bool,
) -> Result<Entry, String> {
    let kept = cache(home).join("meta").join(format!("{want}.rokon"));
    let text = if let Ok(t) = std::fs::read_to_string(&kept) {
        t
    } else {
        if offline && remote(loc) {
            return Err(format!(
                "native package `{name}` needs {loc}, but --offline is set (it is not in the cache)"
            ));
        }
        let tmp = scratch(home, want)?;
        let bytes = fetch(loc, &tmp);
        let _ = std::fs::remove_file(&tmp);
        let bytes = bytes?;
        check_hash(&bytes, want, &format!("native package `{name}`"), loc)?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{loc} is not UTF-8"))?;
        std::fs::create_dir_all(kept.parent().unwrap_or(&kept))
            .and_then(|()| std::fs::write(&tmp, &text))
            .and_then(|()| std::fs::rename(&tmp, &kept))
            .map_err(|e| format!("{}: {e}", kept.display()))?;
        text
    };
    let e = parse_one(&text, loc)?;
    if !manifest::same(&e.name, name) {
        return Err(format!(
            "{loc} describes native package `{}`, not `{name}`",
            e.name
        ));
    }
    Ok(e)
}

/// `name`'s files for target `t`, unpacked in the cache once after the
/// hash and contents checked out.
pub fn unpack(
    home: &Path,
    n: &Resolved,
    t: &TargetSpec,
    offline: bool,
    quiet: bool,
) -> Result<(PathBuf, Artifact), String> {
    let triple = &t.name;
    let e = &n.entry;
    let Some((_, a)) = e.targets.iter().find(|(name, _)| name == triple) else {
        let have: Vec<&str> = e.targets.iter().map(|(name, _)| name.as_str()).collect();
        return Err(format!(
            "native package `{}` {} has no build for {triple} (it has: {})",
            e.name,
            e.vers,
            if have.is_empty() {
                "none".to_owned()
            } else {
                have.join(", ")
            }
        ));
    };
    let dir = cache(home).join(&a.sha256);
    if dir.is_dir() {
        return Ok((dir, a.clone()));
    }
    let what = format!("native package `{}` {} ({triple})", e.name, e.vers);
    let loc = locate(n.base.as_deref(), &a.url).map_err(|m| format!("{what}: {m}"))?;
    if offline && remote(&loc) {
        return Err(format!(
            "{what} needs {loc}, but --offline is set (it is not in the cache)"
        ));
    }
    if !quiet && remote(&loc) {
        rokc_core::term::status("Downloading", &format!("{} {} ({triple})", e.name, e.vers));
    }
    let tmp = scratch(home, &a.sha256)?;
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let done = (|| {
        let archive = tmp.join("archive");
        let bytes = fetch(&loc, &archive)?;
        check_hash(&bytes, &a.sha256, &what, &loc)?;
        std::fs::write(&archive, &bytes).map_err(|e| format!("{}: {e}", archive.display()))?;
        let files = tmp.join("files");
        extract(&archive, &files).map_err(|m| format!("{what}: {m}"))?;
        check(&files, a, t).map_err(|m| format!("{what}: {m}"))?;
        // Another process may have won the race: theirs is as good.
        if std::fs::rename(&files, &dir).is_err() && !dir.is_dir() {
            return Err(format!("cannot create {}", dir.display()));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    done.map(|()| (dir, a.clone()))
}

/// `tar -xf` into `to`, refusing absolute or `..` paths.
fn extract(archive: &Path, to: &Path) -> Result<(), String> {
    let tar = |args: &[&std::ffi::OsStr]| {
        let out = Command::new("tar")
            .args(args)
            .output()
            .map_err(|e| format!("cannot run `tar`: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "`tar` cannot read the archive: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let list = tar(&["-tf".as_ref(), archive.as_os_str()])?;
    for f in list.lines() {
        let p = Path::new(f);
        if p.has_root()
            || p.components()
                .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(format!("the archive has a path outside it: `{f}`"));
        }
    }
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    tar(&[
        "-xf".as_ref(),
        archive.as_os_str(),
        "-C".as_ref(),
        to.as_os_str(),
    ])
    .map(drop)
}

/// The file a `stat` link needs on `t` (COFF links a DLL through its
/// import library).
fn lib_file(name: &str, stat: bool, t: &TargetSpec) -> String {
    if stat || t.object_format == ObjectFormat::Coff {
        format!("lib/{}", t.staticlib(name))
    } else {
        format!("lib/{}", t.dylib(name))
    }
}

/// An unpacked archive has what `a` names, and no link leaves it.
fn check(files: &Path, a: &Artifact, t: &TargetSpec) -> Result<(), String> {
    fn walk(dir: &Path, depth: usize, root: &Path) -> Result<(), String> {
        for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let p = e.map_err(|e| e.to_string())?.path();
            let meta = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
            let shown = || p.strip_prefix(root).unwrap_or(&p).display().to_string();
            if meta.is_symlink() {
                let to = std::fs::read_link(&p).map_err(|e| e.to_string())?;
                let mut d = depth as isize;
                for c in to.components() {
                    d += match c {
                        Component::Normal(_) => 1,
                        Component::CurDir => 0,
                        Component::ParentDir => -1,
                        _ => -1000,
                    };
                    if d < 0 {
                        return Err(format!(
                            "the archive's link `{}` points outside it",
                            shown()
                        ));
                    }
                }
            } else if meta.is_dir() {
                walk(&p, depth + 1, root)?;
            } else if !meta.is_file() {
                return Err(format!("the archive's `{}` is not a file", shown()));
            }
        }
        Ok(())
    }
    walk(files, 0, files)?;
    for (name, stat) in &a.links {
        let f = lib_file(name, *stat, t);
        if !files.join(&f).is_file() {
            return Err(format!("link `{name}` needs {f}, which the archive lacks"));
        }
    }
    for r in &a.runtime {
        let p = files.join(r);
        if !p.is_file() {
            return Err(format!("runtime file {r} is not in the archive"));
        }
        let file = Path::new(r)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        // ELF executables name a library by soname and look for it beside
        // themselves.
        if t.object_format == ObjectFormat::Elf && file.contains(".so") {
            let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            match soname(&bytes) {
                Some(n) if n == file => {}
                n => {
                    return Err(format!(
                        "{r}'s soname is {}; a shared library beside the executable needs its own \
                         file name (link it with `-Wl,-soname,{file}`)",
                        n.map_or_else(|| "missing".to_owned(), |n| format!("`{n}`")),
                    ));
                }
            }
        }
        if t.object_format == ObjectFormat::MachO && p.extension().is_some_and(|x| x == "dylib") {
            let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            match install_name(&bytes) {
                Some(n) if n.starts_with("@rpath/") => {}
                n => {
                    return Err(format!(
                        "{r}'s install name is {}; a shared library beside the executable needs \
                         `@rpath/<file>` (link it with `-install_name @rpath/{file}`)",
                        n.map_or_else(|| "missing".to_owned(), |n| format!("`{n}`")),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `DT_SONAME` of a little-endian ELF shared library.
fn soname(b: &[u8]) -> Option<String> {
    let n = |o: usize, w: usize| -> Option<usize> {
        let mut v = 0u64;
        for (i, x) in b.get(o..o + w)?.iter().enumerate() {
            v |= u64::from(*x) << (8 * i);
        }
        usize::try_from(v).ok()
    };
    if !b.starts_with(b"\x7fELF") || b.get(5) != Some(&1) {
        return None;
    }
    // Word size, and section header field offsets: offset, size, link.
    let (w, sh) = match b.get(4)? {
        1 => (4, [0x10, 0x14, 0x18]),
        2 => (8, [0x18, 0x20, 0x28]),
        _ => return None,
    };
    let (shoff, shentsize, shnum) = if w == 4 {
        (n(0x20, 4)?, n(0x2e, 2)?, n(0x30, 2)?)
    } else {
        (n(0x28, 8)?, n(0x3a, 2)?, n(0x3c, 2)?)
    };
    let section = |i: usize| -> Option<(u32, usize, usize, usize)> {
        let h = shoff + i * shentsize;
        let ty = u32::try_from(n(h + 4, 4)?).ok()?;
        Some((ty, n(h + sh[0], w)?, n(h + sh[1], w)?, n(h + sh[2], 4)?))
    };
    for i in 0..shnum {
        let (ty, off, size, link) = section(i)?;
        // SHT_DYNAMIC
        if ty != 6 {
            continue;
        }
        let (_, strs, strs_size, _) = section(link)?;
        for e in (off..off + size).step_by(2 * w) {
            match n(e, w)? {
                0 => break,
                // DT_SONAME
                14 => {
                    let at = n(e + w, w)?;
                    let s = b.get(strs + at..strs + strs_size)?;
                    let s = s.split(|c| *c == 0).next()?;
                    return Some(String::from_utf8_lossy(s).into_owned());
                }
                _ => {}
            }
        }
    }
    None
}

/// A Mach-O dylib's `LC_ID_DYLIB` name (a fat file's first slice).
fn install_name(b: &[u8]) -> Option<String> {
    let u32_at = |o: usize, be: bool| {
        let w: [u8; 4] = b.get(o..o + 4)?.try_into().ok()?;
        Some(if be {
            u32::from_be_bytes(w)
        } else {
            u32::from_le_bytes(w)
        } as usize)
    };
    let base = if b.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) {
        u32_at(16, true)?
    } else {
        0
    };
    let header = match u32_at(base, false)? {
        0xfeed_facf => 32,
        0xfeed_face => 28,
        _ => return None,
    };
    let mut at = base + header;
    for _ in 0..u32_at(base + 16, false)? {
        let size = u32_at(at + 4, false)?;
        // LC_ID_DYLIB
        if u32_at(at, false)? == 0xd {
            let s = b.get(at + u32_at(at + 8, false)?..at + size)?;
            let s = s.split(|c| *c == 0).next()?;
            return Some(String::from_utf8_lossy(s).into_owned());
        }
        at += size.max(8);
    }
    None
}

/// rokc arguments linking `natives` (dependents first) for `t`.
pub fn rokc_args(
    home: &Path,
    natives: &[&Resolved],
    t: &TargetSpec,
    offline: bool,
    quiet: bool,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for n in natives {
        // Static executables load no shared library.
        if !t.dynamic_linking {
            if let Some((_, a)) = n.entry.targets.iter().find(|(name, _)| *name == t.name) {
                if let Some((l, _)) = a.links.iter().find(|(_, stat)| !stat) {
                    return Err(format!(
                        "native package `{}` {} links `{l}` as a shared library, but {} \
                         executables are static (it needs \"static\" links)",
                        n.entry.name, n.entry.vers, t.name
                    ));
                }
            }
        }
        let (dir, a) = unpack(home, n, t, offline, quiet)?;
        out.push(format!("-L{}", dir.join("lib").display()));
        for (name, stat) in &a.links {
            out.push(format!(
                "-l{}={name}",
                if *stat { "static" } else { "dylib" }
            ));
        }
        for r in &a.runtime {
            out.push(format!("--native-runtime={}", dir.join(r).display()));
        }
    }
    Ok(out)
}

/// `rok publish --native <file>`: fetch and check every build (licenses
/// too), against `index` if given; `write` commits there.
pub fn publish(
    home: &Path,
    file: &Path,
    index: Option<&Path>,
    write: bool,
) -> Result<(Entry, Option<PathBuf>), String> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let mut e = parse_one(&text, &file.display().to_string())?;
    e.yanked = false;
    if e.targets.is_empty() {
        return Err(format!("native package `{}` has no `targets`", e.name));
    }
    for d in &e.deps {
        if let NativeSource::Direct { .. } = d.source {
            return Err(format!(
                "dependency `{}` is not in an index: an indexed native package depends on \
                 `Dep::native(\"<req>\")` ones",
                d.name
            ));
        }
    }
    for (t, a) in &e.targets {
        if !a.url.contains("://") {
            return Err(format!(
                "{t}: url `{}` must be absolute (https:// or file://) in an index",
                a.url
            ));
        }
        let n = Resolved {
            name: e.name.clone(),
            entry: e.clone(),
            base: None,
            users: Vec::new(),
        };
        let spec = TargetSpec::builtin(t).ok_or_else(|| format!("{t}: not a built-in target"))?;
        let (dir, _) = unpack(home, &n, &spec, false, true)?;
        let license = std::fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .any(|f| {
                let n = f.file_name().to_string_lossy().to_ascii_uppercase();
                ["LICENSE", "LICENCE", "COPYING", "NOTICE"]
                    .iter()
                    .any(|p| n.starts_with(p))
            });
        if !license {
            return Err(format!(
                "{t}: the archive has no license file (LICENSE*, COPYING*, NOTICE* or \
                 LICENSES/ at its top)"
            ));
        }
    }
    let Some(index) = index else {
        return Ok((e, None));
    };
    let (path, have_text) = registry::local(index, &path(&e.name))?;
    let have = parse_file(&have_text, &path.display().to_string())?;
    registry::taken(
        index,
        "native/",
        &e.name,
        have.iter().map(|o| o.name.clone()),
    )?;
    if have.iter().any(|o| o.vers == e.vers) {
        return Err(format!(
            "native package {} {} is already in the index",
            e.name, e.vers
        ));
    }
    if write {
        let mut out = have_text;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&e.to_line());
        out.push('\n');
        registry::commit(
            index,
            &path,
            &out,
            &format!("Publish native {} {}", e.name, e.vers),
        )?;
    }
    Ok((e, Some(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A newer rok's fields: skipped in an index line, rejected in a file
    /// of one's own.
    #[test]
    fn unknown_fields() {
        let line = format!(
            "Native {{ name: \"zz\", vers: \"1.0.0\", targets: {{ \"aarch64-apple-darwin\": \
             Artifact {{ url: \"https://x/zz.tar.gz\", sha256: \"{}\" }} }}, yanked: false }}",
            "ab".repeat(32)
        );
        let e = parse_file(&line, "f").unwrap();
        let abi = line.replace("\" } }", "\", abi: \"c\" } }");
        let newer = abi.replace("false }", "false, keywords: [\"z\"] }");
        assert!(abi != line && newer != abi);
        assert_eq!(parse_file(&newer, "f").unwrap(), e);
        let err = parse_one(&newer, "f").unwrap_err();
        assert!(err.contains("unknown field `keywords`"), "{err}");
        let err = parse_one(&abi, "f").unwrap_err();
        assert!(err.contains("unknown field `abi`"), "{err}");
    }

    #[test]
    fn lines_paths_and_names() {
        let h = "ab".repeat(32);
        let line = format!(
            "Native {{ name: \"zz\", vers: \"1.0.0\", deps: {{ \"yy\": Dep::native(\"^1.0\") }}, \
             targets: {{ \"aarch64-apple-darwin\": Artifact {{ url: \"https://x/zz.tar.gz\", \
             sha256: \"{h}\", links: {{ \"zz\": \"static\" }}, runtime: [] }} }}, yanked: false, \
             license: \"MIT\" }}"
        );
        let e = &parse_file(&format!("\n{line}\n"), "f").unwrap()[0];
        assert_eq!(e.to_line(), line);
        assert_eq!(e.targets[0].1.links, [("zz".to_owned(), true)]);
        assert_eq!(
            e.checksum(),
            Entry {
                yanked: true,
                ..e.clone()
            }
            .checksum()
        );
        assert_eq!(path("zz"), "native/2/zz");
        for (bad, want) in [
            (line.replace("static", "shared"), "\"static\" or \"dylib\""),
            (line.replace(&h, "ab"), "64 hex digits"),
            (line.replace("[]", "[\"../x\"]"), "relative, no `..`"),
            (line.replace("targets", "builds"), "missing field `targets`"),
        ] {
            let e = parse_file(&bad, "f").unwrap_err();
            assert!(e.contains(want), "{e}");
        }
        assert_eq!(
            locate(Some("https://h/a"), "z.tgz").unwrap(),
            "https://h/a/z.tgz"
        );
        assert_eq!(
            locate(Some("/v"), "z.tgz").unwrap(),
            Path::new("/v").join("z.tgz").display().to_string()
        );
        assert_eq!(locate(None, "file:///z").unwrap(), "file:///z");
        assert!(locate(None, "z.tgz").is_err());
        assert_eq!(dir_of("https://h/a/m.rokon"), "https://h/a");
        assert_eq!(dir_of("/v/m.rokon"), "/v");
        let t = |n| TargetSpec::builtin(n).unwrap();
        assert_eq!(
            lib_file("z", false, &t("x86_64-pc-windows-msvc")),
            "lib/z.lib"
        );
        assert_eq!(
            lib_file("z", false, &t("x86_64-unknown-linux-gnu")),
            "lib/libz.so"
        );
        assert_eq!(
            lib_file("z", true, &t("aarch64-apple-darwin")),
            "lib/libz.a"
        );
        assert_eq!(
            lib_file("z", false, &t("aarch64-apple-darwin")),
            "lib/libz.dylib"
        );
        // A minimal 64-bit Mach-O with LC_ID_DYLIB `@rpath/libz.dylib`.
        let name = b"@rpath/libz.dylib\0\0\0\0\0\0\0";
        let mut m = Vec::new();
        for w in [0xfeed_facf_u32, 0, 0, 6, 1, 24 + name.len() as u32, 0, 0] {
            m.extend(w.to_le_bytes());
        }
        for w in [0xd_u32, 24 + name.len() as u32, 24, 0, 0, 0] {
            m.extend(w.to_le_bytes());
        }
        m.extend(name);
        assert_eq!(install_name(&m).as_deref(), Some("@rpath/libz.dylib"));
        assert_eq!(install_name(b"\x7fELF"), None);
        // A minimal ELF64: sections null, `.dynamic` (DT_SONAME 1),
        // `.dynstr` ("\0libz.so\0").
        let mut e = vec![0u8; 304];
        let put = |e: &mut Vec<u8>, at: usize, v: u64, w: usize| {
            e[at..at + w].copy_from_slice(&v.to_le_bytes()[..w]);
        };
        e[..6].copy_from_slice(b"\x7fELF\x02\x01");
        put(&mut e, 0x28, 64, 8);
        put(&mut e, 0x3a, 64, 2);
        put(&mut e, 0x3c, 3, 2);
        for (i, ty, off, size, link) in [(1, 6, 272, 32, 2), (2, 3, 256, 9, 0)] {
            let h = 64 + i * 64;
            put(&mut e, h + 4, ty, 4);
            put(&mut e, h + 0x18, off, 8);
            put(&mut e, h + 0x20, size, 8);
            put(&mut e, h + 0x28, link, 4);
        }
        e[257..264].copy_from_slice(b"libz.so");
        put(&mut e, 272, 14, 8);
        put(&mut e, 280, 1, 8);
        assert_eq!(soname(&e).as_deref(), Some("libz.so"));
        put(&mut e, 272, 1, 8);
        assert_eq!(soname(&e), None);
        assert_eq!(soname(&m), None);
    }

    #[test]
    fn musl_refuses_shared_links_before_fetching() {
        let t = "x86_64-unknown-linux-musl";
        let line = format!(
            "Native {{ name: \"zz\", vers: \"1.0.0\", targets: {{ \"{t}\": Artifact {{ url: \
             \"https://nowhere.invalid/zz.tgz\", sha256: \"{}\", links: {{ \"zz\": \"dylib\" }} \
             }} }} }}",
            "ab".repeat(32)
        );
        let n = Resolved {
            name: "zz".into(),
            entry: parse_one(&line, "f").unwrap(),
            base: None,
            users: Vec::new(),
        };
        let home = Path::new("/nonexistent-rok-home");
        let spec = TargetSpec::builtin(t).unwrap();
        let e = rokc_args(home, &[&n], &spec, true, true).unwrap_err();
        assert!(e.contains("links `zz` as a shared library"), "{e}");
    }
}
