//! Package-level incremental builds (902.16): skip a build whose output
//! exists and whose inputs didn't change. The record (`.<out>.fp`) holds
//! the key, the output's `file_stamp` and the rendered warnings, which a
//! skipped build reprints.
//!
//! The key covers the compiler, report style, options and paths, every
//! source the build can read and the prebuilt archives it links. Files read
//! beyond those (`#[path]` outside the package, `include_str!`) are listed
//! with content hashes. Any change rebuilds the whole package.

use std::path::{Path, PathBuf};

use rokc_core::file_stamp;
use rokc_core::hash::{FNV_OFFSET, fnv_add};

use crate::package::source_files;

/// A build's fingerprint and where it is kept.
pub struct Fingerprint {
    key: String,
    /// False when an input could not be read while computing the key.
    cacheable: bool,
    file: PathBuf,
    out: PathBuf,
    /// More files that must exist (a library's ROKHIR).
    also: Vec<PathBuf>,
    /// The inputs the key covers, canonical.
    covered: Vec<PathBuf>,
}

fn content_hash(p: &Path) -> std::io::Result<String> {
    std::fs::read(p).map(|bytes| format!("{:016x}", fnv_add(FNV_OFFSET, &bytes)))
}

impl Fingerprint {
    /// `words`: options and paths; `inputs`: files read (a directory: its
    /// sources); `stamped`: big files tracked by `file_stamp`.
    #[must_use]
    pub fn new(
        out: &Path,
        also: Vec<PathBuf>,
        words: &[String],
        inputs: &[PathBuf],
        stamped: &[PathBuf],
    ) -> Self {
        let mut h = FNV_OFFSET;
        let own = [
            rokc_driver::rokhir::producer(),
            rokc_diagnostic::report_style().to_string(),
        ];
        for w in own.iter().chain(words) {
            h = fnv_add(fnv_add(h, w.as_bytes()), b"\0");
        }
        for p in stamped {
            h = fnv_add(
                h,
                format!("{}\0{}\0", p.display(), file_stamp(p)).as_bytes(),
            );
        }
        let mut files = Vec::new();
        let mut cacheable = true;
        let covered: Vec<PathBuf> = inputs
            .iter()
            .filter_map(|i| {
                if let Ok(p) = i.canonicalize() {
                    Some(p)
                } else {
                    cacheable = false;
                    None
                }
            })
            .collect();
        // Canonical: `run s.rok` in two directories is two builds (the
        // record lists the files read beside it by full path).
        for i in &covered {
            if i.is_file() {
                files.push(i.clone());
            } else {
                files.extend(source_files(i));
            }
        }
        files.sort();
        files.dedup();
        for f in files {
            h = fnv_add(h, f.display().to_string().as_bytes());
            match std::fs::read(&f) {
                Ok(bytes) => h = fnv_add(h, &bytes),
                Err(_) => cacheable = false,
            }
        }
        let name = out
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        Self {
            key: format!("{h:016x}"),
            cacheable,
            file: out.with_file_name(format!(".{name}.fp")),
            out: out.to_path_buf(),
            also,
            covered,
        }
    }

    /// Keep the record in `file` instead of `.<out>.fp` beside the output.
    #[must_use]
    pub fn kept_in(mut self, file: PathBuf) -> Self {
        self.file = file;
        self
    }

    /// The hash of everything the build is made from.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The same build into `out` instead (its record beside it), with
    /// `also` to exist.
    #[must_use]
    pub fn moved_to(self, out: &Path, also: Vec<PathBuf>) -> Self {
        let name = out.file_name().unwrap_or_default().to_string_lossy();
        Self {
            file: out.with_file_name(format!(".{name}.fp")),
            out: out.to_path_buf(),
            also,
            ..self
        }
    }

    fn head(&self) -> String {
        format!("{}\n{}\n", self.key, file_stamp(&self.out))
    }

    /// The recorded warnings when the output is up to date.
    #[must_use]
    pub fn fresh(&self) -> Option<String> {
        if !self.cacheable || !self.out.is_file() || !self.also.iter().all(|p| p.is_file()) {
            return None;
        }
        let record = std::fs::read_to_string(&self.file).ok()?;
        let rest = record.strip_prefix(&self.head())?;
        let (n, mut rest) = rest.split_once('\n')?;
        for _ in 0..n.parse::<usize>().ok()? {
            let (line, tail) = rest.split_once('\n')?;
            let (hash, path) = line.split_once(' ')?;
            if content_hash(Path::new(path)).ok()? != hash {
                return None;
            }
            rest = tail;
        }
        Some(rest.to_owned())
    }

