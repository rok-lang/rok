//! Module-level passes: item collection (types, fns, impls, statics),
//! duplicate / trait-impl checks, and the final unresolved-type check.

#[allow(clippy::wildcard_imports)]
use super::*;
use rokc_ir_common::{Abi, FnRole, Linkage};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// An `extern` block marked `#[link(.., kind = "static")]` (spec 6.2.3):
/// its statics are in the image, not imported from a DLL.
fn links_static(attrs: &[rokc_ast::Attribute]) -> bool {
    use rokc_ast::{AttrArg, AttrBody, Literal};
    attrs.iter().filter(|a| a.path == ["link"]).any(|a| {
        matches!(&a.body, AttrBody::Args(args) if args.iter().any(|x| {
            matches!(x, AttrArg::KeyValue(k, Literal::Str(v)) if k == "kind" && v == "static")
        }))
    })
}

fn is_lazy_init_fn(name: &str) -> bool {
    leaf_name(name).starts_with("__rok_static_init_")
}

fn lazy_init_fn_name(static_name: &str) -> String {
    match static_name.rsplit_once("::") {
        Some((m, leaf)) => format!("{m}::__rok_static_init_{leaf}"),
        None => format!("__rok_static_init_{static_name}"),
    }
}

/// A concrete function whose code a prebuilt library's `.a` holds (the
/// ones an ROKHIR interface cuts to `;`).
fn in_archive(f: &FnDef) -> bool {
    f.generics.params.is_empty()
        && !f.is_async
        && !f.is_generator
        && !rokc_ast::inline_wanted(&f.attrs)
        && !f.attrs.iter().any(|a| a.path == [rokc_ast::EXTERN_FN_ATTR])
}

/// A body small enough to copy into a program for inlining
/// (`HirInput::inline_prebuilt`): at most `INLINE_EXPRS` expressions.
fn small_body(f: &FnDef) -> bool {
    const INLINE_EXPRS: usize = 40;
    struct Count(usize);
    impl crate::walk::AstVisitor for Count {
        fn expr(&mut self, _: &rokc_ast::Expr) {
            self.0 += 1;
        }
    }
    let mut n = Count(0);
    crate::walk::walk_fn(&mut n, f);
    n.0 <= INLINE_EXPRS
}

/// Package `pkg` is `p` or one of its tiers (`rok::core` is in `rok`,
/// the prebuilt standard library).
fn within(pkg: &str, p: &str) -> bool {
    pkg.strip_prefix(p)
        .is_some_and(|r| r.is_empty() || r.starts_with("::"))
}

/// Spec 4.2.2 / 2.10.7: an untyped parameter is an implicit type parameter
/// (`fn id(x)` is `fn id<T#x>(x: T#x)`), checked per instance, no bound
/// synthesized. Free fns and inherent methods only (trait impl methods
/// take the trait's types).
fn implicit_generics(program: &mut Program) {
    let fix = |f: &mut FnDef| {
        if f.body.is_none() {
            return;
        }
        for p in &mut f.params {
            if p.ty.is_some() || p.name == "self" || p.variadic {
                continue;
            }
            let name = format!("T#{}", p.name);
            let path = rokc_ast::Path {
                segments: vec![rokc_ast::PathSegment {
                    name: name.clone(),
                    generics: Vec::new(),
                }],
                span: p.span,
            };
            p.ty = Some(Ty {
                kind: AstTyKind::Path(path),
                span: p.span,
            });
            f.generics.params.push(rokc_ast::GenericParam::Type {
                name,
                bounds: Vec::new(),
                default: None,
            });
        }
    };
    for item in &mut program.items {
        match item {
            Item::Fn(f) => fix(f),
            Item::Impl(i) if i.trait_path.is_none() => {
                for it in &mut i.items {
                    if let AstImplItem::Method(f) = it {
                        fix(f);
                    }
                }
            }
            _ => {}
        }
    }
}

fn add_lazy_static_inits(program: &mut Program) {
    let inits: Vec<Item> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Static(s) => s.value.as_ref().map(|v| {
                Item::Fn(rokc_ast::FnDef {
                    vis: Visibility::Private,
                    attrs: Vec::new(),
                    name: lazy_init_fn_name(&s.name),
                    generics: rokc_ast::Generics::default(),
                    params: Vec::new(),
                    ret_ty: Some(s.ty.clone()),
                    // `return v;` (not a trailing `v`): the return type
                    // is the expected type that infers `Atomic::new(0)`.
                    body: Some(
                        rokc_ast::Block {
                            stmts: vec![rokc_ast::Stmt {
                                kind: rokc_ast::StmtKind::Return(Some(v.clone())),
                                span: s.span,
                            }],
                            trailing: None,
                            span: s.span,
                            label: None,
                        }
                        .into(),
                    ),
                    is_async: false,
                    is_generator: false,
                    is_unsafe: false,
                    throws: rokc_ast::ThrowsKind::None,
                    where_clause: Vec::new(),
                    span: s.span,
                })
            }),
            _ => None,
        })
        .collect();
    program.items.extend(inits);
}

