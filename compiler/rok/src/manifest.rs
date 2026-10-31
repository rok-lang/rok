//! `rok.rokon` (a package's or an environment's) and a script's
//! `//! ```rokon` block: one data-only ROKON value, `Package { .. }` (the
//! name is optional).
//!
//! ```rokon
//! Package {
//!     name: "app",
//!     version: "0.1.0",
//!     toolchain: "0.3",
//!     description: "A JSON app",
//!     license: "MIT OR Apache-2.0",
//!     repository: "https://example.com/app.git",
//!     authors: ["Ann <ann@example.com>"],
//!     homepage: "https://example.com/app",
//!     documentation: "https://example.com/app/docs",
//!     readme: "README.md",              // the default when README.md exists
//!     keywords: ["json", "parser"],     // at most 5
//!     publish: true,                    // false: `rok publish` refuses
//!     deps: {
//!         "json": Dep::git("https://example.com/json.git", tag: "v1.2"),
//!         "util": Dep::path("../util"),
//!         "geom": Dep::version("^0.3"),   // a stone; also `"geom": "^0.3"`
//!         "tls": Dep::version("^2", optional: true, default_features: false),
//!     },
//!     features: { "default": ["json"], "json": [], "tls": ["dep:tls", "geom/fast"] },
//!     dev_deps: {},
//!     lints_from: "..",                 // ../rok.rokon's lints first
//!     lints: { "dead_code": "allow" },
//!     native: {                              // prebuilt C libraries (`native`)
//!         "zlib": "^1.3",                    // = Dep::native("^1.3")
//!         "png": Dep::native(url: "vendor/png.rokon", sha256: "<hex>"),
//!     },
//! }
//! ```

use std::path::{Path, PathBuf};

use rokc_core::lint::Level;
use rokc_core::term::{self, Role, Stream};
use rokc_rokon::edit::{self, Step};
use rokc_rokon::{Decoder, Doc, Field, Kind, Value};

pub const MANIFEST: &str = "rok.rokon";
pub const LOCK: &str = "rok.lock";
/// A manifest with nothing in it, for `rok add` to fill.
pub const EMPTY: &str = "Package {\n    deps: {},\n}\n";

/// `keywords` holds at most this many.
pub const MAX_KEYWORDS: usize = 5;

const FIELDS: [&str; 18] = [
    "name",
    "version",
    "toolchain",
    "description",
    "license",
    "repository",
    "authors",
    "homepage",
    "documentation",
    "readme",
    "keywords",
    "publish",
    "deps",
    "dev_deps",
    "lints",
    "lints_from",
    "native",
    "features",
];

