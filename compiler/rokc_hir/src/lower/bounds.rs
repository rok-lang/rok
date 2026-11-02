//! Generic bodies checked at their definition.
//!
//! Each generic body (fn, impl or trait method, trait default) is lowered
//! once with its type parameters (and a default's `Self`) as opaque
//! `HirTy::Param`s that have only what their bounds give: supertraits,
//! associated types, `where` clauses. Methods resolve through the bound
//! traits (`bound_methods`), operators and `has_impl` through the bound
//! list; instances over a `Param` are signatures only
//! (`mono::var_instance`). Everything the check lowered is then undone and
//! only its errors stay; an incremental build reuses them
//! (`incr::Incr::checked`).
//!
//! Monomorphization stays the code path; an instance of a template that
//! failed its check reports nothing more (`failed`). Implicit type
//! parameters are checked per instance only. A `const`
//! parameter gets a distinct prime past the unrolled array lengths, so
//! `[T; N]` and `[T; M]` differ but `[T; M * N]` and `[T; N * M]` agree;
//! diagnostics show its name.

#[allow(clippy::wildcard_imports)]
use super::*;
use rokc_diagnostic::Code;

/// One type parameter (or projection) being checked.
#[derive(Clone)]
pub(super) struct ParamInfo {
    pub(super) name: String,
    bounds: Vec<TraitRef>,
    /// `bounds` with every supertrait, once asked for.
    all: Option<Vec<TraitRef>>,
    /// Associated types projected so far: (bound, name) → type.
    projs: Vec<(TraitRef, String, HirTy)>,
    /// A bound names no trait (reported where it is written): method
    /// lookups through the parameter stay quiet.
    unknown: bool,
}

fn fresh(env: &Env<'_>, name: String) -> HirTy {
    let mut ps = env.sink.params.borrow_mut();
    ps.push(ParamInfo {
        name,
        bounds: Vec::new(),
        all: None,
        projs: Vec::new(),
        unknown: false,
    });
    HirTy::Param(u32::try_from(ps.len() - 1).expect("type parameter overflow"))
}

fn add_bound(env: &Env<'_>, p: u32, r: TraitRef) {
    let mut ps = env.sink.params.borrow_mut();
    let info = &mut ps[p as usize];
    if !info.bounds.contains(&r) {
        info.bounds.push(r);
        info.all = None;
    }
}

/// Does `t` mention a `Param`?
pub(super) fn has_param(ctx: &LowerCtx<'_>, t: &HirTy) -> bool {
    mentions(ctx, t, &|t| matches!(t, HirTy::Param(_)))
}

/// Does `t` mention a type `hit` picks?
pub(super) fn mentions(ctx: &LowerCtx<'_>, t: &HirTy, hit: &dyn Fn(&HirTy) -> bool) -> bool {
    let rec = |t: &HirTy| mentions(ctx, t, hit);
    match t {
        _ if hit(t) => true,
        HirTy::Struct(id) => ctx.struct_def(*id).generic_args.iter().any(rec),
        HirTy::Enum(id) => ctx.enum_def(*id).generic_args.iter().any(rec),
        HirTy::Sum(ps) | HirTy::Tuple(ps) => ps.iter().any(rec),
        HirTy::Array(e, _) | HirTy::GcBuf(e) => rec(e),
        HirTy::Fn(a, r, _) => a.iter().any(rec) || rec(r),
        HirTy::Closure { args, ret, env, .. } => args.iter().any(rec) || rec(ret) || rec(env),
        HirTy::Throws { ok, err } => rec(ok) || rec(err),
        HirTy::AnyTrait(bs) => bs
            .iter()
            .any(|b| b.args.iter().chain(b.binds.iter().map(|(_, t)| t)).any(rec)),
        _ => false,
    }
}

/// A context resolving names inside trait `info`'s declaration, its
/// parameters bound to `r`'s arguments and `Self` to `self_ty`.
fn in_trait<'a>(
    ctx: &LowerCtx<'a>,
    info: &TraitInfo,
    r: &TraitRef,
    self_ty: &HirTy,
    subst: &'a mut TySubst,
) -> LowerCtx<'a> {
    let names = info.generics.params.iter().map(|gp| match gp {
        rokc_ast::GenericParam::Type { name, .. } | rokc_ast::GenericParam::Const { name, .. } => {
            name.clone()
        }
    });
    subst.extend(names.zip(r.args.iter().cloned()));
    subst.extend(r.binds.iter().cloned());
    let subst: &'a TySubst = subst;
    let mut rcx = LowerCtx::new(ctx.env, subst, Some(self_ty.clone()), ctx.here());
    rcx.current_module.clone_from(&info.module);
    // A bare associated type is `Self::X`.
    rcx.assoc_names = info.assoc.iter().map(|(n, _)| n.clone()).collect();
    rcx
}

/// `r` (a bound on `self_ty`) with the trait's omitted arguments taken
/// from its parameter defaults (`Add` is `Add<Self>`).
fn complete(ctx: &LowerCtx<'_>, mut r: TraitRef, self_ty: &HirTy) -> TraitRef {
    let Some(info) = ctx.env.trait_of(&r.path) else {
        return r;
    };
    for gp in info.generics.params.iter().skip(r.args.len()) {
        let t = match gp {
            rokc_ast::GenericParam::Type {
                default: Some(d), ..
            } => {
                let mut subst = TySubst::new();
                in_trait(ctx, info, &r, self_ty, &mut subst).resolve_ast_ty(d)
            }
            // Left open by the bound (`S: Source<Item = i32>` of
            // `Source<K>`): some type, unknown.
            rokc_ast::GenericParam::Type { name, .. } => fresh(ctx.env, name.clone()),
            rokc_ast::GenericParam::Const { .. } => break,
        };
        r.args.push(t);
    }
    r
}