/// What collecting a stage leaves for the next one and for lowering.
/// The library is collected before the package, so its ids don't depend on
/// the package.
#[derive(Clone)]
pub(super) struct Collected<'a> {
    t: Tables<'a>,
    /// Ahead of lowering's own diagnostics and not merged with them.
    early: Vec<rokc_diagnostic::RokDiagnostic>,
    /// Diagnostics of the passes before bodies.
    pre_diags: Vec<rokc_diagnostic::RokDiagnostic>,
    /// Every `pub use` re-export (globs expanded).
    reexports: Vec<crate::uses::ReExport>,
    /// The declared (not generic) enums and structs, by id.
    enum_decls: Vec<(EnumId, &'a AstEnumDef)>,
    struct_decls: Vec<(StructId, &'a AstStructDef)>,
    /// Inherent associated consts (`S::N`).
    inherent_keys: std::collections::HashSet<String>,
    static_defs: Vec<HirStaticDef>,
    /// Per static: its type as written, module and initializer.
    static_asts: Vec<(&'a Ty, String)>,
    /// Per decl: its body is library code (`rok::<tier>` / a dependency),
    /// lowered only when reached (`lower_bodies`).
    lib_decl: Vec<bool>,
    /// Decl index → symbol path of a dependency's method.
    link_names: HashMap<usize, String>,
    /// Generic impls with method templates by tag (`generic_trait_impl`).
    templated: HashMap<String, usize>,
    system_fns: std::collections::HashSet<FunctionId>,
    no_safepoint: std::collections::HashSet<FunctionId>,
    wasm_imports: HashMap<FunctionId, (String, String)>,
    extern_symbols: HashMap<FunctionId, String>,
    trait_defs: HashMap<String, &'a rokc_ast::TraitDef>,
    exports: Vec<(FunctionId, String)>,
    prebuilt: std::collections::HashSet<FunctionId>,
    inline_copies: std::collections::HashSet<FunctionId>,
    c_defs: Vec<FunctionId>,
    /// Leaf names of the functions that diverge (`may_diverge`).
    diverging: std::collections::HashSet<String>,
}

impl Collected<'_> {
    fn empty() -> Self {
        Self {
            t: Tables::empty(),
            early: Vec::new(),
            pre_diags: Vec::new(),
            reexports: Vec::new(),
            enum_decls: Vec::new(),
            struct_decls: Vec::new(),
            inherent_keys: std::collections::HashSet::new(),
            static_defs: Vec::new(),
            static_asts: Vec::new(),
            lib_decl: Vec::new(),
            link_names: HashMap::new(),
            templated: HashMap::new(),
            system_fns: std::collections::HashSet::new(),
            no_safepoint: std::collections::HashSet::new(),
            wasm_imports: HashMap::new(),
            extern_symbols: HashMap::new(),
            trait_defs: HashMap::new(),
            exports: Vec::new(),
            prebuilt: std::collections::HashSet::new(),
            inline_copies: std::collections::HashSet::new(),
            c_defs: Vec::new(),
            diverging: std::collections::HashSet::new(),
        }
    }
}

/// Lower `program`: collect the library's items, then the package's on top
/// (`collect`), then the bodies (`finish`).
#[must_use]
pub fn lower_full(
    program: Program,
    prec: &PrecTable,
    raw_uses: &[(String, Visibility, rokc_ast::UseTree)],
    uses: &crate::uses::UseTable,
    reexports: &[crate::uses::ReExport],
    input: &crate::HirInput,
    sources: &rokc_diagnostic::SourceMap,
) -> HirModule {
    let collect_started = rokc_diagnostic::time_passes().then(std::time::Instant::now);
    resolve::set_packages(input.packages.clone());
    resolve::set_own_library(input.own_library.clone());
    resolve::set_no_implicit(input.no_implicit_rok.clone());
    super::const_eval::set_pointer_width(input.pointer_width);
    super::const_eval::set_stdcall_system(input.stdcall_system);
    crate::clear_type_names();
    let (mut lib, mut user) = (Program::default(), Program::default());
    let (mut lib_modules, mut user_modules) = (Vec::new(), Vec::new());
    for (item, module) in program.items.into_iter().zip(&input.item_modules) {
        if package_of(module).is_empty() {
            user.items.push(item);
            user_modules.push(module.clone());
        } else {
            lib.items.push(item);
            lib_modules.push(module.clone());
        }
    }
    let lib_uses: Vec<_> = raw_uses
        .iter()
        .filter(|(m, _, _)| !package_of(m).is_empty())
        .cloned()
        .collect();
    let user_uses: Vec<_> = raw_uses
        .iter()
        .filter(|(m, _, _)| package_of(m).is_empty())
        .cloned()
        .collect();
    let lib_reexports: Vec<_> = reexports
        .iter()
        .filter(|r| !package_of(&r.module_path).is_empty())
        .cloned()
        .collect();
    let user_reexports: Vec<_> = reexports
        .iter()
        .filter(|r| package_of(&r.module_path).is_empty())
        .cloned()
        .collect();
    let at = Stage {
        prec,
        uses,
        input,
        sources,
    };
    // The library's stage: kept for the next compile with the same
    // library (`HirInput::library`), its items leaked to live as long.
    let key = input.library;
    let kept = key.and(input.stage.as_ref()).map(|l| {
        let l = stage(l);
        (l.c.clone(), l.lib, l.type_names.clone())
    });
    let mut lib_items: Program;
    let (base, lib): (Collected<'_>, &Program) = if let Some((mut c, lib, type_names)) = kept {
        crate::set_type_names(type_names);
        // The stage collected the library under another compile's table.
        c.t.names.uses.clone_from(uses);
        (c, lib)
    } else if let Some(k) = key {
        let lib: &'static mut Program = Box::leak(Box::new(lib));
        let (c, lib) = match collect(
            lib,
            &lib_modules,
            &[],
            Collected::empty(),
            &lib_uses,
            &lib_reexports,
            &at,
        ) {
            Ok(c) => c,
            Err(m) => return *m,
        };
        let kept = LibraryStage {
            c: c.clone(),
            lib,
            type_names: crate::type_names(),
        };
        keep_library(k, kept);
        (c, lib)
    } else {
        lib_items = lib;
        match collect(
            &mut lib_items,
            &lib_modules,
            &[],
            Collected::empty(),
            &lib_uses,
            &lib_reexports,
            &at,
        ) {
            Ok(c) => c,
            Err(m) => return *m,
        }
    };
    let (c, user) = match collect(
        &mut user,
        &user_modules,
        &lib.items,
        base,
        &user_uses,
        &user_reexports,
        &at,
    ) {
        Ok(c) => c,
        Err(m) => return *m,
    };
    if let Some(t) = collect_started {
        eprintln!(
            "time: {:>9.1}ms  hir collect",
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    finish(c, [lib, user], raw_uses, &at)
}

mod stage;
pub use stage::LibraryStage;

/// The last library stage, by key. A compile holds its own copy
/// (`HirInput::stage`); another compile may replace this one meanwhile.
static LIBRARY: Mutex<Option<(u64, KeptLibrary)>> = Mutex::new(None);

/// A kept library stage, shared by the compiles that take it.
pub type KeptLibrary = Arc<Mutex<LibraryStage>>;

/// The stage of `kept`, locked.
pub fn stage(kept: &KeptLibrary) -> MutexGuard<'_, LibraryStage> {
    kept.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The library's stage of key `library`, if it is kept.
#[must_use]
pub fn kept_library(library: u64) -> Option<KeptLibrary> {
    LIBRARY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .filter(|(k, _)| *k == library)
        .map(|(_, s)| Arc::clone(s))
}

/// Keep `stage` as the library's stage of key `library`.
pub fn keep_library(library: u64, stage: LibraryStage) -> KeptLibrary {
    let stage = Arc::new(Mutex::new(stage));
    *LIBRARY.lock().unwrap_or_else(PoisonError::into_inner) = Some((library, Arc::clone(&stage)));
    stage
}

/// What every stage of a compile shares.
struct Stage<'s> {
    prec: &'s PrecTable,
    uses: &'s crate::uses::UseTable,
    input: &'s crate::HirInput,
    sources: &'s rokc_diagnostic::SourceMap,
}

/// Collect `owned` on top of `base`: the library's on an empty base, then
/// the package's on the library's (`lib`). `use`s and re-exports are the
/// stage's own.
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)] // linear sequence of collecting passes
fn collect<'a>(
    owned: &'a mut Program,
    item_modules: &[String],
    lib: &'a [Item],
    base: Collected<'a>,
    raw_uses: &[(String, Visibility, rokc_ast::UseTree)],
    reexports: &[crate::uses::ReExport],
    at: &Stage<'_>,
) -> Result<(Collected<'a>, &'a Program), Box<HirModule>> {
    let Stage {
        prec,
        uses,
        input,
        sources,
    } = *at;
    let Collected {
        t: base_t,
        mut early,
        mut pre_diags,
        reexports: base_reexports,
        mut enum_decls,
        mut struct_decls,
        mut inherent_keys,
        mut static_defs,
        static_asts: base_static_asts,
        mut lib_decl,
        mut link_names,
        mut templated,
        mut system_fns,
        mut no_safepoint,
        mut wasm_imports,
        mut extern_symbols,
        trait_defs: base_trait_defs,
        mut exports,
        mut prebuilt,
        mut inline_copies,
        mut c_defs,
        mut diverging,
    } = base;
    let Tables {
        fn_table,
        fn_leaves: _,
        extern_fns,
        method_table,
        key_of: base_key_of,
        infer_ret: base_infer_ret,
        implicit_main: _,
        main_lets: _,
        harness_main: _,
        may_diverge: base_may_diverge,
        duplicated: _,
        method_info,
        defs,
        consts,
        const_leaves: _,
        generic_fns,
        generic_methods,
        intrinsics,
        generic_intrinsics,
        extern_fn_ids,
        rok_externs,
        c_variadic,
        c_defs: base_c_defs,
        stdcall_fns: base_stdcall,
        sink,
        item_visibility,
        field_vis,
        impls,
        unsafe_fns,
        neg_auto,
        auto_memo: _,
        trait_info: base_trait_info,
        statics: static_tys,
        static_leaves: _,
        mut_statics,
        literal_shapes,
        names: base_names,
        decls,
        non_exhaustive,
        bound_methods: _,
        bound_traits: _,
    } = base_t;
    for (item, module) in owned.items.iter_mut().zip(item_modules) {
        qualify(item, module);
    }
    // Spec 6.4: `#[lang(...)]` slots over library + user items; a slot
    // bound twice (6.4.6) stops lowering.
    let (lang, lang_diags) = rokc_parser::collect_lang_table_onto(base_names.lang.clone(), owned);
    if lang_diags
        .iter()
        .any(rokc_diagnostic::RokDiagnostic::is_error)
    {
        return Err(Box::new(HirModule {
            diagnostics: lang_diags,
            ..HirModule::default()
        }));
    }
    early.extend(lang_diags);
    let all_items: Vec<&Item> = lib.iter().chain(&owned.items).collect();
    let (mut stage_reexports, glob_diags) =
        crate::uses::glob_reexports(raw_uses, &all_items, uses, reexports);
    // Spec 5.9: a `pub` function of an `extern` block in module `m` is
    // also `m::f` (its symbol stays `f`).
    for (item, module) in owned.items.iter().zip(item_modules) {
        let Item::Extern(b) = item else { continue };
        for ei in &b.items {
            match ei {
                rokc_ast::ExternItem::Fn(f)
                    if !module.is_empty() && f.vis != Visibility::Private =>
                {
                    stage_reexports.push(crate::uses::ReExport {
                        module_path: module.clone(),
                        short_name: f.name.clone(),
                        full_path: f.name.clone(),
                        vis: f.vis,
                    });
                }
                _ => {}
            }
        }
    }
    early.extend(glob_diags);
    let mut reexports = base_reexports;
    reexports.extend(stage_reexports);
    let reexports = reexports;
    // A prebuilt dependency's build has no user package to advise.
    let missing = crate::uses::Missing {
        script: input.script.as_ref(),
        packages: &input.packages,
    };
    let (use_diags, hiding) = crate::uses::check_use_targets(
        raw_uses,
        &all_items,
        uses,
        &reexports,
        input.lib.is_none().then_some(missing),
    );
    early.extend(use_diags);
    early.extend(crate::uses::check_use_clashes(raw_uses, uses, &reexports));
    let (hiding_diags, hiding): (Vec<_>, Vec<_>) = hiding.into_iter().unzip();
    pre_diags.extend(hiding_diags);
    early.extend(check_tiers(raw_uses, &owned.items, item_modules));
    let block_globs = block_glob_renames(&owned.items, item_modules, &all_items, &reexports, uses);
    drop(all_items);
    for (i, r) in block_globs {
        r.item(&mut owned.items[i]);
    }
    add_lazy_static_inits(owned);
    implicit_generics(owned);
    let err = |pre: &mut Vec<rokc_diagnostic::RokDiagnostic>, at: Span, msg: String| {
        pre.push(rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Type,
            at,
            msg,
        ));
    };
    let program: &Program = &*owned;
    // Spec 6.2: `#[derive]` expanded into impls by the parser; a union has
    // no field-wise meaning.
    for item in &program.items {
        if let Item::Union(u) = item {
            if let Some((_, at)) = rokc_parser::derive::derive_names(&u.attrs).first() {
                err(
                    &mut pre_diags,
                    *at,
                    format!("cannot derive traits for union `{}` (spec 6.2.1)", u.name),
                );
            }
        }
    }

    // A field / variant name declared twice in one type.
    for item in &program.items {
        let (kind, owner, names): (&str, &str, Vec<(&str, rokc_diagnostic::Span)>) = match item {
            Item::Struct(s) => match &s.kind {
                rokc_ast::StructKind::Named(fs) => (
                    "field",
                    &s.name,
                    fs.iter().map(|f| (f.name.as_str(), f.span)).collect(),
                ),
                _ => continue,
            },
            Item::Union(u) => (
                "field",
                &u.name,
                u.fields.iter().map(|f| (f.name.as_str(), f.span)).collect(),
            ),
            Item::Enum(e) => (
                "variant",
                &e.name,
                e.variants
                    .iter()
                    .map(|v| (v.name.as_str(), v.span))
                    .collect(),
            ),
            _ => continue,
        };
        for (i, (n, at)) in names.iter().enumerate() {
            if names[..i].iter().any(|(m, _)| m == n) {
                err(
                    &mut pre_diags,
                    *at,
                    format!(
                        "{kind} `{n}` is declared more than once in `{}`",
                        leaf_name(owner)
                    ),
                );
            }
        }
    }

    // Pass 0: the type namespace. Every type-like (already qualified) name
    // goes into one index; a duplicate is an error and skipped so ids stay
    // consistent.
    let mut names = base_names;
    // An answer for a package module may change with its items.
    names
        .canon_memo
        .get_mut()
        .retain(|(_, module), _| !package_of(module).is_empty());
    let mut type_spans: HashMap<&str, Span> = HashMap::new();
    for item in &program.items {
        let (name, vis) = match item {
            Item::Struct(s) => (&s.name, s.vis),
            Item::Enum(e) => (&e.name, e.vis),
            Item::Union(u) => (&u.name, u.vis),
            Item::TypeAlias(a) => (&a.name, a.vis),
            Item::Trait(t) => {
                names.traits.insert(t.name.clone());
                let mut methods: HashMap<&str, Span> = HashMap::new();
                for ti in &t.items {
                    let rokc_ast::TraitItem::Method(m) = ti else {
                        continue;
                    };
                    if let Some(first) = methods.insert(&m.name, m.span) {
                        pre_diags.push(rokc_diagnostic::duplicate(
                            rokc_diagnostic::ErrorKind::Type,
                            format!(
                                "method `{}` is defined more than once in trait `{}`",
                                m.name, t.name
                            ),
                            &m.name,
                            m.span,
                            Some(first),
                            true,
                        ));
                    }
                }
                (&t.name, t.vis)
            }
            _ => continue,
        };
        if names.names.insert(name.clone()) {
            names.visibility.insert(name.clone(), vis);
            type_spans.insert(name, item.span());
        } else {
            pre_diags.push(rokc_diagnostic::duplicate(
                rokc_diagnostic::ErrorKind::Type,
                format!("type `{name}` is defined more than once"),
                name,
                item.span(),
                type_spans.get(name.as_str()).copied(),
                false,
            ));
        }
    }
    // `pub use` aliases → what they name, chains followed to the item.
    let aliases = reexport_aliases(&reexports);
    names.reexports = aliases
        .iter()
        .filter(|(_, end)| names.names.contains(*end))
        .map(|(a, end)| (a.clone(), end.clone()))
        .collect();
    // `Enum::Variant` keys, so `use E::{A}` / `use E::*` (and the
    // implicit `rok::core::**`) can bring variants in bare.
    names.variants.extend(
        program
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Enum(e) => Some(e),
                _ => None,
            })
            .flat_map(|e| e.variants.iter().map(|v| format!("{}::{}", e.name, v.name))),
    );
    // Structural auto traits: a type has them unless a component lacks them.
    names.auto_traits = ["send_trait", "sync_trait"]
        .into_iter()
        .filter_map(|s| lang.lookup(s))
        .map(|e| e.item_name.clone())
        .collect();
    names.lang = lang;
    names
        .mod_vis
        .extend(super::resolve::module_visibility(&program.items));
    names.uses.clone_from(uses);
    for it in &program.items {
        match it {
            Item::TypeAlias(a) if !a.generics.params.is_empty() || !a.where_clause.is_empty() => {
                names.generic_aliases.insert(a.name.clone(), a.clone());
            }
            _ => {}
        }
        match it {
            Item::TypeAlias(a) if a.generics.params.is_empty() => {
                names.plain_aliases.insert(a.name.clone(), a.ty.clone());
            }
            Item::Const(c) => {
                names
                    .consts
                    .insert(c.name.clone(), (c.ty.clone(), c.value.clone()));
            }
            _ => {}
        }
    }
    // The leaf indexes are built on the first lookup, over every stage.
    names.leaves = std::cell::OnceCell::new();
    names.variant_leaves = std::cell::OnceCell::new();
    names.const_leaves = std::cell::OnceCell::new();
    let statics: std::collections::HashSet<&str> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Static(s) => Some(s.name.as_str()),
            _ => None,
        })
        .chain(static_tys.keys().map(String::as_str))
        .collect();
    names.value_reexports = aliases
        .iter()
        .filter(|(_, end)| names.consts.contains_key(*end) || statics.contains(end.as_str()))
        .map(|(a, end)| (a.clone(), end.clone()))
        .collect();
    drop(statics);
    let aliased: Vec<(String, (Ty, Expr))> = names
        .value_reexports
        .iter()
        .filter_map(|(a, end)| Some((a.clone(), names.consts.get(end)?.clone())))
        .collect();
    for (alias, c) in aliased {
        names.consts.entry(alias).or_insert(c);
    }
    warn_assoc_shadowing(program, &names, &mut pre_diags);
    let defaults = assoc_defaults(program, &names, &base_trait_defs);
    for (i, added) in defaults {
        if let Item::Impl(imp) = &mut owned.items[i] {
            imp.items.extend(added);
        }
    }
    expand_impl_aliases(owned, &names);
    impl_param_types(owned, &names, &base_trait_defs);
    add_self_assoc_sigs(owned);
    let program: &'a Program = owned;
    check_bound_names(program, &names, &base_trait_defs, &mut pre_diags);
    check_niche_attrs(program, &mut pre_diags);
    check_repr_attrs(program, &mut pre_diags);

    // Pass 1a: every struct / union / enum gets its id before any type
    // resolves; generic ones are templates instantiated in the same id space
    // (`TypeDefs`). Unions follow the structs.
    let mut defs = defs;
    let (first_enum, first_struct) = (defs.enums.borrow().len(), defs.structs.borrow().len());
    let enum_id = |n: usize| EnumId(u32::try_from(first_enum + n).expect("EnumId overflow"));
    let struct_id =
        |n: usize| StructId(u32::try_from(first_struct + n).expect("StructId overflow"));
    let mut new_enums: Vec<&AstEnumDef> = Vec::new();
    let mut new_structs: Vec<&AstStructDef> = Vec::new();
    let mut union_decls: Vec<&rokc_ast::UnionDef> = Vec::new();
    for item in &program.items {
        match item {
            Item::Enum(e) => {
                if defs.enum_table.borrow().contains_key(&e.name)
                    || defs.generic_enums.contains_key(&e.name)
                {
                    continue;
                }
                if !e.generics.params.is_empty() {
                    if mono::all_defaulted(&e.generics) {
                        defs.defaulted_leaves.insert(leaf_name(&e.name).to_owned());
                    }
                    defs.generic_enums.insert(e.name.clone(), e);
                    continue;
                }
                let id = enum_id(new_enums.len());
                crate::name_type(true, id.0, &e.name, &[]);
                defs.enum_table.borrow_mut().insert(e.name.clone(), id);
                new_enums.push(e);
            }
            Item::Struct(s) => {
                if defs.struct_table.borrow().contains_key(&s.name)
                    || defs.generic_structs.contains_key(&s.name)
                {
                    continue;
                }
                if !s.generics.params.is_empty() {
                    if mono::all_defaulted(&s.generics) {
                        defs.defaulted_leaves.insert(leaf_name(&s.name).to_owned());
                    }
                    defs.generic_structs.insert(s.name.clone(), s);
                    continue;
                }
                let id = struct_id(new_structs.len());
                crate::name_type(false, id.0, &s.name, &[]);
                defs.struct_table.borrow_mut().insert(s.name.clone(), id);
                new_structs.push(s);
            }
            _ => {}
        }
    }
    for item in &program.items {
        if let Item::Union(u) = item {
            if defs.struct_table.borrow().contains_key(&u.name)
                || defs.generic_structs.contains_key(&u.name)
                || defs.generic_unions.contains_key(&u.name)
            {
                continue;
            }
            if !u.generics.params.is_empty() {
                if u.fields.is_empty() {
                    union_storage_strategy(&[], false, &u.name, u.span, &mut pre_diags);
                }
                if mono::all_defaulted(&u.generics) {
                    defs.defaulted_leaves.insert(leaf_name(&u.name).to_owned());
                }
                defs.generic_unions.insert(u.name.clone(), u);
                continue;
            }
            let id = struct_id(new_structs.len() + union_decls.len());
            crate::name_type(false, id.0, &u.name, &[]);
            defs.struct_table.borrow_mut().insert(u.name.clone(), id);
            union_decls.push(u);
        }
    }
    // Declared ones first (field types not resolved yet), so an instance
    // made while resolving them takes the next id.
    for (i, e) in new_enums.iter().enumerate() {
        defs.enums.borrow_mut().push(HirEnumDef {
            id: enum_id(i),
            repr: crate::Repr::default(),
            def_id: None,
            name: e.name.clone(),
            variants: Vec::new(),
            span: e.span,
            generic_args: Vec::new(),
        });
    }
    let struct_shells = new_structs
        .iter()
        .map(|s| (&s.name, s.span))
        .chain(union_decls.iter().map(|u| (&u.name, u.span)));
    for (i, (name, span)) in struct_shells.enumerate() {
        defs.structs.borrow_mut().push(HirStructDef {
            id: struct_id(i),
            repr: crate::Repr::default(),
            def_id: None,
            name: name.clone(),
            fields: Vec::new(),
            span,
            generic_args: Vec::new(),
            union_kind: None,
            niche: Vec::new(),
        });
    }
    for (i, e) in new_enums.iter().enumerate() {
        let id = enum_id(i);
        let module = module_of(&e.name);
        let sc = Scope {
            names: &names,
            module: &module,
            at: e.span,
        };
        let def = lower_enum_def(e, id, &defs, sc);
        defs.enums.borrow_mut()[id.0 as usize] = def;
    }
    for (i, s) in new_structs.iter().enumerate() {
        let id = struct_id(i);
        let module = module_of(&s.name);
        let sc = Scope {
            names: &names,
            module: &module,
            at: s.span,
        };
        let def = lower_struct_def(s, id, &defs, sc);
        defs.structs.borrow_mut()[id.0 as usize] = def;
    }
    for (i, u) in union_decls.iter().enumerate() {
        let id = struct_id(new_structs.len() + i);
        let module = module_of(&u.name);
        let sc = Scope {
            names: &names,
            module: &module,
            at: u.span,
        };
        let def = lower_union_def(u, id, &defs, sc, &mut pre_diags);
        defs.structs.borrow_mut()[id.0 as usize] = def;
    }
    let first_new_enum = enum_decls.len();
    enum_decls.extend(new_enums.iter().enumerate().map(|(i, e)| (enum_id(i), *e)));
    struct_decls.extend(
        new_structs
            .iter()
            .enumerate()
            .map(|(i, s)| (struct_id(i), *s)),
    );

    // Pass 1c: module-level `const`s, kept as AST and inlined at each
    // use; `static`s get global storage (`HirStaticDef`).
    let mut consts = consts;
    // Per impl item: its associated consts, for its `Impl`.
    let mut impl_const_lists: HashMap<usize, Vec<AssocConst>> = HashMap::new();
    // Where this stage's consts are, for a later item of their name.
    let mut const_spans: HashMap<String, Span> = HashMap::new();
    // Module-level consts and statics: one value namespace with fns.
    let mut value_spans: HashMap<String, (Span, &str)> = HashMap::new();
    // The kept definition of each name defined again (`Env::duplicated`),
    // and each item hiding an import.
    let mut duplicated: std::collections::HashSet<Span> = hiding.into_iter().collect();
    // A tuple or unit struct's constructor is a value too.
    for item in &program.items {
        if let Item::Struct(s) = item {
            match s.kind {
                rokc_ast::StructKind::Tuple(_) => {
                    value_spans.insert(s.name.clone(), (s.span, "tuple struct"))
                }
                rokc_ast::StructKind::Unit => {
                    value_spans.insert(s.name.clone(), (s.span, "unit struct"))
                }
                rokc_ast::StructKind::Named(_) => None,
            };
        }
    }
    for (idx, item) in program.items.iter().enumerate() {
        // Spec 2.9.1: an impl's `const` is `Type::NAME`; a trait impl also gets
        // the trait's defaults (2.8.3a).
        let item_consts: Vec<(String, Ty, Expr)> = match item {
            Item::Const(c) => vec![(c.name.clone(), c.ty.clone(), c.value.clone())],
            Item::Impl(i) => {
                let (keyed, all) =
                    impl_consts(i, program, &base_trait_defs, &names, &mut pre_diags);
                impl_const_lists.insert(idx, all);
                keyed
            }
            _ => Vec::new(),
        };
        // Two trait impls may both give `S::N` (ambiguous where read); an
        // inherent `S::N` shadows them, as for methods.
        let trait_impl = matches!(item, Item::Impl(i) if i.trait_path.is_some());
        // An inherent impl's own consts come first, in order.
        let spans: Vec<Span> = match item {
            Item::Impl(i) => i
                .items
                .iter()
                .filter_map(|it| match it {
                    AstImplItem::Const(c) => Some(c.span),
                    _ => None,
                })
                .collect(),
            _ => vec![item.span()],
        };
        for (n, (key, ty, value)) in item_consts.into_iter().enumerate() {
            let here = spans.get(n).copied().unwrap_or_else(|| item.span());
            if trait_impl {
                if !inherent_keys.contains(&key) {
                    consts.insert(key, (ty, value));
                }
            } else if let Some(&(first, _)) = value_spans
                .get(&key)
                .filter(|_| matches!(item, Item::Const(_)))
            {
                duplicated.insert(first);
                pre_diags.push(rokc_diagnostic::duplicate(
                    rokc_diagnostic::ErrorKind::Type,
                    value_dup("const", &key, &value_spans),
                    &key,
                    here,
                    Some(first),
                    false,
                ));
            } else if inherent_keys.insert(key.clone()) {
                if matches!(item, Item::Const(_)) {
                    value_spans.insert(key.clone(), (here, "const"));
                }
                const_spans.insert(key.clone(), here);
                consts.insert(key, (ty, value));
            } else {
                pre_diags.push(rokc_diagnostic::duplicate(
                    rokc_diagnostic::ErrorKind::Type,
                    format!("const `{key}` is defined more than once"),
                    &key,
                    here,
                    const_spans.get(&key).copied(),
                    false,
                ));
            }
        }
    }
    for (alias, end) in &names.value_reexports {
        if let Some(c) = consts.get(end).cloned() {
            consts.entry(alias.clone()).or_insert(c);
        }
    }
    let first_static = static_defs.len();
    let mut static_asts = base_static_asts;
    let mut static_tys = static_tys;
    for (idx, item) in program.items.iter().enumerate() {
        let here = item.span();
        if let Item::Static(s) = item {
            if static_tys.contains_key(&s.name) || value_spans.contains_key(&s.name) {
                let first = static_defs
                    .iter()
                    .find(|d| d.name == s.name)
                    .map(|d| d.span)
                    .or_else(|| value_spans.get(&s.name).map(|(at, _)| *at));
                duplicated.extend(first);
                pre_diags.push(rokc_diagnostic::duplicate(
                    rokc_diagnostic::ErrorKind::Type,
                    value_dup("static", &s.name, &value_spans),
                    &s.name,
                    here,
                    first,
                    false,
                ));
                continue;
            }
            value_spans.insert(s.name.clone(), (here, "static"));
            // Its type (and literal initializer) once every item is
            // registered.
            static_asts.push((&s.ty, module_of(&s.name)));
            static_tys.insert(s.name.clone(), HirTy::Unknown);
            static_defs.push(HirStaticDef {
                name: s.name.clone(),
                ty: HirTy::Unknown,
                init: ConstVal::Unit,
                is_mut: s.is_mut,
                is_extern: false,
                dll_import: false,
                init_fn: None,
                span: s.span,
            });
        }
        // Spec 5.9.3: `extern "C" { static errno: i32; }` - a C global,
        // read (and with `mut` written) like a `static`.
        if let Item::Extern(blk) = item {
            let dll_import = !links_static(&blk.attrs);
            for ei in &blk.items {
                let rokc_ast::ExternItem::Static(s) = ei else {
                    continue;
                };
                if static_tys.contains_key(&s.name) {
                    pre_diags.push(rokc_diagnostic::duplicate(
                        rokc_diagnostic::ErrorKind::Type,
                        format!("static `{}` is defined more than once", s.name),
                        &s.name,
                        s.span,
                        static_defs
                            .iter()
                            .find(|d| d.name == s.name)
                            .map(|d| d.span),
                        false,
                    ));
                    continue;
                }
                // Its type is resolved in the block's module.
                let module = item_modules.get(idx).cloned().unwrap_or_default();
                static_asts.push((&s.ty, module));
                static_tys.insert(s.name.clone(), HirTy::Unknown);
                static_defs.push(HirStaticDef {
                    name: s.name.clone(),
                    ty: HirTy::Unknown,
                    init: ConstVal::Unit,
                    is_mut: s.is_mut,
                    is_extern: true,
                    dll_import,
                    init_fn: None,
                    span: s.span,
                });
            }
        }
    }
    // Pass 1d: each plain type alias resolved once, for its diagnostics
    // (a use expands the alias where it is resolved).
    for item in &program.items {
        if let Item::TypeAlias(a) = item {
            if !a.generics.params.is_empty() {
                continue;
            }
            let module = module_of(&a.name);
            let sc = Scope {
                names: &names,
                module: &module,
                at: item.span(),
            };
            resolve_ty_with_structs(&a.ty, &defs, None, sc);
        }
    }
    // An alias naming itself, directly or through other aliases and
    // anywhere in the type (`type A = B; type B = A;`, `type T = List<T>;`).
    {
        let alias_items: HashMap<&str, &rokc_ast::TypeAliasDef> = program
            .items
            .iter()
            .filter_map(|i| match i {
                Item::TypeAlias(a) if a.generics.params.is_empty() => Some((a.name.as_str(), a)),
                _ => None,
            })
            .collect();
        let mentions = |a: &rokc_ast::TypeAliasDef| {
            struct Names(Vec<String>);
            impl crate::walk::AstVisitor for Names {
                fn ty(&mut self, t: &rokc_ast::Ty) {
                    // `B` and `B::Out`'s base `B`.
                    if let rokc_ast::TyKind::Path(p) = &t.kind {
                        if let [s] | [s, _] = p.segments.as_slice() {
                            self.0.push(s.name.clone());
                        }
                    }
                }
            }
            let mut v = Names(Vec::new());
            crate::walk::walk_ty(&mut v, &a.ty);
            let module = module_of(&a.name);
            v.0.into_iter()
                .map(|n| {
                    if module.is_empty() {
                        n
                    } else {
                        format!("{module}::{n}")
                    }
                })
                .filter(|n| alias_items.contains_key(n.as_str()))
                .collect::<Vec<_>>()
        };
        let mut cyclic: Vec<&rokc_ast::TypeAliasDef> = alias_items
            .values()
            .filter(|a| {
                let mut stack = mentions(a);
                let mut seen = std::collections::HashSet::new();
                while let Some(n) = stack.pop() {
                    if n == a.name {
                        return true;
                    }
                    if seen.insert(n.clone()) {
                        stack.extend(mentions(alias_items[n.as_str()]));
                    }
                }
                false
            })
            .copied()
            .collect();
        cyclic.sort_by(|a, b| a.name.cmp(&b.name));
        for a in cyclic {
            names.broken_aliases.borrow_mut().insert(a.name.clone());
            err(
                &mut pre_diags,
                a.span,
                format!("type alias `{}` is cyclic", a.name),
            );
        }
    }

    // Pass 2: functions, then `impl` methods. Generic functions stay AST
    // templates, monomorphized per call, outside `decls` / `fn_table`.
    let mut fn_table = fn_table;
    let mut extern_fns = extern_fns;
    // Later extern declarations of a name already declared: whether
    // `extern "rok"`, the first one's id.
    let mut extern_redecls: Vec<(&FnDef, bool, FunctionId)> = Vec::new();
    let mut impls = impls;
    let mut decls = decls;
    let first_decl = decls.len();
    let std_prebuilt = input.prebuilt.iter().any(|p| p == "rok");
    let std_linked = std_prebuilt || input.lib.as_deref() == Some("rok");
    let mut generic_fns = generic_fns;
    // Generic impl method templates are `__impl{tag}_m`, the tag hashing the
    // impl's module, trait and target so it is stable; repeats get a count.
    let mut intrinsics = intrinsics;
    let mut generic_intrinsics = generic_intrinsics;
    let mut unsafe_fns = unsafe_fns;
    let mut extern_fn_ids = extern_fn_ids;
    let mut c_variadic = c_variadic;
    // `extern "rok"` declarations: the runtime's own ABI.
    let mut rok_externs = rok_externs;
    // A declaration's signature: its types are resolved once every item
    // is registered (`mono::decl_sig`), the rest is read off it here.
    let sig_of = |fn_def: &FnDef, id: FunctionId, sc: Scope<'_>| FnSig {
        id,
        ret_ty: HirTy::Unknown,
        param_defaults: fn_def
            .params
            .iter()
            .map(|p| p.default.clone().map(|d| in_module(sc.module, d)))
            .collect(),
        param_tys: vec![HirTy::Unknown; fn_def.params.len()],
        param_names: fn_def.params.iter().map(|p| p.name.clone()).collect(),
        param_refs: fn_def.params.iter().map(|p| p.by_ref).collect(),
        variadic: fn_def.params.last().is_some_and(|p| p.variadic),
    };
    for (idx, item) in program.items.iter().enumerate() {
        let here = item.span();
        match item {
            Item::Fn(fn_def) => {
                let name = &fn_def.name;
                if fn_table.contains_key(name)
                    || generic_fns.contains_key(name)
                    || value_spans.contains_key(name)
                {
                    let first = generic_fns
                        .get(name)
                        .map(|f| f.span)
                        .or_else(|| {
                            let id = fn_table.get(name)?.id.0 as usize;
                            decls.get(id).map(decl_span)
                        })
                        .or_else(|| value_spans.get(name).map(|(at, _)| *at));
                    duplicated.extend(first);
                    pre_diags.push(rokc_diagnostic::duplicate(
                        rokc_diagnostic::ErrorKind::Type,
                        value_dup("function", name, &value_spans),
                        name,
                        here,
                        first,
                        !value_spans.contains_key(name),
                    ));
                    continue;
                }
                let module = module_of(&fn_def.name);
                if !fn_def.generics.params.is_empty() {
                    if let Some(i) = intrinsic_of(fn_def, &module, &mut pre_diags) {
                        generic_intrinsics.insert(fn_def.name.clone(), i);
                    }
                    generic_fns.insert(fn_def.name.clone(), fn_def);
                    continue;
                }
                let id = FunctionId(u32::try_from(decls.len()).expect("FunctionId overflow"));
                if let Some(i) = intrinsic_of(fn_def, &module, &mut pre_diags) {
                    intrinsics.insert(id, i);
                }
                let sc = Scope {
                    names: &names,
                    module: &module,
                    at: here,
                };
                let sig = sig_of(fn_def, id, sc);
                fn_table.insert(fn_def.name.clone(), std::sync::Arc::new(sig));
                decls.push(MethodOrFn::Fn(fn_def));
                lib_decl.push(!package_of(&module).is_empty());
            }
            Item::Extern(blk) => {
                let rok_abi = blk.abi == "rok";
                let quiet = blk.attrs.iter().any(|a| a.path == ["no_safepoint"]);
                if quiet && !rok_abi {
                    err(
                        &mut pre_diags,
                        here,
                        "`#[no_safepoint]` is for `extern \"rok\"` blocks".to_owned(),
                    );
                }
                // `"system"` is `"C"` but on 32-bit x86 Windows (the
                // driver rejects it there).
                if !rok_abi && blk.abi != "C" && blk.abi != "system" {
                    err(
                        &mut pre_diags,
                        here,
                        format!(
                            "unsupported ABI `\"{}\"` (`\"C\"`, `\"system\"`, or `\"rok\"` for the runtime)",
                            blk.abi
                        ),
                    );
                }
                let import_module = wasm_import_module(&blk.attrs);
                let module = item_modules.get(idx).cloned().unwrap_or_default();
                for ei in &blk.items {
                    let rokc_ast::ExternItem::Fn(fn_def) = ei else {
                        continue;
                    };
                    // `rok_*` is the runtime's prefix (8.5), whose `str` is
                    // `(ptr, len)`; `extern "C"` would make it a `char*`.
                    let is_str = |t: &rokc_ast::Ty| matches!(&t.kind, AstTyKind::Path(p) if p.segments.len() == 1 && p.segments[0].name == "str");
                    if !rok_abi
                        && fn_def.name.starts_with("rok_")
                        && (fn_def
                            .params
                            .iter()
                            .filter_map(|p| p.ty.as_ref())
                            .any(is_str)
                            || fn_def.ret_ty.as_ref().is_some_and(is_str))
                    {
                        err(
                            &mut pre_diags,
                            fn_def.span,
                            format!(
                                "`{}`: a `rok_*` symbol is the runtime's, whose `str` is \
                                 `(ptr, len)`; an `extern \"C\"` `str` is a `char*` (spec 5.9.4, \
                                 8.5) - give the C function another name",
                                fn_def.name
                            ),
                        );
                    }
                    // A repeated extern declaration names the same symbol:
                    // keep the first, check the signatures agree below.
                    if let Some(first) = fn_table.get(&fn_def.name) {
                        if extern_fns.contains_key(&fn_def.name) {
                            extern_redecls.push((fn_def, rok_abi, first.id));
                        }
                        continue;
                    }
                    extern_fns.insert(fn_def.name.clone(), module.clone());
                    let id = FunctionId(u32::try_from(decls.len()).expect("FunctionId overflow"));
                    // Spec 4.15.1: an extern function is `unsafe` to call.
                    unsafe_fns.insert(id);
                    let sc = Scope {
                        names: &names,
                        module: "",
                        at: here,
                    };
                    fn_table.insert(
                        fn_def.name.clone(),
                        std::sync::Arc::new(sig_of(fn_def, id, sc)),
                    );
                    if rok_abi {
                        rok_externs.insert(id);
                        if quiet {
                            no_safepoint.insert(id);
                        }
                    } else {
                        extern_fn_ids.insert(id);
                        if blk.abi == "system" {
                            system_fns.insert(id);
                        }
                    }
                    if fn_def
                        .attrs
                        .iter()
                        .any(|a| a.path == [rokc_ast::C_VARIADIC_ATTR])
                    {
                        c_variadic.insert(id);
                    }
                    let link_name = link_name_of(fn_def);
                    if let Some(m) = &import_module {
                        let name = link_name.unwrap_or_else(|| fn_def.name.clone());
                        wasm_imports.insert(id, (m.clone(), name));
                    } else if let Some(n) = link_name {
                        extern_symbols.insert(id, n);
                    }
                    decls.push(MethodOrFn::Fn(fn_def));
                    lib_decl.push(false);
                }
            }
            _ => {}
        }
    }
    // Pre-collect every trait declaration so `impl Trait for T` blocks
    // can pick up default method bodies.
    let mut trait_defs = base_trait_defs;
    for item in &program.items {
        if let Item::Trait(t) = item {
            trait_defs.entry(t.name.clone()).or_insert(t);
        }
    }
    // `any Tr`: each trait's method names and supertrait keys, and
    // (filled per impl below) its implementing types in source order.
    let new_info: Vec<(String, TraitInfo)> = trait_defs
        .iter()
        .filter(|(name, _)| !base_trait_info.contains_key(*name))
        .map(|(name, t)| {
            let module = module_of(name);
            let supers = t
                .supertraits
                .iter()
                .filter_map(|st| match &st.kind {
                    AstTyKind::Path(p) => {
                        let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
                        Some(names.trait_key(&segs, &module, t.span))
                    }
                    _ => None,
                })
                .collect();
            let fns = || {
                t.items.iter().filter_map(|i| match i {
                    rokc_ast::TraitItem::Method(m) => Some(m),
                    _ => None,
                })
            };
            let methods = fns().map(|m| leaf_name(&m.name).to_owned()).collect();
            let params: Vec<&str> = t
                .generics
                .params
                .iter()
                .filter_map(|p| match p {
                    rokc_ast::GenericParam::Type { name, .. } => Some(name.as_str()),
                    rokc_ast::GenericParam::Const { .. } => None,
                })
                .collect();
            let assoc: Vec<&str> = t
                .items
                .iter()
                .filter_map(|i| match i {
                    rokc_ast::TraitItem::AssocType { name, .. } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            let needs = fns()
                .map(|m| {
                    let mut out = Vec::new();
                    for ty in std::iter::once(sig_ret_ast(m)).chain(
                        m.params
                            .iter()
                            .filter(|p| p.name != "self")
                            .filter_map(|p| p.ty.clone()),
                    ) {
                        ast_ty_needs(&ty, &params, &assoc, &mut out);
                    }
                    (leaf_name(&m.name).to_owned(), out)
                })
                .collect();
            (
                name.clone(),
                TraitInfo {
                    methods,
                    supers,
                    needs,
                    decls: fns()
                        .map(|m| (leaf_name(&m.name).to_owned(), m.clone()))
                        .collect(),
                    module,
                    params: params.iter().map(|p| (*p).to_owned()).collect(),
                    generics: t.generics.clone(),
                    super_tys: t.supertraits.clone(),
                    assoc: t
                        .items
                        .iter()
                        .filter_map(|i| match i {
                            rokc_ast::TraitItem::AssocType { name, bounds, .. } => {
                                Some((name.clone(), bounds.clone()))
                            }
                            _ => None,
                        })
                        .collect(),
                    consts: t
                        .items
                        .iter()
                        .filter_map(|i| match i {
                            rokc_ast::TraitItem::Const { name, ty, .. } => {
                                Some((name.clone(), ty.clone()))
                            }
                            _ => None,
                        })
                        .collect(),
                },
            )
        })
        .collect();
    let mut trait_info = base_trait_info;
    trait_info.extend(new_info);
    let mut neg_auto = neg_auto;
    // Impls registered by their written target, resolved once every item
    // is (`impl_heads`).
    let mut pending: Vec<PendingImpl<'_>> = Vec::new();

    for (idx, item) in program.items.iter().enumerate() {
        let here = item.span();
        let Item::Impl(impl_def) = item else { continue };
        let module = impl_module(impl_def);
        if !check_auto_impl(impl_def, &names, &module, &mut neg_auto, &mut pre_diags) {
            continue;
        }
        let impl_generic = !impl_def.generics.params.is_empty();
        let consts = impl_const_lists.remove(&idx).unwrap_or_default();
        let trait_name: Option<String> = impl_def.trait_path.as_ref().map(|tp| {
            let segs: Vec<&str> = tp.segments.iter().map(|s| s.name.as_str()).collect();
            names.trait_key(&segs, &module, here)
        });
        // A generic trait impl, or a generic inherent impl on a structural
        // type or on some instances of a base (`impl<V> P<str, V>`): matched
        // against the receiver, its methods templates.
        if impl_generic && (trait_name.is_some() || !covers_base(impl_def)) {
            let trait_key = trait_name.unwrap_or_default();
            let td = trait_defs.get(&trait_key).copied();
            let id = format!(
                "{module}|{trait_key}|{}",
                impl_def_display(&impl_def.self_ty)
            );
            let mut tag = format!("{:08x}", rokc_core::hash::fnv(id.as_bytes()) as u32);
            let seen = templated.entry(tag.clone()).or_insert(0);
            if *seen > 0 {
                tag = format!("{tag}_{seen}");
            }
            *seen += 1;
            let mut gi = generic_trait_impl(
                impl_def,
                trait_key,
                td,
                &module,
                &tag,
                &mut pre_diags,
                &mut duplicated,
            );
            gi.consts = consts;
            impls.push(gi);
            continue;
        }
        // Generic inherent methods (and generic methods anywhere) are
        // templates keyed by the target's generic base.
        let generic_base = match &impl_def.self_ty.kind {
            AstTyKind::Path(p) => {
                let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
                // `impl str { fn parse<T>(..) }`: a primitive's generic
                // methods key by the primitive's name.
                names.canon(&segs, &module, &[], here).or_else(|| {
                    match crate::ty::resolve_ty(&impl_def.self_ty) {
                        HirTy::Prim(prim) => Some(prim.name().to_owned()),
                        _ => None,
                    }
                })
            }
            _ => None,
        };
        if impl_generic && generic_base.is_none() {
            let target = impl_def_display(&impl_def.self_ty);
            let is_param = impl_def.generics.params.iter().any(
                |gp| matches!(gp, rokc_ast::GenericParam::Type { name, .. } if *name == target),
            );
            if is_param {
                err(
                    &mut pre_diags,
                    here,
                    format!(
                        "an inherent `impl` attaches methods to a type, and `{target}` is a \
                         type parameter of this `impl` (spec 2.9.1); methods for every type \
                         belong to a trait: `impl<{target}> Tr for {target} {{ .. }}`"
                    ),
                );
            } else {
                // A path that names no type (other targets are templates above).
                let name = target.split('<').next().unwrap_or(&target);
                let d = rokc_diagnostic::RokDiagnostic::error(
                    rokc_diagnostic::ErrorKind::Resolve,
                    here,
                    format!("cannot find type `{name}` in this scope"),
                );
                let help = (!name.contains("::"))
                    .then(|| {
                        names.import_help(&module, names.type_leaf(name), |k| names.type_vis(k))
                    })
                    .flatten();
                pre_diags.push(match help {
                    Some(h) => d.with_help(h),
                    None => d,
                });
            }
            continue;
        }
        pending.push(PendingImpl {
            def: impl_def,
            module: module.clone(),
            trait_name: trait_name.clone(),
            generic_base,
            at: impls.all.len(),
        });
        impls.push(Impl {
            trait_key: trait_name.unwrap_or_default(),
            trait_args: impl_def
                .trait_path
                .as_ref()
                .and_then(|tp| tp.segments.last())
                .map(|s| s.generics.clone())
                .unwrap_or_default(),
            assoc: impl_assoc_list(impl_def),
            generics: impl_def.generics.clone(),
            where_clause: impl_def.where_clause.clone(),
            self_ty: impl_def.self_ty.clone(),
            module,
            methods: HashMap::new(),
            consts,
            concrete: None,
        });
    }

    // Install `pub use` re-exports as fn / static table aliases (types go
    // through `TypeNames`).
    for (alias_key, end) in &aliases {
        if let Some(sig) = fn_table.get(end).cloned() {
            fn_table.entry(alias_key.clone()).or_insert(sig);
        }
        if let Some(def) = generic_fns.get(end).copied() {
            generic_fns.entry(alias_key.clone()).or_insert(def);
        }
        if let Some(ty) = static_tys.get(end).cloned() {
            static_tys.entry(alias_key.clone()).or_insert(ty);
        }
    }

    // Name → Visibility for every value-namespace item (fns, consts,
    // statics) plus type items; consulted by path resolution.
    let mut item_visibility = item_visibility;
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                item_visibility.entry(f.name.clone()).or_insert(f.vis);
            }
            Item::Const(c) => {
                item_visibility.entry(c.name.clone()).or_insert(c.vis);
            }
            Item::Static(s) => {
                item_visibility.entry(s.name.clone()).or_insert(s.vis);
            }
            _ => {}
        }
    }
    let mut field_vis = field_vis;
    field_vis.extend(collect_field_visibility(program));

    // Pass 3: function bodies.
    let (bound_methods, bound_traits) = super::bounds::templates(&trait_info);
    let mut_statics: std::collections::HashSet<String> = mut_statics
        .into_iter()
        .chain(
            static_defs
                .iter()
                .filter(|s| s.is_mut)
                .map(|s| s.name.clone()),
        )
        .collect();
    // The package's handlers ahead of the library's.
    let mut stage_shapes = literal_handlers(program, &mut pre_diags);
    stage_shapes.extend(literal_shapes);
    let literal_shapes = stage_shapes;
    let mut env = EnvIn {
        sources,
        prec,
        t: Tables {
            infer_ret: base_infer_ret,
            implicit_main: input.root_main == crate::RootMain::Implicit,
            main_lets: if input.root_main == crate::RootMain::Implicit {
                main_lets(program)
            } else {
                HashMap::new()
            },
            harness_main: input.root_main == crate::RootMain::Harness,
            may_diverge: base_may_diverge
                .into_iter()
                .chain(may_diverge(program, &mut diverging))
                .collect(),
            duplicated: std::cell::RefCell::default(),
            fn_table,
            fn_leaves: std::cell::OnceCell::new(),
            extern_fns,
            method_table,
            key_of: base_key_of,
            method_info,
            defs,
            consts,
            const_leaves: std::cell::RefCell::default(),
            generic_fns,
            generic_methods,
            intrinsics,
            generic_intrinsics,
            extern_fn_ids: extern_fn_ids.clone(),
            rok_externs: rok_externs.clone(),
            c_variadic: c_variadic.clone(),
            c_defs: base_c_defs,
            stdcall_fns: base_stdcall,
            sink,
            item_visibility,
            field_vis,
            impls,
            unsafe_fns: std::collections::HashSet::new(),
            neg_auto,
            auto_memo: std::cell::RefCell::default(),
            trait_info,
            statics: static_tys,
            static_leaves: std::cell::RefCell::default(),
            mut_statics,
            literal_shapes,
            names,
            decls,
            non_exhaustive: non_exhaustive
                .into_iter()
                .chain(program.items.iter().filter_map(|i| match i {
                    Item::Struct(s) if s.attrs.iter().any(|a| a.path == ["non_exhaustive"]) => {
                        Some(s.name.clone())
                    }
                    Item::Enum(e) if e.attrs.iter().any(|a| a.path == ["non_exhaustive"]) => {
                        Some(e.name.clone())
                    }
                    _ => None,
                }))
                .collect(),
            bound_methods,
            bound_traits,
        },
    };
    // Impl targets and trait arguments, now that every item is
    // registered; then each impl's methods, keyed by them.
    let heads = rokc_diagnostic::time("hir impls", || {
        impl_heads(&mut env, &pending, &trait_defs, &mut pre_diags)
    });
    let mut fn_table = std::mem::take(&mut env.fn_table);
    let mut decls = std::mem::take(&mut env.decls);
    let mut method_table = std::mem::take(&mut env.method_table);
    let mut method_info = std::mem::take(&mut env.method_info);
    let mut intrinsics = std::mem::take(&mut env.intrinsics);
    let mut generic_methods = std::mem::take(&mut env.generic_methods);
    let names = &env.names;
    let defs = &env.defs;
    for (p, head) in pending.iter().zip(heads) {
        // `None`: a repeated impl, reported.
        let Some((target_ty, provides)) = head else {
            continue;
        };
        let impl_def = p.def;
        let here = impl_def.span;
        let module = p.module.clone();
        let sc = Scope {
            names,
            module: &module,
            at: here,
        };
        let impl_generic = !impl_def.generics.params.is_empty();
        let trait_name = p.trait_name.clone();
        let generic_base = p.generic_base.clone();
        let target = method_target_for(&target_ty);
        // `From<i32>` / `From<i8>` impls on one type keep their methods
        // apart by the trait's arguments (`overload_args`).
        let trait_args_key: String = provides.as_ref().map_or_else(String::new, |r| {
            r.args
                .iter()
                .map(|a| "_".to_owned() + &ty_mangle(a))
                .collect()
        });
        let trait_args: Vec<HirTy> = provides
            .as_ref()
            .map(|r| r.args.clone())
            .unwrap_or_default();
        let trait_def = trait_name
            .as_ref()
            .and_then(|tn| trait_defs.get(tn).copied());
        let target_display = impl_def_display(&impl_def.self_ty);
        let mut dup = |leaf: &str, at: Span, first: Option<Span>| {
            duplicated.extend(first);
            rokc_diagnostic::duplicate(
                rokc_diagnostic::ErrorKind::Type,
                format!("method `{leaf}` is defined more than once for `{target_display}`"),
                leaf,
                at,
                first,
                true,
            )
        };
        // Methods this impl provides: their trait defaults are skipped.
        let mut explicit_methods: HashMap<String, Span> = HashMap::new();
        for ii in &impl_def.items {
            let m = match ii {
                AstImplItem::Method(m) => m,
                AstImplItem::Const(_) | AstImplItem::AssocType { .. } => continue,
            };
            let leaf = leaf_name(&m.name).to_owned();
            if let Some(first) = explicit_methods.get(&leaf) {
                pre_diags.push(dup(&leaf, m.span, Some(*first)));
                continue;
            }
            explicit_methods.insert(leaf.clone(), m.span);
            if let (Some(base), true) =
                (&generic_base, impl_generic || !m.generics.params.is_empty())
            {
                // Impl generics first, then the method's own; `self` and
                // `Self` take the impl target type at instantiation.
                let mut def = m.clone();
                let base_leaf = leaf_name(base);
                // `Box__get` is both the mono cache key and the name
                // diagnostics print.
                def.name = if module.is_empty() {
                    format!("{base_leaf}__{leaf}")
                } else {
                    format!("{module}::{base_leaf}__{leaf}")
                };
                def.generics.params = [&impl_def.generics.params, &m.generics.params]
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect();
                def.where_clause = [&impl_def.where_clause, &m.where_clause]
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect();
                let slot = generic_methods
                    .entry((base.clone(), leaf.clone()))
                    .or_default();
                if let Some(first) = slot
                    .iter()
                    .find(|g| g.info.trait_name.is_none() && trait_name.is_none())
                {
                    pre_diags.push(dup(&leaf, m.span, Some(first.def.span)));
                    continue;
                }
                // A trait impl's generic method (`fn from_iter<I>` of
                // `impl FromIterator<char> for str`): as visible as the trait.
                slot.push(GenericMethod {
                    def: std::sync::Arc::new(def),
                    self_ty: impl_def.self_ty.clone(),
                    info: MethodInfo {
                        module: module.clone(),
                        vis: if trait_name.is_some() {
                            Visibility::Pub
                        } else {
                            m.vis
                        },
                        trait_name: trait_name.clone(),
                        trait_args: trait_args.clone(),
                    },
                });
                continue;
            }
            if let Some(td) = trait_def {
                let tm = td.items.iter().find_map(|ti| match ti {
                    rokc_ast::TraitItem::Method(tm) if tm.name == leaf => Some(tm),
                    _ => None,
                });
                match tm {
                    None => err(
                        &mut pre_diags,
                        here,
                        format!("method `{leaf}` is not a member of trait `{}`", td.name),
                    ),
                    // Spec 4.14.11: a throwing fn can't fill a non-throwing
                    // slot. The operator traits (stdlib 2.3) never throw.
                    Some(tm) if is_throwing(&m.throws) && !is_throwing(&tm.throws) => {
                        err(
                            &mut pre_diags,
                            here,
                            format!(
                                "`{leaf}` throws but `{}::{leaf}` is declared without \
                                 `throws`, so an impl can't add one (spec 4.14.11){}",
                                td.name,
                                if td.attrs.iter().any(|a| a.path == ["lang"]) {
                                    " - operators don't throw; return a `Result` or use \
                                     a named method"
                                } else {
                                    ""
                                }
                            ),
                        );
                    }
                    // Mismatched with the trait: reported here and left out,
                    // so trait-method callers report nothing more.
                    Some(tm) if sig_mismatch(m, tm, &td.name, &mut pre_diags) => continue,
                    Some(_) => {}
                }
            }
            let Some(key) = method_key(
                &fn_table,
                &method_info,
                &target_ty,
                &leaf,
                trait_name.as_deref(),
                &trait_args_key,
            ) else {
                let first = fn_table
                    .get(&format!("{}__{leaf}", ty_mangle(&target_ty)))
                    .and_then(|sig| decls.get(sig.id.0 as usize))
                    .map(decl_span);
                pre_diags.push(dup(&leaf, m.span, first));
                continue;
            };
            let id = FunctionId(u32::try_from(decls.len()).expect("FunctionId overflow"));
            let sig = sig_of(m, id, sc);
            fn_table.insert(key.clone(), std::sync::Arc::new(sig));
            if let Some(i) = intrinsic_of(m, &module, &mut pre_diags) {
                intrinsics.insert(id, i);
            }
            if let Some(t) = target.clone() {
                method_table.entry((t, leaf.clone())).or_default().push(id);
            }
            // Trait-impl methods are as visible as the trait itself;
            // inherent methods carry their own visibility.
            let vis = if trait_name.is_some() {
                Visibility::Pub
            } else {
                m.vis
            };
            method_info.insert(
                id,
                MethodInfo {
                    module: module.clone(),
                    vis,
                    trait_name: trait_name.clone(),
                    trait_args: trait_args.clone(),
                },
            );
            // A dependency's method symbol must not depend on this
            // build's struct numbering, so prebuilt code and its users
            // agree.
            if resolve::is_dependency(package_of(&module))
                || (std_linked && !package_of(&module).is_empty())
            {
                let mut name = format!("{module}::{}.{leaf}", impl_def_display(&impl_def.self_ty));
                if let Some(tp) = &impl_def.trait_path {
                    name.push_str("__by_");
                    name.push_str(&impl_def_display(&Ty {
                        kind: AstTyKind::Path(tp.clone()),
                        span: tp.span,
                    }));
                }
                link_names.insert(decls.len(), name);
            }
            decls.push(MethodOrFn::Method {
                fn_def: std::borrow::Cow::Borrowed(m),
                self_ty: target_ty.clone(),
                module: module.clone(),
                key,
                subst: HashMap::new(),
            });
            lib_decl.push(!package_of(&module).is_empty());
        }

        // Trait defaults the impl doesn't override, lowered as methods
        // with `Self = target_ty`. A missing required method is an error.
        if let Some(trait_def) = trait_def {
            let trait_module = module_of(&trait_def.name);
            for ti in &trait_def.items {
                let rokc_ast::TraitItem::Method(tm) = ti else {
                    continue;
                };
                if explicit_methods.insert(tm.name.clone(), tm.span).is_some() {
                    continue;
                }
                if tm.body.is_none() {
                    err(
                        &mut pre_diags,
                        here,
                        format!(
                            "impl of trait `{}` for `{target_display}` is missing method `{}`",
                            trait_def.name, tm.name
                        ),
                    );
                    continue;
                }
                let Some(key) = method_key(
                    &fn_table,
                    &method_info,
                    &target_ty,
                    &tm.name,
                    trait_name.as_deref(),
                    &trait_args_key,
                ) else {
                    err(
                        &mut pre_diags,
                        here,
                        format!(
                            "method `{}` is defined more than once for `{target_display}`",
                            tm.name
                        ),
                    );
                    continue;
                };
                // A default with its own type parameters or naming the
                // trait's associated types becomes a template, instantiated
                // per call with this impl's types.
                if !tm.generics.params.is_empty()
                    || sig_mentions(tm, &trait_assoc_names(trait_def))
                    || returns_self_generic(tm)
                {
                    let base = match &target_ty {
                        HirTy::Struct(id) => {
                            Some(defs.structs.borrow()[id.0 as usize].name.clone())
                        }
                        HirTy::Enum(id) => Some(defs.enums.borrow()[id.0 as usize].name.clone()),
                        _ => None,
                    };
                    if let Some(base) = base {
                        let mut def = tm.clone();
                        subst_assoc_sig(&mut def, &impl_assoc_types(impl_def));
                        def.name = format!("{trait_module}::{}__{}", leaf_name(&base), tm.name);
                        let slot = generic_methods.entry((base, tm.name.clone())).or_default();
                        if !slot.is_empty() {
                            // Another trait's same-named default: its own
                            // mono cache key / symbol.
                            def.name = format!(
                                "{}__by_{}",
                                def.name,
                                leaf_name(trait_name.as_deref().unwrap_or_default())
                            );
                        }
                        slot.push(GenericMethod {
                            def: std::sync::Arc::new(def),
                            self_ty: impl_def.self_ty.clone(),
                            info: MethodInfo {
                                module: trait_module.clone(),
                                vis: Visibility::Pub,
                                trait_name: trait_name.clone(),
                                trait_args: Vec::new(),
                            },
                        });
                    }
                    // A primitive / tuple target gets no such defaults.
                    continue;
                }
                // `trait Contains<Item>` + `impl Contains<i32> for S`: the
                // default's signature sees `Item = i32`.
                let mut targs = trait_param_args(trait_def, impl_def);
                // Const arguments (`3`, `W`, `{ W + 1 }`) become literals.
                for gp in &trait_def.generics.params {
                    if let rokc_ast::GenericParam::Const { name, .. } = gp {
                        if let Some(t) = targs.get_mut(name) {
                            if let Some(v) = const_arg_value(t, names, sc.module) {
                                *t = const_literal_ty(v, t.span);
                            }
                        }
                    }
                }
                let body_subst: HashMap<String, HirTy> = trait_def
                    .generics
                    .params
                    .iter()
                    .enumerate()
                    .filter_map(|(i, gp)| match gp {
                        rokc_ast::GenericParam::Type { name, .. } => {
                            Some((name.clone(), provides.as_ref()?.args.get(i)?.clone()))
                        }
                        rokc_ast::GenericParam::Const { name, ty } => {
                            let v = match &targs.get(name)?.kind {
                                AstTyKind::Const(c) => {
                                    super::const_eval::syntactic_int(c, &|_| None)?
                                }
                                _ => return None,
                            };
                            let prim = match resolve_ty(ty) {
                                HirTy::Prim(p) => Some(p),
                                _ => None,
                            };
                            Some((name.clone(), HirTy::Const(v, prim)))
                        }
                    })
                    .collect();
                let tm: std::borrow::Cow<'_, FnDef> = if targs.is_empty() {
                    std::borrow::Cow::Borrowed(tm)
                } else {
                    let mut d = tm.clone();
                    subst_assoc_sig(&mut d, &targs);
                    std::borrow::Cow::Owned(d)
                };
                let id = FunctionId(u32::try_from(decls.len()).expect("FunctionId overflow"));
                let tsc = Scope {
                    names,
                    module: &trait_module,
                    at: here,
                };
                let sig = sig_of(&tm, id, tsc);
                fn_table.insert(key.clone(), std::sync::Arc::new(sig));
                if let Some(t) = target.clone() {
                    method_table
                        .entry((t, tm.name.clone()))
                        .or_default()
                        .push(id);
                }
                method_info.insert(
                    id,
                    MethodInfo {
                        module: trait_module.clone(),
                        vis: Visibility::Pub,
                        trait_name: trait_name.clone(),
                        trait_args: trait_args.clone(),
                    },
                );
                decls.push(MethodOrFn::Method {
                    fn_def: tm,
                    self_ty: target_ty.clone(),
                    module: trait_module.clone(),
                    key,
                    subst: body_subst,
                });
                // A library default in a user impl is checked with it.
                lib_decl.push(!package_of(&module).is_empty());
            }
        }
    }
    for (i, d) in decls.iter().enumerate().skip(first_decl) {
        let f = d.def();
        let here = f.span;
        let id = FunctionId(u32::try_from(i).expect("FunctionId overflow"));
        if f.is_unsafe {
            unsafe_fns.insert(id);
        }
        if let Some(abi) = rokc_ast::extern_fn_abi(&f.attrs) {
            check_extern_fn(f, abi, &mut pre_diags);
            if abi == "system" {
                system_fns.insert(id);
            }
            c_defs.push(id);
        }
        if !c_variadic.contains(&id)
            && f.attrs
                .iter()
                .any(|a| a.path == [rokc_ast::C_VARIADIC_ATTR])
        {
            err(
                &mut pre_diags,
                here,
                format!(
                    "`{}`: a C variadic `...` parameter is only allowed on a declaration in an \
                     `extern \"C\" {{ .. }}` block (spec 8.6.10)",
                    f.name
                ),
            );
        }
        let module = match d {
            MethodOrFn::Fn(f) => fn_module(&env, f),
            MethodOrFn::Method { module, .. } => module.clone(),
        };
        // The prebuilt std interface keeps its bodies (const evaluation runs
        // them); its `.a` has the code of those named alike in every build
        // (not trait defaults). A prebuilt package's `.a` initializes its
        // lazy statics.
        let stable = matches!(d, MethodOrFn::Fn(_)) || link_names.contains_key(&i);
        let std_fn = std_prebuilt && within(package_of(&module), "rok");
        let linked =
            f.body.is_none() || is_lazy_init_fn(&f.name) || (std_fn && stable && in_archive(f));
        let prebuilt_fn = linked
            && !(extern_fn_ids.contains(&id) || rok_externs.contains(&id))
            && input
                .prebuilt
                .iter()
                .any(|p| within(package_of(&module), p));
        if prebuilt_fn {
            prebuilt.insert(id);
            if input.inline_prebuilt && std_fn && f.body.is_some() && small_body(f) {
                inline_copies.insert(id);
            }
        }
        if let Some(sym) = export_name(f) {
            if f.body.is_none() && !prebuilt_fn {
                err(
                    &mut pre_diags,
                    here,
                    format!("`#[export_name]` on `{}` needs a function body", f.name),
                );
            } else if exports.iter().any(|(_, s)| *s == sym) {
                err(
                    &mut pre_diags,
                    here,
                    format!("symbol `{sym}` is exported twice (`#[export_name]`)"),
                );
            } else {
                exports.push((id, sym));
            }
        }
    }
    let stage_generics = program.items.iter().filter_map(|i| match i {
        Item::Fn(f) if !f.generics.params.is_empty() => Some(f),
        _ => None,
    });
    for f in stage_generics {
        if let Some(abi) = rokc_ast::extern_fn_abi(&f.attrs) {
            check_extern_fn(f, abi, &mut pre_diags);
        }
        if export_name(f).is_some() {
            err(
                &mut pre_diags,
                f.span,
                format!(
                    "`#[export_name]` on generic function `{}`: only a concrete function \
                     has one symbol",
                    f.name
                ),
            );
        }
    }
    let mut key_of: HashMap<FunctionId, String> = HashMap::new();
    for (k, s) in &fn_table {
        // Re-export aliases share the id; keep the shortest (original) key.
        let e = key_of.entry(s.id).or_insert_with(|| k.clone());
        if k.len() < e.len() {
            e.clone_from(k);
        }
    }
    let mut infer_ret = std::mem::take(&mut env.infer_ret);
    infer_ret.extend(
        decls
            .iter()
            .enumerate()
            .skip(first_decl)
            .filter(|(_, d)| mono::infers_ret(d.def(), |s| env.may_diverge.contains(&s)))
            .map(|(i, _)| FunctionId(u32::try_from(i).expect("FunctionId overflow"))),
    );
    env.fn_table = fn_table;
    env.decls = decls;
    env.duplicated = duplicated.into();
    env.method_table = method_table;
    env.method_info = method_info;
    env.intrinsics = intrinsics;
    env.generic_methods = generic_methods;
    env.key_of = key_of;
    env.infer_ret = infer_ret;
    env.unsafe_fns = unsafe_fns;
    env.c_defs = c_defs.iter().copied().collect();
    if input.stdcall_system {
        env.stdcall_fns.clone_from(&system_fns);
    }
    env.sink
        .next_id
        .set(u32::try_from(env.decls.len()).expect("FunctionId overflow"));
    // Fields and payloads naming projections waited for the impls; resolve
    // them now.
    resolve_projection_aliases(&env, program);
    // Every signature, now that all items and impls exist. Array lengths
    // calling `const fn`s wait for a second round.
    let mut waiting: Vec<FunctionId> = Vec::new();
    let mut sigs: HashMap<FunctionId, FnSig> = (first_decl..env.decls.len())
        .map(|i| {
            let id = FunctionId(u32::try_from(i).expect("FunctionId overflow"));
            let base = &env.fn_table[&env.key_of[&id]];
            env.sink.sig_pass.set(Some(false));
            let sig = mono::decl_sig(&env, i, base);
            if env.sink.sig_pass.get() == Some(true) {
                waiting.push(id);
            }
            (id, sig)
        })
        .collect();
    env.sink.sig_pass.set(None);
    install_sigs(&mut env.fn_table, &sigs);
    for id in waiting {
        let base = &env.fn_table[&env.key_of[&id]];
        let sig = mono::decl_sig(&env, id.0 as usize, base);
        sigs.insert(id, sig);
    }
    install_sigs(&mut env.fn_table, &sigs);
    resolve_open_members(&env, &struct_decls, &enum_decls);
    // Spec 5.9.4: one extern namespace; a second declaration with another
    // signature is an error, reported against the runtime's declaration
    // (`rok::core::rt`) when one is involved.
    let no_subst = HashMap::new();
    for (f, rok_abi, first) in &extern_redecls {
        let (params, ret) = mono::sig_tys(&env, f, None, fn_module(&env, f), &no_subst);
        // The first declaration may be an earlier stage's.
        let s = sigs
            .get(first)
            .unwrap_or_else(|| &env.fn_table[&env.key_of[first]]);
        let c_var = |f: &FnDef| {
            f.attrs
                .iter()
                .any(|a| a.path == [rokc_ast::C_VARIADIC_ATTR])
        };
        let MethodOrFn::Fn(first_def) = env.decls[first.0 as usize] else {
            continue;
        };
        if params == s.param_tys
            && ret == s.ret_ty
            && c_var(f) == c_var(first_def)
            && f.params.last().is_some_and(|p| p.variadic) == s.variadic
        {
            continue;
        }
        let cx = LowerCtx::new(&env, &no_subst, None, f.span);
        let show = |ps: &[HirTy], r: &HirTy| {
            let ps: Vec<String> = ps.iter().map(|t| cx.ty_name(t)).collect();
            format!("fn({}) -> {}", ps.join(", "), cx.ty_name(r))
        };
        let (here, other, at) = if *rok_abi && !rok_externs.contains(first) {
            (
                show(&s.param_tys, &s.ret_ty),
                show(&params, &ret),
                first_def.span,
            )
        } else {
            (show(&params, &ret), show(&s.param_tys, &s.ret_ty), f.span)
        };
        let elsewhere = if at == f.span { first_def.span } else { f.span };
        // Calls are checked against neither declaration.
        env.duplicated.borrow_mut().insert(first_def.span);
        pre_diags.push(
            rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Type,
                at,
                format!(
                    "extern function `{}` is declared twice with different signatures: `{here}` \
                     here, `{other}` elsewhere{} (spec 5.9.4)",
                    f.name,
                    if f.name.starts_with("rok_") {
                        "; `rok_*` symbols are the runtime's, with the signatures of spec 8.5"
                    } else {
                        ""
                    }
                ),
            )
            .with_secondary(elsewhere, format!("declared as `{other}` here"))
            .with_help(format!(
                "a C symbol has one signature: declare `{}` as `{other}` here too, or drop \
                 this declaration",
                f.name
            )),
        );
    }
    let mut ref_out: Vec<(FunctionId, RefOut)> = sigs
        .values()
        .filter_map(|s| Some((s.id, ref_out_of(s)?)))
        .collect();
    ref_out.sort_by_key(|(id, _)| *id);
    env.sink.ref_out.borrow_mut().extend(ref_out);
    // Every static's type, now that every item is registered.
    let no_subst = HashMap::new();
    let static_types: Vec<HirTy> = static_defs[first_static..]
        .iter()
        .zip(&static_asts[first_static..])
        .map(|(d, (ast, module))| {
            let mut rcx = LowerCtx::new(&env, &no_subst, None, d.span);
            rcx.current_module.clone_from(module);
            rcx.resolve_ast_ty(ast)
        })
        .collect();
    for (d, ty) in static_defs[first_static..].iter_mut().zip(static_types) {
        env.statics.insert(d.name.clone(), ty.clone());
        d.ty = ty;
    }
    assign_discriminants(&env, &enum_decls[first_new_enum..]);
    let collected = Collected {
        t: env.t,
        early,
        pre_diags,
        reexports,
        enum_decls,
        struct_decls,
        inherent_keys,
        static_defs,
        static_asts,
        lib_decl,
        link_names,
        templated,
        system_fns,
        no_safepoint,
        wasm_imports,
        extern_symbols,
        trait_defs,
        exports,
        prebuilt,
        inline_copies,
        c_defs,
        diverging,
    };
    Ok((collected, program))
}

