//! WIT for rok: `.wit` files named by `ffi::wit::import` /
//! `export` are parsed and resolved ([`load`]) and bound through the
//! canonical ABI ([`bind`]), plus the world for `wasm-component-ld`.
//!
//! A path is a `.wit` file or a directory (one package, `deps/` holding
//! its dependencies), as `wit-parser` reads them; a file is read with its
//! directory but names only what it defines.

// Code generation reads best with the canonical ABI's own short names
// (`t` type, `v` value, `a` address, `p` / `n` pointer and length).
#![allow(clippy::many_single_char_names)]

pub mod abi;
mod bind;
pub mod parse;
pub mod resolve;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

pub use bind::{Bindings, bind};
pub use parse::Error;
pub use resolve::{IfaceId, Resolve, WorldId};

/// What one `ffi::wit` item asks for, by path.
#[derive(Clone, Debug)]
pub struct Request {
    pub export: bool,
    pub paths: Vec<PathBuf>,
}

/// Files served instead of the disk, by [`normal`] path: a prebuilt
/// dependency's WIT, carried in its ROKHIR.
pub type Overlay = HashMap<PathBuf, String>;

/// One package source the linker's WIT directory is made of: a file or
/// a directory (its `*.wit` files), as indices into `Resolve::files`.
#[derive(Debug)]
pub struct Unit {
    pub path: PathBuf,
    pub dir: bool,
    pub files: Vec<usize>,
}

/// Files read: those requests name (`roots`, per path its indices in
/// `Resolve::files`) and their dependencies.
#[derive(Debug)]
pub struct Loaded {
    pub resolve: Resolve,
    pub roots: Vec<Vec<usize>>,
    pub units: Vec<Unit>,
}

/// `p` absolute, without `.` and `..` (the disk is not asked).
#[must_use]
pub fn normal(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_owned());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn cannot(path: &Path, e: &std::io::Error) -> Error {
    Error {
        path: path.to_owned(),
        line: 0,
        col: 0,
        msg: format!("cannot read: {e}"),
    }
}

/// The overlay, else the disk.
struct Fs<'a> {
    overlay: &'a Overlay,
}

impl Fs<'_> {
    /// The overlay's path for `p` when it holds `p` or files under it.
    fn served(&self, p: &Path) -> Option<PathBuf> {
        let n = normal(p);
        self.overlay.keys().any(|k| k.starts_with(&n)).then_some(n)
    }

    fn canonical(&self, p: &Path) -> Result<PathBuf, Error> {
        match self.served(p) {
            Some(n) => Ok(n),
            None => std::fs::canonicalize(p).map_err(|e| cannot(p, &e)),
        }
    }

    fn is_dir(&self, p: &Path) -> bool {
        self.served(p)
            .is_some_and(|n| !self.overlay.contains_key(&n))
            || p.is_dir()
    }

    fn read(&self, p: &Path) -> Result<(parse::File, String), Error> {
        let src = match self.overlay.get(&normal(p)) {
            Some(t) => t.clone(),
            None => std::fs::read_to_string(p).map_err(|e| cannot(p, &e))?,
        };
        Ok((parse::parse_file(p, &src)?, src))
    }

    /// The entries of directory `dir`, sorted.
    fn entries(&self, dir: &Path) -> Result<Vec<PathBuf>, Error> {
        let mut out: Vec<PathBuf> = match self.served(dir) {
            Some(n) => self
                .overlay
                .keys()
                .filter_map(|k| Some(n.join(k.strip_prefix(&n).ok()?.components().next()?)))
                .collect(),
            None => std::fs::read_dir(dir)
                .map_err(|e| cannot(dir, &e))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect(),
        };
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// The `*.wit` files of `dir`, sorted; a file without `package`
    /// takes the one another file of the directory declares.
    fn read_dir(&self, dir: &Path) -> Result<Vec<(parse::File, String)>, Error> {
        let mut files = self
            .entries(dir)?
            .iter()
            .filter(|p| !self.is_dir(p) && p.extension().is_some_and(|x| x == "wit"))
            .map(|p| self.read(p))
            .collect::<Result<Vec<_>, _>>()?;
        let pkg = files.iter().find_map(|(f, _)| f.package.clone());
        for (f, _) in &mut files {
            if f.package.is_none() {
                f.package.clone_from(&pkg);
            }
        }
        Ok(files)
    }

    /// The files of `path` (a file or a directory) as a unit, read once.
    fn add(
        &self,
        path: &Path,
        files: &mut Vec<(parse::File, String)>,
        units: &mut Vec<Unit>,
    ) -> Result<Vec<usize>, Error> {
        let canon = self.canonical(path)?;
        if let Some(u) = units.iter().find(|u| u.path == canon) {
            return Ok(u.files.clone());
        }
        let dir = self.is_dir(&canon);
        let read = if dir {
            self.read_dir(&canon)?
        } else {
            vec![self.read(&canon)?]
        };
        let ids: Vec<usize> = (files.len()..files.len() + read.len()).collect();
        files.extend(read);
        units.push(Unit {
            path: canon,
            dir,
            files: ids.clone(),
        });
        Ok(ids)
    }
}

/// Read and resolve every path of `requests` (and their `deps/`), from
/// `overlay` where it has them.
pub fn load(requests: &[Request], overlay: &Overlay) -> Result<Loaded, Error> {
    let fs = Fs { overlay };
    let mut files: Vec<(parse::File, String)> = Vec::new();
    let mut units = Vec::new();
    let mut roots = Vec::new();
    let mut deps_dirs = Vec::new();
    for req in requests {
        for p in &req.paths {
            // A file is read with its directory (its package), but names
            // only what it defines.
            let is_dir = fs.is_dir(p);
            let dir = if is_dir {
                p.clone()
            } else {
                p.parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
            };
            let mut ids = fs.add(&dir, &mut files, &mut units)?;
            if !is_dir {
                let canon = fs.canonical(p)?;
                ids.retain(|&i| files[i].0.path == canon);
            }
            roots.push(ids);
            if fs.is_dir(&dir.join("deps")) {
                deps_dirs.push(dir.join("deps"));
            }
        }
    }
    // Dependencies: each entry of a `deps/` directory is a package, and
    // may have `deps/` of its own.
    while let Some(d) = deps_dirs.pop() {
        for e in fs.entries(&d).unwrap_or_default() {
            let dir = fs.is_dir(&e);
            if dir || e.extension().is_some_and(|x| x == "wit") {
                fs.add(&e, &mut files, &mut units)?;
                if dir && fs.is_dir(&e.join("deps")) {
                    deps_dirs.push(e.join("deps"));
                }
            }
        }
    }
    let resolve = Resolve::new(files)?;
    Ok(Loaded {
        resolve,
        roots,
        units,
    })
}

#[cfg(test)]
mod tests;
