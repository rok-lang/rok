//! `#[cfg(...)]` item filtering. The environment is the `--cfg` flags plus
//! the target's `target_*` keys; an excluded inline `mod` goes with its
//! subtree. Members and statements are dropped by the parser.
//!
//! A dropped item leaves an empty `extern` block marked `CFG_OFF_ATTR` in
//! its place (the parser marks an impl's dropped methods on the impl), so
//! not-found errors can say where it exists (`rokc_hir::note_dropped`).

use rokc_ast::{AttrArg, AttrBody, Attribute, ExternBlock, ImplItem, Item};

pub use rokc_parser::CfgEnv;

/// The value of a `--cfg feature=<f>` flag (quotes optional).
#[must_use]
pub fn feature_of(flag: &str) -> Option<&str> {
    let (k, v) = flag.split_once('=')?;
    (k.trim() == "feature").then(|| v.trim().trim_matches('"'))
}

/// The target's cfg keys; `target_family` / `target_env`
/// only when the target has one.
pub fn add_target(env: &mut CfgEnv, target: &rokc_mir::Target) {
    env.add_pair("target_os", &target.os);
    env.add_pair("target_arch", &target.arch);
    env.add_pair("target_pointer_width", &target.pointer_width.to_string());
    env.add_pair("target_endian", &target.endian);
    for (k, v) in [
        ("target_family", &target.family),
        ("target_env", &target.env),
    ] {
        if !v.is_empty() {
            env.add_pair(k, v);
        }
    }
}

/// Drop items whose `#[cfg]` is false, recursing into inline modules.
#[must_use]
pub fn filter_items(items: Vec<Item>, env: &CfgEnv) -> Vec<Item> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if !item_passes(&item, env) {
            out.extend(stand_ins(&item));
            continue;
        }
        let item = match item {
            Item::Mod(mut m) => {
                if let Some(inner) = m.items.take() {
                    m.items = Some(filter_items(inner, env));
                }
                Item::Mod(m)
            }
            other => other,
        };
        out.push(item);
    }
    out
}

fn item_passes(item: &Item, env: &CfgEnv) -> bool {
    let attrs = item.attrs();
    // `#[test]` implies `#[cfg(test)]`; so does `#[bench]`
    // (a bench build is a test build, as with cargo).
    (env.has_flag("test")
        || !(crate::harness::is_test(attrs) || crate::harness::has_attr(attrs, "bench")))
        && attrs_pass(attrs, env)
}

/// Whether every `#[cfg(..)]` among `attrs` holds.
pub fn attrs_pass(attrs: &[Attribute], env: &CfgEnv) -> bool {
    env.attrs_pass(attrs)
}

/// The marked empty `extern` blocks standing in for dropped `item`: one
/// per name it declared (each method of an impl).
fn stand_ins(item: &Item) -> Vec<Item> {
    let preds = rokc_hir::cfg_preds(item.attrs());
    if preds.is_empty() {
        return Vec::new();
    }
    let block = |kind: Vec<String>, name: &str, extra: &[AttrArg]| {
        let mut args = vec![AttrArg::Word(name.to_owned())];
        args.extend(preds.iter().cloned());
        args.extend(extra.iter().cloned());
        let mut path = vec![rokc_ast::CFG_OFF_ATTR.to_owned()];
        path.extend(kind);
        Item::Extern(ExternBlock {
            attrs: vec![Attribute {
                path,
                body: AttrBody::Args(args),
                inner: false,
                span: item.span(),
            }],
            abi: "C".to_owned(),
            items: Vec::new(),
            span: item.span(),
            wit: None,
        })
    };
    match item {
        Item::Impl(i) => {
            let owner = rokc_ast::print::print_ty(&i.self_ty);
            i.items
                .iter()
                .filter_map(|m| match m {
                    ImplItem::Method(f) => Some(block(
                        vec!["method".to_owned(), owner.clone()],
                        &f.name,
                        &rokc_hir::cfg_preds(&f.attrs),
                    )),
                    _ => None,
                })
                .collect()
        }
        Item::Mod(m) => vec![block(vec!["mod".to_owned()], &m.name, &[])],
        Item::Extern(e) => e
            .items
            .iter()
            .filter_map(|x| match x {
                rokc_ast::ExternItem::Fn(f) => Some(block(Vec::new(), &f.name, &[])),
                rokc_ast::ExternItem::Static(_) => None,
            })
            .collect(),
        other => other
            .name()
            .map(|n| block(Vec::new(), n, &[]))
            .into_iter()
            .collect(),
    }
}