/// Bound `b` on `self_ty` as written in `module` (resolved by `rcx`).
fn bound_on(rcx: &LowerCtx<'_>, b: &Ty, module: &str, self_ty: &HirTy) -> Option<TraitRef> {
    let r = rcx.bound_ref(b, module, |t| rcx.resolve_ast_ty(t))?;
    rcx.env
        .names
        .is_trait(&r.path)
        .then(|| complete(rcx, r, self_ty))
}

/// `p`'s bounds and all their supertraits.
pub(super) fn traits_of(ctx: &LowerCtx<'_>, p: u32) -> Vec<TraitRef> {
    if let Some(all) = &ctx.env.sink.params.borrow()[p as usize].all {
        return all.clone();
    }
    let me = HirTy::Param(p);
    let mut out: Vec<TraitRef> = Vec::new();
    let mut stack: Vec<TraitRef> = ctx.env.sink.params.borrow()[p as usize]
        .bounds
        .iter()
        .rev()
        .cloned()
        .collect();
    while let Some(r) = stack.pop() {
        if out.contains(&r) {
            continue;
        }
        if let Some(info) = ctx.env.trait_of(&r.path) {
            let mut subst = TySubst::new();
            let rcx = in_trait(ctx, info, &r, &me, &mut subst);
            for st in info.super_tys.iter().rev() {
                if let Some(s) = bound_on(&rcx, st, &info.module, &me) {
                    stack.push(s);
                }
            }
        }
        out.push(r);
    }
    ctx.env.sink.params.borrow_mut()[p as usize].all = Some(out.clone());
    out
}

/// Does `p` meet `want` through its bounds?
pub(super) fn has(ctx: &LowerCtx<'_>, p: u32, want: &TraitRef) -> bool {
    traits_of(ctx, p).iter().any(|r| r.satisfies(want))
}

/// The bounds of `p` (with supertraits) on trait `key`: several when a
/// bound names the trait twice (`T: From<A> + From<B>`).
pub(super) fn of_trait(ctx: &LowerCtx<'_>, p: u32, key: &str) -> Vec<TraitRef> {
    traits_of(ctx, p)
        .into_iter()
        .filter(|r| r.path == key)
        .collect()
}

/// Is bound `r` one of `wants` (if any)? Non-concrete arguments match.
fn fits_wants(r: &TraitRef, wants: &[TraitRef]) -> bool {
    let mut of = wants.iter().filter(|w| w.path == r.path).peekable();
    of.peek().is_none()
        || of.any(|w| {
            w.args.iter().zip(&r.args).all(|(w, h)| {
                let mut open = Vec::new();
                collect_unknown_paths(w, &mut open);
                !open.is_empty() || *w == HirTy::Unknown || w == h
            })
        })
}

/// The one bound of `p` that associated item `name` comes from (of
/// `traits`, agreeing with `wants`, accepted by `has`). Two left
/// (`T: Add<i32> + Add<f64>` for `T::Output`) is ambiguous.
fn item_bound(
    ctx: &LowerCtx<'_>,
    p: u32,
    name: &str,
    traits: Option<&[String]>,
    wants: &[TraitRef],
    has: impl Fn(&TraitRef) -> bool,
) -> Option<Result<TraitRef, ()>> {
    // One trait with the same arguments is one bound, its bindings
    // merged (`I: Iterator<Item = i32>` and a supertrait's `Iterator`).
    let mut cands: Vec<TraitRef> = Vec::new();
    for r in traits_of(ctx, p) {
        if !traits.is_none_or(|ts| ts.contains(&r.path)) || !fits_wants(&r, wants) {
            continue;
        }
        match cands
            .iter_mut()
            .find(|c| c.path == r.path && c.args == r.args)
        {
            Some(c) => {
                for b in r.binds {
                    if !c.binds.iter().any(|(n, _)| *n == b.0) {
                        c.binds.push(b);
                    }
                }
            }
            None => cands.push(r),
        }
    }
    cands.retain(|r| has(r));
    match cands.as_slice() {
        [] => None,
        [r] => Some(Ok(r.clone())),
        [a, b, ..] => {
            let t = ctx.ty_name(&HirTy::Param(p));
            let show = |r: &TraitRef| r.render(leaf_name(&r.path), |t| ctx.ty_name(t));
            ctx.emit_error(
                Code::AMBIGUOUS_ITEM,
                format!(
                    "`{t}::{name}` is ambiguous: the bounds `{}` and `{}` of `{t}` both give it \
                     - name the bound (`<{t} as {}>::{name}`)",
                    show(a),
                    show(b),
                    show(a)
                ),
            );
            Some(Err(()))
        }
    }
}

/// `T::name` as shown: `<T as Tr<A>>::name` when `p` has `r`'s trait
/// more than once.
fn item_shown(ctx: &LowerCtx<'_>, p: u32, r: &TraitRef, name: &str) -> String {
    let t = ctx.env.sink.params.borrow()[p as usize].name.clone();
    if of_trait(ctx, p, &r.path).len() > 1 {
        let tr = r.render(leaf_name(&r.path), |t| ctx.ty_name(t));
        return format!("<{t} as {tr}>::{name}");
    }
    format!("{t}::{name}")
}

