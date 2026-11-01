//! The git cache, shared by every project and environment:
//!
//! ```text
//! $ROK_HOME/cache/git/db/<repo>-<hash of url>/   bare clone, one per url
//! $ROK_HOME/cache/git/checkouts/<commit>/        a commit's files
//! $ROK_HOME/cache/registry/db/<repo>-<hash>/     bare clone of a stone index
//! ```
//!
//! All through the `git` CLI.

// `^{commit}` / `^{tree}` are git revision syntax, not format arguments.
#![allow(clippy::literal_string_with_formatting_args)]

use std::path::{Path, PathBuf};
use std::process::Command;

use rokc_core::hash::fnv;
use rokc_core::term::{self, Role, Stream};

use crate::manifest::GitRef;

pub struct Cache {
    root: PathBuf,
    /// `(url, mirror)`: fetch `url` from `mirror` (`registry::sources`);
    /// the clone stays keyed by `url`.
    replace: Vec<(String, String)>,
}

/// `git -C dir args..` in a working tree (a package's, an index checkout's).
pub fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("cannot run `git` (is it installed?): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`git {}` in {} failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git(dir: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new("git");
    // Checkouts are the committed bytes (the package hash covers them),
    // whatever `core.autocrlf` says.
    cmd.args(["-c", "core.autocrlf=false", "--git-dir"])
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run `git` (is it installed?): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

impl Cache {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join("cache/git"),
            replace: Vec::new(),
        }
    }

    /// The cache of stone indexes (read in place, never checked out),
    /// fetched from the mirrors in `replace`.
    pub fn index(home: &Path, replace: Vec<(String, String)>) -> Self {
        Self {
            root: home.join("cache/registry"),
            replace,
        }
    }

    /// Where `url` is fetched from.
    fn remote<'a>(&'a self, url: &'a str) -> &'a str {
        self.replace
            .iter()
            .find(|(u, _)| u.trim_end_matches('/') == url.trim_end_matches('/'))
            .map_or(url, |(_, m)| m.as_str())
    }

    pub fn has(&self, url: &str) -> bool {
        self.db(url).join("HEAD").is_file()
    }

    /// File `path` at the fetched HEAD of `url`, or `None` when absent.
    pub fn show(&self, url: &str, path: &str) -> Result<Option<String>, String> {
        let db = self.db(url);
        let spec = format!("refs/rok/HEAD:{path}");
        if git(&db, &["cat-file", "-e", &spec], None).is_err() {
            return Ok(None);
        }
        git(&db, &["cat-file", "blob", &spec], None).map(Some)
    }

    /// Every file path at the fetched HEAD of `url`.
    pub fn files(&self, url: &str) -> Result<Vec<String>, String> {
        let out = git(
            &self.db(url),
            &["ls-tree", "-r", "--name-only", "refs/rok/HEAD"],
            None,
        )?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    fn db(&self, url: &str) -> PathBuf {
        let base = url
            .trim_end_matches('/')
            .rsplit(['/', ':', '\\'])
            .next()
            .unwrap_or("repo")
            .trim_end_matches(".git");
        let base: String = base
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            .collect();
        self.root
            .join("db")
            .join(format!("{base}-{:016x}", fnv(url.as_bytes())))
    }

    /// Bring the bare clone of `url` up to date (branches, tags, HEAD).
    pub fn fetch(&self, url: &str) -> Result<(), String> {
        let db = self.db(url);
        if !db.join("HEAD").is_file() {
            std::fs::create_dir_all(&db).map_err(|e| format!("{}: {e}", db.display()))?;
            git(&db, &["init", "--quiet", "--bare"], None)?;
        }
        let from = self.remote(url);
        git(
            &db,
            &[
                "fetch",
                "--quiet",
                "--force",
                from,
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
                "+HEAD:refs/rok/HEAD",
            ],
            None,
        )
        .map(drop)
        .map_err(|e| {
            // git's own words, without the refspecs we gave it.
            let why = e.split_once("` failed: ").map_or(e.as_str(), |(_, w)| w);
            let hint = if why.contains("Could not resolve host") || why.contains("unable to access")
            {
                "\n(cannot reach the host; with a cached clone, `--offline` works without it)"
            } else if why.contains("not found")
                || why.contains("does not exist")
                || why.contains("could not read Username")
                || why.contains("does not appear to be a git repository")
            {
                "\n(no such repository, or it needs credentials; check the url)"
            } else {
                ""
            };
            if from == url {
                format!("cannot fetch {url}: {why}{hint}")
            } else {
                format!("cannot fetch {url} from its mirror {from} (sources.rokon): {why}{hint}")
            }
        })
    }

    /// The commit `reference` names in the cached clone of `url`.
    pub fn resolve(&self, url: &str, reference: &GitRef) -> Result<String, String> {
        let spec = match reference {
            GitRef::Default => "refs/rok/HEAD".to_owned(),
            GitRef::Branch(b) => format!("refs/heads/{b}"),
            GitRef::Tag(t) => format!("refs/tags/{t}"),
            GitRef::Rev(r) => r.clone(),
        };
        let db = self.db(url);
        if !db.join("HEAD").is_file() {
            return Err(format!("{url} is not in the cache"));
        }
        git(
            &db,
            &["rev-parse", "--verify", "--quiet", &(spec + "^{commit}")],
            None,
        )
        .map_err(|_| {
            let what = reference.pair().map_or_else(
                || "its default branch".to_owned(),
                |(k, v)| format!("{k} `{v}`"),
            );
            format!("{url} has no {what}")
        })
    }

    pub fn has_commit(&self, url: &str, commit: &str) -> bool {
        let db = self.db(url);
        db.join("HEAD").is_file()
            && git(
                &db,
                &["cat-file", "-e", &(commit.to_owned() + "^{commit}")],
                None,
            )
            .is_ok()
    }

    /// `commit`'s files in `checkouts/<commit>/` (re-extracted if changed)
    /// and its tree hash.
    pub fn checkout(&self, url: &str, commit: &str) -> Result<(PathBuf, String), String> {
        let db = self.db(url);
        let tree = git(&db, &["rev-parse", &(commit.to_owned() + "^{tree}")], None)?;
        let dir = self.root.join("checkouts").join(commit);
        if dir.is_dir() {
            if self.disk_tree(&db, &dir)? == tree {
                return Ok((dir, tree));
            }
            eprintln!(
                "rok: {} {} was modified; checking it out again",
                term::label(Stream::Stderr, Role::Warning),
                dir.display()
            );
            std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = self
            .root
            .join("checkouts")
            .join(format!(".tmp-{commit}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let index = tmp.with_extension("index");
        git(&db, &["read-tree", commit], Some(&index))?;
        let work = format!("--work-tree={}", tmp.display());
        git(
            &db,
            &[&work, "checkout-index", "--all", "--force"],
            Some(&index),
        )?;
        let _ = std::fs::remove_file(&index);
        // Another process may have won the race: theirs is as good.
        if std::fs::rename(&tmp, &dir).is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Ok((dir, tree))
    }

    /// The git tree hash of what is on disk in `dir`.
    fn disk_tree(&self, db: &Path, dir: &Path) -> Result<String, String> {
        let index = self
            .root
            .join(format!(".verify-{}.index", std::process::id()));
        let _ = std::fs::remove_file(&index);
        let work = format!("--work-tree={}", dir.display());
        git(db, &[&work, "add", "--all", "--force", "."], Some(&index))?;
        let tree = git(db, &["write-tree"], Some(&index));
        let _ = std::fs::remove_file(&index);
        tree
    }
}
