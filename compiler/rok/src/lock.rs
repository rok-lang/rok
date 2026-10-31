//! `rok.lock`: the exact commit and content hash of every git package and
//! stone, and the version and entry hash of every index native package.
//! Path packages are never pinned; a direct native dep is pinned by its
//! hash.

use std::path::Path;

use rokc_rokon::{Doc, Kind, Value};

use crate::manifest::{GitRef, Source};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locked {
    pub name: String,
    /// A stone's version; `url` is then its index and `reference` is
    /// `Default`.
    pub vers: Option<String>,
    pub url: String,
    pub reference: GitRef,
    pub commit: String,
    /// Git tree hash of the package's files.
    pub tree: String,
}

/// A native package from an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockedNative {
    pub name: String,
    pub vers: String,
    /// The index.
    pub url: String,
    /// `crate::native::Entry::checksum`.
    pub sha256: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lock {
    pub packages: Vec<Locked>,
    pub natives: Vec<LockedNative>,
}

impl Lock {
    pub fn find(&self, name: &str) -> Option<&Locked> {
        self.packages
            .iter()
            .find(|p| crate::manifest::same(&p.name, name))
    }

    pub fn find_native(&self, name: &str) -> Option<&LockedNative> {
        self.natives
            .iter()
            .find(|p| crate::manifest::same(&p.name, name))
    }

    pub fn read(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, &path.display().to_string()).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    pub fn parse(text: &str, file: &str) -> Result<Self, String> {
        let bad = |e: String| format!("{e}\n(delete {file} and run `rok update` to make it again)");
        let doc = Doc::parse(text, file).map_err(bad)?;
        let root = doc.root().record("Lock").map_err(bad)?;
        // Before the fields: a newer rok's lock may have others.
        let version = root.req("version").and_then(|v| Ok((v, v.int::<u32>()?)));
        match version {
            Ok((_, 1)) => {}
            // Deleting it would throw away what a newer rok resolved.
            Ok((_, n)) if n > 1 => {
                return Err(format!(
                    "{file} was written by a newer rok (lock version {n}); run that rok's \
                     toolchain here: set `toolchain` in rok.rokon to its version, or run `rok \
                     +<version> ..`"
                ));
            }
            Ok((v, _)) => return Err(bad(v.error("unsupported lockfile version"))),
            Err(e) => return Err(bad(e)),
        }
        root.only(&["version", "packages", "natives"])
            .map_err(bad)?;
        let mut packages = Vec::new();
        let list = root
            .opt("packages")
            .map(|p| p.list())
            .transpose()
            .map_err(bad)?;
        for item in list.unwrap_or_default() {
            let p = item.record("Locked").map_err(bad)?;
            p.only(&["name", "vers", "source", "commit", "tree"])
                .map_err(bad)?;
            let field = |k: &str| p.req(k).and_then(|d| d.string()).map_err(bad);
            let name = field("name")?;
            let source = p.req("source").map_err(bad)?;
            let vers = p.opt("vers").map(|v| v.string()).transpose().map_err(bad)?;
            let registry = source
                .variant("Dep")
                .ok()
                .filter(|v| v.name == "registry")
                .map(|v| match v.args.as_slice() {
                    [u] if v.fields.names().next().is_none() => u.string(),
                    _ => Err(source.error("`Dep::registry` takes one string, the index url")),
                })
                .transpose()
                .map_err(bad)?;
            let (url, reference) = match (registry, &vers) {
                (Some(url), Some(_)) => (url, GitRef::Default),
                (Some(_), None) => return Err(bad(p.req("vers").unwrap_err())),
                (None, None) => match crate::manifest::source(source).map_err(bad)? {
                    Source::Git { url, reference } => (url, reference),
                    _ => {
                        return Err(bad(source.error(
                            "a locked package comes from `Dep::git(..)` or `Dep::registry(..)`",
                        )));
                    }
                },
                (None, Some(_)) => {
                    return Err(bad(
                        p.field_error("vers", "`vers` goes with `Dep::registry(..)`")
                    ));
                }
            };
            packages.push(Locked {
                name,
                vers,
                url,
                reference,
                commit: field("commit")?,
                tree: field("tree")?,
            });
        }
        let mut natives = Vec::new();
        let list = root
            .opt("natives")
            .map(|p| p.list())
            .transpose()
            .map_err(bad)?;
        for item in list.unwrap_or_default() {
            let p = item.record("LockedNative").map_err(bad)?;
            p.only(&["name", "vers", "source", "sha256"]).map_err(bad)?;
            let field = |k: &str| p.req(k).and_then(|d| d.string()).map_err(bad);
            let source = p.req("source").map_err(bad)?;
            let url = source
                .variant("Dep")
                .ok()
                .filter(|v| v.name == "registry")
                .and_then(|v| match v.args.as_slice() {
                    [u] => u.string().ok(),
                    _ => None,
                })
                .ok_or_else(|| bad(source.error("expected `Dep::registry(\"<index url>\")`")))?;
            natives.push(LockedNative {
                name: field("name")?,
                vers: field("vers")?,
                url,
                sha256: field("sha256")?,
            });
        }
        Ok(Self { packages, natives })
    }