/// A dependency's feature arguments (`Dep::..(.., optional: true)`).
const DEP_FLAGS: [&str; 3] = ["optional", "features", "default_features"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitRef {
    Default,
    Rev(String),
    Tag(String),
    Branch(String),
}

impl GitRef {
    /// `(name, value)` of the `Dep::git(.., name: value)` argument.
    pub fn pair(&self) -> Option<(&'static str, &str)> {
        match self {
            Self::Default => None,
            Self::Rev(r) => Some(("rev", r)),
            Self::Tag(t) => Some(("tag", t)),
            Self::Branch(b) => Some(("branch", b)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// As written: relative to the manifest's directory.
    Path(String),
    Git {
        url: String,
        reference: GitRef,
    },
    /// A stone: a version requirement (`crate::semver::Req` syntax) on a
    /// registry index (`None`: the configured one).
    Registry {
        req: String,
        registry: Option<String>,
    },
}

impl Source {
    /// The `Dep::..` value.
    pub fn to_value(&self) -> Value {
        match self {
            Self::Path(p) => Value::call("Dep::path", vec![Value::str(p)], Vec::new()),
            Self::Git { url, reference } => Value::call(
                "Dep::git",
                vec![Value::str(url)],
                reference
                    .pair()
                    .map(|(k, v)| (k, Value::str(v)))
                    .into_iter()
                    .collect(),
            ),
            Self::Registry { req, registry } => Value::call(
                "Dep::version",
                vec![Value::str(req)],
                registry
                    .iter()
                    .map(|r| ("registry", Value::str(r)))
                    .collect(),
            ),
        }
    }

    /// The `Dep::..` text for `rok.rokon`, on one line (it is spliced in).
    pub fn to_rok(&self) -> String {
        rokc_rokon::to_line(&self.to_value())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dep {
    pub name: String,
    pub source: Source,
    /// Built only when a feature enables it (`dep:<name>`, `<name>/<f>`).
    pub optional: bool,
    /// Its features this package enables.
    pub features: Vec<String>,
    /// `default_features: false` turns it off.
    pub default_features: bool,
}

impl Dep {
    pub const fn new(name: String, source: Source) -> Self {
        Self {
            name,
            source,
            optional: false,
            features: Vec::new(),
            default_features: true,
        }
    }

    /// The `Dep::..` value; feature arguments only where not the default.
    pub fn to_value(&self) -> Value {
        let mut v = self.source.to_value();
        if let Kind::Variant { named, .. } = &mut v.kind {
            if self.optional {
                named.push(Field::new("optional", Value::new(Kind::Bool(true))));
            }
            if !self.features.is_empty() {
                named.push(Field::new("features", strings(&self.features)));
            }
            if !self.default_features {
                named.push(Field::new(
                    "default_features",
                    Value::new(Kind::Bool(false)),
                ));
            }
        }
        v
    }

    /// `to_value` on one line (it is spliced into `rok.rokon`).
    pub fn to_rok(&self) -> String {
        rokc_rokon::to_line(&self.to_value())
    }
}

pub fn strings(items: &[String]) -> Value {
    Value::new(Kind::List(items.iter().map(Value::str).collect()))
}

/// `features`: name -> what it enables, in file order.
pub type Features = Vec<(String, Vec<String>)>;

/// One entry of a feature's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enables<'a> {
    /// Another feature of the package.
    Feature(&'a str),
    /// `dep:<name>`: the optional dependency.
    Dep(&'a str),
    /// `<dep>/<feature>` (enabling the dependency too), or with `weak`
    /// `<dep>?/<feature>` (only when something else enables it).
    DepFeature {
        dep: &'a str,
        feature: &'a str,
        weak: bool,
    },
}

impl<'a> Enables<'a> {
    pub fn parse(s: &'a str) -> Self {
        if let Some(d) = s.strip_prefix("dep:") {
            return Self::Dep(d);
        }
        match s.split_once('/') {
            Some((dep, feature)) => match dep.strip_suffix('?') {
                Some(dep) => Self::DepFeature {
                    dep,
                    feature,
                    weak: true,
                },
                None => Self::DepFeature {
                    dep,
                    feature,
                    weak: false,
                },
            },
            None => Self::Feature(s),
        }
    }
}

/// A feature name: `[a-z0-9][a-z0-9_-]*`.
pub fn check_feature(name: &str) -> Result<(), String> {
    let ok = name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` is not a valid feature name: lowercase ASCII letters, digits, `-` and `_`, \
             starting with a letter or digit"
        ))
    }
}

/// Where a feature table is wrong.
#[derive(Debug, PartialEq, Eq)]
pub enum At {
    /// The feature's name.
    Feature(usize),
    /// An entry of its list.
    Entry(usize, usize),
    /// A dependency (by index in `deps`).
    Dep(usize),
}

/// Check names and references, that no feature reaches itself, and that
/// some feature enables each optional dependency.
pub fn check_features(features: &Features, deps: &[Dep]) -> Result<(), (At, String)> {
    // 0 new, 1 on the path, 2 done.
    fn visit(
        i: usize,
        features: &Features,
        state: &mut [u8],
        path: &mut Vec<usize>,
    ) -> Result<(), (At, String)> {
        state[i] = 1;
        path.push(i);
        for (j, e) in features[i].1.iter().enumerate() {
            let Enables::Feature(f) = Enables::parse(e) else {
                continue;
            };
            let Some(k) = features.iter().position(|(n, _)| n == f) else {
                continue;
            };
            if state[k] == 1 {
                let from = path.iter().position(|&p| p == k).unwrap_or(0);
                let names: Vec<&str> = path[from..]
                    .iter()
                    .chain([&k])
                    .map(|&p| features[p].0.as_str())
                    .collect();
                return Err((
                    At::Entry(i, j),
                    format!("feature cycle: {}", names.join(" -> ")),
                ));
            }
            if state[k] == 0 {
                visit(k, features, state, path)?;
            }
        }
        path.pop();
        state[i] = 2;
        Ok(())
    }
    let find = |d: &str| deps.iter().position(|x| same(&x.name, d));
    for (i, (name, list)) in features.iter().enumerate() {
        check_feature(name).map_err(|e| (At::Feature(i), e))?;
        for (j, e) in list.iter().enumerate() {
            let at = || At::Entry(i, j);
            match Enables::parse(e) {
                Enables::Feature(f) => {
                    check_feature(f).map_err(|m| (at(), m))?;
                    if !features.iter().any(|(n, _)| n == f) {
                        return Err((at(), format!("`{f}` is not a feature of this package")));
                    }
                }
                Enables::Dep(d) => match find(d) {
                    Some(k) if deps[k].optional => {}
                    Some(_) => {
                        return Err((
                            at(),
                            format!("`{e}`: dependency `{d}` is not `optional: true`"),
                        ));
                    }
                    None => return Err((at(), format!("`{e}`: there is no dependency `{d}`"))),
                },
                Enables::DepFeature { dep, feature, weak } => {
                    check_feature(feature).map_err(|m| (at(), format!("`{e}`: {m}")))?;
                    match find(dep) {
                        Some(k) if weak && !deps[k].optional => {
                            return Err((
                                at(),
                                format!(
                                    "`{e}`: `?` is for an optional dependency, and `{dep}` is not"
                                ),
                            ));
                        }
                        Some(_) => {}
                        None => {
                            return Err((at(), format!("`{e}`: there is no dependency `{dep}`")));
                        }
                    }
                }
            }
        }
    }
    let mut state = vec![0; features.len()];
    for i in 0..features.len() {
        if state[i] == 0 {
            visit(i, features, &mut state, &mut Vec::new())?;
        }
    }
    for (k, d) in deps.iter().enumerate().filter(|(_, d)| d.optional) {
        let enabled = features
            .iter()
            .flat_map(|(_, l)| l)
            .any(|e| match Enables::parse(e) {
                Enables::Dep(x)
                | Enables::DepFeature {
                    dep: x,
                    weak: false,
                    ..
                } => same(x, &d.name),
                _ => false,
            });
        if !enabled {
            return Err((
                At::Dep(k),
                format!(
                    "optional dependency `{}` is not enabled by any feature (add \"dep:{}\" to \
                     one)",
                    d.name, d.name
                ),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub toolchain: Option<String>,
    /// Package metadata, carried into a stone's index entry.
    pub description: Option<String>,
    /// An SPDX license expression.
    pub license: Option<String>,
    pub repository: Option<String>,
    // authors, homepage, documentation, keywords: checked, not yet in the
    // index entry (older rok rejects unknown entry fields).
    /// `"Name <email>"` each.
    #[allow(dead_code)]
    pub authors: Vec<String>,
    #[allow(dead_code)]
    pub homepage: Option<String>,
    #[allow(dead_code)]
    pub documentation: Option<String>,
    /// As written: relative to the manifest's directory (`readme_path`).
    pub readme: Option<String>,
    /// At most `MAX_KEYWORDS`, none empty.
    #[allow(dead_code)]
    pub keywords: Vec<String>,
    /// `publish: false`: `rok publish` refuses the package (`None`: true).
    pub publish: Option<bool>,
    pub deps: Vec<Dep>,
    pub dev_deps: Vec<Dep>,
    /// `lints`: `"name": "allow" | "warn" | "deny"`, in file order
    /// (lint names are checked by `rokc lint`).
    pub lints: Vec<(String, Level)>,
    /// `lints_from`: a directory (relative to this manifest's) whose
    /// manifest's lints come before `lints` (`inherit_lints`).
    pub lints_from: Option<String>,
    /// `native`: prebuilt C libraries (a namespace of their own).
    pub native: Vec<NativeDep>,
    pub features: Features,
    /// Problems that do not stop a build: a name no file can carry on
    /// some OS, a `version` that is not semver (`load` prints them).
    pub warnings: Vec<String>,
}

/// Where a native package comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeSource {
    /// `Dep::native("req"[, registry: "url"])` or `"req"`: the index's
    /// `native/` files.
    Registry {
        req: String,
        registry: Option<String>,
    },
    /// `Dep::native(url: "..", sha256: "..")`: a `Native { .. }` file (a
    /// url, or a path relative to the file naming it) with that SHA-256.
    Direct { url: String, sha256: String },
}

impl NativeSource {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Registry { req, registry } => Value::call(
                "Dep::native",
                vec![Value::str(req)],
                registry
                    .iter()
                    .map(|r| ("registry", Value::str(r)))
                    .collect(),
            ),
            Self::Direct { url, sha256 } => Value::call(
                "Dep::native",
                Vec::new(),
                vec![("url", Value::str(url)), ("sha256", Value::str(sha256))],
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeDep {
    pub name: String,
    pub source: NativeSource,
}

/// A package name: `[a-z][a-z0-9_-]*`, a letter first since code spells
/// `-` as `_` (`code_name`).
pub fn check_name(name: &str) -> Result<(), String> {
    let valid = |n: &str| {
        n.starts_with(|c: char| c.is_ascii_lowercase())
            && n.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    if let Some(e) = rokc_core::reserved_package(&name.replace('-', "_")) {
        return Err(e);
    }
    if valid(name) {
        return Ok(());
    }
    let fixed: String = name
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
    Err(format!(
        "`{name}` is not a valid package name: lowercase ASCII letters, digits, `-` and `_`, \
         starting with a letter{}",
        if valid(&fixed) {
            format!("; try `{fixed}`")
        } else {
            String::new()
        }
    ))
}

/// `check_name`, and a name every OS can store (`rokc_core::check_os_name`):
/// what `rok new`, `init`, `add` and `publish` require. An existing
/// manifest only warns (`parse`).
pub fn check_new_name(name: &str) -> Result<(), String> {
    check_name(name)?;
    rokc_core::check_os_name("package", name)
}

/// The name code uses for package `name` (`use`, `--extern`): `-` as `_`.
pub fn code_name(name: &str) -> String {
    name.replace('-', "_")
}

/// Identity: names equal up to case and `-` / `_` are one package.
pub fn key(name: &str) -> String {
    code_name(&name.to_ascii_lowercase())
}

pub fn same(a: &str, b: &str) -> bool {
    key(a) == key(b)
}

/// `dir/rok.rokon`, if present. A case variant (`Rok.rokon`) only warns,
/// whatever the file system.
pub fn find(dir: &Path) -> Option<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut found = None;
    let mut names: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect();
    names.sort();
    for (name, path) in names {
        if name == MANIFEST {
            found = Some(path);
        } else if name.eq_ignore_ascii_case(MANIFEST) {
            eprintln!(
                "rok: {} ignoring `{}`: the manifest is named `{MANIFEST}` (lowercase)",
                term::label(Stream::Stderr, Role::Warning),
                path.display()
            );
        }
    }
    found
}

/// `parse` the file at `path`; its warnings go to stderr, once each.
pub fn load(path: &Path) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let m = parse(&text, &path.display().to_string())?;
    for w in &m.warnings {
        crate::warn_once(w);
    }
    Ok(m)
}

/// `toolchain` of the manifest text, read alone: fields this version does
/// not know (a newer toolchain's) do not drop the pin.
pub fn toolchain_pin(text: &str) -> Option<String> {
    let doc = Doc::parse(text, MANIFEST).ok()?;
    let pkg = doc.root().record("Package").ok()?;
    pkg.opt("toolchain")?.string().ok()
}

impl Manifest {
    /// The readme `rok publish` takes: `readme` (a file inside the
    /// package), else `README.md` when there is one.
    pub fn readme_path(&self, dir: &Path) -> Result<Option<PathBuf>, String> {
        let Some(r) = &self.readme else {
            return Ok(Some(dir.join("README.md")).filter(|p| p.is_file()));
        };
        // Lexical: `a/../b` stays inside, `../x` and `/x` do not.
        let mut depth = 0usize;
        let inside = Path::new(r).components().all(|c| match c {
            std::path::Component::Normal(_) => {
                depth += 1;
                true
            }
            std::path::Component::CurDir => true,
            std::path::Component::ParentDir => depth.checked_sub(1).map(|d| depth = d).is_some(),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => false,
        });
        if !inside || r.is_empty() {
            return Err(format!(
                "`readme: {r:?}` in {MANIFEST} is outside the package; write a path inside it, \
                 relative to {MANIFEST} (e.g. `readme: \"README.md\"`)"
            ));
        }
        let p = dir.join(r);
        if p.is_file() {
            Ok(Some(p))
        } else {
            Err(format!(
                "`readme: {r:?}` in {MANIFEST}: {} is not a file; write the path relative to \
                 {MANIFEST}, or remove `readme` to use README.md",
                p.display()
            ))
        }
    }

    /// `Err` with how to change it when `publish: false`.
    pub fn check_publish(&self) -> Result<(), String> {
        if self.publish == Some(false) {
            let name = self.name.as_deref().unwrap_or("this package");
            return Err(format!(
                "`{name}` has `publish: false` in {MANIFEST}, so it is not published; remove \
                 that line (or write `publish: true`) to publish it"
            ));
        }
        Ok(())
    }
}

/// Parse manifest text; `file` names it in errors.
pub fn parse(text: &str, file: &str) -> Result<Manifest, String> {
    // TOML (`name = "app"`) or JSON (`{"name": ..}`): show the one value.
    let example = |e: String| {
        rokc_rokon::with_help(
            &e,
            "a `rok.rokon` holds one `Package { .. }`: `Package { name: \"app\", version: \
             \"0.1.0\", deps: {} }`",
        )
    };
    let doc = Doc::parse(text, file).map_err(|e| {
        if e.contains("holds one value") {
            example(e)
        } else {
            e
        }
    })?;
    let pkg = doc.root().record("Package").map_err(example)?;
    pkg.only(&FIELDS)?;
    let string = |f: &str| pkg.opt(f).map(|d| d.string()).transpose();
    let deps = |f: &str| {
        pkg.opt(f)
            .map_or(Ok(Vec::new()), |d| deps(d, f == "dev_deps"))
    };
    // A list of non-empty strings.
    let list = |f: &str| -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for d in pkg
            .opt(f)
            .map(|d| d.list())
            .transpose()?
            .unwrap_or_default()
        {
            let s = d.string()?;
            if s.trim().is_empty() {
                return Err(d.error(&format!(
                    "an entry of `{f}` is empty; remove it or write a value"
                )));
            }
            out.push(s);
        }
        Ok(out)
    };
    let keywords = list("keywords")?;
    if let Some(d) = pkg
        .opt("keywords")
        .filter(|_| keywords.len() > MAX_KEYWORDS)
    {
        return Err(d.error(&format!(
            "`keywords` has {} entries; keep at most {MAX_KEYWORDS}, e.g. `keywords: [\"json\", \
             \"parser\"]`",
            keywords.len()
        )));
    }
    let mut lints = Vec::new();
    if let Some(d) = pkg.opt("lints") {
        for (k, v) in d.dict()? {
            let Some(level) = Level::parse(&v.string()?) else {
                return Err(v.error(&format!(
                    "lint `{}` takes \"allow\", \"warn\" or \"deny\"",
                    k.string()?
                )));
            };
            lints.push((k.string()?, level));
        }
    }
    let mut warnings = Vec::new();
    let mut warn = |d: Decoder<'_>, msg: &str| {
        let (line, col) = rokc_rokon::line_col(text, d.span().start as usize);
        warnings.push(format!("{file}:{line}:{col}: {msg}"));
    };
    // The package name is the dependency name others give it.
    if let Some(d) = pkg.opt("name") {
        let name = d.string()?;
        check_name(&name).map_err(|m| d.error(&m))?;
        if let Err(m) = rokc_core::check_os_name("package", &name) {
            warn(d, &m);
        }
    }
    if let Some(d) = pkg.opt("version") {
        if let Err(e) = crate::semver::Version::parse(&d.string()?) {
            warn(
                d,
                &format!("{e}; `rok publish` needs one, e.g. `version: \"0.1.0\"`"),
            );
        }
    }
    if let Some(d) = pkg.opt("toolchain") {
        let pin = d.string()?;
        if crate::toolchain::ToolchainReq::parse(&pin).is_err() {
            warn(
                d,
                &format!("`toolchain: {pin:?}`: {}", crate::toolchain::PIN_FORMS),
            );
        }
    }
    let normal = deps("deps")?;
    let mut features = Features::new();
    let mut at = Vec::new();
    if let Some(d) = pkg.opt("features") {
        for (k, v) in d.dict()? {
            let items = v.list()?;
            let list = items
                .iter()
                .map(Decoder::string)
                .collect::<Result<_, _>>()?;
            features.push((k.string()?, list));
            at.push((k, items));
        }
    }
    check_features(&features, &normal).map_err(|(a, m)| match a {
        At::Feature(i) => at[i].0.error(&m),
        At::Entry(i, j) => at[i].1[j].error(&m),
        At::Dep(k) => pkg
            .opt("deps")
            .and_then(|d| d.dict().ok())
            .and_then(|kv| kv.into_iter().nth(k))
            .map_or_else(|| m.clone(), |(key, _)| key.error(&m)),
    })?;
    let dev = deps("dev_deps")?;
    let native = pkg.opt("native").map_or(Ok(Vec::new()), natives)?;
    for f in ["deps", "dev_deps", "native"] {
        for (k, _) in pkg
            .opt(f)
            .map(|d| d.dict())
            .transpose()?
            .unwrap_or_default()
        {
            if let Err(m) = rokc_core::check_os_name("package", &k.string()?) {
                warn(k, &m);
            }
        }
    }
    Ok(Manifest {
        name: string("name")?,
        version: string("version")?,
        toolchain: string("toolchain")?,
        description: string("description")?,
        license: string("license")?,
        repository: string("repository")?,
        authors: list("authors")?,
        homepage: string("homepage")?,
        documentation: string("documentation")?,
        readme: string("readme")?,
        keywords,
        publish: pkg.opt("publish").map(|d| d.bool()).transpose()?,
        deps: normal,
        dev_deps: dev,
        lints,
        lints_from: string("lints_from")?,
        native,
        features,
        warnings,
    })
}

/// `m.lints` after its `lints_from` chain, outermost first; later wins.
pub fn inherit_lints(m: &Manifest, path: &Path) -> Result<Vec<(String, Level)>, String> {
    let real = rokc_core::real_path(path).map_err(|e| format!("{}: {e}", path.display()))?;
    chain_lints(m, path, &mut vec![real])
}

fn chain_lints(
    m: &Manifest,
    path: &Path,
    seen: &mut Vec<PathBuf>,
) -> Result<Vec<(String, Level)>, String> {
    let mut out = Vec::new();
    if let Some(from) = &m.lints_from {
        let err = |what: String| format!("{}: `lints_from: {from:?}`: {what}", path.display());
        let dir = path.parent().unwrap_or_else(|| Path::new(".")).join(from);
        if !dir.is_dir() {
            return Err(err(format!("{} is not a directory", dir.display())));
        }
        let next = find(&dir).ok_or_else(|| err(format!("no {MANIFEST} in {}", dir.display())))?;
        let real = rokc_core::real_path(&next).map_err(|e| err(e.to_string()))?;
        if seen.contains(&real) {
            let names: Vec<String> = seen
                .iter()
                .chain([&real])
                .map(|p| p.display().to_string())
                .collect();
            return Err(err(format!("cycle: {}", names.join(" -> "))));
        }
        seen.push(real);
        out = chain_lints(&load(&next)?, &next, seen)?;
    }
    out.extend(m.lints.iter().cloned());
    Ok(out)
}

/// A `native` dict: name -> `Dep::native(..)` or a requirement string.
pub fn natives(d: Decoder<'_>) -> Result<Vec<NativeDep>, String> {
    let mut out: Vec<NativeDep> = Vec::new();
    for (k, v) in d.dict()? {
        let name = k.string()?;
        check_name(&name).map_err(|m| k.error(&m))?;
        if let Some(o) = out.iter().find(|o| same(&o.name, &name)) {
            return Err(k.error(&format!(
                "`{name}` and `{}` are the same native package (`-` and `_` are one)",
                o.name
            )));
        }
        let source = if v.string().is_ok() {
            NativeSource::Registry {
                req: req(v)?,
                registry: None,
            }
        } else {
            let n = v
                .variant("Dep")
                .ok()
                .filter(|n| n.name == "native")
                .ok_or_else(|| v.expected("`Dep::native(..)` or a version requirement"))?;
            match n.args.as_slice() {
                [r] => {
                    n.fields.only(&["registry"])?;
                    NativeSource::Registry {
                        req: req(*r)?,
                        registry: n.fields.opt("registry").map(|r| r.string()).transpose()?,
                    }
                }
                [] => {
                    n.fields.only(&["url", "sha256"])?;
                    let sha = n.fields.req("sha256")?;
                    let sha256 = sha.string()?;
                    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err(sha.error("`sha256` is 64 hex digits"));
                    }
                    NativeSource::Direct {
                        url: n.fields.req("url")?.string()?,
                        sha256: sha256.to_ascii_lowercase(),
                    }
                }
                _ => {
                    return Err(v.error(
                        "`Dep::native` takes a version requirement, or `url:` and `sha256:`",
                    ));
                }
            }
        };
        out.push(NativeDep { name, source });
    }
    Ok(out)
}

