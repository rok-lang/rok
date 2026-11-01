//! AST → HIR lowering.

use std::collections::HashMap;

use rokc_ast::{
    Block, CallArg, EnumDef as AstEnumDef, EnumVariantKind as AstEnumVariantKind, Expr, ExprKind,
    FnDef, ImplItem as AstImplItem, Item, Literal as AstLit, MatchArm as AstMatchArm, Param,
    Path as AstPath, Pattern as AstPattern, PatternKind as AstPatternKind, Program, Stmt, StmtKind,
    StructDef as AstStructDef, StructKind as AstStructKind, Ty, TyKind as AstTyKind,
};
use rokc_core::Visibility;
use rokc_diagnostic::Span;
use rokc_ir_common::{ConstVal, FunctionId, LocalId, PrimOp, PrimTy};
use rokc_parser::{Fixity, PrecTable};

use crate::ty::resolve_ty;
use crate::{
    EnumId, HirBlock, HirEnumDef, HirEnumVariant, HirExpr, HirExprKind, HirField, HirFunction,
    HirLocal, HirMatchArm, HirModule, HirParam, HirPattern, HirStaticDef, HirStmt, HirStructDef,
    HirTy, HirUnionKind, NicheArg, NicheVal, StructId, TraitRef, UnionStorage,
};

mod affine;
mod bignum;
mod bounds;
mod broadcast;
mod caller;
mod collect;
pub mod const_eval;
mod coro;
mod desugar;
mod expr;
mod frame_ptr;
mod fstring;
mod handoff;
mod incr;
mod infer;
mod method_value;
mod mono;
mod pattern;
mod reflect;
mod resolve;
pub use resolve::{hidden_module, hidden_module_msg, module_visibility, visibility_rank};
pub use resolve::{implicit_use, is_dependency, root_path, set_packages, visible_from};
mod send_check;
mod suggest;
mod throws;
mod views;

use collect::collect_unknown_paths;
pub use collect::{KeptLibrary, LibraryStage, keep_library, kept_library, lower_full, stage};
pub use coro::FN_BLOCK_LABEL;
use desugar::{
    desugar_nullish_coalesce, desugar_question_dot, desugar_try_q, lower_closure_expr, lower_for,
};
use expr::{
    build_any_dispatchers, enum_base_name, field_access, has_int_suffix, hir_binop, hir_int,
    hir_local, lower_block, lower_cond, lower_expr, lower_loop_body, lower_sibling_pair,
    method_call_on, parse_char_literal, parse_int_literal, resolve_enum_variant, variant_arg_tys,
};
pub use incr::{Bodies, BodyCache, BodyReuse, Unit};
use mono::{
    bind_from_arg, bind_self, generic_enum_variants, generic_struct_fields, generic_subst,
    lower_fn, monomorphize, monomorphize_on, resolve_in_mono, unify_param_ty_with,
};
use pattern::{int_fits, lower_match, lower_pattern};
use resolve::{
    COPY_TRAIT, DEFAULT_TRAIT, EQ_TRAIT, GC_BUF, impl_module, is_toolchain_module, item_module,
    leaf_name, mangle_inst, method_target_for, module_display, module_of, package_of, resolve_name,
    resolve_ty_with_structs, substitute_params, ty_mangle,
};
use throws::{
    ErrFrame, adapt_fn_value, adapt_never_fn, erase_unbound_errors, exit_through,
    finish_throwing_body, jump_index, lower_catch, lower_return, lower_throw, lower_try,
    main_frame, rethrows_value, sig_ret_ast, uncaught_in_main, unpack_call,
};

/// Glob resolution of one bare name: a single import beats any glob; two
/// globs landing on one suffix are `Ambiguous`.
enum LookupOutcome {
    None,
    Resolved(String),
    Ambiguous(Vec<String>),
}

/// A name table to resolve against: membership test plus the keys
/// ending in a given segment, needed only by `use a::**` deep globs.
struct NameSet<'b> {
    exists: &'b dyn Fn(&str) -> bool,
    keys: &'b dyn Fn(&str) -> Vec<String>,
    /// Is the key visible from the resolving module? A glob brings in only
    /// what it can see (spec 5.6), so core's private helpers never compete.
    visible: &'b dyn Fn(&str) -> bool,
    /// Does a deep glob's matched path end at a type before the name? Then
    /// the key is an associated item, which `**` doesn't reach (spec 5.5.7).
    in_type: &'b dyn Fn(&str) -> bool,
}

/// Type namespace: every struct / union / enum / alias / trait name
/// (qualified, generic bases unmangled) plus `pub use` aliases. Every type
/// path resolves through `canon`.
#[derive(Clone)]
struct TypeNames {
    names: std::collections::HashSet<String>,
    traits: std::collections::HashSet<String>,
    /// Structural auto traits (`#[lang("send_trait" | "sync_trait")]`):
    /// a type has them unless a component lacks them.
    auto_traits: std::collections::HashSet<String>,
    /// The `#[lang]` slots (spec 6.4).
    lang: rokc_parser::LangTable,
    /// Every enum variant as `a::E::V` (generic bases unmangled). A
    /// variant is as visible as its enum (spec 5.6.9).
    variants: std::collections::HashSet<String>,
    /// `pub use` alias key → the type its re-export chain ends at.
    reexports: HashMap<String, String>,
    visibility: HashMap<String, Visibility>,
    /// Module path → its `mod` visibility (the driver's flattened stubs).
    mod_vis: HashMap<String, Visibility>,
    uses: crate::uses::UseTable,
    diags: std::cell::RefCell<Vec<rokc_diagnostic::RokDiagnostic>>,
    /// Generic aliases by full name, expanded at each use; also non-generic
    /// ones with a `where` clause (checked once, not expanded here).
    generic_aliases: HashMap<String, rokc_ast::TypeAliasDef>,
    /// Non-generic aliases, so generic instance arguments can name one
    /// before the alias table exists.
    plain_aliases: HashMap<String, Ty>,
    /// Plain aliases being expanded (caps a cycle).
    alias_depth: std::cell::Cell<u32>,
    /// Plain aliases that are cyclic or name a projection that fails.
    broken_aliases: std::cell::RefCell<std::collections::HashSet<String>>,
    /// Every generic alias use: (alias, arguments, module, span), for
    /// its bounds check (`check_type_bounds`).
    alias_uses: std::cell::RefCell<Vec<AliasUse>>,
    /// First use of each unresolved type path, for its "cannot find
    /// type" location.
    unknown_spans: std::cell::RefCell<HashMap<String, rokc_diagnostic::Span>>,
    /// First use of each generic type written without arguments (a
    /// leftover one is "needs generic arguments", not "cannot find").
    bare_generics: std::cell::RefCell<HashMap<String, rokc_diagnostic::Span>>,
    /// `canon` results without local `use`s or diagnostics, by (path,
    /// module): its glob lookups scan every type name.
    canon_memo: std::cell::RefCell<HashMap<(String, String), Option<String>>>,
    /// Type and variant keys by last segment, built on the first deep
    /// glob lookup (the tables are complete by then).
    leaves: std::cell::OnceCell<Leaves>,
    variant_leaves: std::cell::OnceCell<Leaves>,
    /// Module-level `const`s by full name, for array lengths in
    /// signatures and fields (`[i32; N]`).
    consts: HashMap<String, (Ty, rokc_ast::Expr)>,
    const_leaves: std::cell::OnceCell<Leaves>,
    /// `pub use` alias key → the `const` / `static` its re-export chain
    /// ends at (both keys are in the const / static tables).
    value_reexports: HashMap<String, String>,
}

/// An unknown name's help: the `use` that brings it in, else a close
/// name.
fn join_help(import: Option<String>, typo: Option<String>) -> Option<String> {
    // An item of exactly that name beats a close one.
    import.or(typo)
}

/// A warning at `at` that belongs to `lint`, with a `help:` note.
fn lint_diag(lint: &str, at: Span, msg: String, help: String) -> rokc_diagnostic::RokDiagnostic {
    rokc_diagnostic::RokDiagnostic::warning(rokc_diagnostic::ErrorKind::Type, at, msg)
        .with_help(help)
        .with_lint(lint)
}

/// Keys by their last `::` segment.
type Leaves = HashMap<String, Vec<String>>;

fn leaves<'k>(keys: impl IntoIterator<Item = &'k String>) -> Leaves {
    let mut out: Leaves = HashMap::new();
    for k in keys {
        let leaf = k.rsplit("::").next().unwrap_or(k);
        out.entry(leaf.to_owned()).or_default().push(k.clone());
    }
    out
}

/// (alias full name, arguments, module of the use, span of the use).
type AliasUse = (String, Vec<Ty>, String, rokc_diagnostic::Span);

/// Can code in `module` name the type-like item `full` (spec 5.6)?
/// Unknown names (primitives, ..) are visible.
#[must_use]
pub fn type_visible(m: &crate::HirModule, full: &str, module: &str) -> bool {
    m.type_visibility
        .get(full)
        .is_none_or(|&vis| visible_from(module, &module_of(full), vis))
}

impl TypeNames {
    /// The item bound to lang slot `slot` (spec 6.4), `""` if unbound; a
    /// method slot (`drop_method`) gives its trait.
    fn lang_item(&self, slot: &str) -> &str {
        self.lang.lookup(slot).map_or("", |e| {
            if slot.ends_with("_method") {
                e.item_name.rsplit_once("::").map_or("", |(t, _)| t)
            } else {
                &e.item_name
            }
        })
    }

    /// `Row<3>` → `[i32; 3]` for a generic alias; the body is resolved in
    /// the alias's module and written absolute.
    fn expand_alias(&self, ty: &Ty, module: &str, at: Span) -> Option<Ty> {
        let AstTyKind::Path(p) = &ty.kind else {
            return None;
        };
        let args = &p.segments.last()?.generics;
        if args.is_empty() || !self.any_generic_alias() {
            return None;
        }
        let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
        let full = self.canon(&segs, module, &[], at)?;
        let alias = self
            .generic_alias(&full)
            .filter(|a| !a.generics.params.is_empty())?;
        let params = &alias.generics.params;
        let min = params
            .iter()
            .take_while(|p| {
                !matches!(
                    p,
                    rokc_ast::GenericParam::Type {
                        default: Some(_),
                        ..
                    }
                )
            })
            .count();
        if args.len() < min || args.len() > params.len() {
            let (msg, help) = mono::misfit(leaf_name(&full), &alias.generics, args.len());
            self.error_help(if ty.span.is_dummy() { at } else { ty.span }, msg, help);
        }
        // A missing argument (reported above) reads as `any`.
        let any = Ty {
            kind: AstTyKind::Any,
            span: ty.span,
        };
        let map: HashMap<String, Ty> = params
            .iter()
            .enumerate()
            .map(|(i, gp)| match gp {
                rokc_ast::GenericParam::Type { name, default, .. } => (
                    name.clone(),
                    args.get(i).or(default.as_ref()).unwrap_or(&any).clone(),
                ),
                rokc_ast::GenericParam::Const { name, .. } => {
                    (name.clone(), args.get(i).unwrap_or(&any).clone())
                }
            })
            .collect();
        let mut out = alias.ty.clone();
        self.absolutize(&mut out, &module_of(&full), module, &map, at);
        collect::subst_assoc_ty(&mut out, &map);
        self.alias_uses
            .borrow_mut()
            .push((full, args.clone(), module.to_owned(), ty.span));
        Some(out)
    }

    /// The non-generic alias `ty` names in `module`.
    fn plain_alias(&self, ty: &Ty, module: &str, at: Span) -> Option<String> {
        let AstTyKind::Path(p) = &ty.kind else {
            return None;
        };
        if !p.segments.last()?.generics.is_empty() {
            return None;
        }
        let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
        self.canon(&segs, module, &[], at)
            .filter(|full| self.plain_alias_ty(full).is_some())
    }

    /// `B` → `<A as P>::Out` for a plain alias of a projection, expanded in
    /// body context (after impl registration).
    fn projection_alias(&self, full: &str, module: &str, at: Span) -> Option<Ty> {
        let body = self.plain_alias_ty(full)?;
        let from = module_of(full);
        if !self.names_projection(body, &from, at) {
            return None;
        }
        let mut out = body.clone();
        self.absolutize(&mut out, &from, module, &HashMap::new(), at);
        Some(out)
    }

    /// Does `ty` (written in `module`) name a projection: `<T as Tr>::X`,
    /// or `A::X` with `A` a type?
    fn names_projection(&self, ty: &Ty, module: &str, at: Span) -> bool {
        struct Find<'x> {
            names: &'x TypeNames,
            module: &'x str,
            at: Span,
            found: bool,
        }
        impl crate::walk::AstVisitor for Find<'_> {
            fn ty(&mut self, t: &Ty) {
                match &t.kind {
                    AstTyKind::QPath(_) => self.found = true,
                    AstTyKind::Path(p) => {
                        let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
                        let n = self.names;
                        match n.canon(&segs, self.module, &[], self.at) {
                            // Through another alias (`type D = List<B>;`).
                            Some(full) if n.alias_depth.get() < 32 => {
                                if let Some(body) = n.plain_alias_ty(&full) {
                                    n.alias_depth.set(n.alias_depth.get() + 1);
                                    self.found |=
                                        n.names_projection(body, &module_of(&full), self.at);
                                    n.alias_depth.set(n.alias_depth.get() - 1);
                                }
                            }
                            Some(_) => {}
                            None => {
                                self.found |= segs.len() > 1
                                    && n.canon(&segs[..segs.len() - 1], self.module, &[], self.at)
                                        .is_some();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut f = Find {
            names: self,
            module,
            at,
            found: false,
        };
        crate::walk::walk_ty(&mut f, ty);
        f.found
    }

    /// Rewrite the type names in `ty` (outside `params`) that resolve in
    /// module `from` to absolute paths valid in module `to`.
    fn absolutize(
        &self,
        ty: &mut Ty,
        from: &str,
        to: &str,
        params: &HashMap<String, Ty>,
        at: Span,
    ) {
        let mut rec = |t: &mut Ty| self.absolutize(t, from, to, params, at);
        match &mut ty.kind {
            AstTyKind::Path(p) => {
                for s in &mut p.segments {
                    s.generics.iter_mut().for_each(&mut rec);
                }
                let mut segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
                if params.contains_key(segs[0]) || matches!(segs[0], "pkg" | "rok" | "Self") {
                    return;
                }
                // `A::Out`: a projection keeps its last segment.
                let mut tail = None;
                let full = match self.canon(&segs, from, &[], at) {
                    Some(full) => full,
                    None if segs.len() > 1 => {
                        segs.pop();
                        let Some(full) = self.canon(&segs, from, &[], at) else {
                            return;
                        };
                        tail = p.segments.pop();
                        full
                    }
                    None => return,
                };
                let pkg = resolve::package_of(&full);
                let abs: Vec<String> = if full.starts_with("rok::") {
                    full.split("::").map(str::to_owned).collect()
                } else if pkg == resolve::package_of(to) {
                    let rest = full.strip_prefix(pkg).unwrap_or(&full);
                    std::iter::once("pkg")
                        .chain(rest.split("::").filter(|x| !x.is_empty()))
                        .map(str::to_owned)
                        .collect()
                } else {
                    full.split("::").map(str::to_owned).collect()
                };
                let last = p.segments.pop().map(|s| s.generics).unwrap_or_default();
                let n = abs.len();
                p.segments = abs
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| rokc_ast::PathSegment {
                        name,
                        generics: if i + 1 == n { last.clone() } else { Vec::new() },
                    })
                    .collect();
                p.segments.extend(tail);
            }
            AstTyKind::Sum(ts) | AstTyKind::Tuple(ts) => ts.iter_mut().for_each(rec),
            AstTyKind::Array(e, _) | AstTyKind::Nest(e) | AstTyKind::Binding(_, e) => rec(e),
            AstTyKind::QPath(q) => {
                rec(&mut q.self_ty);
                let mut tp = Ty {
                    kind: AstTyKind::Path(q.trait_path.clone()),
                    span: ty.span,
                };
                rec(&mut tp);
                if let AstTyKind::Path(p) = tp.kind {
                    q.trait_path = p;
                }
            }
            AstTyKind::Fn(args, ret, _) => {
                args.iter_mut().for_each(&mut rec);
                rec(ret);
            }
            AstTyKind::Throws { ok, err } => {
                rec(ok);
                rec(err);
            }
            _ => {}
        }
    }

    /// The type key `segments` names from `module`; its errors
    /// (ambiguity, visibility) are reported at `at`.
    fn canon(
        &self,
        segments: &[&str],
        module: &str,
        local_uses: &[Vec<crate::uses::UseEntry>],
        at: Span,
    ) -> Option<String> {
        if !local_uses.iter().all(Vec::is_empty) {
            return self.canon_uncached(segments, module, local_uses, at);
        }
        incr::read(|| {
            incr::Read::Canon(
                segments.iter().map(|s| (*s).to_owned()).collect(),
                module.to_owned(),
            )
        });
        let key = (segments.join("::"), module.to_owned());
        if let Some(hit) = self.canon_memo.borrow().get(&key) {
            return hit.clone();
        }
        let errors = self.diags.borrow().len();
        let out = self.canon_uncached(segments, module, local_uses, at);
        if self.diags.borrow().len() == errors {
            self.canon_memo.borrow_mut().insert(key, out.clone());
        }
        out
    }

    /// Can `module` see type-namespace key `k`?
    fn visible_type(&self, k: &str, module: &str) -> bool {
        self.type_vis(k)
            .is_none_or(|vis| visible_from(module, &module_of(k), vis))
    }

    fn canon_uncached(
        &self,
        segments: &[&str],
        module: &str,
        local_uses: &[Vec<crate::uses::UseEntry>],
        at: Span,
    ) -> Option<String> {
        let exists = |k: &str| self.has_type(k) || self.reexport(k).is_some();
        let keys = |leaf: &str| self.type_leaf(leaf);
        let visible = |k: &str| self.visible_type(k, module);
        let set = NameSet {
            exists: &exists,
            keys: &keys,
            visible: &visible,
            in_type: &|_| false,
        };
        let item = |k: &String| self.reexport(k).unwrap_or(k).clone();
        let mut full = match resolve_name(segments, module, local_uses, &self.uses, &set) {
            Ok(r) => r?,
            // An item and its re-exports are one item (spec 5.8.4).
            Err(hits) if hits.iter().all(|h| item(h) == item(&hits[0])) => item(&hits[0]),
            Err(hits) => {
                self.error(
                    at,
                    format!(
                        "type name `{}` is ambiguous in module `{}` - multiple glob imports \
                     resolve it: {}",
                        segments.join("::"),
                        module_display(module),
                        hits.join(", ")
                    ),
                );
                hits.into_iter().next()?
            }
        };
        // Spec 5.6.13: every module on the path as written (a re-export
        // key, before following it) must be visible too.
        if let Some(m) = self.hidden(module, &full) {
            self.error(at, resolve::hidden_module_msg(&full, &m, module));
        }
        if let Some(t) = self.reexport(&full) {
            full.clone_from(t);
        }
        if let Some(vis) = self.type_vis(&full) {
            let def_module = module_of(&full);
            if !visible_from(module, &def_module, vis) {
                let msg = format!(
                    "type `{full}` is private to module `{}` and cannot be used from `{}` \
                     (spec 5.6 - add `pub` to expose it)",
                    module_display(&def_module),
                    module_display(module)
                );
                match resolve::own_library_privacy(&def_module) {
                    Some(help) => self.error_help(at, msg, help),
                    None => self.error(at, msg),
                }
            }
        }
        Some(full)
    }

    /// A bare (imported) variant name → its `a::E::V` key. Use entries
    /// may name the enum relative to the module (`use Result::{Ok}`).
    fn canon_variant(
        &self,
        segments: &[&str],
        module: &str,
        local_uses: &[Vec<crate::uses::UseEntry>],
        at: Span,
    ) -> Option<String> {
        let full_key = |k: &str| -> Option<String> {
            if self.is_variant(k) {
                return Some(k.to_owned());
            }
            let (ty, v) = k.rsplit_once("::")?;
            let segs: Vec<&str> = ty.split("::").collect();
            let e = self.canon(&segs, module, local_uses, at)?;
            let key = format!("{e}::{v}");
            self.is_variant(&key).then_some(key)
        };
        let exists = |k: &str| full_key(k).is_some();
        let keys = |leaf: &str| self.variant_leaf(leaf);
        // A variant is as visible as its enum (spec 5.6.9).
        let visible = |k: &str| self.visible_type(&module_of(k), module);
        let set = NameSet {
            exists: &exists,
            keys: &keys,
            visible: &visible,
            in_type: &|_| false,
        };
        match resolve_name(segments, module, local_uses, &self.uses, &set) {
            Ok(r) => full_key(&r?),
            Err(hits) => {
                self.error(
                    at,
                    format!(
                        "variant name `{}` is ambiguous in module `{}` - multiple glob imports \
                     resolve it: {}",
                        segments.join("::"),
                        module_display(module),
                        hits.join(", ")
                    ),
                );
                full_key(hits.first()?)
            }
        }
    }

    /// A `fn` type's bounds: `+ Send + Sync` (sorted auto-trait keys),
    /// `unsafe`, `extern "C"`.
    fn fn_bounds(&self, bounds: &rokc_ast::FnTyBounds, module: &str, at: Span) -> crate::FnBounds {
        let mut keys: Vec<String> = bounds
            .auto
            .iter()
            .filter_map(|b| {
                let segs: Vec<&str> = b.segments.iter().map(|s| s.name.as_str()).collect();
                let key = self.trait_key(&segs, module, at);
                if self.auto_traits.contains(&key) {
                    Some(key)
                } else {
                    self.error(
                        at,
                        format!(
                            "`{}` in a `fn` type bound is not an auto trait - only `Send` / \
                         `Sync` can bound a `fn` type",
                            segs.join("::")
                        ),
                    );
                    None
                }
            })
            .collect();
        keys.sort();
        keys.dedup();
        crate::FnBounds {
            auto: keys,
            unsafe_fn: bounds.unsafe_fn,
            c_abi: bounds.c_abi,
            stdcall: bounds.system && const_eval::stdcall_system(),
        }
    }

    /// Every auto trait, sorted: what a named function (no captures)
    /// carries as a `fn` value.
    fn all_auto(&self) -> crate::FnBounds {
        let mut auto: Vec<String> = self.auto_traits.iter().cloned().collect();
        auto.sort();
        crate::FnBounds {
            auto,
            ..crate::FnBounds::default()
        }
    }

    /// A pre-body integer constant (array lengths, trait-path const
    /// arguments): literals, bound const parameters and module `const`s via
    /// `const_eval::syntactic_int`. `None` (e.g. a `const fn` call) waits
    /// for the full evaluator.
    fn const_int(
        &self,
        c: &rokc_ast::ConstExpr,
        module: &str,
        param: &dyn Fn(&str) -> Option<i128>,
    ) -> Option<i128> {
        self.const_int_in(c, module, param, &std::cell::RefCell::new(Vec::new()))
    }

    fn const_int_in(
        &self,
        c: &rokc_ast::ConstExpr,
        module: &str,
        param: &dyn Fn(&str) -> Option<i128>,
        seen: &std::cell::RefCell<Vec<String>>,
    ) -> Option<i128> {
        const_eval::syntactic_int(c, &|path| {
            if let Some(v) = param(path) {
                return Some(v);
            }
            let segs: Vec<&str> = path.split("::").collect();
            let key = self.const_key(&segs, module)?;
            if seen.borrow().contains(&key) {
                return None;
            }
            seen.borrow_mut().push(key.clone());
            let value = rokc_ast::ConstExpr::Expr(Box::new(self.name_const(&key)?.1.clone()));
            let v = self.const_int_in(&value, &module_of(&key), &|_| None, seen);
            seen.borrow_mut().pop();
            v
        })
    }

    /// The module `const` `segments` names from `module`.
    fn const_key(&self, segments: &[&str], module: &str) -> Option<String> {
        let exists = |k: &str| self.name_const(k).is_some();
        let keys = |leaf: &str| self.name_const_leaf(leaf);
        let visible = |_: &str| true;
        let set = NameSet {
            exists: &exists,
            keys: &keys,
            visible: &visible,
            in_type: &|k| self.has_type(k),
        };
        resolve_name(segments, module, &[], &self.uses, &set)
            .ok()
            .flatten()
            .map(|k| self.value_reexport(&k).cloned().unwrap_or(k))
    }

    fn trait_key(&self, segments: &[&str], module: &str, at: Span) -> String {
        self.canon(segments, module, &[], at)
            .filter(|full| self.is_trait(full))
            .unwrap_or_else(|| segments.last().copied().unwrap_or_default().to_owned())
    }

    fn error(&self, at: Span, msg: String) {
        self.diags
            .borrow_mut()
            .push(rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Type,
                at,
                msg,
            ));
    }

    fn error_help(&self, at: Span, msg: String, help: String) {
        self.diags.borrow_mut().push(
            rokc_diagnostic::RokDiagnostic::error(rokc_diagnostic::ErrorKind::Type, at, msg)
                .with_help(help),
        );
    }
}

/// Lowering reads the name tables through these: each read is logged
/// for the incremental store (`incr::Read`).
impl TypeNames {
    /// Type-namespace key `k` names an item.
    fn has_type(&self, k: &str) -> bool {
        incr::read(|| incr::Read::Type(k.to_owned()));
        self.names.contains(k)
    }

    /// The type `pub use` alias `k` ends at.
    fn reexport(&self, k: &str) -> Option<&String> {
        incr::read(|| incr::Read::Reexport(k.to_owned()));
        self.reexports.get(k)
    }

    /// Type keys and re-exports whose last segment is `leaf`.
    fn type_leaf(&self, leaf: &str) -> Vec<String> {
        incr::read(|| incr::Read::TypeLeaf(leaf.to_owned()));
        self.leaves
            .get_or_init(|| leaves(self.names.iter().chain(self.reexports.keys())))
            .get(leaf)
            .cloned()
            .unwrap_or_default()
    }

    fn type_vis(&self, k: &str) -> Option<Visibility> {
        incr::read(|| incr::Read::TypeVis(k.to_owned()));
        self.visibility.get(k).copied()
    }

    /// `resolve::hidden_module` over the `mod` visibilities.
    fn hidden(&self, cur: &str, full: &str) -> Option<String> {
        let def = module_of(full);
        for (i, _) in def.match_indices("::") {
            incr::read(|| incr::Read::ModVis(def[..i].to_owned()));
        }
        incr::read(|| incr::Read::ModVis(def.clone()));
        resolve::hidden_module(&self.mod_vis, cur, full)
    }

    /// Help for a name `cur` can't find: up to three `use` lines for items
    /// of that name (own package, library, dependencies; shortest first),
    /// or which `pub` is missing when only hidden own items match.
    fn import_help(
        &self,
        cur: &str,
        mut keys: Vec<String>,
        vis: impl Fn(&str) -> Option<Visibility>,
    ) -> Option<String> {
        // The shortest path the prelude gives (`task::sleep`), before the
        // spellings through it are dropped.
        let open = |k: &str| {
            self.hidden(cur, k).is_none()
                && visible_from(cur, &module_of(k), vis(k).unwrap_or(Visibility::Pub))
        };
        let write = self.prelude_write(cur, keys.iter().filter(|k| open(k)));
        let prelude = format!("{}::", resolve::PRELUDE);
        keys.retain(|k| {
            module_of(k) != cur
                && !k.contains('/')
                && !k.starts_with(&prelude)
                && !k.split("::").any(|s| s.starts_with("__"))
        });
        keys.sort();
        keys.dedup();
        let tier = |k: &str| match package_of(&module_of(k)) {
            "" => 0,
            p if p.starts_with("rok::") => 1,
            _ => 2,
        };
        keys.sort_by_key(|k| (tier(k), k.matches("::").count()));
        if let Some(w) = write {
            // The `use` of the same item the path names.
            let same = format!("::{w}");
            let import = (keys.iter().filter(|k| open(k)))
                .find(|k| k.ends_with(&same))
                .or_else(|| keys.iter().find(|k| open(k)));
            return Some(match import {
                Some(k) => format!("write `{w}`, or add `use {k};`"),
                None => format!("write `{w}`"),
            });
        }
        let lines: Vec<String> = keys
            .iter()
            .filter(|k| open(k))
            .take(3)
            .map(|k| format!("`use {};`", resolve::use_path(cur, k)))
            .collect();
        // The package's own item, when only a private one has the name.
        let private = (!keys.iter().any(|k| tier(k) == 0 && open(k)))
            .then(|| keys.iter().find(|k| tier(k) == 0))
            .flatten()
            .map(|k| match self.hidden(cur, k) {
                Some(m) => {
                    format!(
                        "`{k}` has that name, but module `{m}` is private: add `pub` to its `mod`"
                    )
                }
                None => format!(
                    "`{k}` has that name, but it is private to module `{}`: add `pub` to it",
                    module_display(&module_of(k))
                ),
            });
        let import = match lines.as_slice() {
            [] => None,
            [one] => Some(format!("add {one}")),
            _ => Some(format!("add one of {}", lines.join(", "))),
        };
        match (private, import) {
            (Some(p), Some(i)) => Some(format!("{p}; or {i}")),
            (p, i) => p.or(i),
        }
    }

    /// The shortest of `keys` as code in `cur` writes it through the
    /// prelude (`rok::core::task::sleep`: `task::sleep`), if it applies
    /// there.
    fn prelude_write<'k>(
        &self,
        cur: &str,
        keys: impl Iterator<Item = &'k String>,
    ) -> Option<String> {
        let prelude = resolve::PRELUDE;
        if !matches!(implicit_use(cur), Some(crate::uses::UseEntry::Glob { prefix }) if prefix == prelude)
        {
            return None;
        }
        incr::read(|| incr::Read::Uses(prelude.to_owned()));
        let entries = self.uses.get(prelude)?;
        let entries: Vec<(&str, &str)> = (entries.iter())
            .filter_map(|e| match e {
                crate::uses::UseEntry::Single { local, full } => {
                    Some((local.as_str(), full.as_str()))
                }
                _ => None,
            })
            .collect();
        // A key spelled through the prelude (`rok::core::prelude::task::x`)
        // stands for its own path (`rok::core::task::x`).
        let real = |k: &str| {
            let Some(rest) = k.strip_prefix(prelude).and_then(|r| r.strip_prefix("::")) else {
                return k.to_owned();
            };
            let (head, tail) = rest.split_once("::").unwrap_or((rest, ""));
            match entries.iter().find(|(local, _)| *local == head) {
                Some((_, full)) if tail.is_empty() => (*full).to_owned(),
                Some((_, full)) => format!("{full}::{tail}"),
                None => k.to_owned(),
            }
        };
        let entries = &entries;
        keys.map(|k| real(k))
            .filter(|k| self.hidden(cur, k).is_none())
            .flat_map(|k| {
                entries.iter().filter_map(move |(local, full)| {
                    if k == *full {
                        return Some((*local).to_owned());
                    }
                    let rest = k.strip_prefix(full)?.strip_prefix("::")?;
                    Some(format!("{local}::{rest}"))
                })
            })
            .min_by_key(|p| (p.matches("::").count(), p.len(), p.clone()))
    }

    /// Value key `k` is a type's associated item (`Duration::MAX`,
    /// `f32::MAX`): no `use` names it.
    fn type_member(&self, k: &str) -> bool {
        let owner = module_of(k);
        self.has_type(&owner) || crate::ty::prim_named(&owner).is_some()
    }

    fn is_trait(&self, k: &str) -> bool {
        incr::read(|| incr::Read::Trait(k.to_owned()));
        self.traits.contains(k)
    }

    fn all_traits(&self) -> &std::collections::HashSet<String> {
        incr::read(|| incr::Read::Traits);
        &self.traits
    }

    fn is_variant(&self, k: &str) -> bool {
        incr::read(|| incr::Read::Variant(k.to_owned()));
        self.variants.contains(k)
    }

    fn variant_leaf(&self, leaf: &str) -> Vec<String> {
        incr::read(|| incr::Read::VariantLeaf(leaf.to_owned()));
        self.variant_leaves
            .get_or_init(|| leaves(&self.variants))
            .get(leaf)
            .cloned()
            .unwrap_or_default()
    }

    fn name_const(&self, k: &str) -> Option<&(Ty, rokc_ast::Expr)> {
        incr::read(|| incr::Read::NameConst(k.to_owned()));
        self.consts.get(k)
    }

    fn name_const_leaf(&self, leaf: &str) -> Vec<String> {
        incr::read(|| incr::Read::NameConstLeaf(leaf.to_owned()));
        self.const_leaves
            .get_or_init(|| leaves(self.consts.keys()))
            .get(leaf)
            .cloned()
            .unwrap_or_default()
    }

    /// The `const` / `static` `pub use` alias `k` ends at.
    fn value_reexport(&self, k: &str) -> Option<&String> {
        incr::read(|| incr::Read::ValueReexport(k.to_owned()));
        self.value_reexports.get(k)
    }

    fn generic_alias(&self, k: &str) -> Option<&rokc_ast::TypeAliasDef> {
        incr::read(|| incr::Read::GenericAlias(k.to_owned()));
        self.generic_aliases.get(k)
    }

    /// Whether any generic alias exists.
    fn any_generic_alias(&self) -> bool {
        incr::read(|| incr::Read::AnyGenericAlias);
        !self.generic_aliases.is_empty()
    }

    fn plain_alias_ty(&self, k: &str) -> Option<&Ty> {
        incr::read(|| incr::Read::PlainAlias(k.to_owned()));
        self.plain_aliases.get(k)
    }
}

/// `LowerCtx::access_module`: the current module, or a definition's.
enum AccessModule<'a> {
    Here(&'a str),
    Def(std::rc::Rc<str>),
}

impl std::ops::Deref for AccessModule<'_> {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Self::Here(m) => m,
            Self::Def(m) => m,
        }
    }
}