/// After both stages: the checks over every impl, the roots, the
/// bodies, and the module.
#[allow(clippy::too_many_lines)] // linear sequence of lowering passes
fn finish<'a>(
    c: Collected<'a>,
    programs: [&'a Program; 2],
    raw_uses: &[(String, Visibility, rokc_ast::UseTree)],
    at: &Stage<'a>,
) -> HirModule {
    let Stage {
        prec,
        input,
        sources,
        ..
    } = *at;
    let lib_pkg: Option<String> = input.lib.clone();
    let check_library = input.check_library;
    let Collected {
        t,
        early,
        pre_diags,
        mut static_defs,
        lib_decl,
        link_names,
        system_fns,
        no_safepoint,
        wasm_imports,
        extern_symbols,
        mut exports,
        prebuilt,
        inline_copies,
        c_defs,
        ..
    } = c;
    let env = Env { sources, prec, t };
    // The files of the package's declarations, and of the library's.
    let decl_files = {
        let files = |lib: bool| -> std::collections::HashSet<u32> {
            (env.decls.iter().zip(&lib_decl))
                .filter(|(_, l)| **l == lib)
                .map(|(d, _)| d.def().span.file)
                .collect()
        };
        (files(false), files(true))
    };
    // Coherence and impl rules for the package's impls (a dependency's
    // when it is built), every library's with `--check-library`.
    let own = |module: &str| {
        let pkg = package_of(module);
        pkg.is_empty() || check_library || lib_pkg.as_ref().is_some_and(|p| within(pkg, p))
    };
    let checked: &[&Program] = if check_library || lib_pkg.is_some() {
        &programs
    } else {
        &programs[1..]
    };
    check_impl_overlap(&env, &own);
    check_impl_rules(&env, checked, &programs, &own);
    let failed_derives: Vec<Span> = checked
        .iter()
        .flat_map(|p| check_derives(&env, p))
        .collect();
    let main = env
        .fn_table
        .get("main")
        .map(|s| s.id)
        .filter(|id| !env.extern_fn_ids.contains(id) && !env.rok_externs.contains(id));
    let exported: std::collections::HashSet<FunctionId> =
        exports.iter().map(|(id, _)| *id).collect();
    let decl_roots: Vec<bool> = env
        .decls
        .iter()
        .zip(&lib_decl)
        .enumerate()
        .map(|(i, (d, &lib))| {
            let f = d.def();
            let module = || match d {
                MethodOrFn::Fn(f) => fn_module(&env, f),
                MethodOrFn::Method { module, .. } => module.clone(),
            };
            !lib || check_library
                || exported.contains(&FunctionId(u32::try_from(i).expect("FunctionId overflow")))
                || is_lazy_init_fn(&f.name)
                || lib_pkg
                    .as_ref()
                    .is_some_and(|p| within(package_of(&module()), p))
        })
        .collect();
    let extern_fn_ids = env.extern_fn_ids.clone();
    let rok_externs = env.rok_externs.clone();
    let c_variadic = env.c_variadic.clone();
    rokc_diagnostic::time("hir consts", || super::const_eval::check_user_consts(&env));
    let incr = input.bodies.as_ref().map(|b| {
        rokc_diagnostic::time("hir interface", || {
            let lib_decls = lib_decl.iter().take_while(|&&l| l).count();
            super::incr::Incr::new(&env, &*b.0, &programs, raw_uses, input, lib_decls)
        })
    });
    // Spec 2.10.9: the package's own generic bodies (a dependency's when
    // it is built prebuilt); every library's with `--check-library`.
    rokc_diagnostic::time("hir generic bodies", || {
        super::bounds::check_all(&env, incr.as_ref(), |module| {
            let pkg = package_of(module);
            pkg.is_empty()
                || check_library
                || lib_pkg
                    .as_ref()
                    .is_some_and(|p| p != "rok" && within(pkg, p))
        });
    });
    let (mut functions, unlowered) = rokc_diagnostic::time("hir bodies", || {
        lower_bodies(&env, &decl_roots, &prebuilt, &inline_copies, incr.as_ref())
    });
    let reuse = incr.map(|i| i.reuse.get()).unwrap_or_default();
    rokc_diagnostic::time("hir const fns", || {
        let origin = env.sink.fn_origin.borrow();
        let user_const = |f: &&HirFunction| match origin.get(&f.id) {
            Some(&(is_const, std)) => is_const && !std,
            None => env.decls.get(f.id.0 as usize).is_some_and(|d| {
                d.def()
                    .attrs
                    .iter()
                    .any(|a| a.path == [rokc_ast::CONST_FN_ATTR])
                    && package_of(&match d {
                        MethodOrFn::Fn(f) => fn_module(&env, f),
                        MethodOrFn::Method { module, .. } => module.clone(),
                    })
                    .is_empty()
            }),
        };
        // Checking may lower more bodies (into `extras`): a copy of the
        // ones it checks.
        let extras: Vec<HirFunction> = env
            .sink
            .extras
            .borrow()
            .iter()
            .filter(user_const)
            .cloned()
            .collect();
        let fns: Vec<&HirFunction> = functions.iter().filter(user_const).chain(&extras).collect();
        drop(origin);
        super::const_eval::check_const_fns(&env, &fns);
    });

    // Spec 5.3.1: an initializer evaluated now needs no lazy init.
    let folded_statics: HashMap<String, ConstVal> = static_defs
        .iter()
        .filter_map(|s| {
            let init = lazy_init_fn_name(&s.name);
            let f = functions.iter().find(|f| f.name == init)?;
            Some((s.name.clone(), super::const_eval::static_init(&env, f)?))
        })
        .collect();
    for (i, name) in link_names {
        functions[i].name = name;
    }
    check_type_bounds(&env);
    check_statics_sync(&env, checked);
    rokc_diagnostic::time("hir send check", || {
        *env.auto_memo.borrow_mut() = Some(HashMap::new());
        super::send_check::check(&env, &functions);
        *env.auto_memo.borrow_mut() = None;
    });
    super::frame_ptr::check(&env, &functions);
    let summaries = rokc_diagnostic::time("hir handoff check", || {
        *env.auto_memo.borrow_mut() = Some(HashMap::new());
        let s = super::handoff::check(&env, &functions, &input.summaries, lib_pkg.is_some());
        *env.auto_memo.borrow_mut() = None;
        s
    });
    let (any_seeds, any_exports) = super::expr::any_link(&env, lib_pkg.as_deref());
    let any_types = super::expr::any_types(&env);
    if lib_pkg.is_some() || !prebuilt.is_empty() {
        exports.extend(any_exports);
    }
    let Tables {
        defs, sink, names, ..
    } = env.t;
    // Append every closure body / monomorphization lifted to a top-level
    // function during expression lowering.
    functions.extend(sink.extras.into_inner());
    let mut structs = defs.structs.into_inner();
    let mut enums = defs.enums.into_inner();
    super::infer::drop_var_instances(&mut structs, &mut enums);
    for s in &mut static_defs {
        if let Some(c) = folded_statics.get(&s.name) {
            s.init = c.clone();
            continue;
        }
        let init = lazy_init_fn_name(&s.name);
        s.init_fn = functions.iter().find(|f| f.name == init).map(|f| f.id);
    }
    // Per-function facts for MIR / codegen.
    let index: HashMap<FunctionId, usize> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id, i))
        .collect();
    let mut set = |id: &FunctionId, f: &dyn Fn(&mut rokc_ir_common::FnFacts)| {
        if let Some(&i) = index.get(id) {
            f(&mut functions[i].facts);
        }
    };
    for id in &extern_fn_ids {
        let variadic = c_variadic.contains(id);
        set(id, &|f| {
            f.linkage = Linkage::Extern;
            f.abi = Abi::C {
                variadic,
                system: system_fns.contains(id),
            };
        });
    }
    for id in &rok_externs {
        set(id, &|f| f.linkage = Linkage::Extern);
    }
    for id in &no_safepoint {
        set(id, &|f| f.no_safepoint = true);
    }
    for (id, w) in &wasm_imports {
        set(id, &|f| f.wasm_import = Some(w.clone()));
    }
    for (id, n) in &extern_symbols {
        set(id, &|f| f.link_name = Some(n.clone()));
    }
    for id in &prebuilt {
        set(id, &|f| f.linkage = Linkage::Prebuilt);
    }
    for id in &c_defs {
        let system = system_fns.contains(id);
        set(id, &|f| {
            f.abi = Abi::C {
                variadic: false,
                system,
            };
        });
    }
    for (id, sym) in &exports {
        set(id, &|f| f.export_name = Some(sym.clone()));
    }
    if let Some(id) = &main {
        set(id, &|f| f.role = FnRole::Main);
    }
    super::caller::lower(&mut functions, &sink.track_caller.borrow(), sources);
    // A prebuilt package's statics are initialized by its `.a`.
    let lazy_inits = static_defs
        .iter()
        .filter(|s| {
            let module = module_of(&s.name);
            let pkg = package_of(&module);
            !input.prebuilt.iter().any(|p| within(pkg, p))
        })
        .filter_map(|s| s.init_fn);
    let roots = if lib_pkg.is_some() {
        let package = (0..decl_roots.len())
            .filter(|&i| decl_roots[i] && lib_decl[i])
            .map(|i| FunctionId(i as u32));
        let dispatchers = functions
            .iter()
            .filter(|f| f.facts.role == FnRole::AnyDispatcher)
            .map(|f| f.id);
        Some(
            package
                .chain(dispatchers)
                .chain(exports.iter().map(|(id, _)| *id))
                .chain(lazy_inits)
                .collect(),
        )
    } else {
        main.map(|m| {
            std::iter::once(m)
                .chain(exports.iter().map(|(id, _)| *id))
                .chain(lazy_inits)
                .collect()
        })
    };
    // The package's functions ahead of the library's (their ids are the
    // other way round): its first one names the debug info's unit.
    functions.sort_by_key(|f| lib_decl.get(f.id.0 as usize).copied().unwrap_or(false));
    let mut module = HirModule {
        functions,
        structs,
        enums,
        statics: static_defs,
        diagnostics: pre_diags,
        roots,
        unlowered,
        any_seeds,
        any_types,
        any_tags: sink.any_tags.take(),
        type_visibility: names.visibility.clone(),
        reuse,
        summaries,
    };
    module.diagnostics.extend(names.diags.take());
    module.diagnostics.extend(sink.diagnostics.into_inner());
    module
        .diagnostics
        .extend(sink.instance_warns.into_inner().agreed());
    let bare = names.bare_generics.take();
    let generics_of = |full: &str| -> Option<(rokc_ast::Generics, rokc_diagnostic::Span)> {
        let at = *bare.get(full)?;
        defs.generic_structs
            .get(full)
            .map(|d| &d.generics)
            .or_else(|| defs.generic_unions.get(full).map(|d| &d.generics))
            .or_else(|| defs.generic_enums.get(full).map(|d| &d.generics))
            .or_else(|| names.generic_aliases.get(full).map(|d| &d.generics))
            .filter(|g| !g.params.is_empty())
            .map(|g| (g.clone(), at))
    };
    check_unresolved_types(
        &mut module,
        &names.unknown_spans.take(),
        &generics_of,
        sources,
        &names,
    );
    check_static_cycles(&mut module);
    check_value_types(&mut module);
    check_packed(&mut module);
    // One cause can be reached from several passes; report each message
    // once per place.
    let mut seen = std::collections::HashSet::new();
    module
        .diagnostics
        .retain(|d| seen.insert((d.primary.as_ref().map(|l| l.span), d.message.clone())));
    // A library error alongside a package error follows from it: drop it.
    if !check_library && lib_pkg.is_none() {
        let (own, lib) = &decl_files;
        let file_of = |d: &rokc_diagnostic::RokDiagnostic| d.primary.as_ref().map(|l| l.span.file);
        let in_own =
            |d: &rokc_diagnostic::RokDiagnostic| file_of(d).is_some_and(|f| own.contains(&f));
        if module.diagnostics.iter().any(|d| d.is_error() && in_own(d)) {
            module.diagnostics.retain(|d| {
                !d.is_error() || in_own(d) || !file_of(d).is_some_and(|f| lib.contains(&f))
            });
        }
    }
    // A derive that failed its field check leaves its generated impl's
    // errors (located at the attribute) as a cascade: drop them.
    module.diagnostics.retain(|d| {
        d.kind == rokc_diagnostic::ErrorKind::Meta
            || !d
                .primary
                .as_ref()
                .is_some_and(|l| failed_derives.contains(&l.span))
    });
    // A block-local item's capture error (spec 3.9.1) says why its name
    // is not found there.
    let captured: std::collections::HashSet<_> = (module.diagnostics.iter())
        .filter(|d| d.message.contains(" and can't use `"))
        .filter_map(|d| d.primary.as_ref().map(|l| l.span))
        .collect();
    module.diagnostics.retain(|d| {
        !(d.message.starts_with("cannot find ")
            && d.primary
                .as_ref()
                .is_some_and(|l| captured.contains(&l.span)))
    });
    for p in programs {
        show_local_names(p, &mut module.diagnostics);
    }
    module.diagnostics.splice(0..0, early);
    module
}

/// Show block-declared items (spec 3.9.1, 4.10) as written: `f/S` reads
/// `S`, or `f::S` when the file has another `S`. A name not found where
/// such an item has it gets a note on its scope.
fn show_local_names(program: &Program, diags: &mut [rokc_diagnostic::RokDiagnostic]) {
    use rokc_core::local::{leaf, qualified};
    if diags.is_empty() {
        return;
    }
    let names: Vec<&str> = program
        .items
        .iter()
        .filter_map(Item::name)
        .map(leaf_name)
        .collect();
    let mut lifted: Vec<&str> = names.iter().copied().filter(|n| n.contains('/')).collect();
    if lifted.is_empty() {
        return;
    }
    let mut count: HashMap<std::borrow::Cow<'_, str>, usize> = HashMap::new();
    for n in &names {
        *count.entry(leaf(n)).or_default() += 1;
    }
    lifted.sort_by_key(|l| std::cmp::Reverse(l.len()));
    let shown: Vec<(&str, String)> = lifted
        .iter()
        .map(|l| {
            let short = leaf(l);
            let s = if count[&short] > 1 {
                qualified(l)
            } else {
                short.into_owned()
            };
            (*l, s)
        })
        .collect();
    let fix = |s: &mut String| {
        for (l, shown) in &shown {
            if s.contains(l) {
                *s = s.replace(l, shown);
            }
        }
    };
    for d in diags {
        let missing = d
            .message
            .strip_prefix("cannot find ")
            .and_then(|r| r.split('`').nth(1));
        if let Some(l) = missing.and_then(|m| lifted.iter().find(|l| leaf(l) == m)) {
            let outer = qualified(rokc_core::local::outer(l));
            d.notes.push(format!(
                "`{}` is declared inside `{outer}` and visible only in its block; move it to \
                 module level to use it here (spec 3.9.1)",
                leaf(l)
            ));
        }
        fix(&mut d.message);
        if let Some(p) = &mut d.primary {
            fix(&mut p.message);
        }
        d.secondary.iter_mut().for_each(|l| fix(&mut l.message));
        d.notes.iter_mut().for_each(fix);
    }
}

/// Stdlib 0.1.2: `rok::bare` names neither `rok::core` nor `rok::std`, and
/// `rok::core` doesn't name `rok::std`.
fn check_tiers(
    raw_uses: &[(String, Visibility, rokc_ast::UseTree)],
    items: &[Item],
    modules: &[String],
) -> Vec<rokc_diagnostic::RokDiagnostic> {
    struct Paths<'a> {
        tier: &'static str,
        out: &'a mut Vec<rokc_diagnostic::RokDiagnostic>,
    }
    impl Paths<'_> {
        fn check(&mut self, p: &rokc_ast::Path) {
            let [r, t, ..] = p.segments.as_slice() else {
                return;
            };
            let forbidden = match self.tier {
                "rok::bare" => ["core", "std"].contains(&t.name.as_str()),
                _ => t.name == "std",
            };
            if r.name == "rok" && forbidden {
                self.out.push(rokc_diagnostic::RokDiagnostic::error(
                    rokc_diagnostic::ErrorKind::Module,
                    p.span,
                    format!(
                        "`{}` must not depend on `rok::{}` (stdlib spec 0.1.2)",
                        self.tier, t.name
                    ),
                ));
            }
        }
    }
    impl crate::walk::AstVisitor for Paths<'_> {
        fn path(&mut self, p: &rokc_ast::Path) {
            self.check(p);
        }
    }
    let tier = |module: &str| match package_of(module) {
        "rok::bare" => Some("rok::bare"),
        "rok::core" => Some("rok::core"),
        _ => None,
    };
    let mut out = Vec::new();
    for (item, module) in items.iter().zip(modules) {
        if let Some(tier) = tier(module) {
            crate::walk::walk_item(
                &mut Paths {
                    tier,
                    out: &mut out,
                },
                item,
            );
        }
    }
    for (module, _, tree) in raw_uses {
        if let Some(tier) = tier(module) {
            Paths {
                tier,
                out: &mut out,
            }
            .check(&tree.prefix);
        }
    }
    out
}

/// Put each declaration's signature in `fn_table` (aliases included).
fn install_sigs(
    fn_table: &mut HashMap<String, std::sync::Arc<FnSig>>,
    sigs: &HashMap<FunctionId, FnSig>,
) {
    for sig in fn_table.values_mut() {
        if let Some(s) = sigs.get(&sig.id) {
            *sig = std::sync::Arc::new(s.clone());
        }
    }
}

/// Re-resolve fields and payloads left open (a projection) now that impls
/// are known, an instance's under its arguments. Union storage is fixed at
/// registration.
fn resolve_open_members(
    env: &Env<'_>,
    structs: &[(StructId, &AstStructDef)],
    enums: &[(EnumId, &AstEnumDef)],
) {
    let structs: HashMap<usize, &AstStructDef> =
        structs.iter().map(|(id, d)| (id.0 as usize, *d)).collect();
    let enums: HashMap<usize, &AstEnumDef> =
        enums.iter().map(|(id, d)| (id.0 as usize, *d)).collect();
    let open = |t: &HirTy| {
        let mut o = Vec::new();
        collect_unknown_paths(t, &mut o);
        !o.is_empty()
    };
    let struct_open: Vec<(usize, &AstStructDef, Vec<HirTy>)> = env
        .defs
        .structs
        .borrow()
        .iter()
        .enumerate()
        .filter(|(_, d)| d.union_kind.is_none() && d.fields.iter().any(|f| open(&f.ty)))
        .filter_map(|(i, d)| {
            let def = match structs.get(&i) {
                Some(s) if d.generic_args.is_empty() => *s,
                _ => *env.defs.generic_structs.get(enum_base_name(&d.name))?,
            };
            Some((i, def, d.generic_args.clone()))
        })
        .collect();
    for (i, def, args) in struct_open {
        let subst = generic_subst(&def.generics, &args);
        let mut rcx = LowerCtx::new(env, &subst, None, def.span);
        rcx.current_module = module_of(enum_base_name(&def.name));
        let fields = generic_struct_fields(def, &|t| rcx.resolve_ast_ty(t));
        env.defs.structs.borrow_mut()[i].fields = fields;
    }
    let enum_open: Vec<(usize, &AstEnumDef, Vec<HirTy>)> = env
        .defs
        .enums
        .borrow()
        .iter()
        .enumerate()
        .filter(|(_, d)| {
            d.variants
                .iter()
                .any(|v| v.payload_ty.as_ref().is_some_and(open))
        })
        .filter_map(|(i, d)| {
            let def = match enums.get(&i) {
                Some(e) if d.generic_args.is_empty() => *e,
                _ => *env.defs.generic_enums.get(enum_base_name(&d.name))?,
            };
            Some((i, def, d.generic_args.clone()))
        })
        .collect();
    for (i, def, args) in enum_open {
        let subst = generic_subst(&def.generics, &args);
        let mut rcx = LowerCtx::new(env, &subst, None, def.span);
        rcx.current_module = module_of(enum_base_name(&def.name));
        let payloads = generic_enum_variants(def, &|t| rcx.resolve_ast_ty(t));
        let mut all = env.defs.enums.borrow_mut();
        for (v, fresh) in all[i].variants.iter_mut().zip(payloads) {
            v.payload_ty = fresh.payload_ty;
        }
    }
}

pub(super) fn reexport_key(re: &crate::uses::ReExport) -> String {
    if re.module_path.is_empty() {
        re.short_name.clone()
    } else {
        format!("{}::{}", re.module_path, re.short_name)
    }
}