fn deps(d: Decoder<'_>, dev: bool) -> Result<Vec<Dep>, String> {
    let mut out = Vec::new();
    for (k, v) in d.dict()? {
        let name = k.string()?;
        check_name(&name).map_err(|m| k.error(&m))?;
        if let Some(o) = out.iter().find(|o: &&Dep| same(&o.name, &name)) {
            return Err(k.error(&format!(
                "`{name}` and `{}` are the same package (`-` and `_` are one)",
                o.name
            )));
        }
        let mut dep = dep(v, true, false)?;
        if dev && dep.optional {
            return Err(v.error("a dev dependency cannot be optional"));
        }
        dep.name = name;
        out.push(dep);
    }
    Ok(out)
}

/// A requirement string, checked.
fn req(d: Decoder<'_>) -> Result<String, String> {
    let s = d.string()?;
    crate::semver::Req::parse(&s).map_err(|e| d.error(&e))?;
    Ok(s)
}

/// A `Dep::path(..)` / `Dep::git(..)` / `Dep::version(..)` value, or a
/// requirement string (`"^1.2"`, short for `Dep::version("^1.2")`).
pub fn source(d: Decoder<'_>) -> Result<Source, String> {
    dep(d, false, false).map(|d| d.source)
}

/// `source`, plus feature arguments when `flags` (name left empty);
/// `lenient` skips arguments it does not know (a file a newer rok wrote).
fn dep(d: Decoder<'_>, flags: bool, lenient: bool) -> Result<Dep, String> {
    if let Ok(s) = d.string() {
        // `"geom": "../geom"`: a path where a requirement goes.
        if s.starts_with(['.', '/', '~']) || s.contains(['/', '\\']) {
            return Err(rokc_rokon::with_help(
                &d.error(&format!("`{s}` is a path, not a version requirement")),
                &format!("a path dependency is `Dep::path(\"{s}\")`"),
            ));
        }
        return Ok(Dep::new(
            String::new(),
            Source::Registry {
                req: req(d)?,
                registry: None,
            },
        ));
    }
    let v = d.variant("Dep").map_err(|_| {
        d.expected("`Dep::path(\"..\")`, `Dep::git(\"..\")` or `Dep::version(\"..\")`")
    })?;
    let one = |what: &str| match v.args.as_slice() {
        [x] => x.string(),
        _ => Err(d.error(&format!("`Dep::{}` takes one string, the {what}", v.name))),
    };
    let only = |own: &[&str]| {
        if lenient {
            return Ok(());
        }
        let extra: &[&str] = if flags { &DEP_FLAGS } else { &[] };
        v.fields.only(&[own, extra].concat())
    };
    let source = match v.name {
        "path" => {
            only(&[])?;
            Source::Path(one("path")?)
        }
        "git" => {
            only(&["rev", "tag", "branch"])?;
            let mut refs = Vec::new();
            for k in v
                .fields
                .names()
                .filter(|k| ["rev", "tag", "branch"].contains(k))
            {
                let s = v.fields.req(k)?.string()?;
                refs.push(match k {
                    "rev" => GitRef::Rev(s),
                    "tag" => GitRef::Tag(s),
                    _ => GitRef::Branch(s),
                });
            }
            Source::Git {
                url: one("url")?,
                reference: match refs.len() {
                    0 => GitRef::Default,
                    1 => refs.remove(0),
                    _ => return Err(d.error("give at most one of rev, tag, branch")),
                },
            }
        }
        "version" => {
            only(&["registry"])?;
            let [r] = v.args.as_slice() else {
                return Err(d.error("`Dep::version` takes one string, the version requirement"));
            };
            Source::Registry {
                req: req(*r)?,
                registry: v.fields.opt("registry").map(|r| r.string()).transpose()?,
            }
        }
        "native" => {
            return Err(
                d.error("a native package goes in the `native` field, not in `deps` / `dev_deps`")
            );
        }
        other => {
            return Err(d.error(&format!(
                "unknown dependency kind `Dep::{other}` (expected Dep::path, Dep::git, \
                 Dep::version)"
            )));
        }
    };
    let mut out = Dep::new(String::new(), source);
    if let Some(o) = v.fields.opt("optional") {
        out.optional = o.bool()?;
    }
    if let Some(f) = v.fields.opt("features") {
        for x in f.list()? {
            let name = x.string()?;
            check_feature(&name).map_err(|m| x.error(&m))?;
            out.features.push(name);
        }
    }
    if let Some(o) = v.fields.opt("default_features") {
        out.default_features = o.bool()?;
    }
    Ok(out)
}