/// Associated type `name` of `p`: the binding a bound gives
/// (`I: Iterator<Item = i32>`), else an opaque `I::Item` bounded as the
/// trait and `where` clauses say.
pub(super) fn project(
    ctx: &LowerCtx<'_>,
    p: u32,
    name: &str,
    traits: Option<&[String]>,
    wants: &[TraitRef],
) -> Option<HirTy> {
    let has = |r: &TraitRef| {
        r.binds.iter().any(|(n, _)| n == name)
            || ctx
                .env
                .trait_of(&r.path)
                .is_some_and(|i| i.assoc.iter().any(|(n, _)| n == name))
    };
    let Ok(r) = item_bound(ctx, p, name, traits, wants, has)? else {
        return Some(HirTy::Unknown);
    };
    if let Some((_, t)) = r.binds.iter().find(|(n, _)| n == name) {
        return Some(t.clone());
    }
    if let Some((.., t)) = ctx.env.sink.params.borrow()[p as usize]
        .projs
        .iter()
        .find(|(b, n, _)| *b == r && n == name)
    {
        return Some(t.clone());
    }
    let info = ctx.env.trait_of(&r.path).expect("a trait");
    let decl = &info.assoc.iter().find(|(n, _)| n == name)?.1;
    let t = fresh(ctx.env, item_shown(ctx, p, &r, name));
    ctx.env.sink.params.borrow_mut()[p as usize].projs.push((
        r.clone(),
        name.to_owned(),
        t.clone(),
    ));
    let HirTy::Param(q) = t else {
        unreachable!("fresh makes a Param")
    };
    let mut subst = TySubst::new();
    let rcx = in_trait(ctx, info, &r, &HirTy::Param(p), &mut subst);
    for b in decl {
        if let Some(b) = bound_on(&rcx, b, &info.module, &t) {
            add_bound(ctx.env, q, b);
        }
    }
    Some(t)
}

/// The templates calling method `method` of `p`'s bound traits (of
/// trait `only` when given) goes through, one per trait providing it.
pub(super) fn bound_methods<'e>(
    ctx: &LowerCtx<'e>,
    p: u32,
    method: &str,
    only: Option<&str>,
) -> Vec<&'e GenericMethod> {
    let env: &'e Env<'e> = ctx.env;
    let mut out: Vec<&'e GenericMethod> = Vec::new();
    for r in traits_of(ctx, p) {
        if only.is_some_and(|k| k != r.path) {
            continue;
        }
        if let Some(gm) = env.bound_method(&r.path, method) {
            if !out.iter().any(|g| std::ptr::eq(*g, gm)) {
                out.push(gm);
            }
        }
    }
    out
}

/// `bind_self` for a bound template: the trait's parameters take `p`'s
/// bound arguments (only those shared, with several bounds of the trait);
/// the call binds the rest and `pick_bound` checks them.
pub(super) fn bind_trait_args(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    p: u32,
    subst: &mut HashMap<String, HirTy>,
) {
    let Some(key) = ctx.env.bound_trait(&fn_def.name) else {
        return;
    };
    let cands = of_trait(ctx, p, key);
    let Some(first) = cands.first() else {
        return;
    };
    let info = ctx.env.trait_of(key).expect("a trait");
    for (k, (gp, a)) in info.generics.params.iter().zip(&first.args).enumerate() {
        let (rokc_ast::GenericParam::Type { name, .. }
        | rokc_ast::GenericParam::Const { name, .. }) = gp;
        if cands.iter().all(|r| r.args.get(k) == Some(a)) {
            subst.entry(name.clone()).or_insert_with(|| a.clone());
        }
    }
}

/// With `p` bounded by the trait more than once, the bound a call goes
/// through: the one whose arguments the call bound. None or
/// several is an error.
pub(super) fn pick_bound(
    ctx: &LowerCtx<'_>,
    fn_def: &FnDef,
    p: u32,
    subst: &mut HashMap<String, HirTy>,
) -> Result<(), String> {
    let Some(key) = ctx.env.bound_trait(&fn_def.name) else {
        return Ok(());
    };
    let info = ctx.env.trait_of(key).expect("a trait");
    let names: Vec<&String> = info
        .generics
        .params
        .iter()
        .map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. }
            | rokc_ast::GenericParam::Const { name, .. } => name,
        })
        .collect();
    // Bounds that differ past the trait's parameters (`Into<A>` of the
    // parameterless `Into`) are one for its methods.
    let mut cands: Vec<TraitRef> = Vec::new();
    for r in of_trait(ctx, p, key) {
        let n = names.len().min(r.args.len());
        if !cands.iter().any(|c| c.args.get(..n) == r.args.get(..n)) {
            cands.push(r);
        }
    }
    if cands.len() < 2 {
        return Ok(());
    }
    let have: Vec<Option<HirTy>> = names.iter().map(|n| subst.get(*n).cloned()).collect();
    // Inference variables settle later; the instance is a signature.
    if have.iter().flatten().any(|t| super::infer::has_var(ctx, t)) {
        return Ok(());
    }
    let fits: Vec<&TraitRef> = cands
        .iter()
        .filter(|r| {
            r.args
                .iter()
                .zip(&have)
                .all(|(a, h)| h.as_ref().is_none_or(|h| h == a))
        })
        .collect();
    let t = ctx.ty_name(&HirTy::Param(p));
    let show = |r: &TraitRef| format!("`{}`", r.render(leaf_name(key), |t| ctx.ty_name(t)));
    let all: Vec<String> = cands.iter().map(show).collect();
    let method = fn_def.name.rsplit("__").next().unwrap_or_default();
    match fits.as_slice() {
        [r] => {
            for (n, a) in names.iter().zip(&r.args) {
                subst.entry((*n).clone()).or_insert_with(|| a.clone());
            }
            Ok(())
        }
        [] => {
            let asked: Vec<String> = have
                .iter()
                .map(|h| {
                    h.as_ref()
                        .map_or_else(|| "_".to_owned(), |t| ctx.ty_name(t))
                })
                .collect();
            Err(format!(
                "`{method}` on `{t}` needs a bound `{}<{}>`, but `{t}` has {}",
                leaf_name(key),
                asked.join(", "),
                all.join(" and ")
            ))
        }
        _ => Err(format!(
            "`{method}` on `{t}` is ambiguous: the arguments do not choose between its \
             bounds {} - name one (`<{t} as {}>::{method}(..)`)",
            all.join(" and "),
            cands[0].render(leaf_name(key), |t| ctx.ty_name(t))
        )),
    }
}