/// Headers of block-lifted items with `use p::*`s (`#block_glob`) name
/// what the globs bring in by path, as named `use`s do (spec 3.9.1).
fn block_glob_renames(
    items: &[Item],
    item_modules: &[String],
    all: &[&Item],
    reexports: &[crate::uses::ReExport],
    uses: &crate::uses::UseTable,
) -> Vec<(usize, rokc_ast::rename::Renames)> {
    let blocks = |item: &Item| -> Vec<Vec<String>> {
        (item.attrs().iter())
            .filter(|a| a.path == [rokc_ast::BLOCK_GLOB_ATTR])
            .map(|a| match &a.body {
                rokc_ast::AttrBody::Args(args) => (args.iter())
                    .filter_map(|x| match x {
                        rokc_ast::AttrArg::Lit(rokc_ast::Literal::Str(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect()
    };
    let mut names = None;
    let mut out = Vec::new();
    for (i, (item, module)) in items.iter().zip(item_modules).enumerate() {
        let blocks = blocks(item);
        if blocks.is_empty() {
            continue;
        }
        let names = names.get_or_insert_with(|| crate::uses::GlobNames::new(all, reexports));
        let paths = names.block_paths(&blocks, module, uses);
        if !paths.is_empty() {
            out.push((
                i,
                rokc_ast::rename::Renames {
                    paths,
                    headers_only: true,
                    ..Default::default()
                },
            ));
        }
    }
    out
}

fn qualify(item: &mut Item, module: &str) {
    if module.is_empty() {
        return;
    }
    let q = |name: &mut String| *name = format!("{module}::{name}");
    match item {
        Item::Fn(f) => q(&mut f.name),
        Item::Struct(s) => q(&mut s.name),
        Item::Enum(e) => q(&mut e.name),
        Item::Const(c) => q(&mut c.name),
        Item::Static(s) => q(&mut s.name),
        Item::TypeAlias(a) => q(&mut a.name),
        Item::Trait(t) => q(&mut t.name),
        Item::Union(u) => q(&mut u.name),
        Item::Mod(m) => q(&mut m.name),
        Item::Impl(i) => {
            for it in &mut i.items {
                match it {
                    rokc_ast::ImplItem::Method(m) => q(&mut m.name),
                    rokc_ast::ImplItem::Const(c) => q(&mut c.name),
                    rokc_ast::ImplItem::AssocType { .. } => {}
                }
            }
            module.clone_into(&mut i.module);
        }
        Item::Use(_) | Item::Extern(_) => {}
    }
}

/// Resolve plain aliases of projections once impls exist, reporting at the
/// body; a broken one is `{unknown}` where used.
fn resolve_projection_aliases(env: &Env<'_>, program: &Program) {
    let names = &env.names;
    let stage: std::collections::HashSet<&str> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::TypeAlias(a) => Some(a.name.as_str()),
            _ => None,
        })
        .collect();
    let mut aliases: Vec<(&String, &Ty)> = names
        .plain_aliases
        .iter()
        .filter(|(full, body)| {
            stage.contains(full.as_str())
                && !names.broken_aliases.borrow().contains(*full)
                && names.names_projection(body, &module_of(full), body.span)
        })
        .collect();
    aliases.sort_by_key(|(full, _)| *full);
    let no_subst = HashMap::new();
    for (full, body) in aliases {
        let count = || names.diags.borrow().len() + env.sink.diagnostics.borrow().len();
        let before = count();
        let mut rcx = LowerCtx::new(env, &no_subst, None, body.span);
        rcx.current_module = module_of(full);
        rcx.resolve_ast_ty(body);
        if count() != before {
            names.broken_aliases.borrow_mut().insert(full.clone());
        }
    }
}

/// `pub use` alias key → the end of its re-export chain (a cycle stops
/// where it closes); the first re-export of a key wins.
fn reexport_aliases(reexports: &[crate::uses::ReExport]) -> HashMap<String, String> {
    let mut direct: HashMap<String, &str> = HashMap::new();
    for re in reexports {
        direct
            .entry(reexport_key(re))
            .or_insert(re.full_path.as_str());
    }
    direct
        .iter()
        .map(|(alias, first)| {
            let mut seen = std::collections::HashSet::from([alias.as_str()]);
            let mut end = *first;
            while let Some(next) = direct.get(end).copied().filter(|_| seen.insert(end)) {
                end = next;
            }
            (alias.clone(), end.to_owned())
        })
        .collect()
}

/// Source-ish rendering of an AST type for diagnostics.
pub(super) fn impl_def_display(ty: &Ty) -> String {
    let list = |ts: &[Ty], sep: &str| {
        ts.iter()
            .map(impl_def_display)
            .collect::<Vec<_>>()
            .join(sep)
    };
    match &ty.kind {
        AstTyKind::Path(p) => {
            let path = p
                .segments
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join("::");
            match p.segments.last().map(|s| s.generics.as_slice()) {
                Some(args) if !args.is_empty() => format!("{path}<{}>", list(args, ", ")),
                _ => path,
            }
        }
        AstTyKind::Tuple(ts) => format!("({})", list(ts, ", ")),
        AstTyKind::Sum(ts) => list(ts, " | "),
        AstTyKind::Nest(x) => impl_def_display(x),
        AstTyKind::Binding(n, t) => format!("{n} = {}", impl_def_display(t)),
        AstTyKind::QPath(q) => format!(
            "<{} as {}>::{}",
            impl_def_display(&q.self_ty),
            impl_def_display(&Ty {
                kind: AstTyKind::Path(q.trait_path.clone()),
                span: ty.span,
            }),
            q.item
        ),
        AstTyKind::None => "none".to_owned(),
        AstTyKind::Array(e, rokc_ast::ConstExpr::Literal(rokc_ast::Literal::Int(n))) => {
            format!("[{}; {n}]", impl_def_display(e))
        }
        AstTyKind::Array(e, rokc_ast::ConstExpr::Path(p)) if p.segments.len() == 1 => {
            format!("[{}; {}]", impl_def_display(e), p.segments[0].name)
        }
        AstTyKind::Array(e, rokc_ast::ConstExpr::Expr(x)) => match &x.kind {
            ExprKind::Path(p) if p.segments.len() == 1 => {
                format!("[{}; {}]", impl_def_display(e), p.segments[0].name)
            }
            _ => format!("[{}; _]", impl_def_display(e)),
        },
        _ => rokc_ast::print::print_ty(ty),
    }
}

/// struct names; generic instantiations look up their base name).
pub(super) fn collect_field_visibility(
    program: &Program,
) -> HashMap<String, HashMap<String, Visibility>> {
    let mut out: HashMap<String, HashMap<String, Visibility>> = HashMap::new();
    for item in &program.items {
        match item {
            Item::Struct(s) => {
                let vis: HashMap<String, Visibility> = match &s.kind {
                    AstStructKind::Named(fs) => {
                        fs.iter().map(|f| (f.name.clone(), f.vis)).collect()
                    }
                    // Positional fields are named "0", "1", ... as in `HirField`.
                    AstStructKind::Tuple(fs) => fs
                        .iter()
                        .enumerate()
                        .map(|(i, f)| (i.to_string(), f.vis))
                        .collect(),
                    AstStructKind::Unit => continue,
                };
                out.entry(s.name.clone()).or_insert(vis);
            }
            Item::Union(u) => {
                out.entry(u.name.clone())
                    .or_insert_with(|| u.fields.iter().map(|f| (f.name.clone(), f.vis)).collect());
            }
            _ => {}
        }
    }
    out
}

/// What rok writes for `Option<T>`: `T | none`, `T` as written at
/// `span`.
fn habit_type_help(
    name: &str,
    span: rokc_diagnostic::Span,
    sources: &rokc_diagnostic::SourceMap,
) -> Option<String> {
    let (_, text) = sources.file(span.file)?;
    let rest = text.get(span.start as usize..)?.strip_prefix(name)?;
    let args = rest.strip_prefix('<').and_then(|r| {
        let mut depth = 1;
        let end = r.find(|c| {
            depth += match c {
                '<' => 1,
                '>' => -1,
                _ => 0,
            };
            depth == 0
        })?;
        Some(r[..end].trim())
    });
    (name == "Option").then(|| {
        format!(
            "rok has no `Option`: an optional is `T | none`; write `{} | none`",
            args.unwrap_or("T")
        )
    })
}

/// Safety net: nothing leaves lowering with an unresolved type unless an
/// error was reported. A leftover `HirTy::Path` is an unknown name; a
/// leftover `Unknown` / `UnresolvedPath` is a silent fall-through that MIR
/// would lower as unit / `any`.
pub(super) fn check_unresolved_types(
    module: &mut HirModule,
    spans: &HashMap<String, rokc_diagnostic::Span>,
    generics_of: &dyn Fn(&str) -> Option<(rokc_ast::Generics, rokc_diagnostic::Span)>,
    sources: &rokc_diagnostic::SourceMap,
    names: &TypeNames,
) {
    // Report each unknown name at the first item naming it, not at the
    // thread-local current span (whatever pass ran last).
    // (name, where, mentioning item's module)
    let mut unknown_types: Vec<(String, rokc_diagnostic::Span, String)> = Vec::new();
    let mut note = |ty: &HirTy, span: rokc_diagnostic::Span, item: &str| {
        let mut names = Vec::new();
        collect_unknown_paths(ty, &mut names);
        for n in names {
            if !unknown_types.iter().any(|(t, ..)| *t == n) {
                unknown_types.push((n, span, owner_module(item)));
            }
        }
    };
    for s in &module.structs {
        for f in &s.fields {
            note(&f.ty, s.span, &s.name);
        }
    }
    for e in &module.enums {
        for v in &e.variants {
            if let Some(t) = &v.payload_ty {
                note(t, e.span, &e.name);
            }
        }
    }
    for st in &module.statics {
        note(&st.ty, st.span, &st.name);
    }
    for f in &module.functions {
        note(&f.ret_ty, f.span, &f.name);
        for p in &f.params {
            note(&p.ty, f.span, &f.name);
        }
        for l in &f.locals {
            note(&l.ty, f.span, &f.name);
        }
    }
    unknown_types.sort_by(|a, b| a.0.cmp(&b.0));
    // Type keys a user may name: the package's own, and the library's
    // `pub` ones.
    let named: Vec<&str> = module
        .structs
        .iter()
        .map(|s| s.name.as_str())
        .chain(module.enums.iter().map(|e| e.name.as_str()))
        .map(|n| n.split("__").next().unwrap_or(n))
        .filter(|n| {
            !n.starts_with("rok::") || module.type_visibility.get(*n) == Some(&Visibility::Pub)
        })
        .collect();
    let known: Vec<&str> = named
        .iter()
        .map(|n| leaf_name(n))
        .chain(super::PRIM_NAMES.iter().copied())
        .collect();
    let mut diags = Vec::new();
    let mut reported = Vec::new();
    for (t, span, at) in unknown_types {
        let span = spans.get(&t).copied().unwrap_or(span);
        // A generic type written bare: it exists, it lacks its arguments.
        if let Some((g, at)) = generics_of(&t) {
            let leaf = leaf_name(&t);
            let (msg, help) = super::mono::bare_generic(leaf, &g);
            diags.push(
                rokc_diagnostic::RokDiagnostic::error(rokc_diagnostic::ErrorKind::Resolve, at, msg)
                    .with_help(help),
            );
            reported.push(t);
            continue;
        }
        let elsewhere = named
            .iter()
            .filter(|k| !k.starts_with("rok::") && leaf_name(k) == t && module_of(k) != at)
            .min();
        let d = if let (Some(k), false) = (elsewhere, at.is_empty()) {
            let def = module_of(k);
            let depth = |m: &str| m.split("::").filter(|s| !s.is_empty()).count();
            let up = depth(&at).saturating_sub(depth(&def));
            let (where_, path) = if def.is_empty() || at.starts_with(&format!("{def}::")) {
                let whose = if up == 1 {
                    "parent".to_owned()
                } else {
                    format!("{up} levels up")
                };
                (
                    format!("the {whose} module"),
                    format!("{}{t}", "sup::".repeat(up)),
                )
            } else {
                (format!("module `{def}`"), format!("pkg::{k}"))
            };
            rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Resolve,
                span,
                format!(
                    "cannot find type `{t}` in module `{at}`: it is defined in {where_}, \
                     and a module sees only its own items and what it imports"
                ),
            )
            .with_help(format!("add `use {path};` inside `mod {}`", leaf_name(&at)))
        } else {
            let d = rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Resolve,
                span,
                format!("cannot find type `{t}` in this scope"),
            );
            let public = named
                .iter()
                .copied()
                .filter(|n| module.type_visibility.get(*n) == Some(&Visibility::Pub));
            if let Some(h) = habit_type_help(&t, span, sources) {
                d.with_help(h)
            } else if let Some(h) = resolve::own_library_import(&t, public) {
                d.with_help(h)
            } else if let Some(h) = super::join_help(
                (!t.contains("::"))
                    .then(|| names.import_help(&at, names.type_leaf(&t), |k| names.type_vis(k)))
                    .flatten(),
                rokc_diagnostic::suggest(leaf_name(&t), known.iter().copied())
                    .map(|s| format!("did you mean `{s}`?")),
            ) {
                d.with_help(h)
            } else {
                d
            }
        };
        diags.push(d);
        reported.push(t);
    }
    // A type error naming an unknown type follows from it: report the
    // name first and alone. A block item's capture error says why the
    // name is unknown; it stays.
    module.diagnostics.retain(|d| {
        d.kind != rokc_diagnostic::ErrorKind::Type
            || d.message.contains(" and can't use `")
            || !reported
                .iter()
                .filter(|t| t.starts_with(char::is_alphabetic))
                .any(|t| names_type(&d.message, t))
    });
    module.diagnostics.splice(0..0, diags);
    if module
        .diagnostics
        .iter()
        .any(rokc_diagnostic::RokDiagnostic::is_error)
    {
        return;
    }
    let bad = module.functions.iter().find(|f| {
        f.params.iter().any(|p| p.ty == HirTy::Unknown)
            || f.locals.iter().any(|l| l.ty == HirTy::Unknown)
            || f.body.as_ref().is_some_and(hir_block_has_unknown)
    });
    if let Some(f) = bad {
        module
            .diagnostics
            .push(rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Type,
                f.span,
                format!(
                    "internal compiler error: an expression in `{}` has no resolved type \
                 (lowering fell through without a diagnostic) - please report",
                    f.name
                ),
            ));
    }
}

/// Whether a quoted type in `msg` names the path `t`: `ConstPtr` in
/// "no method `is_null` found for type `ConstPtr`" or "`[ConstPtr; 2]`".
fn names_type(msg: &str, t: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == ':';
    msg.split('`').skip(1).step_by(2).any(|quoted| {
        quoted.match_indices(t).any(|(i, _)| {
            let before = quoted[..i].chars().next_back();
            let after = quoted[i + t.len()..].chars().next();
            !before.is_some_and(word) && !after.is_some_and(word)
        })
    })
}

/// Most scalar slots one value type may flatten to: nesting a generic
/// that doubles its payload (`W<W<..>>`) grows exponentially.
const MAX_VALUE_LEAVES: u64 = 1 << 16;
/// Largest value type, in (estimated) bytes.
const MAX_VALUE_BYTES: u64 = 1 << 32;

/// A struct / enum containing itself by value has no finite size; one past
/// the layout caps is refused before MIR expands it.
/// Statics whose initializers name each other directly form a cycle no
/// order can initialize (spec 5.3.6). A cycle through a function call
/// stays the run-time panic.
fn check_static_cycles(module: &mut HirModule) {
    use std::collections::HashMap;
    let bodies: HashMap<FunctionId, &crate::HirFunction> =
        module.functions.iter().map(|f| (f.id, f)).collect();
    let index: HashMap<&str, usize> = (module.statics.iter().enumerate())
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();
    let reads: Vec<Vec<usize>> = (module.statics.iter())
        .map(|s| {
            let mut out = Vec::new();
            let body = s.init_fn.and_then(|id| bodies.get(&id)?.body.as_ref());
            if let Some(b) = body {
                crate::each_block(b, &mut |e| {
                    if let HirExprKind::StaticRef(n) = &e.kind {
                        if let Some(&j) = index.get(n.as_str()) {
                            if !out.contains(&j) {
                                out.push(j);
                            }
                        }
                    }
                });
            }
            out
        })
        .collect();
    // 0 unvisited, 1 on the path, 2 done.
    let mut state = vec![0u8; reads.len()];
    let mut diags = Vec::new();
    for start in 0..reads.len() {
        if state[start] != 0 {
            continue;
        }
        let mut path = vec![start];
        let mut next = vec![0usize];
        state[start] = 1;
        while let Some(&at) = path.last() {
            let k = next.last_mut().expect("parallel to path");
            let Some(&to) = reads[at].get(*k) else {
                state[at] = 2;
                path.pop();
                next.pop();
                continue;
            };
            *k += 1;
            if state[to] == 0 {
                state[to] = 1;
                path.push(to);
                next.push(0);
            } else if state[to] == 1 {
                let from = path.iter().position(|&p| p == to).unwrap_or(0);
                let cycle: Vec<&str> = path[from..]
                    .iter()
                    .chain(std::iter::once(&to))
                    .map(|&i| leaf_name(&module.statics[i].name))
                    .collect();
                diags.push(
                    rokc_diagnostic::RokDiagnostic::error(
                        rokc_diagnostic::ErrorKind::Type,
                        module.statics[to].span,
                        format!(
                            "static `{}` is initialized from itself: `{}`",
                            cycle[0],
                            cycle.join(" -> ")
                        ),
                    )
                    .with_help(format!(
                        "give `{}` a value that does not read `{}`, such as a literal or a \
                         `const`",
                        cycle[0], cycle[1]
                    )),
                );
            }
        }
    }
    module.diagnostics.extend(diags);
}

fn check_value_types(module: &mut HirModule) {
    use std::collections::{HashMap, HashSet};
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    enum Def {
        S(usize),
        E(usize),
    }
    struct Defs<'m> {
        m: &'m HirModule,
        structs: HashMap<StructId, usize>,
        enums: HashMap<EnumId, usize>,
    }
    impl Defs<'_> {
        fn of(&self, t: &HirTy) -> Option<Def> {
            match t {
                HirTy::Struct(id) => self.structs.get(id).copied().map(Def::S),
                HirTy::Enum(id) => self.enums.get(id).copied().map(Def::E),
                _ => None,
            }
        }
        fn children(&self, d: Def) -> Vec<&HirTy> {
            match d {
                Def::S(i) => self.m.structs[i].fields.iter().map(|f| &f.ty).collect(),
                Def::E(i) => self.m.enums[i]
                    .variants
                    .iter()
                    .filter_map(|v| v.payload_ty.as_ref())
                    .collect(),
            }
        }
        // Defs held by value, through tuples / sums / arrays.
        fn inline(&self, t: &HirTy, out: &mut Vec<Def>) {
            match t {
                HirTy::Tuple(ps) | HirTy::Sum(ps) => ps.iter().for_each(|p| self.inline(p, out)),
                HirTy::Array(e, _) => self.inline(e, out),
                t => out.extend(self.of(t)),
            }
        }
        fn edges(&self, d: Def) -> Vec<Def> {
            let mut out = Vec::new();
            for t in self.children(d) {
                self.inline(t, &mut out);
            }
            out
        }
        // Source name (`W<..>` for an instance), span, "a struct" / "an enum".
        fn name_span(&self, d: Def) -> (String, rokc_diagnostic::Span, &'static str) {
            let (name, generic, span, kind) = match d {
                Def::S(i) => {
                    let s = &self.m.structs[i];
                    (&s.name, !s.generic_args.is_empty(), s.span, "a struct")
                }
                Def::E(i) => {
                    let e = &self.m.enums[i];
                    (&e.name, !e.generic_args.is_empty(), e.span, "an enum")
                }
            };
            let base = name.split("__").next().unwrap_or(name);
            let name = if generic {
                format!("{base}<..>")
            } else {
                base.to_owned()
            };
            (name, span, kind)
        }
        // (scalar slots, estimated bytes); saturating.
        fn size(&self, t: &HirTy, memo: &mut HashMap<Def, (u64, u64)>) -> (u64, u64) {
            let add =
                |a: (u64, u64), b: (u64, u64)| (a.0.saturating_add(b.0), a.1.saturating_add(b.1));
            match t {
                HirTy::Tuple(ps) | HirTy::Sum(ps) => ps
                    .iter()
                    .fold((1, 8), |acc, p| add(acc, self.size(p, memo))),
                HirTy::Array(elem, len) => {
                    let (slots, bytes) = self.size(elem, memo);
                    (slots, bytes.saturating_mul(u64::from(*len)))
                }
                t => match self.of(t) {
                    Some(d) => {
                        if let Some(v) = memo.get(&d) {
                            return *v;
                        }
                        // Provisional entry: cycles were reported already.
                        memo.insert(d, (1, 8));
                        let v = self
                            .children(d)
                            .iter()
                            .fold((1, 8), |acc, c| add(acc, self.size(c, memo)));
                        memo.insert(d, v);
                        v
                    }
                    None => (1, 8),
                },
            }
        }
    }
    let defs = Defs {
        m: module,
        structs: module
            .structs
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect(),
        enums: module
            .enums
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id, i))
            .collect(),
    };
    let all: Vec<Def> = (0..module.structs.len())
        .map(Def::S)
        .chain((0..module.enums.len()).map(Def::E))
        .collect();
    let mut errs = Vec::new();
    for &d in &all {
        let mut stack = defs.edges(d);
        let mut seen = HashSet::new();
        while let Some(x) = stack.pop() {
            if x == d {
                let (name, span, kind) = defs.name_span(d);
                errs.push((
                    span,
                    format!(
                        "recursive type `{name}` has infinite size ({kind} can't contain \
                         itself by value; hold it through a reference type such as `List`)"
                    ),
                ));
                break;
            }
            if seen.insert(x) {
                stack.extend(defs.edges(x));
            }
        }
    }
    if errs.is_empty() {
        let mut memo = HashMap::new();
        for &d in &all {
            let t = match d {
                Def::S(i) => HirTy::Struct(module.structs[i].id),
                Def::E(i) => HirTy::Enum(module.enums[i].id),
            };
            let (leaves, bytes) = defs.size(&t, &mut memo);
            if leaves > MAX_VALUE_LEAVES || bytes > MAX_VALUE_BYTES {
                let (name, span, _) = defs.name_span(d);
                errs.push((
                    span,
                    format!(
                        "value type `{name}` is too large ({leaves} scalar slots, ~{bytes} \
                         bytes; the limit is {MAX_VALUE_LEAVES} slots / {MAX_VALUE_BYTES} bytes)"
                    ),
                ));
                break;
            }
        }
    }
    for (span, msg) in errs {
        module
            .diagnostics
            .push(rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Type,
                span,
                msg,
            ));
    }
}

/// Spec 6.2.2: a `#[repr(packed)]` struct is for C data; its fields may
/// sit unaligned, where the collector cannot read a reference.
fn check_packed(module: &mut HirModule) {
    fn gc(m: &HirModule, t: &HirTy, seen: &mut Vec<HirTy>) -> bool {
        if seen.contains(t) {
            return false;
        }
        seen.push(t.clone());
        let mut rec = |t: &HirTy| gc(m, t, seen);
        match t {
            HirTy::Prim(PrimTy::Str)
            | HirTy::GcBuf(_)
            | HirTy::Any
            | HirTy::AnyTrait(_)
            | HirTy::Closure { .. } => true,
            HirTy::Fn(..) => !t.is_c_fn(),
            HirTy::Tuple(ts) | HirTy::Sum(ts) => ts.iter().any(rec),
            HirTy::Array(e, n) => *n > 0 && rec(e),
            HirTy::Throws { ok, err } => rec(ok) || rec(err),
            HirTy::Struct(id) => {
                let s = &m.structs[id.0 as usize];
                match &s.union_kind {
                    Some(u) if matches!(u.storage, crate::UnionStorage::ByteArray) => false,
                    _ => s.fields.iter().any(|f| rec(&f.ty)),
                }
            }
            HirTy::Enum(id) => m.enums[id.0 as usize]
                .variants
                .iter()
                .filter_map(|v| v.payload_ty.as_ref())
                .any(rec),
            _ => false,
        }
    }
    let bad: Vec<(rokc_diagnostic::Span, String)> = module
        .structs
        .iter()
        .filter(|s| s.repr.packed && s.fields.iter().any(|f| gc(module, &f.ty, &mut Vec::new())))
        .map(|s| {
            (
                s.span,
                s.name.split("__").next().unwrap_or(&s.name).to_owned(),
            )
        })
        .collect();
    for (span, name) in bad {
        module
            .diagnostics
            .push(rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Type,
                span,
                format!(
                    "`#[repr(packed)]` struct `{}` holds a GC reference (`str`, a collection, \
                     a closure ..); a packed struct is for C data (spec 6.2.2)",
                    leaf_name(&name)
                ),
            ));
    }
}

pub(super) fn collect_unknown_paths(ty: &HirTy, out: &mut Vec<String>) {
    match ty {
        HirTy::Path(p) => {
            out.push(p.join("::"));
        }
        // Open until inference or a binding fixes them (an inference
        // variable shows as `_`).
        HirTy::Var(_) => out.push("_".to_owned()),
        HirTy::OpenConst(n) => out.push(n.clone()),
        HirTy::NoThrow => out.push("#nothrow".to_owned()),
        HirTy::Sum(parts) | HirTy::Tuple(parts) => {
            for p in parts {
                collect_unknown_paths(p, out);
            }
        }
        HirTy::Array(e, _) | HirTy::GcBuf(e) => collect_unknown_paths(e, out),
        HirTy::Fn(a, r, _)
        | HirTy::Closure {
            args: a, ret: r, ..
        } => {
            for p in a {
                collect_unknown_paths(p, out);
            }
            collect_unknown_paths(r, out);
        }
        HirTy::Throws { ok, err } => {
            collect_unknown_paths(ok, out);
            collect_unknown_paths(err, out);
        }
        _ => {}
    }
}

pub(super) fn hir_block_has_unknown(b: &HirBlock) -> bool {
    b.stmts.iter().any(|s| match s {
        HirStmt::Let { init: e, .. }
        | HirStmt::Assign { value: e, .. }
        | HirStmt::StaticAssign { value: e, .. }
        | HirStmt::Expr(e)
        | HirStmt::Panic(e)
        | HirStmt::Return(Some(e))
        | HirStmt::Break { value: Some(e), .. } => hir_expr_has_unknown(e),
        HirStmt::ArrayAssign { index, value, .. } => {
            hir_expr_has_unknown(index) || hir_expr_has_unknown(value)
        }
        HirStmt::Return(None) | HirStmt::Break { value: None, .. } | HirStmt::Continue { .. } => {
            false
        }
    }) || b.trailing.as_ref().is_some_and(hir_expr_has_unknown)
}

pub(super) fn hir_expr_has_unknown(e: &HirExpr) -> bool {
    use HirExprKind as K;
    if e.ty == HirTy::Unknown {
        return true;
    }
    let any = |es: &[HirExpr]| es.iter().any(hir_expr_has_unknown);
    match &e.kind {
        K::UnresolvedPath(_) => true,
        K::Const(_) | K::Local(_) | K::StaticRef(_) | K::FnRef(_) | K::SizeOf { .. } => false,
        K::Block(b) => hir_block_has_unknown(b),
        K::PrimBinOp { lhs, rhs, .. } => hir_expr_has_unknown(lhs) || hir_expr_has_unknown(rhs),
        K::PrimUnaryOp { operand, .. } | K::Cast { operand, .. } => hir_expr_has_unknown(operand),
        K::Call { args, .. } | K::EnumVariantInit { args, .. } | K::CoroNew { args, .. } => {
            any(args)
        }
        K::CallIndirect { callee, args, .. } => hir_expr_has_unknown(callee) || any(args),
        K::ClosureValue { env, .. } => hir_expr_has_unknown(env),
        K::CallClosure { closure, args, .. } => hir_expr_has_unknown(closure) || any(args),
        K::If {
            cond,
            then_block,
            else_branch,
        } => {
            hir_expr_has_unknown(cond)
                || hir_block_has_unknown(then_block)
                || else_branch.as_deref().is_some_and(hir_expr_has_unknown)
        }
        K::While { cond, body, .. } => hir_expr_has_unknown(cond) || hir_block_has_unknown(body),
        K::Loop { body, .. } => hir_block_has_unknown(body),
        K::Match { scrutinee, arms } => {
            hir_expr_has_unknown(scrutinee)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(hir_expr_has_unknown)
                        || hir_expr_has_unknown(&a.body)
                })
        }
        K::StructInit { fields, .. } | K::Tuple { fields } => any(fields),
        K::FieldRead { target, .. }
        | K::TupleField { target, .. }
        | K::UnionRead { target, .. }
        | K::ArrayRepeat { value: target, .. } => hir_expr_has_unknown(target),
        K::WithField { target, value, .. } => {
            hir_expr_has_unknown(target) || hir_expr_has_unknown(value)
        }
        K::SumWrap { value, .. } => value.as_deref().is_some_and(hir_expr_has_unknown),
        K::ArrayInit { elements, .. } => any(elements),
        K::ArrayIndex { array, index, .. } => {
            hir_expr_has_unknown(array) || hir_expr_has_unknown(index)
        }
        K::UnionWrite { value, .. } | K::Suspend { value, .. } => hir_expr_has_unknown(value),
        K::BufNew { len, .. } | K::RawRead { addr: len, .. } | K::AddrOf { place: len } => {
            hir_expr_has_unknown(len)
        }
        K::RawWrite { addr, value, .. } => {
            hir_expr_has_unknown(addr) || hir_expr_has_unknown(value)
        }
        K::BufGet { buf, index, .. } => hir_expr_has_unknown(buf) || hir_expr_has_unknown(index),
        K::BufSet {
            buf, index, value, ..
        } => {
            hir_expr_has_unknown(buf) || hir_expr_has_unknown(index) || hir_expr_has_unknown(value)
        }
        K::BufAtomic {
            buf, index, args, ..
        } => hir_expr_has_unknown(buf) || hir_expr_has_unknown(index) || any(args),
        K::MlirOp { args, regions, .. } => {
            any(args) || regions.iter().any(|(_, b)| hir_expr_has_unknown(b))
        }
    }
}

pub(super) fn lower_enum_def(
    e: &AstEnumDef,
    id: EnumId,
    defs: &TypeDefs<'_>,
    sc: Scope<'_>,
) -> HirEnumDef {
    let variants = e
        .variants
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let tag = i128::try_from(i).expect("variant index overflow");
            match &v.kind {
                // A value variant's tag is filled in once consts are known
                // (`assign_discriminants`).
                AstEnumVariantKind::Unit | AstEnumVariantKind::Value(_) => HirEnumVariant {
                    name: v.name.clone(),
                    tag,
                    payload_ty: None,
                    named_fields: Vec::new(),
                    arity: 0,
                },
                // Single-element tuple - store the payload type directly.
                AstEnumVariantKind::Tuple(tys) if tys.len() == 1 => HirEnumVariant {
                    name: v.name.clone(),
                    tag,
                    payload_ty: Some(resolve_ty_with_structs(&tys[0], defs, None, sc)),
                    named_fields: Vec::new(),
                    arity: 1,
                },
                // Multi-element tuple - pack into a tuple payload type.
                AstEnumVariantKind::Tuple(tys) => HirEnumVariant {
                    name: v.name.clone(),
                    tag,
                    payload_ty: Some(HirTy::Tuple(
                        tys.iter()
                            .map(|t| resolve_ty_with_structs(t, defs, None, sc))
                            .collect(),
                    )),
                    named_fields: Vec::new(),
                    arity: tys.len(),
                },
                // `Move { x: i32; y: i32; }` (spec 2.4): a tuple payload in
                // declaration order, with field names for init / patterns.
                AstEnumVariantKind::Named(fields) => HirEnumVariant {
                    name: v.name.clone(),
                    tag,
                    payload_ty: Some(HirTy::Tuple(
                        fields
                            .iter()
                            .map(|f| resolve_ty_with_structs(&f.ty, defs, None, sc))
                            .collect(),
                    )),
                    named_fields: fields.iter().map(|f| f.name.clone()).collect(),
                    arity: fields.len(),
                },
            }
        })
        .collect();
    HirEnumDef {
        id,
        repr: repr_of(&e.attrs).unwrap_or_default(),
        def_id: None,
        name: e.name.clone(),
        variants,
        span: e.span,
        generic_args: Vec::new(),
    }
}

/// `#[niche(..)]` arguments (spec 2.7.4): integers, `MIN` / `MAX`, `NAN` /
/// `INFINITY` / `NEG_INFINITY`, and ranges. Empty without the attribute;
/// `Err` names the first unreadable argument.
pub(super) fn niche_args(attrs: &[rokc_ast::Attribute]) -> Result<Vec<NicheArg>, String> {
    use rokc_ast::{AttrArg, AttrBody, Literal};
    let Some(attr) = attrs.iter().find(|a| a.name() == "niche") else {
        return Ok(Vec::new());
    };
    let AttrBody::Args(args) = &attr.body else {
        return Err("`#[niche]` takes arguments: `#[niche(0)]`".to_owned());
    };
    let val = |a: &AttrArg| -> Option<NicheVal> {
        let name = match a {
            AttrArg::Lit(Literal::Int(text)) => {
                let (body, neg) = text
                    .strip_prefix('-')
                    .map_or((text.as_str(), false), |b| (b, true));
                let (v, _) = super::expr::parse_int_literal(body)?;
                return Some(NicheVal::Int(if neg { -v } else { v }));
            }
            AttrArg::Word(w) => w.as_str(),
            AttrArg::Nested { path, body } if matches!(**body, AttrBody::Empty) => path.last()?,
            _ => return None,
        };
        Some(match name {
            "NAN" => NicheVal::Nan,
            "INFINITY" => NicheVal::Inf,
            "NEG_INFINITY" => NicheVal::NegInf,
            "MIN" => NicheVal::Min,
            "MAX" => NicheVal::Max,
            _ => {
                let (ty, end) = name.rsplit_once('_')?;
                let bits: u32 = match ty {
                    "ISIZE" | "USIZE" => 64,
                    _ => ty.get(1..)?.parse().ok()?,
                };
                let signed = match ty.as_bytes().first()? {
                    b'I' => true,
                    b'U' => false,
                    _ => return None,
                };
                if !matches!(bits, 8 | 16 | 32 | 64) {
                    return None;
                }
                NicheVal::Int(match (signed, end) {
                    (true, "MIN") => -(1i128 << (bits - 1)),
                    (true, "MAX") => (1i128 << (bits - 1)) - 1,
                    (false, "MIN") => 0,
                    (false, "MAX") => (1i128 << bits) - 1,
                    _ => return None,
                })
            }
        })
    };
    args.iter()
        .enumerate()
        .map(|(i, a)| {
            let bad = || {
                format!(
                    "`#[niche]` argument {}: expected an integer, `MIN` / `MAX`, `NAN` / \
                     `INFINITY` / `NEG_INFINITY`, or a range of them",
                    i + 1
                )
            };
            match a {
                AttrArg::Range { lo, hi, inclusive } => {
                    let lo = lo
                        .as_deref()
                        .map_or(Some(NicheVal::Min), val)
                        .ok_or_else(bad)?;
                    let hi = match (hi.as_deref().map(val), inclusive) {
                        (None, _) => NicheVal::Max,
                        (Some(Some(v)), true) => v,
                        (Some(Some(NicheVal::Int(v))), false) => NicheVal::Int(v - 1),
                        _ => return Err(bad()),
                    };
                    Ok(NicheArg::Range(lo, hi))
                }
                _ => val(a).map(NicheArg::Val).ok_or_else(bad),
            }
        })
        .collect()
}

