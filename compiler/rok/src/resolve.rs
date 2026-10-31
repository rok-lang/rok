//! Dependency resolution: from the manifest's `deps` (and `dev_deps`)
//! through every dependency's `rok.rokon` to one package per name, each
//! after its dependencies. Git packages and stones come from the lock
//! unless `update` names them; a stone takes the highest non-yanked match
//! (the locked one while it matches) and one version per name, so an unmet
//! later requirement is a conflict. Native packages are resolved the same
//! way afterwards, in their own namespace.
//!
//! `rok.lock` pins the whole graph, optional deps included. Features then
//! pick what a build has: from the root's (`--features`, `default`) along
//! every enabled edge, one set per package. `dev_deps` count only for
//! tests.
// Greedy, no backtracking: a root requirement both sides accept
// (`rok add x@=1.2.0`) settles a conflict.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::git::Cache;
use crate::lock::{Lock, Locked, LockedNative};
use crate::manifest::{
    self, Dep, Enables, Features, GitRef, Manifest, NativeDep, NativeSource, Source,
};
use crate::native;
use crate::registry;
use crate::semver::{Req, Version};

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // independent switches
pub struct Options {
    pub offline: bool,
    /// `--locked`: fail instead of changing `rok.lock`.
    pub locked: bool,
    /// `rok update [names]`: re-resolve these git packages and stones
    /// (empty: all).
    pub update: Option<Vec<String>>,
    pub quiet: bool,
    /// `--features`: the root's features, `<dep>/<feature>` and the
    /// other forms of a feature's list.
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    /// A test / bench build: with the root's `dev_deps`.
    pub dev: bool,
}

/// What a command works on: a project (a directory with `rok.rokon`), a
/// named environment, a script's `//! ```rokon` block, or nothing.
pub struct Context {
    pub manifest: Manifest,
    /// Path dependencies are relative to this directory.
    pub base: PathBuf,
    /// `None`: nothing to lock (no dependencies).
    pub lock_path: Option<PathBuf>,
    /// A script: its `deps` are those of its `//! ```rokon` blocks.
    pub script: bool,
}

#[derive(Clone)]
pub struct Resolved {
    pub name: String,
    /// The package's `src/lib.rok`.
    pub root: PathBuf,
    /// Its enabled features, sorted.
    pub features: Vec<String>,
    /// `path+<dir>`, `git+<url>#<commit>` or `registry+<url>`.
    pub source: String,
    /// A stone's version, else its rok.rokon's `version`.
    pub version: Option<String>,
}

/// A package of the whole graph.
struct Node {
    name: String,
    root: PathBuf,
    deps: Vec<Dep>,
    features: Features,
    /// As in `Resolved`.
    source: String,
    version: Option<String>,
}

/// What a build uses: packages after their dependencies, native packages
/// before theirs (linker order), and the root's features.
pub struct Graph {
    pub packages: Vec<Resolved>,
    pub natives: Vec<native::Resolved>,
    pub features: Vec<String>,
}

/// A native package's identity.
#[derive(PartialEq, Eq)]
enum NativeId {
    Index { url: String, vers: Version },
    File { sha256: String },
}

/// (dependency, base of its relative url, requirer, requirer's key; `""`
/// is the root).
type NativeReq = (NativeDep, Option<String>, String, String);

/// Where a package came from: two requirements of one name must agree.
#[derive(PartialEq, Eq)]
enum Id {
    Path(PathBuf),
    Git { url: String, commit: String },
    Stone { registry: String, vers: Version },
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(p) => write!(f, "path {}", p.display()),
            Self::Git { url, commit } => {
                write!(f, "git {url} at {}", &commit[..commit.len().min(12)])
            }
            Self::Stone { registry, vers } => write!(f, "stone {vers} from {registry}"),
        }
    }
}

struct Resolver<'a> {
    home: PathBuf,
    /// The package being built (`Context::base`, resolved).
    root: PathBuf,
    cache: Cache,
    index: Cache,
    /// For `Dep::version(..)` without `registry`.
    registry: String,
    opts: &'a Options,
    old: Lock,
    new: Lock,
    fetched: Vec<String>,
    /// What each name resolved to, who required it, and how (a stone's
    /// requirement).
    seen: HashMap<String, (Id, String, String)>,
    out: Vec<Node>,
    natives: Vec<NativeReq>,
}