/// `text` with `field` (`deps` / `dev_deps`) entry `key` set to the
/// ROKON text `value`; the rest of the text (comments too) as it was.
pub fn set_dep(
    text: &str,
    file: &str,
    field: &str,
    key: &str,
    value: &str,
) -> Result<String, String> {
    parse(text, file)?;
    let out = edit::set(text, &[Step::Field(field), Step::Key(key)], value)
        .map_err(|e| e.render(text, file))?;
    parse(&out, file).map(|_| out)
}

/// `text` with `dep` in `field`; an optional one also gets a feature of
/// its name enabling it, unless the package has one.
pub fn add_dep(text: &str, file: &str, field: &str, dep: &Dep) -> Result<String, String> {
    let m = parse(text, file)?;
    let edit = |text: &str, steps: &[Step<'_>], value: &str| {
        edit::set(text, steps, value).map_err(|e| e.render(text, file))
    };
    let mut out = text.to_owned();
    if dep.optional && !m.features.iter().any(|(f, _)| *f == dep.name) {
        let enable = strings(&[format!("dep:{}", dep.name)]);
        let steps = [Step::Field("features"), Step::Key(&dep.name)];
        out = edit(&out, &steps, &rokc_rokon::to_line(&enable))?;
    }
    out = edit(
        &out,
        &[Step::Field(field), Step::Key(&dep.name)],
        &dep.to_rok(),
    )?;
    parse(&out, file).map(|_| out)
}