/// The types parameter `i` takes under each of `p`'s bounds when it is a
/// bare trait parameter they give differently (`From<T>::from` under
/// `From<i64> + From<str>`): an unsuffixed literal's choices.
pub(super) fn slot_choices(ctx: &LowerCtx<'_>, fn_def: &FnDef, p: u32, i: usize) -> Vec<HirTy> {
    let Some(key) = ctx.env.bound_trait(&fn_def.name) else {
        return Vec::new();
    };
    let cands = of_trait(ctx, p, key);
    let info = ctx.env.trait_of(key).expect("a trait");
    let slot = fn_def.params.get(i).and_then(|q| q.ty.as_ref());
    let Some(AstTyKind::Path(path)) = slot.map(|t| &t.kind) else {
        return Vec::new();
    };
    let [seg] = path.segments.as_slice() else {
        return Vec::new();
    };
    let Some(k) = info.generics.params.iter().position(
        |gp| matches!(gp, rokc_ast::GenericParam::Type { name, .. } if *name == seg.name),
    ) else {
        return Vec::new();
    };
    let mut out: Vec<HirTy> = Vec::new();
    for r in &cands {
        if let Some(a) = r.args.get(k) {
            if !out.contains(a) {
                out.push(a.clone());
            }
        }
    }
    if out.len() < 2 {
        out.clear();
    }
    out
}

/// `bound_methods` templates: each trait method's declaration with the
/// trait's parameters first, no body, bare associated types read as
/// `Self::X`; plus template name → trait.
pub(super) fn templates(
    trait_info: &HashMap<String, TraitInfo>,
) -> (
    HashMap<(String, String), GenericMethod>,
    HashMap<String, String>,
) {
    let mut out = HashMap::new();
    let mut traits = HashMap::new();
    for (key, info) in trait_info {
        let mut assoc: HashMap<String, Ty> = HashMap::new();
        let mut seen: Vec<&str> = Vec::new();
        let mut stack = vec![key.as_str()];
        while let Some(k) = stack.pop() {
            if seen.contains(&k) {
                continue;
            }
            seen.push(k);
            let Some(i) = trait_info.get(k) else { continue };
            for (n, _) in &i.assoc {
                // The trait's own `X` is its bound's, `<Self as Tr<A>>::X`:
                // `Self` may have `Tr<B>`, or another trait's `X`.
                let own = (k == key).then(|| own_assoc(key, i, n));
                assoc
                    .entry(n.clone())
                    .or_insert_with(|| own.unwrap_or_else(|| self_assoc(n)));
            }
            stack.extend(i.supers.iter().map(String::as_str));
        }
        let module = module_of(key);
        for (m, decl) in &info.decls {
            let mut def = decl.clone();
            def.body = None;
            def.name = format!("#{}__{m}", leaf_name(key));
            if !module.is_empty() {
                def.name = format!("{module}::{}", def.name);
            }
            let own = std::mem::take(&mut def.generics.params);
            def.generics.params = info
                .generics
                .params
                .iter()
                .map(|gp| match gp {
                    rokc_ast::GenericParam::Type { name, .. } => rokc_ast::GenericParam::Type {
                        name: name.clone(),
                        bounds: Vec::new(),
                        default: None,
                    },
                    c @ rokc_ast::GenericParam::Const { .. } => c.clone(),
                })
                .chain(own)
                .collect();
            collect::subst_assoc_sig(&mut def, &assoc);
            traits.insert(def.name.clone(), key.clone());
            let self_ty = Ty {
                kind: AstTyKind::SelfUpper,
                span: def.span,
            };
            out.insert(
                (key.clone(), m.clone()),
                GenericMethod {
                    def: std::sync::Arc::new(def),
                    self_ty,
                    info: MethodInfo {
                        module: module.clone(),
                        vis: Visibility::Pub,
                        trait_name: Some(key.clone()),
                        trait_args: Vec::new(),
                    },
                },
            );
        }
    }
    (out, traits)
}

/// `Self::name` as written.
fn self_assoc(name: &str) -> Ty {
    let seg = |n: &str| rokc_ast::PathSegment {
        name: n.to_owned(),
        generics: Vec::new(),
    };
    Ty {
        kind: AstTyKind::Path(AstPath {
            segments: vec![seg("Self"), seg(name)],
            span: Span::DUMMY,
            origin: None,
        }),
        span: Span::DUMMY,
    }
}

/// `<Self as Tr<P..>>::name` for trait `key` (`info`), over its own
/// parameter names, as written in its module.
fn own_assoc(key: &str, info: &TraitInfo, name: &str) -> Ty {
    let ty = |n: &str| Ty {
        kind: AstTyKind::Path(AstPath {
            segments: vec![rokc_ast::PathSegment {
                name: n.to_owned(),
                generics: Vec::new(),
            }],
            span: Span::DUMMY,
            origin: None,
        }),
        span: Span::DUMMY,
    };
    let args = info
        .generics
        .params
        .iter()
        .map(|gp| match gp {
            rokc_ast::GenericParam::Type { name, .. }
            | rokc_ast::GenericParam::Const { name, .. } => ty(name),
        })
        .collect();
    Ty {
        kind: AstTyKind::QPath(Box::new(rokc_ast::QPath {
            self_ty: Ty {
                kind: AstTyKind::SelfUpper,
                span: Span::DUMMY,
            },
            trait_path: AstPath {
                segments: vec![rokc_ast::PathSegment {
                    name: leaf_name(key).to_owned(),
                    generics: args,
                }],
                span: Span::DUMMY,
                origin: None,
            },
            item: name.to_owned(),
            member: None,
        })),
        span: Span::DUMMY,
    }
}