/// `#[repr(..)]` arguments (spec 6.2.2), unchecked against the item.
pub(super) fn repr_of(attrs: &[rokc_ast::Attribute]) -> Result<crate::Repr, String> {
    use rokc_ast::{AttrArg, AttrBody, Literal};
    let mut r = crate::Repr::default();
    for attr in attrs.iter().filter(|a| a.name() == "repr") {
        let AttrBody::Args(args) = &attr.body else {
            return Err("`#[repr]` takes arguments: `#[repr(C)]`".to_owned());
        };
        for a in args {
            match a {
                AttrArg::Word(w) => match w.as_str() {
                    "C" => r.c = true,
                    "transparent" => r.transparent = true,
                    "packed" => r.packed = true,
                    w => {
                        r.int = Some(match w {
                            "u8" => PrimTy::U8,
                            "i8" => PrimTy::I8,
                            "u16" => PrimTy::U16,
                            "i16" => PrimTy::I16,
                            "u32" => PrimTy::U32,
                            "i32" => PrimTy::I32,
                            "u64" => PrimTy::U64,
                            "i64" => PrimTy::I64,
                            _ => return Err(format!("unknown `#[repr]` argument `{w}`")),
                        });
                    }
                },
                AttrArg::Nested { path, body } if path.as_slice() == ["align"] => {
                    let n = match &**body {
                        AttrBody::Args(v) => match v.as_slice() {
                            [AttrArg::Lit(Literal::Int(t))] => super::expr::parse_int_literal(t)
                                .and_then(|(v, _)| u32::try_from(v).ok()),
                            _ => None,
                        },
                        _ => None,
                    };
                    match n {
                        Some(n) if n.is_power_of_two() && n <= 1 << 29 => r.align = n,
                        _ => {
                            return Err("`align(N)` takes a power of two up to 2^29".to_owned());
                        }
                    }
                }
                _ => return Err("unknown `#[repr]` argument".to_owned()),
            }
        }
    }
    Ok(r)
}

/// `#[repr(..)]` arguments and placement (spec 6.2.2).
fn check_repr_attrs(program: &rokc_ast::Program, diags: &mut Vec<rokc_diagnostic::RokDiagnostic>) {
    for item in &program.items {
        let attrs = match item {
            Item::Struct(s) => &s.attrs,
            Item::Enum(e) => &e.attrs,
            Item::Union(u) => &u.attrs,
            _ => continue,
        };
        let Some(attr) = attrs.iter().find(|a| a.name() == "repr") else {
            continue;
        };
        let msg = match (repr_of(attrs), item) {
            (Err(e), _) => Some(e),
            (Ok(r), Item::Struct(s)) => {
                let n = match &s.kind {
                    AstStructKind::Named(fs) => fs.len(),
                    AstStructKind::Tuple(fs) => fs.len(),
                    AstStructKind::Unit => 0,
                };
                if r.int.is_some() {
                    Some("an integer `#[repr]` goes on an enum, not a struct".to_owned())
                } else if r.packed && r.align != 0 {
                    Some("a struct cannot be both `packed` and `align(N)`".to_owned())
                } else if r.transparent && (r.c || r.packed || r.align != 0) {
                    Some("`#[repr(transparent)]` takes no other argument".to_owned())
                } else if r.transparent && n != 1 {
                    Some(format!(
                        "`#[repr(transparent)]` needs exactly one field, not {n}"
                    ))
                } else {
                    None
                }
            }
            (Ok(r), Item::Enum(e)) => {
                let data = e.variants.iter().any(|v| {
                    !matches!(
                        v.kind,
                        AstEnumVariantKind::Unit | AstEnumVariantKind::Value(_)
                    )
                });
                if r.packed || r.transparent || r.align != 0 {
                    Some("an enum takes only `C` or an integer `#[repr]`".to_owned())
                } else if data
                    && r.int
                        .is_some_and(|p| !matches!(p, PrimTy::I32 | PrimTy::U32))
                {
                    Some(
                        "a data-carrying enum's tag is 32 bits: only `#[repr(i32)]` / \
                         `#[repr(u32)]`"
                            .to_owned(),
                    )
                } else {
                    None
                }
            }
            (Ok(r), _) => (r.packed || r.transparent || r.align != 0 || r.int.is_some())
                .then(|| "a union takes only `#[repr(C)]`".to_owned()),
        };
        if let Some(msg) = msg {
            diags.push(rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Type,
                attr.span,
                msg,
            ));
        }
    }
}

/// Value variants (spec 2.4.2): all or none; each value a const integer
/// that fits the tag type and is unique.
fn assign_discriminants(env: &Env<'_>, decls: &[(EnumId, &AstEnumDef)]) {
    let no_subst = HashMap::new();
    for &(id, decl) in decls {
        let i = id.0 as usize;
        let values = decl
            .variants
            .iter()
            .filter(|v| matches!(v.kind, AstEnumVariantKind::Value(_)))
            .count();
        if values == 0 {
            continue;
        }
        let mut ctx = LowerCtx::new(env, &no_subst, None, decl.span);
        ctx.current_module = module_of(&decl.name);
        let err = |msg: String| {
            ctx.emit_error(rokc_diagnostic::ErrorKind::Type, msg);
        };
        if values != decl.variants.len() {
            err(format!(
                "enum `{}` mixes value variants (`V = n`) with other variants; \
                 a value enum has only value variants",
                decl.name
            ));
            continue;
        }
        let tag_ty = ctx.enum_def(id).repr.int.unwrap_or(PrimTy::I32);
        let mut seen: HashMap<i128, &str> = HashMap::new();
        let mut tags = Vec::new();
        for v in &decl.variants {
            let AstEnumVariantKind::Value(e) = &v.kind else {
                continue;
            };
            let c = rokc_ast::ConstExpr::Expr(Box::new(e.clone()));
            let n = match ctx.eval_const_msg(&c) {
                Ok(n) => n,
                Err(why) => {
                    err(format!(
                        "the value of `{}::{}` is not a const integer: {why}",
                        decl.name, v.name
                    ));
                    tags.push(None);
                    continue;
                }
            };
            let (lo, hi) = super::const_eval::int_range(tag_ty);
            if !(lo..=hi).contains(&n) {
                err(format!(
                    "the value {n} of `{}::{}` does not fit the tag type `{}`",
                    decl.name,
                    v.name,
                    tag_ty.name()
                ));
                tags.push(None);
                continue;
            }
            if let Some(other) = seen.insert(n, &v.name) {
                err(format!(
                    "`{}::{}` and `{}::{other}` have the same value {n}",
                    decl.name, v.name, decl.name
                ));
            }
            tags.push(Some(n));
        }
        let mut enums = env.defs.enums.borrow_mut();
        for (hv, t) in enums[i].variants.iter_mut().zip(tags) {
            if let Some(n) = t {
                hv.tag = n;
            }
        }
    }
}

/// `#[niche]` placement (spec 2.7.5): a single-field struct only.
fn check_niche_attrs(program: &rokc_ast::Program, diags: &mut Vec<rokc_diagnostic::RokDiagnostic>) {
    for item in &program.items {
        // `misplaced`: what the attribute sits on, if not one field.
        let (attrs, misplaced) = match item {
            Item::Struct(s) => {
                let n = match &s.kind {
                    AstStructKind::Named(fs) => fs.len(),
                    AstStructKind::Tuple(fs) => fs.len(),
                    AstStructKind::Unit => 0,
                };
                (
                    &s.attrs,
                    (n != 1).then(|| format!("a struct with {n} fields")),
                )
            }
            Item::Enum(e) => (&e.attrs, Some("an enum".to_owned())),
            Item::Union(u) => (&u.attrs, Some("a union".to_owned())),
            _ => continue,
        };
        let Some(attr) = attrs.iter().find(|a| a.name() == "niche") else {
            continue;
        };
        let msg = match misplaced {
            None => niche_args(attrs).err(),
            Some(w) => Some(format!(
                "`#[niche]` goes on a single-field struct, not {w} (spec 2.7.5)"
            )),
        };
        if let Some(msg) = msg {
            diags.push(rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Type,
                attr.span,
                msg,
            ));
        }
    }
}

pub(super) fn lower_struct_def(
    s: &AstStructDef,
    id: StructId,
    defs: &TypeDefs<'_>,
    sc: Scope<'_>,
) -> HirStructDef {
    let fields = match &s.kind {
        AstStructKind::Named(fs) => fs
            .iter()
            .map(|f| HirField {
                name: f.name.clone(),
                ty: resolve_ty_with_structs(&f.ty, defs, None, sc),
                // A literal omitting the field fills it at the use site.
                default: f.default.clone().map(|d| in_module(&module_of(&s.name), d)),
            })
            .collect(),
        // Tuple struct: fields named "0", "1", ... so `n.0` reads like a
        // named field (spec 2.7.4).
        AstStructKind::Tuple(fs) => fs
            .iter()
            .enumerate()
            .map(|(i, f)| HirField {
                name: i.to_string(),
                ty: resolve_ty_with_structs(&f.ty, defs, None, sc),
                default: None,
            })
            .collect(),
        AstStructKind::Unit => Vec::new(),
    };
    HirStructDef {
        id,
        repr: repr_of(&s.attrs).unwrap_or_default(),
        def_id: None,
        name: s.name.clone(),
        fields,
        span: s.span,
        generic_args: Vec::new(),
        union_kind: None,
        niche: niche_args(&s.attrs).unwrap_or_default(),
    }
}

/// A `union` as one synthetic `HirStructDef` field `__data` of the chosen
/// storage (`union_storage_strategy`); field → declared type in
/// `union_kind.aliases`.
pub(super) fn lower_union_def(
    u: &rokc_ast::UnionDef,
    id: StructId,
    defs: &TypeDefs<'_>,
    sc: Scope<'_>,
    pre_diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) -> HirStructDef {
    let res = |t: &rokc_ast::Ty| resolve_ty_with_structs(t, defs, None, sc);
    union_def(u, id, u.name.clone(), &res, Vec::new(), pre_diags)
}

/// `u` as a struct named `name` (a generic one's instance: `res`
/// resolves under its substitution, `generic_args` are its arguments).
pub(super) fn union_def(
    u: &rokc_ast::UnionDef,
    id: StructId,
    name: String,
    res: &dyn Fn(&rokc_ast::Ty) -> HirTy,
    generic_args: Vec<HirTy>,
    pre_diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) -> HirStructDef {
    let aliases: Vec<(String, HirTy)> = u
        .fields
        .iter()
        .map(|f| (f.name.clone(), res(&f.ty)))
        .collect();
    let is_repr_c = u.attrs.iter().any(|a| {
        a.path.as_slice() == ["repr"]
            && matches!(&a.body, rokc_ast::AttrBody::Args(args)
                if args.iter().any(|arg| matches!(arg, rokc_ast::AttrArg::Word(w) if w == "C")))
    });
    let storage = union_storage_strategy(&aliases, is_repr_c, &u.name, u.span, pre_diags);
    let storage_ty = match &storage {
        UnionStorage::WidestPrim(p) => HirTy::Prim(*p),
        // Every field side by side; MIR overlays them (`union_storage`).
        UnionStorage::ByteArray => HirTy::Tuple(aliases.iter().map(|(_, t)| t.clone()).collect()),
    };
    HirStructDef {
        id,
        repr: repr_of(&u.attrs).unwrap_or_default(),
        def_id: None,
        name,
        fields: vec![HirField {
            name: "__data".to_owned(),
            ty: storage_ty,
            default: None,
        }],
        span: u.span,
        generic_args,
        union_kind: Some(HirUnionKind {
            storage,
            aliases,
            is_repr_c,
        }),
        niche: Vec::new(),
    }
}

/// Union storage: all-primitive unions without `#[repr(C)]` use the widest
/// primitive (model A); otherwise `[u8; size]` (model B).
pub(super) fn union_storage_strategy(
    aliases: &[(String, HirTy)],
    is_repr_c: bool,
    name: &str,
    span: rokc_diagnostic::Span,
    pre_diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) -> UnionStorage {
    if aliases.is_empty() {
        pre_diags.push(rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Type,
            span,
            format!("union `{name}` declares no fields"),
        ));
        return UnionStorage::WidestPrim(PrimTy::U8);
    }
    let all_prim = aliases
        .iter()
        .all(|(_, t)| matches!(t, HirTy::Prim(p) if *p != PrimTy::Str));
    if all_prim && !is_repr_c {
        // Pick the widest primitive. Tie-break: prefer unsigned > signed
        // > float so written bits round-trip cleanly through a wider read.
        let mut best: Option<PrimTy> = None;
        let mut best_score: (u32, u8) = (0, 0);
        for (_, t) in aliases {
            if let HirTy::Prim(p) = t {
                let width = p.bit_width(64);
                let kind_pref: u8 = match p {
                    PrimTy::U8 | PrimTy::U16 | PrimTy::U32 | PrimTy::U64 | PrimTy::Usize => 3,
                    PrimTy::I8
                    | PrimTy::I16
                    | PrimTy::I32
                    | PrimTy::I64
                    | PrimTy::Isize
                    | PrimTy::CChar
                    | PrimTy::Char => 2,
                    PrimTy::F32 | PrimTy::F64 => 1,
                    PrimTy::Bool | PrimTy::Str => 0,
                };
                let score = (width, kind_pref);
                if best.is_none() || score > best_score {
                    best = Some(*p);
                    best_score = score;
                }
            }
        }
        return UnionStorage::WidestPrim(best.unwrap_or(PrimTy::U8));
    }
    UnionStorage::ByteArray
}

/// What a trait method's signature type depends on besides concrete types:
/// `Self`, an associated type (pushed as `Self::X`) or a trait parameter.
fn ast_ty_needs(ty: &rokc_ast::Ty, params: &[&str], assoc: &[&str], out: &mut Vec<String>) {
    let kids: Vec<&rokc_ast::Ty> = match &ty.kind {
        AstTyKind::SelfUpper => {
            out.push("Self".to_owned());
            Vec::new()
        }
        AstTyKind::Path(p) => {
            match p.segments.as_slice() {
                [s, x, ..] if s.name == "Self" => out.push(format!("Self::{}", x.name)),
                [s] | [s, _] if assoc.contains(&s.name.as_str()) => {
                    out.push(format!("Self::{}", s.name));
                }
                [s, ..] if s.name == "Self" || params.contains(&s.name.as_str()) => {
                    out.push(s.name.clone());
                }
                _ => {}
            }
            p.segments.iter().flat_map(|s| &s.generics).collect()
        }
        AstTyKind::QPath(q) => std::iter::once(&q.self_ty)
            .chain(q.trait_path.segments.iter().flat_map(|s| &s.generics))
            .collect(),
        AstTyKind::AnyTrait(bounds) => bounds
            .iter()
            .flat_map(|b| b.segments.iter().flat_map(|s| &s.generics))
            .collect(),
        AstTyKind::Sum(parts) | AstTyKind::Tuple(parts) => parts.iter().collect(),
        AstTyKind::Fn(args, ret, _) => args.iter().chain(std::iter::once(&**ret)).collect(),
        AstTyKind::Throws { ok, err } => vec![&**ok, &**err],
        AstTyKind::Array(elem, _) | AstTyKind::Nest(elem) | AstTyKind::Binding(_, elem) => {
            vec![&**elem]
        }
        AstTyKind::Any | AstTyKind::None | AstTyKind::Never | AstTyKind::Const(_) => Vec::new(),
    };
    for k in kids {
        ast_ty_needs(k, params, assoc, out);
    }
}

/// Report impl method `m` when its parameter count, `ref` modes or return
/// differ from the trait's `tm`; the declaration is the fix.
fn sig_mismatch(
    m: &FnDef,
    tm: &FnDef,
    trait_name: &str,
    diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) -> bool {
    let mut why = Vec::new();
    if m.params.len() == tm.params.len() {
        // The receiver may differ: `self` fills a `ref self` slot.
        for (p, tp) in m
            .params
            .iter()
            .zip(&tm.params)
            .filter(|(p, _)| p.name != "self")
        {
            match (p.by_ref, tp.by_ref) {
                (false, true) => why.push(format!("`{}` must be `ref {}`", p.name, p.name)),
                (true, false) => why.push(format!("`{}` must not be `ref`", p.name)),
                _ => {}
            }
        }
    } else {
        why.push(format!(
            "it takes {} parameters where the trait's takes {}",
            m.params.len(),
            tm.params.len()
        ));
    }
    let ret = |f: &FnDef| {
        f.ret_ty
            .as_ref()
            .filter(|t| !matches!(&t.kind, AstTyKind::Tuple(v) if v.is_empty()))
            .map(rokc_ast::print::print_ty)
    };
    match (ret(m), ret(tm)) {
        (Some(r), None) => why.push(format!(
            "it returns `{r}` where the trait's returns nothing"
        )),
        (None, Some(r)) => why.push(format!(
            "it returns nothing where the trait's returns `{r}`"
        )),
        _ => {}
    }
    if why.is_empty() {
        return false;
    }
    let name = leaf_name(&m.name);
    diags.push(
        rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Type,
            m.span,
            format!(
                "`{name}` does not match its declaration in trait `{}`: {}",
                leaf_name(trait_name),
                why.join(", and ")
            ),
        )
        .with_help(format!("write `{}`", rokc_ast::print::print_fn_sig(tm))),
    );
    true
}

fn is_throwing(t: &rokc_ast::ThrowsKind) -> bool {
    match t {
        rokc_ast::ThrowsKind::None => false,
        rokc_ast::ThrowsKind::Untyped => true,
        rokc_ast::ThrowsKind::Typed(v) => !v.is_empty(),
    }
}

/// `impl<..> Tr for Target` as method templates (trait defaults included),
/// monomorphized per receiver.
fn generic_trait_impl(
    impl_def: &rokc_ast::ImplDef,
    trait_key: String,
    trait_def: Option<&rokc_ast::TraitDef>,
    module: &str,
    tag: &str,
    diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
    duplicated: &mut std::collections::HashSet<Span>,
) -> Impl {
    let err = |pre: &mut Vec<rokc_diagnostic::RokDiagnostic>, msg: String| {
        pre.push(rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Type,
            impl_def.span,
            msg,
        ));
    };
    let target_display = impl_def_display(&impl_def.self_ty);
    let assoc = impl_assoc_types(impl_def);
    let template = |m: &FnDef, def_module: &str| {
        let mut def = m.clone();
        subst_assoc_sig(&mut def, &assoc);
        let leaf = leaf_name(&m.name);
        // `__impl1f2e3d4c_show` is both the mono cache key and the name
        // diagnostics print.
        def.name = if def_module.is_empty() {
            format!("__impl{tag}_{leaf}")
        } else {
            format!("{def_module}::__impl{tag}_{leaf}")
        };
        // The method's own (assoc-substituted) generics after the impl's.
        def.generics.params = [&impl_def.generics.params, &def.generics.params]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        def.where_clause = [&impl_def.where_clause, &def.where_clause]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        GenericMethod {
            def: std::sync::Arc::new(def),
            self_ty: impl_def.self_ty.clone(),
            info: MethodInfo {
                module: def_module.to_owned(),
                vis: Visibility::Pub,
                trait_name: (!trait_key.is_empty()).then(|| trait_key.clone()),
                trait_args: Vec::new(),
            },
        }
    };
    let mut methods: HashMap<String, GenericMethod> = HashMap::new();
    // Methods unlike the trait's: reported, not missing.
    let mut mismatched = std::collections::HashSet::new();
    for ii in &impl_def.items {
        let AstImplItem::Method(m) = ii else {
            continue;
        };
        let leaf = leaf_name(&m.name).to_owned();
        if let Some(td) = trait_def {
            let tm = td.items.iter().find_map(|ti| match ti {
                rokc_ast::TraitItem::Method(tm) if tm.name == leaf => Some(tm),
                _ => None,
            });
            match tm {
                None => err(
                    diags,
                    format!("method `{leaf}` is not a member of trait `{}`", td.name),
                ),
                Some(tm) if sig_mismatch(m, tm, &td.name, diags) => {
                    mismatched.insert(leaf);
                    continue;
                }
                Some(_) => {}
            }
        }
        if let Some(first) = methods.get(&leaf) {
            duplicated.insert(first.def.span);
            diags.push(rokc_diagnostic::duplicate(
                rokc_diagnostic::ErrorKind::Type,
                format!("method `{leaf}` is defined more than once for `{target_display}`"),
                &leaf,
                m.span,
                Some(first.def.span),
                true,
            ));
            continue;
        }
        methods.insert(leaf.clone(), template(m, module));
    }
    if let Some(td) = trait_def {
        let trait_module = module_of(&td.name);
        for ti in &td.items {
            let rokc_ast::TraitItem::Method(tm) = ti else {
                continue;
            };
            if methods.contains_key(&tm.name) || mismatched.contains(&tm.name) {
                continue;
            }
            if tm.body.is_none() {
                err(
                    diags,
                    format!(
                        "impl of trait `{}` for `{target_display}` is missing method `{}`",
                        td.name, tm.name
                    ),
                );
                continue;
            }
            // `trait Contains<Item>` + `impl<T> Contains<T> for L<T>`: the
            // default's signature sees `Item = T`.
            let mut tm = tm.clone();
            let mut args = trait_param_args(td, impl_def);
            supertrait_projections(&tm, td, impl_def, &mut args);
            subst_assoc_sig(&mut tm, &args);
            methods.insert(tm.name.clone(), template(&tm, &trait_module));
        }
    }
    Impl {
        trait_key,
        trait_args: impl_def
            .trait_path
            .as_ref()
            .and_then(|tp| tp.segments.last())
            .map(|s| s.generics.clone())
            .unwrap_or_default(),
        assoc: impl_assoc_list(impl_def),
        generics: impl_def.generics.clone(),
        where_clause: impl_def.where_clause.clone(),
        self_ty: impl_def.self_ty.clone(),
        module: module.to_owned(),
        methods,
        consts: Vec::new(),
        concrete: None,
    }
}

/// An impl that is not a generic trait impl, registered by its written
/// target (`at`: its `Impl`).
struct PendingImpl<'p> {
    def: &'p rokc_ast::ImplDef,
    module: String,
    trait_name: Option<String>,
    /// The target's generic base / primitive name (method templates).
    generic_base: Option<String>,
    at: usize,
}

/// Each pending impl's target and trait ref, once all items exist (a
/// generic inherent impl's target is its base). Projection targets resolve
/// through earlier impls, so rounds repeat while progress is made; the last
/// reports what's left. `None`: a duplicate trait impl for the target.
fn impl_heads(
    env: &mut EnvIn<'_, '_>,
    pending: &[PendingImpl<'_>],
    trait_defs: &HashMap<String, &rokc_ast::TraitDef>,
    diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) -> Vec<Option<(HirTy, Option<TraitRef>)>> {
    let mut heads = vec![None; pending.len()];
    let mut left: Vec<usize> = Vec::new();
    for (i, p) in pending.iter().enumerate() {
        if p.def.generics.params.is_empty() {
            left.push(i);
        } else {
            let base = p.generic_base.clone().unwrap_or_default();
            heads[i] = Some((HirTy::Path(vec![base]), None));
        }
    }
    let open = |t: &HirTy| {
        let mut paths = Vec::new();
        collect_unknown_paths(t, &mut paths);
        *t == HirTy::Unknown || !paths.is_empty()
    };
    let mut last = false;
    let no_subst = HashMap::new();
    while !left.is_empty() {
        // One round's heads, against the impls resolved before it; one
        // still open waits (its diagnostics dropped) unless it is the last.
        let round: Vec<Option<(HirTy, Option<TraitRef>)>> = {
            let mut rcx = LowerCtx::new(env, &no_subst, None, Span::DUMMY);
            left.iter()
                .map(|&i| {
                    let p = &pending[i];
                    rcx.at.set(p.def.span);
                    let mark = env.sink.diagnostics.borrow().len();
                    let td = p
                        .trait_name
                        .as_ref()
                        .and_then(|t| trait_defs.get(t).copied());
                    let (ty, provides) = impl_head(&mut rcx, p, td);
                    let unresolved = open(&ty)
                        || provides.as_ref().is_some_and(|r| {
                            r.args
                                .iter()
                                .chain(r.binds.iter().map(|(_, t)| t))
                                .any(open)
                        });
                    if unresolved && !last {
                        env.sink.diagnostics.borrow_mut().truncate(mark);
                        return None;
                    }
                    Some((ty, provides))
                })
                .collect()
        };
        let mut waiting = Vec::new();
        for (&i, head) in left.iter().zip(round) {
            let p = &pending[i];
            let Some((ty, provides)) = head else {
                waiting.push(i);
                continue;
            };
            if let Some(r) = provides.as_ref().filter(|r| !r.args.is_empty()) {
                if env
                    .impls
                    .refs_on(&ty)
                    .any(|o| o.path == r.path && o.args == r.args)
                {
                    let shown = TraitRef::new(r.path.clone(), r.args.clone(), Vec::new());
                    let shown = shown.render(&r.path, |t| match t {
                        HirTy::Struct(id) => env.defs.structs.borrow()[id.0 as usize].name.clone(),
                        HirTy::Enum(id) => env.defs.enums.borrow()[id.0 as usize].name.clone(),
                        t => ty_mangle(t),
                    });
                    diags.push(rokc_diagnostic::RokDiagnostic::error(
                        rokc_diagnostic::ErrorKind::Type,
                        p.def.span,
                        format!(
                            "`{shown}` is implemented more than once for `{}`",
                            impl_def_display(&p.def.self_ty)
                        ),
                    ));
                    continue;
                }
            }
            env.impls.resolve(p.at, ty.clone(), provides.clone());
            heads[i] = Some((ty, provides));
        }
        last = waiting.len() == left.len();
        left = waiting;
    }
    heads
}

/// A concrete impl's target and trait ref: `impl Get<i32> for T { type
/// Item = u8; }` also answers `Get<Item = u8>` and `Get<i32, Item = u8>`.
fn impl_head(
    rcx: &mut LowerCtx<'_>,
    p: &PendingImpl<'_>,
    trait_def: Option<&rokc_ast::TraitDef>,
) -> (HirTy, Option<TraitRef>) {
    rcx.self_ty = None;
    rcx.current_module.clone_from(&p.module);
    let target = rcx.resolve_ast_ty(&p.def.self_ty);
    let Some(tn) = &p.trait_name else {
        return (target, None);
    };
    if method_target_for(&target).is_none() {
        return (target, None);
    }
    rcx.self_ty = Some(target.clone());
    let mut args: Vec<HirTy> = p
        .def
        .trait_path
        .as_ref()
        .and_then(|tp| tp.segments.last())
        .map(|s| s.generics.iter().map(|g| rcx.resolve_ast_ty(g)).collect())
        .unwrap_or_default();
    // Spec 2.10.5: omitted trait arguments take the parameter's default
    // (`trait Add<Rhs = Self>`: `impl Add for V` is `impl Add<V> for V`).
    if let Some(td) = trait_def {
        for gp in td.generics.params.iter().skip(args.len()) {
            let rokc_ast::GenericParam::Type {
                default: Some(d), ..
            } = gp
            else {
                break;
            };
            args.push(rcx.resolve_ast_ty(d));
        }
    }
    let assoc = impl_assoc_list(p.def)
        .iter()
        .map(|(n, t)| (n.clone(), rcx.resolve_ast_ty(t)))
        .collect();
    (target, Some(TraitRef::new(tn.clone(), args, assoc)))
}

/// An impl's `type X = T;` items, in order.
fn impl_assoc_list(impl_def: &rokc_ast::ImplDef) -> Vec<(String, Ty)> {
    impl_def
        .items
        .iter()
        .filter_map(|i| match i {
            AstImplItem::AssocType { name, ty } => Some((name.clone(), ty.clone())),
            _ => None,
        })
        .collect()
}

/// An impl's consts, plus a trait impl's omitted defaults: `(Type::NAME,
/// type, value)` keyed by generic base or primitive name where one names
/// it, and all of them.
// One key per base: two instances' impls can't give a const two values.
fn impl_consts(
    i: &rokc_ast::ImplDef,
    program: &Program,
    base: &HashMap<String, &rokc_ast::TraitDef>,
    names: &TypeNames,
    pre_diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) -> (Vec<AssocConst>, Vec<AssocConst>) {
    let mut out: Vec<(String, Ty, Expr)> = i
        .items
        .iter()
        .filter_map(|it| match it {
            AstImplItem::Const(c) => {
                Some((leaf_name(&c.name).to_owned(), c.ty.clone(), c.value.clone()))
            }
            _ => None,
        })
        .collect();
    let module = impl_module(i);
    let trait_def = i.trait_path.as_ref().and_then(|tp| {
        let segs: Vec<&str> = tp.segments.iter().map(|s| s.name.as_str()).collect();
        let key = names.trait_key(&segs, &module, i.span);
        let fits = |t: &rokc_ast::TraitDef| {
            t.name == key || (!key.contains("::") && leaf_name(&t.name) == key)
        };
        program
            .items
            .iter()
            .find_map(|it| match it {
                Item::Trait(t) if fits(t) => Some(t),
                _ => None,
            })
            .or_else(|| base.get(key.as_str()).copied())
            .or_else(|| {
                let mut leaf: Vec<&rokc_ast::TraitDef> =
                    base.values().copied().filter(|t| fits(t)).collect();
                leaf.sort_by(|a, b| a.name.cmp(&b.name));
                leaf.first().copied()
            })
    });
    if let Some(t) = trait_def.filter(|_| !i.negative) {
        for ti in &t.items {
            let rokc_ast::TraitItem::Const { name, ty, default } = ti else {
                continue;
            };
            if out.iter().any(|(n, ..)| n == name) {
                continue;
            }
            match default {
                Some(v) => out.push((name.clone(), ty.clone(), v.clone())),
                None => pre_diags.push(rokc_diagnostic::RokDiagnostic::error(
                    rokc_diagnostic::ErrorKind::Type,
                    i.span,
                    format!(
                        "impl of `{}` for `{}` is missing associated const `{name}` \
                         (spec 2.8.3a)",
                        leaf_name(&t.name),
                        impl_def_display(&i.self_ty)
                    ),
                )),
            }
        }
    }
    if out.is_empty() {
        return (Vec::new(), out);
    }
    let params: Vec<&str> = i
        .generics
        .params
        .iter()
        .filter_map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. } => Some(name.as_str()),
            rokc_ast::GenericParam::Const { .. } => None,
        })
        .collect();
    // `Type::N` names the const when the impl covers that type or every
    // instance of a base; others are found from a concrete type (`T::N`,
    // `Self::N`, `<T as Tr>::N`).
    let keyed_target = match &i.self_ty.kind {
        AstTyKind::Path(p) => {
            let args = p.segments.last().map_or(&[][..], |s| s.generics.as_slice());
            let all_params = args.len() == params.len()
                && args.iter().zip(&params).all(|(a, n)| {
                    matches!(&a.kind, AstTyKind::Path(ap)
                        if ap.segments.len() == 1 && ap.segments[0].name == *n)
                });
            if let HirTy::Prim(prim) = resolve_ty(&i.self_ty) {
                Some(prim.name().to_owned())
            } else if (params.is_empty() && args.is_empty())
                || (i.trait_path.is_none() && !args.is_empty() && all_params)
            {
                let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
                names.canon(&segs, &module, &[], i.span)
            } else {
                None
            }
        }
        _ => None,
    };
    let keyed = keyed_target.map_or_else(Vec::new, |t| {
        out.iter()
            .map(|(n, ty, v)| (format!("{t}::{n}"), ty.clone(), v.clone()))
            .collect()
    });
    (keyed, out)
}

/// Lower declaration `i` (`stub`: its signature only).
pub(super) fn lower_decl(env: &Env<'_>, i: usize, stub: bool) -> HirFunction {
    let id = FunctionId(u32::try_from(i).expect("FunctionId overflow"));
    let decl = env.decl(id).expect("a declaration");
    let f = decl.def();
    let bodiless;
    let f: &FnDef = if stub {
        bodiless = FnDef {
            vis: f.vis,
            attrs: f.attrs.clone(),
            name: f.name.clone(),
            generics: f.generics.clone(),
            params: f.params.clone(),
            ret_ty: f.ret_ty.clone(),
            body: None,
            is_async: f.is_async,
            is_generator: f.is_generator,
            is_unsafe: f.is_unsafe,
            throws: f.throws.clone(),
            where_clause: f.where_clause.clone(),
            span: f.span,
        };
        &bodiless
    } else {
        f
    };
    match decl {
        MethodOrFn::Fn(_) => lower_fn(env, f, id, None, fn_module(env, f)),
        MethodOrFn::Method {
            self_ty,
            module,
            key,
            subst,
            ..
        } => super::mono::lower_fn_inner(
            env,
            f,
            id,
            key.clone(),
            Some(self_ty.clone()),
            module.clone(),
            subst,
        ),
    }
}