pub fn resolve(home: &Path, ctx: &Context, opts: &Options) -> Result<Graph, String> {
    let old = match &ctx.lock_path {
        Some(p) => Lock::read(p)?.unwrap_or_default(),
        None => Lock::default(),
    };
    if opts.locked && ctx.lock_path.as_ref().is_some_and(|p| !p.is_file()) {
        let any_git = ctx
            .manifest
            .deps
            .iter()
            .chain(&ctx.manifest.dev_deps)
            .any(|d| !matches!(d.source, Source::Path(_)))
            || ctx
                .manifest
                .native
                .iter()
                .any(|d| matches!(d.source, NativeSource::Registry { .. }));
        if any_git {
            return Err("there is no rok.lock, and --locked forbids writing one".to_owned());
        }
    }
    let mut r = Resolver::new(home, &ctx.base, opts, old)?;
    let base = ctx.base.display().to_string();
    r.natives.extend(ctx.manifest.native.iter().map(|d| {
        let by = "the root package".to_owned();
        (d.clone(), Some(base.clone()), by, String::new())
    }));
    let all: Vec<Dep> = ctx
        .manifest
        .deps
        .iter()
        .chain(&ctx.manifest.dev_deps)
        .cloned()
        .collect();
    let by = if ctx.script {
        "the script's `//! rokon` blocks"
    } else {
        "the root package"
    };
    r.walk(&all, &ctx.base, by)?;
    let natives = r.natives()?;
    if let Some(path) = &ctx.lock_path {
        let text = r.new.to_text();
        let old_text = std::fs::read_to_string(path).ok();
        let empty = r.new == Lock::default();
        if old_text.as_deref() != Some(text.as_str()) && !(empty && old_text.is_none()) {
            if opts.locked {
                return Err(format!(
                    "{} is out of date and --locked forbids changing it (run `rok update`)",
                    path.display()
                ));
            }
            std::fs::write(path, text)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        }
    }
    let nodes = order(
        r.out,
        |p| &p.name,
        |p| p.deps.iter().map(|d| d.name.clone()).collect(),
    );
    let mut root_deps = ctx.manifest.deps.clone();
    if opts.dev {
        root_deps.extend(ctx.manifest.dev_deps.iter().cloned());
    }
    let mut on = Features::default();
    let root_features = activate(&nodes, root_deps, &ctx.manifest.features, opts, &mut on)?;
    let packages: Vec<Resolved> = nodes
        .into_iter()
        .filter_map(|n| {
            let features = on
                .iter()
                .find(|(k, _)| *k == manifest::key(&n.name))?
                .1
                .clone();
            Some(Resolved {
                name: n.name,
                root: n.root,
                features,
                source: n.source,
                version: n.version,
            })
        })
        .collect();
    let mut natives: Vec<native::Resolved> = order(
        natives,
        |n| &n.name,
        |n| n.entry.deps.iter().map(|d| d.name.clone()).collect(),
    )
    .into_iter()
    .filter(|n| {
        n.users
            .iter()
            .any(|u| u.is_empty() || on.iter().any(|(k, _)| k == u))
    })
    .collect();
    natives.reverse();
    Ok(Graph {
        packages,
        natives,
        features: root_features,
    })
}

/// The features `opts` asks of the root (with `deps` and `features`):
/// the root's, and in `on` each package reached (by key) with its own.
fn activate(
    nodes: &[Node],
    deps: Vec<Dep>,
    features: &Features,
    opts: &Options,
    on: &mut Features,
) -> Result<Vec<String>, String> {
    let root = Node {
        name: String::new(),
        root: PathBuf::new(),
        deps,
        features: features.clone(),
        source: String::new(),
        version: None,
    };
    let mut all: Vec<&Node> = nodes.iter().collect();
    all.push(&root);
    let mut a = Activation {
        index: all
            .iter()
            .enumerate()
            .map(|(i, n)| (manifest::key(&n.name), i))
            .collect(),
        nodes: all,
        on: HashMap::new(),
        edges: HashSet::new(),
        weak: Vec::new(),
    };
    let r = nodes.len();
    a.enable(r)?;
    let cli = "--features";
    if !opts.no_default_features && features.iter().any(|(n, _)| n == "default") {
        a.apply(r, "default", cli)?;
    }
    if opts.all_features {
        for (f, _) in features {
            a.apply(r, f, cli)?;
        }
    }
    for f in &opts.features {
        a.apply(r, f, cli)?;
    }
    let mine = a.on.remove(&r).unwrap_or_default().into_iter().collect();
    for (i, n) in nodes.iter().enumerate() {
        if let Some(f) = a.on.remove(&i) {
            on.push((manifest::key(&n.name), f.into_iter().collect()));
        }
    }
    Ok(mine)
}

/// Feature resolution over `nodes` (the root last).
struct Activation<'a> {
    nodes: Vec<&'a Node>,
    index: HashMap<String, usize>,
    /// The packages reached, and their features.
    on: HashMap<usize, BTreeSet<String>>,
    /// Dependencies enabled: (package, dependency key).
    edges: HashSet<(usize, String)>,
    /// `<dep>?/<feature>` waiting for its dependency: (package,
    /// dependency key, feature).
    weak: Vec<(usize, String, String)>,
}