/// Is `f` checked per instance only (an implicit parameter)?
fn implicit(f: &FnDef) -> bool {
    f.generics
        .params
        .iter()
        .any(|gp| matches!(gp, rokc_ast::GenericParam::Type { name, .. } if name.starts_with("T#")))
}

/// Check every generic body `wanted` picks; trait defaults once with an
/// opaque `Self` (impl copies aren't checked).
pub(super) fn check_all(
    env: &Env<'_>,
    incr: Option<&super::incr::Incr<'_>>,
    wanted: impl Fn(&str) -> bool,
) {
    let defaults: std::collections::HashSet<Span> = env
        .trait_info
        .values()
        .flat_map(|i| i.decls.values())
        .filter(|d| d.body.is_some())
        .map(|d| d.span)
        .collect();
    // (body, module, `Self`, how many leading parameters are the impl's)
    let mut todo: Vec<(&FnDef, String, SelfOf<'_>, usize)> = env
        .generic_fns
        .values()
        .map(|f| (*f, SelfOf::None, 0))
        .chain(env.impls.generic().flat_map(|gi| {
            let outer = gi.generics.params.len();
            gi.methods
                .values()
                .map(move |gm| (&*gm.def, SelfOf::Impl(&gm.self_ty), outer))
        }))
        .chain(
            env.generic_methods
                .values()
                .flatten()
                .map(|gm| (&*gm.def, SelfOf::Impl(&gm.self_ty), 0)),
        )
        .filter(|(f, ..)| f.body.is_some() && !implicit(f) && !defaults.contains(&f.span))
        .map(|(f, s, outer)| (f, module_of(&f.name), s, outer))
        .chain(env.trait_info.iter().flat_map(|(key, info)| {
            info.decls
                .values()
                .filter(|d| d.body.is_some())
                .map(|d| (d, info.module.clone(), SelfOf::Trait(key.as_str()), 0))
        }))
        .filter(|(_, module, ..)| wanted(module))
        .collect();
    todo.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    todo.dedup_by(|a, b| a.0.name == b.0.name && a.0.span == b.0.span);
    todo.sort_by_key(|(f, ..)| (f.span.file, f.span.start));
    for (f, module, self_of, outer) in todo {
        let generics: Vec<&rokc_ast::GenericParam> = match self_of {
            SelfOf::Trait(key) => env.trait_info[key].generics.params.iter().collect(),
            _ => Vec::new(),
        };
        let generics: Vec<_> = generics.into_iter().chain(&f.generics.params).collect();
        let mut owner = rokc_core::codec::Enc::new(true);
        match &self_of {
            SelfOf::None => {}
            SelfOf::Impl(t) => rokc_core::codec::Codec::enc(*t, &mut owner),
            SelfOf::Trait(k) => owner.buf.extend_from_slice(k.as_bytes()),
        }
        let run = || check_one(env, f, module.clone(), &generics, &self_of, outer);
        let errors = match incr {
            Some(incr) => incr.checked(env, f, &owner.buf, run),
            None => run(),
        };
        if errors.iter().any(rokc_diagnostic::RokDiagnostic::is_error) {
            env.sink.failed.borrow_mut().insert(f.span);
        }
        env.sink.diagnostics.borrow_mut().extend(errors);
    }
    check_decl_types(env, &wanted);
}

/// Generic alias bounds in types no body check sees (generic
/// fields, generic trait impl arguments and associated types, bodiless
/// method signatures) must hold by the declaration's bounds.
fn check_decl_types(env: &Env<'_>, wanted: &impl Fn(&str) -> bool) {
    struct Decl<'a> {
        generics: Vec<&'a rokc_ast::GenericParam>,
        where_clause: Vec<&'a rokc_ast::WherePred>,
        module: String,
        span: Span,
        self_of: SelfOf<'a>,
        tys: Vec<&'a Ty>,
        /// What declares a parameter, from its position.
        owner: Box<dyn Fn(usize) -> String + 'a>,
    }
    let mut todo: Vec<Decl<'_>> = Vec::new();
    let item = |name: &str| {
        let name = format!("`{}`'s generics", leaf_name(name));
        Box::new(move |_| name.clone()) as Box<dyn Fn(usize) -> String>
    };
    for (name, s) in &env.defs.generic_structs {
        let tys = match &s.kind {
            rokc_ast::StructKind::Unit => Vec::new(),
            rokc_ast::StructKind::Tuple(fs) => fs.iter().map(|f| &f.ty).collect(),
            rokc_ast::StructKind::Named(fs) => fs.iter().map(|f| &f.ty).collect(),
        };
        todo.push(Decl {
            generics: s.generics.params.iter().collect(),
            where_clause: s.where_clause.iter().collect(),
            module: module_of(name),
            span: s.span,
            self_of: SelfOf::None,
            tys,
            owner: item(name),
        });
    }
    for (name, u) in &env.defs.generic_unions {
        todo.push(Decl {
            generics: u.generics.params.iter().collect(),
            where_clause: u.where_clause.iter().collect(),
            module: module_of(name),
            span: u.span,
            self_of: SelfOf::None,
            tys: u.fields.iter().map(|f| &f.ty).collect(),
            owner: item(name),
        });
    }
    for (name, e) in &env.defs.generic_enums {
        let tys = e
            .variants
            .iter()
            .flat_map(|v| match &v.kind {
                rokc_ast::EnumVariantKind::Tuple(ts) => ts.iter().collect(),
                rokc_ast::EnumVariantKind::Named(fs) => fs.iter().map(|f| &f.ty).collect(),
                _ => Vec::new(),
            })
            .collect();
        todo.push(Decl {
            generics: e.generics.params.iter().collect(),
            where_clause: e.where_clause.iter().collect(),
            module: module_of(name),
            span: e.span,
            self_of: SelfOf::None,
            tys,
            owner: item(name),
        });
    }
    for gi in env.impls.generic().filter(|gi| !gi.trait_key.is_empty()) {
        todo.push(Decl {
            generics: gi.generics.params.iter().collect(),
            where_clause: gi.where_clause.iter().collect(),
            module: gi.module.clone(),
            span: gi.self_ty.span,
            self_of: SelfOf::Impl(&gi.self_ty),
            tys: gi
                .trait_args
                .iter()
                .chain(gi.assoc.iter().map(|(_, t)| t))
                .collect(),
            owner: Box::new(|_| "the impl's generics".to_owned()),
        });
    }
    for (key, info) in &env.trait_info {
        let outer = info.generics.params.len();
        for d in info.decls.values().filter(|d| d.body.is_none()) {
            let tys = d
                .params
                .iter()
                .filter_map(|p| p.ty.as_ref())
                .chain(&d.ret_ty)
                .collect();
            let (method, of_trait) = (item(&d.name), item(key));
            todo.push(Decl {
                generics: info
                    .generics
                    .params
                    .iter()
                    .chain(&d.generics.params)
                    .collect(),
                where_clause: d.where_clause.iter().collect(),
                module: info.module.clone(),
                span: d.span,
                self_of: SelfOf::Trait(key),
                tys,
                owner: Box::new(move |at| {
                    if at >= outer {
                        method(at)
                    } else {
                        of_trait(at)
                    }
                }),
            });
        }
    }
    todo.retain(|d| wanted(&d.module));
    todo.sort_by_key(|d| (d.span.file, d.span.start));
    for d in todo {
        let owner = |p: &str| {
            let at = d.generics.iter().rposition(
                |gp| matches!(gp, rokc_ast::GenericParam::Type { name, .. } if name == p),
            )?;
            Some((d.owner)(at))
        };
        let (errors, _) = in_scope(
            env,
            &d.generics,
            &d.where_clause,
            (d.module.clone(), d.span, &[]),
            &d.self_of,
            |ctx, _, _| {
                let from = env.names.alias_uses.borrow().len();
                for t in &d.tys {
                    ctx.resolve_ast_ty(t);
                }
                let uses = env.names.alias_uses.borrow()[from..].to_vec();
                let mut seen = std::collections::HashSet::new();
                uses.iter()
                    .filter(|u| seen.insert((&u.0, u.3)))
                    .flat_map(|u| collect::alias_use_unmet(ctx, u, true, owner))
                    .collect::<Vec<_>>()
            },
        );
        env.sink.diagnostics.borrow_mut().extend(errors);
    }
}

