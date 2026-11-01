//! The library's collected stage serialized (spec 9.2.16), so a build of
//! the same library skips collecting it. Borrows into the items are
//! written as (item, member) positions and re-borrowed on read.

use std::borrow::Cow;
use std::collections::HashMap;

use rokc_ast::{ExternItem, ImplItem, TraitDef, TraitItem};
use rokc_core::codec::{Codec, Dec, Enc, Keep};
use rokc_core::{codec_enum, codec_struct};

use super::super::{
    ClosureSink, Impl, Impls, InstanceWarns, MethodOrFn, MethodTarget, ShapeHandler, TypeDefs,
    TypeNames,
};
use super::{Collected, Intrinsic};
use crate::lower::{AstEnumDef, AstStructDef, Expr, FnDef, Item, Program, Ty};

/// The library's stage of a compile (`HirInput::library`).
pub struct LibraryStage {
    pub(super) c: Collected<'static>,
    pub(super) lib: &'static Program,
    pub(super) type_names: [Vec<String>; 2],
}

impl std::fmt::Debug for LibraryStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryStage").finish_non_exhaustive()
    }
}

impl LibraryStage {
    /// The library's items (spans by this compile's file ids).
    #[must_use]
    pub const fn items(&self) -> &'static Program {
        self.lib
    }

    /// Write the stage; `false` if it borrows something outside the
    /// library's items. Bodies are read back lazily (`Dec::keep`).
    #[must_use]
    pub fn enc(&self, e: &mut Enc) -> bool {
        e.lazy = true;
        let at = Locs::of(self.lib);
        let impls = &self.c.t.impls;
        enc_groups(&self.lib.items, ITEM_GROUPS, e, Codec::enc);
        enc_groups(&impls.all, IMPL_GROUPS, e, Codec::enc);
        enc_blob(e, |b| {
            impls.by_target.enc(b);
            impls.by_trait.enc(b);
            impls.generic_of.enc(b);
        });
        let mut methods: Vec<_> = self.c.t.generic_methods.iter().collect();
        methods.sort_by(|a, b| a.0.cmp(b.0));
        // As `((String, String), Vec<GenericMethod>)`.
        enc_groups(&methods, METHOD_GROUPS, e, |(k, v), e| {
            k.enc(e);
            v.enc(e);
        });
        self.type_names.enc(e);
        enc_collected(&self.c, &at, e).is_some()
    }

    /// Read a stage back; its items live for the process. Items, impls,
    /// generic methods and borrow-free tables decode in parallel.
    pub fn dec(d: &mut Dec<'_>) -> Option<Self> {
        let keep = d.keep.clone()?;
        let keep = &keep;
        let items = groups(d)?;
        let impls = groups(d)?;
        let index = d.bytes()?;
        let methods = groups(d)?;
        let type_names = Codec::dec(d)?;
        let (fns, infos, plain) = (d.bytes()?, d.bytes()?, d.bytes()?);
        let files = d.files;
        let (items, all, index, methods, c) = std::thread::scope(|s| {
            let index = spawn(s, move || dec_blob::<ImplIndex>(index, files, keep));
            let fns = spawn(s, move || dec_blob(fns, files, keep));
            let infos = spawn(s, move || dec_blob(infos, files, keep));
            let c = spawn(s, move || {
                let mut d = Dec::new(plain);
                d.files = files;
                d.keep = Some(keep.clone());
                let mut c = dec_plain(&mut d).filter(|_| d.done())?;
                c.t.fn_table = join(fns)?;
                c.t.method_info = join(infos)?;
                Some(c)
            });
            let items = dec_groups::<Item>(s, items, files, keep);
            let all = dec_groups(s, impls, files, keep);
            let methods = dec_groups(s, methods, files, keep);
            (
                join_all(items),
                join_all(all),
                join(index),
                join_all(methods),
                join(c),
            )
        });
        let lib: &'static Program = Box::leak(Box::new(Program {
            items: items?,
            ..Program::default()
        }));
        let mut c = c?;
        dec_borrows(lib, d, &mut c)?;
        let (by_target, by_trait, generic_of) = index?;
        c.t.impls = Impls {
            all: all?,
            by_target,
            by_trait,
            generic_of,
        };
        c.t.generic_methods = methods?.into_iter().collect();
        Some(Self { c, lib, type_names })
    }
}