/// Where a type is being resolved: the name index, the module the
/// referencing item lives in, and the location its errors go to.
#[derive(Clone, Copy)]
struct Scope<'b> {
    names: &'b TypeNames,
    module: &'b str,
    at: Span,
}

// AST → HIR. Three passes: register struct names, then function
// signatures, then lower function bodies.
#[must_use]
pub fn lower(program: &Program, prec: &PrecTable) -> HirModule {
    let empty_uses: crate::uses::UseTable = HashMap::new();
    let empty_reexports: Vec<crate::uses::ReExport> = Vec::new();
    lower_full(
        program.clone(),
        prec,
        &[],
        &empty_uses,
        &empty_reexports,
        &crate::HirInput::default(),
        &rokc_diagnostic::SourceMap::new(),
    )
}

#[must_use]
pub fn lower_with_uses(
    program: &Program,
    prec: &PrecTable,
    uses: &crate::uses::UseTable,
) -> HirModule {
    let empty_reexports: Vec<crate::uses::ReExport> = Vec::new();
    lower_full(
        program.clone(),
        prec,
        &[],
        uses,
        &empty_reexports,
        &crate::HirInput::default(),
        &rokc_diagnostic::SourceMap::new(),
    )
}

/// Per-method facts for call sites: privacy module, visibility, and the
/// implemented trait (operators need a trait impl, spec 0.4 / 6.4).
#[derive(Clone)]
struct MethodInfo {
    module: String,
    vis: Visibility,
    trait_name: Option<String>,
    /// The trait's arguments of a concrete impl (`Convert<f64>`: `[f64]`),
    /// defaults filled; empty for a template.
    trait_args: Vec<HirTy>,
}

/// A method template monomorphized per call: `def` has the impl's
/// generics then the method's, named `Base__method` in the impl's module.
#[derive(Clone)]
struct GenericMethod {
    /// Shared: the tables of a kept library stage are cloned per compile.
    def: std::sync::Arc<FnDef>,
    /// The impl target (`Box<T>`): the type of `self` and of `Self`.
    self_ty: Ty,
    info: MethodInfo,
}

/// An associated const `(NAME, type, value)`.
type AssocConst = (String, Ty, Expr);

/// One `impl` (spec 2.9), inherent or trait, concrete or generic (a bare
/// `T` target is a blanket impl).
#[derive(Clone)]
struct Impl {
    /// Empty for an inherent impl.
    trait_key: String,
    /// `impl<T> Get<T> for Box<T> { type Item = T; }`: `[T]` and
    /// `[(Item, T)]`, resolved per instantiation.
    trait_args: Vec<Ty>,
    assoc: Vec<(String, Ty)>,
    generics: rokc_ast::Generics,
    where_clause: Vec<rokc_ast::WherePred>,
    self_ty: Ty,
    module: String,
    /// A generic trait impl's (or structural inherent impl's) method
    /// templates by name, trait default bodies included.
    methods: HashMap<String, GenericMethod>,
    /// Associated consts, trait defaults included (spec 2.8.3a).
    consts: Vec<AssocConst>,
    /// A concrete impl's resolved target, and the trait ref it provides
    /// (arguments with defaults filled, associated types as bindings).
    concrete: Option<(HirTy, Option<TraitRef>)>,
}

impl Impl {
    fn is_generic(&self) -> bool {
        !self.generics.params.is_empty()
    }
}

/// Every impl, in source order, with indexes for the concrete ones.
#[derive(Clone, Default)]
struct Impls {
    all: Vec<std::sync::Arc<Impl>>,
    /// Concrete trait impls by target.
    by_target: HashMap<MethodTarget, Vec<usize>>,
    /// Concrete trait impls by trait key, in resolution order.
    by_trait: HashMap<String, Vec<usize>>,
    /// Generic impls by trait key (`""`: inherent), in source order.
    generic_of: HashMap<String, Vec<usize>>,
}

impl Impls {
    /// Every impl, in source order.
    fn iter(&self) -> impl Iterator<Item = &Impl> {
        self.all.iter().map(|i| &**i)
    }

    fn push(&mut self, i: Impl) {
        if i.is_generic() {
            self.generic_of
                .entry(i.trait_key.clone())
                .or_default()
                .push(self.all.len());
        }
        self.all.push(std::sync::Arc::new(i));
    }

    /// Concrete impl `i`'s target and trait ref, once resolved.
    fn resolve(&mut self, i: usize, ty: HirTy, provides: Option<TraitRef>) {
        if let Some(r) = &provides {
            if let Some(t) = method_target_for(&ty) {
                self.by_target.entry(t).or_default().push(i);
            }
            self.by_trait.entry(r.path.clone()).or_default().push(i);
        }
        std::sync::Arc::make_mut(&mut self.all[i]).concrete = Some((ty, provides));
    }

    fn generic(&self) -> impl Iterator<Item = &Impl> {
        incr::read(|| incr::Read::GenericImpls);
        self.iter().filter(|i| i.is_generic())
    }

    /// The generic impls `pick` names.
    fn generic_by<'i: 'p, 'p>(&'i self, pick: Pick<'p>) -> Box<dyn Iterator<Item = &'i Impl> + 'p> {
        match pick {
            Pick::Trait(key) => {
                incr::read(|| incr::Read::GenericImplsOf(key.to_owned()));
                Box::new(
                    self.generic_of
                        .get(key)
                        .map_or(&[][..], Vec::as_slice)
                        .iter()
                        .map(|&i| &*self.all[i]),
                )
            }
            Pick::Method(m) => {
                incr::read(|| incr::Read::GenericImplsWith(m.to_owned()));
                Box::new(
                    self.iter()
                        .filter(move |i| i.is_generic() && i.methods.contains_key(m)),
                )
            }
            Pick::Where(keep) => Box::new(self.generic().filter(move |i| keep(i))),
        }
    }

    /// The impls with associated const `name`.
    fn with_const<'i: 'n, 'n>(&'i self, name: &'n str) -> impl Iterator<Item = &'i Impl> + 'n {
        incr::read(|| incr::Read::ImplConsts(name.to_owned()));
        self.iter()
            .filter(move |i| i.consts.iter().any(|(n, ..)| n == name))
    }

    /// The trait refs of the concrete impls of trait `key` on `ty`.
    fn refs_of<'s>(&'s self, ty: &HirTy, key: &'s str) -> impl Iterator<Item = &'s TraitRef> + 's {
        incr::read(|| incr::Read::ImplsOnOf(ty.clone(), key.to_owned()));
        method_target_for(ty)
            .and_then(|t| self.by_target.get(&t))
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(|&i| self.all[i].concrete.as_ref()?.1.as_ref())
            .filter(move |r| r.path == key)
    }

    /// The trait refs of the concrete impls on `ty`.
    fn refs_on<'s>(&'s self, ty: &HirTy) -> impl Iterator<Item = &'s TraitRef> + use<'s> {
        incr::read(|| incr::Read::ImplsOn(ty.clone()));
        method_target_for(ty)
            .and_then(|t| self.by_target.get(&t))
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(|&i| self.all[i].concrete.as_ref()?.1.as_ref())
    }

    /// The targets of trait `key`'s concrete impls, in resolution order.
    fn tys_of(&self, key: &str) -> impl Iterator<Item = &HirTy> {
        incr::read(|| incr::Read::ImplsOf(key.to_owned()));
        self.by_trait
            .get(key)
            .into_iter()
            .flatten()
            .filter_map(|&i| Some(&self.all[i].concrete.as_ref()?.0))
    }
}

/// The types `auto_trait` is inside of, outermost first, and the
/// outermost one a `true` below assumed (`usize::MAX`: none).
struct AutoSeen {
    open: Vec<HirTy>,
    low: usize,
}

impl Default for AutoSeen {
    fn default() -> Self {
        Self {
            open: Vec::new(),
            low: usize::MAX,
        }
    }
}

/// Which generic impls a lookup wants: of a trait, with a method, or
/// any `keep` takes.
#[derive(Clone, Copy)]
enum Pick<'p> {
    Trait(&'p str),
    Method(&'p str),
    Where(&'p dyn Fn(&Impl) -> bool),
}

/// Every struct / union / enum by id: declared ones and generic instances,
/// made when first named (`instantiate_struct` / `instantiate_enum`).
#[derive(Clone, Default)]
struct TypeDefs<'a> {
    structs: std::cell::RefCell<Vec<HirStructDef>>,
    struct_table: std::cell::RefCell<HashMap<String, StructId>>,
    enums: std::cell::RefCell<Vec<HirEnumDef>>,
    enum_table: std::cell::RefCell<HashMap<String, EnumId>>,
    /// Generic templates by full name.
    generic_structs: HashMap<String, &'a AstStructDef>,
    generic_unions: HashMap<String, &'a rokc_ast::UnionDef>,
    generic_enums: HashMap<String, &'a AstEnumDef>,
    /// Leaf names of templates whose parameters all have defaults: their
    /// bare name is an instance (spec 2.10.5).
    defaulted_leaves: std::collections::HashSet<String>,
}

impl<'a> TypeDefs<'a> {
    fn generics_of(&self, base: &str) -> Option<&rokc_ast::Generics> {
        self.generic_struct(base)
            .map(|d| &d.generics)
            .or_else(|| self.generic_union(base).map(|d| &d.generics))
            .or_else(|| self.generic_enum(base).map(|d| &d.generics))
    }

    /// `base` is a generic struct or union template (their instances are
    /// structs).
    fn is_generic_struct(&self, base: &str) -> bool {
        self.generic_struct(base).is_some() || self.generic_union(base).is_some()
    }

    fn generic_union(&self, base: &str) -> Option<&'a rokc_ast::UnionDef> {
        incr::read(|| incr::Read::GenericUnion(base.to_owned()));
        self.generic_unions.get(base).copied()
    }

    /// Generic struct template `base`.
    fn generic_struct(&self, base: &str) -> Option<&'a AstStructDef> {
        incr::read(|| incr::Read::GenericStruct(base.to_owned()));
        self.generic_structs.get(base).copied()
    }

    fn generic_enum(&self, base: &str) -> Option<&'a AstEnumDef> {
        incr::read(|| incr::Read::GenericEnum(base.to_owned()));
        self.generic_enums.get(base).copied()
    }

    /// A template with leaf name `leaf` has every parameter defaulted.
    fn defaulted(&self, leaf: &str) -> bool {
        incr::read(|| incr::Read::DefaultedLeaf(leaf.to_owned()));
        self.defaulted_leaves.contains(leaf)
    }

    fn struct_at(&self, id: StructId) -> HirStructDef {
        incr::read(|| incr::Read::StructDef(id));
        self.structs.borrow()[id.0 as usize].clone()
    }

    fn enum_at(&self, id: EnumId) -> HirEnumDef {
        incr::read(|| incr::Read::EnumDef(id));
        self.enums.borrow()[id.0 as usize].clone()
    }

    /// `base` written without arguments names its all-defaults instance.
    fn bare_instance(&self, base: &str) -> bool {
        self.generics_of(base).is_some_and(mono::all_defaulted)
    }

    /// Drop the structs and enums from `structs` / `enums` on (instances
    /// a rolled-back lowering made).
    fn truncate(&self, structs: usize, enums: usize) {
        self.structs.borrow_mut().truncate(structs);
        self.struct_table
            .borrow_mut()
            .retain(|_, id| (id.0 as usize) < structs);
        self.enums.borrow_mut().truncate(enums);
        self.enum_table
            .borrow_mut()
            .retain(|_, id| (id.0 as usize) < enums);
        crate::truncate_type_names(structs, enums);
    }

    fn struct_id(&self, key: &str) -> Option<StructId> {
        let id = self.struct_table.borrow().get(key).copied();
        incr::log_type(false, key, id.map(|i| i.0));
        id
    }

    fn enum_id(&self, key: &str) -> Option<EnumId> {
        let id = self.enum_table.borrow().get(key).copied();
        incr::log_type(true, key, id.map(|i| i.0));
        id
    }

    /// Instance `base<args>` of a generic struct or union, existing or new
    /// (at `span`). The id is taken before field types resolve, so a field
    /// naming the instance finds it.
    fn instantiate_struct(
        &self,
        base: &str,
        args: Vec<HirTy>,
        span: rokc_diagnostic::Span,
        res: impl Fn(&TySubst, &Ty) -> HirTy,
    ) -> Option<StructId> {
        let key = mangle_inst(base, &args);
        let found = self.struct_table.borrow().get(&key).copied();
        incr::log(
            || format!("ts|{key}"),
            || incr::Lookup::StructInst(base.to_owned(), args.clone(), found),
        );
        if let Some(id) = found {
            return Some(id);
        }
        let tmpl = match self.generic_struct(base) {
            Some(d) => Ok(d),
            None => Err(self.generic_union(base)?),
        };
        let (generics, attrs) = match tmpl {
            Ok(d) => (&d.generics, &d.attrs),
            Err(u) => (&u.generics, &u.attrs),
        };
        let id = StructId(u32::try_from(self.structs.borrow().len()).expect("StructId overflow"));
        crate::name_type(false, id.0, base, &args);
        self.struct_table.borrow_mut().insert(key.clone(), id);
        self.structs.borrow_mut().push(HirStructDef {
            id,
            repr: collect::repr_of(attrs).unwrap_or_default(),
            def_id: None,
            name: key.clone(),
            fields: Vec::new(),
            span,
            generic_args: args.clone(),
            union_kind: None,
            niche: collect::niche_args(attrs).unwrap_or_default(),
        });
        let subst = generic_subst(generics, &args);
        let res = |t: &Ty| res(&subst, t);
        match tmpl {
            Ok(d) => {
                let fields = generic_struct_fields(d, &res);
                self.structs.borrow_mut()[id.0 as usize].fields = fields;
            }
            Err(u) => {
                // Declaration errors were reported at the template.
                let inst = collect::union_def(u, id, key, &res, args, &mut Vec::new());
                self.structs.borrow_mut()[id.0 as usize] = HirStructDef { span, ..inst };
            }
        }
        Some(id)
    }

    /// `instantiate_struct` for a generic enum.
    fn instantiate_enum(
        &self,
        base: &str,
        args: Vec<HirTy>,
        span: rokc_diagnostic::Span,
        variants: impl FnOnce(&AstEnumDef, &TySubst) -> Vec<HirEnumVariant>,
    ) -> Option<EnumId> {
        let key = mangle_inst(base, &args);
        let found = self.enum_table.borrow().get(&key).copied();
        incr::log(
            || format!("te|{key}"),
            || incr::Lookup::EnumInst(base.to_owned(), args.clone(), found),
        );
        if let Some(id) = found {
            return Some(id);
        }
        let def = self.generic_enum(base)?;
        let id = EnumId(u32::try_from(self.enums.borrow().len()).expect("EnumId overflow"));
        crate::name_type(true, id.0, base, &args);
        self.enum_table.borrow_mut().insert(key.clone(), id);
        self.enums.borrow_mut().push(HirEnumDef {
            id,
            repr: collect::repr_of(&def.attrs).unwrap_or_default(),
            def_id: None,
            name: key,
            variants: Vec::new(),
            span,
            generic_args: args.clone(),
        });
        let variants = variants(def, &generic_subst(&def.generics, &args));
        self.enums.borrow_mut()[id.0 as usize].variants = variants;
        Some(id)
    }
}

/// Read-only lowering environment shared by every body (plus the
/// interior-mutable `sink`). Inputs live for `'s`, item tables for `'a`.
struct EnvIn<'a, 's> {
    /// The compile's source files (`#[track_caller]` / `throw` sites).
    sources: &'s rokc_diagnostic::SourceMap,
    prec: &'s PrecTable,
    t: Tables<'a>,
}

type Env<'a> = EnvIn<'a, 'a>;

impl<'a> std::ops::Deref for EnvIn<'a, '_> {
    type Target = Tables<'a>;
    fn deref(&self) -> &Tables<'a> {
        &self.t
    }
}

impl std::ops::DerefMut for EnvIn<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.t
    }
}

/// What collecting the items makes (`Env`'s tables).
#[derive(Clone)]
struct Tables<'a> {
    /// Shared: the tables of a kept library stage are cloned per compile.
    fn_table: HashMap<String, std::sync::Arc<FnSig>>,
    /// `fn_table` and `generic_fns` keys by last segment (both are
    /// complete before any body is lowered).
    fn_leaves: std::cell::OnceCell<Leaves>,
    /// `extern` fns: one global C namespace callable by bare name (spec
    /// 5.9); name → declaring module (where its signature resolves).
    extern_fns: HashMap<String, String>,
    /// Concrete methods by `(type, name)`: several when traits share a
    /// method name (`pick_method` chooses, spec 4.9.6).
    method_table: HashMap<(MethodTarget, String), Vec<FunctionId>>,
    /// `fn_table` key of every function id (method signatures).
    key_of: HashMap<FunctionId, String>,
    /// Declarations whose return type is inferred from the body.
    infer_ret: std::collections::HashSet<FunctionId>,
    /// `RootMain::Implicit`.
    implicit_main: bool,
    /// The implicit `main`'s top-level `let`s: name → the `const` that
    /// would make it visible to a `fn` (a hint for a `fn` that names one).
    main_lets: HashMap<String, String>,
    /// `RootMain::Harness`.
    harness_main: bool,
    /// Bodies without `-> T` that may diverge on every path
    /// (`collect::may_diverge`), by span.
    may_diverge: std::collections::HashSet<rokc_diagnostic::Span>,
    /// Definitions a later item of the same name repeats (reported).
    duplicated: std::cell::RefCell<std::collections::HashSet<rokc_diagnostic::Span>>,
    method_info: HashMap<FunctionId, MethodInfo>,
    /// Every struct / union / enum, declared or instantiated.
    defs: TypeDefs<'a>,
    /// Module-level `const NAME: T = expr;` items. A bare `Path(NAME)`
    /// resolves to the const's value expression inlined at the use site.
    consts: HashMap<String, (Ty, Expr)>,
    const_leaves: std::cell::RefCell<(usize, Leaves)>,
    /// Generic function templates keyed by name. `lower_call` queries
    /// this when the callee path matches a generic function.
    generic_fns: HashMap<String, &'a FnDef>,
    /// Generic-impl and generic methods by (impl target's base, name),
    /// called via `monomorphize` when `method_table` has none.
    generic_methods: HashMap<(String, String), Vec<GenericMethod>>,
    /// `#[intrinsic("rok.OP.TYPE")]` methods (`Prim` / `Convert`): calls
    /// lower to the op.
    intrinsics: HashMap<FunctionId, collect::Intrinsic>,
    /// Generic `#[intrinsic]` functions by name.
    generic_intrinsics: HashMap<String, collect::Intrinsic>,
    /// `extern "C"` functions: a `fn` argument must be a named fn (a C
    /// function pointer), not a closure or `fn` variable.
    extern_fn_ids: std::collections::HashSet<FunctionId>,
    /// `extern "rok"` declarations: the runtime's own functions.
    rok_externs: std::collections::HashSet<FunctionId>,
    /// C variadic `extern` declarations (`fn printf(fmt: str, ...)`):
    /// extra arguments take the C default argument promotions.
    c_variadic: std::collections::HashSet<FunctionId>,
    /// `extern "C" fn` definitions (spec 5.9.10): with `extern_fn_ids`,
    /// the named functions an `extern "C" fn(..)` value can point to.
    c_defs: std::collections::HashSet<FunctionId>,
    /// `extern "system"` declarations and definitions where `"system"`
    /// is `stdcall` (else empty): `extern "system" fn(..)` values.
    stdcall_fns: std::collections::HashSet<FunctionId>,
    sink: ClosureSink,
    /// Visibility of every value-namespace item, keyed by full name.
    item_visibility: HashMap<String, Visibility>,
    /// Struct name → field name → visibility.
    field_vis: HashMap<String, HashMap<String, Visibility>>,
    /// Every impl (bounds, dispatch, projections, associated consts).
    impls: Impls,
    /// `unsafe fn`s and C functions (spec 4.15): calls need an unsafe scope.
    unsafe_fns: std::collections::HashSet<FunctionId>,
    /// `impl !Send for S {}` opt-outs: `(generic base of S, trait key)`.
    neg_auto: std::collections::HashSet<(String, String)>,
    /// `auto_trait` answers by type and trait, while the program no
    /// longer changes (`send_check`); `None` otherwise.
    auto_memo: std::cell::RefCell<Option<HashMap<String, HashMap<HirTy, bool>>>>,
    /// `affine::affine` answers by type.
    affine_memo: std::cell::RefCell<HashMap<HirTy, Option<affine::Kind>>>,
    /// Trait key → declared methods + supertraits (`any Tr` calls).
    trait_info: HashMap<String, TraitInfo>,
    /// Module-level `static` items by name → declared type.
    statics: HashMap<String, HirTy>,
    static_leaves: std::cell::RefCell<(usize, Leaves)>,
    /// Statics that may be assigned (`extern` `static mut`, spec 5.9.3).
    mut_statics: std::collections::HashSet<String>,
    /// Statics of `extern` blocks (spec 5.9.3), C globals.
    extern_statics: std::collections::HashSet<String>,
    /// `#[literal]` handlers of the built-in literal shapes (spec 6.5).
    literal_shapes: Vec<ShapeHandler>,
    names: TypeNames,
    /// Every non-generic function by `FunctionId` (the ids below
    /// `sink.next_id`'s start).
    decls: Vec<MethodOrFn<'a>>,
    /// `#[non_exhaustive]` structs and enums (spec 6.2.10), by name.
    non_exhaustive: std::collections::HashSet<String>,
    /// `#[sealed]` traits (spec 6.2.15), by name.
    sealed: std::collections::HashSet<String>,
    /// Marked functions and methods (spec 6.2.7, 6.2.14): the name a
    /// warning shows (`P::new`), the marks.
    fn_marks: HashMap<FunctionId, (String, crate::marks::Mark)>,
    /// Marked fields (`m::P.x`), consts and statics, by key.
    marks: HashMap<String, crate::marks::Mark>,
    /// The package whose uses of marked items warn: the one built.
    mark_pkg: Option<String>,
    /// `--check-library`: `unused_result` also checks the library's code.
    lint_library: bool,
    /// Per `(trait, method)`: the template a call on a type parameter
    /// bounded by the trait goes through (`bounds::templates`).
    bound_methods: HashMap<(String, String), GenericMethod>,
    /// A `bound_methods` template's name → its trait.
    bound_traits: HashMap<String, String>,
}