/// What `Self` is in a checked body.
pub(super) enum SelfOf<'a> {
    None,
    /// An impl's target as written.
    Impl(&'a Ty),
    /// A trait's default method: opaque, bounded by the trait.
    Trait(&'a str),
}

/// What a check may leave in the shared state, to be undone after it.
struct Snapshot {
    next_id: u32,
    structs: usize,
    enums: usize,
    params: usize,
    infer_vars: usize,
    name_diags: usize,
    alias_uses: usize,
    unknown: std::collections::HashSet<String>,
    /// Return types inferred so far: one inferred during the check may
    /// name a type instance the check made.
    inferred_ret: HashMap<FunctionId, Option<HirTy>>,
    recursive_ret: usize,
    tentative_ret: usize,
}

impl Snapshot {
    fn take(env: &Env<'_>) -> Self {
        let sink = &env.sink;
        Self {
            next_id: sink.next_id.get(),
            structs: env.defs.structs.borrow().len(),
            enums: env.defs.enums.borrow().len(),
            params: sink.params.borrow().len(),
            infer_vars: sink.infer_vars.borrow().len(),
            name_diags: env.names.diags.borrow().len(),
            alias_uses: env.names.alias_uses.borrow().len(),
            unknown: env.names.unknown_spans.borrow().keys().cloned().collect(),
            inferred_ret: sink.inferred_ret.borrow().clone(),
            recursive_ret: sink.recursive_ret.borrow().len(),
            tentative_ret: sink.tentative_ret.borrow().len(),
        }
    }

    /// Back to the snapshot (after `LowerCtx::rollback`); the name
    /// diagnostics reported since.
    fn restore(self, env: &Env<'_>) -> Vec<rokc_diagnostic::RokDiagnostic> {
        let sink = &env.sink;
        let old = |id: &FunctionId| id.0 < self.next_id;
        sink.fn_origin.borrow_mut().retain(|id, _| old(id));
        sink.track_caller.borrow_mut().retain(old);
        *sink.inferred_ret.borrow_mut() = self.inferred_ret;
        sink.recursive_ret.borrow_mut().truncate(self.recursive_ret);
        sink.tentative_ret.borrow_mut().truncate(self.tentative_ret);
        sink.next_id.set(self.next_id);
        env.defs.truncate(self.structs, self.enums);
        // The ids of the dropped instances are handed out again.
        env.affine_memo.borrow_mut().clear();
        sink.params.borrow_mut().truncate(self.params);
        sink.infer_vars.borrow_mut().truncate(self.infer_vars);
        sink.const_names.borrow_mut().clear();
        sink.assumed.borrow_mut().clear();
        env.names.alias_uses.borrow_mut().truncate(self.alias_uses);
        env.names
            .unknown_spans
            .borrow_mut()
            .retain(|k, _| self.unknown.contains(k));
        env.names.diags.borrow_mut().split_off(self.name_diags)
    }
}

/// Lower `f` with `generics` opaque; its errors. Shared state is restored.
fn check_one(
    env: &Env<'_>,
    f: &FnDef,
    module: String,
    generics: &[&rokc_ast::GenericParam],
    self_of: &SelfOf<'_>,
    outer: usize,
) -> Vec<rokc_diagnostic::RokDiagnostic> {
    let where_clause: Vec<&rokc_ast::WherePred> = f.where_clause.iter().collect();
    // What declares type parameter `p`, for an alias bound's help.
    let outer = match self_of {
        SelfOf::Trait(key) => env.trait_info[*key].generics.params.len(),
        _ => outer,
    };
    let owner = |p: &str| {
        let at = generics
            .iter()
            .rposition(|gp| matches!(gp, rokc_ast::GenericParam::Type { name, .. } if name == p))?;
        Some(match self_of {
            // A method template is `Base__method`.
            SelfOf::Impl(_) if at >= outer => {
                let name = leaf_name(&f.name);
                format!("`{}`'s generics", name.rsplit("__").next().unwrap_or(name))
            }
            _ if at >= outer => format!("`{}`'s generics", leaf_name(&f.name)),
            SelfOf::Trait(key) => format!("`{}`'s generics", leaf_name(key)),
            _ => "the impl's generics".to_owned(),
        })
    };
    let ((), mut errors) = in_scope(
        env,
        generics,
        &where_clause,
        (module.clone(), f.span, &f.attrs),
        self_of,
        |ctx, subst, self_hir| {
            let from = env.names.alias_uses.borrow().len();
            let id = FunctionId(env.sink.next_id.get());
            env.sink.next_id.set(id.0 + 1);
            mono::lower_fn_inner(env, f, id, f.name.clone(), self_hir, module, subst);
            // A generic alias's bounds on these parameters
            // hold by theirs.
            let uses = env.names.alias_uses.borrow()[from..].to_vec();
            let mut seen = std::collections::HashSet::new();
            for u in uses.iter().filter(|u| seen.insert((&u.0, u.3))) {
                let errors = collect::alias_use_unmet(ctx, u, true, owner);
                env.sink.diagnostics.borrow_mut().extend(errors);
            }
        },
    );
    // A use of a marked item warns even when nothing instantiates the body.
    errors.retain(|d| {
        d.is_error()
            || d.lint
                .as_deref()
                .is_some_and(|l| l.starts_with("deprecated"))
    });
    errors
}

/// Run `run` as a generic body sees things (`generics` opaque under their
/// bounds and `where_clause`); its result and errors. Shared state is
/// restored.
pub(super) fn in_scope<R>(
    env: &Env<'_>,
    generics: &[&rokc_ast::GenericParam],
    where_clause: &[&rokc_ast::WherePred],
    (module, at, attrs): (String, Span, &[rokc_ast::Attribute]),
    self_of: &SelfOf<'_>,
    run: impl FnOnce(&mut LowerCtx<'_>, &TySubst, Option<HirTy>) -> R,
) -> (R, Vec<rokc_diagnostic::RokDiagnostic>) {
    let snapshot = Snapshot::take(env);
    let mut subst = TySubst::new();
    let mut consts = Vec::new();
    for gp in generics {
        match gp {
            rokc_ast::GenericParam::Type { name, .. } => {
                subst.insert(name.clone(), fresh(env, name.clone()));
            }
            rokc_ast::GenericParam::Const { name, ty } => consts.push((name, ty)),
        }
    }
    for (name, ty) in consts {
        let p = match LowerCtx::new(env, &subst, None, at).resolve_ast_ty(ty) {
            HirTy::Prim(p) => p,
            _ => PrimTy::Usize,
        };
        let v = match p {
            PrimTy::Bool => 1,
            PrimTy::Char => i128::from(u32::from('a')),
            _ => stand_in(env, name),
        };
        subst.insert(name.clone(), HirTy::Const(v, Some(p)));
    }
    let self_param = match self_of {
        SelfOf::Trait(key) => {
            let me = fresh(env, "Self".to_owned());
            let args = env
                .trait_of(key)
                .expect("a trait")
                .generics
                .params
                .iter()
                .map(|gp| {
                    let (rokc_ast::GenericParam::Type { name, .. }
                    | rokc_ast::GenericParam::Const { name, .. }) = gp;
                    subst[name].clone()
                })
                .collect();
            if let HirTy::Param(p) = me {
                add_bound(env, p, TraitRef::new((*key).to_owned(), args, Vec::new()));
            }
            Some(me)
        }
        _ => None,
    };
    let mut ctx = LowerCtx::new(env, &subst, self_param.clone(), at);
    ctx.current_module.clone_from(&module);
    ctx.assoc_names = attrs
        .iter()
        .filter(|a| a.path.first().is_some_and(|p| p == crate::ASSOC_TYPES_ATTR))
        .flat_map(|a| a.path[1..].iter().cloned())
        .collect();
    let mark = ctx.probe_mark();
    // Parameter bounds first, then `where` clauses naming one parameter
    // alone, then the rest (`T::Item: Display` projects through `T`).
    for gp in generics {
        if let rokc_ast::GenericParam::Type { name, bounds, .. } = gp {
            for b in bounds {
                let t = &subst[name];
                if let HirTy::Param(p) = t {
                    match bound_on(&ctx, b, &module, t) {
                        Some(r) => add_bound(env, *p, r),
                        None if matches!(b.kind, AstTyKind::Path(_)) => {
                            env.sink.params.borrow_mut()[*p as usize].unknown = true;
                        }
                        None => {}
                    }
                }
            }
        }
    }
    let bare = |w: &&&rokc_ast::WherePred| matches!(&w.ty.kind, AstTyKind::Path(p) if p.segments.len() == 1 && subst.contains_key(&p.segments[0].name));
    let (alone, rest): (Vec<&&rokc_ast::WherePred>, Vec<_>) = where_clause.iter().partition(bare);
    // A clause on another type (`where List<T>: Display`) is assumed in
    // the body.
    for w in alone.iter().chain(&rest) {
        let t = ctx.resolve_ast_ty(&w.ty);
        for b in &w.bounds {
            match (&t, bound_on(&ctx, b, &module, &t)) {
                (HirTy::Param(p), Some(r)) => add_bound(env, *p, r),
                (HirTy::Param(p), None) if matches!(b.kind, AstTyKind::Path(_)) => {
                    env.sink.params.borrow_mut()[*p as usize].unknown = true;
                }
                (_, Some(r)) => env.sink.assumed.borrow_mut().push((t.clone(), r)),
                _ => {}
            }
        }
    }
    let self_hir = match self_of {
        SelfOf::Impl(t) => Some(ctx.resolve_ast_ty(t)),
        _ => self_param,
    };
    ctx.self_ty.clone_from(&self_hir);
    let out = run(&mut ctx, &subst, self_hir);
    let mut errors = ctx.rollback(&mark);
    errors.extend(snapshot.restore(env));
    (out, errors)
}

/// Does a `where` clause of the body being checked give `ty` `want`?
pub(super) fn assumed(env: &Env<'_>, ty: &HirTy, want: &TraitRef) -> bool {
    let assumed = env.sink.assumed.borrow();
    assumed.iter().any(|(t, r)| t == ty && r.satisfies(want))
}

/// The integer standing for const `name` in the body being checked:
/// a prime past the unrolled array lengths no other one there takes.
fn stand_in(env: &Env<'_>, name: &str) -> i128 {
    let mut names = env.sink.const_names.borrow_mut();
    if let Some((v, _)) = names.iter().find(|(_, n)| n == name) {
        return *v;
    }
    let floor = names.iter().map(|(v, _)| *v).max().unwrap_or(100);
    // Prime gaps this low are far below 1000.
    let v = (floor + 1..floor + 1000)
        .find(|n| (2..*n).all(|d| n % d != 0))
        .unwrap_or_default();
    names.push((v, name.to_owned()));
    v
}

/// The `const` parameter value `v` stands for in the body being
/// checked.
pub(super) fn const_name(env: &Env<'_>, v: i128) -> Option<String> {
    let names = env.sink.const_names.borrow();
    names.iter().find(|(n, _)| *n == v).map(|(_, s)| s.clone())
}

/// `T::NAME`: the bound trait's const, of its declared type.
/// An integer, `bool` or `char` gets a value unique in the body (like a
/// `const` parameter); other types are opaque.
pub(super) fn assoc_const(
    ctx: &LowerCtx<'_>,
    p: u32,
    name: &str,
    traits: Option<&[String]>,
    wants: &[TraitRef],
) -> Option<HirExpr> {
    let me = HirTy::Param(p);
    let span = ctx.here();
    let has = |r: &TraitRef| {
        ctx.env
            .trait_of(&r.path)
            .is_some_and(|i| i.consts.iter().any(|(n, _)| n == name))
    };
    let Ok(r) = item_bound(ctx, p, name, traits, wants, has)? else {
        return Some(poison(span));
    };
    let info = ctx.env.trait_of(&r.path).expect("a trait");
    let decl = info.consts.iter().find(|(n, _)| n == name)?.1.clone();
    let mut subst = TySubst::new();
    let ty = in_trait(ctx, info, &r, &me, &mut subst).resolve_ast_ty(&decl);
    let shown = item_shown(ctx, p, &r, name);
    let value = match &ty {
        HirTy::Prim(PrimTy::Bool) => Some(ConstVal::Bool(true)),
        HirTy::Prim(PrimTy::Char) => Some(ConstVal::Char(u32::from('a'))),
        HirTy::Prim(t) if t.is_int() => Some(ConstVal::Int {
            value: stand_in(ctx.env, &shown),
            ty: *t,
        }),
        _ => None,
    };
    let kind = value.map_or(
        HirExprKind::Call {
            callee: FunctionId(u32::MAX),
            args: Vec::new(),
        },
        HirExprKind::Const,
    );
    Some(HirExpr { kind, ty, span })
}

/// `t.method(..)` on a type parameter whose bounds don't declare it.
pub(super) fn no_method(ctx: &LowerCtx<'_>, p: u32, method: &str, span: Span) -> HirExpr {
    let t = ctx.ty_name(&HirTy::Param(p));
    let info = ctx.env.sink.params.borrow()[p as usize].clone();
    if info.unknown {
        return poison(span);
    }
    let bounds: Vec<String> = info
        .bounds
        .iter()
        .map(|r| format!("`{}`", leaf_name(&r.path)))
        .collect();
    let have = if bounds.is_empty() {
        "it has no bounds".to_owned()
    } else {
        format!("its bounds ({}) do not declare it", bounds.join(", "))
    };
    let e = ctx.error_expr(
        Code::NO_METHOD,
        span,
        format!("no method `{method}` on type parameter `{t}`: {have}"),
    );
    ctx.help_last(Some(format!(
        "add a bound on `{t}` whose trait declares `{method}`"
    )));
    e
}

/// Is a body that failed its check being lowered for an instance or an
/// impl (its errors repeat the check's)?
pub(super) fn in_failed(env: &Env<'_>) -> bool {
    let failed = env.sink.failed.borrow();
    env.sink.bodies.borrow().iter().any(|s| {
        super::incr::read(|| super::incr::Read::CheckFailed(*s));
        failed.contains(s)
    })
}

rokc_core::codec_struct!(ParamInfo {
    name,
    bounds,
    all,
    projs,
    unknown
});