type ImplIndex = (
    HashMap<MethodTarget, Vec<usize>>,
    HashMap<String, Vec<usize>>,
    HashMap<String, Vec<usize>>,
);

/// The stack of a decoding thread: the parse threads' (items nest as
/// deep as they parsed).
const STACK: usize = 1 << 30;

type Job<'s, T> = Option<std::thread::ScopedJoinHandle<'s, Option<T>>>;

fn spawn<'s, T: Send + 's>(
    s: &'s std::thread::Scope<'s, '_>,
    f: impl FnOnce() -> Option<T> + Send + 's,
) -> Job<'s, T> {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn_scoped(s, f)
        .ok()
}

fn join<T>(j: Job<'_, T>) -> Option<T> {
    j?.join().ok().flatten()
}

/// What `write` writes, as bytes of its own (decoded apart, `dec_blob`).
fn enc_blob(e: &mut Enc, write: impl FnOnce(&mut Enc)) {
    let mut b = Enc::new(e.spans);
    b.lazy = e.lazy;
    write(&mut b);
    merge_files(&b, e);
    e.bytes(&b.buf);
}

fn dec_blob<T: Codec>(bytes: &[u8], files: Option<&[u32]>, keep: &Keep) -> Option<T> {
    let mut d = Dec::new(bytes);
    d.files = files;
    d.keep = Some(keep.clone());
    let v = T::dec(&mut d)?;
    d.done().then_some(v)
}

fn merge_files(from: &Enc, e: &mut Enc) {
    for f in &from.files {
        if !e.files.contains(f) {
            e.files.push(*f);
        }
    }
}

/// Groups of about the same size, decoded in parallel: of the items,
/// of the impls, of the generic methods.
const ITEM_GROUPS: usize = 16;
const IMPL_GROUPS: usize = 8;
const METHOD_GROUPS: usize = 4;

/// `items` (each as `write` writes it) in `n` groups, each its item
/// count and bytes.
fn enc_groups<T>(items: &[T], n: usize, e: &mut Enc, write: impl Fn(&T, &mut Enc)) {
    let each: Vec<Enc> = items
        .iter()
        .map(|item| {
            let mut ie = Enc::new(e.spans);
            ie.lazy = e.lazy;
            write(item, &mut ie);
            ie
        })
        .collect();
    // Sized by what a read decodes (not the lazy values).
    let size = |ie: &Enc| ie.buf.len() - ie.lazy_bytes;
    let total: usize = each.iter().map(size).sum();
    let per = total.div_ceil(n).max(1);
    let mut groups: Vec<(u64, Vec<u8>, usize)> = Vec::new();
    for ie in each {
        if groups.last().is_none_or(|g| g.2 >= per) {
            groups.push((0, Vec::new(), 0));
        }
        let g = groups.last_mut().expect("a group");
        g.0 += 1;
        g.1.extend_from_slice(&ie.buf);
        g.2 += size(&ie);
        merge_files(&ie, e);
    }
    e.u(groups.len() as u64);
    for (n, bytes, _) in &groups {
        e.u(*n);
        e.bytes(bytes);
    }
}

/// The groups `enc_groups` wrote: item count and bytes.
fn groups<'a>(d: &mut Dec<'a>) -> Option<Vec<(usize, &'a [u8])>> {
    let groups = usize::try_from(d.u()?).ok()?;
    (0..groups)
        .map(|_| Some((usize::try_from(d.u()?).ok()?, d.bytes()?)))
        .collect()
}

