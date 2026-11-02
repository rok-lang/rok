//! Items `#[cfg]` left out of the build, named in the errors
//! of a lookup that would have found them: "`process_group` is only
//! available on unix (`#[cfg(unix)]`)".

use rokc_ast::{AttrArg, Literal};
use rokc_diagnostic::RokDiagnostic;

/// An item, method or module the build's cfg dropped.
#[derive(Clone, Debug)]
pub struct CfgDropped {
    /// Its module (`rok::std::process`; empty at a package root): for
    /// `is_mod`, the parent of the module dropped.
    pub module: String,
    /// A method's type as its impl writes it (`Command`, `Wrap<T>`).
    pub owner: Option<String>,
    pub name: String,
    pub is_mod: bool,
    /// The predicates of its `#[cfg(..)]` attributes.
    pub preds: Vec<AttrArg>,
}

rokc_core::codec_struct!(CfgDropped {
    module,
    owner,
    name,
    is_mod,
    preds
});

/// The `#[cfg(..)]` predicates among `attrs`.
#[must_use]
pub fn cfg_preds(attrs: &[rokc_ast::Attribute]) -> Vec<AttrArg> {
    attrs
        .iter()
        .filter(|a| a.path == ["cfg"])
        .filter_map(|a| match &a.body {
            rokc_ast::AttrBody::Args(args) if args.len() == 1 => Some(args[0].clone()),
            _ => None,
        })
        .collect()
}

/// What the `CFG_OFF_ATTR` markers among `items` (with their modules)
/// say was dropped.
pub fn cfg_dropped<'a>(
    items: impl IntoIterator<Item = (&'a str, &'a rokc_ast::Item)>,
) -> Vec<CfgDropped> {
    let mut out = Vec::new();
    for (module, item) in items {
        let impl_ty = match item {
            rokc_ast::Item::Impl(d) => Some(rokc_ast::print::print_ty(&d.self_ty)),
            _ => None,
        };
        for a in item.attrs() {
            let Some(kind) = a
                .path
                .split_first()
                .filter(|(p, _)| *p == rokc_ast::CFG_OFF_ATTR)
                .map(|(_, k)| k)
            else {
                continue;
            };
            let rokc_ast::AttrBody::Args(args) = &a.body else {
                continue;
            };
            let Some((AttrArg::Word(name), preds)) = args.split_first() else {
                continue;
            };
            let owner = match kind {
                [m] if m == "method" => impl_ty.clone(),
                [m, t] if m == "method" => Some(t.clone()),
                _ => None,
            };
            let is_mod = matches!(kind, [m] if m == "mod");
            out.push(CfgDropped {
                module: module.to_owned(),
                owner,
                name: name.clone(),
                is_mod,
                preds: preds.to_vec(),
            });
        }
    }
    out
}

/// Add a note to each not-found error naming an item `dropped` holds.
pub fn note_dropped(diags: &mut [RokDiagnostic], dropped: &[CfgDropped]) {
    if dropped.is_empty() {
        return;
    }
    for d in diags {
        let Some(hit) = lookup(&d.message, dropped) else {
            continue;
        };
        let pred = match hit.preds.as_slice() {
            [one] => rokc_ast::print::print_attr_arg(one),
            many => format!(
                "all({})",
                many.iter()
                    .map(rokc_ast::print::print_attr_arg)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let what = if hit.is_mod {
            format!("module `{}`", hit.name)
        } else {
            format!("`{}`", hit.name)
        };
        let note = format!(
            "{what} is only available {} (`#[cfg({pred})]`)",
            where_(&hit.preds)
        );
        if d.notes.contains(&note) {
            continue;
        }
        // It exists: no help to declare or fetch it.
        d.notes
            .retain(|n| !(n.starts_with("help: add `fn ") || n.starts_with("help: run `rok add ")));
        d.notes.push(note);
        d.notes.push(format!(
            "help: put the code that uses it under the same `#[cfg({pred})]`"
        ));
    }
}

/// The dropped item a not-found message names.
fn lookup<'d>(msg: &str, dropped: &'d [CfgDropped]) -> Option<&'d CfgDropped> {
    let quoted = |i: usize| msg.split('`').nth(i);
    if let Some(rest) = msg.strip_prefix("no method `") {
        let method = rest.split('`').next()?;
        let ty = msg.split(" for type `").nth(1)?.split('`').next()?;
        let base = |t: &str| {
            let t = t.split('<').next().unwrap_or(t);
            t.rsplit("::").next().unwrap_or(t).trim().to_owned()
        };
        return dropped
            .iter()
            .find(|x| x.name == method && x.owner.as_deref().is_some_and(|o| base(o) == base(ty)));
    }
    if !(msg.starts_with("cannot find ") || msg.starts_with("unresolved import: `")) {
        return None;
    }
    let path = quoted(1)?;
    let segs: Vec<&str> = path.split("::").filter(|s| !s.is_empty()).collect();
    let (last, prefix) = segs.split_last()?;
    // A module on the way: `os::unix::CommandExt` with `unix` dropped.
    for (i, seg) in prefix.iter().enumerate() {
        if let Some(m) = dropped
            .iter()
            .find(|x| x.is_mod && x.name == *seg && module_matches(&x.module, &prefix[..i]))
        {
            return Some(m);
        }
    }
    dropped
        .iter()
        .find(|x| x.owner.is_none() && x.name == *last && module_matches(&x.module, prefix))
}

/// Whether module `module` can be the one path prefix `prefix` names: its
/// tail, or, for a bare name, a module of the user's own package.
fn module_matches(module: &str, prefix: &[&str]) -> bool {
    if prefix.is_empty() {
        return module.split("::").next() != Some("rok");
    }
    let segs: Vec<&str> = module.split("::").filter(|s| !s.is_empty()).collect();
    let want: Vec<&str> = prefix
        .iter()
        .copied()
        .filter(|s| !matches!(*s, "crate" | "self" | "super" | "pkg"))
        .collect();
    segs.ends_with(&want)
}

/// Where the predicates hold, in words: "on unix", "with feature `tls`".
fn where_(preds: &[AttrArg]) -> String {
    match preds {
        [one] => where_one(one),
        _ => "when its `cfg` holds".to_owned(),
    }
}

fn where_one(p: &AttrArg) -> String {
    match p {
        AttrArg::Word(w) if w == "test" => "in test builds".to_owned(),
        AttrArg::Word(w) => format!("on {w}"),
        AttrArg::KeyValue(k, Literal::Str(v) | Literal::RawStr(v)) => match k.as_str() {
            "target_os" | "target_family" | "target_arch" | "target_env" => format!("on {v}"),
            "feature" => format!("with feature `{v}`"),
            _ => format!("when `{k} = \"{v}\"`"),
        },
        _ => "when its `cfg` holds".to_owned(),
    }
}