/// Pass 3: bodies. The `roots` (user code, exports, lazy static inits) are
/// lowered, then every decl a lowered body names, lowest id first. An
/// unreached library body stays a signature-only stub that MIR drops;
/// `--check-library` lowers all. Prebuilt (`linked`) functions keep only
/// their signatures.
fn lower_bodies(
    env: &Env<'_>,
    roots: &[bool],
    linked: &std::collections::HashSet<FunctionId>,
    copies: &std::collections::HashSet<FunctionId>,
    incr: Option<&super::incr::Incr<'_>>,
) -> (Vec<HirFunction>, Vec<FunctionId>) {
    let decls = &env.decls;
    let lower = |i: usize, stub: bool| match incr {
        Some(incr) if !stub => incr.lower(env, i),
        _ => lower_decl(env, i, stub),
    };
    let mut done: Vec<Option<HirFunction>> = (0..decls.len()).map(|_| None).collect();
    let mut pending: std::collections::BTreeSet<usize> =
        (0..decls.len()).filter(|&i| roots[i]).collect();
    let named = |f: &HirFunction, pending: &mut std::collections::BTreeSet<usize>| {
        let Some(body) = &f.body else { return };
        crate::each_block(body, &mut |e| {
            if let HirExprKind::Call { callee: id, .. }
            | HirExprKind::FnRef(id)
            | HirExprKind::ClosureValue { fn_id: id, .. }
            | HirExprKind::CoroNew { body_fn: id, .. } = &e.kind
            {
                pending.insert(id.0 as usize);
            }
        });
    };
    // Lifted closures / monomorphizations / dispatchers already looked at.
    let mut scanned = 0;
    loop {
        while let Some(i) = pending.pop_first() {
            if done.get(i).is_none_or(Option::is_some) {
                continue; // an extra (already lowered) or a done decl
            }
            #[allow(clippy::cast_possible_truncation)]
            let id = FunctionId(i as u32);
            let f = lower(i, linked.contains(&id) && !copies.contains(&id));
            named(&f, &mut pending);
            done[i] = Some(f);
            let extras = env.sink.extras.borrow();
            extras[scanned..]
                .iter()
                .for_each(|f| named(f, &mut pending));
            scanned = extras.len();
        }
        build_any_dispatchers(env);
        let extras = env.sink.extras.borrow();
        extras[scanned..]
            .iter()
            .for_each(|f| named(f, &mut pending));
        drop(extras);
        pending.retain(|&i| done.get(i).is_some_and(Option::is_none));
        if pending.is_empty() {
            break;
        }
        // The new bodies may add `any` tags: drop the dispatchers (the
        // last ones in) and build them again after.
        let n = env.sink.any_dispatch.borrow().len();
        let mut extras = env.sink.extras.borrow_mut();
        scanned = extras.len() - n;
        extras.truncate(scanned);
    }
    env.sink.bodies_done.set(true);
    let mut unlowered = Vec::new();
    let functions = done
        .into_iter()
        .enumerate()
        .map(|(i, f)| {
            f.unwrap_or_else(|| {
                let stub = sig_stub(env, i);
                unlowered.push(stub.id);
                stub
            })
        })
        .collect();
    (functions, unlowered)
}

/// The body-less stub of unreached declaration `i`, from its resolved
/// signature (for tools; MIR drops it).
fn sig_stub(env: &Env<'_>, i: usize) -> HirFunction {
    let id = FunctionId(u32::try_from(i).expect("FunctionId overflow"));
    let f = env
        .decl(FunctionId(u32::try_from(i).expect("FunctionId overflow")))
        .expect("a declaration")
        .def();
    let name = match &env.decls[i] {
        MethodOrFn::Fn(f) => f.name.clone(),
        MethodOrFn::Method { key, .. } => key.clone(),
    };
    let sig = &env.fn_table[&env.key_of[&id]];
    let mut params = Vec::with_capacity(f.params.len());
    let mut locals = Vec::with_capacity(f.params.len());
    let mut refs = Vec::new();
    for (k, (p, ty)) in f.params.iter().zip(&sig.param_tys).enumerate() {
        let local = rokc_ir_common::LocalId(u32::try_from(k).expect("LocalId overflow"));
        if p.by_ref {
            refs.push(ty.clone());
        }
        locals.push(HirLocal {
            id: local,
            name: Some(p.name.clone()),
            ty: ty.clone(),
            boxed: false,
        });
        params.push(HirParam {
            name: p.name.clone(),
            local,
            ty: ty.clone(),
            by_ref: p.by_ref,
        });
    }
    HirFunction {
        id,
        def_id: None,
        name,
        params,
        locals,
        ret_ty: super::mono::ref_packed(&sig.ret_ty, &refs),
        body: None,
        is_async: false,
        is_generator: false,
        span: f.span,
        facts: rokc_ir_common::FnFacts::default(),
    }
}

/// Spec 6.2.1: `#[derive(Tr)]` needs every field to implement `Tr` (here
/// for non-generic types; generic impls bound their parameters), and
/// `Copy` excludes `Drop`. Returns the failed attributes' spans.
fn check_derives(env: &Env<'_>, program: &Program) -> Vec<rokc_diagnostic::Span> {
    let mut failed = Vec::new();
    let no_subst = HashMap::new();
    let ctx = LowerCtx::new(env, &no_subst, None, Span::DUMMY);
    for item in &program.items {
        let (attrs, name, generic, fields): (_, _, _, Vec<(String, HirTy)>) = match item {
            Item::Struct(s) => (
                &s.attrs,
                &s.name,
                !s.generics.params.is_empty(),
                env.defs.struct_id(&s.name).map_or_else(Vec::new, |id| {
                    ctx.struct_def(id)
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty.clone()))
                        .collect()
                }),
            ),
            Item::Enum(e) => (
                &e.attrs,
                &e.name,
                !e.generics.params.is_empty(),
                env.defs.enum_id(&e.name).map_or_else(Vec::new, |id| {
                    ctx.enum_def(id)
                        .variants
                        .iter()
                        .flat_map(|v| {
                            super::expr::variant_arg_tys(v)
                                .into_iter()
                                .map(|t| (v.name.clone(), t))
                        })
                        .collect()
                }),
            ),
            _ => continue,
        };
        if generic {
            continue;
        }
        let self_ty = match item {
            Item::Struct(s) => env.defs.struct_id(&s.name).map(HirTy::Struct),
            Item::Enum(e) => env.defs.enum_id(&e.name).map(HirTy::Enum),
            _ => None,
        };
        ctx.at.set(item.span());
        check_pod(&ctx, attrs, name, self_ty.as_ref(), &fields);
        for (tr, at) in rokc_parser::derive::derive_names(attrs) {
            let Some(key) = rokc_parser::derive::derivable(&tr) else {
                continue;
            };
            ctx.at.set(at);
            for (field, ty) in &fields {
                if !ctx.has_trait(ty, key) {
                    failed.push(at);
                    ctx.emit_error(
                        rokc_diagnostic::ErrorKind::Meta,
                        format!(
                            "cannot derive `{tr}` for `{}`: `{field}` has type `{}`, which does \
                             not implement `{tr}` (spec 6.2.1)",
                            leaf_name(name),
                            ctx.ty_name(ty)
                        ),
                    );
                }
            }
            if tr == "Copy"
                && self_ty
                    .as_ref()
                    .is_some_and(|t| ctx.has_trait(t, ctx.lang("drop_method")))
            {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Meta,
                    format!(
                        "cannot derive `Copy` for `{}`: it implements `Drop` (stdlib 2.13.3)",
                        leaf_name(name)
                    ),
                );
            }
        }
    }
    failed
}

/// `Pod` (stdlib 1.3.3): a `#[repr(C)]` / `#[repr(transparent)]` struct of
/// `Pod` fields without `Drop`.
fn check_pod(
    ctx: &LowerCtx<'_>,
    attrs: &[rokc_ast::Attribute],
    name: &str,
    self_ty: Option<&HirTy>,
    fields: &[(String, HirTy)],
) {
    const POD: &str = "rok::bare::mem::Pod";
    let Some(ty) = self_ty.filter(|t| ctx.has_trait(t, POD)) else {
        return;
    };
    let fail = |why: String| {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Meta,
            format!(
                "`{}` cannot be `Pod`: {why} (stdlib 1.3.3)",
                leaf_name(name)
            ),
        );
    };
    if !matches!(ty, HirTy::Struct(_)) {
        return fail("only a struct can be".to_owned());
    }
    let repr = repr_of(attrs).unwrap_or_default();
    if !repr.c && !repr.transparent {
        fail("it needs `#[repr(C)]` or `#[repr(transparent)]`".to_owned());
    }
    if ctx.has_trait(ty, ctx.lang("drop_method")) {
        fail("it implements `Drop`".to_owned());
    }
    for (field, fty) in fields {
        if !ctx.has_trait(fty, POD) {
            fail(format!(
                "`{field}` has type `{}`, which is not `Pod`",
                ctx.ty_name(fty)
            ));
        }
    }
}

/// Every trait in a bound position must exist, with no more type arguments
/// than parameters (stdlib 2.11.3).
fn check_bound_names(
    program: &Program,
    names: &TypeNames,
    base: &HashMap<String, &rokc_ast::TraitDef>,
    pre_diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) {
    let arity: HashMap<&str, &rokc_ast::Generics> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Trait(t) => Some(t),
            _ => None,
        })
        .chain(base.values().copied())
        .map(|t| (t.name.as_str(), &t.generics))
        .collect();
    // `role` says where the trait was written: "in a bound", "in an impl", ..
    let mut check = |b: &Ty, module: &str, role: &str| {
        let AstTyKind::Path(p) = &b.kind else {
            return;
        };
        let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
        let key = names.trait_key(&segs, module, b.span);
        // A bootstrap trait keys by its leaf (see `trait_key`).
        let Some(found) = names.traits.get(&key) else {
            let d = rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Resolve,
                b.span,
                format!("cannot find trait `{}` (named {role})", segs.join("::")),
            );
            let keys = || {
                names
                    .traits
                    .iter()
                    .filter(|t| leaf_name(t) == key)
                    .cloned()
                    .collect()
            };
            let help = (segs.len() == 1)
                .then(|| names.import_help(module, keys(), |k| names.type_vis(k)))
                .flatten();
            pre_diags.push(match help {
                Some(h) => d.with_help(h),
                None => d,
            });
            return;
        };
        let written = p.segments.last().map_or(0, |s| {
            s.generics
                .iter()
                .filter(|g| rokc_ast::as_assoc_binding(g).is_none())
                .count()
        });
        let Some(&g) = arity.get(found.as_str()) else {
            return;
        };
        // Fewer is allowed: the bound holds for any of the rest
        // (`S: Source<Item = i32>`).
        let max = g.params.len();
        if written > max {
            let (msg, help) = mono::misfit(leaf_name(found), g, written);
            pre_diags.push(
                rokc_diagnostic::RokDiagnostic::error(
                    rokc_diagnostic::ErrorKind::Type,
                    b.span,
                    msg,
                )
                .with_help(help),
            );
        }
    };
    let mut generics = |g: &rokc_ast::Generics, w: &[rokc_ast::WherePred], module: &str| {
        for gp in &g.params {
            if let rokc_ast::GenericParam::Type { bounds, .. } = gp {
                bounds.iter().for_each(|b| check(b, module, "in a bound"));
            }
        }
        for p in w {
            p.bounds.iter().for_each(|b| check(b, module, "in a bound"));
        }
    };
    let fn_bounds =
        |f: &FnDef, generics: &mut dyn FnMut(&rokc_ast::Generics, &[rokc_ast::WherePred], &str)| {
            generics(&f.generics, &f.where_clause, &module_of(&f.name));
        };
    let mut impl_paths: Vec<(Ty, String, &str)> = Vec::new();
    for item in &program.items {
        let module = item_module(item);
        match item {
            Item::Fn(f) => fn_bounds(f, &mut generics),
            Item::Struct(s) => generics(&s.generics, &s.where_clause, &module),
            Item::Enum(e) => generics(&e.generics, &e.where_clause, &module),
            Item::Union(u) => generics(&u.generics, &u.where_clause, &module),
            Item::TypeAlias(a) => generics(&a.generics, &a.where_clause, &module),
            Item::Trait(t) => {
                generics(&t.generics, &t.where_clause, &module);
                for s in &t.supertraits {
                    impl_paths.push((s.clone(), module.clone(), "as a supertrait"));
                }
                for ti in &t.items {
                    match ti {
                        rokc_ast::TraitItem::Method(m) => fn_bounds(m, &mut generics),
                        rokc_ast::TraitItem::AssocType { bounds, .. } => {
                            impl_paths.extend(
                                bounds
                                    .iter()
                                    .map(|b| (b.clone(), module.clone(), "in a bound")),
                            );
                        }
                        rokc_ast::TraitItem::Const { .. } => {}
                    }
                }
            }
            Item::Impl(i) => {
                let module = impl_module(i);
                generics(&i.generics, &i.where_clause, &module);
                if let Some(tp) = &i.trait_path {
                    impl_paths.push((
                        Ty {
                            kind: AstTyKind::Path(tp.clone()),
                            span: tp.span,
                        },
                        module.clone(),
                        "in an impl",
                    ));
                }
                for ii in &i.items {
                    if let AstImplItem::Method(m) = ii {
                        fn_bounds(m, &mut generics);
                    }
                }
            }
            _ => {}
        }
    }
    for (t, module, role) in &impl_paths {
        check(t, module, role);
    }
}

/// Spec 2.10.4: every generic struct / union / enum instantiation must
/// meet its parameters' bounds, inline (`T: Tr`) and `where` alike.
fn check_type_bounds(env: &Env<'_>) {
    // (display name, item's module, arguments, generics, `where`, use site)
    type Inst<'d> = (
        String,
        String,
        Vec<HirTy>,
        &'d rokc_ast::Generics,
        &'d [rokc_ast::WherePred],
        rokc_diagnostic::Span,
    );
    let no_subst = HashMap::new();
    let ctx = LowerCtx::new(env, &no_subst, None, Span::DUMMY);
    let n_structs = env.defs.structs.borrow().len();
    let n_enums = env.defs.enums.borrow().len();
    let mut insts: Vec<Inst<'_>> = Vec::new();
    for i in 0..n_structs {
        let id = StructId(u32::try_from(i).expect("StructId overflow"));
        let d = ctx.struct_def(id);
        let base = d.name.split("__").next().unwrap_or_default().to_owned();
        if d.generic_args.is_empty() {
            continue;
        }
        let tmpl = env
            .defs
            .generic_structs
            .get(&base)
            .map(|s| (&s.generics, &s.where_clause));
        let tmpl = tmpl.or_else(|| {
            env.defs
                .generic_unions
                .get(&base)
                .map(|u| (&u.generics, &u.where_clause))
        });
        if let Some((generics, where_clause)) = tmpl {
            insts.push((
                ctx.ty_name(&HirTy::Struct(id)),
                module_of(&base),
                d.generic_args,
                generics,
                where_clause,
                d.span,
            ));
        }
    }
    for i in 0..n_enums {
        let id = EnumId(u32::try_from(i).expect("EnumId overflow"));
        let d = ctx.enum_def(id);
        let base = enum_base_name(&d.name).to_owned();
        if let Some(def) = env
            .defs
            .generic_enums
            .get(&base)
            .filter(|_| !d.generic_args.is_empty())
        {
            insts.push((
                ctx.ty_name(&HirTy::Enum(id)),
                module_of(&base),
                d.generic_args,
                &def.generics,
                &def.where_clause,
                d.span,
            ));
        }
    }
    // Spec 2.10.4: a non-generic alias's `where` holds once (at the
    // alias); a generic alias's bounds hold at each use.
    for (full, def) in &env.names.generic_aliases {
        if def.generics.params.is_empty() {
            insts.push((
                leaf_name(full).to_owned(),
                module_of(full),
                Vec::new(),
                &def.generics,
                &def.where_clause,
                def.span,
            ));
        }
    }
    for (shown, module, args, generics, where_clause, span) in insts {
        let errors = unmet_bounds(
            env,
            &shown,
            module,
            &args,
            generics,
            where_clause,
            span,
            |_| None,
        );
        env.sink.diagnostics.borrow_mut().extend(errors);
    }
    // Generic alias uses naming a type parameter are checked where its
    // bounds are known: by the body's definition check, or here for impl
    // targets.
    let mut seen = std::collections::HashSet::new();
    let uses = env.names.alias_uses.borrow().clone();
    for u in uses {
        if !seen.insert((u.0.clone(), u.3)) {
            continue;
        }
        let errors = if let Some(gi) = env.impls.generic().find(|gi| gi.self_ty.span == u.3) {
            let params: Vec<_> = gi.generics.params.iter().collect();
            let where_clause: Vec<_> = gi.where_clause.iter().collect();
            let self_of = super::bounds::SelfOf::Impl(&gi.self_ty);
            let owner = |_: &str| Some("the impl's generics".to_owned());
            super::bounds::in_scope(
                env,
                &params,
                &where_clause,
                (u.2.clone(), u.3, &[]),
                &self_of,
                |rcx, _, _| alias_use_unmet(rcx, &u, false, owner),
            )
            .0
        } else {
            let mut rcx = LowerCtx::new(env, &no_subst, None, u.3);
            let mark = rcx.probe_mark();
            let errors = alias_use_unmet(&mut rcx, &u, false, |_| None);
            rcx.rollback(&mark);
            errors
        };
        env.sink.diagnostics.borrow_mut().extend(errors);
    }
}

/// Bounds that generic alias use `u` fails, arguments resolved by `rcx`.
/// None when an argument names a type `rcx` doesn't know, or with
/// `params_only` when none names a parameter.
pub(super) fn alias_use_unmet(
    rcx: &mut LowerCtx<'_>,
    (full, args, module, span): &AliasUse,
    params_only: bool,
    owner: impl Fn(&str) -> Option<String>,
) -> Vec<rokc_diagnostic::RokDiagnostic> {
    let env = rcx.env;
    let Some(def) = env.names.generic_aliases.get(full) else {
        return Vec::new();
    };
    let outer = std::mem::replace(&mut rcx.current_module, module.clone());
    let args: Vec<HirTy> = def
        .generics
        .params
        .iter()
        .zip(args)
        .map(|(gp, a)| match (gp, &a.kind) {
            (rokc_ast::GenericParam::Const { .. }, AstTyKind::Const(c)) => rcx
                .eval_const(c)
                .map_or(HirTy::Unknown, |v| HirTy::Const(v, None)),
            _ => rcx.resolve_ast_ty(a),
        })
        .collect();
    rcx.current_module = outer;
    let unknown = |a: &HirTy| super::bounds::mentions(rcx, a, &|t| matches!(t, HirTy::Path(_)));
    let param = |a: &HirTy| super::bounds::has_param(rcx, a);
    if args.iter().any(unknown) || params_only && !args.iter().any(param) {
        return Vec::new();
    }
    let shown: Vec<String> = args.iter().map(|a| rcx.ty_name(a)).collect();
    let shown = format!("{}<{}>", leaf_name(full), shown.join(", "));
    let (generics, where_clause) = (&def.generics, &def.where_clause);
    unmet_bounds(
        env,
        &shown,
        module_of(full),
        &args,
        generics,
        where_clause,
        *span,
        owner,
    )
}

/// Bounds of `generics` / `where_clause` that `args` fail, as errors at
/// `span`; `owner` names the declaring item for the help.
#[allow(clippy::too_many_arguments)]
fn unmet_bounds(
    env: &Env<'_>,
    shown: &str,
    module: String,
    args: &[HirTy],
    generics: &rokc_ast::Generics,
    where_clause: &[rokc_ast::WherePred],
    span: Span,
    owner: impl Fn(&str) -> Option<String>,
) -> Vec<rokc_diagnostic::RokDiagnostic> {
    let subst = generic_subst(generics, args);
    let mut rcx = LowerCtx::new(env, &subst, None, span);
    rcx.current_module = module;
    let mut goals: Vec<(HirTy, &Vec<Ty>)> = Vec::new();
    for gp in &generics.params {
        if let rokc_ast::GenericParam::Type { name, bounds, .. } = gp {
            if let Some(t) = subst.get(name) {
                goals.push((t.clone(), bounds));
            }
        }
    }
    for w in where_clause {
        goals.push((rcx.resolve_ast_ty(&w.ty), &w.bounds));
    }
    let mut out = Vec::new();
    for (ty, bounds) in goals {
        if ty == HirTy::Unknown {
            continue;
        }
        for b in bounds {
            let module = rcx.current_module.clone();
            let Some(want) = rcx.bound_ref(b, &module, |t| rcx.resolve_ast_ty(t)) else {
                continue;
            };
            if rcx.implements_trait(&ty, &want, 0) {
                continue;
            }
            let name = rcx.ty_name(&ty);
            let mut d = rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Type,
                span,
                format!(
                    "type `{name}` does not satisfy trait bound `{}` required by `{shown}` \
                     (spec 2.10.4)",
                    rcx.trait_name(&want),
                ),
            );
            if let (HirTy::Param(_), Some(o)) = (&ty, owner(&name)) {
                let bound = want.render(leaf_name(&want.path), |t| rcx.ty_name(t));
                d.notes.push(format!("help: add `{name}: {bound}` to {o}"));
            }
            out.push(d);
        }
    }
    out
}

/// Spec 5.3: a `static` is read by every thread, so it must be `Sync`; the
/// error suggests a `Mutex`, or an `Atomic` for a `Cell` of a number.
fn check_statics_sync(env: &Env<'_>, programs: &[&Program]) {
    let no_subst = HashMap::new();
    let ctx = LowerCtx::new(env, &no_subst, None, Span::DUMMY);
    let sync = ctx.lang("sync_trait");
    if sync.is_empty() {
        return;
    }
    let text = |sp: Span| {
        let (_, src) = env.sources.file(sp.file)?;
        src.get(sp.start as usize..sp.end as usize)
    };
    for s in programs
        .iter()
        .flat_map(|p| &p.items)
        .filter_map(|i| match i {
            Item::Static(s) => Some(s),
            _ => None,
        })
    {
        let Some(ty) = env.statics.get(&s.name) else {
            continue;
        };
        if ctx.has_trait(ty, sync) {
            continue;
        }
        // `Cell<T>` / `RwLock<T>` around a `T`: the wrapper is replaced,
        // with the argument of its `::new(..)`.
        let wrapped = match ty {
            HirTy::Struct(id) => {
                let d = ctx.struct_def(*id);
                let base = d.name.split("__").next().unwrap_or_default();
                ["rok::core::sync::Cell", "rok::core::sync::RwLock"]
                    .contains(&base)
                    .then(|| d.generic_args.first().cloned())
                    .flatten()
                    .map(|t| (base == "rok::core::sync::Cell", t))
            }
            _ => None,
        };
        let arg = match s.value.as_ref().map(|v| &v.kind) {
            Some(ExprKind::Call { args, .. }) if wrapped.is_some() && args.len() == 1 => {
                match &args[0] {
                    rokc_ast::CallArg::Positional(e) => text(e.span),
                    _ => None,
                }
            }
            _ => s
                .value
                .as_ref()
                .and_then(|v| text(v.span))
                .filter(|_| wrapped.is_none()),
        }
        .unwrap_or("..");
        let name = leaf_name(&s.name);
        let (inner, cell) = match &wrapped {
            Some((cell, t)) => (t, *cell),
            None => (ty, false),
        };
        let atomic = cell && matches!(inner, HirTy::Prim(p) if p.is_int() || *p == PrimTy::Bool);
        let inner_name = ctx.ty_name(inner);
        let help = if atomic {
            format!(
                "help: write `static {name}: Atomic<{inner_name}> = Atomic::new({arg});` and use \
                 `{name}.load()` / `{name}.store(..)`"
            )
        } else {
            format!(
                "help: write `static {name}: Mutex<{inner_name}> = Mutex::new({arg});` and use it \
                 inside `using g = {name}.lock() {{ .. }}`"
            )
        };
        ctx.emit_error_at(
            rokc_diagnostic::ErrorKind::Type,
            s.span,
            format!(
                "a `static` is shared by every thread, so it can't be a `{}`: two threads could \
                 change it at once (spec 5.3)",
                ctx.ty_name(ty)
            ),
        );
        if let Some(d) = env.sink.diagnostics.borrow_mut().last_mut() {
            d.notes.push(help);
        }
    }
}

/// Spec 2.9.3: one impl per trait and type (a concrete impl a generic one
/// covers is a duplicate); the orphan rule (2.9.5); supertraits (2.8.4,
/// for every instance a generic impl's bounds allow).
fn check_impl_rules(
    env: &Env<'_>,
    checked: &[&Program],
    all: &[&Program],
    own: &dyn Fn(&str) -> bool,
) {
    let no_subst = HashMap::new();
    let traits: HashMap<&str, &rokc_ast::TraitDef> = all
        .iter()
        .flat_map(|p| &p.items)
        .filter_map(|it| match it {
            Item::Trait(t) => Some((t.name.as_str(), t)),
            _ => None,
        })
        .collect();
    for item in checked.iter().flat_map(|p| &p.items) {
        let Item::Impl(i) = item else {
            continue;
        };
        let Some(tp) = &i.trait_path else {
            continue;
        };
        let module = impl_module(i);
        if !own(&module) {
            continue;
        }
        let segs: Vec<&str> = tp.segments.iter().map(|s| s.name.as_str()).collect();
        let key = env.names.trait_key(&segs, &module, i.span);
        let pkg = package_of(&module);
        if package_of(&module_of(&key)) != pkg
            && target_package(env, i, &module).as_deref() != Some(pkg)
        {
            env.sink
                .diagnostics
                .borrow_mut()
                .push(rokc_diagnostic::RokDiagnostic::error(
                    rokc_diagnostic::ErrorKind::Type,
                    i.span,
                    format!(
                        "`impl {} for {}`: neither the trait nor the type is defined in this \
                         package (orphan rule, spec 2.9.5)",
                        leaf_name(&key),
                        impl_def_display(&i.self_ty)
                    ),
                ));
            continue;
        }
        if i.negative {
            continue;
        }
        let Some(info) = env.trait_info.get(&key) else {
            continue;
        };
        let mut ctx = LowerCtx::new(env, &no_subst, None, i.span);
        ctx.current_module.clone_from(&module);
        if !i.generics.params.is_empty() {
            let td = traits.get(key.as_str()).copied();
            check_generic_supers(&ctx, i, &module, &key, &info.supers, td);
            continue;
        }
        let target = ctx.resolve_ast_ty(&i.self_ty);
        if target == HirTy::Unknown {
            continue;
        }
        for sup in &info.supers {
            if ctx.has_trait(&target, sup) {
                continue;
            }
            ctx.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "`{}` implements `{}` but not its supertrait `{}` (spec 2.8.4)",
                    ctx.ty_name(&target),
                    leaf_name(&key),
                    leaf_name(sup)
                ),
            );
        }
        if let Some(td) = traits.get(key.as_str()) {
            ctx.self_ty = Some(target);
            check_assoc_bounds(&ctx, td, i);
        }
    }
}

/// Spec 2.8.3: the bounds a trait declares on its associated types
/// (`type Label: Display;`), as `(name, the impl's type, bound key)`.
fn assoc_bound_goals<'i>(
    ctx: &LowerCtx<'_>,
    td: &rokc_ast::TraitDef,
    i: &'i rokc_ast::ImplDef,
) -> Vec<(String, &'i Ty, String)> {
    let trait_module = module_of(&td.name);
    let mut out = Vec::new();
    for item in &td.items {
        let rokc_ast::TraitItem::AssocType { name, bounds, .. } = item else {
            continue;
        };
        let Some(ty) = i.items.iter().find_map(|it| match it {
            AstImplItem::AssocType { name: n, ty } if n == name => Some(ty),
            _ => None,
        }) else {
            continue;
        };
        for b in bounds {
            if let AstTyKind::Path(bp) = &b.kind {
                let segs: Vec<&str> = bp.segments.iter().map(|s| s.name.as_str()).collect();
                let key = ctx.env.names.trait_key(&segs, &trait_module, ctx.here());
                out.push((name.clone(), ty, key));
            }
        }
    }
    out
}

fn assoc_bound_error(ctx: &LowerCtx<'_>, td: &str, name: &str, ty: &str, bound: &str, every: &str) {
    ctx.emit_error(
        rokc_diagnostic::ErrorKind::Type,
        format!(
            "associated type `{name}` = `{ty}` does not implement `{}`{every}, a bound of \
             `{}::{name}` (spec 2.8.3)",
            leaf_name(bound),
            leaf_name(td)
        ),
    );
}

/// A concrete impl's associated types meet their bounds.
fn check_assoc_bounds(ctx: &LowerCtx<'_>, td: &rokc_ast::TraitDef, i: &rokc_ast::ImplDef) {
    for (name, ty, bound) in assoc_bound_goals(ctx, td, i) {
        let t = ctx.resolve_ast_ty(ty);
        if !matches!(t, HirTy::Unknown) && !ctx.has_trait(&t, &bound) {
            assoc_bound_error(ctx, &td.name, &name, &ctx.ty_name(&t), &bound, "");
        }
    }
}

/// Spec 2.8.4 for a generic impl: its target, parameters opaque under their
/// bounds, must implement each supertrait.
fn check_generic_supers(
    ctx: &LowerCtx<'_>,
    i: &rokc_ast::ImplDef,
    module: &str,
    key: &str,
    supers: &[String],
    td: Option<&rokc_ast::TraitDef>,
) {
    let vars: HashMap<String, String> = i
        .generics
        .params
        .iter()
        .map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. }
            | rokc_ast::GenericParam::Const { name, .. } => (name.clone(), format!("#r.{name}")),
        })
        .collect();
    let Some(target) = canon_ast_ty(ctx, module, &vars, &i.self_ty) else {
        return;
    };
    let target = rigid(&target);
    let mut assume: HashMap<String, Vec<String>> = HashMap::new();
    let mut add = |subject: &Ty, bounds: &[Ty]| {
        let AstTyKind::Path(p) = &subject.kind else {
            return;
        };
        let [seg] = p.segments.as_slice() else {
            return;
        };
        let Some(v) = vars.get(&seg.name) else {
            return;
        };
        let have = assume.entry(v.clone()).or_default();
        for b in bounds {
            if let AstTyKind::Path(bp) = &b.kind {
                let segs: Vec<&str> = bp.segments.iter().map(|s| s.name.as_str()).collect();
                have.push(ctx.env.names.trait_key(&segs, module, ctx.here()));
            }
        }
    };
    for gp in &i.generics.params {
        if let rokc_ast::GenericParam::Type { name, bounds, .. } = gp {
            add(&path_ty_named(name, i.span), bounds);
        }
    }
    for w in &i.where_clause {
        add(&w.ty, &w.bounds);
    }
    let params = i
        .generics
        .params
        .iter()
        .map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. }
            | rokc_ast::GenericParam::Const { name, .. } => name.as_str(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    for sup in supers {
        if entails(ctx, &target, sup, &assume, 0) {
            continue;
        }
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "`{}` implements `{}` but not its supertrait `{}` for every `{params}` the \
                 impl allows (spec 2.8.4)",
                target.to_string().replace("#r.", ""),
                leaf_name(key),
                leaf_name(sup),
            ),
        );
    }
    for (name, ty, bound) in td
        .map(|td| assoc_bound_goals(ctx, td, i))
        .unwrap_or_default()
    {
        let Some(t) = canon_ast_ty(ctx, module, &vars, ty) else {
            continue;
        };
        let t = rigid(&t);
        if !entails(ctx, &t, &bound, &assume, 0) {
            let every = format!(" for every `{params}` the impl allows");
            let shown = t.to_string().replace("#r.", "");
            assoc_bound_error(ctx, key, &name, &shown, &bound, &every);
        }
    }
}

/// Impl parameters as opaque named types (`#r.T`), so unification with
/// another impl's target can bind its variables to them but not them.
fn rigid(t: &CanonTy) -> CanonTy {
    match t {
        CanonTy::Var(v) => CanonTy::Named(v.clone(), Vec::new()),
        CanonTy::Named(n, a) => CanonTy::Named(n.clone(), a.iter().map(rigid).collect()),
        CanonTy::Tuple(a) => CanonTy::Tuple(a.iter().map(rigid).collect()),
        CanonTy::Sum(a) => CanonTy::Sum(a.iter().map(rigid).collect()),
        CanonTy::Array(e, n) => CanonTy::Array(Box::new(rigid(e)), Box::new(rigid(n))),
    }
}

fn has_rigid(t: &CanonTy) -> bool {
    match t {
        CanonTy::Var(_) => false,
        CanonTy::Named(n, a) => n.starts_with("#r.") || a.iter().any(has_rigid),
        CanonTy::Tuple(a) | CanonTy::Sum(a) => a.iter().any(has_rigid),
        CanonTy::Array(e, n) => has_rigid(e) || has_rigid(n),
    }
}