/// One of `Env`'s sets of functions.
#[derive(Clone, Copy)]
enum FnSet {
    ExternC,
    RokExtern,
    CVariadic,
    CDef,
    Stdcall,
    Unsafe,
}

impl FnSet {
    const fn of(n: u8) -> Option<Self> {
        Some(match n {
            0 => Self::ExternC,
            1 => Self::RokExtern,
            2 => Self::CVariadic,
            3 => Self::CDef,
            4 => Self::Stdcall,
            5 => Self::Unsafe,
            _ => return None,
        })
    }

    fn contains(self, env: &Env<'_>, id: FunctionId) -> bool {
        match self {
            Self::ExternC => env.extern_fn_ids.contains(&id),
            Self::RokExtern => env.rok_externs.contains(&id),
            Self::CVariadic => env.c_variadic.contains(&id),
            Self::CDef => env.c_defs.contains(&id),
            Self::Stdcall => env.stdcall_fns.contains(&id),
            Self::Unsafe => env.unsafe_fns.contains(&id),
        }
    }
}

/// Body lowering reads the interface tables through these: each read
/// is logged for the incremental store (`incr::Read`).
impl<'a> Env<'a> {
    fn sig(&self, key: &str) -> Option<&FnSig> {
        incr::read(|| incr::Read::Sig(key.to_owned()));
        self.fn_table.get(key).map(|s| &**s)
    }

    fn generic_fn(&self, key: &str) -> Option<&'a FnDef> {
        incr::read(|| incr::Read::Generic(key.to_owned()));
        self.generic_fns.get(key).copied()
    }

    /// Function and generic function keys whose last segment is `leaf`.
    fn fn_leaf(&self, leaf: &str) -> Vec<String> {
        incr::read(|| incr::Read::FnLeaf(leaf.to_owned()));
        self.fn_leaves
            .get_or_init(|| leaves(self.fn_table.keys().chain(self.generic_fns.keys())))
            .get(leaf)
            .cloned()
            .unwrap_or_default()
    }

    /// The module declaring `extern` function `name`.
    fn extern_fn(&self, name: &str) -> Option<&String> {
        incr::read(|| incr::Read::ExternFn(name.to_owned()));
        self.extern_fns.get(name)
    }

    /// The concrete methods `method` of `ty`.
    fn methods_on(&self, ty: &HirTy, method: &str) -> Option<&Vec<FunctionId>> {
        let t = method_target_for(ty)?;
        incr::read(|| incr::Read::Methods(ty.clone(), method.to_owned()));
        self.method_table.get(&(t, method.to_owned()))
    }

    /// Every concrete method.
    fn all_methods(&self) -> &HashMap<(MethodTarget, String), Vec<FunctionId>> {
        incr::read(|| incr::Read::AllMethods);
        &self.method_table
    }

    /// Every generic method template.
    fn all_generic_methods(&self) -> &HashMap<(String, String), Vec<GenericMethod>> {
        incr::read(|| incr::Read::AllGenericMethods);
        &self.generic_methods
    }

    fn method_info(&self, id: FunctionId) -> Option<&MethodInfo> {
        incr::read(|| incr::Read::MethodInfo(id));
        self.method_info.get(&id)
    }

    fn generic_methods(&self, base: &str, method: &str) -> Option<&Vec<GenericMethod>> {
        incr::read(|| incr::Read::GenericMethods(base.to_owned(), method.to_owned()));
        self.generic_methods
            .get(&(base.to_owned(), method.to_owned()))
    }

    /// The `fn_table` key of function `id`.
    fn fn_key(&self, id: FunctionId) -> Option<&String> {
        incr::read(|| incr::Read::KeyOf(id));
        self.key_of.get(&id)
    }

    /// Declaration `id`'s return type is inferred from its body.
    fn infers_ret(&self, id: FunctionId) -> bool {
        incr::read(|| incr::Read::InferRet(id));
        self.infer_ret.contains(&id)
    }

    /// The body at `at` may diverge on every path (`collect::may_diverge`).
    fn diverges(&self, at: Span) -> bool {
        incr::read(|| incr::Read::MayDiverge(at));
        self.may_diverge.contains(&at)
    }

    /// The definition at `at` is repeated later: its calls report no
    /// argument errors. Not a logged read: no unit is made while any exist.
    fn duplicated(&self, at: Span) -> bool {
        self.duplicated.borrow().contains(&at)
    }

    fn intrinsic(&self, id: FunctionId) -> Option<&collect::Intrinsic> {
        incr::read(|| incr::Read::Intrinsic(id));
        self.intrinsics.get(&id)
    }

    fn generic_intrinsic(&self, name: &str) -> Option<&collect::Intrinsic> {
        incr::read(|| incr::Read::GenericIntrinsic(name.to_owned()));
        self.generic_intrinsics.get(name)
    }

    fn is(&self, set: FnSet, id: FunctionId) -> bool {
        incr::read(|| incr::Read::FnIn(set as u8, id));
        set.contains(self, id)
    }

    /// The visibility of value item `key`.
    fn item_vis(&self, key: &str) -> Option<Visibility> {
        incr::read(|| incr::Read::ItemVis(key.to_owned()));
        self.item_visibility.get(key).copied()
    }

    fn fields_vis(&self, ty: &str) -> Option<&HashMap<String, Visibility>> {
        incr::read(|| incr::Read::FieldVis(ty.to_owned()));
        self.field_vis.get(ty)
    }

    fn konst(&self, key: &str) -> Option<&(Ty, Expr)> {
        incr::read(|| incr::Read::Const(key.to_owned()));
        self.consts.get(key)
    }

    fn static_ty(&self, key: &str) -> Option<&HirTy> {
        incr::read(|| incr::Read::Static(key.to_owned()));
        self.statics.get(key)
    }

    fn is_mut_static(&self, key: &str) -> bool {
        incr::read(|| incr::Read::MutStatic(key.to_owned()));
        self.mut_statics.contains(key)
    }

    fn is_extern_static(&self, key: &str) -> bool {
        incr::read(|| incr::Read::ExternStatic(key.to_owned()));
        self.extern_statics.contains(key)
    }

    fn trait_of(&self, key: &str) -> Option<&TraitInfo> {
        incr::read(|| incr::Read::TraitInfo(key.to_owned()));
        self.trait_info.get(key)
    }

    /// The keys of the statics (else consts) whose last segment is `leaf`.
    fn value_leaf(&self, statics: bool, leaf: &str) -> Vec<String> {
        let (index, len) = if statics {
            (&self.static_leaves, self.statics.len())
        } else {
            (&self.const_leaves, self.consts.len())
        };
        let mut index = index.borrow_mut();
        if index.0 != len {
            let keys = if statics {
                leaves(self.statics.keys())
            } else {
                leaves(self.consts.keys())
            };
            *index = (len, keys);
        }
        index.1.get(leaf).cloned().unwrap_or_default()
    }

    /// `impl !key for base {}`.
    fn opts_out(&self, base: &str, key: &str) -> bool {
        incr::read(|| incr::Read::NegAuto(base.to_owned(), key.to_owned()));
        self.neg_auto.contains(&(base.to_owned(), key.to_owned()))
    }

    fn is_non_exhaustive(&self, name: &str) -> bool {
        incr::read(|| incr::Read::NonExhaustive(name.to_owned()));
        self.non_exhaustive.contains(name)
    }

    fn is_sealed(&self, name: &str) -> bool {
        incr::read(|| incr::Read::Sealed(name.to_owned()));
        self.sealed.contains(name)
    }

    /// Function `id`'s marks, with the name a warning shows.
    fn fn_mark(&self, id: FunctionId) -> Option<&(String, crate::marks::Mark)> {
        incr::read(|| incr::Read::FnMark(id));
        self.fn_marks.get(&id)
    }

    /// The marks of field, const or static `key`.
    fn mark(&self, key: &str) -> Option<&crate::marks::Mark> {
        incr::read(|| incr::Read::Mark(key.to_owned()));
        self.marks.get(key)
    }

    fn bound_method(&self, tr: &str, method: &str) -> Option<&GenericMethod> {
        incr::read(|| incr::Read::BoundMethod(tr.to_owned(), method.to_owned()));
        self.bound_methods.get(&(tr.to_owned(), method.to_owned()))
    }

    /// The trait of `bound_methods` template `name`.
    fn bound_trait(&self, name: &str) -> Option<&String> {
        incr::read(|| incr::Read::BoundTrait(name.to_owned()));
        self.bound_traits.get(name)
    }

    /// Module `module`'s `use` entries.
    fn uses_of(&self, module: &str) -> Option<&Vec<crate::uses::UseEntry>> {
        incr::read(|| incr::Read::Uses(module.to_owned()));
        self.names.uses.get(module)
    }

    fn decl(&self, id: FunctionId) -> Option<&MethodOrFn<'a>> {
        incr::read(|| incr::Read::Decl(id));
        self.decls.get(id.0 as usize)
    }
}

impl ClosureSink {
    fn new() -> Self {
        Self {
            extras: std::cell::RefCell::new(Vec::new()),
            next_id: std::cell::Cell::new(0),
            mono_cache: std::cell::RefCell::new(HashMap::new()),
            mono_sigs: std::cell::RefCell::new(HashMap::new()),
            mono_depth: std::cell::Cell::new(0),
            uninferred: std::cell::Cell::new(0),
            any_tags: std::cell::RefCell::new(Vec::new()),
            ref_out: std::cell::RefCell::new(HashMap::new()),
            any_dispatch: std::cell::RefCell::new(Vec::new()),
            projecting: std::cell::RefCell::new(Vec::new()),
            thunks: std::cell::RefCell::new(HashMap::new()),
            instances: std::cell::RefCell::new(Vec::new()),
            instance_warns: std::cell::RefCell::new(InstanceWarns::default()),
            diagnostics: std::cell::RefCell::new(Vec::new()),
            infer_vars: std::cell::RefCell::new(Vec::new()),
            infer_depth: std::cell::Cell::new(0),
            infer_floor: std::cell::Cell::new(0),
            fn_origin: std::cell::RefCell::new(HashMap::new()),
            track_caller: std::cell::RefCell::new(std::collections::HashSet::new()),
            const_lowering: std::cell::RefCell::new(std::collections::HashSet::new()),
            const_stack: std::cell::RefCell::new(Vec::new()),
            inferred_ret: std::cell::RefCell::default(),
            inferred_fns: std::cell::RefCell::default(),
            bodies_done: std::cell::Cell::new(false),
            sig_pass: std::cell::Cell::new(None),
            recursive_ret: std::cell::RefCell::default(),
            call_sites: std::cell::RefCell::default(),
            tentative_ret: std::cell::RefCell::default(),
            params: std::cell::RefCell::default(),
            failed: std::cell::RefCell::default(),
            bodies: std::cell::RefCell::default(),
            assumed: std::cell::RefCell::default(),
            const_names: std::cell::RefCell::default(),
            dropped_at: std::cell::Cell::new(None),
        }
    }
}

impl Tables<'_> {
    /// No item collected yet.
    fn empty() -> Self {
        Self {
            fn_table: HashMap::new(),
            fn_leaves: std::cell::OnceCell::new(),
            extern_fns: HashMap::new(),
            method_table: HashMap::new(),
            key_of: HashMap::new(),
            infer_ret: std::collections::HashSet::new(),
            implicit_main: false,
            main_lets: HashMap::new(),
            harness_main: false,
            may_diverge: std::collections::HashSet::new(),
            duplicated: std::cell::RefCell::default(),
            method_info: HashMap::new(),
            defs: TypeDefs::default(),
            consts: HashMap::new(),
            const_leaves: std::cell::RefCell::default(),
            generic_fns: HashMap::new(),
            generic_methods: HashMap::new(),
            intrinsics: HashMap::new(),
            generic_intrinsics: HashMap::new(),
            extern_fn_ids: std::collections::HashSet::new(),
            rok_externs: std::collections::HashSet::new(),
            c_variadic: std::collections::HashSet::new(),
            c_defs: std::collections::HashSet::new(),
            stdcall_fns: std::collections::HashSet::new(),
            sink: ClosureSink::new(),
            item_visibility: HashMap::new(),
            field_vis: HashMap::new(),
            impls: Impls::default(),
            unsafe_fns: std::collections::HashSet::new(),
            neg_auto: std::collections::HashSet::new(),
            auto_memo: std::cell::RefCell::default(),
            affine_memo: std::cell::RefCell::default(),
            trait_info: HashMap::new(),
            statics: HashMap::new(),
            static_leaves: std::cell::RefCell::default(),
            mut_statics: std::collections::HashSet::new(),
            extern_statics: std::collections::HashSet::new(),
            literal_shapes: Vec::new(),
            names: TypeNames::empty(),
            decls: Vec::new(),
            non_exhaustive: std::collections::HashSet::new(),
            sealed: std::collections::HashSet::new(),
            fn_marks: HashMap::new(),
            marks: HashMap::new(),
            mark_pkg: None,
            lint_library: false,
            bound_methods: HashMap::new(),
            bound_traits: HashMap::new(),
        }
    }
}

impl TypeNames {
    /// No name declared yet.
    fn empty() -> Self {
        Self {
            names: std::collections::HashSet::new(),
            traits: std::collections::HashSet::new(),
            auto_traits: std::collections::HashSet::new(),
            lang: rokc_parser::LangTable::default(),
            variants: std::collections::HashSet::new(),
            reexports: HashMap::new(),
            visibility: HashMap::new(),
            mod_vis: HashMap::new(),
            uses: HashMap::new(),
            diags: std::cell::RefCell::new(Vec::new()),
            generic_aliases: HashMap::new(),
            plain_aliases: HashMap::new(),
            alias_depth: std::cell::Cell::new(0),
            broken_aliases: std::cell::RefCell::default(),
            alias_uses: std::cell::RefCell::new(Vec::new()),
            unknown_spans: std::cell::RefCell::default(),
            bare_generics: std::cell::RefCell::default(),
            canon_memo: std::cell::RefCell::default(),
            leaves: std::cell::OnceCell::new(),
            variant_leaves: std::cell::OnceCell::new(),
            consts: HashMap::new(),
            const_leaves: std::cell::OnceCell::new(),
            value_reexports: HashMap::new(),
        }
    }
}

/// A handler registered with a built-in shape's pattern (`[..]` etc.).
#[derive(Clone)]
struct ShapeHandler {
    shape: rokc_parser::Shape,
    /// The handler's full name.
    name: String,
    priority: i64,
    /// The `#[literal]` attribute.
    span: rokc_diagnostic::Span,
}

/// What `any Tr` needs from a trait declaration.
#[derive(Clone)]
struct TraitInfo {
    methods: Vec<String>,
    supers: Vec<String>,
    /// Per method, what its signature past `self` depends on (`Self`,
    /// `Self::X`, trait parameters); `any Tr` dispatch needs the bound to
    /// fix each.
    needs: HashMap<String, Vec<String>>,
    /// Method declarations (signature when no concrete impl exists).
    decls: HashMap<String, rokc_ast::FnDef>,
    /// The trait's module (to resolve `decls`).
    module: String,
    /// The trait's type parameters, in order (`Index<K>` → `[K]`).
    params: Vec<String>,
    /// Its generic parameters as declared (defaults included).
    generics: rokc_ast::Generics,
    /// Its supertraits as written.
    super_tys: Vec<Ty>,
    /// Its associated types and their declared bounds.
    assoc: Vec<(String, Vec<Ty>)>,
    /// Its associated consts and their declared types.
    consts: Vec<(String, Ty)>,
}

#[derive(Clone, Eq, Hash, PartialEq, PartialOrd, Ord)]
enum MethodTarget {
    Struct(StructId),
    Enum(EnumId),
    /// Impl targets that aren't one struct or enum (spec 2.9.1), keyed by
    /// `ty_mangle` so impls on one shape share a target.
    Mangled(String),
}

#[derive(Clone)]
enum MethodOrFn<'a> {
    Fn(&'a FnDef),
    /// `module` is where the body's names resolve: the impl's module
    /// for explicit methods, the trait's module for default bodies.
    Method {
        /// Borrowed from the program, or a trait default with the
        /// impl's trait arguments substituted into its signature.
        fn_def: std::borrow::Cow<'a, FnDef>,
        self_ty: HirTy,
        module: String,
        /// `fn_table` key, also the function's name.
        key: String,
        /// A trait default body's trait parameters → this impl's
        /// arguments (`trait Dim<const D: usize>` + `impl Dim<3>`).
        subst: HashMap<String, HirTy>,
    },
}

/// Where a free function's names resolve: its module, or for an
/// `extern` declaration (not a module item) the module declaring it.
fn fn_module(env: &Env<'_>, f: &FnDef) -> String {
    env.extern_fn(&f.name)
        .cloned()
        .unwrap_or_else(|| module_of(&f.name))
}

impl MethodOrFn<'_> {
    fn def(&self) -> &FnDef {
        match self {
            Self::Fn(f) => f,
            Self::Method { fn_def, .. } => fn_def,
        }
    }
}

/// A function with `ref` parameters (spec 4.7), copy-in / copy-out: it
/// returns `ret` packed with each `ref` parameter's final value
/// (`ref_packed`), which the call site writes back.
#[derive(Clone)]
struct RefOut {
    /// The declared return (a `Throws` carrier for a throwing fn).
    ret: HirTy,
    /// `(parameter index, type)` of each `ref` parameter.
    refs: Vec<(usize, HirTy)>,
    /// An `async` method's `ref self`: `refs` hold the parameters' cells
    /// (`GcBuf<T>`), read back once the future completes (`ref_unpack`).
    deferred: bool,
}

/// A projection being resolved: base type, associated name, trait filter.
type ProjectionGoal = (HirTy, String, Option<Vec<String>>);

/// Per-function signature for lowering; `param_defaults[i]` is the
/// parameter's default expression, if any.
#[derive(Clone)]
struct FnSig {
    id: FunctionId,
    ret_ty: HirTy,
    param_defaults: Vec<Option<Expr>>,
    /// Resolved parameter types: the expected types of the arguments, so
    /// an untyped closure argument takes its parameter types from them.
    param_tys: Vec<HirTy>,
    /// Parameter names, to place named arguments (spec 4.1).
    param_names: Vec<String>,
    /// Which parameters are `ref` (empty = none).
    param_refs: Vec<bool>,
    /// The last parameter is a variadic `...T` (a `Slice<T>` the call
    /// builds from its trailing arguments, spec 4.2.5).
    variadic: bool,
}

/// The lock guard `ty` is, if any. Only `using` releases one (stdlib
/// 2.16.3); the module defining guards hands them over raw.
fn lock_guard(ctx: &LowerCtx<'_>, ty: &HirTy) -> Option<&'static str> {
    let HirTy::Struct(id) = ty else { return None };
    if ctx.current_module == "rok::core::sync" {
        return None;
    }
    let def = ctx.struct_def(*id);
    let name = def.name.split("__").next().unwrap_or(&def.name);
    ["MutexGuard", "RwLockReadGuard", "RwLockWriteGuard"]
        .into_iter()
        .find(|g| name.strip_prefix("rok::core::sync::") == Some(g))
}

// If `slot_ty` is a Sum and `value.ty` matches one of its variants,
// wrap the value into the sum's tagged-union representation.
fn wrap_into_sum(slot_ty: &HirTy, value: HirExpr) -> HirExpr {
    let HirTy::Sum(variants) = slot_ty else {
        return value;
    };
    let Some(idx) = sum_variant_index(variants, &value.ty) else {
        // A nested optional (spec 2.6.4): a value of the inner type's
        // members goes in wrapped by the inner sum.
        return match variants.iter().find(|v| nested_fits(v, &value.ty)) {
            Some(inner) => wrap_into_sum(slot_ty, wrap_into_sum(inner, value)),
            None => value,
        };
    };
    let span = value.span;
    if variants[idx].is_none() {
        return wrap_none(slot_ty, idx, value);
    }
    HirExpr {
        kind: HirExprKind::SumWrap {
            sum_ty: slot_ty.clone(),
            variant_index: u32::try_from(idx).expect("variant overflow"),
            value: Some(Box::new(value)),
        },
        ty: slot_ty.clone(),
        span,
    }
}

/// `value: none` as member `idx` of `slot_ty`. The value may have effects
/// (`{ f(); none }`), so the wrap moves into its branches or the value
/// runs first.
fn wrap_none(slot_ty: &HirTy, idx: usize, mut value: HirExpr) -> HirExpr {
    let span = value.span;
    let tag = HirExpr {
        kind: HirExprKind::SumWrap {
            sum_ty: slot_ty.clone(),
            variant_index: u32::try_from(idx).expect("variant overflow"),
            value: None,
        },
        ty: slot_ty.clone(),
        span,
    };
    let wrap_block = |b: &mut HirBlock| {
        if let Some(t) = b.trailing.take() {
            b.trailing = Some(wrap_none(slot_ty, idx, t));
        }
    };
    match &mut value.kind {
        HirExprKind::Const(_) | HirExprKind::Local(_) | HirExprKind::UnresolvedPath(_) => tag,
        HirExprKind::Block(b) => {
            wrap_block(b);
            value.ty = slot_ty.clone();
            value
        }
        HirExprKind::If {
            then_block,
            else_branch: Some(e),
            ..
        } => {
            wrap_block(then_block);
            let inner = std::mem::replace(&mut **e, tag);
            **e = wrap_none(slot_ty, idx, inner);
            value.ty = slot_ty.clone();
            value
        }
        HirExprKind::Match { arms, .. } => {
            for a in arms.iter_mut() {
                let body = std::mem::replace(&mut a.body, tag.clone());
                a.body = wrap_none(slot_ty, idx, body);
            }
            value.ty = slot_ty.clone();
            value
        }
        _ => HirExpr {
            kind: HirExprKind::Block(Box::new(HirBlock {
                stmts: vec![HirStmt::Expr(value)],
                trailing: Some(tag),
                label: None,
            })),
            ty: slot_ty.clone(),
            span,
        },
    }
}

/// Re-tag a sum as `to`, member by member. Widening is total; arms left
/// out for a sub-sum type pattern can't occur.
fn resum(ctx: &mut LowerCtx<'_>, value: HirExpr, to: &HirTy) -> HirExpr {
    // An `any` value (narrowed by `x: A | B`) is matched by tag.
    let members: Vec<(u32, HirTy)> = match (&value.ty, to) {
        (HirTy::Sum(vs), HirTy::Sum(ws)) => vs
            .iter()
            .enumerate()
            .filter(|(_, v)| ws.contains(v))
            .map(|(i, v)| (u32::try_from(i).expect("variant overflow"), v.clone()))
            .collect(),
        // A stored `to` value, or one of its members.
        (HirTy::AnyTrait(_), HirTy::Sum(ws)) => std::iter::once(to)
            .chain(ws)
            .map(|w| (ctx.any_tag(w), w.clone()))
            .collect(),
        // A plain member: wrap it.
        _ => return wrap_into_sum(to, value),
    };
    let span = value.span;
    let arms = members
        .into_iter()
        .map(|(i, v)| {
            let b = ctx.declare(None, v.clone());
            HirMatchArm {
                pat: HirPattern::SumVariant {
                    sum_ty: value.ty.clone(),
                    variant_index: i,
                    binder: Some(b),
                    payload_ty: Some(v.clone()),
                },
                guard: None,
                body: wrap_into_sum(to, hir_local(b, &v, span)),
            }
        })
        .collect();
    HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(value),
            arms,
        },
        ty: to.clone(),
        span,
    }
}

/// Does nested sum `member` take a `value_ty` by injection (`i32` or
/// `none` into `(i32 | none)?`, spec 2.6.4)?
fn nested_fits(member: &HirTy, value_ty: &HirTy) -> bool {
    match member {
        HirTy::Sum(ws) => {
            sum_variant_index(ws, value_ty).is_some() || ws.iter().any(|w| nested_fits(w, value_ty))
        }
        _ => false,
    }
}

// Locate `value_ty` in a sum's variant list (structural equality).
fn sum_variant_index(variants: &[HirTy], value_ty: &HirTy) -> Option<usize> {
    variants.iter().position(|v| v == value_ty)
}

/// Can `actual` flow into `expected` (after `wrap_into_sum`)? `Unknown`
/// (already reported) and `Any` fit everything.
fn ty_compatible(expected: &HirTy, actual: &HirTy) -> bool {
    let all = |a: &[HirTy], b: &[HirTy]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| ty_compatible(x, y))
    };
    let exact = |a: &[HirTy], b: &[HirTy]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| ty_exact(x, y))
    };
    match (expected, actual) {
        // `!` (a diverging expression) fits any slot.
        (HirTy::Unknown | HirTy::Any, _) | (_, HirTy::Unknown | HirTy::Any | HirTy::Never) => true,
        (HirTy::Sum(vs), a) if !matches!(a, HirTy::Sum(_)) => sum_variant_index(vs, a).is_some(),
        (HirTy::Tuple(a), HirTy::Tuple(b)) | (HirTy::Sum(a), HirTy::Sum(b)) => all(a, b),
        (HirTy::Array(a, n), HirTy::Array(b, m)) => n == m && ty_compatible(a, b),
        (HirTy::GcBuf(a), HirTy::GcBuf(b)) => ty_compatible(a, b),
        // No fn subtyping (spec 4.4.9), except `fn() + Send` into `fn()`.
        // A `fn() -> !` isn't a `fn() -> R` (different return ABI); only
        // `adapt_never_fn` makes one from a named fn.
        (HirTy::Fn(a1, r1, b1), HirTy::Fn(a2, r2, b2)) => {
            exact(a1, a2)
                && ty_exact(r1, r2)
                && (**r2 != HirTy::Never || **r1 == HirTy::Never)
                && fn_bounds_fit(b1, b2)
        }
        // Spec 4.4.2: a closure is a value of its `fn` type (`coerce`
        // checks a bound against its captures).
        (
            HirTy::Fn(a1, r1, _),
            HirTy::Closure {
                args: a2, ret: r2, ..
            },
        ) => exact(a1, a2) && ty_exact(r1, r2),
        // Error types must match exactly: a wider error slot has another
        // carrier layout (fn values are adapted by `adapt_fn_value`).
        (HirTy::Throws { ok: o1, err: e1 }, HirTy::Throws { ok: o2, err: e2 }) => {
            ty_compatible(o1, o2) && (e1 == e2 || **e1 == HirTy::Unknown || **e2 == HirTy::Unknown)
        }
        _ => expected == actual,
    }
}