    /// Deterministic text: packages sorted by name, fixed field order.
    pub fn to_text(&self) -> String {
        let mut pkgs: Vec<&Locked> = self.packages.iter().collect();
        pkgs.sort_by(|a, b| a.name.cmp(&b.name));
        let packages = pkgs
            .into_iter()
            .map(|p| {
                let mut fields = vec![("name", Value::str(&p.name))];
                if let Some(v) = &p.vers {
                    fields.push(("vers", Value::str(v)));
                    let src = Value::call("Dep::registry", vec![Value::str(&p.url)], Vec::new());
                    fields.push(("source", src));
                } else {
                    let src = Source::Git {
                        url: p.url.clone(),
                        reference: p.reference.clone(),
                    };
                    fields.push(("source", src.to_value()));
                }
                fields.push(("commit", Value::str(&p.commit)));
                fields.push(("tree", Value::str(&p.tree)));
                Value::record(Some("Locked"), fields)
            })
            .collect();
        let mut natives: Vec<&LockedNative> = self.natives.iter().collect();
        natives.sort_by(|a, b| a.name.cmp(&b.name));
        let natives: Vec<Value> = natives
            .into_iter()
            .map(|n| {
                let src = Value::call("Dep::registry", vec![Value::str(&n.url)], Vec::new());
                Value::record(
                    Some("LockedNative"),
                    vec![
                        ("name", Value::str(&n.name)),
                        ("vers", Value::str(&n.vers)),
                        ("source", src),
                        ("sha256", Value::str(&n.sha256)),
                    ],
                )
            })
            .collect();
        let mut fields = vec![
            ("version", Value::new(Kind::Int(1))),
            ("packages", Value::new(Kind::List(packages))),
        ];
        if !natives.is_empty() {
            fields.push(("natives", Value::new(Kind::List(natives))));
        }
        let lock = Value::record(Some("Lock"), fields);
        format!(
            "// rok.lock: generated by `rok`; do not edit. Commit it with your sources.\n{}\n",
            rokc_rokon::to_string(&lock)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_sorted() {
        let lock = Lock {
            natives: Vec::new(),
            packages: vec![
                Locked {
                    name: "z".into(),
                    vers: None,
                    url: "file:///z".into(),
                    reference: GitRef::Default,
                    commit: "c1".into(),
                    tree: "t1".into(),
                },
                Locked {
                    name: "a".into(),
                    vers: None,
                    url: "https://x/a.git".into(),
                    reference: GitRef::Branch("main".into()),
                    commit: "c2".into(),
                    tree: "t2".into(),
                },
            ],
        };
        let text = lock.to_text();
        assert!(text.find("name: \"a\"").unwrap() < text.find("name: \"z\"").unwrap());
        let back = Lock::parse(&text, "rok.lock").unwrap();
        assert_eq!(back, lock_sorted(&lock));
        assert_eq!(back.to_text(), text);

        // The documented layout, with real hashes.
        let (c, t) = ("3f2a".repeat(10), "9c1e".repeat(10));
        let one = Lock {
            natives: Vec::new(),
            packages: vec![Locked {
                name: "json".into(),
                vers: None,
                url: "https://example.com/json.git".into(),
                reference: GitRef::Tag("v1.2".into()),
                commit: c.clone(),
                tree: t.clone(),
            }],
        };
        assert_eq!(
            one.to_text(),
            format!(
                "// rok.lock: generated by `rok`; do not edit. Commit it with your sources.\n\
                 Lock {{\n    version: 1,\n    packages: [\n        Locked {{\n            \
                 name: \"json\",\n            source: Dep::git(\"https://example.com/json.git\", \
                 tag: \"v1.2\"),\n            commit: \"{c}\",\n            tree: \"{t}\",\n        \
                 }},\n    ],\n}}\n"
            )
        );
        let stone = Lock {
            natives: vec![LockedNative {
                name: "zlib".into(),
                vers: "1.3.1".into(),
                url: "file:///i".into(),
                sha256: "5".repeat(64),
            }],
            packages: vec![Locked {
                name: "geom".into(),
                vers: Some("0.3.1".into()),
                url: "https://github.com/o/rokstones.io-index".into(),
                reference: GitRef::Default,
                commit: c,
                tree: t,
            }],
        };
        let text = stone.to_text();
        assert!(
            text.contains(
                "name: \"geom\",\n            vers: \"0.3.1\",\n            source: \
                 Dep::registry(\"https://github.com/o/rokstones.io-index\"),\n"
            ),
            "{text}"
        );
        assert_eq!(Lock::parse(&text, "rok.lock").unwrap(), stone);
        assert!(
            text.contains(&format!(
                "natives: [\n        LockedNative {{\n            name: \"zlib\",\n            vers: \
                 \"1.3.1\",\n            source: Dep::registry(\"file:///i\"),\n            sha256: \
                 \"{}\",\n",
                "5".repeat(64)
            )),
            "{text}"
        );
        assert_eq!(
            Lock::default().to_text().lines().nth(1),
            Some("Lock { version: 1, packages: [] }")
        );
        assert_eq!(
            Lock::parse("Lock { version: 1 }", "rok.lock").unwrap(),
            Lock::default()
        );

        for (src, want) in [
            (
                "Lock { version: 0, packages: [] }",
                "unsupported lockfile version\n  --> rok.lock:1:17",
            ),
            ("version = 1", "--> rok.lock:1:9\n"),
            ("Lock { packages: [] }", "missing field `version`"),
            (
                "Lock { version: 1, packages: [Locked { name: \"a\", source: Dep::path(\"p\"), \
                 commit: \"c\", tree: \"t\" }] }",
                "a locked package comes from `Dep::git(..)` or `Dep::registry(..)`",
            ),
            (
                "Lock { version: 1, packages: [Locked { name: \"a\", source: Dep::registry(\"u\"), \
                 commit: \"c\", tree: \"t\" }] }",
                "missing field `vers`",
            ),
            (
                "Lock { version: 1, packages: [Locked { name: \"a\", vers: \"1.0.0\", \
                 source: Dep::git(\"u\"), commit: \"c\", tree: \"t\" }] }",
                "`vers` goes with",
            ),
            (
                "Lock { version: 1, packages: [Locked { name: \"a\", source: Dep::git(\"u\") }] }",
                "missing field `commit`",
            ),
        ] {
            let e = Lock::parse(src, "rok.lock").unwrap_err();
            assert!(e.contains(want), "{src}: {e}");
            assert!(e.contains("run `rok update`"), "{e}");
        }
    }

    #[test]
    fn newer_lock_version_says_which_toolchain() {
        // Fields of a newer format do not matter: the version comes first.
        let e =
            Lock::parse("Lock { version: 2, packages: [], groups: {} }", "rok.lock").unwrap_err();
        assert_eq!(
            e,
            "rok.lock was written by a newer rok (lock version 2); run that rok's toolchain \
             here: set `toolchain` in rok.rokon to its version, or run `rok +<version> ..`"
        );
        assert!(!e.contains("delete"), "{e}");
    }

    fn lock_sorted(l: &Lock) -> Lock {
        let mut packages = l.packages.clone();
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        Lock {
            packages,
            natives: l.natives.clone(),
        }
    }
}
