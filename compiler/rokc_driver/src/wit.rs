//! `ffi::wit::import` / `export` (stdlib 1.8): modules holding them get
//! `rokc_wit`'s bindings, the link the component's world.

use std::path::Path;

use rokc_ast::Item;
use rokc_diagnostic::{ErrorKind, RokDiagnostic, SourceMap, Span};
use rokc_lexer::Lexer;
use rokc_mir::WitLink;
use rokc_parser::{ExtOps, apply_keywords, parse_program_meta};

use crate::{MetaCtx, ModItem, UseEntry, flatten_module_items};

/// Replace the `ffi::wit` items of `items` (the package's, by module)
/// with their bindings; the component's link data when there are any.
#[allow(clippy::too_many_arguments)] // the parse context of the package
pub fn expand(
    items: &mut Vec<ModItem>,
    uses: &mut Vec<UseEntry>,
    packages: &[String],
    prebuilt: &[String],
    target: &rokc_mir::TargetSpec,
    ext: &ExtOps,
    metas: &MetaCtx,
    diags: &mut Vec<RokDiagnostic>,
    files: &mut SourceMap,
) -> Option<WitLink> {
    // The `ffi::wit` items by module, in order.
    let mut groups: Vec<(String, Vec<(rokc_wit::Request, Span)>)> = Vec::new();
    items.retain(|(module, item)| {
        let Item::Extern(b) = item else { return true };
        let Some(w) = &b.wit else { return true };
        let dir = files
            .path(b.span.file)
            .and_then(|p| Path::new(p).parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let req = rokc_wit::Request {
            export: w.export,
            paths: w.paths.iter().map(|p| dir.join(p)).collect(),
        };
        match groups.iter_mut().find(|(m, _)| m == module) {
            Some((_, g)) => g.push((req, b.span)),
            None => groups.push((module.clone(), vec![(req, b.span)])),
        }
        false
    });
    if groups.is_empty() {
        return None;
    }
    let mut link = WitLink::default();
    let mut any = false;
    let mut world = String::new();
    for (module, g) in groups {
        let span = g[0].1;
        let requests: Vec<rokc_wit::Request> = g.into_iter().map(|(r, _)| r).collect();
        let fail = |diags: &mut Vec<RokDiagnostic>, msg: String| {
            diags.push(RokDiagnostic::error(ErrorKind::Module, span, msg));
        };
        // A prebuilt dependency's WIT is in its ROKHIR (the overlay).
        let loaded = match crate::OVERLAY.with(|o| rokc_wit::load(&requests, &o.borrow())) {
            Ok(l) => l,
            Err(e) => {
                fail(diags, format!("WIT: {e}"));
                continue;
            }
        };
        // A changed `.wit` file rebuilds (`rokc_interface::fresh`).
        crate::OVERLAY.with(|o| {
            let o = o.borrow();
            crate::READ.with(|r| {
                r.borrow_mut().extend(
                    loaded
                        .resolve
                        .files
                        .iter()
                        .map(|(p, _)| p)
                        .filter(|p| !o.contains_key(*p))
                        .cloned(),
                );
            });
        });
        // `pkg::` paths within the package the module is in (a
        // dependency's mount name is its first segment).
        let rel = packages
            .iter()
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                if module == *p {
                    Some("")
                } else {
                    module.strip_prefix(p.as_str())?.strip_prefix("::")
                }
            })
            .min_by_key(|r| r.len())
            .unwrap_or(&module);
        let base = if rel.is_empty() {
            "pkg".to_owned()
        } else {
            format!("pkg::{rel}")
        };
        let drops = drop_impls(items, &module);
        let b = match rokc_wit::bind(&loaded, &requests, &base, &drops) {
            Ok(b) => b,
            Err(e) => {
                fail(diags, format!("WIT: {e}"));
                continue;
            }
        };
        // WIT is checked for any target, but only a WASI preview 2
        // component has the imports and exports the bindings stand for.
        if target.linker != rokc_target::Linker::WasmComponentLd {
            fail(
                diags,
                format!(
                    "`ffi::wit` builds a WebAssembly component: it needs `--target wasm32-wasi` \
                     (or another component target), not {} (stdlib 1.8)",
                    target.name
                ),
            );
            continue;
        }
        let name = format!("<WIT bindings of `{base}`>");
        let file_id = files.add(name.clone(), b.source.clone());
        let (mut tokens, lex) = Lexer::new(&b.source, name.clone())
            .with_ext_ops(ext.symbols())
            .lex_all();
        for t in &mut tokens {
            t.span.file = file_id;
        }
        diags.extend(lex);
        apply_keywords(&mut tokens, &metas.keywords, diags);
        let (program, parse_diags) = parse_program_meta(
            &tokens,
            name,
            &metas.package,
            ext,
            &metas.table,
            &metas.literals,
            Some(&metas.cfg),
        );
        diags.extend(parse_diags);
        let mut generated = program.items;
        // A prebuilt dependency's archive has the component's entry
        // points: the bindings here only declare.
        let pkg = module.split("::").next().unwrap_or_default();
        if prebuilt.iter().any(|p| p == pkg) {
            drop_entries(&mut generated);
        }
        let path: Vec<String> = if module.is_empty() {
            Vec::new()
        } else {
            module.split("::").map(str::to_owned).collect()
        };
        flatten_module_items(
            generated, &path, None, ext, metas, diags, items, uses, None, files,
        );
        any = true;
        world.push_str(&b.world);
        for u in loaded.units {
            if !link.sources.iter().any(|s| s.path == u.path) {
                link.sources.push(rokc_mir::WitSource {
                    pkg: pkg.to_owned(),
                    files: u
                        .files
                        .iter()
                        .map(|&i| loaded.resolve.files[i].clone())
                        .collect(),
                    path: u.path,
                    dir: u.dir,
                });
            }
        }
        for e in b.exports {
            if !link.exports.contains(&e) {
                link.exports.push(e);
            }
        }
    }
    if !any {
        return None;
    }
    link.world = format!("package rok:component;\n\nworld component {{\n{world}}}\n");
    Some(link)
}

/// The types of `module` with an `impl Drop`: an exported resource's
/// `[dtor]` runs it.
fn drop_impls(items: &[ModItem], module: &str) -> std::collections::BTreeSet<String> {
    items
        .iter()
        .filter(|(m, _)| m == module)
        .filter_map(|(_, item)| {
            let Item::Impl(i) = item else { return None };
            let tr = i.trait_path.as_ref()?.segments.last()?;
            let rokc_ast::TyKind::Path(p) = &i.self_ty.kind else {
                return None;
            };
            (tr.name == "Drop").then(|| p.segments.last().map(|s| s.name.clone()))?
        })
        .collect()
}

/// `items` without their `#[export_name]` functions.
fn drop_entries(items: &mut Vec<Item>) {
    items
        .retain(|i| !matches!(i, Item::Fn(f) if f.attrs.iter().any(|a| a.name() == "export_name")));
    for i in items {
        if let Item::Mod(m) = i {
            if let Some(items) = m.items.as_mut() {
                drop_entries(items);
            }
        }
    }
}
