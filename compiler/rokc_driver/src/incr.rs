//! Incremental compilation below the package (spec 9.2.16). A store is a
//! file of entries by key in the incremental directory; a build reads what
//! it needs and writes back what it used or made. Spans keep their file
//! paths and are renumbered on read; an entry that doesn't read back is a
//! miss, computed afresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rokc_core::codec::{Codec, Dec, Enc};
/// An entry key: a hash of the parts.
pub fn key(parts: &[&[u8]]) -> u64 {
    parts.iter().fold(0, |h, p| rokc_core::hash::fast_add(h, p))
}

/// One store file.
pub struct Store {
    path: PathBuf,
    /// The file read at the start, and its entries (key → range).
    data: std::sync::Arc<[u8]>,
    index: HashMap<u64, (usize, usize)>,
    /// Entries to write back: read ones (their range) and new ones.
    kept: Mutex<Vec<(u64, Kept)>>,
}

enum Kept {
    Old(usize, usize),
    New(Vec<u8>),
}

impl Store {
    /// An empty store to be written at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            data: std::sync::Arc::from([]),
            index: HashMap::new(),
            kept: Mutex::new(Vec::new()),
        }
    }

    /// The store at `path` (empty when missing, of another compiler or
    /// unreadable). `magic` names the format.
    pub fn open(path: PathBuf, magic: &str) -> Self {
        let mut s = Self::new(path);
        let head = header(magic);
        let Ok(data) = std::fs::read(&s.path) else {
            return s;
        };
        if !data.starts_with(&head) {
            return s;
        }
        let mut d = Dec::new(&data[head.len()..]);
        let mut index = HashMap::new();
        while !d.done() {
            let (Some(k), Some(b)) = (d.u(), d.bytes()) else {
                return s;
            };
            let start = b.as_ptr() as usize - data.as_ptr() as usize;
            index.insert(k, (start, start + b.len()));
        }
        s.data = data.into();
        s.index = index;
        s
    }

    /// The value at `k`, read with spans renumbered for this build.
    pub fn get<T: Codec>(&self, k: u64, ids: &Ids) -> Option<T> {
        self.get_with(k, ids, T::dec)
    }

    /// The value at `k` as `read` reads it.
    pub fn get_with<T>(
        &self,
        k: u64,
        ids: &Ids,
        read: impl FnOnce(&mut Dec<'_>) -> Option<T>,
    ) -> Option<T> {
        let &(a, b) = self.index.get(&k)?;
        let v = read_value(&self.data[a..b], ids, None, read)?;
        self.lock().push((k, Kept::Old(a, b)));
        Some(v)
    }

    /// `get_with`, the `Lazy` values left to read when first used: the
    /// file stays in memory while one is not.
    pub fn get_kept<T>(
        &self,
        k: u64,
        ids: &Ids,
        read: impl FnOnce(&mut Dec<'_>) -> Option<T>,
    ) -> Option<T> {
        let &(a, b) = self.index.get(&k)?;
        let v = read_value(&self.data[a..b], ids, Some(&self.data), read)?;
        self.lock().push((k, Kept::Old(a, b)));
        Some(v)
    }

    /// Keep `v` at `k`.
    pub fn put<T: Codec>(&self, k: u64, v: &T, ids: &Ids) {
        self.put_with(k, ids, |e| {
            v.enc(e);
            true
        });
    }

    /// Keep what `write` writes at `k`, unless it gives `false`.
    pub fn put_with(&self, k: u64, ids: &Ids, write: impl FnOnce(&mut Enc) -> bool) {
        let mut e = Enc::new(true);
        if !write(&mut e) {
            return;
        }
        let mut out = Enc::new(true);
        e.files
            .iter()
            .filter_map(|&id| Some((id, ids.paths.get((id as usize).checked_sub(1)?)?.clone())))
            .collect::<Vec<_>>()
            .enc(&mut out);
        out.buf.extend_from_slice(&e.buf);
        self.lock().push((k, Kept::New(out.buf)));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(u64, Kept)>> {
        self.kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write the kept entries back (atomically: a concurrent build reads
    /// the old file or the new one); not when they are the file's.
    pub fn save(&self, magic: &str) {
        let kept = std::mem::take(&mut *self.lock());
        let old: std::collections::HashSet<u64> = kept
            .iter()
            .filter_map(|(k, v)| matches!(v, Kept::Old(..)).then_some(*k))
            .collect();
        if kept.iter().all(|(_, v)| matches!(v, Kept::Old(..))) && old.len() == self.index.len() {
            return;
        }
        let mut e = Enc::new(true);
        e.buf.extend_from_slice(&header(magic));
        // The last entry at a key wins (a read one, then its new value).
        let mut seen = std::collections::HashSet::new();
        for (k, v) in kept.iter().rev() {
            if !seen.insert(*k) {
                continue;
            }
            e.u(*k);
            e.bytes(match v {
                Kept::Old(a, b) => &self.data[*a..*b],
                Kept::New(b) => b,
            });
        }
        let Some(dir) = self.path.parent() else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = self.path.with_extension(format!(
            "tmp{}.{}",
            std::process::id(),
            TMP.fetch_add(1, Ordering::Relaxed)
        ));
        if std::fs::write(&tmp, &e.buf).is_ok() && std::fs::rename(&tmp, &self.path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Numbers each store write's temporary file: builds on other threads of
/// the process write the same store too.
static TMP: AtomicUsize = AtomicUsize::new(0);

fn header(magic: &str) -> Vec<u8> {
    format!("{magic}\n{}\n", crate::rokhir::producer()).into_bytes()
}

/// The files of a compile: path by id and id by path.
pub struct Ids {
    paths: Vec<String>,
    by_path: HashMap<String, u32>,
}

impl Ids {
    pub fn of(files: &rokc_diagnostic::SourceMap) -> Self {
        let paths: Vec<String> = files.files().iter().map(|(p, _)| p.clone()).collect();
        let by_path = paths
            .iter()
            .enumerate()
            .filter_map(|(i, p)| Some((p.clone(), u32::try_from(i + 1).ok()?)))
            .collect();
        Self { paths, by_path }
    }
}

/// An entry's value: its files (old id, path), then the value.
fn read_value<T>(
    bytes: &[u8],
    ids: &Ids,
    keep: Option<&std::sync::Arc<[u8]>>,
    read: impl FnOnce(&mut Dec<'_>) -> Option<T>,
) -> Option<T> {
    let mut d = Dec::new(bytes);
    let files = Vec::<(u32, String)>::dec(&mut d)?;
    let mut map = vec![0u32; files.iter().map(|f| f.0 as usize + 1).max().unwrap_or(0)];
    for (old, path) in &files {
        map[*old as usize] = *ids.by_path.get(path)?;
    }
    let mut d = Dec::new(&bytes[bytes.len() - d.rest()..]);
    d.files = Some(&map);
    d.keep = keep.map(|buf| rokc_core::codec::Keep {
        buf: buf.clone(),
        files: Some(map.as_slice().into()),
    });
    let v = read(&mut d)?;
    d.done().then_some(v)
}

thread_local! {
    /// This compile's stores (`CompilerConfig::incremental`).
    static INCR: std::cell::RefCell<Option<std::sync::Arc<Incr>>> =
        const { std::cell::RefCell::new(None) };
}

/// The incremental state of one compile.
pub struct Incr {
    pub parse: Store,
    pub bodies: Store,
    /// Files whose parse was read from the store, and all files.
    pub parsed_reused: AtomicUsize,
    pub parsed: AtomicUsize,
    /// `HirModule::reuse` of the compile.
    pub body_reuse: std::sync::Mutex<(usize, usize)>,
    /// The library's stage was read back.
    pub library_reused: std::sync::atomic::AtomicBool,
}

/// What a compile took from the incremental stores (`-v`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Reuse {
    /// Files parsed: from the store, and in all.
    pub files: (usize, usize),
    /// Bodies lowered: replayed, and in all.
    pub bodies: (usize, usize),
    /// The library's collected stage was read back.
    pub library: bool,
}

impl Incr {
    /// What HIR lowering replayed of what it lowered.
    pub fn bodies_reused(&self, replayed: usize, lowered: usize) {
        *self
            .body_reuse
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (replayed, lowered);
    }

    /// Count a file's parse, `reused` or not.
    pub fn count_parse(&self, reused: bool) {
        self.parsed.fetch_add(1, Ordering::Relaxed);
        if reused {
            self.parsed_reused.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Library stages and pre-pass stores unused this long go when another
/// is written.
const LIBRARY_KEPT: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Folder under `cache_dir` with the library's collected stages, one file
/// per key, shared by every build of the toolchain.
pub const LIBRARY_STAGES: &str = "library";

fn library_path(k: u64) -> Option<PathBuf> {
    Some(
        crate::cache_dir()?
            .join(LIBRARY_STAGES)
            .join(format!("library-{k:016x}.rokcol")),
    )
}

/// Store of the library files' pre-pass summaries for `roots` (one file
/// per root set), in the stages' folder.
pub fn library_prepass(roots: &str) -> Option<Store> {
    let k = key(&[roots.as_bytes()]);
    let dir = crate::cache_dir()?.join(LIBRARY_STAGES);
    Some(Store::open(
        dir.join(format!("prepass-{k:016x}.rokast")),
        PARSE,
    ))
}

/// Write back pre-pass store `s` of `library_prepass`, marked used.
pub fn library_prepass_save(s: &Store) {
    s.save(PARSE);
    used(&s.path);
    sweep(&s.path);
}

/// Mark the file at `path` used: kept from `sweep`.
fn used(path: &Path) {
    if let Ok(f) = std::fs::File::options().append(true).open(path) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

/// Remove the stages and pre-pass stores beside `path` unused for a
/// while.
fn sweep(path: &Path) {
    let Some(Ok(dir)) = path.parent().map(std::fs::read_dir) else {
        return;
    };
    for e in dir.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > LIBRARY_KEPT);
        let ours = (name.starts_with("library-") && name.ends_with(".rokcol"))
            || (name.starts_with("prepass-") && name.ends_with(".rokast"));
        if p != path && old && ours {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// The library stage of key `k` as `read` reads it.
pub fn library_get<T>(
    k: u64,
    ids: &Ids,
    read: impl FnOnce(&mut Dec<'_>) -> Option<T>,
) -> Option<T> {
    let path = library_path(k)?;
    let v = Store::open(path.clone(), LIBRARY).get_kept(k, ids, read)?;
    used(&path);
    Some(v)
}

/// Keep what `write` writes as the library stage of key `k`; the
/// stages not used for a while go.
pub fn library_put(k: u64, ids: &Ids, write: impl FnOnce(&mut Enc) -> bool) {
    let Some(path) = library_path(k) else {
        return;
    };
    let store = Store::new(path.clone());
    store.put_with(k, ids, write);
    store.save(LIBRARY);
    sweep(&path);
}

pub const PARSE: &str = "ROKAST";
const BODIES: &str = "ROKBODY";
const LIBRARY: &str = "ROKCOL";

impl Incr {
    fn open(dir: &Path) -> Self {
        Self {
            parse: Store::open(dir.join("parse.rokast"), PARSE),
            bodies: Store::open(dir.join("bodies.rokbody"), BODIES),
            parsed_reused: AtomicUsize::new(0),
            parsed: AtomicUsize::new(0),
            body_reuse: std::sync::Mutex::new((0, 0)),
            library_reused: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A failed build keeps the bodies of the last one: it may not
    /// have lowered (and so kept) them.
    fn save(&self, ok: bool) {
        self.parse.save(PARSE);
        if ok {
            self.bodies.save(BODIES);
        }
    }
}

/// Run `f` with the stores of `dir` (none: no caching), then save them
/// (`ok`: whether it succeeded); what was reused.
pub fn with<T>(
    dir: Option<&Path>,
    f: impl FnOnce() -> T,
    ok: impl FnOnce(&T) -> bool,
) -> (T, Option<Reuse>) {
    let incr = dir.map(|d| std::sync::Arc::new(Incr::open(d)));
    INCR.with(|i| i.borrow_mut().clone_from(&incr));
    let r = f();
    INCR.with(|i| *i.borrow_mut() = None);
    let ok = ok(&r);
    let reuse = incr.map(|i| {
        i.save(ok);
        Reuse {
            files: (
                i.parsed_reused.load(Ordering::Relaxed),
                i.parsed.load(Ordering::Relaxed),
            ),
            bodies: *i
                .body_reuse
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            library: i.library_reused.load(Ordering::Relaxed),
        }
    });
    (r, reuse)
}

/// This compile's stores.
pub fn current() -> Option<std::sync::Arc<Incr>> {
    INCR.with(|i| i.borrow().clone())
}

/// What of the target and the build's flags a cached result depends on:
/// the target's spec and the cfg.
pub fn target_key(cfg: &crate::CfgEnv, target: &rokc_mir::Target) -> String {
    format!("{:016x}|{cfg:?}", target.hash())
}

/// The body store of a compile, for HIR lowering.
struct BodyStore(std::sync::Arc<Incr>, Ids);

impl rokc_hir::BodyCache for BodyStore {
    fn get(&self, key: u64) -> Option<rokc_hir::Unit> {
        self.0.bodies.get(key, &self.1)
    }
    fn put(&self, key: u64, unit: &rokc_hir::Unit) {
        self.0.bodies.put(key, unit, &self.1);
    }
    fn get_check(&self, key: u64) -> Option<Vec<rokc_diagnostic::RokDiagnostic>> {
        self.0.bodies.get(key, &self.1)
    }
    fn put_check(&self, key: u64, errors: &[rokc_diagnostic::RokDiagnostic]) {
        self.0.bodies.put(key, &errors.to_vec(), &self.1);
    }
}

pub fn bodies(i: std::sync::Arc<Incr>, files: &rokc_diagnostic::SourceMap) -> rokc_hir::Bodies {
    rokc_hir::Bodies(std::sync::Arc::new(BodyStore(i, Ids::of(files))))
}

/// `ROK_INCREMENTAL_VERIFY=1`: lower with and without the stores and compare;
/// a difference fails the build.
pub fn verify() -> Option<fn(&rokc_hir::HirModule, &mut rokc_hir::HirModule)> {
    rokc_core::env_flag("ROK_INCREMENTAL_VERIFY").then_some(check)
}

fn check(clean: &rokc_hir::HirModule, incr: &mut rokc_hir::HirModule) {
    let parts = |m: &rokc_hir::HirModule| {
        let mut vis: Vec<_> = m.type_visibility.iter().collect();
        vis.sort_by(|a, b| a.0.cmp(b.0));
        let fns = m
            .functions
            .iter()
            .map(|f| (f.name.clone(), format!("{f:?}")));
        let rest = [
            ("structs", format!("{:?}", m.structs)),
            ("enums", format!("{:?}", m.enums)),
            ("statics", format!("{:?}", m.statics)),
            ("diagnostics", format!("{:?}", m.diagnostics)),
            ("roots", format!("{:?}", (&m.roots, &m.unlowered))),
            (
                "any",
                format!("{:?}", (&m.any_seeds, &m.any_types, &m.any_tags)),
            ),
            ("visibility", format!("{vis:?}")),
        ]
        .map(|(k, v)| (k.to_owned(), v));
        fns.chain(rest).collect::<Vec<_>>()
    };
    let (a, b) = (parts(clean), parts(incr));
    let differ = if a.len() == b.len() {
        a.iter()
            .zip(&b)
            .find(|(x, y)| x != y)
            .map(|(x, _)| x.0.clone())
    } else {
        Some(format!("{} functions, not {}", b.len(), a.len()))
    };
    if let Some(what) = differ {
        incr.diagnostics.push(rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Internal,
            rokc_diagnostic::Span::DUMMY,
            format!(
                "ROK_INCREMENTAL_VERIFY: the incremental lowering differs from a clean one at `{what}`"
            ),
        ));
    }
}
