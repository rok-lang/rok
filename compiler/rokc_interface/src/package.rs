//! Package discovery (spec 902.2, 0.3, 4.16.8) and the source-tree walk.

use std::path::{Path, PathBuf};

/// A package: a directory with `src/main.rok`, `src/lib.rok` or both, or a
/// lone file outside any `src/` (anonymous executable, 4.16.8).
#[derive(Clone, Debug)]
pub struct Package {
    /// `src/main.rok` when there, else `src/lib.rok`; or the anonymous
    /// file.
    pub root: PathBuf,
    pub dir: PathBuf,
    /// Artifact name: the directory name, or the anonymous file's stem.
    pub name: String,
    pub lib: bool,
    pub anonymous: bool,
    /// `src/lib.rok` when `root` is `src/main.rok`: the executable uses
    /// that library as dependency `name` (0.3.1).
    pub library: Option<PathBuf>,
}

/// The root file name standing for standard input (`check -`, 901.9).
pub const STDIN: &str = "<stdin>";

impl Package {
    /// Source read from stdin: an anonymous package (no file submodules).
    #[must_use]
    pub fn stdin() -> Self {
        Self {
            root: PathBuf::from(STDIN),
            dir: PathBuf::from("."),
            name: "stdin".to_owned(),
            lib: false,
            anonymous: true,
            library: None,
        }
    }

    /// The library of a package that also has `src/main.rok`.
    #[must_use]
    pub fn library_target(&self) -> Option<Self> {
        Some(Self {
            root: self.library.clone()?,
            lib: true,
            library: None,
            ..self.clone()
        })
    }

    /// `target/` (spec 902.15); `ROK_TARGET_DIR` overrides (901.7).
    #[must_use]
    pub fn target_dir(&self) -> PathBuf {
        std::env::var_os("ROK_TARGET_DIR").map_or_else(|| self.dir.join("target"), PathBuf::from)
    }
}

/// Package name `name` as code and symbols spell it: `my-app` is
/// `my_app`.
#[must_use]
pub fn code_name(name: &str) -> String {
    let n: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    if n.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{n}")
    } else {
        n
    }
}

/// The root file of the package at `dir`: `src/main.rok`, else
/// `src/lib.rok` (spec 0.3.1).
#[must_use]
pub fn root_file(dir: &Path) -> Option<PathBuf> {
    ["src/main.rok", "src/lib.rok"]
        .into_iter()
        .map(|f| dir.join(f))
        .find(|f| f.is_file())
}

/// The package of `path`: the nearest directory with `src/main.rok` /
/// `src/lib.rok` (902.2); a file outside its `src/` is anonymous.
pub fn discover(path: &Path) -> Result<Package, String> {
    if !path.exists() {
        return Err(format!("`{}` does not exist", path.display()));
    }
    let abs = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let is_file = abs.is_file();
    let start: &Path = if is_file {
        abs.parent().unwrap_or(&abs)
    } else {
        &abs
    };
    for dir in start.ancestors() {
        let Some(root) = root_file(dir) else {
            continue;
        };
        if is_file && !abs.starts_with(dir.join("src")) {
            break;
        }
        let lib = root.ends_with("lib.rok");
        let library = Some(dir.join("src/lib.rok")).filter(|l| !lib && l.is_file());
        return Ok(Package {
            lib,
            root: shown(&root),
            dir: shown(dir),
            name: dir_name(dir),
            anonymous: false,
            library: library.as_deref().map(shown),
        });
    }
    if !is_file {
        return Err(format!(
            "no `src/main.rok` or `src/lib.rok` in `{}` or any parent directory",
            path.display()
        ));
    }
    if abs.extension().is_none_or(|x| x != "rok") {
        return Err(format!(
            "`{}` is not a Rok source file: Rok sources end in `.rok`",
            path.display()
        ));
    }
    Ok(Package {
        root: shown(&abs),
        dir: shown(start),
        name: abs
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("main")
            .to_owned(),
        lib: false,
        anonymous: true,
        library: None,
    })
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("main")
        .to_owned()
}

/// Relative to the working directory when under it (diagnostics show
/// `src/main.rok`, not an absolute path).
fn shown(p: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    match p.strip_prefix(&cwd) {
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => p.to_path_buf(),
    }
}

/// Every `.rok` and `.rokon` file under `dir`, sorted; `target/` and dot
/// directories are skipped.
#[must_use]
pub fn source_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut |_, _| false, &mut out, &mut |_| {});
    out.sort();
    out
}