/// `text` without `field` entry `key`, or `None` when it has none.
pub fn remove_dep(
    text: &str,
    file: &str,
    field: &str,
    key: &str,
) -> Result<Option<String>, String> {
    parse(text, file)?;
    edit::remove(text, &[Step::Field(field), Step::Key(key)]).map_err(|e| e.render(text, file))
}

/// `$ROK_HOME/installs.rokon` (a `Package` with `deps`): its entries, and
/// what it holds that this rok skipped. Every toolchain shares the file,
/// so fields, arguments and entry kinds a newer rok wrote are skipped
/// (spec 9.10.6).
pub fn parse_installs(text: &str, file: &str) -> Result<(Vec<Dep>, Vec<String>), String> {
    let doc = Doc::parse(text, file)?;
    let pkg = doc.root().record("Package")?;
    let mut skipped: Vec<String> = pkg
        .known(&["deps"])
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut out = Vec::new();
    for (k, v) in pkg
        .opt("deps")
        .map(|d| d.dict())
        .transpose()?
        .unwrap_or_default()
    {
        let name = k.string()?;
        let kind = v.variant("Dep").ok().map(|v| v.name);
        if kind.is_some_and(|k| !["path", "git", "version"].contains(&k)) {
            skipped.push(format!("deps[\"{name}\"]"));
            continue;
        }
        let mut dep = dep(v, true, true)?;
        dep.name = name;
        out.push(dep);
    }
    Ok((out, skipped))
}