/// Does `subject` implement `key` for any opaque `#r.T`s given their bounds
/// `assume`? Opaque types have their bounds and supertraits; others need a
/// unifying impl whose bounds hold. Structural rules and unreadable types
/// are taken as met.
fn entails(
    ctx: &LowerCtx<'_>,
    subject: &CanonTy,
    key: &str,
    assume: &HashMap<String, Vec<String>>,
    depth: u32,
) -> bool {
    if depth > 8
        || key == super::reflect::REFLECT
        || ctx.env.names.auto_traits.contains(key)
        || !has_rigid(subject)
    {
        return true;
    }
    if let CanonTy::Named(n, args) = subject {
        if args.is_empty() && n.starts_with("#r.") {
            let mut stack: Vec<String> = assume.get(n).cloned().unwrap_or_default();
            let mut seen: Vec<String> = Vec::new();
            while let Some(h) = stack.pop() {
                if h == key {
                    return true;
                }
                if let (false, Some(info)) = (seen.contains(&h), ctx.env.trait_info.get(&h)) {
                    stack.extend(info.supers.iter().cloned());
                }
                seen.push(h);
            }
            return false;
        }
    }
    if matches!(subject, CanonTy::Sum(_)) {
        return true;
    }
    ctx.env
        .impls
        .generic()
        .filter(|gi| gi.trait_key == key)
        .any(|gi| {
            let vars: HashMap<String, String> = gi
                .generics
                .params
                .iter()
                .map(|gp| match gp {
                    rokc_ast::GenericParam::Type { name, .. }
                    | rokc_ast::GenericParam::Const { name, .. } => {
                        (name.clone(), format!("#c.{name}"))
                    }
                })
                .collect();
            let Some(target) = canon_ast_ty(ctx, &gi.module, &vars, &gi.self_ty) else {
                return true;
            };
            let mut u = Unifier::default();
            if u.unify(&target, subject).is_none() {
                return false;
            }
            let mut goals: Vec<(CanonTy, &Vec<Ty>)> = Vec::new();
            for gp in &gi.generics.params {
                if let rokc_ast::GenericParam::Type { name, bounds, .. } = gp {
                    goals.push((u.resolve(&vars[name]), bounds));
                }
            }
            for w in &gi.where_clause {
                match canon_ast_ty(ctx, &gi.module, &vars, &w.ty) {
                    Some(t) => goals.push((u.walk(&t), &w.bounds)),
                    None => return true,
                }
            }
            goals.iter().all(|(t, bounds)| {
                bounds.iter().all(|b| {
                    let AstTyKind::Path(bp) = &b.kind else {
                        return true;
                    };
                    let segs: Vec<&str> = bp.segments.iter().map(|s| s.name.as_str()).collect();
                    let k = ctx.env.names.trait_key(&segs, &gi.module, ctx.here());
                    entails(ctx, t, &k, assume, depth + 1)
                })
            })
        })
}

/// The package defining an impl's target type: a struct / enum's own,
/// `rok::core` for a built-in type, `None` for a bare type parameter.
fn target_package(env: &Env<'_>, i: &rokc_ast::ImplDef, module: &str) -> Option<String> {
    if let AstTyKind::Path(p) = &i.self_ty.kind {
        let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
        let param = i.generics.params.iter().any(
            |gp| matches!(gp, rokc_ast::GenericParam::Type { name, .. } if segs == [name.as_str()]),
        );
        if param {
            return None;
        }
        if let Some(full) = env.names.canon(&segs, module, &[], i.span) {
            return Some(package_of(&module_of(&full)).to_owned());
        }
    }
    Some("rok::core".to_owned())
}

fn check_impl_overlap(env: &Env<'_>, own: &dyn Fn(&str) -> bool) {
    let no_subst = HashMap::new();
    let ctx = LowerCtx::new(env, &no_subst, None, Span::DUMMY);
    let impls = &env.impls;
    let mine: Vec<bool> = impls.iter().map(|i| own(&i.module)).collect();
    for (g, gi) in impls.iter().enumerate().filter(|(_, i)| i.is_generic()) {
        ctx.at.set(gi.self_ty.span);
        let concrete = impls.by_trait.get(&gi.trait_key).into_iter().flatten();
        for &c in concrete {
            let ci = &impls.all[c];
            let Some((ty, _)) = &ci.concrete else {
                continue;
            };
            if !mine[g] && !mine[c] {
                continue;
            }
            if ctx
                .generic_impl_subst(gi, ty, 0)
                .is_some_and(|s| trait_args_overlap(&ctx, gi, &s, ty))
            {
                ctx.emit_error_at(
                    rokc_diagnostic::ErrorKind::Type,
                    gi.self_ty.span,
                    format!(
                        "conflicting implementations of trait `{}` for `{}`: the concrete \
                         impl overlaps `impl<..> {} for {}` (spec 2.9.3 - one impl per \
                         trait and type; no specialization)",
                        gi.trait_key,
                        ctx.ty_name(ty),
                        gi.trait_key,
                        impl_def_display(&gi.self_ty)
                    ),
                );
            }
        }
    }
    // Two generic impls conflict when their targets (and trait arguments)
    // unify; bounds don't separate them (`impl<T: A> X for T` and
    // `impl<T: B> X for T` clash). Beyond `symbolic_overlap`, look for a
    // witness among the program's types.
    let gis: Vec<(&Impl, bool)> = impls
        .iter()
        .zip(&mine)
        .filter(|(i, _)| i.is_generic())
        .map(|(i, &m)| (i, m))
        .collect();
    let mut candidates: Option<Vec<HirTy>> = None;
    for (i, &(a, a_mine)) in gis.iter().enumerate() {
        // Inherent ones (empty key) clash when they share a method name.
        let shared = |b: &Impl| {
            let mut m: Vec<&String> = a
                .methods
                .keys()
                .filter(|k| b.methods.contains_key(*k))
                .collect();
            m.sort();
            m.first().map(|s| (*s).clone())
        };
        for &(b, _) in gis.iter().skip(i + 1).filter(|(b, b_mine)| {
            b.trait_key == a.trait_key
                && (a_mine || *b_mine)
                && (!a.trait_key.is_empty() || shared(b).is_some())
        }) {
            ctx.at.set(b.self_ty.span);
            let witness = symbolic_overlap(&ctx, a, b).unwrap_or_else(|| {
                let cands = candidates.get_or_insert_with(|| named_types(env));
                cands
                    .iter()
                    .find(|t| {
                        ctx.generic_impl_subst(a, t, 0).is_some()
                            && ctx.generic_impl_subst(b, t, 0).is_some()
                    })
                    .map(|t| ctx.ty_name(t))
            });
            if let (Some(t), Some(m)) = (
                &witness,
                a.trait_key.is_empty().then(|| shared(b)).flatten(),
            ) {
                ctx.emit_error_at(
                    rokc_diagnostic::ErrorKind::Type,
                    b.self_ty.span,
                    format!(
                        "duplicate method `{m}`: `impl<..> {}` and `impl<..> {}` both define it \
                         for `{t}` (bounds don't make impls disjoint)",
                        impl_def_display(&a.self_ty),
                        impl_def_display(&b.self_ty),
                    ),
                );
            } else if let Some(t) = witness {
                ctx.emit_error_at(
                    rokc_diagnostic::ErrorKind::Type,
                    b.self_ty.span,
                    format!(
                        "conflicting implementations of trait `{}`: `impl<..> {} for {}` and \
                         `impl<..> {} for {}` overlap on `{t}` - bounds don't make impls \
                         disjoint (spec 2.9.5 coherence; no specialization)",
                        a.trait_key,
                        a.trait_key,
                        impl_def_display(&a.self_ty),
                        b.trait_key,
                        impl_def_display(&b.self_ty),
                    ),
                );
            }
        }
    }
}

/// Does generic `gi`, matched on `ty`, also match a concrete impl's trait
/// arguments? A parameter only the trait arguments name is open: assume
/// overlap.
fn trait_args_overlap(
    ctx: &LowerCtx<'_>,
    gi: &Impl,
    subst: &HashMap<String, HirTy>,
    ty: &HirTy,
) -> bool {
    let open = gi.generics.params.iter().any(|gp| match gp {
        rokc_ast::GenericParam::Type { name, .. } | rokc_ast::GenericParam::Const { name, .. } => {
            !subst.contains_key(name)
        }
    });
    if gi.trait_args.is_empty() || open {
        return true;
    }
    let want = ctx.generic_impl_ref(gi, subst);
    if want.args.contains(&HirTy::Unknown) {
        return true;
    }
    ctx.concrete_impls(ty)
        .any(|r| r.path == want.path && r.args == want.args)
}

/// Concrete impl targets and generic struct / enum instantiations: the
/// witnesses for impl targets `symbolic_overlap` can't read.
fn named_types(env: &Env<'_>) -> Vec<HirTy> {
    let mut out: Vec<HirTy> = Vec::new();
    let named = env.impls.all.iter().filter_map(|i| match &i.concrete {
        Some((t, Some(_))) => Some(t.clone()),
        _ => None,
    });
    let structs: Vec<HirTy> = env
        .defs
        .structs
        .borrow()
        .iter()
        .filter(|s| !s.generic_args.is_empty())
        .map(|s| HirTy::Struct(s.id))
        .collect();
    let enums: Vec<HirTy> = env
        .defs
        .enums
        .borrow()
        .iter()
        .filter(|e| !e.generic_args.is_empty())
        .map(|e| HirTy::Enum(e.id))
        .collect();
    for t in named.chain(structs).chain(enums) {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Do two generic impls overlap? Unify targets and trait arguments over
/// impl parameters. Bounds left generic are assumed satisfiable
/// (open-world); bounds made concrete are checked. `Some(Some(shown))`
/// overlaps, `Some(None)` disjoint, `None` unreadable (fn types, aliases).
#[allow(clippy::option_option)] // overlap / disjoint / can't tell
fn symbolic_overlap(ctx: &LowerCtx<'_>, a: &Impl, b: &Impl) -> Option<Option<String>> {
    let names = |gi: &Impl, tag: &str| -> HashMap<String, String> {
        gi.generics
            .params
            .iter()
            .map(|gp| match gp {
                rokc_ast::GenericParam::Type { name, .. }
                | rokc_ast::GenericParam::Const { name, .. } => {
                    (name.clone(), format!("{tag}{name}"))
                }
            })
            .collect()
    };
    let (va, vb) = (names(a, "#a."), names(b, "#b."));
    let canon =
        |gi: &Impl, vars: &HashMap<String, String>, t: &Ty| canon_ast_ty(ctx, &gi.module, vars, t);
    let a_self = canon(a, &va, &a.self_ty)?;
    let b_self = canon(b, &vb, &b.self_ty)?;
    if a.trait_args.len() != b.trait_args.len() {
        return Some(None);
    }
    let mut args = Vec::new();
    for (x, y) in a.trait_args.iter().zip(&b.trait_args) {
        args.push((canon(a, &va, x)?, canon(b, &vb, y)?));
    }
    let mut u = Unifier::default();
    if u.unify(&a_self, &b_self).is_none() || args.iter().any(|(x, y)| u.unify(x, y).is_none()) {
        return Some(None);
    }
    // Negative reasoning only on known constructors (`List<T>: Iterator`
    // fails if no impl targets `List`); a bare variable is open.
    for (gi, vars) in [(a, &va), (b, &vb)] {
        let mut goals: Vec<(CanonTy, &Vec<Ty>)> = Vec::new();
        for gp in &gi.generics.params {
            if let rokc_ast::GenericParam::Type { name, bounds, .. } = gp {
                goals.push((u.resolve(&vars[name]), bounds));
            }
        }
        for w in &gi.where_clause {
            if let Some(t) = canon(gi, vars, &w.ty) {
                goals.push((u.walk(&t), &w.bounds));
            }
        }
        for (subject, bounds) in goals {
            if matches!(subject, CanonTy::Var(_)) {
                continue;
            }
            for bound in bounds {
                let AstTyKind::Path(bp) = &bound.kind else {
                    continue;
                };
                if bp.segments.iter().any(|s| !s.generics.is_empty()) {
                    continue;
                }
                let segs: Vec<&str> = bp.segments.iter().map(|s| s.name.as_str()).collect();
                let key = ctx.env.names.trait_key(&segs, &gi.module, ctx.here());
                if !may_implement(ctx, &subject, &key) {
                    return Some(None);
                }
            }
        }
    }
    Some(Some(u.walk(&a_self).to_string()))
}

/// May non-variable `subject` implement `key`? Concrete types are asked
/// directly; others need an impl targeting their constructor or a bare
/// parameter.
fn may_implement(ctx: &LowerCtx<'_>, subject: &CanonTy, key: &str) -> bool {
    if let Some(t) = canon_hir(ctx, subject) {
        return ctx.has_trait(&t, key);
    }
    let head = canon_head(subject);
    let env = ctx.env;
    let concrete = env.impls.tys_of(key).any(|t| {
        let h = match t {
            HirTy::Struct(id) => enum_base_name(&ctx.struct_def(*id).name).to_owned(),
            HirTy::Enum(id) => enum_base_name(&ctx.enum_def(*id).name).to_owned(),
            HirTy::Tuple(ts) => format!("({})", ts.len()),
            HirTy::Array(..) => "[]".to_owned(),
            HirTy::Sum(_) => "|".to_owned(),
            _ => return false,
        };
        h == head
    });
    concrete
        || env
            .impls
            .generic()
            .filter(|gi| gi.trait_key == key)
            .any(|gi| {
                let vars = gi
                    .generics
                    .params
                    .iter()
                    .filter_map(|gp| match gp {
                        rokc_ast::GenericParam::Type { name, .. } => {
                            Some((name.clone(), format!("#c.{name}")))
                        }
                        rokc_ast::GenericParam::Const { .. } => None,
                    })
                    .collect();
                match canon_ast_ty(ctx, &gi.module, &vars, &gi.self_ty) {
                    Some(CanonTy::Var(_)) | None => true,
                    Some(t) => canon_head(&t) == head,
                }
            })
}

/// A type's constructor for `may_implement`: a path's name, `(n)` for an
/// n-tuple, `[]`, `|`.
fn canon_head(t: &CanonTy) -> String {
    match t {
        CanonTy::Named(n, _) => n.clone(),
        CanonTy::Tuple(ts) => format!("({})", ts.len()),
        CanonTy::Array(..) => "[]".to_owned(),
        CanonTy::Sum(_) => "|".to_owned(),
        CanonTy::Var(_) => String::new(),
    }
}

/// A type for `symbolic_overlap`: paths by canonical name, impl
/// parameters as variables.
#[derive(Clone, Debug, PartialEq)]
enum CanonTy {
    Var(String),
    Named(String, Vec<CanonTy>),
    Tuple(Vec<CanonTy>),
    /// Element and length (a literal `Named` or a const parameter).
    Array(Box<CanonTy>, Box<CanonTy>),
    Sum(Vec<CanonTy>),
}

fn canon_ast_ty(
    ctx: &LowerCtx<'_>,
    module: &str,
    vars: &HashMap<String, String>,
    t: &Ty,
) -> Option<CanonTy> {
    let rec = |t: &Ty| canon_ast_ty(ctx, module, vars, t);
    Some(match &t.kind {
        AstTyKind::Path(p) => {
            let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
            if let ([one], true) = (segs.as_slice(), p.segments[0].generics.is_empty()) {
                if let Some(v) = vars.get(*one) {
                    return Some(CanonTy::Var(v.clone()));
                }
            }
            let names = &ctx.env.names;
            if let Some(full) = names.plain_alias(t, module, ctx.here()) {
                let body = names.plain_alias_ty(&full)?;
                return canon_ast_ty(ctx, &module_of(&full), &HashMap::new(), body);
            }
            let full = names.canon(&segs, module, &[], ctx.here());
            if full
                .as_ref()
                .is_some_and(|f| names.generic_alias(f).is_some())
            {
                return None;
            }
            let name = full.unwrap_or_else(|| segs.join("::"));
            let args = p
                .segments
                .last()
                .map_or(Some(Vec::new()), |s| s.generics.iter().map(rec).collect())?;
            CanonTy::Named(name, args)
        }
        AstTyKind::Tuple(ts) => CanonTy::Tuple(ts.iter().map(rec).collect::<Option<_>>()?),
        AstTyKind::Sum(ts) => {
            // Written sums flatten (2.6.4; a nested layer `Nest` stays
            // one member); duplicates collapse.
            let mut set: Vec<CanonTy> = Vec::new();
            for t in ts {
                let members = match (rec(t)?, &t.kind) {
                    (CanonTy::Sum(inner), k) if !matches!(k, AstTyKind::Nest(_)) => inner,
                    (one, _) => vec![one],
                };
                for m in members {
                    if !set.contains(&m) {
                        set.push(m);
                    }
                }
            }
            CanonTy::Sum(set)
        }
        AstTyKind::Nest(x) => rec(x)?,
        AstTyKind::None => CanonTy::Named("none".to_owned(), Vec::new()),
        AstTyKind::Never => CanonTy::Named("!".to_owned(), Vec::new()),
        AstTyKind::Any => CanonTy::Named("any".to_owned(), Vec::new()),
        AstTyKind::AnyTrait(bounds) => {
            let mut keys: Vec<String> = bounds
                .iter()
                .map(|b| {
                    let segs: Vec<&str> = b.segments.iter().map(|s| s.name.as_str()).collect();
                    ctx.env.names.trait_key(&segs, module, ctx.here())
                })
                .collect();
            if bounds
                .iter()
                .any(|b| b.segments.iter().any(|s| !s.generics.is_empty()))
            {
                return None;
            }
            keys.sort();
            CanonTy::Named(format!("any {}", keys.join(" + ")), Vec::new())
        }
        // `fn(A) -> R`: kind and ABI name the constructor, return then
        // parameters are its arguments; `+ Send` doesn't separate.
        AstTyKind::Fn(params, ret, b) => {
            let stdcall = b.system && super::const_eval::stdcall_system();
            let kind = format!("fn/{}/{}/{stdcall}", b.unsafe_fn, b.c_abi);
            let args = std::iter::once(&**ret)
                .chain(params)
                .map(rec)
                .collect::<Option<_>>()?;
            CanonTy::Named(kind, args)
        }
        AstTyKind::Throws { ok, err } => {
            CanonTy::Named("throws".to_owned(), vec![rec(ok)?, rec(err)?])
        }
        AstTyKind::Array(e, rokc_ast::ConstExpr::Literal(rokc_ast::Literal::Int(n))) => {
            CanonTy::Array(
                Box::new(rec(e)?),
                Box::new(CanonTy::Named(n.clone(), Vec::new())),
            )
        }
        AstTyKind::Array(e, len) => {
            let p = match len {
                rokc_ast::ConstExpr::Path(p) => p,
                rokc_ast::ConstExpr::Expr(x) => match &x.kind {
                    ExprKind::Path(p) => p,
                    _ => return None,
                },
                rokc_ast::ConstExpr::Literal(_) => return None,
            };
            let [seg] = p.segments.as_slice() else {
                return None;
            };
            let v = vars.get(&seg.name)?;
            CanonTy::Array(Box::new(rec(e)?), Box::new(CanonTy::Var(v.clone())))
        }
        _ => return None,
    })
}

/// A concrete `CanonTy` as a type: a primitive or a non-generic struct
/// / enum; `None` for anything else (the bound check gives up).
fn canon_hir(ctx: &LowerCtx<'_>, t: &CanonTy) -> Option<HirTy> {
    let CanonTy::Named(name, args) = t else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    match crate::ty::resolve_ty(&Ty {
        kind: AstTyKind::Path(AstPath {
            segments: vec![rokc_ast::PathSegment {
                name: name.clone(),
                generics: Vec::new(),
            }],
            span: rokc_diagnostic::Span::default(),
        }),
        span: rokc_diagnostic::Span::default(),
    }) {
        p @ HirTy::Prim(_) => Some(p),
        _ => ctx
            .lookup_struct(name)
            .map(HirTy::Struct)
            .or_else(|| ctx.lookup_enum(name).map(HirTy::Enum)),
    }
}

impl std::fmt::Display for CanonTy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list =
            |ts: &[Self], sep: &str| ts.iter().map(Self::to_string).collect::<Vec<_>>().join(sep);
        match self {
            Self::Var(v) => write!(f, "{}", v.rsplit('.').next().unwrap_or(v)),
            Self::Named(n, args) if n.starts_with("fn/") => {
                let ret = args.first().map_or_else(String::new, Self::to_string);
                write!(
                    f,
                    "fn({}) -> {ret}",
                    list(args.get(1..).unwrap_or_default(), ", ")
                )
            }
            Self::Named(n, args) if n == "throws" && args.len() == 2 => {
                write!(f, "throws ({}) {}", args[1], args[0])
            }
            Self::Named(n, args) if args.is_empty() => {
                write!(f, "{}", n.rsplit("::").next().unwrap_or(n))
            }
            Self::Named(n, args) => {
                write!(
                    f,
                    "{}<{}>",
                    n.rsplit("::").next().unwrap_or(n),
                    list(args, ", ")
                )
            }
            Self::Tuple(ts) => write!(f, "({})", list(ts, ", ")),
            Self::Sum(ts) => write!(f, "{}", list(ts, " | ")),
            Self::Array(e, n) => write!(f, "[{e}; {n}]"),
        }
    }
}

/// First-order unification over `CanonTy` (no occurs check: the two
/// impls' variables are distinct).
#[derive(Default)]
struct Unifier {
    sub: HashMap<String, CanonTy>,
}

impl Unifier {
    fn resolve(&self, v: &str) -> CanonTy {
        match self.sub.get(v) {
            Some(CanonTy::Var(w)) => self.resolve(w),
            Some(t) => self.walk(t),
            None => CanonTy::Var(v.to_owned()),
        }
    }

    fn walk(&self, t: &CanonTy) -> CanonTy {
        match t {
            CanonTy::Var(v) => self.resolve(v),
            CanonTy::Named(n, args) => {
                CanonTy::Named(n.clone(), args.iter().map(|a| self.walk(a)).collect())
            }
            CanonTy::Tuple(ts) => CanonTy::Tuple(ts.iter().map(|a| self.walk(a)).collect()),
            CanonTy::Sum(ts) => CanonTy::Sum(ts.iter().map(|a| self.walk(a)).collect()),
            CanonTy::Array(e, n) => CanonTy::Array(Box::new(self.walk(e)), Box::new(self.walk(n))),
        }
    }

    #[allow(clippy::many_single_char_names)]
    fn unify(&mut self, x: &CanonTy, y: &CanonTy) -> Option<()> {
        match (self.walk(x), self.walk(y)) {
            (CanonTy::Var(v), CanonTy::Var(w)) if v == w => Some(()),
            // No cyclic binding (`T` against `List<T>`): no overlap.
            (CanonTy::Var(v), t) | (t, CanonTy::Var(v)) => {
                if occurs(&v, &t) {
                    return None;
                }
                self.sub.insert(v, t);
                Some(())
            }
            (CanonTy::Named(n, xs), CanonTy::Named(m, ys)) if n == m && xs.len() == ys.len() => {
                xs.iter().zip(&ys).try_for_each(|(p, q)| self.unify(p, q))
            }
            (CanonTy::Tuple(xs), CanonTy::Tuple(ys)) if xs.len() == ys.len() => {
                xs.iter().zip(&ys).try_for_each(|(p, q)| self.unify(p, q))
            }
            (CanonTy::Array(p, n), CanonTy::Array(q, m)) => {
                self.unify(&n, &m)?;
                self.unify(&p, &q)
            }
            (CanonTy::Sum(xs), CanonTy::Sum(ys)) if xs.len() == ys.len() => {
                self.unify_set(&xs, &ys)
            }
            _ => None,
        }
    }

    /// Sums are sets (2.6.4): some pairing of the members unifies.
    fn unify_set(&mut self, xs: &[CanonTy], ys: &[CanonTy]) -> Option<()> {
        let Some((x, rest)) = xs.split_first() else {
            return Some(());
        };
        for k in 0..ys.len() {
            let saved = self.sub.clone();
            let mut others = ys.to_vec();
            let y = others.remove(k);
            if self.unify(x, &y).is_some() && self.unify_set(rest, &others).is_some() {
                return Some(());
            }
            self.sub = saved;
        }
        None
    }
}

/// Does variable `v` occur in `t` (already walked)?
fn occurs(v: &str, t: &CanonTy) -> bool {
    match t {
        CanonTy::Var(w) => w == v,
        CanonTy::Named(_, ts) | CanonTy::Tuple(ts) | CanonTy::Sum(ts) => {
            ts.iter().any(|t| occurs(v, t))
        }
        CanonTy::Array(e, n) => occurs(v, e) || occurs(v, n),
    }
}

/// Auto trait impls: opt in with `unsafe impl`, out with `impl !Tr for S`
/// (recorded in `neg`); both only for auto traits. Whether to register the
/// impl normally (false for opt-outs and errors).
fn check_auto_impl(
    impl_def: &rokc_ast::ImplDef,
    names: &TypeNames,
    module: &str,
    neg: &mut std::collections::HashSet<(String, String)>,
    diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) -> bool {
    let err = |d: &mut Vec<rokc_diagnostic::RokDiagnostic>, msg: String| {
        d.push(rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Type,
            impl_def.span,
            msg,
        ));
    };
    let Some(tp) = &impl_def.trait_path else {
        if impl_def.is_unsafe || impl_def.negative {
            err(diags, "`unsafe impl` / `impl !` needs a trait".to_owned());
        }
        return true;
    };
    let segs: Vec<&str> = tp.segments.iter().map(|s| s.name.as_str()).collect();
    let key = names.trait_key(&segs, module, impl_def.span);
    let target = impl_def_display(&impl_def.self_ty);
    let tr = segs.join("::");
    if key == super::reflect::REFLECT {
        err(
            diags,
            format!(
                "`Reflect` is implemented by the compiler for every type; remove `impl {tr} for \
                 {target}` (spec 6.10.1)"
            ),
        );
        return false;
    }
    if !names.auto_traits.contains(&key) {
        if impl_def.is_unsafe || impl_def.negative {
            err(
                diags,
                format!(
                    "`{tr}` is not an auto trait - `unsafe impl` and `impl !Trait` are only \
                     for `Send` / `Sync`"
                ),
            );
            return false;
        }
        return true;
    }
    if impl_def.negative {
        let base = match &impl_def.self_ty.kind {
            AstTyKind::Path(p) => {
                let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
                names.canon(&segs, module, &[], impl_def.span)
            }
            _ => None,
        };
        match base {
            Some(b) if impl_def.items.is_empty() && !impl_def.is_unsafe => {
                neg.insert((b, key));
            }
            Some(_) => err(
                diags,
                format!("`impl !{tr} for {target}` takes no `unsafe` and has an empty body"),
            ),
            None => err(
                diags,
                format!("`impl !{tr}` target `{target}` must be a struct or enum"),
            ),
        }
        return false;
    }
    if !impl_def.is_unsafe {
        err(
            diags,
            format!(
                "`{tr}` is an auto trait the compiler derives from the fields; a manual \
                 impl for `{target}` must be `unsafe impl {tr} for {target} {{}}`"
            ),
        );
        return false;
    }
    true
}

/// Spec 5.9.10: an `extern "C" fn` definition is called from C: no
/// captures, generics or `throws` (errors can't cross into C), FFI types
/// only.
fn check_extern_fn(f: &FnDef, abi: &str, diags: &mut Vec<rokc_diagnostic::RokDiagnostic>) {
    let mut bad = |msg: String| {
        diags.push(rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Type,
            f.span,
            format!(
                "`extern {abi:?} fn {}`: {msg} (spec 5.9.10)",
                leaf_name(&f.name)
            ),
        ));
    };
    if abi != "C" && abi != "system" {
        bad("unsupported ABI (`\"C\"` or `\"system\"`)".to_owned());
    }
    if !matches!(f.throws, rokc_ast::ThrowsKind::None) {
        bad("cannot `throws` - an error cannot cross into C".to_owned());
    }
    if !f.generics.params.is_empty() {
        bad("cannot be generic".to_owned());
    }
    if f.is_async || f.is_generator {
        bad("cannot be `async` / a generator".to_owned());
    }
    let is_str = |t: &rokc_ast::Ty| matches!(&t.kind, AstTyKind::Path(p) if p.segments.len() == 1 && p.segments[0].name == "str");
    if f.params
        .iter()
        .filter_map(|p| p.ty.as_ref())
        .chain(f.ret_ty.as_ref())
        .any(is_str)
    {
        bad("`str` is not an FFI type; pass a `Ptr<u8>` and a length".to_owned());
    }
}

/// `#[link(wasm_import_module = "m")]` on an `extern` block (spec
/// 6.2.3): the core module its functions are imported from on wasm32.
fn wasm_import_module(attrs: &[rokc_ast::Attribute]) -> Option<String> {
    attrs
        .iter()
        .filter(|a| a.path == ["link"])
        .find_map(|a| match &a.body {
            rokc_ast::AttrBody::Args(args) => args.iter().find_map(|x| match x {
                rokc_ast::AttrArg::KeyValue(k, rokc_ast::Literal::Str(m))
                    if k == "wasm_import_module" =>
                {
                    Some(m.clone())
                }
                _ => None,
            }),
            _ => None,
        })
}

