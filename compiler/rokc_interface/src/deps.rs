//! Prebuilt dependencies (spec 902.3 / 904): each source `--extern` is
//! built once into `lib<name>.a` + `<name>.rokhir` and the build goes on
//! with the ROKHIR; rebuilt when its fingerprint changes. Each sees the
//! ones before it (its deps, and the GC type ids to start after), so
//! `externs` come dependencies first; each has its own features.

use std::path::{Path, PathBuf};

use rokc_diagnostic::print_rendered;

use crate::fresh::Fingerprint;
use crate::{BuildOptions, Session, TargetSpec, rokhir};

/// What dependency builds share with the build that needs them.
pub struct DepBuild<'a> {
    pub bootstrap: &'a [PathBuf],
    pub cfg: &'a [String],
    /// `(dependency, feature)`: `--extern-feature`.
    pub features: &'a [(String, String)],
    pub opts: &'a BuildOptions,
    pub target: &'a TargetSpec,
    /// `target/[<target>/]<profile>/deps`.
    pub dir: &'a Path,
    pub quiet: bool,
    /// The prebuilt standard library the build links (`prebuilt_std`).
    pub std: Option<&'a Path>,
}

impl DepBuild<'_> {
    /// `externs` with every source dependency (a `src/lib.rok`) replaced by
    /// its prebuilt `<name>.rokhir`; dependency errors are reported.
    pub fn prebuild(
        &self,
        externs: &[(String, PathBuf)],
    ) -> Result<Vec<(String, PathBuf)>, String> {
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        for (name, root) in externs {
            if root.extension().is_some_and(|e| e == "rokhir") {
                out.push((name.clone(), root.clone()));
                continue;
            }
            std::fs::create_dir_all(self.dir)
                .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
            let archive = self.dir.join(format!("lib{name}.a"));
            let rokhir = rokhir::companions(&archive).0;
            let fp = self.fingerprint(name, root, &archive, &out);
            if let Some(warnings) = fp.fresh() {
                print_rendered(&warnings);
            } else {
                fp.clear();
                let (read, warnings) = self.build(name, root, &archive, &out)?;
                fp.save(&read, &warnings);
            }
            out.push((name.clone(), rokhir));
        }
        Ok(out)
    }

    /// Dependency `name`'s cfg: the build's without its features, and
    /// its own.
    fn cfg_of(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .cfg
            .iter()
            .filter(|c| crate::feature_of(c).is_none())
            .cloned()
            .collect();
        out.extend(
            self.features
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, f)| format!("feature=\"{f}\"")),
        );
        out
    }

    fn fingerprint(
        &self,
        name: &str,
        root: &Path,
        archive: &Path,
        before: &[(String, PathBuf)],
    ) -> Fingerprint {
        let mut words = vec![
            format!("{:?}", self.opts),
            format!("target {:016x}", self.target.hash()),
            name.to_owned(),
        ];
        words.extend(self.cfg_of(name));
        let mut inputs = vec![root.parent().unwrap_or(root).to_path_buf()];
        let mut stamped = Vec::new();
        stamped.extend(self.opts.profile.used().map(Path::to_path_buf));
        if let Some(std) = self.std {
            words.push(std.display().to_string());
            inputs.push(std.to_path_buf());
        }
        for b in self.bootstrap {
            words.push(b.display().to_string());
            inputs.push(b.parent().unwrap_or(b).to_path_buf());
        }
        for (n, p) in before {
            words.push(format!("{n}={}", p.display()));
            let (rokhir, a) = rokhir::companions(p);
            inputs.push(rokhir);
            stamped.push(a);
        }
        let also = vec![rokhir::companions(archive).0];
        Fingerprint::new(archive, also, &words, &inputs, &stamped)
            .kept_in(self.dir.join(format!("{name}.fp")))
    }

    /// Build one dependency; the files it read and its rendered warnings.
    fn build(
        &self,
        name: &str,
        root: &Path,
        archive: &Path,
        before: &[(String, PathBuf)],
    ) -> Result<(Vec<PathBuf>, String), String> {
        if !self.quiet {
            // The package directory, above `src/lib.rok`.
            let dir = root.parent().unwrap_or(root);
            let dir = if dir.ends_with("src") {
                dir.parent().unwrap_or(dir)
            } else {
                dir
            };
            // `src/lib.rok` relative to the current directory.
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            rokc_core::term::status("Compiling", &format!("{name} ({})", dir.display()));
        }
        let mut s = Session::new();
        for b in self.bootstrap {
            s.add_bootstrap_file(b.clone());
        }
        for c in self.cfg_of(name) {
            s.add_cfg(c);
        }
        for (n, p) in before {
            s.add_extern(n.clone(), p.clone());
        }
        s.set_lib_name(Some(name.to_owned()));
        s.set_target(self.target);
        s.set_std_prebuilt(self.std.map(Path::to_path_buf));
        let r = s.compile_file(root);
        let warnings: String = r.warnings.iter().map(|w| r.render(w)).collect();
        print_rendered(&warnings);
        let failed = || format!("could not build dependency `{name}`");
        if r.has_errors() {
            r.report_errors();
            return Err(failed());
        }
        r.archive(self.opts, archive).map_err(|d| {
            r.report(&d);
            failed()
        })?;
        Ok((r.files, warnings))
    }
}