impl Activation<'_> {
    fn who(&self, n: usize) -> String {
        match self.nodes[n].name.as_str() {
            "" => "the root package".to_owned(),
            name => format!("package `{name}`"),
        }
    }

    /// Package `n` is in the build, with its non-optional dependencies.
    fn enable(&mut self, n: usize) -> Result<(), String> {
        if self.on.contains_key(&n) {
            return Ok(());
        }
        self.on.insert(n, BTreeSet::new());
        let deps: Vec<String> = self.nodes[n]
            .deps
            .iter()
            .filter(|d| !d.optional)
            .map(|d| d.name.clone())
            .collect();
        for d in deps {
            self.edge(n, &d)?;
        }
        Ok(())
    }

    /// Enable `n`'s dependency `dep` with the features `n` asks; its key.
    fn edge(&mut self, n: usize, dep: &str) -> Result<usize, String> {
        let Some(d) = self.nodes[n]
            .deps
            .iter()
            .find(|d| manifest::same(&d.name, dep))
            .cloned()
        else {
            return Err(format!("{} has no dependency `{dep}`", self.who(n)));
        };
        let key = manifest::key(dep);
        let Some(&t) = self.index.get(&key) else {
            return Err(format!("dependency `{dep}` was not resolved"));
        };
        if !self.edges.insert((n, key.clone())) {
            return Ok(t);
        }
        self.enable(t)?;
        let by = self.who(n);
        for f in &d.features {
            self.feature(t, f, &by)?;
        }
        if d.default_features && self.nodes[t].features.iter().any(|(f, _)| f == "default") {
            self.feature(t, "default", &by)?;
        }
        let waiting: Vec<String> = self
            .weak
            .iter()
            .filter(|(w, k, _)| *w == n && *k == key)
            .map(|(_, _, f)| f.clone())
            .collect();
        for f in waiting {
            self.feature(t, &f, &by)?;
        }
        Ok(t)
    }

    /// Feature `f` of package `n` (in the build), required by `by`.
    fn feature(&mut self, n: usize, f: &str, by: &str) -> Result<(), String> {
        let Some((_, list)) = self.nodes[n].features.iter().find(|(x, _)| x == f) else {
            return Err(format!(
                "{} has no feature `{f}` (required by {by})",
                self.who(n)
            ));
        };
        let list = list.clone();
        if !self.on.entry(n).or_default().insert(f.to_owned()) {
            return Ok(());
        }
        let by = self.who(n);
        for e in &list {
            self.apply(n, e, &by)?;
        }
        Ok(())
    }

    /// Entry `e` of a feature list of package `n`.
    fn apply(&mut self, n: usize, e: &str, by: &str) -> Result<(), String> {
        match Enables::parse(e) {
            Enables::Feature(f) => self.feature(n, f, by),
            Enables::Dep(d) => self.edge(n, d).map(drop),
            Enables::DepFeature {
                dep,
                feature,
                weak: false,
            } => {
                let t = self.edge(n, dep)?;
                self.feature(t, feature, by)
            }
            Enables::DepFeature { dep, feature, .. } => {
                let key = manifest::key(dep);
                if !self.nodes[n]
                    .deps
                    .iter()
                    .any(|d| manifest::same(&d.name, dep))
                {
                    return Err(format!("{} has no dependency `{dep}`", self.who(n)));
                }
                if self.edges.contains(&(n, key.clone())) {
                    let t = self.index[&key];
                    return self.feature(t, feature, by);
                }
                self.weak.push((n, key, feature.to_owned()));
                Ok(())
            }
        }
    }
}

/// The entry's `features` must match the stone's own `rok.rokon` at its
/// commit (any order).
fn check_stone_features(dir: &Path, name: &str, e: &registry::Entry) -> Result<(), String> {
    let own = match manifest::find(dir) {
        Some(p) => manifest::load(&p)?.features,
        None => Features::new(),
    };
    let set = |f: &Features| -> BTreeSet<(String, BTreeSet<String>)> {
        f.iter()
            .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
            .collect()
    };
    if set(&own) != set(&e.features) {
        return Err(format!(
            "stone `{name}` {}: the index's features differ from its rok.rokon's at commit {}",
            e.vers, e.rev
        ));
    }
    Ok(())
}

/// `items` each after the ones its `deps` name.
fn order<T>(
    items: Vec<T>,
    name: impl Fn(&T) -> &String,
    deps: impl Fn(&T) -> Vec<String>,
) -> Vec<T> {
    // 0 new, 1 on the stack (a cycle is cut there), 2 placed.
    fn visit(i: usize, edges: &[Vec<usize>], state: &mut [u8], out: &mut Vec<usize>) {
        if state[i] != 0 {
            return;
        }
        state[i] = 1;
        for &j in &edges[i] {
            visit(j, edges, state, out);
        }
        state[i] = 2;
        out.push(i);
    }
    let edges: Vec<Vec<usize>> = items
        .iter()
        .map(|x| {
            deps(x)
                .iter()
                .filter_map(|n| items.iter().position(|y| manifest::same(name(y), n)))
                .collect()
        })
        .collect();
    let mut state = vec![0; items.len()];
    let mut idx = Vec::new();
    for i in 0..items.len() {
        visit(i, &edges, &mut state, &mut idx);
    }
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    idx.into_iter().filter_map(|i| slots[i].take()).collect()
}