/// `installs.rokon` text with `deps` entry `dep.name` set to `dep`, the
/// rest (a newer rok's fields too) as it was.
pub fn set_install(text: &str, file: &str, dep: &Dep) -> Result<String, String> {
    parse_installs(text, file)?;
    let out = edit::set(
        text,
        &[Step::Field("deps"), Step::Key(&dep.name)],
        &dep.to_rok(),
    )
    .map_err(|e| e.render(text, file))?;
    parse_installs(&out, file).map(|_| out)
}

/// `installs.rokon` text without entry `name`, or `None` when it has none.
pub fn remove_install(text: &str, file: &str, name: &str) -> Result<Option<String>, String> {
    parse_installs(text, file)?;
    edit::remove(text, &[Step::Field("deps"), Step::Key(name)]).map_err(|e| e.render(text, file))
}

/// A script's `//! ```rokon` block.
pub struct Block {
    /// A `rok.rokon` value, without the `//!` prefixes.
    pub text: String,
    /// The bytes of the file its lines take, fences included.
    pub span: std::ops::Range<usize>,
}

/// Leading `//!` lines after a shebang: (start, end incl. newline, text).
fn doc_lines(src: &str) -> impl Iterator<Item = (usize, usize, &str)> {
    let start = if rokc_core::shebang_len(src) > 0 {
        src.find('\n').map_or(src.len(), |i| i + 1)
    } else {
        0
    };
    let mut at = start;
    src[start..].split_inclusive('\n').map_while(move |line| {
        let rest = line.trim_end_matches(['\n', '\r']).strip_prefix("//!")?;
        let from = at;
        at += line.len();
        Some((from, at, rest.strip_prefix(' ').unwrap_or(rest)))
    })
}

/// A script's block of leading `//!` lines from a ```` ```rokon ````
/// line to a ```` ``` ```` line.
pub fn script_block(src: &str) -> Option<Block> {
    let mut open = None;
    let mut text = String::new();
    for (from, to, l) in doc_lines(src) {
        match open {
            None if l.trim() == "```rokon" => open = Some(from),
            Some(o) if l.trim() == "```" => {
                return Some(Block { text, span: o..to });
            }
            Some(_) => {
                text.push_str(l);
                text.push('\n');
            }
            None => {}
        }
    }
    None
}