/// Type identity inside a fn type: `ty_compatible` without sum
/// widening (an unresolved type still matches anything).
fn ty_exact(a: &HirTy, b: &HirTy) -> bool {
    let all = |a: &[HirTy], b: &[HirTy]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| ty_exact(x, y))
    };
    match (a, b) {
        (HirTy::Unknown | HirTy::Any, _) | (_, HirTy::Unknown | HirTy::Any) => true,
        (HirTy::Tuple(a), HirTy::Tuple(b)) | (HirTy::Sum(a), HirTy::Sum(b)) => all(a, b),
        (HirTy::Array(a, n), HirTy::Array(b, m)) => n == m && ty_exact(a, b),
        (HirTy::GcBuf(a), HirTy::GcBuf(b)) => ty_exact(a, b),
        (HirTy::Fn(..), HirTy::Fn(..) | HirTy::Closure { .. }) => ty_compatible(a, b),
        (HirTy::Throws { ok: o1, err: e1 }, HirTy::Throws { ok: o2, err: e2 }) => {
            ty_exact(o1, o2) && ty_exact(e1, e2)
        }
        _ => a == b,
    }
}

/// Wrap `value` into `slot` (sum injection) and report a type mismatch.
/// `what` names the slot for the diagnostic ("argument 1 of `f`").
fn coerce(
    ctx: &mut LowerCtx<'_>,
    slot: &HirTy,
    value: HirExpr,
    what: &dyn Fn() -> String,
) -> HirExpr {
    let ann = ctx.let_ann.take();
    if infer::unify_value(ctx, slot, &value) {
        return value;
    }
    // A diverging block has type `!`: it fits any slot (spec 3.9).
    if hir_expr_diverges(&value) {
        return value;
    }
    let value = c_fn_value(ctx, slot, value);
    let value = adapt_never_fn(ctx, slot, value);
    let value = adapt_fn_value(ctx, slot, value);
    let value = box_into_any(ctx, slot, value);
    let value = drop_fn_bounds(slot, value);
    let value = expr::list_into_view(ctx, slot, value);
    // Spec 2.6.4: a sum flows into any sum holding all its members; one
    // that is a layer of the slot goes into that layer.
    let value = match (&value.ty, slot) {
        (HirTy::Sum(vs), HirTy::Sum(ws))
            if vs != ws && !ws.contains(&value.ty) && vs.iter().all(|v| ws.contains(v)) =>
        {
            resum(ctx, value, slot)
        }
        // Into a nested optional's inner sum (spec 2.6.4): a sub-sum of
        // it re-tagged, then wrapped by the outer layer.
        (HirTy::Sum(vs), HirTy::Sum(ws)) if !ws.contains(&value.ty) => {
            let inner = ws
                .iter()
                .find(|w| matches!(w, HirTy::Sum(is) if vs.iter().all(|v| is.contains(v))));
            match inner.cloned() {
                Some(inner) => resum(ctx, value, &inner),
                None => value,
            }
        }
        _ => value,
    };
    let mut v = wrap_into_sum(slot, value);
    // Same `{code, env}` layout: the closure takes the slot's type once
    // its captures meet the slot's bounds (`+ Send`).
    let closure_fits = |v: &HirExpr| match (slot, &v.ty) {
        (HirTy::Fn(_, _, want), HirTy::Closure { .. }) => {
            ty_compatible(slot, &v.ty) && want.auto.iter().all(|b| ctx.has_trait(&v.ty, b))
        }
        _ => false,
    };
    if closure_fits(&v) {
        v.ty = slot.clone();
    }
    // An unknown type name is reported on its own.
    let unknown_slot = matches!(slot, HirTy::Path(_)) && {
        let mut names = Vec::new();
        collect::collect_unknown_paths(slot, &mut names);
        !names.is_empty()
    };
    if !unknown_slot
        && (!ty_compatible(slot, &v.ty)
            || matches!(v.ty, HirTy::Closure { .. } if matches!(slot, HirTy::Fn(..))))
    {
        if let (HirTy::Fn(_, _, want), Some((name, x))) = (slot, ctx.ref_capture(&v)) {
            if !want.auto.is_empty() {
                ctx.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!(
                        "`[ref {name}]` can't be used in a closure that may run on another \
                         thread: both threads could change `{name}` at once (spec 4.6.9)"
                    ),
                );
                if let Some(d) = ctx.env.sink.diagnostics.borrow_mut().last_mut() {
                    d.notes.push(ref_capture_help(&name, &x));
                }
                return v;
            }
        }
        let hint = fn_mismatch_hint(slot, &v.ty);
        // Underline the value, not the statement holding it.
        let here = ctx.here();
        let at = if !v.span.is_dummy() && v.span.file == here.file && here.start <= v.span.start {
            v.span
        } else {
            here
        };
        ctx.emit_error_at(
            rokc_diagnostic::ErrorKind::Type,
            at,
            format!(
                "mismatched types: {} expects `{}`, found `{}`{hint}",
                what(),
                ctx.ty_name(slot),
                ctx.ty_name(&v.ty)
            ),
        );
        convert_help(ctx, slot, &v, ann, &what());
        // `let d: Dict<K, V> = {};` (Python, JS): `{}` is an empty block.
        let slot_name = ctx.ty_name(slot);
        let base = slot_name.split('<').next().unwrap_or_default();
        if v.ty == HirTy::Unit && matches!(base, "Dict" | "Set") && ctx.src(v.span) == Some("{}") {
            ctx.help_last(Some(format!(
                "`{{}}` is an empty block, not an empty `{base}`; write `{base}::new()`"
            )));
        }
        mismatch_help(ctx, slot, &v);
        if let (Some(note), Some(d)) = (
            ctx.closure_bound_note(&v),
            ctx.env.sink.diagnostics.borrow_mut().last_mut(),
        ) {
            d.notes.push(note);
            d.notes.push(
                "help: drop the capture: compute the value inside the closure, or capture a \
                 copy that is `Send + Sync` (an array, not a collection)"
                    .to_owned(),
            );
        }
    }
    v
}

/// "`f` takes 2 arguments but 1 was supplied", and a help showing the
/// signature `sig` (`f(a: i32, b: i32)`) when known.
fn arity_msg(
    callee: &str,
    want: usize,
    got: usize,
    sig: Option<String>,
) -> (String, Option<String>) {
    let callee = short_callee(callee);
    let args = |n: usize| {
        if n == 1 {
            "1 argument".to_owned()
        } else {
            format!("{n} arguments")
        }
    };
    let was = if got == 1 { "was" } else { "were" };
    let msg = format!("`{callee}` takes {} but {got} {was} supplied", args(want));
    let help = sig.map(|s| format!("it is declared `{s}`; pass {}", args(want)));
    (msg, help)
}

/// A standard library function by its own name (`println`), a method
/// by its type's (`List::push`).
fn short_callee(callee: &str) -> std::borrow::Cow<'_, str> {
    if !callee.starts_with("rok::") {
        return callee.into();
    }
    let leaf = leaf_name(callee);
    match leaf.split_once("__") {
        Some((ty, method)) => format!("{ty}::{method}").into(),
        None => leaf.into(),
    }
}

/// `f(a: i32, b: i32)` from parameter names and types; `self` and
/// unnamed (`#1`) parameters show their type only, or nothing.
fn sig_text<'n>(callee: &str, params: impl IntoIterator<Item = (&'n str, String)>) -> String {
    let parts: Vec<String> = params
        .into_iter()
        .filter(|(n, _)| *n != "self")
        .map(|(n, t)| {
            if n.starts_with('#') || n.is_empty() {
                t
            } else {
                format!("{n}: {t}")
            }
        })
        .collect();
    format!("{}({})", short_callee(callee), parts.join(", "))
}

/// Help for a value of type `v.ty` in a `slot` it does not fit: an
/// optional where its value is wanted, or a collection of other
/// elements.
fn mismatch_help(ctx: &LowerCtx<'_>, slot: &HirTy, v: &HirExpr) {
    let src = suggest::short_src(ctx, v.span);
    if suggest::some_part(&v.ty).as_ref() == Some(slot) {
        // `try?` over an optional nests (spec 3.14.3).
        if let Some(s) = src.filter(|s| {
            s.starts_with("try?") && matches!(slot, HirTy::Sum(ts) if ts.iter().any(HirTy::is_none))
        }) {
            let fix = format!("({s}) ?? none");
            ctx.fix_last(v.span, fix.clone(), format!("use `{fix}`"));
            ctx.help_last(Some(format!(
                "`try?` keeps a thrown error apart from a returned `none`: write `{fix}` to \
                 merge them, or annotate `{}`",
                ctx.ty_name(&v.ty)
            )));
            return;
        }
        let name = src.unwrap_or("v");
        let help = match (src, suggest::default_lit(slot)) {
            (Some(s), Some(d)) => {
                let fix = format!("{s} ?? {d}");
                ctx.fix_last(v.span, fix.clone(), format!("use `{fix}`"));
                format!(
                    "`{s}` may be none: write `{fix}`, or `if let x: {} = {s} {{ .. }}`",
                    ctx.ty_name(slot)
                )
            }
            _ => format!(
                "`{name}` may be none: {}",
                suggest::optional_forms(ctx, name, slot)
            ),
        };
        ctx.help_last(Some(help));
        return;
    }
    let (HirTy::Struct(a), HirTy::Struct(b)) = (slot, &v.ty) else {
        return;
    };
    let (want, have) = (ctx.struct_def(*a), ctx.struct_def(*b));
    let base = |n: &str| expr::enum_base_name(n).to_owned();
    if base(&want.name) != base(&have.name) || want.generic_args.len() != 1 {
        return;
    }
    let (Some(to), Some(from)) = (want.generic_args.first(), have.generic_args.first()) else {
        return;
    };
    let num = |t: &HirTy| matches!(t, HirTy::Prim(p) if p.is_int() || p.is_float());
    if !num(to) || !num(from) {
        return;
    }
    let xs = src.unwrap_or("xs");
    ctx.help_last(Some(format!(
        "convert the elements: `{xs}.iter().map(|x| x as {}).collect()`",
        ctx.ty_name(to)
    )));
}

/// Help for a primitive in another primitive's slot: `as` between
/// numbers, `parse` from a `str`, and for a literal under `let x: T`
/// (`ann` the `: T`), dropping the annotation.
fn convert_help(ctx: &LowerCtx<'_>, slot: &HirTy, v: &HirExpr, ann: Option<Span>, what: &str) {
    let (HirTy::Prim(to), HirTy::Prim(from)) = (slot, &v.ty) else {
        return;
    };
    let num = |p: &PrimTy| p.is_int() || p.is_float();
    let src = suggest::short_src(ctx, v.span);
    // Needs no parentheses before `as` or `.parse()`.
    let simple = |s: &str| {
        s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '\''))
            || s.len() > 1 && s.starts_with('"') && s.ends_with('"')
            || s.ends_with(')') && !s.contains(' ')
    };
    let wrap = |s: &str| {
        if simple(s) {
            s.to_owned()
        } else {
            format!("({s})")
        }
    };
    let to_name = to.name();
    let mut helped = true;
    match src {
        Some(s) if num(to) && num(from) => {
            let fix = format!("{} as {to_name}", wrap(s));
            ctx.help_last(Some(format!("convert it with `as`: `{fix}`")));
            ctx.fix_last(v.span, fix, format!("convert with `as {to_name}`"));
        }
        None if num(to) && num(from) => {
            ctx.help_last(Some(format!("convert it with `as {to_name}`")));
        }
        Some(s) if num(to) && *from == PrimTy::Str => {
            let fix = format!("try! {}.parse()", wrap(s));
            ctx.help_last(Some(format!(
                "to read a number from the text, write `{fix}`"
            )));
            ctx.fix_last(v.span, fix, "parse the text");
        }
        // `+` on text has its own f-string help.
        Some(s)
            if *to == PrimTy::Str
                && *from != PrimTy::CChar
                && !s.contains('"')
                && !what.contains("operand of `+`") =>
        {
            let fix = format!("f\"{{{s}}}\"");
            ctx.help_last(Some(format!("to make text from it, write `{fix}`")));
            ctx.fix_last(v.span, fix, "format as text");
        }
        _ => helped = false,
    }
    let literal = src.is_some_and(|s| {
        s.trim_start_matches('-')
            .starts_with(|c: char| c.is_ascii_digit() || matches!(c, '"' | '\''))
            || matches!(s, "true" | "false")
    });
    let (Some(ann), true) = (ann, literal) else {
        return;
    };
    let Some(text) = ctx.src(ann) else {
        return;
    };
    let or = if helped { "or, " } else { "" };
    ctx.help_last(Some(format!(
        "{or}to keep the `{}`, drop `{text}`",
        from.name()
    )));
    ctx.fix_last(ann, "", format!("remove `{text}`"));
}

/// What to write instead of a `[ref name]` capture (`x` its variable's
/// type) in a closure that may run on another thread.
fn ref_capture_help(name: &str, x: &HirTy) -> String {
    let share = if matches!(x, HirTy::Prim(p) if p.is_int() || *p == PrimTy::Bool) {
        format!(
            "`let {name} = Atomic::new({name});` before the closure, capture `{name}` without \
             `ref`, and use `{name}.load()` / `{name}.store(..)`"
        )
    } else {
        format!(
            "`let {name} = Mutex::new({name});` before the closure, capture `{name}` without \
             `ref`, and use `using g = {name}.lock() {{ g.set(..); }}`"
        )
    };
    format!("help: write {share}; or return the value from the closure instead")
}

/// A closure with a `[ref]` capture, or one capturing such a closure.
fn holds_ref_cell(ty: &HirTy) -> bool {
    match ty {
        HirTy::Closure { env, shares, .. } => *shares || holds_ref_cell(env),
        HirTy::Tuple(ts) => ts.iter().any(holds_ref_cell),
        _ => false,
    }
}

/// Why a function value does not fit a `fn` slot (`""`: no hint).
fn fn_mismatch_hint(slot: &HirTy, ty: &HirTy) -> &'static str {
    if matches!(slot, HirTy::Fn(_, _, want) if !want.auto.is_empty()) && holds_ref_cell(ty) {
        " - the closure holds a `[ref ..]` capture, which the code that made it could change at \
         the same time; share the value through a `Mutex` or an `Atomic` instead (spec 4.6.9)"
    } else if matches!((slot, ty), (HirTy::Fn(_, _, want), HirTy::Closure { .. }) if *want != crate::FnBounds::default())
    {
        " - the closure's captures don't meet the `fn` type's bounds"
    } else if matches!((slot, ty), (HirTy::Fn(_, _, want), HirTy::Fn(_, _, have))
        if have.unsafe_fn && !want.unsafe_fn)
    {
        " - an `unsafe fn` value only fits an `unsafe fn` type (spec 4.15)"
    } else if matches!((slot, ty), (HirTy::Fn(_, _, want), HirTy::Fn(_, _, have))
        if want.c_abi != have.c_abi)
    {
        " - an `extern \"C\" fn` value is a bare C function pointer, not a rok `fn` value \
         (spec 5.9.4)"
    } else if matches!((slot, ty), (HirTy::Fn(_, _, want), HirTy::Fn(_, _, have))
        if want.stdcall != have.stdcall)
    {
        " - `extern \"system\" fn` is `stdcall` on this target, not `extern \"C\" fn` \
         (spec 5.9.4)"
    } else if matches!((slot, ty), (HirTy::Fn(_, _, want), HirTy::Fn(_, _, have))
        if !fn_bounds_fit(want, have))
    {
        " - a `fn` value must already carry the bound in its type; only a named \
         function or a capture-free closure is always `Send + Sync`"
    } else if matches!((slot, ty), (HirTy::Fn(_, r, _), HirTy::Fn(..))
        if matches!(**r, HirTy::Throws { .. }))
    {
        " - a function value fits a `fn` type that throws more only with the same \
         parameters and result (spec 4.4.9)"
    } else {
        ""
    }
}

/// Spec 5.9.4: a named C-ABI function fills an `extern "C" fn(..)` slot
/// (or sum member) as its own address; MIR takes the C entry. Where
/// `"system"` is `stdcall`, it and `"C"` only fill their own kind.
fn c_fn_value(ctx: &LowerCtx<'_>, slot: &HirTy, mut value: HirExpr) -> HirExpr {
    let (HirExprKind::FnRef(id), HirTy::Fn(va, vr, have)) = (&value.kind, &value.ty) else {
        return value;
    };
    if have.c_abi {
        return value;
    }
    let fits = |t: &&HirTy| matches!(t, HirTy::Fn(a, r, want) if want.c_abi && a == va && r == vr);
    let target = match slot {
        HirTy::Sum(vs) => vs.iter().find(fits),
        s => Some(s).filter(fits),
    };
    let Some(HirTy::Fn(_, _, want)) = target else {
        return value;
    };
    if !(ctx.env.is(FnSet::ExternC, *id) || ctx.env.is(FnSet::CDef, *id)) {
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            "only an `extern \"C\"` function (an `extern` block declaration or an \
             `extern \"C\" fn` definition) is an `extern \"C\" fn(..)` value (spec 5.9.4)"
                .to_owned(),
        );
    }
    let stdcall = ctx.env.is(FnSet::Stdcall, *id);
    if stdcall != want.stdcall {
        let (is, slot) = if stdcall {
            ("an `extern \"system\"`", "`extern \"C\" fn(..)`")
        } else {
            ("a C", "`extern \"system\" fn(..)`")
        };
        ctx.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!(
                "{is} function is no {slot} value: `\"system\"` is `stdcall` on this \
                 target (spec 5.9.4)"
            ),
        );
    }
    let mut bounds = have.clone();
    bounds.c_abi = true;
    bounds.stdcall = want.stdcall;
    value.ty = HirTy::Fn(va.clone(), vr.clone(), bounds);
    value
}

/// Can a `fn` with bounds `have` fill `want`? It needs every auto trait
/// the slot names; `unsafe fn` fills only `unsafe fn` slots.
fn fn_bounds_fit(want: &crate::FnBounds, have: &crate::FnBounds) -> bool {
    want.auto.iter().all(|b| have.auto.contains(b))
        && (!have.unsafe_fn || want.unsafe_fn)
        && want.c_abi == have.c_abi
        && want.stdcall == have.stdcall
}

/// A `fn` with extra auto-trait bounds takes the slot's type, so
/// `fn() + Send` → `fn()` (or `fn() | none`) needs no conversion.
fn drop_fn_bounds(slot: &HirTy, mut value: HirExpr) -> HirExpr {
    let HirTy::Fn(va, vr, have) = &value.ty else {
        return value;
    };
    let fits = |t: &&HirTy| {
        matches!(t, HirTy::Fn(a, r, want)
            if a == va && r == vr && fn_bounds_fit(want, have))
    };
    let target = match slot {
        HirTy::Sum(vs) => vs.iter().find(fits),
        s => Some(s).filter(fits),
    };
    if let Some(t) = target {
        value.ty = t.clone();
    }
    value
}

/// Does `any` with `have` satisfy `want`: some bound or transitive
/// supertrait is `want` (`any Tr<i32>` satisfies `any Tr`).
fn bound_covers(ctx: &LowerCtx<'_>, have: &[TraitRef], want: &TraitRef) -> bool {
    let mut stack: Vec<TraitRef> = have.to_vec();
    let mut seen: Vec<String> = Vec::new();
    while let Some(h) = stack.pop() {
        if h == *want || (want.is_plain() && h.path == want.path) {
            return true;
        }
        if let (false, Some(info)) = (seen.contains(&h.path), ctx.env.trait_of(&h.path)) {
            stack.extend(info.supers.iter().map(|s| TraitRef::plain(s)));
        }
        seen.push(h.path);
    }
    false
}

/// Does a value of `ty` (not `any`) or an `any` value meet bound `b`?
fn meets(ctx: &LowerCtx<'_>, ty: &HirTy, b: &TraitRef) -> bool {
    match ty {
        HirTy::AnyTrait(have) => bound_covers(ctx, have, b),
        _ => ctx.implements_trait(ty, b, 0),
    }
}

/// The `any Tr` slot a `ty` value goes into: `slot`, or the `any Tr`
/// member of a sum without an exact member for `ty`.
fn any_slot_for<'s>(slot: &'s HirTy, ty: &HirTy) -> Option<&'s HirTy> {
    match slot {
        HirTy::AnyTrait(_) => Some(slot),
        HirTy::Sum(vs) if !matches!(ty, HirTy::Sum(_)) && sum_variant_index(vs, ty).is_none() => {
            vs.iter().find(|v| matches!(v, HirTy::AnyTrait(_)))
        }
        _ => None,
    }
}

/// Store into `any Tr`: check every bound and box as `SumWrap { tag,
/// value }` (program-wide type number, heap copy). `any (A + B)` → `any A`
/// keeps the value (same tags).
fn box_into_any(ctx: &LowerCtx<'_>, slot: &HirTy, value: HirExpr) -> HirExpr {
    let Some(target @ HirTy::AnyTrait(bounds)) = any_slot_for(slot, &value.ty) else {
        return value;
    };
    match &value.ty {
        HirTy::AnyTrait(have) if bounds.iter().all(|b| bound_covers(ctx, have, b)) => HirExpr {
            ty: target.clone(),
            ..value
        },
        // Spec 2.2.10: no implicit `any` → `any Tr` (it would hide a
        // runtime check); `any Tr` → `any` is the arm above.
        HirTy::AnyTrait(_) => ctx.error_expr(
            value.span,
            format!(
                "cannot store an `{}` value in an `{}` slot without narrowing - test it with \
                 `is` or a type pattern (`match v {{ x: T => ... }}`) first (spec 2.2.10)",
                ctx.ty_name(&value.ty),
                ctx.ty_name(target)
            ),
        ),
        HirTy::Unknown | HirTy::Any => value,
        ty => {
            let names: Vec<String> = bounds
                .iter()
                .filter(|b| !ctx.implements_trait(ty, b, 0))
                .map(|b| format!("`{}`", ctx.trait_name(b)))
                .collect();
            if !names.is_empty() {
                return ctx.error_expr(
                    value.span,
                    format!(
                        "`{}` does not implement {}, so it cannot be stored in an `{}` slot",
                        ctx.ty_name(ty),
                        names.join(", "),
                        ctx.ty_name(target)
                    ),
                );
            }
            let span = value.span;
            HirExpr {
                kind: HirExprKind::SumWrap {
                    sum_ty: target.clone(),
                    variant_index: ctx.any_tag(ty),
                    value: Some(Box::new(value)),
                },
                ty: target.clone(),
                span,
            }
        }
    }
}

/// A default value (`x: T = e`, `f: T = e`) resolved where it is
/// declared, not at the use that splices it in (spec 4.2.3).
fn in_module(module: &str, e: Expr) -> Expr {
    Expr {
        span: e.span,
        kind: ExprKind::InModule(module.to_owned(), Box::new(e)),
    }
}

/// `ty_compatible`, or storable through `box_into_any`.
fn coercible(ctx: &LowerCtx<'_>, slot: &HirTy, ty: &HirTy) -> bool {
    ty_compatible(slot, ty)
        || matches!(any_slot_for(slot, ty), Some(HirTy::AnyTrait(bs))
        if bs.iter().all(|b| meets(ctx, ty, b)))
}

/// Lower `e` with `ty` as its expected type (`Unknown` = no expectation).
fn lower_expected(ctx: &mut LowerCtx<'_>, e: &Expr, ty: HirTy) -> HirExpr {
    ctx.push_expected(ty);
    let r = lower_expr(ctx, e);
    ctx.pop_expected();
    r
}

/// Never completes normally: ends in `return` / `throw` / `break` /
/// `continue`, a `loop` without `break`, or all-diverging branches.
fn hir_block_diverges(b: &HirBlock) -> bool {
    b.stmts.iter().any(|s| match s {
        HirStmt::Return(_)
        | HirStmt::Panic(_)
        | HirStmt::Break { .. }
        | HirStmt::Continue { .. } => true,
        HirStmt::Expr(e) | HirStmt::Let { init: e, .. } => hir_expr_diverges(e),
        _ => false,
    }) || b.trailing.as_ref().is_some_and(hir_expr_diverges)
}

fn hir_expr_diverges(e: &HirExpr) -> bool {
    match &e.kind {
        // `'l: { break 'l v; }` exits with a value: not divergence.
        HirExprKind::Block(b) => {
            hir_block_diverges(b)
                && !(b.label.is_some() && block_breaks_out(b, b.label.as_deref(), false))
        }
        HirExprKind::If {
            then_block,
            else_branch: Some(eb),
            ..
        } => hir_block_diverges(then_block) && hir_expr_diverges(eb),
        HirExprKind::Match { arms, .. } => {
            !arms.is_empty() && arms.iter().all(|a| hir_expr_diverges(&a.body))
        }
        HirExprKind::Loop { label, body, .. } => !block_breaks_out(body, label.as_deref(), true),
        _ => false,
    }
}

/// Does `b` contain a `break` that exits the loop labelled `label`?
/// `direct` = we're not inside a nested loop (unlabelled breaks count).
fn block_breaks_out(b: &HirBlock, label: Option<&str>, direct: bool) -> bool {
    let in_expr = |e: &HirExpr| expr_breaks_out(e, label, direct);
    b.stmts.iter().any(|s| match s {
        HirStmt::Break { label: bl, .. } => {
            (bl.is_none() && direct) || (bl.is_some() && bl.as_deref() == label)
        }
        HirStmt::Let { init: e, .. }
        | HirStmt::Assign { value: e, .. }
        | HirStmt::StaticAssign { value: e, .. }
        | HirStmt::Expr(e)
        | HirStmt::Return(Some(e))
        | HirStmt::Panic(e) => in_expr(e),
        _ => false,
    }) || b.trailing.as_ref().is_some_and(in_expr)
}

fn expr_breaks_out(e: &HirExpr, label: Option<&str>, direct: bool) -> bool {
    match &e.kind {
        HirExprKind::Block(b) => block_breaks_out(b, label, direct),
        HirExprKind::If {
            then_block,
            else_branch,
            ..
        } => {
            block_breaks_out(then_block, label, direct)
                || else_branch
                    .as_deref()
                    .is_some_and(|x| expr_breaks_out(x, label, direct))
        }
        HirExprKind::Match { arms, .. } => {
            arms.iter().any(|a| expr_breaks_out(&a.body, label, direct))
        }
        HirExprKind::While { body, .. } | HirExprKind::Loop { body, .. } => {
            block_breaks_out(body, label, false)
        }
        _ => false,
    }
}

/// Outcome of resolving `[..enum path.., Variant]`.
enum EnumRes {
    /// The prefix doesn't name an enum.
    NotEnum,
    Found(EnumId, usize),
    /// An enum was named but the variant / instantiation is wrong
    /// (already reported).
    Error,
}