/// Directory of `dep`, fetched into the cache if missing (paths relative
/// to `base`), and its exact source: `=<version>`, a commit, or the path.
pub fn fetch_package(
    home: &Path,
    dep: &Dep,
    base: &Path,
    opts: &Options,
) -> Result<(PathBuf, Source), String> {
    let mut r = Resolver::new(home, base, opts, Lock::default())?;
    let (dir, id, _) = r.locate(dep, base)?;
    let exact = match (id, &dep.source) {
        (Id::Stone { vers, .. }, Source::Registry { registry, .. }) => Source::Registry {
            req: format!("={vers}"),
            registry: registry.clone(),
        },
        (Id::Git { commit, .. }, Source::Git { url, .. }) => Source::Git {
            url: url.clone(),
            reference: GitRef::Rev(commit),
        },
        _ => dep.source.clone(),
    };
    Ok((dir, exact))
}

impl<'a> Resolver<'a> {
    fn new(home: &Path, base: &Path, opts: &'a Options, old: Lock) -> Result<Self, String> {
        Ok(Resolver {
            home: home.to_path_buf(),
            root: rokc_core::real_path(base).unwrap_or_else(|_| base.to_path_buf()),
            cache: Cache::new(home),
            index: Cache::index(home),
            registry: registry::default_url(home)?,
            opts,
            old,
            new: Lock::default(),
            fetched: Vec::new(),
            seen: HashMap::new(),
            out: Vec::new(),
            natives: Vec::new(),
        })
    }

    /// Every package `deps` reach, optional ones too.
    fn walk(&mut self, deps: &[Dep], base: &Path, by: &str) -> Result<(), String> {
        let mut queue: Vec<(Dep, PathBuf, String)> = deps
            .iter()
            .map(|d| (d.clone(), base.to_path_buf(), by.to_owned()))
            .collect();
        let mut i = 0;
        while i < queue.len() {
            let (dep, base, by) = queue[i].clone();
            i += 1;
            if let (
                Some((Id::Stone { registry, vers }, prev_by, prev_req)),
                Source::Registry { req, .. },
            ) = (self.seen.get(&manifest::key(&dep.name)), &dep.source)
            {
                let url = self.registry_of(&dep.source);
                if *registry == url && Req::parse(req)?.matches(vers) {
                    continue;
                }
                return Err(format!(
                    "conflicting requirements for stone `{}`:\n  `{prev_req}` (required by \
                     {prev_by}) chose {vers} from {registry}\n  `{req}` (required by {by}) from \
                     {url}\none build has one version of a stone; require a version both accept \
                     in the root package",
                    dep.name
                ));
            }
            let (dir, id, stone_deps) = self.locate(&dep, &base)?;
            if dir == self.root {
                return Err(format!(
                    "dependency `{}` (required by {by}) is the package being built; a package \
                     names its own items with `pkg::`",
                    dep.name
                ));
            }
            if let Some((prev, prev_by, _)) = self.seen.get(&manifest::key(&dep.name)) {
                if *prev == id {
                    continue;
                }
                return Err(format!(
                    "conflicting requirements for package `{}`:\n  {prev} (required by {prev_by})\n  \
                     {id} (required by {by})\none build has one version of a package",
                    dep.name
                ));
            }
            let root = dir.join("src/lib.rok");
            if !root.is_file() && dir.join("src/main.rok").is_file() {
                return Err(format!(
                    "dependency `{}` ({id}) is an executable package (src/main.rok, no \
                     src/lib.rok): a dependency must be a library; a program is installed with \
                     `rok install`",
                    dep.name
                ));
            }
            if !root.is_file() {
                return Err(format!(
                    "dependency `{}` ({id}) is not a library package: no src/lib.rok in {}",
                    dep.name,
                    dir.display()
                ));
            }
            let me = format!("`{}`", dep.name);
            let mut version = None;
            let (next, natives, features) = match stone_deps {
                Some(d) => d,
                None => match manifest::find(&dir) {
                    Some(m) => {
                        let m = manifest::load(&m)?;
                        version = m.version;
                        (m.deps, m.native, m.features)
                    }
                    None => (Vec::new(), Vec::new(), Vec::new()),
                },
            };
            let source = match &id {
                Id::Path(p) => format!("path+{}", p.display()),
                Id::Git { url, commit } => format!("git+{url}#{commit}"),
                Id::Stone { registry, vers } => {
                    version = Some(vers.to_string());
                    format!("registry+{registry}")
                }
            };
            let base = dir.display().to_string();
            let user = manifest::key(&dep.name);
            self.natives.extend(
                natives
                    .into_iter()
                    .map(|n| (n, Some(base.clone()), me.clone(), user.clone())),
            );
            queue.extend(next.iter().map(|d| (d.clone(), dir.clone(), me.clone())));
            let req = match &dep.source {
                Source::Registry { req, .. } => req.clone(),
                _ => String::new(),
            };
            self.seen.insert(manifest::key(&dep.name), (id, by, req));
            self.out.push(Node {
                name: dep.name,
                root,
                deps: next,
                features,
                source,
                version,
            });
        }
        Ok(())
    }