/// `#[link_name = "sym"]` on an extern function (spec 5.9.9): the symbol
/// it links to; in a `wasm_import_module` block, its import name.
fn link_name_of(f: &FnDef) -> Option<String> {
    let a = f.attrs.iter().find(|a| a.path == ["link_name"])?;
    match &a.body {
        rokc_ast::AttrBody::Value(rokc_ast::Literal::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `#[export_name("sym")]` (spec 905): the symbol it names.
fn export_name(f: &FnDef) -> Option<String> {
    let a = f.attrs.iter().find(|a| a.name() == "export_name")?;
    match &a.body {
        rokc_ast::AttrBody::Args(args) => args.iter().find_map(|x| match x {
            rokc_ast::AttrArg::Lit(rokc_ast::Literal::Str(s)) => Some(s.clone()),
            _ => None,
        }),
        _ => None,
    }
}

/// What a `#[intrinsic("rok.NAME")]` function is (stdlib spec 0.3): its
/// calls become the operation; the body is only a fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Intrinsic {
    /// `rok.OP.TYPE` on a primitive's method.
    Prim(PrimOp),
    /// `rok.cast.FROM.TO` / `rok.clone.copy`: the operand as the return type.
    Convert,
    BufNew,
    BufGet,
    BufSet,
    BufSame,
    BufErase,
    /// `rok.buf_cast`: the reference as a `GcBuf` of the expected type.
    BufCast,
    PtrRead {
        volatile: bool,
    },
    PtrWrite {
        volatile: bool,
    },
    SizeOf,
    AlignOf,
    /// `rok.addr_of`: a pointer (the declared return type's) to the
    /// argument's place, or to a `ref self` receiver's.
    AddrOf,
    /// `rok.drop_in_place`: run the drop glue of the `T` a `Ptr<T>`
    /// points at.
    DropInPlace,
    /// `rok.atomic.OP`: the op `HirExprKind::BufAtomic` carries.
    Atomic(&'static str),
    AnyCast,
    MethodTable,
    MlirOp,
    MlirAttr,
    /// `rok.transfer`: a hand-off copy of the value (stdlib 2.13.9).
    Transfer,
    /// `rok.transfer_keep`: note every collection reached as its own copy.
    TransferKeep,
    /// `rok.transfer_visit`: note every collection reached for the memo's
    /// current owner (`par`'s distinct items).
    TransferVisit,
    /// `rok.transfer_needed`: whether `rok.transfer` copies anything.
    TransferNeeded,
    /// `rok.transfer_plain`: whether a type holds no GC reference.
    TransferPlain,
    /// `rok.transfer_flat`: one listed handle whose elements need no copy.
    TransferFlat,
    /// `rok.hand_over`: the value itself when `rok.transfer` would copy
    /// nothing, else a call of `marker::hand_over_copy`.
    HandOver,
}

impl Intrinsic {
    fn parse(name: &str) -> Option<Self> {
        let rest = name.strip_prefix("rok.")?;
        Some(match rest {
            "buf_new" => Self::BufNew,
            "buf_get" => Self::BufGet,
            "buf_set" => Self::BufSet,
            "buf_same" => Self::BufSame,
            "buf_erase" => Self::BufErase,
            "buf_cast" => Self::BufCast,
            "clone.copy" => Self::Convert,
            "ptr.read" => Self::PtrRead { volatile: false },
            "ptr.write" => Self::PtrWrite { volatile: false },
            "ptr.read_volatile" => Self::PtrRead { volatile: true },
            "ptr.write_volatile" => Self::PtrWrite { volatile: true },
            "size_of" => Self::SizeOf,
            "align_of" => Self::AlignOf,
            "addr_of" => Self::AddrOf,
            "drop_in_place" => Self::DropInPlace,
            "any_cast" => Self::AnyCast,
            "method_table" => Self::MethodTable,
            "mlir.op" => Self::MlirOp,
            "mlir.attr" => Self::MlirAttr,
            "transfer" => Self::Transfer,
            "transfer_keep" => Self::TransferKeep,
            "transfer_visit" => Self::TransferVisit,
            "transfer_needed" => Self::TransferNeeded,
            "transfer_plain" => Self::TransferPlain,
            "transfer_flat" => Self::TransferFlat,
            "hand_over" => Self::HandOver,
            _ => {
                if let Some(tys) = rest.strip_prefix("cast.") {
                    return (tys.split('.').count() == 2).then_some(Self::Convert);
                }
                if let Some(op) = rest.strip_prefix("atomic.") {
                    return ["load", "xchg", "add", "sub", "and", "or", "xor", "cas"]
                        .into_iter()
                        .find(|o| *o == op)
                        .map(Self::Atomic);
                }
                // `rok.OP_assign.TYPE` (stdlib 2.4.6): the op, written
                // back through the `ref self` receiver.
                let op = rest.split('.').next()?;
                Self::Prim(match op.strip_suffix("_assign").unwrap_or(op) {
                    "add" => PrimOp::Add,
                    "sub" => PrimOp::Sub,
                    "mul" => PrimOp::Mul,
                    "div" => PrimOp::Div,
                    "rem" => PrimOp::Rem,
                    "mod" => PrimOp::Mod,
                    "and" => PrimOp::BitAnd,
                    "or" => PrimOp::BitOr,
                    "xor" => PrimOp::BitXor,
                    "shl" => PrimOp::Shl,
                    "shr" => PrimOp::Shr,
                    "eq" => PrimOp::Eq,
                    "ne" => PrimOp::Ne,
                    "lt" => PrimOp::Lt,
                    "le" => PrimOp::Le,
                    "gt" => PrimOp::Gt,
                    "ge" => PrimOp::Ge,
                    "neg" => PrimOp::Neg,
                    // Bitwise on integers, logical on `bool` (picked at the call).
                    "not" => PrimOp::Not,
                    "sqrt" => PrimOp::Sqrt,
                    "wrapping_add" => PrimOp::WrappingAdd,
                    "wrapping_sub" => PrimOp::WrappingSub,
                    "wrapping_mul" => PrimOp::WrappingMul,
                    "ctpop" => PrimOp::CountOnes,
                    "ctlz" => PrimOp::LeadingZeros,
                    "cttz" => PrimOp::TrailingZeros,
                    "bswap" => PrimOp::SwapBytes,
                    _ => return None,
                })
            }
        })
    }
}

/// `m`'s `#[intrinsic(..)]`, standard library only (stdlib 0.3.5).
pub(super) fn intrinsic_of(
    m: &FnDef,
    module: &str,
    diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) -> Option<Intrinsic> {
    let attr = m.attrs.iter().find(|a| a.name() == "intrinsic")?;
    let mut err = |msg: String| {
        diags.push(rokc_diagnostic::RokDiagnostic::error(
            rokc_diagnostic::ErrorKind::Meta,
            m.span,
            msg,
        ));
        None
    };
    if !module.starts_with("rok::") {
        return err(format!(
            "`#[intrinsic]` on `{}`: the intrinsic attribute is for the standard library only \
             (stdlib spec 0.3.5)",
            m.name
        ));
    }
    let name = match &attr.body {
        rokc_ast::AttrBody::Args(args) => args.iter().find_map(|a| match a {
            rokc_ast::AttrArg::Lit(rokc_ast::Literal::Str(s)) => Some(s.clone()),
            _ => None,
        }),
        _ => None,
    };
    let Some(intrinsic) = name.as_deref().and_then(Intrinsic::parse) else {
        return err(format!(
            "`#[intrinsic({})]` on `{}`: unknown intrinsic - expected `rok.OP.TYPE` \
             (stdlib spec 0.3.3)",
            name.map_or_else(String::new, |n| format!("\"{n}\"")),
            m.name
        ));
    };
    // A primitive op is a concrete method; the others take their types
    // from a generic function's arguments.
    if intrinsic == Intrinsic::Convert {
        if !m.generics.params.is_empty() || m.params.len() != 1 {
            return err(format!(
                "`#[intrinsic]` on `{}`: a conversion is a non-generic method of one operand",
                m.name
            ));
        }
        return Some(intrinsic);
    }
    let Intrinsic::Prim(op) = intrinsic else {
        if m.generics.params.is_empty() {
            return err(format!(
                "`#[intrinsic]` on `{}`: this intrinsic is a generic function",
                m.name
            ));
        }
        return Some(intrinsic);
    };
    if !m.generics.params.is_empty() {
        return err(format!(
            "`#[intrinsic]` on `{}`: a primitive op is not generic",
            m.name
        ));
    }
    let arity = if matches!(
        op,
        PrimOp::Neg
            | PrimOp::Not
            | PrimOp::Sqrt
            | PrimOp::CountOnes
            | PrimOp::LeadingZeros
            | PrimOp::TrailingZeros
            | PrimOp::SwapBytes
    ) {
        1
    } else {
        2
    };
    if m.params.len() != arity {
        return err(format!(
            "`#[intrinsic]` on `{}`: the op takes {arity} operand(s)",
            m.name
        ));
    }
    let assign = name
        .as_deref()
        .is_some_and(|n| n.split('.').nth(1).is_some_and(|o| o.ends_with("_assign")));
    if assign != (arity == 2 && m.params[0].by_ref) {
        return err(format!(
            "`#[intrinsic]` on `{}`: a `rok.OP_assign.TYPE` op takes `ref self`, and only it",
            m.name
        ));
    }
    Some(intrinsic)
}

/// An impl's associated types: `type Item = T;` → `Item ↦ T`.
fn impl_assoc_types(impl_def: &rokc_ast::ImplDef) -> HashMap<String, Ty> {
    impl_def
        .items
        .iter()
        .filter_map(|i| match i {
            AstImplItem::AssocType { name, ty } => Some((name.clone(), ty.clone())),
            _ => None,
        })
        .collect()
}

/// A trait const argument: a literal, a `const` (from `module`, then the
/// root) or a braced const expression over those.
fn const_arg_value(t: &Ty, names: &TypeNames, module: &str) -> Option<i128> {
    let c = match &t.kind {
        AstTyKind::Const(c) => c.clone(),
        AstTyKind::Path(p) => rokc_ast::ConstExpr::Path(p.clone()),
        _ => return None,
    };
    names.const_int(&c, module, &|_| None)
}

/// `v` as a const type argument.
fn const_literal_ty(v: i128, span: rokc_diagnostic::Span) -> Ty {
    let lit = Expr {
        kind: ExprKind::Literal(rokc_ast::Literal::Int(v.unsigned_abs().to_string())),
        span,
    };
    let e = if v < 0 {
        Expr {
            kind: ExprKind::OpPrefix {
                op: "-".to_owned(),
                operand: Box::new(lit),
            },
            span,
        }
    } else {
        lit
    };
    Ty {
        kind: AstTyKind::Const(rokc_ast::ConstExpr::Expr(Box::new(e))),
        span,
    }
}

/// Trait type parameter → the impl's argument (or the parameter's
/// default, `Self` meaning the impl target; spec 2.10.5).
fn trait_param_args(t: &rokc_ast::TraitDef, impl_def: &rokc_ast::ImplDef) -> HashMap<String, Ty> {
    let given: &[Ty] = impl_def
        .trait_path
        .as_ref()
        .and_then(|tp| tp.segments.last())
        .map_or(&[], |s| s.generics.as_slice());
    let mut out = HashMap::new();
    for (i, gp) in t.generics.params.iter().enumerate() {
        let (name, default) = match gp {
            rokc_ast::GenericParam::Type { name, default, .. } => (name, default),
            rokc_ast::GenericParam::Const { name, .. } => (name, &None),
        };
        let ty = given.get(i).cloned().or_else(|| {
            default.as_ref().map(|d| match &d.kind {
                AstTyKind::Path(p) if p.segments.len() == 1 && p.segments[0].name == "Self" => {
                    impl_def.self_ty.clone()
                }
                _ => d.clone(),
            })
        });
        if let Some(ty) = ty {
            out.insert(name.clone(), ty);
        }
    }
    out
}

/// `Self::X` from a supertrait in a default's signature becomes the
/// projection on the impl's target (`ListIter<T>::Item`).
fn supertrait_projections(
    m: &FnDef,
    t: &rokc_ast::TraitDef,
    impl_def: &rokc_ast::ImplDef,
    args: &mut HashMap<String, Ty>,
) {
    let AstTyKind::Path(target) = &impl_def.self_ty.kind else {
        return;
    };
    let mut needs = Vec::new();
    let tys = m.params.iter().filter_map(|p| p.ty.as_ref());
    for ty in tys.chain(m.ret_ty.as_ref()) {
        ast_ty_needs(ty, &[], &[], &mut needs);
    }
    let own = trait_assoc_names(t);
    for x in needs.iter().filter_map(|n| n.strip_prefix("Self::")) {
        if args.contains_key(x) || own.iter().any(|o| o == x) {
            continue;
        }
        let mut p = target.clone();
        p.segments.push(rokc_ast::PathSegment {
            name: x.to_owned(),
            generics: Vec::new(),
        });
        let ty = Ty {
            kind: AstTyKind::Path(p),
            span: impl_def.self_ty.span,
        };
        args.insert(x.to_owned(), ty);
    }
}

fn trait_assoc_names(t: &rokc_ast::TraitDef) -> Vec<String> {
    t.items
        .iter()
        .filter_map(|i| match i {
            rokc_ast::TraitItem::AssocType { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Does `m`'s signature name one of `assoc` (bare or `Self::X`)?
fn sig_mentions(m: &FnDef, assoc: &[String]) -> bool {
    let params: Vec<&str> = Vec::new();
    let names: Vec<&str> = assoc.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for p in &m.params {
        if let Some(t) = &p.ty {
            ast_ty_needs(t, &params, &names, &mut out);
        }
    }
    ast_ty_needs(&sig_ret_ast(m), &params, &names, &mut out);
    out.iter().any(|n| n.starts_with("Self::"))
}

/// Does `m` return a generic over `Self` (`Flatten<Self>`)? Such a default
/// is instantiated only when called, since the adapter may not fit every
/// implementor.
fn returns_self_generic(m: &FnDef) -> bool {
    matches!(&sig_ret_ast(m).kind, AstTyKind::Path(p)
        if p.segments.iter().any(|s| s.generics.iter().any(|g| matches!(g.kind, AstTyKind::SelfUpper))))
}

/// Replace a default method's associated-type names (`Item`,
/// `Self::Item`) in its signature with the impl's types.
fn path_ty_named(name: &str, span: rokc_diagnostic::Span) -> Ty {
    Ty {
        kind: AstTyKind::Path(rokc_ast::Path {
            segments: vec![rokc_ast::PathSegment {
                name: name.to_owned(),
                generics: Vec::new(),
            }],
            span,
        }),
        span,
    }
}

/// Spec 2.6.4 / 2.8.3 / 2.13.4: in a trait or impl, `X` and `Self::X` name
/// the associated type, shadowing outer `X`. Impl signatures get the
/// impl's type; methods are marked `ASSOC_TYPES_ATTR` so bodies read `X`
/// as `Self::X`.
fn add_self_assoc_sigs(program: &mut Program) {
    let mark = |d: &mut FnDef, names: Vec<String>| {
        d.attrs.push(rokc_ast::Attribute {
            path: std::iter::once(crate::ASSOC_TYPES_ATTR.to_owned())
                .chain(names)
                .collect(),
            body: rokc_ast::AttrBody::default(),
            inner: false,
            span: d.span,
        });
    };
    for it in &mut program.items {
        match it {
            Item::Impl(i) => {
                let assoc = impl_assoc_types(i);
                if assoc.is_empty() {
                    continue;
                }
                for m in &mut i.items {
                    if let AstImplItem::Method(d) = m {
                        for prm in &mut d.params {
                            if let Some(t) = &mut prm.ty {
                                subst_assoc_ty(t, &assoc);
                            }
                        }
                        if let Some(t) = &mut d.ret_ty {
                            subst_assoc_ty(t, &assoc);
                        }
                        if let rokc_ast::ThrowsKind::Typed(ts) = &mut d.throws {
                            ts.iter_mut().for_each(|t| subst_assoc_ty(t, &assoc));
                        }
                        let mut names: Vec<String> = assoc.keys().cloned().collect();
                        names.sort_unstable();
                        mark(d, names);
                    }
                }
            }
            Item::Trait(t) => {
                let names = trait_assoc_names(t);
                if names.is_empty() {
                    continue;
                }
                for ti in &mut t.items {
                    if let rokc_ast::TraitItem::Method(d) = ti {
                        mark(d, names.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Associated type defaults: an impl omitting `type X` gets the trait's,
/// resolved in the trait's module with its parameters replaced. Returns
/// the additions by item index.
fn assoc_defaults(
    program: &Program,
    names: &TypeNames,
    base: &HashMap<String, &rokc_ast::TraitDef>,
) -> Vec<(usize, Vec<AstImplItem>)> {
    let traits: HashMap<&str, &rokc_ast::TraitDef> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Trait(t) => Some((t.name.as_str(), t)),
            _ => None,
        })
        .chain(base.iter().map(|(k, t)| (k.as_str(), *t)))
        .filter(|(_, t)| {
            t.items.iter().any(|i| {
                matches!(
                    i,
                    rokc_ast::TraitItem::AssocType {
                        default: Some(_),
                        ..
                    }
                )
            })
        })
        .collect();
    let mut out = Vec::new();
    if traits.is_empty() {
        return out;
    }
    for (idx, it) in program.items.iter().enumerate() {
        let Item::Impl(i) = it else { continue };
        let Some(tp) = &i.trait_path else { continue };
        let module = impl_module(i);
        let segs: Vec<&str> = tp.segments.iter().map(|s| s.name.as_str()).collect();
        let Some(td) = traits.get(names.trait_key(&segs, &module, i.span).as_str()) else {
            continue;
        };
        let have = impl_assoc_types(i);
        let params = trait_param_args(td, i);
        let mut added = Vec::new();
        for ti in &td.items {
            let rokc_ast::TraitItem::AssocType {
                name,
                default: Some(d),
                ..
            } = ti
            else {
                continue;
            };
            if have.contains_key(name) {
                continue;
            }
            let mut ty = d.clone();
            names.absolutize(&mut ty, &module_of(&td.name), &module, &params, i.span);
            subst_assoc_ty(&mut ty, &params);
            added.push(AstImplItem::AssocType {
                name: name.clone(),
                ty,
            });
        }
        if !added.is_empty() {
            out.push((idx, added));
        }
    }
    out
}

/// Bodies without `-> T` that may diverge on every path (spec 4.3.7), by
/// span, so their return type is inferred (`!` if lowering confirms): each
/// ends in `throw`, a `loop`, a call of a known diverging name, or
/// branches that all do. Names match by leaf (lowering settles it);
/// `names` accumulates across stages.
fn may_diverge(
    program: &Program,
    names: &mut std::collections::HashSet<String>,
) -> std::collections::HashSet<rokc_diagnostic::Span> {
    use std::collections::HashSet;
    fn tail_block(b: &Block, d: &HashSet<String>) -> bool {
        if let Some(t) = &b.trailing {
            return tail(t, d);
        }
        b.stmts.last().is_some_and(|s| match &s.kind {
            StmtKind::Throw(_) => true,
            StmtKind::Expr(e) => tail(e, d),
            _ => false,
        })
    }
    // A `loop` with no `break` / `return` in it (outside closures).
    fn endless(body: &Block) -> bool {
        struct V {
            depth: u32,
            exits: bool,
        }
        impl crate::walk::AstVisitor for V {
            fn stmt(&mut self, s: &rokc_ast::Stmt) {
                if self.depth == 0 && matches!(s.kind, StmtKind::Break { .. } | StmtKind::Return(_))
                {
                    self.exits = true;
                }
            }
            fn enter_body(&mut self) {
                self.depth += 1;
            }
            fn leave_body(&mut self) {
                self.depth -= 1;
            }
        }
        let mut v = V {
            depth: 0,
            exits: false,
        };
        crate::walk::walk_block(&mut v, body);
        !v.exits
    }
    fn tail(e: &Expr, d: &HashSet<String>) -> bool {
        match &e.kind {
            ExprKind::Loop { body, .. } => endless(body),
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Path(p) => p.segments.last().is_some_and(|s| d.contains(&s.name)),
                ExprKind::QPath(q) => d.contains(q.member.as_ref().unwrap_or(&q.item)),
                _ => false,
            },
            ExprKind::MethodCall { method, .. } => d.contains(method),
            ExprKind::Try { operand, .. } => tail(operand, d),
            ExprKind::Block(b) | ExprKind::Unsafe(b) => tail_block(b, d),
            ExprKind::If {
                then_branch,
                else_branch: Some(eb),
                ..
            } => tail_block(then_branch, d) && tail(eb, d),
            ExprKind::Match { arms, .. } => {
                !arms.is_empty() && arms.iter().all(|a| tail(&a.body, d))
            }
            _ => false,
        }
    }
    let mut fns: Vec<&FnDef> = Vec::new();
    for it in &program.items {
        match it {
            Item::Fn(f) => fns.push(f),
            Item::Impl(i) => fns.extend(i.items.iter().filter_map(|m| match m {
                AstImplItem::Method(f) => Some(f),
                _ => None,
            })),
            Item::Trait(t) => fns.extend(t.items.iter().filter_map(|m| match m {
                rokc_ast::TraitItem::Method(f) => Some(f),
                _ => None,
            })),
            _ => {}
        }
    }
    names.extend(
        fns.iter()
            .filter(|f| {
                f.ret_ty
                    .as_ref()
                    .is_some_and(|t| matches!(t.kind, AstTyKind::Never))
            })
            .map(|f| leaf_name(&f.name).to_owned()),
    );
    let open: Vec<&FnDef> = fns
        .into_iter()
        .filter(|f| f.ret_ty.is_none() && f.body.is_some())
        .collect();
    let mut out = HashSet::new();
    loop {
        let before = out.len();
        for f in &open {
            if !out.contains(&f.span) && f.body.as_ref().is_some_and(|b| tail_block(b, names)) {
                out.insert(f.span);
                names.insert(leaf_name(&f.name).to_owned());
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// Does generic impl `i` cover every instance of its base: arguments are
/// distinct impl parameters (`Box<T>`, not `P<str, V>` or `P<T, T>`)?
fn covers_base(i: &rokc_ast::ImplDef) -> bool {
    let AstTyKind::Path(p) = &i.self_ty.kind else {
        return false;
    };
    let args = p.segments.last().map_or(&[][..], |s| s.generics.as_slice());
    let mut seen = std::collections::HashSet::new();
    args.iter().all(|a| {
        matches!(&a.kind, AstTyKind::Path(ap)
            if ap.segments.len() == 1
                && ap.segments[0].generics.is_empty()
                && i.generics.params.iter().any(|gp| matches!(gp,
                    rokc_ast::GenericParam::Type { name, .. }
                    | rokc_ast::GenericParam::Const { name, .. } if *name == ap.segments[0].name))
                && seen.insert(ap.segments[0].name.as_str()))
    })
}

/// Expand generic alias targets (`impl<T> Al<T>` with `type Al<T> = S<T>`)
/// before impls are keyed.
fn expand_impl_aliases(program: &mut Program, names: &TypeNames) {
    for it in &mut program.items {
        let Item::Impl(i) = it else { continue };
        let module = impl_module(i);
        for _ in 0..32 {
            let Some(mut ty) = names.expand_alias(&i.self_ty, &module, i.span) else {
                break;
            };
            ty.span = i.self_ty.span;
            i.self_ty = ty;
        }
    }
}

fn impl_param_types(
    program: &mut Program,
    names: &TypeNames,
    base: &HashMap<String, &rokc_ast::TraitDef>,
) {
    let traits: HashMap<&str, &rokc_ast::TraitDef> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Trait(t) => Some((t.name.as_str(), t)),
            _ => None,
        })
        .chain(base.iter().map(|(k, t)| (k.as_str(), *t)))
        .collect();
    let untyped = |p: &Param| p.ty.is_none() && p.name != "self" && !p.variadic;
    let mut fills: Vec<(usize, usize, usize, Ty)> = Vec::new();
    for (ii, it) in program.items.iter().enumerate() {
        let Item::Impl(i) = it else { continue };
        let Some(tp) = &i.trait_path else { continue };
        let has_untyped =
            |m: &AstImplItem| matches!(m, AstImplItem::Method(d) if d.params.iter().any(untyped));
        if !i.items.iter().any(has_untyped) {
            continue;
        }
        let module = impl_module(i);
        let segs: Vec<&str> = tp.segments.iter().map(|s| s.name.as_str()).collect();
        let Some(td) = traits.get(names.trait_key(&segs, &module, i.span).as_str()) else {
            continue;
        };
        let params = trait_param_args(td, i);
        for (mi, m) in i.items.iter().enumerate() {
            let AstImplItem::Method(d) = m else { continue };
            let decl = td.items.iter().find_map(|ti| match ti {
                rokc_ast::TraitItem::Method(t) if leaf_name(&t.name) == leaf_name(&d.name) => {
                    Some(t)
                }
                _ => None,
            });
            let Some(decl) = decl else { continue };
            let mut own = params.clone();
            for gp in &decl.generics.params {
                let (rokc_ast::GenericParam::Type { name, .. }
                | rokc_ast::GenericParam::Const { name, .. }) = gp;
                own.entry(name.clone())
                    .or_insert_with(|| path_ty_named(name, decl.span));
            }
            for (pi, (p, tp)) in d.params.iter().zip(&decl.params).enumerate() {
                let (true, Some(t)) = (untyped(p), &tp.ty) else {
                    continue;
                };
                let mut ty = t.clone();
                names.absolutize(&mut ty, &module_of(&td.name), &module, &own, i.span);
                subst_assoc_ty(&mut ty, &params);
                fills.push((ii, mi, pi, ty));
            }
        }
    }
    for (ii, mi, pi, ty) in fills {
        if let Item::Impl(i) = &mut program.items[ii] {
            if let AstImplItem::Method(d) = &mut i.items[mi] {
                d.params[pi].ty = Some(ty);
            }
        }
    }
}

/// Spec 2.8.3: warn when an associated type hides an outer type of the
/// same name inside its trait / impl.
fn warn_assoc_shadowing(
    program: &Program,
    names: &TypeNames,
    diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) {
    for it in &program.items {
        let (assoc, module, owner) = match it {
            Item::Impl(i) => (
                i.items
                    .iter()
                    .filter_map(|m| match m {
                        AstImplItem::AssocType { name, .. } => Some(name.clone()),
                        _ => None,
                    })
                    .collect(),
                impl_module(i),
                format!("impl for `{}`", impl_def_display(&i.self_ty)),
            ),
            Item::Trait(t) => (
                trait_assoc_names(t),
                item_module(it),
                format!("trait `{}`", leaf_name(&t.name)),
            ),
            _ => continue,
        };
        for a in assoc {
            let prim = matches!(
                crate::ty::resolve_ty(&path_ty_named(&a, it.span())),
                HirTy::Prim(_)
            );
            // A trait of that name is not a type (`trait IntoFuture { type
            // IntoFuture; }`).
            let outer = names
                .canon(&[a.as_str()], &module, &[], it.span())
                .filter(|full| !names.traits.contains(full));
            if prim || outer.is_some() {
                diags.push(rokc_diagnostic::RokDiagnostic::warning(
                    rokc_diagnostic::ErrorKind::Type,
                    it.span(),
                    format!(
                        "associated type `{a}` shadows the outer type `{a}` inside the {owner}; \
                         there `{a}` and `Self::{a}` both mean the associated type (spec 2.8.3)"
                    ),
                ));
            }
        }
    }
}

pub(super) fn subst_assoc_sig(def: &mut FnDef, assoc: &HashMap<String, Ty>) {
    if assoc.is_empty() {
        return;
    }
    for p in &mut def.params {
        if let Some(t) = &mut p.ty {
            subst_assoc_ty(t, assoc);
        }
    }
    if let Some(t) = &mut def.ret_ty {
        subst_assoc_ty(t, assoc);
    }
    if let rokc_ast::ThrowsKind::Typed(ts) = &mut def.throws {
        ts.iter_mut().for_each(|t| subst_assoc_ty(t, assoc));
    }
    // Bounds and defaults too: `fn chain<J: Iterator<Item = Item>>`,
    // `fn collect<C = List<Item>>`.
    for gp in &mut def.generics.params {
        if let rokc_ast::GenericParam::Type {
            bounds, default, ..
        } = gp
        {
            bounds.iter_mut().for_each(|b| subst_assoc_ty(b, assoc));
            if let Some(d) = default {
                subst_assoc_ty(d, assoc);
            }
        }
    }
    // `where Item: Ord` names the bounded type through `assoc` as well.
    for w in &mut def.where_clause {
        subst_assoc_ty(&mut w.ty, assoc);
        w.bounds.iter_mut().for_each(|b| subst_assoc_ty(b, assoc));
    }
}

/// Replace `X` and `Self::X` by `assoc[X]` (associated types, generic
/// alias parameters).
pub(super) fn subst_assoc_ty(ty: &mut Ty, assoc: &HashMap<String, Ty>) {
    match &mut ty.kind {
        AstTyKind::Path(p) => {
            let name = match p.segments.as_slice() {
                [s] if s.generics.is_empty() => Some(s.name.clone()),
                [s, x] if s.name == "Self" && x.generics.is_empty() => Some(x.name.clone()),
                _ => None,
            };
            if let Some(t) = name.and_then(|n| assoc.get(&n)) {
                *ty = t.clone();
                return;
            }
            // `Item::IntoIter` → `List<T>::IntoIter` (a projection on
            // the impl's type).
            if let [s, x] = p.segments.as_slice() {
                if let (true, Some(AstTyKind::Path(ap))) = (
                    s.generics.is_empty() && x.generics.is_empty(),
                    assoc.get(&s.name).map(|t| &t.kind),
                ) {
                    let mut segs = ap.segments.clone();
                    segs.push(x.clone());
                    p.segments = segs;
                    return;
                }
            }
            for s in &mut p.segments {
                for g in &mut s.generics {
                    subst_assoc_ty(g, assoc);
                }
            }
        }
        AstTyKind::Sum(ps) => {
            for t in ps.iter_mut() {
                // A `Nest` (`X?`) is already one member.
                let before = matches!(&t.kind, AstTyKind::Path(p) if p.segments.len() <= 2)
                    .then(|| format!("{:?}", t.kind));
                subst_assoc_ty(t, assoc);
                // A replaced parameter stays one member (spec 2.6.4).
                if before.is_some_and(|b| b != format!("{:?}", t.kind)) {
                    let span = t.span;
                    let inner = std::mem::replace(
                        t,
                        Ty {
                            kind: AstTyKind::Never,
                            span,
                        },
                    );
                    t.kind = AstTyKind::Nest(Box::new(inner));
                }
            }
        }
        AstTyKind::Nest(x) | AstTyKind::Binding(_, x) => subst_assoc_ty(x, assoc),
        AstTyKind::QPath(q) => {
            subst_assoc_ty(&mut q.self_ty, assoc);
            for s in &mut q.trait_path.segments {
                s.generics.iter_mut().for_each(|g| subst_assoc_ty(g, assoc));
            }
        }
        AstTyKind::Tuple(ps) => ps.iter_mut().for_each(|t| subst_assoc_ty(t, assoc)),
        AstTyKind::Array(e, len) => {
            subst_assoc_ty(e, assoc);
            // `[T; D]` with const parameter `D` given as `3`.
            let name = match len {
                rokc_ast::ConstExpr::Path(p) => {
                    p.segments.first().filter(|_| p.segments.len() == 1)
                }
                rokc_ast::ConstExpr::Expr(e) => match &e.kind {
                    ExprKind::Path(p) if p.segments.len() == 1 => p.segments.first(),
                    _ => None,
                },
                rokc_ast::ConstExpr::Literal(_) => None,
            };
            if let Some(AstTyKind::Const(c)) =
                name.and_then(|s| assoc.get(&s.name)).map(|t| &t.kind)
            {
                *len = c.clone();
            }
        }
        AstTyKind::Fn(args, ret, _) => {
            args.iter_mut().for_each(|t| subst_assoc_ty(t, assoc));
            subst_assoc_ty(ret, assoc);
        }
        AstTyKind::Throws { ok, err } => {
            subst_assoc_ty(ok, assoc);
            subst_assoc_ty(err, assoc);
        }
        _ => {}
    }
}

/// Where a declaration is.
fn decl_span(d: &MethodOrFn<'_>) -> Span {
    match d {
        MethodOrFn::Fn(f) => f.span,
        MethodOrFn::Method { fn_def, .. } => fn_def.span,
    }
}

/// A second `what` named `name`; against a const or static of that
/// name, both kinds.
fn value_dup(what: &str, name: &str, values: &HashMap<String, (Span, &str)>) -> String {
    match values.get(name) {
        Some((_, other)) if *other != what => {
            format!("`{name}` is defined more than once, as a {other} and as a {what}")
        }
        _ => format!("{what} `{name}` is defined more than once"),
    }
}

fn method_key(
    fn_table: &HashMap<String, std::sync::Arc<FnSig>>,
    method_info: &HashMap<FunctionId, MethodInfo>,
    target: &HirTy,
    leaf: &str,
    trait_name: Option<&str>,
    trait_args: &str,
) -> Option<String> {
    let key = format!("{}__{leaf}", ty_mangle(target));
    let Some(prev) = fn_table.get(&key) else {
        return Some(key);
    };
    let prev_trait = method_info
        .get(&prev.id)
        .and_then(|i| i.trait_name.as_deref());
    // The same trait again is a duplicate unless its arguments differ
    // (checked per impl before this).
    if prev_trait == trait_name && trait_args.is_empty() {
        return None;
    }
    let alt = format!(
        "{key}__by_{}{trait_args}",
        trait_name.map_or("inherent".to_owned(), |t| t.replace("::", "_"))
    );
    (!fn_table.contains_key(&alt)).then_some(alt)
}

/// The copy-out shape of a function with `ref` parameters (spec 4.7).
pub(super) fn ref_out_of(sig: &FnSig) -> Option<RefOut> {
    let refs: Vec<(usize, HirTy)> = sig
        .param_refs
        .iter()
        .enumerate()
        .filter(|(_, r)| **r)
        .map(|(i, _)| (i, sig.param_tys[i].clone()))
        .collect();
    (!refs.is_empty()).then(|| RefOut {
        ret: sig.ret_ty.clone(),
        refs,
    })
}

/// Spec 6.5: every `#[literal]` function's signature against its
/// pattern's captures; the built-in shapes' handlers.
fn literal_handlers(
    program: &Program,
    diags: &mut Vec<rokc_diagnostic::RokDiagnostic>,
) -> Vec<super::ShapeHandler> {
    use rokc_parser::{ArgKind, PatKind, Pattern};
    let mut out = Vec::new();
    // (pattern, priority, return type leaf) → first handler; a second is
    // ambiguous wherever the pattern matches (6.5.5).
    let mut seen: HashMap<(String, i64, String), (String, rokc_diagnostic::Span)> = HashMap::new();
    for item in &program.items {
        let Item::Fn(f) = item else { continue };
        let Some(attr) = f.attrs.iter().find(|a| a.name() == "literal") else {
            continue;
        };
        let rokc_ast::AttrBody::Args(args) = &attr.body else {
            continue;
        };
        let Some(rokc_ast::AttrArg::Lit(rokc_ast::Literal::Str(text))) = args.first() else {
            continue;
        };
        let Some(pat) = Pattern::from_text(text) else {
            continue;
        };
        let priority = args
            .iter()
            .find_map(|a| match a {
                rokc_ast::AttrArg::KeyValue(k, rokc_ast::Literal::Int(v)) if k == "priority" => {
                    v.parse().ok()
                }
                _ => None,
            })
            .unwrap_or(0);
        let want = pat.args();
        let fits = want.len() == f.params.len()
            && want
                .iter()
                .zip(&f.params)
                .all(|(k, p)| !p.variadic || *k == ArgKind::Many);
        if !fits {
            let shape: Vec<&str> = want
                .iter()
                .map(|k| match k {
                    ArgKind::One => "a value",
                    ArgKind::Many => "a repetition (`...T`, an array or a List)",
                })
                .collect();
            diags.push(
                rokc_diagnostic::RokDiagnostic::error(
                    rokc_diagnostic::ErrorKind::Meta,
                    attr.span,
                    format!(
                        "`#[literal({text})]` handler `{}` must take {} parameter(s): {} \
                         (spec 6.5.4)",
                        f.name,
                        want.len(),
                        if shape.is_empty() {
                            "none".to_owned()
                        } else {
                            shape.join(", ")
                        }
                    ),
                )
                .with_secondary(f.span, "handler declared here"),
            );
            continue;
        }
        let ret_leaf = match f.ret_ty.as_ref().map(|t| &t.kind) {
            Some(AstTyKind::Path(p)) => p.segments.last().map(|s| s.name.clone()),
            _ => None,
        };
        let key = (pat.render(), priority, ret_leaf.unwrap_or_default());
        if let Some((first, first_span)) = seen.get(&key) {
            diags.push(
                rokc_diagnostic::RokDiagnostic::error(
                    rokc_diagnostic::ErrorKind::Meta,
                    attr.span,
                    format!(
                        "`#[literal({text})]` is already registered by `{first}` with the same \
                         priority and return type: every use would be ambiguous - drop one, or \
                         give one a `priority = N` (spec 6.5.5)"
                    ),
                )
                .with_secondary(*first_span, "first registered here"),
            );
            continue;
        }
        seen.insert(key, (f.name.clone(), attr.span));
        if let Ok(PatKind::Shape(shape)) = pat.kind() {
            out.push(super::ShapeHandler {
                shape,
                name: f.name.clone(),
                priority,
                span: attr.span,
            });
        }
    }
    out
}

/// The module of item `name` (`m::f`, `m::T`, or a method `m::T::f`):
/// type segments are `UpperCamelCase`, module segments are not.
fn owner_module(name: &str) -> String {
    let mut m = module_of(name);
    while leaf_name(&m).starts_with(|c: char| c.is_ascii_uppercase()) {
        m = module_of(&m);
    }
    m
}

/// `Tables::main_lets` of the implicit `main`.
fn main_lets(program: &Program) -> HashMap<String, String> {
    let main = program.items.iter().find_map(|i| match i {
        Item::Fn(f) if f.name == "main" => f.body.as_ref(),
        _ => None,
    });
    let mut out = HashMap::new();
    for s in main.iter().flat_map(|b| &b.stmts) {
        let StmtKind::Let { name, ty, init } = &s.kind else {
            continue;
        };
        if name.starts_with(['#', '_']) {
            continue;
        }
        let ty = match (ty, &init.kind) {
            (Some(t), _) => rokc_ast::print::print_ty(t),
            (None, ExprKind::Literal(rokc_ast::Literal::Int(t)))
                if t.bytes().all(|b| b.is_ascii_digit() || b == b'_') =>
            {
                "i32".to_owned()
            }
            (None, ExprKind::Literal(rokc_ast::Literal::Float(_))) => "f64".to_owned(),
            (None, ExprKind::Literal(rokc_ast::Literal::Str(_))) => "str".to_owned(),
            (None, ExprKind::Literal(rokc_ast::Literal::Bool(_))) => "bool".to_owned(),
            (None, _) => "..".to_owned(),
        };
        let konst = format!(
            "const {}: {ty} = {};",
            name.to_uppercase(),
            super::expr::snippet(init)
        );
        out.insert(name.clone(), konst);
    }
    out
}