/// Where call arguments come from: still-AST expressions (lowered
/// against the parameter type) or values already lowered (spread).
enum ArgSrc<'e> {
    Ast(&'e Expr),
    Lowered(HirExpr),
    /// `ref x` (spec 4.7.3).
    Ref(&'e Expr),
    /// A dict spread's read of this parameter (spec 4.2.6): the index of
    /// its built expression.
    Dict(usize),
}

type TySubst = HashMap<String, HirTy>;

#[derive(Clone, Default)]
struct InstanceWarns {
    instances: HashMap<rokc_diagnostic::Span, std::collections::HashSet<FunctionId>>,
    warns: Vec<(
        rokc_diagnostic::Span,
        rokc_diagnostic::RokDiagnostic,
        std::collections::HashSet<FunctionId>,
    )>,
}

impl InstanceWarns {
    /// Instance `id` of the template at `template` is lowered.
    fn instance(&mut self, template: rokc_diagnostic::Span, id: FunctionId) {
        incr::log_warn(|| incr::WarnEvent::Instance(template, id));
        self.instances.entry(template).or_default().insert(id);
    }

    /// Instance `id` of the template at `template` raised `d`.
    fn warn(
        &mut self,
        template: rokc_diagnostic::Span,
        d: rokc_diagnostic::RokDiagnostic,
        id: FunctionId,
    ) {
        incr::log_warn(|| incr::WarnEvent::Warn(template, d.clone(), id));
        let same = |(t, w, _): &&mut (_, rokc_diagnostic::RokDiagnostic, _)| {
            *t == template && w.message == d.message && w.primary == d.primary
        };
        if let Some((_, _, by)) = self.warns.iter_mut().find(same) {
            by.insert(id);
        } else {
            self.warns
                .push((template, d, std::iter::once(id).collect()));
        }
    }

    /// The warnings every instance of their template raised.
    fn agreed(self) -> impl Iterator<Item = rokc_diagnostic::RokDiagnostic> {
        let instances = self.instances;
        self.warns
            .into_iter()
            .filter(move |(t, _, by)| instances.get(t).is_some_and(|all| all.is_subset(by)))
            .map(|(_, d, _)| d)
    }
}

#[derive(Clone)]
struct ClosureSink {
    extras: std::cell::RefCell<Vec<HirFunction>>,
    /// Functions with `ref` parameters (spec 4.7, `ref self` included).
    ref_out: std::cell::RefCell<HashMap<FunctionId, RefOut>>,
    next_id: std::cell::Cell<u32>,
    /// Cached monomorphizations: `(generic_fn_name, concrete_type_args)`
    /// → specialized `FunctionId`.
    mono_cache: std::cell::RefCell<HashMap<(String, Vec<String>), FunctionId>>,
    /// Specialized `FnSig` per monomorphization for call-site lookup.
    mono_sigs: std::cell::RefCell<HashMap<FunctionId, FnSig>>,
    /// Current monomorphization nesting (bounded by `MONO_DEPTH_LIMIT`).
    mono_depth: std::cell::Cell<u32>,
    /// Bumped by each "type annotations needed" failure, so an unannotated
    /// `let` knows to look at later uses.
    uninferred: std::cell::Cell<u32>,
    /// `any` type tags of the concrete types stored so far (`any_tag`).
    any_tags: std::cell::RefCell<Vec<(HirTy, u32)>>,
    /// `any Tr` method dispatchers (bodies built after all lowering).
    any_dispatch: std::cell::RefCell<Vec<expr::AnyDispatch>>,
    /// `T::Item` projections being resolved (cuts `impl<I: Iterator>
    /// IntoIterator for I { type Item = I::Item; }` cycles).
    projecting: std::cell::RefCell<Vec<ProjectionGoal>>,
    /// Adapter thunks (`adapt_fn_value` / `adapt_never_fn`) by
    /// `kind|callee|slot type|callee type`.
    thunks: std::cell::RefCell<HashMap<String, FunctionId>>,
    /// Generic instance bodies being lowered: `(template span, instance
    /// id)`, `None` for a non-instance body nested in one.
    instances: std::cell::RefCell<Vec<Option<(rokc_diagnostic::Span, FunctionId)>>>,
    /// Instances per template and the warnings each raised: a warning is
    /// reported only if every instance raised it.
    instance_warns: std::cell::RefCell<InstanceWarns>,
    /// Diagnostics emitted during lowering (`HirModule.diagnostics`).
    diagnostics: std::cell::RefCell<Vec<rokc_diagnostic::RokDiagnostic>>,
    /// Inference variables (`infer`): one per open type parameter of an
    /// un-annotated `let` being inferred from its later uses.
    infer_vars: std::cell::RefCell<Vec<infer::Var>>,
    /// `let` inferences in progress: unification only runs when > 0.
    infer_depth: std::cell::Cell<u32>,
    /// Variables before the outermost inference in progress.
    infer_floor: std::cell::Cell<usize>,
    /// Every lowered function's `(const fn, standard library)`.
    fn_origin: std::cell::RefCell<HashMap<FunctionId, (bool, bool)>>,
    /// Lowered `#[track_caller]` functions (`caller`).
    track_caller: std::cell::RefCell<std::collections::HashSet<FunctionId>>,
    /// Declarations being lowered for a compile-time evaluation.
    const_lowering: std::cell::RefCell<std::collections::HashSet<FunctionId>>,
    /// Const items being lowered, innermost last (cycle check).
    const_stack: std::cell::RefCell<Vec<String>>,
    /// Return types inferred from bodies (spec 4.1.3), by function;
    /// `None` while the body is being inferred.
    inferred_ret: std::cell::RefCell<HashMap<FunctionId, Option<HirTy>>>,
    /// Declarations whose inference lowering was final, with the first
    /// function id they could have made (for `rollback`); the decl pass
    /// reuses them.
    inferred_fns: std::cell::RefCell<HashMap<FunctionId, (u32, HirFunction)>>,
    /// Every reachable body is lowered: a later inference (a signature
    /// asked for by a check) must leave no functions behind.
    bodies_done: std::cell::Cell<bool>,
    /// The first round of signature types (`Some`): whether a type
    /// waited for the full const evaluator.
    sig_pass: std::cell::Cell<Option<bool>>,
    /// Call sites of the implicit-generic instances being lowered
    /// (spec 4.2.2: their body errors point at the call too).
    call_sites: std::cell::RefCell<Vec<(String, rokc_diagnostic::Span)>>,
    /// Calls met of functions whose return type is being inferred.
    recursive_ret: std::cell::RefCell<Vec<FunctionId>>,
    /// Inferred return types that depend on a fn still being inferred:
    /// dropped once it is done.
    tentative_ret: std::cell::RefCell<Vec<FunctionId>>,
    /// Type parameters of the generic bodies being checked
    /// (`HirTy::Param`, `bounds`).
    params: std::cell::RefCell<Vec<bounds::ParamInfo>>,
    /// Templates whose definition check failed, by span: their
    /// instances (and a trait default's copies) report nothing more.
    failed: std::cell::RefCell<std::collections::HashSet<Span>>,
    /// The declarations whose bodies are being lowered, innermost last.
    bodies: std::cell::RefCell<Vec<Span>>,
    /// The `where` clauses on other types than a parameter of the body
    /// being checked.
    assumed: std::cell::RefCell<Vec<(HirTy, TraitRef)>>,
    /// The `const` parameters of the body being checked, by the value
    /// standing for each (`bounds`).
    const_names: std::cell::RefCell<Vec<(i128, String)>>,
    /// `diagnostics.len()` when `emit_diag` last dropped one: the
    /// `*_last` helpers then leave the earlier diagnostic alone.
    dropped_at: std::cell::Cell<Option<usize>>,
}

/// Per-function lowering state: scoped name → local-id map plus the
/// running list of local declarations, on top of the shared `Env`.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)] // independent per-body modes
struct LowerCtx<'a> {
    env: &'a Env<'a>,
    locals: Vec<HirLocal>,
    scopes: Vec<HashMap<String, LocalId>>,
    /// This instance's type-parameter substitution (empty if not generic);
    /// overrides same-named aliases. Owned while an associated const lowers
    /// under its impl's.
    ty_subst: std::borrow::Cow<'a, TySubst>,
    self_ty: Option<HirTy>,
    /// Enclosing function's (or closure's) return type. `return v;`
    /// checks against it and wraps `v` into a matching sum type.
    fn_ret_ty: HirTy,
    /// Closure without a declared / expected return type: the first
    /// `return v;` fixes `fn_ret_ty`.
    infer_ret: bool,
    /// Inferring a function's return type (spec 4.1.3): the types of
    /// its `return` values and trailing value.
    ret_tys: Option<Vec<HirTy>>,
    /// Lowering the implicit `main`, where `return` is banned (spec 4.16.4);
    /// closures get a fresh context.
    implicit_main: bool,
    /// Lowering the test harness `main` (`RootMain::Harness`).
    harness_main: bool,
    /// The `ref` parameters' locals and types (`ref self` included);
    /// every return also hands their values back (copy-out).
    ref_params: Vec<(LocalId, HirTy)>,
    /// `ref_params` are cells handed back as they are (an `async` method's
    /// `ref self`, `mono::defers_refs`).
    ref_cells: bool,
    /// `await e`: the span of `e`, whose deferred `ref` write-back
    /// (`mono::ref_unpack`) waits for the future, and that write-back.
    awaited: Option<rokc_diagnostic::Span>,
    await_write_back: Vec<HirStmt>,
    /// Bindings some closure captures as `[ref x]` (spec 4.6.2), by name and
    /// span; they live in a one-element `GcBuf` cell.
    ref_captured: std::collections::HashSet<(String, rokc_diagnostic::Span)>,
    /// Pattern bindings (`declare_bound`) still to be moved into their
    /// cells (`take_cells`) before the code in their scope runs.
    pending_cells: Vec<(String, LocalId, HirTy)>,
    /// Sub-sum type-pattern binders (`v: A | B` against `A | B | C`):
    /// `(binder, whole-scrutinee local)`, converted by `take_cells`.
    pending_resums: Vec<(LocalId, LocalId)>,
    /// In a closure: locals holding a value capture (assigning one is an
    /// error, spec 4.6.4).
    value_captured: std::collections::HashSet<LocalId>,
    /// Set by a path call whose first argument fills `self`: whether it was
    /// `ref`. Taken by that call's argument lowering.
    explicit_recv: Option<bool>,
    /// The slice expression being lowered fills a generic `SliceMut<T>`
    /// parameter: a list's range is a writable view.
    want_view: bool,
    /// Spans of calls whose `ref self` receiver is no place to write
    /// back to (an element, a static, a const): the write is dropped.
    dropped_ref_recv: Vec<Span>,
    /// Enclosing `unsafe fn` body / `unsafe { }` blocks (spec 4.15):
    /// unsafe calls are allowed when > 0.
    unsafe_depth: u32,
    /// Set by a call through a lang slot (`expr::lang_fn`): lowering
    /// made it, so the hook's own marks do not warn. Taken by the call.
    hook_call: std::cell::Cell<bool>,
    /// Spans of calls to `#[discardable]` functions (`unused_result`).
    discardable_calls: std::cell::RefCell<Vec<Span>>,
    /// Lowering `addr_of`'s place: taking an extern static's address
    /// reads nothing.
    addr_place: bool,
    /// Enclosing breakable constructs, innermost last: `(label,
    /// is_loop)` - labelled blocks accept `break 'l` but not `continue`.
    breakables: Vec<(Option<String>, bool)>,
    /// `breakables` index → the type its `break` values must have:
    /// the annotated slot (`true` = inferred from the first `break`).
    break_tys: HashMap<usize, (HirTy, bool)>,
    /// Expected types. A sub-expression with its own slot pushes it (or
    /// `Unknown`), so leaves only see the slot they flow into.
    expected_ty_stack: Vec<HirTy>,
    /// Module path of the function being lowered (`""` at the root).
    current_module: String,
    /// While a name a meta template or the compiler wrote is resolved:
    /// the module of its definition, whose scope and access it has
    /// (spec 6.9.9).
    access: std::cell::RefCell<Option<std::rc::Rc<str>>>,
    /// Block-scoped `use`s, one level per `push_scope`, searched before
    /// module uses.
    local_uses: Vec<Vec<crate::uses::UseEntry>>,
    /// Per scope: nested fns (spec 4.10) by name → lifted fn key.
    nested_fns: Vec<HashMap<String, String>>,
    /// Or-pattern binders: name → preset local, so every alternative binds
    /// `x` once with one type (spec 3.11.4).
    or_pattern_binders: Vec<HashMap<String, LocalId>>,
    /// `f::<T, 3>(..)` arguments for the generic call being lowered,
    /// read by `monomorphize_on` (empty otherwise).
    explicit_generics: HashMap<String, HirTy>,
    /// `x.m::<T>(..)`: the method call's explicit type arguments, taken
    /// by `method_call_on`.
    method_type_args: Vec<Ty>,
    /// Error type of the enclosing function's / closure's `throws`
    /// (`None`: no error may leave it).
    fn_throws: Option<HirTy>,
    /// Enclosing prefix `try`s; a throwing call must sit under one.
    try_depth: u32,
    /// `try for`: the `try_depth` outside the `try`, set for the `for`
    /// it marks, which takes it (the `try` covers the iteration only).
    try_for: Option<u32>,
    /// That `for`'s body block and the `try_depth` it is lowered at.
    try_for_body: Option<(Span, u32)>,
    /// Inside the body of a `try for` (a throwing call there gets a hint).
    in_try_for_body: u32,
    /// Enclosing `catch` / `try?` / `try!` / `finally` frames, innermost
    /// last (see `throws`).
    err_frames: Vec<ErrFrame>,
    /// By-value parameters whose release the body owes (`affine`).
    owe_params: Vec<(LocalId, HirTy)>,
    /// Probe pass over a closure body: collects the error types that
    /// escape it instead of reporting them.
    throws_probe: Option<Vec<HirTy>>,
    /// Lowering the body of `fn main() throws (E)`: an escaping error
    /// ends the process (spec 4.16.3).
    uncaught_in_main: bool,
    /// Throwing calls lowered so far (a `try` covering none is
    /// pointless).
    throwing_calls: u32,
    /// Counter for synthesized labels and local names.
    next_synth: u32,
    /// Narrowed rebinding (spec 2.2.6) → (original local, its type);
    /// assignments to the rebinding also store into the original.
    narrowed: HashMap<LocalId, (LocalId, HirTy)>,
    /// Narrowing of a `[ref]` cell → the read of its rebinding: each read
    /// tests the cell again, as a capture may have changed it.
    cell_views: HashMap<LocalId, HirExpr>,
    /// Narrowings ended by assignment (spec 2.2.6): (name, rebinding, scope
    /// index). Branches undo and rejoin them.
    unnarrowed: Vec<(String, LocalId, usize)>,
    /// Lock guards bound but not yet released by `using` (local, depth,
    /// span, binder, type); an error when their scope ends (stdlib 2.16.3).
    guard_lets: Vec<(LocalId, usize, Span, String, &'static str)>,
    /// Expressions (by address) where a guard may stay unreleased:
    /// initializers, assigned values, `return`, the body's value
    /// (`expr::guard_sink`). Elsewhere a produced guard is never released
    /// and a read guard variable is moved.
    guard_sinks: std::collections::HashSet<usize>,
    /// Names the function body binds once and names once after
    /// (`sole_names`): a fresh future in one is its only handle.
    sole_names: std::collections::HashSet<String>,
    /// Locals holding a fresh future nothing else names
    /// (`expr::plain_fresh_future`), with the loop depth they were made
    /// at.
    sole_fresh: HashMap<LocalId, usize>,
    /// Type parameter → its trait bounds and their supertraits (trait
    /// keys): `T::Item` resolves through them (`set_bounds`).
    ty_bounds: HashMap<String, Vec<String>>,
    /// Type parameter → its bounds as written, so `T::Error` takes the
    /// `TryFrom<S>` impl, not any `TryFrom`.
    ty_bound_args: HashMap<String, Vec<AstPath>>,
    /// Lowering a coroutine body (`yield` / `await` are allowed).
    coro: Option<coro::CoroCtx>,
    /// Associated types of the enclosing trait / impl
    /// (`ASSOC_TYPES_ATTR`): a bare `X` is `Self::X` (spec 2.8.3).
    assoc_names: Vec<String>,
    /// Lowering an unannotated `let`'s initializer or what follows for
    /// inference: open type parameters become inference variables
    /// (`infer`), not errors.
    infer_vars: bool,
    /// Already lowered expressions an AST desugar refers to by a `#sN`
    /// path (`expr::splice`).
    splices: HashMap<String, HirExpr>,
    /// What is being lowered (the expression, statement or item): where
    /// diagnostics without a location of their own go.
    at: std::cell::Cell<Span>,
    /// Temporaries named so far (`expr::tmp_path`): their names count
    /// per body, so a body lowers the same whatever was lowered before.
    temps: std::cell::Cell<u32>,
    /// The `: T` of the `let` whose value the next `coerce` checks.
    let_ann: std::cell::Cell<Option<Span>>,
}

/// Where a speculative lowering started (`probe_mark`); `rollback`
/// undoes everything since.
struct ProbeMark {
    diagnostics: usize,
    uninferred: u32,
    next_id: u32,
    any_tags: usize,
    locals: usize,
}

impl<'a> LowerCtx<'a> {
    fn probe_mark(&self) -> ProbeMark {
        ProbeMark {
            diagnostics: self.env.sink.diagnostics.borrow().len(),
            uninferred: self.env.sink.uninferred.get(),
            next_id: self.env.sink.next_id.get(),
            any_tags: self.env.sink.any_tags.borrow().len(),
            locals: self.locals.len(),
        }
    }

    /// Whether nothing was reported since `mark`.
    fn probe_clean(&self, mark: &ProbeMark) -> bool {
        self.env.sink.diagnostics.borrow().len() == mark.diagnostics
    }

    /// Undo everything since `mark`: locals, diagnostics (returned), and
    /// every function made since (closures, thunks, instances and their
    /// cache entries). Struct / enum instances stay;
    /// `infer::drop_var_instances` drops the open ones.
    fn rollback(&mut self, mark: &ProbeMark) -> Vec<rokc_diagnostic::RokDiagnostic> {
        self.locals.truncate(mark.locals);
        let kept = |l: &LocalId| (l.0 as usize) < mark.locals;
        for scope in &mut self.scopes {
            scope.retain(|_, l| kept(l));
        }
        self.value_captured.retain(kept);
        self.narrowed.retain(|l, _| kept(l));
        self.cell_views.retain(|l, _| kept(l));
        self.guard_lets.retain(|g| kept(&g.0));
        self.unnarrowed.retain(|u| kept(&u.1));
        let sink = &self.env.sink;
        let old = |id: &FunctionId| id.0 < mark.next_id;
        let dropped = sink.diagnostics.borrow_mut().split_off(mark.diagnostics);
        sink.uninferred.set(mark.uninferred);
        sink.extras.borrow_mut().retain(|f| old(&f.id));
        sink.mono_cache.borrow_mut().retain(|_, id| old(id));
        sink.mono_sigs.borrow_mut().retain(|id, _| old(id));
        sink.ref_out.borrow_mut().retain(|id, _| old(id));
        sink.inferred_fns
            .borrow_mut()
            .retain(|_, (start, _)| *start < mark.next_id);
        sink.thunks.borrow_mut().retain(|_, id| old(id));
        sink.any_dispatch.borrow_mut().retain(|d| old(&d.id));
        sink.any_tags.borrow_mut().truncate(mark.any_tags);
        let mut iw = sink.instance_warns.borrow_mut();
        for set in iw.instances.values_mut() {
            set.retain(old);
        }
        iw.warns.retain(|(_, _, by)| by.iter().any(old));
        dropped
    }

    /// A "type annotations needed" error (see `ClosureSink::uninferred`).
    fn emit_uninferred(&self, msg: String) {
        self.env
            .sink
            .uninferred
            .set(self.env.sink.uninferred.get() + 1);
        self.emit_error(
            rokc_diagnostic::ErrorKind::Type,
            format!("type annotations needed: {msg}"),
        );
    }

    /// A context lowering at `at` (see `LowerCtx::at`).
    fn new(
        env: &'a Env<'a>,
        ty_subst: &'a HashMap<String, HirTy>,
        self_ty: Option<HirTy>,
        at: Span,
    ) -> Self {
        Self::with_subst(env, std::borrow::Cow::Borrowed(ty_subst), self_ty, at)
    }

    fn with_subst(
        env: &'a Env<'a>,
        ty_subst: std::borrow::Cow<'a, TySubst>,
        self_ty: Option<HirTy>,
        at: Span,
    ) -> Self {
        Self {
            env,
            locals: Vec::new(),
            scopes: vec![HashMap::new()],
            ty_subst,
            self_ty,
            fn_ret_ty: HirTy::Unit,
            infer_ret: false,
            ret_tys: None,
            implicit_main: false,
            harness_main: false,
            ref_params: Vec::new(),
            ref_cells: false,
            awaited: None,
            await_write_back: Vec::new(),
            ref_captured: std::collections::HashSet::new(),
            pending_cells: Vec::new(),
            pending_resums: Vec::new(),
            value_captured: std::collections::HashSet::new(),
            unsafe_depth: 0,
            hook_call: std::cell::Cell::new(false),
            discardable_calls: std::cell::RefCell::new(Vec::new()),
            addr_place: false,
            explicit_recv: None,
            want_view: false,
            dropped_ref_recv: Vec::new(),
            breakables: Vec::new(),
            break_tys: HashMap::new(),
            expected_ty_stack: Vec::new(),
            current_module: String::new(),
            access: std::cell::RefCell::new(None),
            local_uses: vec![Vec::new()],
            nested_fns: vec![HashMap::new()],
            or_pattern_binders: Vec::new(),
            explicit_generics: HashMap::new(),
            method_type_args: Vec::new(),
            fn_throws: None,
            try_depth: 0,
            try_for: None,
            try_for_body: None,
            in_try_for_body: 0,
            err_frames: Vec::new(),
            owe_params: Vec::new(),
            throws_probe: None,
            uncaught_in_main: false,
            throwing_calls: 0,
            next_synth: 0,
            narrowed: HashMap::new(),
            cell_views: HashMap::new(),
            unnarrowed: Vec::new(),
            guard_lets: Vec::new(),
            guard_sinks: std::collections::HashSet::new(),
            sole_names: std::collections::HashSet::new(),
            sole_fresh: HashMap::new(),
            ty_bounds: HashMap::new(),
            ty_bound_args: HashMap::new(),
            coro: None,
            assoc_names: Vec::new(),
            infer_vars: false,
            splices: HashMap::new(),
            at: std::cell::Cell::new(at),
            temps: std::cell::Cell::new(0),
            let_ann: std::cell::Cell::new(None),
        }
    }

    /// The location being lowered.
    fn here(&self) -> Span {
        self.at.get()
    }

    /// Lower at `span`: `f`'s diagnostics without a location of their
    /// own go there.
    fn at_span<R>(&mut self, span: Span, f: impl FnOnce(&mut Self) -> R) -> R {
        let outer = self.at.replace(span);
        let r = f(self);
        self.at.set(outer);
        r
    }

    /// The module whose scope and access the name being resolved has:
    /// its definition's for a name from a meta template (spec 6.9.9),
    /// else the current one.
    fn access_module(&self) -> AccessModule<'_> {
        match self.access.borrow().as_ref() {
            Some(m) => AccessModule::Def(std::rc::Rc::clone(m)),
            None => AccessModule::Here(&self.current_module),
        }
    }

    /// Block `use`s in reach: none for a name from a definition.
    fn reach_uses(&self) -> &[Vec<crate::uses::UseEntry>] {
        if self.access.borrow().is_some() {
            &[]
        } else {
            &self.local_uses
        }
    }

    /// Resolve names from `origin` (a path's definition module; `None`:
    /// the current one) until the returned value is put back.
    fn set_access(&self, origin: Option<&str>) -> Option<std::rc::Rc<str>> {
        self.access.replace(origin.map(std::rc::Rc::from))
    }

    /// The source text under `span`, for help that quotes the user's code.
    fn src(&self, span: Span) -> Option<&str> {
        let (_, text) = self.env.sources.file(span.file)?;
        text.get(span.start as usize..span.end as usize)
    }

    /// Bytes `from..to` of `outer`'s text as a span of their own.
    fn sub_span(&self, outer: Span, from: usize, to: usize) -> Option<Span> {
        let text = self.src(outer)?;
        let before = text.get(..from)?;
        text.get(from..to)?;
        let (line, col) = match before.rfind('\n') {
            Some(nl) => (
                outer.line + before.matches('\n').count() as u32,
                before[nl + 1..].chars().count() as u32 + 1,
            ),
            None => (outer.line, outer.col + before.chars().count() as u32),
        };
        Some(Span {
            start: outer.start + from as u32,
            end: outer.start + to as u32,
            line,
            col,
            file: outer.file,
        })
    }

    /// Add `help` to the last diagnostic if it is an error pushed after
    /// the sink held `since` of them.
    fn help_since(&self, since: usize, help: String) {
        let mut diags = self.env.sink.diagnostics.borrow_mut();
        if diags.len() > since {
            if let Some(d) = diags.last_mut().filter(|d| d.is_error()) {
                d.notes.push(format!("help: {help}"));
            }
        }
    }

    /// Push a lowering-time diagnostic onto the shared sink.
    fn emit_error(&self, kind: rokc_diagnostic::ErrorKind, msg: String) {
        self.emit_error_at(kind, self.here(), msg);
    }

    /// An error at `at`.
    fn emit_error_at(&self, kind: rokc_diagnostic::ErrorKind, at: Span, msg: String) {
        self.emit_diag(rokc_diagnostic::RokDiagnostic::error(kind, at, msg));
    }

    /// An error built by the caller.
    fn emit_diag(&self, mut d: rokc_diagnostic::RokDiagnostic) {
        if bounds::in_failed(self.env) {
            let len = self.env.sink.diagnostics.borrow().len();
            self.env.sink.dropped_at.set(Some(len));
            return;
        }
        if let Some((callee, at)) = self.env.sink.call_sites.borrow().first() {
            d = d.with_secondary(*at, format!("in this call of `{}`", leaf_name(callee)));
        }
        self.env.sink.dropped_at.set(None);
        self.env.sink.diagnostics.borrow_mut().push(d);
    }

    /// An error at `at` with a `help:` note.
    fn emit_error_help(&self, at: Span, msg: String, help: String) {
        self.emit_error_at(rokc_diagnostic::ErrorKind::Type, at, msg);
        self.help_last(Some(help));
    }

    /// A warning. In a generic instance it is held and reported only if
    /// every instance raised it: a `rethrows` instance with a non-throwing
    /// argument would otherwise flag every `try`.
    fn emit_warning(&self, msg: String) {
        self.push_warning(rokc_diagnostic::RokDiagnostic::warning(
            rokc_diagnostic::ErrorKind::Type,
            self.here(),
            msg,
        ));
    }

    /// A warning at `at` that belongs to `lint`, so `-A lint` and
    /// `#[allow(lint)]` silence it.
    fn emit_lint(&self, lint: &str, at: Span, msg: String, help: String) {
        self.push_warning(lint_diag(lint, at, msg, help));
    }

    fn push_warning(&self, d: rokc_diagnostic::RokDiagnostic) {
        if self.ty_subst.is_empty() {
            self.env.sink.diagnostics.borrow_mut().push(d);
            return;
        }
        let Some(Some((template, id))) = self.env.sink.instances.borrow().last().copied() else {
            return;
        };
        self.env
            .sink
            .instance_warns
            .borrow_mut()
            .warn(template, d, id);
    }

    /// Report a type error and return an `Unknown` placeholder; checks skip
    /// `Unknown`, so one cause gives one diagnostic.
    fn error_expr(&self, span: rokc_diagnostic::Span, msg: String) -> HirExpr {
        self.emit_error(rokc_diagnostic::ErrorKind::Type, msg);
        poison(span)
    }

    /// An unknown name (spec 9.9.4 `resolve`), with an optional hint.
    fn unknown_name(
        &self,
        span: rokc_diagnostic::Span,
        msg: String,
        help: Option<String>,
    ) -> HirExpr {
        self.emit_error(rokc_diagnostic::ErrorKind::Resolve, msg);
        // A name a meta template wrote is looked up where the template is.
        let help = match self.access.borrow().as_deref() {
            Some(m) => Some(format!(
                "the meta template that wrote this name looks it up in module `{}`; to look \
                 it up where the meta block is used, write `@use_site(..)` around it (spec 6.9.9)",
                module_display(m)
            )),
            None => help,
        };
        self.help_last(help);
        poison(span)
    }

    /// The diagnostic just emitted, unless `emit_diag` dropped it.
    fn with_last(&self, f: impl FnOnce(&mut rokc_diagnostic::RokDiagnostic)) {
        let mut diags = self.env.sink.diagnostics.borrow_mut();
        if self.env.sink.dropped_at.get() == Some(diags.len()) {
            return;
        }
        if let Some(d) = diags.last_mut() {
            f(d);
        }
    }

    /// Attach `help` to the diagnostic just emitted.
    fn help_last(&self, help: Option<String>) {
        if let Some(h) = help {
            self.with_last(|d| d.notes.push(format!("help: {h}")));
        }
    }

    /// Attach a fix (replace `span` with `text`) to the diagnostic just
    /// emitted.
    fn fix_last(&self, span: Span, text: impl Into<String>, title: impl Into<String>) {
        let fix = rokc_diagnostic::Fix {
            span,
            text: text.into(),
            title: title.into(),
        };
        self.with_last(|d| d.fixes.push(fix));
    }

    /// Tag the diagnostic just emitted.
    fn tag_last(&self, tag: rokc_diagnostic::Tag) {
        self.with_last(|d| d.tag = tag);
    }

    /// "struct `S` has no field `x`" plus the closest field name.
    fn no_field(&self, sid: StructId, name: &str, span: rokc_diagnostic::Span) -> HirExpr {
        let def = self.struct_def(sid);
        let fields: Vec<&str> = def
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .chain(
                def.union_kind
                    .iter()
                    .flat_map(|u| u.aliases.iter().map(|(n, _)| n.as_str())),
            )
            .filter(|n| !n.starts_with("__") && self.field_hidden(sid, n).is_none())
            .collect();
        let kind = if def.union_kind.is_some() {
            "union"
        } else {
            "struct"
        };
        let e = self.error_expr(
            span,
            format!(
                "{kind} `{}` has no field `{name}`",
                self.ty_name(&HirTy::Struct(sid))
            ),
        );
        // `xs.length` / `.size` / `.count` (JS, Java, C#): the `len` method.
        let recv = self
            .src(span)
            .and_then(|t| t.strip_suffix(name)?.strip_suffix('.'));
        let len = suggest::is_synonym(name, "len")
            && matches!(
                expr::pick_method(self, &HirTy::Struct(sid), "len", None),
                Ok(Some(_))
            );
        let help = recv
            .filter(|_| len)
            .map(|r| {
                self.fix_last(span, format!("{r}.len()"), "call `len`");
                format!("the length is a method: `{r}.len()`")
            })
            .or_else(|| {
                rokc_diagnostic::suggest(name, fields.iter().copied())
                    .map(|s| format!("did you mean `{s}`?"))
            })
            .or_else(|| {
                (!fields.is_empty() && fields.len() <= 8).then(|| {
                    let list: Vec<String> = fields.iter().map(|f| format!("`{f}`")).collect();
                    format!("available fields: {}", list.join(", "))
                })
            });
        self.help_last(help);
        e
    }

    /// "did you mean" for an unknown type path: types of the current
    /// module and the primitives.
    fn type_help(&self, segs: &[&str]) -> Option<String> {
        let name = segs.last()?;
        if let (1, Some(h)) = (segs.len(), wide_int_help(name)) {
            return Some(h);
        }
        let module = self.current_module.as_str();
        let defs = &self.env.defs;
        let (structs, enums) = (defs.struct_table.borrow(), defs.enum_table.borrow());
        let types = structs
            .keys()
            .chain(enums.keys())
            .chain(defs.generic_structs.keys())
            .chain(defs.generic_unions.keys())
            .chain(defs.generic_enums.keys())
            .filter_map(|k| {
                let (m, n) = k.rsplit_once("::").unwrap_or(("", k.as_str()));
                (m == module).then_some(n)
            })
            .chain(PRIM_NAMES.iter().copied());
        let typo = rokc_diagnostic::suggest(name, types).map(|s| format!("did you mean `{s}`?"));
        let import = (segs.len() == 1).then(|| {
            let names = &self.env.names;
            names.import_help(module, names.type_leaf(name), |k| names.type_vis(k))
        });
        join_help(import.flatten(), typo)
    }

    /// "did you mean" for an unknown value path (one segment): locals in
    /// scope, then functions / constants / statics of the current module.
    fn value_help(&self, segs: &[&str]) -> Option<String> {
        let [name] = segs else {
            if let Some(h) = segs
                .first()
                .filter(|_| segs.len() == 2)
                .and_then(|t| wide_int_help(t))
            {
                return Some(h);
            }
            // `T::f` with `T` a type not imported.
            let names = &self.env.names;
            let cur = self.current_module.as_str();
            return self.import_help(segs).or_else(|| match segs {
                [ty, member] => {
                    let keys = names.type_leaf(ty);
                    let open = keys.iter().filter(|k| names.hidden(cur, k).is_none());
                    match names.prelude_write(cur, open) {
                        Some(w) => Some(format!("write `{w}::{member}`")),
                        None => names.import_help(cur, keys, |k| names.type_vis(k)),
                    }
                }
                _ => None,
            });
        };
        let name = *name;
        if let Some(k) = match name {
            "null" | "nil" | "None" | "NULL" => Some("none"),
            "True" => Some("true"),
            "False" => Some("false"),
            _ => None,
        } {
            return Some(format!("use `{k}`"));
        }
        let module = self.current_module.as_str();
        let in_module = |k: &'a String| {
            let (m, n) = k.rsplit_once("::").unwrap_or(("", k.as_str()));
            (m == module).then_some(n)
        };
        let locals = self
            .scopes
            .iter()
            .flat_map(|s| s.keys().map(String::as_str));
        let all = || {
            self.env
                .fn_table
                .keys()
                .chain(self.env.generic_fns.keys())
                .chain(self.env.consts.keys())
                .chain(self.env.statics.keys())
        };
        let public = all()
            .filter(|k| self.env.item_vis(k).unwrap_or(Visibility::Pub) == Visibility::Pub)
            .map(String::as_str);
        if let Some(help) = resolve::own_library_import(name, public) {
            return Some(help);
        }
        let mut keys = [
            self.env.fn_leaf(name),
            self.env.value_leaf(true, name),
            self.env.value_leaf(false, name),
        ]
        .concat();
        keys.retain(|k| !self.env.names.type_member(k));
        let import = self
            .env
            .names
            .import_help(module, keys, |k| self.env.item_vis(k));
        let items = all().filter_map(in_module);
        let typo = rokc_diagnostic::suggest(name, locals.chain(items))
            .map(|s| format!("did you mean `{s}`?"));
        join_help(import, typo)
    }

    /// A bare `name` that is only some enum's variant, with no expected
    /// type to pick it (spec 5.7.1): the error and its help, naming the
    /// variant by its shortest path here.
    fn variant_not_in_scope(&self, name: &str) -> Option<(String, String)> {
        // Another language's spelling has its own help (`none`).
        if matches!(
            name,
            "null" | "nil" | "None" | "NULL" | "True" | "False" | "Some"
        ) {
            return None;
        }
        let names = &self.env.names;
        let cur = self.current_module.as_str();
        let write = |e: &String| {
            if self.canon_type(&[leaf_name(e)]).as_ref() == Some(e) {
                return leaf_name(e).to_owned();
            }
            names
                .prelude_write(cur, std::iter::once(e))
                .unwrap_or_else(|| resolve::use_path(cur, e))
        };
        let mut paths: Vec<String> = names
            .variant_leaf(name)
            .iter()
            .map(|k| module_of(k))
            .filter(|e| {
                names.hidden(cur, e).is_none()
                    && names
                        .type_vis(e)
                        .is_none_or(|v| visible_from(cur, &module_of(e), v))
            })
            .map(|e| format!("{}::{name}", write(&e)))
            .collect();
        paths.sort_by_key(|p| (p.matches("::").count(), p.len(), p.clone()));
        paths.dedup();
        let first = paths.first()?;
        let msg = format!("`{name}` is not in scope: it is the variant `{first}`");
        let picks: Vec<String> = paths.iter().take(3).map(|p| format!("`{p}`")).collect();
        let help = format!("write {}", picks.join(" or "));
        Some((msg, help))
    }

    /// `m::f` naming an item of a module `m` that is not imported: the
    /// `use` that brings `m` in.
    fn import_help(&self, segs: &[&str]) -> Option<String> {
        let tail = format!("::{}", segs.join("::"));
        let env = self.env;
        let found = env
            .fn_table
            .keys()
            .chain(env.generic_fns.keys())
            .chain(env.consts.keys())
            .chain(env.statics.keys())
            .filter_map(|k| k.strip_suffix(&tail))
            .min()?;
        Some(format!(
            "add `use {found}::{};` at the top of the file",
            segs[0]
        ))
    }

    /// User-facing type name (no internal `Struct0` / mangled names).
    fn ty_name(&self, ty: &HirTy) -> String {
        if let Some(n) = ty
            .as_const()
            .and_then(|(v, _)| bounds::const_name(self.env, v))
        {
            return n;
        }
        if let Some((v, p)) = ty.as_const() {
            return match u32::try_from(v).ok().and_then(char::from_u32) {
                Some(c) if p == Some(PrimTy::Char) => format!("{c:?}"),
                _ => v.to_string(),
            };
        }
        match ty {
            HirTy::GcBuf(t) => format!("GcBuf<{}>", self.ty_name(t)),
            HirTy::Never => "!".to_owned(),
            HirTy::Struct(_) | HirTy::Enum(_) => {
                let (name, generic_args) = match ty {
                    HirTy::Struct(id) => {
                        let d = self.struct_def(*id);
                        (d.name, d.generic_args)
                    }
                    HirTy::Enum(id) => {
                        let d = self.enum_def(*id);
                        (d.name, d.generic_args)
                    }
                    _ => unreachable!("matched struct / enum"),
                };
                if name == self.lang("int_type") {
                    return "int".to_owned();
                }
                if name == self.lang("float_type") {
                    return "float".to_owned();
                }
                // A library type by its name (`List<i32>`, not
                // `rok::core::collections::list::List<i32>`).
                let name = if name.starts_with("rok::") {
                    let base = name.split("__").next().unwrap_or(&name);
                    leaf_name(base).to_owned() + &name[base.len()..]
                } else {
                    name
                };
                if generic_args.is_empty() {
                    name
                } else {
                    let base = name.split("__").next().unwrap_or(&name).to_owned();
                    let args: Vec<String> = generic_args.iter().map(|a| self.ty_name(a)).collect();
                    format!("{base}<{}>", args.join(", "))
                }
            }
            HirTy::Prim(p) => p.name().to_owned(),
            HirTy::Unit => "()".to_owned(),
            HirTy::Any => "any".to_owned(),
            HirTy::AnyTrait(b) if b.is_empty() => "any".to_owned(),
            HirTy::AnyTrait(b) if b.len() == 1 => format!("any {}", self.trait_name(&b[0])),
            HirTy::AnyTrait(b) => {
                let b: Vec<String> = b.iter().map(|r| self.trait_name(r)).collect();
                format!("any ({})", b.join(" + "))
            }
            HirTy::Unknown => "{unknown}".to_owned(),
            HirTy::Var(_) => "_".to_owned(),
            HirTy::Param(p) => self.env.sink.params.borrow()[*p as usize].name.clone(),
            HirTy::Path(p) => p.join("::"),
            HirTy::None => "none".to_owned(),
            HirTy::OpenConst(n) => format!("#{n}"),
            HirTy::NoThrow => "#nothrow".to_owned(),
            HirTy::Const(v, _) => v.to_string(),
            HirTy::Tuple(parts) => {
                let inner: Vec<String> = parts.iter().map(|t| self.ty_name(t)).collect();
                format!("({})", inner.join(", "))
            }
            HirTy::Sum(_) => ty.sum_name(|t| self.ty_name(t)),
            HirTy::Array(e, n) => format!(
                "[{}; {}]",
                self.ty_name(e),
                bounds::const_name(self.env, i128::from(*n)).unwrap_or_else(|| n.to_string())
            ),
            HirTy::Fn(args, ret, _) | HirTy::Closure { args, ret, .. } => {
                let inner: Vec<String> = args.iter().map(|t| self.ty_name(t)).collect();
                let kw = if matches!(ty, HirTy::Fn(..)) {
                    "fn"
                } else {
                    "closure"
                };
                let ret = match &**ret {
                    HirTy::Throws { .. } => self.ty_name(ret),
                    r => format!("-> {}", self.ty_name(r)),
                };
                let (bounds, kw): (String, &str) = match ty {
                    HirTy::Fn(_, _, b) => (
                        b.auto.iter().flat_map(|k| [" + ", leaf_name(k)]).collect(),
                        match (b.unsafe_fn, b.c_abi, b.stdcall) {
                            (true, true, true) => "unsafe extern \"system\" fn",
                            (false, true, true) => "extern \"system\" fn",
                            (true, true, false) => "unsafe extern \"C\" fn",
                            (true, false, _) => "unsafe fn",
                            (false, true, false) => "extern \"C\" fn",
                            (false, false, _) => kw,
                        },
                    ),
                    _ => (String::new(), kw),
                };
                format!("{kw}({}) {ret}{bounds}", inner.join(", "))
            }
            HirTy::Throws { ok, err } => {
                format!("throws ({}) -> {}", self.ty_name(err), self.ty_name(ok))
            }
        }
    }

    /// `Path<A, Name = B>` for diagnostics.
    fn trait_name(&self, r: &TraitRef) -> String {
        r.render(&self.display_path(&r.path), |t| self.ty_name(t))
    }

    /// A library item as written: `List::sort` for
    /// `rok::core::collections::list::List__sort`, the leaf when no other
    /// function shares it, else the shortest suffix naming it alone
    /// (`thread::spawn`). Other paths are kept.
    fn display_path(&self, full: &str) -> String {
        // A generic impl's method `__impl1f2e3d4c_show` (or `_2_show`).
        if let Some(rest) = leaf_name(full).strip_prefix("__impl") {
            let rest = rest.get(8..).unwrap_or_default();
            let rest = rest.trim_start_matches(|c: char| c == '_' || c.is_ascii_digit());
            if !rest.is_empty() {
                return rest.to_owned();
            }
        }
        if !full.starts_with("rok::") {
            return full.to_owned();
        }
        let leaf = leaf_name(full);
        if let Some((ty, method)) = leaf.split_once("__") {
            return format!("{ty}::{method}");
        }
        let others = self.env.fn_leaf(leaf);
        let segs: Vec<&str> = full.split("::").collect();
        (1..segs.len())
            .map(|n| segs[segs.len() - n..].join("::"))
            .find(|s| {
                let tail = format!("::{s}");
                // The prelude's re-exports name the same items.
                others.iter().all(|o| {
                    o == full
                        || !o.ends_with(&tail)
                        || o.starts_with(&format!("{}::", resolve::PRELUDE))
                })
            })
            .unwrap_or_else(|| full.to_owned())
    }

    /// The item bound to lang slot `slot` (spec 6.4), `""` if unbound.
    fn lang(&self, slot: &str) -> &'a str {
        self.env.names.lang_item(slot)
    }

    /// The trait refs of `ty`'s concrete impls.
    fn concrete_impls(&self, ty: &HirTy) -> impl Iterator<Item = &'a TraitRef> + use<'a> {
        let env: &'a Env<'a> = self.env;
        env.impls.refs_on(ty)
    }

    /// Does `ty` implement trait `path` (whatever its arguments)?
    fn has_trait(&self, ty: &HirTy, path: &str) -> bool {
        self.implements_trait(ty, &TraitRef::plain(path), 0)
    }

    /// Does `ty` meet `want`: an impl covers it (`has_impl`) or a
    /// structural rule does (auto traits, sum members, `any` bounds,
    /// `Reflect`).
    fn implements_trait(&self, ty: &HirTy, want: &TraitRef, depth: u32) -> bool {
        let trait_key = want.path.as_str();
        if trait_key == reflect::REFLECT {
            return true;
        }
        if self.env.names.auto_traits.contains(trait_key) {
            return self.auto_trait(ty, trait_key, depth, &mut AutoSeen::default());
        }
        // A sum has the structural traits its variants have (`none`
        // has them all); `expr::sum_structural_call` dispatches them.
        match ty {
            HirTy::Sum(vs) if want.is_plain() => {
                // `T | none` defaults to `none`.
                if trait_key == DEFAULT_TRAIT {
                    return vs.iter().any(HirTy::is_none);
                }
                // No order across two value terms (spec 2.6.4).
                if expr::is_order_trait(self, trait_key) && expr::sum_value_terms(ty) > 1 {
                    return false;
                }
                if expr::is_sum_trait(self, trait_key) {
                    return vs
                        .iter()
                        .all(|v| v.is_none() || self.implements_trait(v, want, depth + 1));
                }
            }
            // An `any` value formats through its bound (`expr::any_fmt`).
            HirTy::AnyTrait(b)
                if want.is_plain()
                    && (trait_key == self.lang("display_trait")
                        || trait_key == self.lang("debug_trait")) =>
            {
                return bound_covers(self, b, want);
            }
            _ => {}
        }
        self.has_impl(ty, want, depth)
    }

    /// Structural `Send` / `Sync`: `unsafe impl` opts in, `impl !Tr` out,
    /// else every component must have it. A recursive type assumes it for
    /// itself (`seen`).
    fn auto_trait(&self, ty: &HirTy, key: &str, depth: u32, seen: &mut AutoSeen) -> bool {
        let memo = self.env.auto_memo.borrow().is_some();
        if let Some(&v) = self
            .env
            .auto_memo
            .borrow()
            .as_ref()
            .and_then(|t| t.get(key)?.get(ty))
        {
            return v;
        }
        let at = seen.open.len();
        let low = std::mem::replace(&mut seen.low, usize::MAX);
        let ok = self.auto_trait_inner(ty, key, depth, seen);
        // A `true` that assumed an enclosing type holds only inside it.
        let assumed = seen.low < at;
        seen.low = seen.low.min(low);
        if memo && (!ok || !assumed) {
            if let Some(t) = self.env.auto_memo.borrow_mut().as_mut() {
                t.entry(key.to_owned()).or_default().insert(ty.clone(), ok);
            }
        }
        ok
    }

    fn auto_trait_inner(&self, ty: &HirTy, key: &str, depth: u32, seen: &mut AutoSeen) -> bool {
        if self.has_impl(ty, &TraitRef::plain(key), depth) {
            return true;
        }
        let base = match ty {
            HirTy::Struct(id) => Some(self.struct_def(*id).name),
            HirTy::Enum(id) => Some(self.enum_def(*id).name),
            _ => None,
        };
        if let Some(b) = base {
            let b = b.split("__").next().unwrap_or(&b).to_owned();
            if self.env.opts_out(&b, key) {
                return false;
            }
        }
        if let Some(i) = seen.open.iter().position(|t| t == ty) {
            seen.low = seen.low.min(i);
            return true;
        }
        seen.open.push(ty.clone());
        let mut rec = |t: &HirTy| self.auto_trait(t, key, depth, seen);
        let ok = match ty {
            HirTy::Tuple(ts) | HirTy::Sum(ts) => ts.iter().all(&mut rec),
            HirTy::Array(e, _) | HirTy::GcBuf(e) => rec(e),
            // A closure crossing threads shares its captures with the
            // creator, so each must be `Sync` too; a `[ref x]` cell never is
            // (spec 4.6.9).
            HirTy::Closure { env, shares, .. } => {
                let sync = self.lang("sync_trait");
                !shares
                    && rec(env)
                    && (key == sync || self.auto_trait(env, sync, depth, &mut AutoSeen::default()))
            }
            HirTy::Throws { ok, err } => rec(ok) && rec(err),
            HirTy::Struct(id) => {
                let def = self.struct_def(*id);
                // A union has what every declared field has (its storage
                // is plain bytes).
                match &def.union_kind {
                    Some(u) => u.aliases.iter().all(|(_, t)| rec(t)),
                    None => def.fields.iter().all(|f| rec(&f.ty)),
                }
            }
            HirTy::Enum(id) => self
                .enum_def(*id)
                .variants
                .iter()
                .all(|v| v.payload_ty.as_ref().is_none_or(&mut rec)),
            // A `+ Send` closure's captures are all `Sync` (above), so a
            // `+ Send` value is also `Sync`.
            HirTy::Fn(_, _, b) => b.auto.iter().any(|k| {
                k == key || (key == self.lang("sync_trait") && *k == self.lang("send_trait"))
            }),
            // As for a `+ Send` closure: copies of an `any` share its box, so
            // crossing threads needs `Sync` too (no collection inside).
            HirTy::AnyTrait(b) => {
                bound_covers(self, b, &TraitRef::plain(key))
                    && (key != self.lang("send_trait")
                        || bound_covers(self, b, &TraitRef::plain(self.lang("sync_trait"))))
            }
            // Unresolved paths (`none`, type-param leftovers) and error
            // types count as having it: no cascade errors.
            HirTy::Prim(_)
            | HirTy::Unit
            | HirTy::Never
            | HirTy::Any
            | HirTy::Unknown
            | HirTy::Path(_)
            | HirTy::None
            | HirTy::Const(..)
            | HirTy::OpenConst(_)
            | HirTy::NoThrow
            | HirTy::Var(_) => true,
            // Only through its bounds (`has_impl`).
            HirTy::Param(_) => false,
        };
        seen.open.pop();
        ok
    }

    /// Why `ty` lacks `key`: the path to the component that opts out, to
    /// follow "`T` is not `Sync`".
    fn auto_trait_cause(&self, ty: &HirTy, key: &str, depth: u32) -> String {
        self.cause_on(ty, key, depth, &[])
    }

    /// `auto_trait_cause` below the types `path` already went through: a
    /// type that contains itself is said once, not followed round again.
    fn cause_on(&self, ty: &HirTy, key: &str, depth: u32, path: &[HirTy]) -> String {
        let tr = leaf_name(key);
        let lacks = |t: &HirTy| !self.auto_trait(t, key, 0, &mut AutoSeen::default());
        let mut below = path.to_vec();
        below.push(ty.clone());
        let step = |what: String, t: &HirTy| {
            let name = self.ty_name(t);
            if below.contains(t) {
                return format!(
                    " because {what} has type `{name}`, and so on: `{name}` contains itself"
                );
            }
            format!(
                " because {what} has type `{name}`{}",
                self.cause_on(t, key, depth + 1, &below)
            )
        };
        // Of two causes, the one that reaches the opting-out part beats one
        // that only comes round to a type already on the path.
        let not_round = |a: String, b: String| {
            if a.ends_with("contains itself") && !b.ends_with("contains itself") {
                b
            } else {
                a
            }
        };
        if depth > 8 {
            return String::new();
        }
        match ty {
            HirTy::Struct(id) => {
                let def = self.struct_def(*id);
                let base = def.name.split("__").next().unwrap_or(&def.name);
                if base.starts_with("rok::core::collections::") {
                    return format!(
                        " (collections are not `{tr}`; share one through a `Mutex`, or use an \
                         array)"
                    );
                }
                if base == "rok::bare::gc::Weak" && key == self.lang("send_trait") {
                    return " (a hand-off cannot copy what a `Weak` names, so it stays with its \
                            thread: send the `GcPtr`; stdlib 2.13.9)"
                        .to_owned();
                }
                if self.env.opts_out(base, key) {
                    return String::new();
                }
                if base == "rok::core::sync::RwLock" {
                    if let Some(t) = def.generic_args.first() {
                        let t = self.ty_name(t);
                        return format!(
                            ": its readers could change the `{t}` inside at once; use \
                             `Mutex<{t}>` and `.lock()` in place of `.read()` / `.write()`"
                        );
                    }
                }
                def.fields
                    .iter()
                    .filter(|f| lacks(&f.ty))
                    .map(|f| step(format!("field `{}`", f.name), &f.ty))
                    .reduce(not_round)
                    .unwrap_or_default()
            }
            HirTy::Enum(id) => self
                .enum_def(*id)
                .variants
                .iter()
                .filter_map(|v| {
                    let t = v.payload_ty.as_ref().filter(|t| lacks(t))?;
                    Some(step(format!("variant `{}`", v.name), t))
                })
                .reduce(not_round)
                .unwrap_or_default(),
            HirTy::Tuple(ts) | HirTy::Sum(ts) => ts
                .iter()
                .filter(|t| lacks(t))
                .map(|t| step("a member".to_owned(), t))
                .reduce(not_round)
                .unwrap_or_default(),
            HirTy::Array(e, _) | HirTy::GcBuf(e) => step("an element".to_owned(), e),
            HirTy::AnyTrait(_) if key == self.lang("send_trait") => " (an `any` crosses \
                 threads only when its bound has both `Send` and `Sync`, since copies share \
                 its box: give the trait `Sync` as a supertrait, or send the concrete type; \
                 stdlib 2.13.9)"
                .to_owned(),
            _ => String::new(),
        }
    }

    /// A closure value's first `[ref x]` capture: `x` and its type.
    fn ref_capture(&self, value: &HirExpr) -> Option<(String, HirTy)> {
        let HirExprKind::ClosureValue { env, .. } = &value.kind else {
            return None;
        };
        let HirExprKind::Tuple { fields } = &env.kind else {
            return None;
        };
        fields.iter().find_map(|f| {
            let (HirExprKind::Local(l), HirTy::GcBuf(x)) = (&f.kind, &f.ty) else {
                return None;
            };
            let local = self.locals.get(l.0 as usize).filter(|x| x.boxed)?;
            Some((local.name.clone()?, (**x).clone()))
        })
    }

    /// For a closure that does not fit a `fn .. + Send` slot: which
    /// capture is not `Send` / `Sync`, and why.
    fn closure_bound_note(&self, value: &HirExpr) -> Option<String> {
        let HirExprKind::ClosureValue { env, .. } = &value.kind else {
            return None;
        };
        let HirExprKind::Tuple { fields } = &env.kind else {
            return None;
        };
        let keys = [self.lang("send_trait"), self.lang("sync_trait")];
        fields.iter().find_map(|f| {
            let local = match &f.kind {
                HirExprKind::Local(l) => *l,
                HirExprKind::BufGet { buf, .. } => match buf.kind {
                    HirExprKind::Local(l) => l,
                    _ => return None,
                },
                _ => return None,
            };
            let key = keys
                .iter()
                .find(|k| !self.auto_trait(&f.ty, k, 0, &mut AutoSeen::default()))?;
            let name = self.locals.get(local.0 as usize)?.name.clone()?;
            let ty = self.ty_name(&f.ty);
            let guard = ["MutexGuard<", "RwLockReadGuard<", "RwLockWriteGuard<"]
                .iter()
                .any(|g| ty.starts_with(g));
            let read = if guard {
                format!(
                    "; a guard stays with its `using` block: read it first (`let v = \
                     {name}.get();`) and capture `v`"
                )
            } else {
                String::new()
            };
            Some(format!(
                "the closure captures `{name}` (type `{ty}`), which is not `{}`{}{read}",
                leaf_name(key),
                self.auto_trait_cause(&f.ty, key, 0)
            ))
        })
    }

    /// Does an impl give `ty` trait `want` (spec 2.9): a concrete one, or a
    /// covering generic one (`generic_impls`)? All impl questions go
    /// through here, `concrete_impls` and `generic_impls`.
    fn has_impl(&self, ty: &HirTy, want: &TraitRef, depth: u32) -> bool {
        if let HirTy::Param(p) = ty {
            if bounds::has(self, *p, want) {
                return true;
            }
        }
        if bounds::assumed(self.env, ty, want) {
            return true;
        }
        self.env
            .impls
            .refs_of(ty, &want.path)
            .any(|r| r.satisfies(want))
            || self
                .generic_impls(ty, Pick::Trait(&want.path), depth)
                .any(|(gi, subst)| {
                    want.is_plain() || self.generic_impl_ref(gi, &subst).satisfies(want)
                })
    }

    /// Generic impls `keep` picks (`trait_key` `""`: inherent) covering
    /// `ty`, with their bindings.
    fn generic_impls<'s>(
        &'s self,
        ty: &'s HirTy,
        pick: Pick<'s>,
        depth: u32,
    ) -> impl Iterator<Item = (&'a Impl, TySubst)> + 's {
        let env: &'a Env<'a> = self.env;
        env.impls
            .generic_by(pick)
            .filter_map(move |gi| Some((gi, self.generic_impl_subst(gi, ty, depth)?)))
    }

    /// The trait ref generic impl `gi` provides under `subst`: its trait
    /// arguments and associated types instantiated.
    fn generic_impl_ref(&self, gi: &Impl, subst: &TySubst) -> TraitRef {
        let mut rcx = LowerCtx::new(self.env, subst, None, self.here());
        rcx.current_module.clone_from(&gi.module);
        rcx.set_bounds(&gi.generics, &gi.where_clause);
        TraitRef::new(
            gi.trait_key.clone(),
            gi.trait_args
                .iter()
                .map(|t| rcx.resolve_ast_ty(t))
                .collect(),
            gi.assoc
                .iter()
                .map(|(n, t)| (n.clone(), rcx.resolve_ast_ty(t)))
                .collect(),
        )
    }

    /// Bound `b` (an AST trait path) as written in `module`, its type
    /// arguments resolved by `rec`.
    fn bound_ref(&self, b: &Ty, module: &str, rec: impl FnMut(&Ty) -> HirTy) -> Option<TraitRef> {
        let AstTyKind::Path(p) = &b.kind else {
            return None;
        };
        Some(resolve::trait_ref(
            &self.env.names,
            p,
            module,
            self.here(),
            rec,
        ))
    }

    /// The impl's type-parameter bindings when generic impl `gi` covers
    /// `ty`: `ty` matches its target and meets its bounds.
    fn generic_impl_subst(
        &self,
        gi: &Impl,
        ty: &HirTy,
        depth: u32,
    ) -> Option<HashMap<String, HirTy>> {
        self.generic_impl_match(gi, ty, depth).ok()
    }

    /// `generic_impl_subst` with a reason: `Err(Some((T, B)))` when the
    /// target matches but `T: B` fails, `Err(None)` when it doesn't match.
    fn generic_impl_match(
        &self,
        gi: &Impl,
        ty: &HirTy,
        depth: u32,
    ) -> Result<HashMap<String, HirTy>, Option<(HirTy, TraitRef)>> {
        // Depth cap instead of cycle detection (`impl<T: Tr> Tr for T`).
        if depth > 8 || matches!(ty, HirTy::Unknown) {
            return Err(None);
        }
        let params: std::collections::HashSet<String> = gi
            .generics
            .params
            .iter()
            .map(|gp| match gp {
                rokc_ast::GenericParam::Type { name, .. }
                | rokc_ast::GenericParam::Const { name, .. } => name.clone(),
            })
            .collect();
        let mut subst = HashMap::new();
        unify_param_ty_with(&gi.self_ty, ty, &params, &mut subst, self);
        // Parameters only the trait arguments name are left to the call.
        let shown = collect::impl_def_display(&gi.self_ty);
        let mut in_self = shown
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| params.contains(*w));
        if in_self.any(|p| !subst.contains_key(p)) {
            return Err(None);
        }
        // `impl<T, const N: usize> Tr for [T; N]`: `N` takes its type.
        for gp in &gi.generics.params {
            if let rokc_ast::GenericParam::Const { name, ty: cty } = gp {
                let prim = match crate::ty::resolve_ty(cty) {
                    HirTy::Prim(p) => Some(p),
                    _ => None,
                };
                if let Some((v, _)) = subst.get(name).and_then(HirTy::as_const) {
                    subst.insert(name.clone(), HirTy::Const(v, prim));
                }
            }
        }
        let mut rcx = LowerCtx::new(self.env, &subst, None, self.here());
        rcx.current_module.clone_from(&gi.module);
        rcx.set_bounds(&gi.generics, &gi.where_clause);
        // Another generic struct's arguments (`List<i32>` against `Rev<I>`)
        // would instantiate a bogus struct.
        if let (AstTyKind::Path(p), HirTy::Struct(_) | HirTy::Enum(_)) = (&gi.self_ty.kind, ty) {
            let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
            let name = match ty {
                HirTy::Struct(id) => self.struct_def(*id).name,
                HirTy::Enum(id) => self.enum_def(*id).name,
                _ => unreachable!("matched struct / enum"),
            };
            if p.segments.last().is_some_and(|s| !s.generics.is_empty())
                && rcx.canon_type(&segs).as_deref() != Some(enum_base_name(&name))
            {
                return Err(None);
            }
        }
        if rcx.resolve_ast_ty(&gi.self_ty) != *ty {
            return Err(None);
        }
        let mut goals: Vec<(HirTy, &Vec<Ty>)> = Vec::new();
        for gp in &gi.generics.params {
            if let rokc_ast::GenericParam::Type { name, bounds, .. } = gp {
                if let Some(t) = subst.get(name) {
                    goals.push((t.clone(), bounds));
                }
            }
        }
        for w in &gi.where_clause {
            goals.push((rcx.resolve_ast_ty(&w.ty), &w.bounds));
        }
        for (bty, bounds) in goals {
            for b in bounds {
                let Some(want) = self.bound_ref(b, &gi.module, |t| rcx.resolve_ast_ty(t)) else {
                    continue;
                };
                if !self.implements_trait(&bty, &want, depth + 1) {
                    return Err(Some((bty, want)));
                }
            }
        }
        Ok(subst)
    }

    /// `any` tag of `ty`: a hash of its name, so separately compiled
    /// packages agree.
    // 31 bits; a collision probes to the next free tag, then build-local.
    fn any_tag(&self, ty: &HirTy) -> u32 {
        let mut tags = self.env.sink.any_tags.borrow_mut();
        let found = tags.iter().find(|(t, _)| t == ty).map(|(_, tag)| *tag);
        incr::log(
            || format!("a|{ty:?}"),
            || incr::Lookup::AnyTag(ty.clone(), found),
        );
        if let Some(tag) = found {
            return tag;
        }
        let mut tag = self.ty_name(ty).bytes().fold(0x811c_9dc5_u32, |h, b| {
            (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
        }) & 0x7fff_ffff;
        while tags.iter().any(|(_, t)| *t == tag) {
            tag = (tag + 1) & 0x7fff_ffff;
        }
        tags.push((ty.clone(), tag));
        tag
    }

    fn struct_def(&self, id: StructId) -> HirStructDef {
        self.env.defs.struct_at(id)
    }

    /// Struct id by table key (a declared struct or an instance).
    fn lookup_struct(&self, key: &str) -> Option<StructId> {
        self.env.defs.struct_id(key)
    }

    /// Instance `base<args>`, fields resolved here; a new one points bound
    /// errors at `span`.
    fn instantiate_struct(
        &self,
        base: &str,
        args: Vec<HirTy>,
        span: rokc_diagnostic::Span,
    ) -> Option<StructId> {
        let mut args = args;
        let in_base = |subst: &TySubst, t: &Ty| {
            let mut rcx = LowerCtx::new(self.env, subst, None, self.here());
            rcx.current_module = module_of(base);
            rcx.resolve_ast_ty(t)
        };
        if let Some(g) = self
            .env
            .defs
            .generics_of(base)
            .filter(|_| self.env.defs.is_generic_struct(base))
        {
            if let Some(e) = mono::fit_args(leaf_name(base), g, &mut args, in_base) {
                self.emit_arity(span, e);
            }
            self.check_const_args(g, &args)?;
        }
        self.env.defs.instantiate_struct(base, args, span, in_base)
    }

    fn enum_def(&self, id: EnumId) -> HirEnumDef {
        self.env.defs.enum_at(id)
    }

    /// Enum id by table key (a declared enum or an instance).
    fn lookup_enum(&self, key: &str) -> Option<EnumId> {
        self.env.defs.enum_id(key)
    }

    /// The instances of generic enum `base` made so far.
    fn enum_instantiations(&self, base: &str) -> Vec<EnumId> {
        let ids = Self::instantiations(&self.env.defs.enum_table.borrow(), base);
        incr::log(
            || format!("ie|{base}"),
            || incr::Lookup::EnumInsts(base.to_owned(), ids.clone()),
        );
        ids
    }

    /// `instantiate_struct` for a generic enum.
    fn instantiate_enum(
        &self,
        base: &str,
        args: Vec<HirTy>,
        span: rokc_diagnostic::Span,
    ) -> Option<EnumId> {
        let mut args = args;
        if let Some(def) = self.env.defs.generic_enum(base) {
            let misfit = mono::fit_args(leaf_name(base), &def.generics, &mut args, |subst, t| {
                let mut rcx = LowerCtx::new(self.env, subst, None, self.here());
                rcx.current_module = module_of(base);
                rcx.resolve_ast_ty(t)
            });
            if let Some(e) = misfit {
                self.emit_arity(span, e);
            }
            self.check_const_args(&def.generics, &args)?;
        }
        self.env
            .defs
            .instantiate_enum(base, args, span, |def, subst| {
                let mut rcx = LowerCtx::new(self.env, subst, None, self.here());
                rcx.current_module = module_of(base);
                generic_enum_variants(def, &|t| rcx.resolve_ast_ty(t))
            })
    }

    /// A generic type's argument list that does not fit (`mono::fit_args`).
    fn emit_arity(&self, span: Span, (msg, help): (String, String)) {
        let at = if span.is_dummy() { self.here() } else { span };
        self.emit_diag(
            rokc_diagnostic::RokDiagnostic::error(rokc_diagnostic::ErrorKind::Type, at, msg)
                .with_help(help),
        );
    }

    /// A `char` const parameter takes a `char` argument, any other an
    /// integer or `bool` (spec 2.11.3).
    fn check_const_args(&self, generics: &rokc_ast::Generics, args: &[HirTy]) -> Option<()> {
        for (gp, arg) in generics.params.iter().zip(args) {
            if let (rokc_ast::GenericParam::Const { name, ty }, Some(v)) = (gp, arg.as_const()) {
                mono::typed_const(self, name, ty, v)?;
            }
        }
        Some(())
    }

    /// Canonical type-namespace name of a path, from this module and its
    /// `use`s.
    fn canon_type(&self, segments: &[&str]) -> Option<String> {
        if let Some(key) = self.self_key(segments) {
            return Some(key);
        }
        self.env.names.canon(
            segments,
            &self.access_module(),
            self.reach_uses(),
            self.here(),
        )
    }

    /// The key of `Self` when `segments` is the absolute path a `Self::..`
    /// rewrites to. `Self` is the impl's target, visible from the impl's
    /// module: a library trait's default may name a user's private type.
    fn self_key(&self, segments: &[&str]) -> Option<String> {
        let key = match segments {
            ["pkg", rest @ ..] => rest.join("::"),
            ["rok", ..] => segments.join("::"),
            _ => return None,
        };
        let is_self = match &self.self_ty {
            Some(HirTy::Struct(id)) => self.lookup_struct(&key) == Some(*id),
            Some(HirTy::Enum(id)) => self.lookup_enum(&key) == Some(*id),
            _ => false,
        };
        is_self.then_some(key)
    }

    /// Split `a::E::V` into (`a::E`, `V`) when the prefix is an enum; a
    /// single segment is an imported variant (`use E::*`, `Ok` via core).
    fn enum_path(&self, segments: &[&str]) -> Option<(String, String)> {
        let (variant, ty) = segments.split_last()?;
        if ty.is_empty() {
            let key = self.env.names.canon_variant(
                segments,
                &self.current_module,
                &self.local_uses,
                self.here(),
            )?;
            let (e, v) = key.rsplit_once("::")?;
            return Some((e.to_owned(), v.to_owned()));
        }
        Some((self.canon_type(ty)?, (*variant).to_owned()))
    }

    /// Instantiations of generic base `base` in `table`, sorted by key
    /// for deterministic choice.
    fn instantiations<V: Copy>(table: &HashMap<String, V>, base: &str) -> Vec<V> {
        let prefix = format!("{base}__");
        let mut hits: Vec<(&String, V)> = table
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| (k, *v))
            .collect();
        hits.sort_by(|a, b| a.0.cmp(b.0));
        hits.into_iter().map(|(_, v)| v).collect()
    }

    /// The module `const` (`statics`: `static`) that `segments` names.
    fn resolve_in(&self, segments: &[&str], statics: bool) -> Option<String> {
        let env = self.env;
        let exists = |k: &str| {
            if statics {
                env.static_ty(k).is_some()
            } else {
                env.konst(k).is_some()
            }
        };
        let keys = |leaf: &str| {
            incr::read(|| {
                if statics {
                    incr::Read::StaticLeaf(leaf.to_owned())
                } else {
                    incr::Read::ConstLeaf(leaf.to_owned())
                }
            });
            env.value_leaf(statics, leaf)
        };
        let visible = |k: &str| {
            let vis = self.env.item_vis(k);
            vis.is_none_or(|v| self.is_visible(&module_of(k), v))
        };
        let set = NameSet {
            exists: &exists,
            keys: &keys,
            visible: &visible,
            in_type: &|k| env.names.has_type(k),
        };
        match resolve_name(
            segments,
            &self.access_module(),
            self.reach_uses(),
            &self.env.names.uses,
            &set,
        ) {
            Ok(r) => r,
            Err(hits) => {
                self.report_ambiguous(&segments.join("::"), &hits);
                hits.into_iter().next()
            }
        }
    }

    /// Resolve a function path across concrete and generic functions, so
    /// the nearest declaration wins (spec 5.7: own items, `use`s, core).
    fn resolve_any_fn(&self, segments: &[&str]) -> Option<String> {
        let env = self.env;
        // A hook lowering calls (`expr::lang_fn`): its slot's item, else
        // the path it had before it had a slot.
        if let Some(slot) = segments.first().and_then(|s| expr::lang_slot(s)) {
            let item = self.lang(slot);
            if !item.is_empty() {
                return Some(item.to_owned());
            }
            return self.resolve_any_fn(&segments[1..]);
        }
        if let [name] = segments {
            // An empty key: a `use` of the name hides outer nested fns.
            let nested = self.nested_fns.iter().rev().find_map(|m| m.get(*name));
            if let Some(k) = nested.filter(|k| !k.is_empty()) {
                return Some(k.clone());
            }
        }
        let exists = |k: &str| env.sig(k).is_some() || env.generic_fn(k).is_some();
        let keys = |leaf: &str| env.fn_leaf(leaf);
        let visible = |k: &str| {
            let vis = self.env.item_vis(k);
            vis.is_none_or(|v| self.is_visible(&module_of(k), v))
        };
        let set = NameSet {
            exists: &exists,
            keys: &keys,
            visible: &visible,
            in_type: &|k| env.names.has_type(k),
        };
        match resolve_name(
            segments,
            &self.access_module(),
            self.reach_uses(),
            &self.env.names.uses,
            &set,
        ) {
            Ok(r) => r,
            // An item and its re-exports are one item (spec 5.8.4).
            Err(mut hits) => {
                let item = |k: &String| {
                    (
                        env.sig(k).map(|s| s.id),
                        env.generic_fn(k).map(std::ptr::from_ref::<FnDef>),
                    )
                };
                // Each item once, by its shortest path.
                hits.sort_by_key(|h| (h.len(), h.clone()));
                let mut seen = Vec::new();
                hits.retain(|h| {
                    let it = item(h);
                    let new = !seen.contains(&it);
                    seen.push(it);
                    new
                });
                if hits.len() > 1 {
                    self.report_ambiguous(&segments.join("::"), &hits);
                }
                hits.into_iter().next()
            }
        }
    }

    /// Resolve a callee name (single- or multi-segment path) to a fully
    /// qualified fn-table key (a concrete fn), or an `extern "C"` one.
    fn resolve_fn_name(&self, segments: &[&str]) -> Option<String> {
        match self.resolve_any_fn(segments) {
            Some(f) => self.env.sig(&f).is_some().then_some(f),
            None => match segments {
                [name] if self.env.extern_fn(name).is_some() => Some((*name).to_owned()),
                _ => None,
            },
        }
    }

    /// `resolve_fn_name` for a generic fn (a `generic_fns` key).
    fn resolve_generic_fn_name(&self, segments: &[&str]) -> Option<String> {
        self.resolve_any_fn(segments)
            .filter(|f| self.env.generic_fn(f).is_some())
    }

    fn report_ambiguous(&self, name: &str, hits: &[String]) {
        self.emit_error(
            rokc_diagnostic::ErrorKind::Module,
            format!(
                "name `{name}` is ambiguous in module `{}` - multiple glob imports \
                 resolve it: {}",
                module_display(&self.access_module()),
                hits.join(", ")
            ),
        );
    }

    /// Visibility check: can a caller in `self.current_module` see an
    /// item declared in `def_module` with the given visibility?
    fn is_visible(&self, def_module: &str, vis: Visibility) -> bool {
        visible_from(&self.access_module(), def_module, vis)
    }

    /// Report use of a private value item (fn / const / static).
    fn check_item_visible(&self, full: &str, what: &str) -> bool {
        if self.harness_main {
            return true;
        }
        let vis = self.env.item_vis(full).unwrap_or(Visibility::Pub);
        let def_module = module_of(full);
        let from = self.access_module();
        if let Some(m) = self.env.names.hidden(&from, full) {
            self.emit_error(
                rokc_diagnostic::ErrorKind::Module,
                resolve::hidden_module_msg(full, &m, &from),
            );
            return false;
        }
        if self.is_visible(&def_module, vis) {
            return true;
        }
        if vis == Visibility::Pkg {
            self.emit_error(
                rokc_diagnostic::ErrorKind::Module,
                format!(
                    "{what} `{full}` is `pub(pkg)`: visible only inside its package \
                     `{}` (spec 5.6.4)",
                    module_display(package_of(&def_module))
                ),
            );
            self.help_last(resolve::own_library_privacy(&def_module));
            return false;
        }
        self.emit_error(
            rokc_diagnostic::ErrorKind::Module,
            format!(
                "{what} `{full}` is private to module `{}` and cannot be used from `{}` \
                 (spec 5.6 - add `pub` to expose it)",
                module_display(&def_module),
                module_display(&from)
            ),
        );
        self.help_last(resolve::own_library_privacy(&def_module));
        false
    }

    /// The module defining `sid`, when the current module may not see
    /// its field `field`.
    fn field_hidden(&self, sid: StructId, field: &str) -> Option<String> {
        let def = self.struct_def(sid);
        let base = if def.generic_args.is_empty() {
            def.name
        } else {
            def.name.split("__").next().unwrap_or(&def.name).to_owned()
        };
        let vis = self.env.fields_vis(&base)?.get(field).copied()?;
        let def_module = module_of(&base);
        (!self.is_visible(&def_module, vis)).then_some(def_module)
    }

    /// Report access to a private field from outside its struct's module,
    /// and warn about a marked one.
    fn check_field_visible(&self, sid: StructId, field: &str) {
        self.check_field_visible_at(sid, field, self.here());
    }

    /// `check_field_visible`, a marked field's warning at `at`.
    fn check_field_visible_at(&self, sid: StructId, field: &str, at: Span) {
        // The struct's own methods still set and read its marked fields.
        if self.marks_checked() && self.self_ty != Some(HirTy::Struct(sid)) {
            let name = self.struct_def(sid).name;
            let base = name.split("__").next().unwrap_or(&name);
            if let Some(m) = self.env.mark(&format!("{base}.{field}")) {
                let shown = format!("{}.{field}", leaf_name(base));
                self.use_marked("field", &shown, m, at);
            }
        }
        if let Some(def_module) = self.field_hidden(sid, field) {
            self.emit_error(
                rokc_diagnostic::ErrorKind::Module,
                format!(
                    "field `{field}` of `{}` is private to module `{}` (spec 5.6 - add \
                     `pub` to the field to expose it)",
                    self.ty_name(&HirTy::Struct(sid)),
                    module_display(&def_module)
                ),
            );
        }
    }

    /// Uses here warn about marked items: the code is the built
    /// package's.
    fn marks_apply(&self) -> bool {
        self.env
            .mark_pkg
            .as_deref()
            .is_some_and(|p| package_of(&self.access_module()) == p)
    }

    /// Uses here are checked against marks at all: the built package's,
    /// or any code outside the toolchain's packages (`#[internal]` holds
    /// in a dependency built from source too).
    fn marks_checked(&self) -> bool {
        self.marks_apply() || !is_toolchain_module(&self.current_module)
    }

    /// Statements here warn `unused_result`: the built package's, and
    /// with `--check-library` the library's.
    fn results_apply(&self) -> bool {
        self.marks_apply()
            || (self.env.lint_library && {
                let p = package_of(&self.current_module);
                p == "rok" || p.starts_with("rok::")
            })
    }

    /// A call at `at` of a function with marks `mark`: a discardable one
    /// is remembered for `unused_result`.
    fn call_marked(&self, mark: &crate::marks::Mark, at: Span) {
        if mark.discardable && self.results_apply() {
            self.discardable_calls.borrow_mut().push(at);
        }
    }

    /// A use at `at` of `name`, a `what` with marks `mark` (spec 6.2.7,
    /// 6.2.14, 6.2.16): an error for an `internal` one, else a `deprecated`
    /// warning, and outside `unsafe` a `deprecated_safe` one. Only the
    /// built package's own uses are checked.
    fn use_marked(&self, what: &str, name: &str, mark: &crate::marks::Mark, at: Span) {
        if at.is_dummy() {
            return;
        }
        if mark.internal && !is_toolchain_module(&self.current_module) {
            let e = rokc_diagnostic::RokDiagnostic::error(
                rokc_diagnostic::ErrorKind::Module,
                at,
                format!(
                    "{what} `{name}` is internal to the Rok toolchain: only `rok::bare`, \
                     `rok::core` and `rok::std` may use it (spec 6.2.16)"
                ),
            )
            .with_note(
                "it is not part of the standard library's API; use the public items the \
                 docs list instead",
            );
            let seen = self
                .env
                .sink
                .diagnostics
                .borrow()
                .iter()
                .any(|d| d.primary == e.primary && d.message == e.message);
            if !seen {
                self.emit_diag(e);
            }
            return;
        }
        if !self.marks_apply() {
            return;
        }
        let mut out = Vec::new();
        if let Some(d) = &mark.deprecated {
            let mut w = rokc_diagnostic::RokDiagnostic::warning(
                rokc_diagnostic::ErrorKind::Lint,
                at,
                format!("use of deprecated {what} `{name}`"),
            )
            .with_lint("deprecated")
            .with_tag(rokc_diagnostic::Tag::Deprecated);
            for n in d.notes(name) {
                w = w.with_note(n);
            }
            out.push(w);
        }
        if let (Some(note), 0) = (&mark.deprecated_safe, self.unsafe_depth) {
            let mut w = rokc_diagnostic::RokDiagnostic::warning(
                rokc_diagnostic::ErrorKind::Lint,
                at,
                format!(
                    "`{name}` will need `unsafe` in a later release: wrap the {} in \
                     `unsafe {{ .. }}`",
                    if what == "static" { "use" } else { "call" }
                ),
            )
            .with_lint("deprecated_safe");
            if !note.is_empty() {
                w = w.with_note(note.clone());
            }
            out.push(w);
        }
        // Every instance of a generic body lowers the same use.
        let mut sink = self.env.sink.diagnostics.borrow_mut();
        for w in out {
            if !sink
                .iter()
                .any(|d| d.primary == w.primary && d.message == w.message)
            {
                sink.push(w);
            }
        }
    }

    fn push_expected(&mut self, ty: HirTy) {
        self.expected_ty_stack.push(ty);
    }
    fn pop_expected(&mut self) {
        self.expected_ty_stack.pop();
    }
    fn current_expected(&self) -> Option<&HirTy> {
        self.expected_ty_stack.last()
    }

    /// AST type to `HirTy` under the substitution, `Self` and aliases;
    /// generic structs with concrete arguments are instantiated on demand.
    fn resolve_ast_ty(&self, ty: &rokc_ast::Ty) -> HirTy {
        let mut t = self.resolve_ty_inner(ty);
        if let Some(st) = &self.self_ty {
            substitute_params(&mut t, &HashMap::from([("Self".to_owned(), st.clone())]));
        }
        t
    }

    /// Spec 2.11.4 / 2.11.5: a compile-time integer (literal, const
    /// parameter, or const expression via `const_eval`).
    fn eval_const(&self, c: &rokc_ast::ConstExpr) -> Option<i128> {
        self.eval_const_msg(c).ok()
    }

    fn eval_const_msg(&self, c: &rokc_ast::ConstExpr) -> Result<i128, String> {
        self.eval_const_arg(c).map(|(v, _)| v)
    }

    /// A const argument's value, tagged `char` when it is one (a `char`
    /// const parameter takes only a `char`, spec 2.11.3).
    fn eval_const_arg(&self, c: &rokc_ast::ConstExpr) -> Result<(i128, Option<PrimTy>), String> {
        let not_int = || "not an integer".to_owned();
        let e = match c {
            rokc_ast::ConstExpr::Literal(rokc_ast::Literal::Char(s)) => {
                return expr::parse_char_literal(s)
                    .map(|v| (i128::from(v), Some(PrimTy::Char)))
                    .ok_or_else(not_int);
            }
            rokc_ast::ConstExpr::Literal(rokc_ast::Literal::Int(s)) => {
                return expr::parse_int_literal(s)
                    .map(|(v, _)| (v, None))
                    .ok_or_else(not_int);
            }
            rokc_ast::ConstExpr::Literal(rokc_ast::Literal::Bool(b)) => {
                return Ok((i128::from(*b), None));
            }
            rokc_ast::ConstExpr::Literal(_) => return Err(not_int()),
            rokc_ast::ConstExpr::Path(p) => {
                if let [seg] = p.segments.as_slice() {
                    if let Some((v, p)) = self.ty_subst.get(&seg.name).and_then(HirTy::as_const) {
                        return Ok((v, p.filter(|p| *p == PrimTy::Char)));
                    }
                }
                Expr {
                    kind: ExprKind::Path(p.clone()),
                    span: p.span,
                }
            }
            rokc_ast::ConstExpr::Expr(e) => (**e).clone(),
        };
        const_eval::eval_scalar(self, &e)
    }

    /// An array length; `None` if unknown (reported unless it waits for the
    /// second signature round or an unbound const parameter).
    fn array_len(&self, len: &rokc_ast::ConstExpr) -> Option<u32> {
        // No body lowers before signatures are known; a hard length waits
        // for the second signature round.
        if let Some(pending) = self.env.sink.sig_pass.get() {
            let param = |name: &str| {
                self.ty_subst
                    .get(name)
                    .and_then(HirTy::as_const)
                    .map(|c| c.0)
            };
            let names = &self.env.names;
            if let Some(n) = names.const_int(len, &self.current_module, &param) {
                if let Ok(n) = u32::try_from(n) {
                    return Some(n);
                }
            }
            if !pending {
                self.env.sink.sig_pass.set(Some(true));
            }
            return None;
        }
        // Spec 2.11.5: `[T; N]`, `[T; M * N]`, `[T; f(N)]`.
        let n = self
            .eval_const_msg(len)
            .and_then(|v| u32::try_from(v).map_err(|_| format!("length {v} is out of range")));
        match n {
            Ok(n) => Some(n),
            // A const parameter not bound yet (argument hints
            // before inference): no type yet, not an error.
            Err(_)
                if self
                    .ty_subst
                    .values()
                    .any(|t| matches!(t, HirTy::OpenConst(_))) =>
            {
                None
            }
            Err(why) => {
                self.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!(
                        "array length must be a compile-time integer (spec 2.11.5): \
                             {why}"
                    ),
                );
                None
            }
        }
    }

    fn resolve_ty_inner(&self, ty: &rokc_ast::Ty) -> HirTy {
        // A type path names what the module it comes from sees (spec
        // 6.9.9); its arguments carry their own origins.
        let origin = match &ty.kind {
            AstTyKind::Path(p) => p.origin.as_deref(),
            _ => None,
        };
        let outer = self.set_access(origin);
        let t = self.resolve_ty_here(ty);
        self.access.replace(outer);
        t
    }

    fn resolve_ty_here(&self, ty: &rokc_ast::Ty) -> HirTy {
        let rec = |t: &rokc_ast::Ty| self.resolve_ty_inner(t);
        let module = self.access_module();
        let module = &*module;
        let scope = Scope {
            names: &self.env.names,
            module,
            at: self.here(),
        };
        match &ty.kind {
            // `Self` inside generic arguments (`Map<Self, ..>`) must be
            // concrete before the instantiation check.
            AstTyKind::SelfUpper if self.self_ty.is_some() => {
                self.self_ty.clone().unwrap_or(HirTy::Unknown)
            }
            AstTyKind::Path(_)
                if self
                    .env
                    .names
                    .expand_alias(ty, module, self.here())
                    .is_some() =>
            {
                let t = self.env.names.expand_alias(ty, module, self.here());
                t.map_or(HirTy::Unknown, |t| rec(&t))
            }
            AstTyKind::Path(p) => {
                // Depth-capped: a cyclic alias is reported by the alias pass.
                let param =
                    matches!(p.segments.as_slice(), [s] if self.ty_subst.contains_key(&s.name));
                let names = &self.env.names;
                let alias = (!param && names.alias_depth.get() < 32)
                    .then(|| names.plain_alias(ty, module, self.here()))
                    .flatten();
                if let Some(full) = alias {
                    // Reported at the alias.
                    if names.broken_aliases.borrow().contains(&full) {
                        return HirTy::Unknown;
                    }
                    if let Some(t) = names.projection_alias(&full, module, self.here()) {
                        let depth = &names.alias_depth;
                        depth.set(depth.get() + 1);
                        let r = rec(&t);
                        depth.set(depth.get() - 1);
                        return r;
                    }
                }
                if let [seg] = p.segments.as_slice() {
                    if seg.generics.is_empty() {
                        if let Some(t) = self.ty_subst.get(&seg.name) {
                            return t.clone();
                        }
                        // `X` in a trait / impl body is `Self::X`.
                        if self.self_ty.is_some() && self.assoc_names.contains(&seg.name) {
                            let mut sp = p.clone();
                            sp.segments.insert(
                                0,
                                rokc_ast::PathSegment {
                                    name: "Self".to_owned(),
                                    generics: Vec::new(),
                                },
                            );
                            if let Some(t) = self.projection(&sp) {
                                return t;
                            }
                        }
                        // A `const` item as a const generic argument
                        // (`Grid<W, 3>`, spec 2.11.4).
                        if self.canon_type(&[seg.name.as_str()]).is_none()
                            && self.resolve_in(&[seg.name.as_str()], false).is_some()
                        {
                            if let Ok((v, t)) =
                                self.eval_const_arg(&rokc_ast::ConstExpr::Path(p.clone()))
                            {
                                return HirTy::Const(v, t);
                            }
                        }
                    }
                }
                if let Some(t) = self.projection(p) {
                    return t;
                }
                let generics = p.segments.last().map_or(&[][..], |s| s.generics.as_slice());
                let bare = generics.is_empty()
                    && p.segments
                        .last()
                        .is_some_and(|s| self.env.defs.defaulted(&s.name));
                if !generics.is_empty() || bare {
                    let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
                    if let Some(full) = self
                        .canon_type(&segs)
                        .filter(|f| !bare || self.env.defs.bare_instance(f))
                    {
                        if let (GC_BUF, [elem]) = (full.as_str(), generics) {
                            return HirTy::GcBuf(Box::new(rec(elem)));
                        }
                        let args: Vec<HirTy> = generics.iter().map(rec).collect();
                        // A new instance's bound errors point at this type.
                        let at = Some(ty.span)
                            .filter(|s| !s.is_dummy())
                            .unwrap_or_else(|| self.here());
                        let mut leftover = Vec::new();
                        args.iter()
                            .for_each(|a| collect_unknown_paths(a, &mut leftover));
                        // An inference variable is an argument like any type.
                        leftover.retain(|p| p != "_");
                        if leftover.is_empty() && self.env.defs.is_generic_struct(&full) {
                            if let Some(id) = self.instantiate_struct(&full, args, at) {
                                return HirTy::Struct(id);
                            }
                        } else if leftover.is_empty() {
                            if let Some(id) = self.instantiate_enum(&full, args, at) {
                                return HirTy::Enum(id);
                            }
                        } else {
                            // Names an unbound type parameter: resolving it
                            // by name could hit a same-named type.
                            return HirTy::Path(vec![full]);
                        }
                    }
                }
                let mut t = resolve_ty_with_structs(ty, &self.env.defs, None, scope);
                substitute_params(&mut t, &self.ty_subst);
                t
            }
            AstTyKind::Sum(parts) => crate::ty::written_sum(
                parts,
                |n| self.ty_subst.contains_key(n) || self.assoc_names.iter().any(|a| a == n),
                rec,
            ),
            AstTyKind::Nest(x) => rec(x),
            AstTyKind::Tuple(parts) if parts.is_empty() => HirTy::Unit,
            AstTyKind::Tuple(parts) => HirTy::Tuple(parts.iter().map(rec).collect()),
            AstTyKind::Array(elem, len) => {
                if let rokc_ast::ConstExpr::Literal(_) = len {
                    return match resolve_ty_with_structs(ty, &self.env.defs, None, scope) {
                        HirTy::Array(_, n) => HirTy::Array(Box::new(rec(elem)), n),
                        other => other,
                    };
                }
                self.array_len(len)
                    .map_or(HirTy::Unknown, |n| HirTy::Array(Box::new(rec(elem)), n))
            }
            AstTyKind::Const(c) => match self.eval_const_arg(c) {
                Ok((v, p)) => HirTy::Const(v, p),
                Err(why) => {
                    self.emit_error(
                        rokc_diagnostic::ErrorKind::Type,
                        format!(
                            "a const generic argument must be a compile-time integer, \
                                 `bool` or `char` (spec 2.11.4): {why}"
                        ),
                    );
                    HirTy::Unknown
                }
            },
            AstTyKind::Fn(args, ret, b) => HirTy::Fn(
                args.iter().map(rec).collect(),
                Box::new(rec(ret)),
                self.env.names.fn_bounds(b, module, self.here()),
            ),
            AstTyKind::Throws { ok, err } => crate::ty::throws_ty(rec(ok), rec(err)),
            AstTyKind::QPath(q) => self.qpath_ty(q),
            _ => resolve_ty_with_structs(ty, &self.env.defs, None, scope),
        }
    }

    /// `<T as Tr>::X` (spec 2.13.4): only `Tr`'s `X`.
    fn qpath_ty(&self, q: &rokc_ast::QPath) -> HirTy {
        let base = self.resolve_ty_inner(&q.self_ty);
        let tp = &q.trait_path;
        let segs: Vec<&str> = tp.segments.iter().map(|s| s.name.as_str()).collect();
        let key = self
            .env
            .names
            .trait_key(&segs, &self.current_module, self.here());
        if !self.env.names.is_trait(&key) {
            self.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "`{}` in `<.. as {}>` is not a trait",
                    segs.join("::"),
                    segs.join("::")
                ),
            );
            return HirTy::Unknown;
        }
        let want = resolve::trait_ref(
            &self.env.names,
            tp,
            &self.current_module,
            self.here(),
            |t| self.resolve_ty_inner(t),
        );
        self.project_or_report(&base, &q.item, Some(&[key]), &[want], true)
    }

    /// `T::Item` / `List<i32>::IntoIter` (spec 2.1, 2.13), resolved once the
    /// prefix is concrete; an unbound parameter prefix stays opaque
    /// (`J::Item`). `None`: not a projection.
    fn projection(&self, p: &AstPath) -> Option<HirTy> {
        let (last, prefix) = p.segments.split_last()?;
        if prefix.is_empty() || !last.generics.is_empty() {
            return None;
        }
        let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
        if self.canon_type(&segs).is_some() {
            return None;
        }
        let pre: Vec<&str> = prefix.iter().map(|s| s.name.as_str()).collect();
        let base = match pre.as_slice() {
            ["Self"] => self.self_ty.clone()?,
            [n] if self.ty_subst.contains_key(*n) && prefix[0].generics.is_empty() => {
                self.ty_subst[*n].clone()
            }
            _ => {
                let ty = Ty {
                    kind: AstTyKind::Path(AstPath {
                        segments: prefix.to_vec(),
                        span: p.span,
                        origin: None,
                    }),
                    span: p.span,
                };
                let is_prim = matches!(crate::ty::resolve_ty(&ty), HirTy::Prim(_));
                if !is_prim && self.canon_type(&pre).is_none() {
                    return None;
                }
                self.resolve_ty_inner(&ty)
            }
        };
        let mut open = Vec::new();
        collect_unknown_paths(&base, &mut open);
        if !open.is_empty() {
            let mut path: Vec<String> = pre.iter().map(|s| (*s).to_owned()).collect();
            path.push(last.name.clone());
            return Some(HirTy::Path(path));
        }
        // A type parameter's projection goes through its bounds (and
        // their supertraits) first.
        let via = match pre.as_slice() {
            [n] => self.ty_bounds.get(*n).cloned(),
            _ => None,
        };
        let with_args = match pre.as_slice() {
            [n] => self.bound_wants(n),
            _ => Vec::new(),
        };
        Some(self.project_or_report(&base, &last.name, via.as_deref(), &with_args, false))
    }

    /// The bounds written on type parameter `name`, as trait refs: what
    /// its `T::X` / `T::N` must come from.
    fn bound_wants(&self, name: &str) -> Vec<TraitRef> {
        self.ty_bound_args
            .get(name)
            .into_iter()
            .flatten()
            .map(|p| {
                resolve::trait_ref(&self.env.names, p, &self.current_module, self.here(), |t| {
                    self.resolve_ty_inner(t)
                })
            })
            .collect()
    }

    /// `project`, reporting a miss. `strict`: only `traits`; else they are
    /// preferred over any impl. `args`: required leading trait arguments.
    fn project_or_report(
        &self,
        base: &HirTy,
        name: &str,
        traits: Option<&[String]>,
        args: &[TraitRef],
        strict: bool,
    ) -> HirTy {
        let hit = match traits {
            Some(t) if strict => self.project(base, name, Some(t), args),
            Some(t) => self
                .project(base, name, Some(t), args)
                .or_else(|| self.project(base, name, None, &[])),
            None => self.project(base, name, None, &[]),
        };
        hit.unwrap_or_else(|| {
            let via = match traits {
                Some([t]) if strict => format!(" of `{t}`"),
                _ => String::new(),
            };
            self.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "`{}` has no associated type `{name}`{via} (no trait impl of it defines one)",
                    self.ty_name(base)
                ),
            );
            HirTy::Unknown
        })
    }

    /// Associated type `name` of concrete type `base`, from its trait
    /// impls (concrete or generic) - of `traits` only, when given.
    fn project(
        &self,
        base: &HirTy,
        name: &str,
        traits: Option<&[String]>,
        args: &[TraitRef],
    ) -> Option<HirTy> {
        if let HirTy::Param(p) = base {
            return bounds::project(self, *p, name, traits, args);
        }
        let key = (
            base.clone(),
            name.to_owned(),
            traits.map(<[String]>::to_vec),
        );
        if self.env.sink.projecting.borrow().contains(&key) {
            // A cycle: this candidate adds nothing (filtered as unknown).
            return Some(HirTy::Unknown);
        }
        let wanted = |t: &str| traits.is_none_or(|ts| ts.iter().any(|x| x == t));
        // An impl's trait arguments against a bound's (`TryFrom<i64>`);
        // an argument not yet concrete matches anything.
        let args_fit = |t: &str, have: &[HirTy]| {
            args.iter().filter(|w| w.path == t).all(|want| {
                want.args.iter().zip(have).all(|(w, h)| {
                    let mut open = Vec::new();
                    collect_unknown_paths(w, &mut open);
                    collect_unknown_paths(h, &mut open);
                    !open.is_empty() || w == h || *w == HirTy::Unknown || *h == HirTy::Unknown
                })
            })
        };
        self.env.sink.projecting.borrow_mut().push(key);
        let mut hits: Vec<HirTy> = self
            .concrete_impls(base)
            .filter(|r| wanted(&r.path) && args_fit(&r.path, &r.args))
            .filter_map(|r| r.binds.iter().find(|(n, _)| n == name))
            .map(|(_, t)| t.clone())
            .collect();
        let keep = |gi: &Impl| wanted(&gi.trait_key) && gi.assoc.iter().any(|(n, _)| n == name);
        let generic = self.generic_impls(base, Pick::Where(&keep), 0);
        for (gi, subst) in generic {
            let Some((_, ty)) = gi.assoc.iter().find(|(n, _)| n == name) else {
                continue;
            };
            let mut rcx = LowerCtx::new(self.env, &subst, None, self.here());
            rcx.current_module.clone_from(&gi.module);
            rcx.set_bounds(&gi.generics, &gi.where_clause);
            let have = resolve::trait_args(&gi.trait_args, |t| rcx.resolve_ast_ty(t)).0;
            if args_fit(&gi.trait_key, &have) {
                hits.push(rcx.resolve_ast_ty(ty));
            }
        }
        self.env.sink.projecting.borrow_mut().pop();
        hits.retain(|t| *t != HirTy::Unknown);
        let mut uniq: Vec<HirTy> = Vec::new();
        for h in hits {
            if !uniq.contains(&h) {
                uniq.push(h);
            }
        }
        if uniq.len() > 1 {
            let names: Vec<String> = uniq
                .iter()
                .map(|t| format!("`{}`", self.ty_name(t)))
                .collect();
            self.emit_error(
                rokc_diagnostic::ErrorKind::Type,
                format!(
                    "associated type `{}::{name}` is ambiguous: impls define it as {} - name \
                     the trait (`<T as Tr>::{name}`, spec 2.13.4)",
                    self.ty_name(base),
                    names.join(", ")
                ),
            );
        }
        uniq.into_iter().next()
    }

    /// Record the trait bounds (with their supertraits) of the type
    /// parameters `generics` / `where_clause` declare, for projections.
    fn set_bounds(&mut self, generics: &rokc_ast::Generics, where_clause: &[rokc_ast::WherePred]) {
        let mut add = |name: &str, bounds: &[Ty]| {
            for b in bounds {
                let AstTyKind::Path(p) = &b.kind else {
                    continue;
                };
                let segs: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
                let key = self
                    .env
                    .names
                    .trait_key(&segs, &self.current_module, self.here());
                self.ty_bound_args
                    .entry(name.to_owned())
                    .or_default()
                    .push(p.clone());
                let mut stack = vec![key];
                while let Some(k) = stack.pop() {
                    let entry = self.ty_bounds.entry(name.to_owned()).or_default();
                    if entry.contains(&k) {
                        continue;
                    }
                    entry.push(k.clone());
                    if let Some(info) = self.env.trait_of(&k) {
                        stack.extend(info.supers.iter().cloned());
                    }
                }
            }
        };
        for gp in &generics.params {
            if let rokc_ast::GenericParam::Type { name, bounds, .. } = gp {
                add(name, bounds);
            }
        }
        for w in where_clause {
            if let AstTyKind::Path(p) = &w.ty.kind {
                if let [seg] = p.segments.as_slice() {
                    add(&seg.name, &w.bounds);
                }
            }
        }
    }

    /// The value type of variable `id` (a `[ref]`-captured cell's
    /// element type).
    fn var_ty(&self, id: LocalId) -> HirTy {
        match self.local_ty(id) {
            HirTy::GcBuf(t) if self.is_boxed(id) => *t,
            t => t,
        }
    }

    /// Read variable `id` (through its cell when `[ref]`-captured).
    fn read_local(&self, id: LocalId, span: rokc_diagnostic::Span) -> HirExpr {
        if let Some(view) = self.cell_views.get(&id) {
            return view.clone();
        }
        let ty = self.local_ty(id);
        let HirTy::GcBuf(elem) = &ty else {
            return hir_local(id, &ty, span);
        };
        if !self.is_boxed(id) {
            return hir_local(id, &ty, span);
        }
        HirExpr {
            kind: HirExprKind::BufGet {
                buf: Box::new(hir_local(id, &ty, span)),
                index: Box::new(hir_int(0, PrimTy::Usize, span)),
                elem: (**elem).clone(),
            },
            ty: (**elem).clone(),
            span,
        }
    }

    /// `id = value` (into its cell when `[ref]`-captured).
    fn store_local(&self, id: LocalId, value: HirExpr) -> HirStmt {
        if !self.is_boxed(id) {
            return HirStmt::Assign { local: id, value };
        }
        let span = value.span;
        HirStmt::Expr(buf_store(hir_local(id, &self.local_ty(id), span), value))
    }

    /// Declare `let name: ty = init` (the binding declared at `at`): a
    /// plain local, or a fresh cell for a `[ref]`-captured binding.
    fn declare_var(
        &mut self,
        name: &str,
        ty: HirTy,
        init: HirExpr,
        at: rokc_diagnostic::Span,
    ) -> HirStmt {
        if !self.ref_captured.contains(&(name.to_owned(), at)) {
            let local = self.declare(Some(name.to_owned()), ty);
            return HirStmt::Let { local, init };
        }
        self.cell_let(name, ty, init)
    }

    /// Track `local` if it binds a lock guard (`guard_lets`).
    fn track_guard(&mut self, local: LocalId, ty: &HirTy, at: Span, name: &str) {
        if let Some(g) = lock_guard(self, ty) {
            let depth = self.scopes.len();
            self.guard_lets.push((local, depth, at, name.to_owned(), g));
        }
    }

    fn declare_bound(&mut self, name: &str, ty: HirTy, at: rokc_diagnostic::Span) -> LocalId {
        if let Some(&local) = self.or_pattern_binders.last().and_then(|m| m.get(name)) {
            let existing = self.local_ty(local);
            if matches!(existing, HirTy::Unknown) {
                self.locals[local.0 as usize].ty = ty;
            } else if existing != ty {
                self.emit_error(
                    rokc_diagnostic::ErrorKind::Type,
                    format!(
                        "or-pattern binding `{name}` has inconsistent types across \
                         alternatives (`{}` and `{}`, spec 3.11.4); to bind either as \
                         their sum, write one type pattern: `{name}: {} | {}`",
                        self.ty_name(&existing),
                        self.ty_name(&ty),
                        self.ty_name(&existing),
                        self.ty_name(&ty),
                    ),
                );
            }
            return local;
        }
        let local = self.declare(Some(name.to_owned()), ty.clone());
        self.track_guard(local, &ty, at, name);
        if self.ref_captured.contains(&(name.to_owned(), at)) {
            self.pending_cells.push((name.to_owned(), local, ty));
        }
        local
    }

    /// `let name = cell(name)` for the bindings since the last call, run
    /// before their scope's code.
    fn take_cells(&mut self, span: rokc_diagnostic::Span) -> Vec<HirStmt> {
        let mut out = self.take_resums(span);
        let cells: Vec<HirStmt> = std::mem::take(&mut self.pending_cells)
            .into_iter()
            .map(|(name, l, ty)| {
                let init = hir_local(l, &ty, span);
                self.cell_let(&name, ty, init)
            })
            .collect();
        out.extend(cells);
        out
    }

    /// `binder = resum(whole)` for the sub-sum type patterns since the
    /// last call.
    fn take_resums(&mut self, span: rokc_diagnostic::Span) -> Vec<HirStmt> {
        std::mem::take(&mut self.pending_resums)
            .into_iter()
            .map(|(binder, whole)| {
                let (to, from) = (self.local_ty(binder), self.local_ty(whole));
                let init = resum(self, hir_local(whole, &from, span), &to);
                HirStmt::Let {
                    local: binder,
                    init,
                }
            })
            .collect()
    }

    /// `let name = <new cell holding init>`.
    fn cell_let(&mut self, name: &str, ty: HirTy, init: HirExpr) -> HirStmt {
        let span = init.span;
        let cell_ty = HirTy::GcBuf(Box::new(ty.clone()));
        let tmp = self.declare(None, cell_ty.clone());
        let local = self.declare(Some(name.to_owned()), cell_ty.clone());
        self.locals[local.0 as usize].boxed = true;
        let cell = HirExprKind::BufNew {
            elem: ty,
            len: Box::new(hir_int(1, PrimTy::Usize, span)),
        };
        HirStmt::Let {
            local,
            init: HirExpr {
                kind: HirExprKind::Block(Box::new(HirBlock {
                    stmts: vec![
                        HirStmt::Let {
                            local: tmp,
                            init: HirExpr {
                                kind: cell,
                                ty: cell_ty.clone(),
                                span,
                            },
                        },
                        HirStmt::Expr(buf_store(hir_local(tmp, &cell_ty, span), init)),
                    ],
                    trailing: Some(hir_local(tmp, &cell_ty, span)),
                    label: None,
                })),
                ty: cell_ty,
                span,
            },
        }
    }

    /// Signature of function `id` (a method's, whatever its key).
    fn sig_by_id(&self, id: FunctionId) -> FnSig {
        self.sig_of_key(self.env.fn_key(id).expect("a declaration"))
    }

    /// `fn_table[key]`, with a return type inferred from the body when
    /// the declaration omits it (spec 4.1.3).
    fn sig_of_key(&self, key: &str) -> FnSig {
        let mut sig = self.env.sig(key).expect("a signature").clone();
        if self.env.infers_ret(sig.id) {
            sig.ret_ty = mono::decl_ret(self, sig.id, sig.ret_ty);
        }
        sig
    }

    fn into_locals(self) -> Vec<HirLocal> {
        self.locals
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.local_uses.push(Vec::new());
        self.nested_fns.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        let depth = self.scopes.len();
        let (open, kept) = std::mem::take(&mut self.guard_lets)
            .into_iter()
            .partition(|g| g.1 >= depth);
        self.guard_lets = kept;
        for (_, _, at, binder, guard) in open {
            self.emit_error_at(
                rokc_diagnostic::ErrorKind::Type,
                at,
                format!(
                    "`{binder}` holds a `{guard}` that is never released: only `using` \
                     releases a lock guard, so bind it with `using {binder} = .. {{ .. }}` \
                     (stdlib 2.16.3)"
                ),
            );
        }
        self.scopes.pop();
        self.local_uses.pop();
        self.nested_fns.pop();
    }

    fn add_local_use(&mut self, entry: crate::uses::UseEntry) {
        if let Some(top) = self.local_uses.last_mut() {
            top.push(entry);
        }
    }

    fn declare(&mut self, name: Option<String>, ty: HirTy) -> LocalId {
        let id = LocalId(u32::try_from(self.locals.len()).expect("LocalId overflow"));
        if let Some(n) = &name {
            if let Some(scope) = self.scopes.last_mut() {
                scope.insert(n.clone(), id);
            }
        }
        self.locals.push(HirLocal {
            id,
            name,
            ty,
            boxed: false,
        });
        id
    }

    /// Whether `id` is a `[ref]`-captured variable's cell.
    fn is_boxed(&self, id: LocalId) -> bool {
        self.locals[id.0 as usize].boxed
    }

    fn lookup(&self, name: &str) -> Option<LocalId> {
        for scope in self.scopes.iter().rev() {
            if let Some(id) = scope.get(name) {
                return Some(*id);
            }
        }
        None
    }

    /// Look up a name only in the innermost scope, for the `let`
    /// redeclaration check: nested scopes can rebind, the same one cannot.
    fn lookup_in_current_scope(&self, name: &str) -> Option<LocalId> {
        self.scopes.last().and_then(|s| s.get(name).copied())
    }

    fn local_ty(&self, id: LocalId) -> HirTy {
        self.locals
            .get(id.0 as usize)
            .map_or(HirTy::Unknown, |l| l.ty.clone())
    }
}