/// The values of `groups`, each group on a thread of its own.
fn dec_groups<'s, T: Codec + Send + 's>(
    s: &'s std::thread::Scope<'s, '_>,
    groups: Vec<(usize, &'s [u8])>,
    files: Option<&'s [u32]>,
    keep: &'s Keep,
) -> Vec<Job<'s, Vec<T>>> {
    groups
        .into_iter()
        .map(|(n, bytes)| {
            spawn(s, move || {
                let mut d = Dec::new(bytes);
                d.files = files;
                d.keep = Some(keep.clone());
                let items: Option<Vec<T>> = (0..n).map(|_| T::dec(&mut d)).collect();
                items.filter(|_| d.done())
            })
        })
        .collect()
}

/// The values of `jobs`, in order.
fn join_all<T>(jobs: Vec<Job<'_, Vec<T>>>) -> Option<Vec<T>> {
    let mut items = Vec::new();
    for j in jobs {
        items.extend(join(j)?);
    }
    Some(items)
}

/// What a borrowed AST node is.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Fn,
    Struct,
    Union,
    Enum,
    Trait,
    Ty,
    Expr,
}

/// No member: the item itself.
const WHOLE: u32 = u32::MAX;

/// Where each borrowable node of a program is: (item, member).
struct Locs(HashMap<(Kind, usize), (u32, u32)>);

fn addr<T>(r: &T) -> usize {
    std::ptr::from_ref(r) as usize
}