    /// Before building: a failed build leaves no record.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.file);
    }

    /// After a successful build that read `read` and printed `warnings`.
    pub fn save(&self, read: &[PathBuf], warnings: &str) {
        if !self.cacheable {
            self.clear();
            return;
        }
        let extra: std::io::Result<Vec<PathBuf>> = read.iter().map(|p| p.canonicalize()).collect();
        let Ok(extra) = extra else {
            self.clear();
            return;
        };
        let mut extra: Vec<PathBuf> = extra
            .into_iter()
            .filter(|p| {
                !self.covered.iter().any(|c| p.starts_with(c))
                    || !p.extension().is_some_and(|x| x == "rok" || x == "rokon")
            })
            .collect();
        extra.sort();
        extra.dedup();
        let mut record = self.head() + &format!("{}\n", extra.len());
        for p in &extra {
            let Ok(hash) = content_hash(p) else {
                self.clear();
                return;
            };
            record += &format!("{hash} {}\n", p.display());
        }
        let _ = std::fs::write(&self.file, record + warnings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_unreadable_included_file_invalidates_the_record() {
        let dir = rokc_core::tmp::test_dir("fresh-missing-include");
        let input = dir.join("empty.txt");
        let out = dir.join("app");
        std::fs::write(&input, "").unwrap();
        std::fs::write(&out, "exe").unwrap();
        let fp = Fingerprint::new(&out, vec![], &[], &[], &[]);
        fp.save(std::slice::from_ref(&input), "");
        assert!(fp.fresh().is_some());
        std::fs::remove_file(&input).unwrap();
        assert!(fp.fresh().is_none());
        std::fs::create_dir(&input).unwrap();
        assert!(fp.fresh().is_none());
        fp.save(std::slice::from_ref(&input), "");
        assert!(!fp.file.exists());
        std::fs::remove_dir(&input).unwrap();
        fp.save(&[], "");
        assert!(fp.fresh().is_some());
        fp.save(&[input], "");
        assert!(!fp.file.exists());
        let missing = dir.join("missing.rok");
        let fp = Fingerprint::new(&out, vec![], &[], &[missing], &[]);
        fp.save(&[], "");
        assert!(!fp.file.exists());
        assert!(fp.fresh().is_none());
    }

    #[test]
    fn cfg_include_cache_recovers_after_restoring_a_missing_file() {
        let dir = rokc_core::tmp::test_dir("fresh-disabled-include");
        let src = dir.join("main.rok");
        let included = dir.join("optional.txt");
        let out = dir.join("app");
        std::fs::write(
            &src,
            "#[cfg(feature = \"optional\")]\nconst NOTE: str = include_str!(\"optional.txt\");\nfn main() {}\n",
        )
        .unwrap();
        std::fs::write(&included, "").unwrap();
        let mut session = crate::Session::new();
        let result = session.compile_file(&src);
        assert!(!result.has_errors(), "{:?}", result.errors);
        // Includes expand during parsing, before cfg removes the item.
        assert!(result.files.contains(&included), "{:?}", result.files);
        std::fs::write(&out, "exe").unwrap();
        let inputs = [dir];
        let fp = Fingerprint::new(&out, vec![], &[], &inputs, &[]);
        fp.save(&result.files, "");
        assert!(
            Fingerprint::new(&out, vec![], &[], &inputs, &[])
                .fresh()
                .is_some()
        );

        std::fs::remove_file(&included).unwrap();
        assert!(fp.fresh().is_none());
        fp.save(&result.files, "");
        assert!(!fp.file.exists());
        let failed = session.compile_file(&src);
        assert!(failed.has_errors());
        assert!(
            failed
                .errors
                .iter()
                .any(|d| d.message.contains("include_str!"))
        );

        std::fs::write(&included, "").unwrap();
        let restored = session.compile_file(&src);
        assert!(!restored.has_errors(), "{:?}", restored.errors);
        let fp = Fingerprint::new(&out, vec![], &[], &inputs, &[]);
        fp.save(&restored.files, "");
        assert!(fp.fresh().is_some());

        let flags = ["feature=\"optional\"".to_owned()];
        let active = Fingerprint::new(&out, vec![], &flags, &inputs, &[]);
        assert!(active.fresh().is_none());
        session.add_cfg(flags[0].clone());
        let result = session.compile_file(&src);
        assert!(!result.has_errors(), "{:?}", result.errors);
        active.save(&result.files, "");
        assert!(active.fresh().is_some());
        std::fs::remove_file(&included).unwrap();
        assert!(active.fresh().is_none());
    }

    #[test]
    fn fresh_until_an_input_or_the_output_changes() {
        let dir = rokc_core::tmp::test_dir("fresh");
        std::fs::create_dir_all(dir.join("target")).unwrap();
        let src = dir.join("main.rok");
        let out = dir.join("target/app");
        std::fs::write(&src, "fn main() {}").unwrap();
        // `target/` is not an input.
        std::fs::write(dir.join("target/skip.rok"), "x").unwrap();
        let fp = |w: &str| Fingerprint::new(&out, vec![], &[w.into()], &[dir.clone()], &[]);
        assert!(fp("a").fresh().is_none());
        std::fs::write(&out, "exe").unwrap();
        fp("a").save(&[], "warning: w\n");
        assert_eq!(fp("a").fresh().as_deref(), Some("warning: w\n"));
        assert!(fp("b").fresh().is_none());
        std::fs::write(dir.join("target/skip.rok"), "y").unwrap();
        assert!(fp("a").fresh().is_some());
        std::fs::write(&src, "fn main() { }").unwrap();
        assert!(fp("a").fresh().is_none());
        fp("a").save(&[], "");
        std::fs::write(&out, "other exe").unwrap();
        assert!(fp("a").fresh().is_none());
        fp("a").save(&[], "");
        fp("a").clear();
        assert!(fp("a").fresh().is_none());
        // A file read outside the inputs is checked by content.
        let outside = rokc_core::tmp::test_dir("fresh-outside").join("a.rok");
        std::fs::write(&outside, "a").unwrap();
        fp("a").save(&[src, outside.clone()], "");
        assert!(fp("a").fresh().is_some());
        std::fs::write(&outside, "b").unwrap();
        assert!(fp("a").fresh().is_none());
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