/// `source_files` for tools (`fmt`): also skips `node_modules/` and what
/// the `.gitignore` at `dir` or at its package's root ignores (see
/// `Ignore`). `progress` gets the number of entries seen so far.
pub fn source_files_with(dir: &Path, progress: &mut dyn FnMut(usize)) -> Vec<PathBuf> {
    let mut ignores = Vec::new();
    if let Ok(abs) = std::path::absolute(dir) {
        let pkg = package_root(&abs);
        for base in std::iter::once(abs.as_path()).chain(pkg.as_deref().filter(|p| *p != abs)) {
            if let (Ok(text), Ok(prefix)) = (
                std::fs::read_to_string(base.join(".gitignore")),
                abs.strip_prefix(base),
            ) {
                ignores.push(Ignore::parse(&text, prefix.to_path_buf()));
            }
        }
    }
    let mut skip = |rel: &Path, is_dir: bool| {
        is_dir && rel.file_name().is_some_and(|n| n == "node_modules")
            || ignores.iter().any(|i| i.matches(rel, is_dir))
    };
    let mut out = Vec::new();
    walk(dir, &mut skip, &mut out, progress);
    out.sort();
    out
}

/// Walk `root`; `skip` sees each entry relative to `root`.
fn walk(
    root: &Path,
    skip: &mut dyn FnMut(&Path, bool) -> bool,
    out: &mut Vec<PathBuf>,
    progress: &mut dyn FnMut(usize),
) {
    let mut seen = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            seen += 1;
            progress(seen);
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            let is_dir = p.is_dir();
            let rel = p.strip_prefix(root).unwrap_or(&p);
            if is_dir {
                if name != "target" && !name.starts_with('.') && !skip(rel, true) {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|x| x == "rok" || x == "rokon") && !skip(rel, false)
            {
                out.push(p);
            }
        }
    }
}

/// The nearest directory at or above `dir` with `src/main.rok`,
/// `src/lib.rok` or `rok.rokon`: the package `dir` is in, as rok finds
/// it.
#[must_use]
pub fn package_root(dir: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(dir).ok()?;
    abs.ancestors()
        .find(|d| root_file(d).is_some() || d.join("rok.rokon").is_file())
        .map(Path::to_path_buf)
}

/// The `.gitignore` subset the tools honour: `name` (anywhere below),
/// `name/` (directories only), `/name` (directly in the `.gitignore`'s
/// directory) and `*suffix`. Other lines (`!`, `**`, other wildcards,
/// `a/b`) are skipped.
struct Ignore {
    /// The walk root relative to the `.gitignore`'s directory.
    prefix: PathBuf,
    rules: Vec<Rule>,
}

struct Rule {
    name: String,
    anchored: bool,
    dir_only: bool,
    /// `*suffix`: `name` is the suffix.
    suffix: bool,
}

impl Ignore {
    fn parse(text: &str, prefix: PathBuf) -> Self {
        let rules = text
            .lines()
            .filter_map(|l| {
                let l = l.trim_end();
                if l.is_empty() || l.starts_with(['#', '!']) || l.contains('\\') {
                    return None;
                }
                let (l, dir_only) = l.strip_suffix('/').map_or((l, false), |l| (l, true));
                let (l, anchored) = l.strip_prefix('/').map_or((l, false), |l| (l, true));
                let (l, suffix) = l.strip_prefix('*').map_or((l, false), |l| (l, true));
                let plain = !l.is_empty() && !l.contains(['/', '*', '?', '[']);
                plain.then(|| Rule {
                    name: l.to_owned(),
                    anchored,
                    dir_only,
                    suffix,
                })
            })
            .collect();
        Self { prefix, rules }
    }

    fn matches(&self, rel: &Path, is_dir: bool) -> bool {
        let full = self.prefix.join(rel);
        let Some(last) = full.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let top = full.components().count() == 1;
        self.rules.iter().any(|r| {
            (is_dir || !r.dir_only)
                && (!r.anchored || top)
                && if r.suffix {
                    last.ends_with(&r.name)
                } else {
                    last == r.name
                }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitignore_subset() {
        let i = Ignore::parse(
            "# c\nbuild/\n/out\n*.gen.rok\nskip.rok\n!keep\na/b\n**/x\n",
            PathBuf::from("src"),
        );
        assert_eq!(i.rules.len(), 4);
        assert!(i.matches(Path::new("build"), true));
        assert!(!i.matches(Path::new("build"), false));
        // `/out` is beside the `.gitignore`, not under `src/`.
        assert!(!i.matches(Path::new("out"), true));
        assert!(i.matches(Path::new("m/x.gen.rok"), false));
        assert!(i.matches(Path::new("skip.rok"), false));
        assert!(!i.matches(Path::new("keep.rok"), false));
        let top = Ignore::parse("/out\n", PathBuf::new());
        assert!(top.matches(Path::new("out"), true));
        assert!(!top.matches(Path::new("a/out"), true));
    }
}