/// `src` with `text` as its block, replacing the old one or placed after
/// the shebang and leading `//!` lines; `None` removes it.
pub fn set_script_block(src: &str, text: Option<&str>) -> String {
    let lines = text.map_or_else(String::new, |t| {
        let mut out = "//! ```rokon\n".to_owned();
        for l in t.lines() {
            out += &if l.is_empty() {
                "//!\n".to_owned()
            } else {
                format!("//! {l}\n")
            };
        }
        out + "//! ```\n"
    });
    if let Some(b) = script_block(src) {
        return format!("{}{lines}{}", &src[..b.span.start], &src[b.span.end..]);
    }
    let at = doc_lines(src).last().map_or_else(
        || {
            if rokc_core::shebang_len(src) > 0 {
                src.find('\n').map_or(src.len(), |i| i + 1)
            } else {
                0
            }
        },
        |(_, to, _)| to,
    );
    let gap = if at > 0 && !src[..at].ends_with('\n') {
        "\n"
    } else {
        ""
    };
    format!("{}{gap}{lines}{}", &src[..at], &src[at..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "// the app\nPackage {\n    name: \"app\",\n    toolchain: \"0.3\",\n    \
                       deps: {\n        \"a\": Dep::path(\"../a\"), // local\n        \"b\": \
                       Dep::git(\"u\", tag: \"v1\"),\n    },\n    dev_deps: { \"c\": \
                       Dep::git(\"u2\") },\n}\n";

    #[test]
    fn package_metadata() {
        let m = parse(
            "Package { name: \"app\", authors: [\"Ann <a@x.org>\"], homepage: \"https://x.org\", \
             documentation: \"https://x.org/doc\", readme: \"doc/README.md\", keywords: [\"a\", \
             \"b\", \"c\", \"d\", \"e\"], publish: false }",
            "f",
        )
        .unwrap();
        assert_eq!(m.authors, ["Ann <a@x.org>"]);
        assert_eq!(m.homepage.as_deref(), Some("https://x.org"));
        assert_eq!(m.documentation.as_deref(), Some("https://x.org/doc"));
        assert_eq!(m.readme.as_deref(), Some("doc/README.md"));
        assert_eq!(m.keywords.len(), MAX_KEYWORDS);
        assert_eq!(m.publish, Some(false));
        let e = m.check_publish().unwrap_err();
        assert!(e.contains("`app` has `publish: false`"), "{e}");
        assert!(e.contains("write `publish: true`"), "{e}");
        let d = parse("Package {}", "f").unwrap();
        assert!(d.authors.is_empty() && d.keywords.is_empty() && d.publish.is_none());
        d.check_publish().unwrap();
        for (src, want) in [
            (
                "Package { keywords: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\"] }",
                "`keywords` has 6 entries; keep at most 5",
            ),
            (
                "Package { keywords: [\"a\", \"\"] }",
                "an entry of `keywords` is empty",
            ),
            (
                "Package { authors: [\" \"] }",
                "an entry of `authors` is empty",
            ),
            ("Package { keywords: \"a\" }", "must be a list"),
            ("Package { publish: \"no\" }", "must be a bool"),
        ] {
            let e = parse(src, "f").unwrap_err();
            assert!(e.contains(want), "{src}: {e}");
        }
        // readme: as written, else README.md when present.
        let dir = rokc_core::tmp::test_dir("manifest-readme");
        assert_eq!(d.readme_path(&dir).unwrap(), None);
        std::fs::write(dir.join("README.md"), "x").unwrap();
        assert_eq!(d.readme_path(&dir).unwrap(), Some(dir.join("README.md")));
        let e = m.readme_path(&dir).unwrap_err();
        assert!(e.contains("`readme: \"doc/README.md\"`"), "{e}");
        assert!(e.contains("remove `readme` to use README.md"), "{e}");
        let at = |r: &str| {
            Manifest {
                readme: Some(r.into()),
                ..Manifest::default()
            }
            .readme_path(&dir)
        };
        std::fs::create_dir(dir.join("doc")).unwrap();
        assert_eq!(
            at("doc/../README.md").unwrap(),
            Some(dir.join("doc/../README.md"))
        );
        assert_eq!(at("./README.md").unwrap(), Some(dir.join("./README.md")));
        let abs = dir.join("README.md").display().to_string();
        for r in ["../README.md", "doc/../../README.md", abs.as_str(), ""] {
            let e = at(r).unwrap_err();
            assert!(
                e.contains("is outside the package; write a path inside it"),
                "{r}: {e}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toolchain_pin_skips_unknown_fields() {
        let text = "Package { toolchain: \"0.3\", from_a_newer_rok: [1], deps: {} }";
        assert!(parse(text, "f").is_err());
        assert_eq!(toolchain_pin(text).as_deref(), Some("0.3"));
        assert_eq!(toolchain_pin("Package { deps: {} }"), None);
        assert_eq!(toolchain_pin("not rokon {"), None);
    }

    #[test]
    fn manifest_forms_and_errors() {
        let m = parse(APP, "rok.rokon").unwrap();
        assert_eq!(m.toolchain.as_deref(), Some("0.3"));
        assert_eq!(m.deps[0].source, Source::Path("../a".into()));
        assert_eq!(
            m.deps[1].source,
            Source::Git {
                url: "u".into(),
                reference: GitRef::Tag("v1".into())
            }
        );
        assert_eq!(m.dev_deps[0].name, "c");
        let m = parse(
            "Package { lints: { \"unused\": \"allow\", \"all\": \"deny\" }, deps: {} }",
            "rok.rokon",
        )
        .unwrap();
        assert_eq!(
            m.lints,
            [
                ("unused".to_owned(), Level::Allow),
                ("all".to_owned(), Level::Deny)
            ]
        );
        assert_eq!(
            parse("Package { name: \"a\\u{62}\" }", "f")
                .unwrap()
                .name
                .as_deref(),
            Some("ab")
        );
        // The root name is optional; `none` is a missing field.
        let m = parse("{ name: \"x\", toolchain: none }", "f").unwrap();
        assert_eq!(m.name.as_deref(), Some("x"));
        assert!(m.toolchain.is_none());
        assert!(parse("{}", "f").unwrap().deps.is_empty());
        for (src, want) in [
            (
                "Package { deps: { \"a\": \"1.0.x.y\" } }",
                "is not a version requirement",
            ),
            (
                "Package { deps: { \"a\": Dep::version(\"1\", tag: \"x\") } }",
                "unknown argument `tag`",
            ),
            (
                "Package { deps: { \"a\": Dep::version() } }",
                "takes one string, the version requirement",
            ),
            (
                "Package { deps: { \"a\": Dep::registry(\"u\") } }",
                "unknown dependency kind `Dep::registry`",
            ),
            (
                "Package { deps: { \"a\": Dep::native(\"z\") } }",
                "goes in the `native` field",
            ),
            ("Package { native: 1 }", "`native` must be a dict"),
            (
                "Package { native: { \"z\": Dep::path(\"x\") } }",
                "expected `Dep::native(..)`",
            ),
            (
                "Package { native: { \"z\": Dep::native(url: \"u\", sha256: \"ab\") } }",
                "64 hex digits",
            ),
            (
                "Package { native: { \"z\": Dep::native(url: \"u\") } }",
                "missing argument `sha256`",
            ),
            (
                "Package { native: { \"z\": Dep::native(\"1\", url: \"u\") } }",
                "unknown argument `url`",
            ),
            ("Package { registry: {} }", "unknown field `registry`"),
            (
                "Package { deps: { \"My Dep\": Dep::path(\"x\") } }",
                "try `my-dep`",
            ),
            (
                "Package { deps: { \"a-b\": Dep::path(\"x\"), \"a_b\": Dep::path(\"y\") } }",
                "`a_b` and `a-b` are the same package",
            ),
            ("Package { name: \"1x\" }", "starting with a letter"),
            (
                "Package { deps: { \"a\": Dep::git(\"u\", tag: \"t\", rev: \"r\") } }",
                "at most one",
            ),
            (
                "Package { deps: { \"a\": Dep::git(\"u\", sha: \"t\") } }",
                "unknown argument `sha`",
            ),
            (
                "Package { deps: { \"a\": Dep::path(\"p\", tag: \"t\") } }",
                "unknown argument `tag`",
            ),
            (
                "Package { deps: { \"a\": Dep::path() } }",
                "takes one string, the path",
            ),
            (
                "Package { deps: { \"a\": Dep::svn(\"u\") } }",
                "unknown dependency kind `Dep::svn`",
            ),
            ("Package { deps: { \"a\": 1 } }", "found an integer"),
            (
                "Package { deps: { \"a\": Dep::path(\"x\"), \"a\": Dep::path(\"y\") } }",
                "given twice",
            ),
            ("Package { deps: [] }", "`deps` must be a dict"),
            ("Package { edition: \"x\" }", "unknown field `edition`"),
            ("Package { name: \"a\", name: \"b\" }", "given twice"),
            ("Package { name: 1 + 2 }", "operator `+` is not allowed"),
            ("Package { name: x }", "bare identifier `x`"),
            (
                "Package { name: 1 }",
                "`name` must be a string, found an integer",
            ),
            (
                "Package { lints: { \"unused\": \"off\" } }",
                "takes \"allow\"",
            ),
            (
                "Manifest { }",
                "expected `Package { .. }`, found a struct `Manifest",
            ),
            ("Package { } Package { }", "holds one value"),
            ("", "expected a value"),
            ("Package { name: \"x }", "unterminated"),
            (
                "Package { features: { \"a\": [\"b\"] } }",
                "`b` is not a feature of this package",
            ),
            (
                "Package { features: { \"a\": [\"b\"], \"b\": [\"c\"], \"c\": [\"b\"] } }",
                "feature cycle: b -> c -> b",
            ),
            (
                "Package { features: { \"A\": [] } }",
                "not a valid feature name",
            ),
            (
                "Package { deps: { \"x\": Dep::path(\"p\", optional: true) } }",
                "optional dependency `x` is not enabled by any feature (add \"dep:x\" to one)",
            ),
            (
                "Package { deps: { \"x\": Dep::path(\"p\") }, features: { \"a\": [\"dep:x\"] } }",
                "dependency `x` is not `optional: true`",
            ),
            (
                "Package { deps: { \"x\": Dep::path(\"p\") }, features: { \"a\": [\"x?/f\"] } }",
                "`?` is for an optional dependency",
            ),
            (
                "Package { features: { \"a\": [\"y/f\"] } }",
                "there is no dependency `y`",
            ),
            (
                "Package { dev_deps: { \"x\": Dep::path(\"p\", optional: true) } }",
                "a dev dependency cannot be optional",
            ),
            (
                "Package { deps: { \"x\": Dep::git(\"u\", features: [\"B\"]) } }",
                "`B` is not a valid feature name",
            ),
            (
                "Package { deps: { \"x\": Dep::version(\"1\", optional: 1) } }",
                "found an integer",
            ),
        ] {
            let e = parse(src, "rok.rokon").unwrap_err();
            assert!(e.contains(want), "{src:?}: {e}");
            assert!(e.contains("--> rok.rokon:1:"), "{e}");
        }
        // The position is the offending token's.
        let e = parse(
            "Package {\n    name: \"a\",\n    edtion: \"x\",\n}",
            "rok.rokon",
        )
        .unwrap_err();
        assert!(
            e.contains("unknown field `edtion`") && e.contains("--> rok.rokon:3:5\n"),
            "{e}"
        );
        let e = parse(APP.replace("tag", "tga").as_str(), "rok.rokon").unwrap_err();
        assert!(
            e.contains("unknown argument `tga`") && e.contains("--> rok.rokon:7:28\n"),
            "{e}"
        );
    }

    #[test]
    fn edits_keep_the_rest() {
        let src = Source::Path("x".into()).to_rok();
        assert_eq!(src, "Dep::path(\"x\")");
        let t = set_dep(APP, "f", "deps", "a", &src).unwrap();
        assert!(t.contains("\"a\": Dep::path(\"x\"), // local\n"), "{t}");
        let t = set_dep("Package {\n    name: \"n\"\n}\n", "f", "deps", "d", &src).unwrap();
        assert_eq!(parse(&t, "f").unwrap().deps[0].name, "d");
        let t = remove_dep(APP, "f", "dev_deps", "c").unwrap().unwrap();
        assert!(parse(&t, "f").unwrap().dev_deps.is_empty());
        assert!(remove_dep(APP, "f", "deps", "zz").unwrap().is_none());
        // An edit that makes a bad manifest is refused.
        assert!(set_dep(APP, "f", "deps", "My.x", &src).is_err());
        // `-` is a package name's, `_` its code name's.
        assert_eq!(code_name("rok-syntax"), "rok_syntax");
        assert!(same("rok-syntax", "Rok_Syntax") && !same("rok-syntax", "roksyntax"));
        let git = Source::Git {
            url: "u".into(),
            reference: GitRef::Branch("b".into()),
        };
        assert_eq!(git.to_rok(), "Dep::git(\"u\", branch: \"b\")");
        let m = parse(
            "{ deps: { \"a\": \"^1.2\", \"b\": Dep::version(\"~0.3\", registry: \"file:///i\") } }",
            "f",
        )
        .unwrap();
        let back: Vec<String> = m.deps.iter().map(|d| d.source.to_rok()).collect();
        assert_eq!(
            back,
            [
                "Dep::version(\"^1.2\")",
                "Dep::version(\"~0.3\", registry: \"file:///i\")"
            ]
        );
        // Feature arguments: on every kind, written when not the default.
        let m = parse(
            "{ deps: { \"t\": Dep::git(\"u\", tag: \"v\", optional: true, features: [\"a\"], \
             default_features: false), \"j\": Dep::path(\"j\", default_features: true) }, \
             features: { \"default\": [\"t?/b\", \"j/c\"], \"tls\": [\"dep:t\"] } }",
            "f",
        )
        .unwrap();
        let back: Vec<String> = m.deps.iter().map(Dep::to_rok).collect();
        assert_eq!(
            back,
            [
                "Dep::git(\"u\", tag: \"v\", optional: true, features: [\"a\"], default_features: \
                 false)",
                "Dep::path(\"j\")"
            ]
        );
        assert_eq!(m.features[0].1, ["t?/b", "j/c"]);
    }

    #[test]
    fn script_blocks() {
        let text = |s: &str| script_block(s).map(|b| b.text);
        let src = "//! ```rokon\n//! Package { deps: {} }\n//! ```\nprintln(\"hi\");\n";
        assert_eq!(text(src).as_deref(), Some("Package { deps: {} }\n"));
        let shebang = format!("#!/usr/bin/env rok\n{src}");
        assert_eq!(text(&shebang).as_deref(), Some("Package { deps: {} }\n"));
        assert_eq!(text("//! ```rokon\n//! x\nfn main() {}"), None);
        assert_eq!(text("println(1);\n//! ```rokon\n//! ```"), None);
        assert_eq!(text("//! ```rok\n//! let x = 1;\n//! ```\n"), None);
        // Written after the shebang and the doc lines, read back, removed.
        let deps = "Package {\n    deps: {\n        \"a\": \"^1\",\n    },\n}\n";
        for src in [
            "println(1);\n",
            "#!/usr/bin/env rok\nprintln(1);\n",
            "#!/usr/bin/env rok\n//! Says one.\n//!\n//! More.\nprintln(1);\n",
            "//! Doc, no newline",
        ] {
            let with = set_script_block(src, Some(deps));
            assert_eq!(text(&with).as_deref(), Some(deps), "{with}");
            let again = set_script_block(&with, Some(deps));
            assert_eq!(again, with);
            let back = set_script_block(&with, None);
            let want = if src.ends_with('\n') {
                src.to_owned()
            } else {
                format!("{src}\n")
            };
            assert_eq!(back, want);
        }
        let with = set_script_block("#!/usr/bin/env rok\n//! Says one.\nx\n", Some(deps));
        assert!(
            with.starts_with("#!/usr/bin/env rok\n//! Says one.\n//! ```rokon\n//! Package {\n")
        );
    }
}