impl Locs {
    fn of(p: &Program) -> Self {
        let mut m = HashMap::new();
        for (i, item) in (0u32..).zip(&p.items) {
            let mut put = |k: Kind, a: usize, j: u32| {
                m.insert((k, a), (i, j));
            };
            match item {
                Item::Fn(f) => put(Kind::Fn, addr(f), WHOLE),
                Item::Struct(s) => put(Kind::Struct, addr(s), WHOLE),
                Item::Union(u) => put(Kind::Union, addr(u), WHOLE),
                Item::Enum(e) => put(Kind::Enum, addr(e), WHOLE),
                Item::Trait(t) => {
                    put(Kind::Trait, addr(t), WHOLE);
                    for (j, ti) in (0u32..).zip(&t.items) {
                        if let TraitItem::Method(f) = ti {
                            put(Kind::Fn, addr(f), j);
                        }
                    }
                }
                Item::Impl(im) => {
                    for (j, ii) in (0u32..).zip(&im.items) {
                        if let ImplItem::Method(f) = ii {
                            put(Kind::Fn, addr(f), j);
                        }
                    }
                }
                Item::Static(s) => {
                    put(Kind::Ty, addr(&s.ty), WHOLE);
                    if let Some(v) = &s.value {
                        put(Kind::Expr, addr(v), WHOLE);
                    }
                }
                Item::Extern(b) => {
                    for (j, xi) in (0u32..).zip(&b.items) {
                        match xi {
                            ExternItem::Fn(f) => put(Kind::Fn, addr(f), j),
                            ExternItem::Static(s) => {
                                put(Kind::Ty, addr(&s.ty), j);
                                if let Some(v) = &s.value {
                                    put(Kind::Expr, addr(v), j);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Self(m)
    }

    fn enc<T>(&self, kind: Kind, r: &T, e: &mut Enc) -> Option<()> {
        let &(item, member) = self.0.get(&(kind, addr(r)))?;
        e.u(u64::from(item));
        e.u(u64::from(member));
        Some(())
    }
}

/// A node `Locs` put at (item, member) of `p`.
trait Located: Sized + 'static {
    fn find(item: &'static Item, member: u32) -> Option<&'static Self>;

    fn read(p: &'static Program, d: &mut Dec<'_>) -> Option<&'static Self> {
        let i = usize::try_from(d.u()?).ok()?;
        let j = u32::try_from(d.u()?).ok()?;
        Self::find(p.items.get(i)?, j)
    }
}

impl Located for FnDef {
    fn find(item: &'static Item, j: u32) -> Option<&'static Self> {
        let j_ = j as usize;
        match item {
            Item::Fn(f) if j == WHOLE => Some(f),
            Item::Trait(t) => match t.items.get(j_)? {
                TraitItem::Method(f) => Some(f),
                _ => None,
            },
            Item::Impl(im) => match im.items.get(j_)? {
                ImplItem::Method(f) => Some(f),
                _ => None,
            },
            Item::Extern(b) => match b.items.get(j_)? {
                ExternItem::Fn(f) => Some(f),
                ExternItem::Static(_) => None,
            },
            _ => None,
        }
    }
}

impl Located for AstStructDef {
    fn find(item: &'static Item, j: u32) -> Option<&'static Self> {
        match item {
            Item::Struct(s) if j == WHOLE => Some(s),
            _ => None,
        }
    }
}

impl Located for rokc_ast::UnionDef {
    fn find(item: &'static Item, j: u32) -> Option<&'static Self> {
        match item {
            Item::Union(u) if j == WHOLE => Some(u),
            _ => None,
        }
    }
}

impl Located for AstEnumDef {
    fn find(item: &'static Item, j: u32) -> Option<&'static Self> {
        match item {
            Item::Enum(e) if j == WHOLE => Some(e),
            _ => None,
        }
    }
}

impl Located for TraitDef {
    fn find(item: &'static Item, j: u32) -> Option<&'static Self> {
        match item {
            Item::Trait(t) if j == WHOLE => Some(t),
            _ => None,
        }
    }
}

/// A static's declaration at (item, member).
fn static_at(item: &'static Item, j: u32) -> Option<&'static rokc_ast::StaticDef> {
    match item {
        Item::Static(s) if j == WHOLE => Some(s),
        Item::Extern(b) => match b.items.get(j as usize)? {
            ExternItem::Static(s) => Some(s),
            ExternItem::Fn(_) => None,
        },
        _ => None,
    }
}

impl Located for Ty {
    fn find(item: &'static Item, j: u32) -> Option<&'static Self> {
        static_at(item, j).map(|s| &s.ty)
    }
}

impl Located for Expr {
    fn find(item: &'static Item, j: u32) -> Option<&'static Self> {
        static_at(item, j)?.value.as_ref()
    }
}

fn enc_refs<'a, K: Codec + Ord + 'a, T: 'a>(
    m: impl IntoIterator<Item = (&'a K, &'a &'a T)>,
    k: Kind,
    at: &Locs,
    e: &mut Enc,
) -> Option<()> {
    let mut v: Vec<(&K, &&T)> = m.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(b.0));
    e.u(v.len() as u64);
    for (key, r) in v {
        key.enc(e);
        at.enc(k, *r, e)?;
    }
    Some(())
}

fn dec_refs<K: Codec + Eq + std::hash::Hash, T: Located>(
    p: &'static Program,
    d: &mut Dec<'_>,
) -> Option<HashMap<K, &'static T>> {
    let n = d.u()?;
    let mut m = HashMap::new();
    for _ in 0..n {
        let key = K::dec(d)?;
        m.insert(key, T::read(p, d)?);
    }
    Some(m)
}

fn enc_list<T>(v: &[(impl Codec, &T)], k: Kind, at: &Locs, e: &mut Enc) -> Option<()> {
    e.u(v.len() as u64);
    for (id, r) in v {
        id.enc(e);
        at.enc(k, *r, e)?;
    }
    Some(())
}

fn dec_list<I: Codec, T: Located>(
    p: &'static Program,
    d: &mut Dec<'_>,
) -> Option<Vec<(I, &'static T)>> {
    let n = d.u()?;
    (0..n).map(|_| Some((I::dec(d)?, T::read(p, d)?))).collect()
}

// Borrow-free data first as its own bytes (`dec_plain`, decoded in
// parallel), then the borrows. Leaf indexes, `auto_memo`,
// `bound_methods` / `bound_traits` and root flags restart per stage.
fn enc_collected(c: &Collected<'_>, at: &Locs, e: &mut Enc) -> Option<()> {
    let t = &c.t;
    enc_blob(e, |e| t.fn_table.enc(e));
    enc_blob(e, |e| t.method_info.enc(e));
    enc_blob(e, |e| enc_plain(c, e));
    enc_refs(&t.defs.generic_structs, Kind::Struct, at, e)?;
    enc_refs(&t.defs.generic_unions, Kind::Union, at, e)?;
    enc_refs(&t.defs.generic_enums, Kind::Enum, at, e)?;
    enc_refs(&t.generic_fns, Kind::Fn, at, e)?;
    e.u(t.decls.len() as u64);
    for d in &t.decls {
        match d {
            MethodOrFn::Fn(f) => {
                e.u(0);
                at.enc(Kind::Fn, *f, e)?;
            }
            MethodOrFn::Method {
                fn_def,
                self_ty,
                module,
                key,
                subst,
            } => {
                match fn_def {
                    Cow::Borrowed(f) => {
                        e.u(1);
                        at.enc(Kind::Fn, *f, e)?;
                    }
                    Cow::Owned(f) => {
                        e.u(2);
                        f.enc(e);
                    }
                }
                self_ty.enc(e);
                module.enc(e);
                key.enc(e);
                subst.enc(e);
            }
        }
    }
    enc_list(&c.enum_decls, Kind::Enum, at, e)?;
    enc_list(&c.struct_decls, Kind::Struct, at, e)?;
    e.u(c.static_asts.len() as u64);
    for (ty, module) in &c.static_asts {
        at.enc(Kind::Ty, *ty, e)?;
        module.enc(e);
    }
    enc_refs(&c.trait_defs, Kind::Trait, at, e)
}

/// The plain part but `fn_table` and `method_info` (bytes of their own).
fn enc_plain(c: &Collected<'_>, e: &mut Enc) {
    let t = &c.t;
    t.extern_fns.enc(e);
    t.method_table.enc(e);
    t.key_of.enc(e);
    t.infer_ret.enc(e);
    t.may_diverge.enc(e);
    let defs = &t.defs;
    defs.structs.enc(e);
    defs.struct_table.enc(e);
    defs.enums.enc(e);
    defs.enum_table.enc(e);
    defs.defaulted_leaves.enc(e);
    t.consts.enc(e);
    t.intrinsics.enc(e);
    t.generic_intrinsics.enc(e);
    t.extern_fn_ids.enc(e);
    t.rok_externs.enc(e);
    t.c_variadic.enc(e);
    t.c_defs.enc(e);
    t.stdcall_fns.enc(e);
    t.sink.enc(e);
    t.item_visibility.enc(e);
    t.field_vis.enc(e);
    t.unsafe_fns.enc(e);
    t.neg_auto.enc(e);
    t.trait_info.enc(e);
    t.statics.enc(e);
    t.mut_statics.enc(e);
    t.extern_statics.enc(e);
    t.literal_shapes.enc(e);
    t.names.enc(e);
    t.non_exhaustive.enc(e);
    t.sealed.enc(e);
    t.fn_marks.enc(e);
    t.marks.enc(e);
    c.early.enc(e);
    c.pre_diags.enc(e);
    c.reexports.enc(e);
    c.inherent_keys.enc(e);
    c.static_defs.enc(e);
    c.lib_decl.enc(e);
    c.link_names.enc(e);
    c.templated.enc(e);
    c.system_fns.enc(e);
    c.no_safepoint.enc(e);
    c.wasm_imports.enc(e);
    c.extern_symbols.enc(e);
    c.exports.enc(e);
    c.prebuilt.enc(e);
    c.inline_copies.enc(e);
    c.c_defs.enc(e);
    c.diverging.enc(e);
}

/// `enc_plain`'s part, the borrows left empty.
#[allow(clippy::cognitive_complexity)] // one line per field
fn dec_plain(d: &mut Dec<'_>) -> Option<Collected<'static>> {
    let mut c = Collected::empty();
    let t = &mut c.t;
    t.extern_fns = Codec::dec(d)?;
    t.method_table = Codec::dec(d)?;
    t.key_of = Codec::dec(d)?;
    t.infer_ret = Codec::dec(d)?;
    t.may_diverge = Codec::dec(d)?;
    t.defs = TypeDefs {
        structs: Codec::dec(d)?,
        struct_table: Codec::dec(d)?,
        enums: Codec::dec(d)?,
        enum_table: Codec::dec(d)?,
        generic_structs: HashMap::new(),
        generic_unions: HashMap::new(),
        generic_enums: HashMap::new(),
        defaulted_leaves: Codec::dec(d)?,
    };
    t.consts = Codec::dec(d)?;
    t.intrinsics = Codec::dec(d)?;
    t.generic_intrinsics = Codec::dec(d)?;
    t.extern_fn_ids = Codec::dec(d)?;
    t.rok_externs = Codec::dec(d)?;
    t.c_variadic = Codec::dec(d)?;
    t.c_defs = Codec::dec(d)?;
    t.stdcall_fns = Codec::dec(d)?;
    t.sink = Codec::dec(d)?;
    t.item_visibility = Codec::dec(d)?;
    t.field_vis = Codec::dec(d)?;
    t.unsafe_fns = Codec::dec(d)?;
    t.neg_auto = Codec::dec(d)?;
    t.trait_info = Codec::dec(d)?;
    t.statics = Codec::dec(d)?;
    t.mut_statics = Codec::dec(d)?;
    t.extern_statics = Codec::dec(d)?;
    t.literal_shapes = Codec::dec(d)?;
    t.names = Codec::dec(d)?;
    t.non_exhaustive = Codec::dec(d)?;
    t.sealed = Codec::dec(d)?;
    t.fn_marks = Codec::dec(d)?;
    t.marks = Codec::dec(d)?;
    c.early = Codec::dec(d)?;
    c.pre_diags = Codec::dec(d)?;
    c.reexports = Codec::dec(d)?;
    c.inherent_keys = Codec::dec(d)?;
    c.static_defs = Codec::dec(d)?;
    c.lib_decl = Codec::dec(d)?;
    c.link_names = Codec::dec(d)?;
    c.templated = Codec::dec(d)?;
    c.system_fns = Codec::dec(d)?;
    c.no_safepoint = Codec::dec(d)?;
    c.wasm_imports = Codec::dec(d)?;
    c.extern_symbols = Codec::dec(d)?;
    c.exports = Codec::dec(d)?;
    c.prebuilt = Codec::dec(d)?;
    c.inline_copies = Codec::dec(d)?;
    c.c_defs = Codec::dec(d)?;
    c.diverging = Codec::dec(d)?;
    Some(c)
}

/// The borrows `enc_collected` wrote after the plain part, into `c`.
fn dec_borrows(p: &'static Program, d: &mut Dec<'_>, c: &mut Collected<'static>) -> Option<()> {
    c.t.defs.generic_structs = dec_refs(p, d)?;
    c.t.defs.generic_unions = dec_refs(p, d)?;
    c.t.defs.generic_enums = dec_refs(p, d)?;
    c.t.generic_fns = dec_refs(p, d)?;
    let decls = d.u()?;
    c.t.decls = (0..decls)
        .map(|_| {
            let fn_def = match d.u()? {
                0 => return Some(MethodOrFn::Fn(FnDef::read(p, d)?)),
                1 => Cow::Borrowed(FnDef::read(p, d)?),
                2 => Cow::Owned(<FnDef as Codec>::dec(d)?),
                _ => return None,
            };
            Some(MethodOrFn::Method {
                fn_def,
                self_ty: Codec::dec(d)?,
                module: Codec::dec(d)?,
                key: Codec::dec(d)?,
                subst: Codec::dec(d)?,
            })
        })
        .collect::<Option<_>>()?;
    c.enum_decls = dec_list(p, d)?;
    c.struct_decls = dec_list(p, d)?;
    let statics = d.u()?;
    c.static_asts = (0..statics)
        .map(|_| {
            let ty = Ty::read(p, d)?;
            let module = String::dec(d)?;
            Some((ty, module))
        })
        .collect::<Option<_>>()?;
    c.trait_defs = dec_refs(p, d)?;
    Some(())
}

// The leaf indexes are left out (rebuilt on the first lookup) and so is
// `uses`: the next stage takes the compile's own table.
impl Codec for TypeNames {
    fn enc(&self, e: &mut Enc) {
        self.names.enc(e);
        self.traits.enc(e);
        self.auto_traits.enc(e);
        self.lang.enc(e);
        self.variants.enc(e);
        self.reexports.enc(e);
        self.visibility.enc(e);
        self.mod_vis.enc(e);
        self.diags.enc(e);
        self.generic_aliases.enc(e);
        self.plain_aliases.enc(e);
        self.alias_depth.enc(e);
        self.broken_aliases.enc(e);
        self.alias_uses.enc(e);
        self.unknown_spans.enc(e);
        self.bare_generics.enc(e);
        self.canon_memo.enc(e);
        self.consts.enc(e);
        self.value_reexports.enc(e);
    }
    fn dec(d: &mut Dec<'_>) -> Option<Self> {
        let mut n = Self::empty();
        n.names = Codec::dec(d)?;
        n.traits = Codec::dec(d)?;
        n.auto_traits = Codec::dec(d)?;
        n.lang = Codec::dec(d)?;
        n.variants = Codec::dec(d)?;
        n.reexports = Codec::dec(d)?;
        n.visibility = Codec::dec(d)?;
        n.mod_vis = Codec::dec(d)?;
        n.diags = Codec::dec(d)?;
        n.generic_aliases = Codec::dec(d)?;
        n.plain_aliases = Codec::dec(d)?;
        n.alias_depth = Codec::dec(d)?;
        n.broken_aliases = Codec::dec(d)?;
        n.alias_uses = Codec::dec(d)?;
        n.unknown_spans = Codec::dec(d)?;
        n.bare_generics = Codec::dec(d)?;
        n.canon_memo = Codec::dec(d)?;
        n.consts = Codec::dec(d)?;
        n.value_reexports = Codec::dec(d)?;
        Some(n)
    }
}

codec_struct!(ClosureSink {
    extras,
    ref_out,
    next_id,
    mono_cache,
    mono_sigs,
    mono_depth,
    uninferred,
    any_tags,
    any_dispatch,
    projecting,
    thunks,
    instances,
    instance_warns,
    diagnostics,
    infer_vars,
    infer_depth,
    infer_floor,
    fn_origin,
    track_caller,
    const_lowering,
    const_stack,
    inferred_ret,
    inferred_fns,
    bodies_done,
    sig_pass,
    call_sites,
    recursive_ret,
    tentative_ret,
    params,
    failed,
    bodies,
    assumed,
    const_names,
    dropped_at,
});
codec_struct!(InstanceWarns { instances, warns });
codec_struct!(Impl {
    trait_key,
    trait_args,
    assoc,
    generics,
    where_clause,
    self_ty,
    module,
    methods,
    consts,
    concrete,
});
codec_struct!(Impls {
    all,
    by_target,
    by_trait,
    generic_of
});
codec_struct!(ShapeHandler {
    shape,
    name,
    priority,
    span
});
codec_enum!(MethodTarget {
    Struct(x0),
    Enum(x0),
    Mangled(x0)
});
codec_enum!(Intrinsic {
    Prim(x0),
    Convert,
    BufNew,
    BufGet,
    BufSet,
    BufSame,
    BufErase,
    BufCast,
    PtrRead { volatile },
    PtrWrite { volatile },
    SizeOf,
    AlignOf,
    AddrOf,
    DropInPlace,
    Atomic(x0),
    AnyCast,
    MethodTable,
    MlirOp,
    MlirAttr,
    Transfer,
    TransferNeeded,
    HandOver,
    TransferKeep,
    TransferPlain,
    TransferFlat,
    TransferVisit,
    FieldTable,
    Shown,
});