/// Primitive type names, for "did you mean" hints.
const PRIM_NAMES: &[&str] = &[
    "i8", "i16", "i32", "i64", "isize", "u8", "u16", "u32", "u64", "usize", "f32", "f64", "bool",
    "char", "str", "int", "float",
];

/// The 64-bit type to suggest for a 128-bit integer name, which rok
/// does not have.
fn wide_int_narrow(name: &str) -> Option<&'static str> {
    match name {
        "i128" => Some("i64"),
        "u128" => Some("u64"),
        _ => None,
    }
}

/// Help for a 128-bit integer type name.
fn wide_int_help(name: &str) -> Option<String> {
    let narrow = wide_int_narrow(name)?;
    Some(format!(
        "rok has no `{name}`; use `int` for integers of any size, or `{narrow}`"
    ))
}

/// Placeholder for a failed expression (already reported). Never reaches
/// MIR: the driver stops on errors and `check_unresolved_types` refuses
/// `Unknown`.
const fn poison(span: rokc_diagnostic::Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Const(ConstVal::Unit),
        ty: HirTy::Unknown,
        span,
    }
}

/// `cell[0] = value` on a one-element `GcBuf` cell.
fn buf_store(cell: HirExpr, value: HirExpr) -> HirExpr {
    let span = value.span;
    let elem = match &cell.ty {
        HirTy::GcBuf(t) => (**t).clone(),
        _ => value.ty.clone(),
    };
    HirExpr {
        kind: HirExprKind::BufSet {
            buf: Box::new(cell),
            index: Box::new(hir_int(0, PrimTy::Usize, span)),
            value: Box::new(value),
            elem,
        },
        ty: HirTy::Unit,
        span,
    }
}