    fn registry_of(&self, source: &Source) -> String {
        match source {
            Source::Registry {
                registry: Some(r), ..
            } => r.clone(),
            _ => self.registry.clone(),
        }
    }

    fn updating(&self, name: &str) -> bool {
        self.opts
            .update
            .as_ref()
            .is_some_and(|names| names.is_empty() || names.iter().any(|n| manifest::same(n, name)))
    }

    /// `dep`'s directory and identity; for a stone also its dependencies
    /// and native packages from the index.
    fn locate(
        &mut self,
        dep: &Dep,
        base: &Path,
    ) -> Result<(PathBuf, Id, Option<StoneDeps>), String> {
        let (url, reference) = match &dep.source {
            Source::Path(p) => {
                let dir = base.join(p);
                let dir = rokc_core::real_path(&dir).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        format!(
                            "dependency `{}`: no directory `{p}` ({}); a `Dep::path` is \
                             relative to the directory of the `rok.rokon` that names it",
                            dep.name,
                            dir.display()
                        )
                    } else {
                        format!("dependency `{}`: path {}: {e}", dep.name, dir.display())
                    }
                })?;
                return Ok((dir.clone(), Id::Path(dir), None));
            }
            Source::Git { url, reference } => (url, reference),
            Source::Registry { req, .. } => {
                let url = self.registry_of(&dep.source);
                return self.locate_stone(&dep.name, req, &url);
            }
        };
        let updating = self.updating(&dep.name);
        let pinned = self
            .old
            .find(&dep.name)
            .filter(|l| !updating && l.vers.is_none() && l.url == *url && l.reference == *reference)
            .cloned();
        let commit = if let Some(l) = &pinned {
            if !self.cache.has_commit(url, &l.commit) {
                self.fetch(url, &dep.name)?;
                if !self.cache.has_commit(url, &l.commit) {
                    return Err(format!(
                        "`{}`: locked commit {} is not in {url} (run `rok update {}`)",
                        dep.name, l.commit, dep.name
                    ));
                }
            }
            l.commit.clone()
        } else {
            if self.opts.locked && !updating {
                return Err(format!(
                    "`{}` is not in rok.lock (or its source changed), and --locked forbids \
                     changing it",
                    dep.name
                ));
            }
            if self.opts.offline {
                self.cache
                    .resolve(url, reference)
                    .map_err(|e| format!("`{}`: {e} (--offline: nothing is fetched)", dep.name))?
            } else {
                self.fetch(url, &dep.name)?;
                self.cache.resolve(url, reference)?
            }
        };
        let (dir, tree) = self.cache.checkout(url, &commit)?;
        if let Some(l) = &pinned {
            if l.tree != tree {
                return Err(format!(
                    "`{}`: commit {} has tree {tree}, but rok.lock says {}",
                    dep.name, l.commit, l.tree
                ));
            }
        }
        if !self
            .new
            .packages
            .iter()
            .any(|p| manifest::same(&p.name, &dep.name))
        {
            self.new.packages.push(Locked {
                name: dep.name.clone(),
                vers: None,
                url: url.clone(),
                reference: reference.clone(),
                commit: commit.clone(),
                tree,
            });
        }
        Ok((
            dir,
            Id::Git {
                url: url.clone(),
                commit,
            },
            None,
        ))
    }

    /// The version `req` picks, checked out and verified against `cksum`.
    fn locate_stone(
        &mut self,
        name: &str,
        req: &str,
        url: &str,
    ) -> Result<(PathBuf, Id, Option<StoneDeps>), String> {
        let parsed = Req::parse(req).map_err(|e| format!("stone `{name}`: {e}"))?;
        let updating = self.updating(name);
        let pinned = self
            .old
            .find(name)
            .filter(|l| !updating && l.url == url)
            .and_then(|l| Some((l.clone(), Version::parse(l.vers.as_deref()?).ok()?)))
            .filter(|(_, v)| parsed.matches(v));
        if pinned.is_none() && self.opts.locked && !updating {
            return Err(format!(
                "`{name}` is not in rok.lock (or its requirement changed), and --locked forbids \
                 changing it"
            ));
        }
        if updating || !self.index.has(url) {
            self.fetch_index(url, name)?;
        }
        // A locked version stays; a new pick skips stones whose
        // `toolchain` this rok is not.
        let pick = |all: &[registry::Entry]| {
            if let Some((_, v)) = &pinned {
                return (all.iter().find(|e| e.vers == *v).cloned(), None);
            }
            let (e, note) = registry::best_here(all, &parsed);
            (e.cloned(), note)
        };
        let mut all = registry::entries(&self.index, url, name)?;
        let (mut chosen, mut skipped) = pick(&all);
        if chosen.is_none() && !self.fetched.iter().any(|u| u == url) && !self.opts.offline {
            self.fetch_index(url, name)?;
            all = registry::entries(&self.index, url, name)?;
            (chosen, skipped) = pick(&all);
        }
        let e = match (chosen, &pinned) {
            (Some(e), _) => e,
            (None, Some((_, v))) => {
                return Err(format!(
                    "`{name}` {v} (in rok.lock) is not in the index {url} (run `rok update {name}`)"
                ));
            }
            (None, None) => {
                let mut e = registry::no_match(&self.index, &all, name, req, url);
                if let Some(s) = skipped {
                    e = format!("{e}\nhelp: {s}");
                }
                return Err(e);
            }
        };
        if let Some(s) = skipped.filter(|_| !self.opts.quiet) {
            rokc_core::term::note(&format!("{s}; using {}", e.vers));
        }
        if let Some((l, _)) = &pinned {
            if l.commit != e.rev {
                return Err(format!(
                    "the index {url} now says {name} {} is commit {}, but rok.lock says {} \
                     (run `rok update {name}` to accept it)",
                    e.vers, e.rev, l.commit
                ));
            }
        }
        if !self.cache.has_commit(&e.url, &e.rev) {
            self.fetch(&e.url, name)?;
            if !self.cache.has_commit(&e.url, &e.rev) {
                return Err(format!(
                    "stone `{name}` {}: commit {} is not in {}",
                    e.vers, e.rev, e.url
                ));
            }
        }
        let (dir, tree) = self.cache.checkout(&e.url, &e.rev)?;
        if tree != e.cksum {
            return Err(format!(
                "stone `{name}` {}: commit {} has tree {tree}, but the index's cksum is {}",
                e.vers, e.rev, e.cksum
            ));
        }
        check_stone_features(&dir, name, &e)?;
        if !self
            .new
            .packages
            .iter()
            .any(|p| manifest::same(&p.name, name))
        {
            self.new.packages.push(Locked {
                name: name.to_owned(),
                vers: Some(e.vers.to_string()),
                url: url.to_owned(),
                reference: crate::manifest::GitRef::Default,
                commit: e.rev.clone(),
                tree,
            });
        }
        let registry =
            |d: &registry::IndexDep| Some(d.registry.clone().unwrap_or_else(|| url.to_owned()));
        let deps = e
            .deps
            .iter()
            .filter(|d| !d.dev && !d.native)
            .map(|d| d.to_dep(url))
            .collect();
        let natives = e
            .deps
            .iter()
            .filter(|d| d.native)
            .map(|d| NativeDep {
                name: d.name.clone(),
                source: NativeSource::Registry {
                    req: d.req.clone(),
                    registry: registry(d),
                },
            })
            .collect();
        Ok((
            dir,
            Id::Stone {
                registry: url.to_owned(),
                vers: e.vers,
            },
            Some((deps, natives, e.features)),
        ))
    }

    /// The native packages every requirement reaches: one per name.
    fn natives(&mut self) -> Result<Vec<native::Resolved>, String> {
        let mut out: Vec<native::Resolved> = Vec::new();
        // Per name: its place in `out`, identity, requirer and requirement.
        let mut seen: HashMap<String, (usize, NativeId, String, String)> = HashMap::new();
        let mut i = 0;
        while i < self.natives.len() {
            let (dep, base, by, user) = self.natives[i].clone();
            i += 1;
            let how = match &dep.source {
                NativeSource::Registry { req, registry } => format!(
                    "`{req}` from {}",
                    registry.as_deref().unwrap_or(&self.registry)
                ),
                NativeSource::Direct { url, sha256 } => format!("{url} (sha256 {sha256})"),
            };
            if let Some((j, id, prev_by, prev_how)) = seen.get(&manifest::key(&dep.name)) {
                let fits = match (&dep.source, id) {
                    (NativeSource::Registry { req, registry }, NativeId::Index { url, vers }) => {
                        registry.as_deref().unwrap_or(&self.registry) == url
                            && Req::parse(req)?.matches(vers)
                    }
                    (NativeSource::Direct { sha256, .. }, NativeId::File { sha256: s }) => {
                        sha256 == s
                    }
                    _ => false,
                };
                if !fits {
                    let e = &out[*j].entry;
                    return Err(format!(
                        "conflicting requirements for native package `{}`:\n  {prev_how} \
                         (required by {prev_by}) chose {}\n  {how} (required by {by})\none build \
                         has one version of a native package",
                        dep.name, e.vers
                    ));
                }
                if !out[*j].users.contains(&user) {
                    out[*j].users.push(user);
                }
                continue;
            }
            let (entry, id, base, registry) = match &dep.source {
                NativeSource::Registry { req, registry } => {
                    let url = registry.clone().unwrap_or_else(|| self.registry.clone());
                    let e = self.locate_native(&dep.name, req, &url)?;
                    let id = NativeId::Index {
                        url: url.clone(),
                        vers: e.vers.clone(),
                    };
                    (e, id, None, url)
                }
                NativeSource::Direct { url, sha256 } => {
                    let loc = native::locate(base.as_deref(), url)?;
                    let e = native::direct(&self.home, &dep.name, &loc, sha256, self.opts.offline)?;
                    let id = NativeId::File {
                        sha256: sha256.clone(),
                    };
                    (e, id, Some(native::dir_of(&loc)), self.registry.clone())
                }
            };
            let me = format!("native package `{}`", dep.name);
            for d in &entry.deps {
                let mut d = d.clone();
                if let NativeSource::Registry {
                    registry: r @ None, ..
                } = &mut d.source
                {
                    *r = Some(registry.clone());
                }
                self.natives
                    .push((d, base.clone(), me.clone(), user.clone()));
            }
            seen.insert(manifest::key(&dep.name), (out.len(), id, by, how));
            out.push(native::Resolved {
                name: dep.name,
                entry,
                base,
                users: vec![user],
            });
        }
        // A native package's dependencies serve its users too.
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..out.len() {
                for d in out[i].entry.deps.clone() {
                    let Some(j) = out.iter().position(|o| manifest::same(&o.name, &d.name)) else {
                        continue;
                    };
                    for u in out[i].users.clone() {
                        if !out[j].users.contains(&u) {
                            out[j].users.push(u);
                            changed = true;
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// The version `req` picks (locked while it matches), checked against
    /// the lock.
    fn locate_native(&mut self, name: &str, req: &str, url: &str) -> Result<native::Entry, String> {
        let parsed = Req::parse(req).map_err(|e| format!("native package `{name}`: {e}"))?;
        let updating = self.updating(name);
        let pinned = self
            .old
            .find_native(name)
            .filter(|l| !updating && l.url == url)
            .and_then(|l| Some((l.clone(), Version::parse(&l.vers).ok()?)))
            .filter(|(_, v)| parsed.matches(v));
        if pinned.is_none() && self.opts.locked && !updating {
            return Err(format!(
                "native package `{name}` is not in rok.lock (or its requirement changed), and \
                 --locked forbids changing it"
            ));
        }
        if updating || !self.index.has(url) {
            self.fetch_index(url, name)?;
        }
        let pick = |all: &[native::Entry]| match &pinned {
            Some((_, v)) => all.iter().find(|e| e.vers == *v).cloned(),
            None => native::best(all, &parsed).cloned(),
        };
        let mut all = native::entries(&self.index, url, name)?;
        let mut chosen = pick(&all);
        if chosen.is_none() && !self.fetched.iter().any(|u| u == url) && !self.opts.offline {
            self.fetch_index(url, name)?;
            all = native::entries(&self.index, url, name)?;
            chosen = pick(&all);
        }
        let e = match (chosen, &pinned) {
            (Some(e), _) => e,
            (None, Some((_, v))) => {
                return Err(format!(
                    "native package `{name}` {v} (in rok.lock) is not in the index {url} (run \
                     `rok update {name}`)"
                ));
            }
            (None, None) => return Err(native::no_match(&self.index, &all, name, req, url)),
        };
        let sum = e.checksum();
        if let Some((l, _)) = &pinned {
            if l.sha256 != sum {
                return Err(format!(
                    "the index {url} changed native package `{name}` {} (entry sha256 {sum}, \
                     rok.lock says {}; run `rok update {name}` to accept it)",
                    e.vers, l.sha256
                ));
            }
        }
        if self.new.find_native(name).is_none() {
            self.new.natives.push(LockedNative {
                name: name.to_owned(),
                vers: e.vers.to_string(),
                url: url.to_owned(),
                sha256: sum,
            });
        }
        Ok(e)
    }

    /// Bring index `url` up to date, once per run.
    fn fetch_index(&mut self, url: &str, name: &str) -> Result<(), String> {
        if self.fetched.iter().any(|u| u == url) {
            return Ok(());
        }
        if self.opts.offline {
            if self.index.has(url) {
                return Ok(());
            }
            return Err(format!(
                "stone `{name}` needs the index {url}, but --offline is set (it is not in the cache)"
            ));
        }
        if !self.opts.quiet {
            rokc_core::term::status("Updating", &format!("index {url}"));
        }
        self.index.fetch(url)?;
        self.fetched.push(url.to_owned());
        Ok(())
    }

    fn fetch(&mut self, url: &str, name: &str) -> Result<(), String> {
        if self.fetched.iter().any(|u| u == url) {
            return Ok(());
        }
        if self.opts.offline {
            return Err(format!(
                "`{name}` needs a fetch of {url}, but --offline is set (it is not in the cache)"
            ));
        }
        if !self.opts.quiet {
            rokc_core::term::status("Fetching", url);
        }
        self.cache.fetch(url)?;
        self.fetched.push(url.to_owned());
        Ok(())
    }
}

/// A stone's dependencies, native packages and features.
type StoneDeps = (Vec<Dep>, Vec<NativeDep>, Features);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependencies_come_first() {
        let r = |n: &str, deps: &[&'static str]| (n.to_owned(), deps.to_vec());
        // Breadth-first from the root: b, a, c; c needs a.
        let all = vec![r("b", &["a", "c"]), r("a", &[]), r("c", &["a"])];
        let deps = |p: &(String, Vec<&str>)| p.1.iter().map(|d| (*d).to_owned()).collect();
        let names: Vec<String> = order(all, |p| &p.0, deps)
            .into_iter()
            .map(|d| d.0)
            .collect();
        assert_eq!(names, ["a", "c", "b"]);
    }

    /// A node: name, dependencies (`?` optional), features.
    type Spec<'a> = (&'a str, &'a [&'a str], &'a [(&'a str, &'a [&'a str])]);

    fn graph(nodes: &[Spec<'_>]) -> Vec<Node> {
        nodes
            .iter()
            .map(|(name, deps, features)| Node {
                name: (*name).to_owned(),
                root: PathBuf::new(),
                source: String::new(),
                version: None,
                deps: deps
                    .iter()
                    .map(|d| Dep {
                        optional: d.ends_with('?'),
                        ..Dep::new(
                            d.trim_end_matches('?').to_owned(),
                            Source::Path(String::new()),
                        )
                    })
                    .collect(),
                features: features
                    .iter()
                    .map(|(f, l)| ((*f).to_owned(), l.iter().map(|e| (*e).to_owned()).collect()))
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn features_unify_and_follow_dependencies() {
        let nodes = graph(&[
            (
                "tls",
                &[],
                &[("default", &["ring"]), ("ring", &[]), ("fast", &[])],
            ),
            ("json", &[], &[("pretty", &[])]),
            (
                "http",
                &["tls?", "json"],
                &[
                    ("tls", &["dep:tls"]),
                    ("both", &["tls?/fast", "json/pretty"]),
                ],
            ),
            ("cli", &["http"], &[]),
        ]);
        let run = |root: &[&str], fs: &[(&str, &[&str])], o: Options| {
            let root = graph(&[("", root, fs)]).remove(0);
            let mut on = Features::default();
            activate(&nodes, root.deps, &root.features, &o, &mut on).map(|mine| (mine, on))
        };
        let opts = |features: &[&str]| Options {
            features: features.iter().map(|f| (*f).to_owned()).collect(),
            ..Options::default()
        };
        // `http` without `tls`: the weak `tls?/fast` waits, `tls` is out.
        let (_, on) = run(&["http", "cli"], &[], opts(&["http/both"])).unwrap();
        let names: Vec<&str> = on.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["json", "http", "cli"]);
        assert_eq!(on[0].1, ["pretty"]);
        // `tls` comes in (with its default) and the weak feature applies.
        let (_, on) = run(&["http"], &[], opts(&["http/both", "http/tls"])).unwrap();
        assert_eq!(
            on[0],
            (
                "tls".into(),
                vec!["default".into(), "fast".into(), "ring".into()]
            )
        );
        // The root's default, and `--no-default-features`.
        let fs: &[(&str, &[&str])] = &[("default", &["x"]), ("x", &["http/tls"])];
        let (mine, on) = run(&["http"], fs, opts(&[])).unwrap();
        assert_eq!((mine.len(), on.len()), (2, 3));
        let o = Options {
            no_default_features: true,
            ..Options::default()
        };
        let (mine, on) = run(&["http"], fs, o).unwrap();
        assert_eq!((mine.len(), on.len()), (0, 2));
        let e = run(&["http"], &[], opts(&["http/nope"])).unwrap_err();
        assert_eq!(
            e,
            "package `http` has no feature `nope` (required by --features)"
        );
        let e = run(&["http"], &[], opts(&["zip/x"])).unwrap_err();
        assert_eq!(e, "the root package has no dependency `zip`");
    }
}