/// Bindings captured as `[ref x]` by closures in `body` (nested ones too);
/// each names the innermost `x` in scope.
fn ref_captured_bindings(
    params: &[Param],
    body: &Block,
) -> std::collections::HashSet<(String, rokc_diagnostic::Span)> {
    type Binding = (String, rokc_diagnostic::Span);
    struct V {
        scopes: Vec<Vec<Binding>>,
        out: std::collections::HashSet<Binding>,
    }
    impl crate::walk::AstVisitor for V {
        fn expr(&mut self, e: &Expr) {
            let ExprKind::Closure { captures, .. } = &e.kind else {
                return;
            };
            for c in captures {
                let rokc_ast::CaptureSpec::Ref(n) = c else {
                    continue;
                };
                let found = self
                    .scopes
                    .iter()
                    .rev()
                    .flat_map(|s| s.iter().rev())
                    .find(|(b, _)| b == n);
                if let Some(b) = found {
                    self.out.insert(b.clone());
                }
            }
        }
        fn bind_at(&mut self, name: &str, at: rokc_diagnostic::Span) {
            if let Some(s) = self.scopes.last_mut() {
                s.push((name.to_owned(), at));
            }
        }
        fn push_scope(&mut self) {
            self.scopes.push(Vec::new());
        }
        fn pop_scope(&mut self) {
            self.scopes.pop();
        }
    }
    let mut v = V {
        scopes: vec![params.iter().map(|p| (p.name.clone(), p.span)).collect()],
        out: std::collections::HashSet::new(),
    };
    crate::walk::walk_block(&mut v, body);
    v.out
}
