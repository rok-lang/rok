//! Incremental body lowering (spec 9.2.16): a declaration's lowered
//! body, with everything its lowering added to the shared state
//! (closures, instances, type instances, `any` tags, warnings), is kept
//! as a unit and replayed in a later build instead of lowered again.
//!
//! A unit is keyed by `global_key` (target, flags, operators, lang slots)
//! and the declaration with its body and positions. Every read a lowering
//! makes is logged: interface reads (`Read`) with a fingerprint of the
//! answer, shared-state reads (`Lookup`) with the answer. A unit replays
//! when every read answers alike and the files its spans point into are
//! unchanged; the functions, types and inference variables it made are
//! renumbered from this build's counters. A miss just lowers again.
//!
//! Only plain bodies (not generic, `const` or a trait default) make units.
//! An inferred return type stays in the interface (`interface_key`) and
//! the unit records it. A lowering that errors or touches unlogged state
//! makes no unit.

use std::collections::{HashMap, HashSet};

use rokc_ast::{FnDef, ImplItem, Item, Program};
use rokc_core::codec::{Codec, Dec, Enc, IdKind};
use rokc_core::{codec_enum, codec_struct};
use rokc_diagnostic::{RokDiagnostic, Span};
use rokc_ir_common::FunctionId;

use super::expr::AnyDispatch;
use super::{
    AliasUse, Env, FnSig, GenericMethod, MethodInfo, MethodOrFn, RefOut, TraitInfo, mangle_inst,
    ty_mangle,
};
use crate::{EnumId, HirEnumDef, HirFunction, HirStructDef, HirTy, StructId};

/// Where an embedder keeps units between builds, and the errors of
/// generic body checks (`bounds`).
pub trait BodyCache {
    fn get(&self, key: u64) -> Option<Unit>;
    fn put(&self, key: u64, unit: &Unit);
    fn get_check(&self, key: u64) -> Option<Vec<RokDiagnostic>>;
    fn put_check(&self, key: u64, errors: &[RokDiagnostic]);
}

/// A `BodyCache` for `HirInput::bodies`.
#[derive(Clone)]
pub struct Bodies(pub std::sync::Arc<dyn BodyCache + Send + Sync>);

impl std::fmt::Debug for Bodies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bodies")
    }
}

/// How many bodies were lowered and how many of them replayed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BodyReuse {
    pub replayed: usize,
    pub lowered: usize,
}

/// The counters of the shared state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    next_id: u32,
    structs: u32,
    enums: u32,
    any_tags: u32,
    any_dispatch: u32,
    extras: u32,
    infer_vars: u32,
}

/// A read of the shared state, with its answer.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Lookup {
    Mono(String, Vec<HirTy>, Option<FunctionId>),
    /// A type by its path (not an instance's key).
    Struct(String, Option<StructId>),
    Enum(String, Option<EnumId>),
    /// The instance of a generic type over these arguments.
    StructInst(String, Vec<HirTy>, Option<StructId>),
    EnumInst(String, Vec<HirTy>, Option<EnumId>),
    /// Every instance of a generic type.
    StructInsts(String, Vec<StructId>),
    EnumInsts(String, Vec<EnumId>),
    Thunk(Key, Option<FunctionId>),
    AnyTag(HirTy, Option<u32>),
    /// `(recv, trait, method, parameter and return types, answer)`.
    AnyDispatch(HirTy, String, String, Vec<HirTy>, Option<FunctionId>),
    /// `inferred_ret`'s entry (`Some(None)`: being inferred).
    #[allow(clippy::option_option)]
    InferredRet(FunctionId, Option<Option<HirTy>>),
}

/// A read of the program's interface (the tables collected from its
/// items), answered by `Incr::answer`'s fingerprint.
#[derive(Clone, Debug)]
pub(super) enum Read {
    /// `fn_table`: a declaration's signature, or none.
    Sig(String),
    /// `generic_fns`: a generic function's template.
    Generic(String),
    /// Function and generic function keys by last segment.
    FnLeaf(String),
    ExternFn(String),
    /// The concrete methods of a name on a type.
    Methods(HirTy, String),
    /// Every concrete method.
    AllMethods,
    /// Every generic method template.
    AllGenericMethods,
    MethodInfo(FunctionId),
    GenericMethods(String, String),
    KeyOf(FunctionId),
    InferRet(FunctionId),
    MayDiverge(Span),
    Intrinsic(FunctionId),
    GenericIntrinsic(String),
    /// Membership in one of the sets of functions (`FnSet`).
    FnIn(u8, FunctionId),
    ItemVis(String),
    FieldVis(String),
    Const(String),
    ConstLeaf(String),
    Static(String),
    StaticLeaf(String),
    MutStatic(String),
    ExternStatic(String),
    TraitInfo(String),
    NegAuto(String, String),
    NonExhaustive(String),
    /// A function's `#[deprecated]` / `#[deprecated_safe]`.
    FnMark(FunctionId),
    /// A field's, const's or static's.
    Mark(String),
    BoundMethod(String, String),
    BoundTrait(String),
    /// A declaration (its signature, body and impl).
    Decl(FunctionId),
    /// The generic body at a span failed its check (`bounds::in_failed`).
    CheckFailed(Span),
    /// A module's `use` entries.
    Uses(String),
    /// Every generic impl.
    GenericImpls,
    /// The generic impls of a trait.
    GenericImplsOf(String),
    /// The generic impls with a method.
    GenericImplsWith(String),
    /// The impls with an associated const.
    ImplConsts(String),
    /// The trait refs of the concrete impls on a type.
    ImplsOn(HirTy),
    /// Those of one trait.
    ImplsOnOf(HirTy, String),
    /// The targets of a trait's concrete impls.
    ImplsOf(String),
    /// The type key a path names from a module (`TypeNames::canon`).
    Canon(Vec<String>, String),
    /// A type-namespace key exists (an item or a re-export).
    Type(String),
    TypeLeaf(String),
    TypeVis(String),
    ModVis(String),
    Reexport(String),
    Trait(String),
    Traits,
    Variant(String),
    VariantLeaf(String),
    NameConst(String),
    NameConstLeaf(String),
    ValueReexport(String),
    GenericAlias(String),
    AnyGenericAlias,
    PlainAlias(String),
    DefaultedLeaf(String),
    StructDef(StructId),
    EnumDef(EnumId),
    GenericStruct(String),
    GenericEnum(String),
    GenericUnion(String),
}

/// An `InstanceWarns` change.
#[derive(Clone, Debug)]
pub(super) enum WarnEvent {
    Instance(Span, FunctionId),
    Warn(Span, RokDiagnostic, FunctionId),
}

/// A key of `mono_cache`, `thunks` or `unknown_spans`.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Key {
    Mono(String, Vec<HirTy>),
    /// `throws::thunk_key`'s parts.
    Thunk(&'static str, Option<FunctionId>, HirTy, HirTy),
    Unknown(String),
}

impl Key {
    /// The table's key.
    fn mono(name: &str, args: &[HirTy]) -> (String, Vec<String>) {
        (name.to_owned(), args.iter().map(ty_mangle).collect())
    }
}

/// One declaration's lowering and what it added: what a replay checks
/// (read with the ids as they were), and the rest (read renumbered).
pub struct Unit {
    head: Head,
    body: Vec<u8>,
    /// The files `body`'s spans name (encoding), or the old → new file
    /// ids to read it with (decoding).
    files: Vec<u32>,
}

struct Head {
    start: Counts,
    end: Counts,
    lookups: Vec<Lookup>,
    /// Its reads of the interface, each with its answer's fingerprint.
    reads: Vec<(Read, u64)>,
    /// Files its spans point into outside its body: path, the end of
    /// the line its furthest span there ends on, and the hash of the
    /// text up to it (the spans' positions, and what they show).
    deps: Vec<(String, u32, u64)>,
    /// The generic type each new struct, then enum, instantiates.
    bases: Vec<String>,
    /// The functions and types made before it that it names: kind, id,
    /// build-independent name (a function's name, a type's
    /// `crate::name_type`).
    named: Vec<(u8, u32, String)>,
}

struct Body {
    func: HirFunction,
    extras: Vec<HirFunction>,
    structs: Vec<HirStructDef>,
    enums: Vec<HirEnumDef>,
    /// The final entries of each function it touched.
    entries: Vec<Entries>,
    /// The final `mono_cache` / `thunks` entry at each key it set.
    keyed: Vec<(Key, Option<FunctionId>)>,
    unknown_spans: Vec<(String, Span)>,
    any_tags: Vec<(HirTy, u32)>,
    any_dispatch: Vec<AnyDispatch>,
    /// `let` inference variables (their numbers name type instances).
    infer_vars: Vec<super::infer::Var>,
    warns: Vec<WarnEvent>,
    diagnostics: Vec<RokDiagnostic>,
    name_diags: Vec<RokDiagnostic>,
    alias_uses: Vec<AliasUse>,
}

/// The entries of one function in the tables by function (`None`:
/// none).
#[derive(Clone)]
struct Entries {
    id: FunctionId,
    ref_out: Option<RefOut>,
    mono_sig: Option<FnSig>,
    fn_origin: Option<(bool, bool)>,
    track_caller: bool,
    #[allow(clippy::option_option)] // an entry of a table of options
    inferred_ret: Option<Option<HirTy>>,
    /// How far past the unit's first function id its lowering started.
    inferred_fn: Option<(u32, HirFunction)>,
}

codec_struct!(Counts {
    next_id,
    structs,
    enums,
    any_tags,
    any_dispatch,
    extras,
    infer_vars
});
codec_enum!(Lookup {
    Mono(x0, x1, x2),
    Struct(x0, x1),
    Enum(x0, x1),
    StructInst(x0, x1, x2),
    EnumInst(x0, x1, x2),
    StructInsts(x0, x1),
    EnumInsts(x0, x1),
    Thunk(x0, x1),
    AnyTag(x0, x1),
    AnyDispatch(x0, x1, x2, x3, x4),
    InferredRet(x0, x1),
});
codec_enum!(Read {
    Sig(x0),
    Generic(x0),
    FnLeaf(x0),
    ExternFn(x0),
    Methods(x0, x1),
    AllMethods,
    AllGenericMethods,
    MethodInfo(x0),
    GenericMethods(x0, x1),
    KeyOf(x0),
    InferRet(x0),
    MayDiverge(x0),
    Intrinsic(x0),
    GenericIntrinsic(x0),
    FnIn(x0, x1),
    ItemVis(x0),
    FieldVis(x0),
    Const(x0),
    ConstLeaf(x0),
    Static(x0),
    StaticLeaf(x0),
    MutStatic(x0),
    ExternStatic(x0),
    TraitInfo(x0),
    NegAuto(x0, x1),
    NonExhaustive(x0),
    FnMark(x0),
    Mark(x0),
    BoundMethod(x0, x1),
    BoundTrait(x0),
    Decl(x0),
    CheckFailed(x0),
    Uses(x0),
    GenericImpls,
    GenericImplsOf(x0),
    GenericImplsWith(x0),
    ImplConsts(x0),
    ImplsOn(x0),
    ImplsOnOf(x0, x1),
    ImplsOf(x0),
    Canon(x0, x1),
    Type(x0),
    TypeLeaf(x0),
    TypeVis(x0),
    ModVis(x0),
    Reexport(x0),
    Trait(x0),
    Traits,
    Variant(x0),
    VariantLeaf(x0),
    NameConst(x0),
    NameConstLeaf(x0),
    ValueReexport(x0),
    GenericAlias(x0),
    AnyGenericAlias,
    PlainAlias(x0),
    DefaultedLeaf(x0),
    StructDef(x0),
    EnumDef(x0),
    GenericStruct(x0),
    GenericEnum(x0),
    GenericUnion(x0),
});
codec_enum!(WarnEvent {
    Instance(x0, x1),
    Warn(x0, x1, x2)
});
codec_enum!(Key {
    Mono(x0, x1),
    Thunk(x0, x1, x2, x3),
    Unknown(x0)
});
codec_struct!(FnSig {
    id,
    ret_ty,
    param_defaults,
    param_tys,
    param_names,
    param_refs,
    variadic
});
codec_struct!(RefOut {
    ret,
    refs,
    deferred
});
codec_struct!(AnyDispatch {
    id,
    recv_ty,
    trait_key,
    method,
    params,
    ret,
    span
});
codec_struct!(Entries {
    id,
    ref_out,
    mono_sig,
    fn_origin,
    track_caller,
    inferred_ret,
    inferred_fn
});
codec_struct!(Head {
    start,
    end,
    lookups,
    reads,
    deps,
    bases,
    named
});
codec_struct!(Body {
    func,
    extras,
    structs,
    enums,
    entries,
    keyed,
    unknown_spans,
    any_tags,
    any_dispatch,
    infer_vars,
    warns,
    diagnostics,
    name_diags,
    alias_uses,
});

impl Codec for Unit {
    fn enc(&self, e: &mut Enc) {
        self.head.enc(e);
        e.bytes(&self.body);
        for f in &self.files {
            if !e.files.contains(f) {
                e.files.push(*f);
            }
        }
    }
    fn dec(d: &mut Dec<'_>) -> Option<Self> {
        let head = Head::dec(d)?;
        let body = d.bytes()?.to_vec();
        Some(Self {
            head,
            body,
            files: d.files.map(<[u32]>::to_vec).unwrap_or_default(),
        })
    }
}

/// A lowering being recorded.
struct Rec {
    lookups: Vec<Lookup>,
    /// Keys looked up so far (the first answer is the one that counts).
    seen: HashSet<String>,
    reads: Vec<Read>,
    /// The reads so far, encoded.
    read_seen: HashSet<Vec<u8>>,
    warns: Vec<WarnEvent>,
    /// Functions whose entries in the tables by function this lowering
    /// may have set or removed, and keys of the tables by key.
    touched: Vec<FunctionId>,
    keys: Vec<Key>,
}

thread_local! {
    static REC: std::cell::RefCell<Option<Rec>> = const { std::cell::RefCell::new(None) };
}

/// Log a read of the shared state (while a unit is recorded): `key`
/// names what was read, `answer` makes the log entry.
pub(super) fn log(key: impl FnOnce() -> String, answer: impl FnOnce() -> Lookup) {
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            if rec.seen.insert(key()) {
                rec.lookups.push(answer());
            }
        }
    });
}

/// Log a read of the interface (while a unit is recorded).
pub(super) fn read(r: impl FnOnce() -> Read) {
    REC.with(|rec| {
        if let Some(rec) = rec.borrow_mut().as_mut() {
            let r = r();
            let mut e = Enc::new(true);
            r.enc(&mut e);
            if rec.read_seen.insert(e.buf) {
                rec.reads.push(r);
            }
        }
    });
}

/// `log` of a struct (`enum_`: enum) id lookup by path.
pub(super) fn log_type(enum_: bool, key: &str, found: Option<u32>) {
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            if !rec.seen.insert(format!("t{enum_}|{key}")) {
                return;
            }
            rec.lookups.push(if enum_ {
                Lookup::Enum(key.to_owned(), found.map(EnumId))
            } else {
                Lookup::Struct(key.to_owned(), found.map(StructId))
            });
        }
    });
}

/// Record that the entries of function `id` may change.
pub(super) fn touch(id: FunctionId) {
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            rec.touched.push(id);
        }
    });
}

/// Record that the entry at a key may change.
pub(super) fn touch_key(k: impl FnOnce() -> Key) {
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            rec.keys.push(k());
        }
    });
}

/// Log an `InstanceWarns` change.
pub(super) fn log_warn(e: impl FnOnce() -> WarnEvent) {
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            rec.warns.push(e());
        }
    });
}

/// The incremental state of one `lower_full`.
pub(super) struct Incr<'a> {
    cache: &'a dyn BodyCache,
    /// `interface_key` (generic body checks).
    interface: u64,
    /// `global_key` (units).
    global: u64,
    /// Per declaration: plain (see the module doc).
    plain: Vec<bool>,
    /// The kept library stage's key and the declarations it made (the
    /// first ones): theirs are named by it.
    library: Option<(u64, usize)>,
    /// Fingerprints of the reads answered so far (by encoding).
    answers: std::cell::RefCell<HashMap<Vec<u8>, Option<u64>>>,
    /// Fingerprints of the impls by index (`Impls::all`), once taken.
    impl_fps: std::cell::RefCell<HashMap<usize, Option<u64>>>,
    /// The generic impls by method, and every impl by associated const.
    impls_by: std::cell::OnceCell<[HashMap<String, Vec<usize>>; 2]>,
    /// `fn_table` keys by `stable_name`, where it differs (`None`: two).
    raw_keys: std::cell::OnceCell<HashMap<String, Option<String>>>,
    /// Per file: each line's end and the hash of the text up to it.
    lines: std::cell::RefCell<HashMap<u32, Vec<(u32, u64)>>>,
    names: std::cell::RefCell<Names>,
    pub reuse: std::cell::Cell<BodyReuse>,
}

/// The names of the functions and types made so far, both ways (a name
/// given twice names neither).
#[derive(Default)]
struct Names {
    /// Functions, structs, enums looked at so far.
    done: [usize; 3],
    decls: bool,
    by_id: HashMap<(u8, u32), String>,
    by_name: HashMap<(u8, String), Option<u32>>,
}

/// `name` with each type id `ty_mangle` writes in the target of a
/// method key (`Struct12` in `Struct12__len`, `Enum3`) replaced by
/// `f(enum, id)` (`None`: no name).
fn map_type_ids(name: &str, f: impl Fn(bool, u32) -> Option<String>) -> Option<String> {
    let Some(head) = name.find("__") else {
        return Some(name.to_owned());
    };
    let mut out = String::with_capacity(name.len());
    let mut rest = &name[..head];
    while let Some(i) = rest
        .find("Struct")
        .into_iter()
        .chain(rest.find("Enum"))
        .min()
    {
        let enum_ = rest[i..].starts_with("Enum");
        let tag = if enum_ { "Enum" } else { "Struct" };
        let digits = rest[i + tag.len()..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        let end = i + tag.len() + digits;
        out.push_str(&rest[..i]);
        match rest[i + tag.len()..end].parse::<u32>() {
            Ok(id) => out.push_str(&f(enum_, id)?),
            Err(_) => out.push_str(&rest[i..end]),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out.push_str(&name[head..]);
    Some(out)
}

/// `name` with the types its method key names by id named by their own
/// names: alike in every build.
fn stable_name(name: &str) -> String {
    map_type_ids(name, |enum_, id| {
        Some(crate::type_name(enum_, id).map_or_else(
            || format!("{}{id}", if enum_ { "Enum" } else { "Struct" }),
            |t| format!("{{{t}}}"),
        ))
    })
    .unwrap_or_default()
}

impl Names {
    fn update(&mut self, env: &Env<'_>) {
        let mut add = |k: u8, id: u32, name: String| {
            let name = if k == 0 { stable_name(&name) } else { name };
            self.by_name
                .entry((k, name.clone()))
                .and_modify(|v| *v = None)
                .or_insert(Some(id));
            self.by_id.insert((k, id), name);
        };
        // The declarations, by `fn_table` key.
        if !self.decls {
            self.decls = true;
            let mut keys: Vec<(&FunctionId, &String)> = env.key_of.iter().collect();
            keys.sort();
            for (id, k) in keys {
                add(0, id.0, k.clone());
            }
        }
        let extras = env.sink.extras.borrow();
        for f in extras.iter().skip(self.done[0]) {
            add(0, f.id.0, f.name.clone());
        }
        self.done[0] = extras.len();
        for (k, len) in [
            (1u8, env.defs.structs.borrow().len()),
            (2, env.defs.enums.borrow().len()),
        ] {
            let from = self.done[usize::from(k)];
            for id in from..len {
                let id = u32::try_from(id).unwrap_or(u32::MAX);
                if let Some(name) = crate::type_name(k == 2, id) {
                    add(k, id, name);
                }
            }
            self.done[usize::from(k)] = len;
        }
    }

    fn name_of(&self, k: u8, id: u32) -> Option<&str> {
        self.by_id.get(&(k, id)).map(String::as_str)
    }

    fn id_of(&self, k: u8, name: &str) -> Option<u32> {
        self.by_name.get(&(k, name.to_owned())).copied().flatten()
    }
}

impl<'a> Incr<'a> {
    pub(super) fn new(
        env: &Env<'_>,
        cache: &'a dyn BodyCache,
        programs: &[&Program],
        raw_uses: &[(String, rokc_core::Visibility, rokc_ast::UseTree)],
        input: &crate::HirInput,
        lib_decls: usize,
    ) -> Self {
        let plain: Vec<bool> = (0..env.decls.len()).map(|i| is_plain(env, i)).collect();
        // An inferred return is part of the interface (callers and
        // generic bodies read it): its body stays in the key.
        let bodies: HashSet<(&str, Span)> = env
            .decls
            .iter()
            .zip(&plain)
            .enumerate()
            .filter(|(i, (_, p))| {
                **p && !env
                    .infer_ret
                    .contains(&FunctionId(u32::try_from(*i).unwrap_or(u32::MAX)))
            })
            .map(|(_, (d, _))| (d.def().name.as_str(), d.def().span))
            .collect();
        let interface = interface_key(env, &bodies, programs, raw_uses, input);
        Self {
            cache,
            interface,
            global: global_key(env, input),
            plain,
            library: input.library.map(|k| (k, lib_decls)),
            answers: std::cell::RefCell::new(HashMap::new()),
            impl_fps: std::cell::RefCell::new(HashMap::new()),
            impls_by: std::cell::OnceCell::new(),
            raw_keys: std::cell::OnceCell::new(),
            lines: std::cell::RefCell::new(HashMap::new()),
            names: std::cell::RefCell::new(Names::default()),
            reuse: std::cell::Cell::new(BodyReuse::default()),
        }
    }

    /// The end of the line of file `id` that offset `end` is on (`exact`:
    /// `end` must be one), and the hash of the text up to it.
    fn prefix(&self, env: &Env<'_>, id: u32, end: u32, exact: bool) -> Option<(u32, u64)> {
        let mut lines = self.lines.borrow_mut();
        let table = lines.entry(id).or_insert_with(|| {
            let Some((_, text)) = env.sources.file(id) else {
                return Vec::new();
            };
            let (mut h, mut at) = (0u64, 0usize);
            let mut out = Vec::new();
            for line in text.split_inclusive('\n') {
                h = rokc_core::hash::fast_add(h, line.as_bytes());
                at += line.len();
                out.push((u32::try_from(at).unwrap_or(u32::MAX), h));
            }
            out
        });
        let k = table.partition_point(|(at, _)| *at < end);
        let found = *table.get(k)?;
        (!exact || found.0 == end).then_some(found)
    }

    /// The errors of checking generic body `f` (`owner`: its impl's
    /// target or its trait), from the store or from `check`. They follow
    /// from the interface and the body (positions in), so they are kept
    /// unless one points outside the body.
    pub(super) fn checked(
        &self,
        env: &Env<'_>,
        f: &FnDef,
        owner: &[u8],
        check: impl FnOnce() -> Vec<RokDiagnostic>,
    ) -> Vec<RokDiagnostic> {
        let mut e = Enc::new(true);
        f.enc(&mut e);
        let path = env.sources.path(f.span.file).unwrap_or_default();
        let key = [b"check".as_slice(), path.as_bytes(), owner, &e.buf]
            .iter()
            .fold(self.interface, |h, part| rokc_core::hash::fast_add(h, part));
        if let Some(errors) = self.cache.get_check(key) {
            return errors;
        }
        let errors = check();
        let inside =
            |s: &Span| s.file == f.span.file && s.start >= f.span.start && s.end <= f.span.end;
        let local = errors.iter().all(|d| {
            d.primary
                .iter()
                .map(|l| &l.span)
                .chain(d.secondary.iter().map(|l| &l.span))
                .all(inside)
        });
        if local {
            self.cache.put_check(key, &errors);
        }
        errors
    }

    /// Lower declaration `i` (not a stub): replayed from its unit, or
    /// lowered and kept as one.
    pub(super) fn lower(&self, env: &Env<'_>, i: usize) -> HirFunction {
        if !self.plain[i] {
            return super::collect::lower_decl(env, i, false);
        }
        let def = env.decls[i].def();
        let mut e = Enc::new(true);
        match self.library {
            // The stage's key names its declarations.
            Some((k, n)) if i < n => {
                e.u(k);
                e.u(i as u64);
            }
            _ => def.enc(&mut e),
        }
        let path = env.sources.path(def.span.file).unwrap_or_default();
        let id = FunctionId(u32::try_from(i).unwrap_or(u32::MAX));
        let name = env
            .key_of
            .get(&id)
            .map(|k| stable_name(k))
            .unwrap_or_default();
        let key = [name.as_bytes(), path.as_bytes(), &e.buf]
            .iter()
            .fold(self.global, |h, part| rokc_core::hash::fast_add(h, part));
        let mut r = self.reuse.get();
        r.lowered += 1;
        if let Some(f) = self.cache.get(key).and_then(|u| self.replay(env, i, &u)) {
            r.replayed += 1;
            self.reuse.set(r);
            return f;
        }
        self.reuse.set(r);
        let start = counts(env);
        let before = Sizes::of(env);
        REC.with(|r| {
            *r.borrow_mut() = Some(Rec {
                lookups: Vec::new(),
                seen: HashSet::new(),
                reads: Vec::new(),
                read_seen: HashSet::new(),
                warns: Vec::new(),
                touched: Vec::new(),
                keys: Vec::new(),
            });
        });
        let f = super::collect::lower_decl(env, i, false);
        let rec = REC.with(|r| r.borrow_mut().take());
        let unit = rec.and_then(|rec| self.record(env, i, &f, start, &before, rec));
        if let Some(unit) = unit {
            self.cache.put(key, &unit);
        }
        f
    }

    /// The unit of declaration `i` lowered as `f` from `start`, unless
    /// the lowering did more than it logged.
    fn record(
        &self,
        env: &Env<'_>,
        i: usize,
        f: &HirFunction,
        start: Counts,
        before: &Sizes,
        rec: Rec,
    ) -> Option<Unit> {
        let sink = &env.sink;
        let end = counts(env);
        let own = |id: &FunctionId| id.0 >= start.next_id && id.0 < end.next_id;
        let diagnostics = sink.diagnostics.borrow()[before.diagnostics..].to_vec();
        let name_diags = env.names.diags.borrow()[before.name_diags..].to_vec();
        if diagnostics
            .iter()
            .chain(&name_diags)
            .any(RokDiagnostic::is_error)
            || sink.uninferred.get() != before.uninferred
            || !sink.tentative_ret.borrow().is_empty()
            || !sink.recursive_ret.borrow().is_empty()
            // Calls of a duplicated item lower without their checks.
            || !env.duplicated.borrow().is_empty()
        {
            return None;
        }
        // Every table entry it may have set, as it is now.
        let mut touched = rec.touched;
        touched.push(FunctionId(u32::try_from(i).ok()?));
        touched.sort_unstable();
        touched.dedup();
        let entries: Vec<Entries> = touched
            .into_iter()
            .map(|id| {
                let inferred_fn = match sink.inferred_fns.borrow().get(&id) {
                    Some((at, f)) => Some((at.checked_sub(start.next_id)?, f.clone())),
                    None => None,
                };
                Some(Entries {
                    id,
                    ref_out: sink.ref_out.borrow().get(&id).cloned(),
                    mono_sig: sink.mono_sigs.borrow().get(&id).cloned(),
                    fn_origin: sink.fn_origin.borrow().get(&id).copied(),
                    track_caller: sink.track_caller.borrow().contains(&id),
                    inferred_ret: sink.inferred_ret.borrow().get(&id).cloned(),
                    inferred_fn,
                })
            })
            .collect::<Option<_>>()?;
        let mut keyed: Vec<(Key, Option<FunctionId>)> = Vec::new();
        let mut unknown_spans: Vec<(String, Span)> = Vec::new();
        for k in rec.keys {
            if let Key::Unknown(path) = &k {
                if !unknown_spans.iter().any(|(p, _)| p == path) {
                    let at = env.names.unknown_spans.borrow().get(path).copied()?;
                    unknown_spans.push((path.clone(), at));
                }
                continue;
            }
            if keyed.iter().any(|(old, _)| *old == k) {
                continue;
            }
            let v = match &k {
                Key::Mono(name, args) => sink
                    .mono_cache
                    .borrow()
                    .get(&Key::mono(name, args))
                    .copied(),
                Key::Thunk(kind, target, slot, callee) => sink
                    .thunks
                    .borrow()
                    .get(&super::throws::thunk_key(kind, *target, slot, callee))
                    .copied(),
                Key::Unknown(_) => None,
            };
            keyed.push((k, v));
        }
        // Tables that grew past what it touched: something it did is
        // not accounted for.
        let grew = |now: usize, then: usize, mine: usize| now > then + mine;
        let count = |pick: &dyn Fn(&Entries) -> bool| entries.iter().filter(|e| pick(e)).count();
        let monos = keyed
            .iter()
            .filter(|(k, _)| matches!(k, Key::Mono(..)))
            .count();
        if grew(sink.mono_cache.borrow().len(), before.mono_cache, monos)
            || grew(
                sink.thunks.borrow().len(),
                before.thunks,
                keyed.len() - monos,
            )
            || grew(
                sink.ref_out.borrow().len(),
                before.ref_out,
                count(&|e| e.ref_out.is_some()),
            )
            || grew(
                sink.mono_sigs.borrow().len(),
                before.mono_sigs,
                count(&|e| e.mono_sig.is_some()),
            )
            || grew(
                sink.fn_origin.borrow().len(),
                before.fn_origin,
                count(&|e| e.fn_origin.is_some()),
            )
            || grew(
                sink.track_caller.borrow().len(),
                before.track_caller,
                count(&|e| e.track_caller),
            )
            || grew(
                sink.inferred_ret.borrow().len(),
                before.inferred_ret,
                count(&|e| e.inferred_ret.is_some()),
            )
            || grew(
                sink.inferred_fns.borrow().len(),
                before.inferred_fns,
                count(&|e| e.inferred_fn.is_some()),
            )
            || grew(
                env.names.unknown_spans.borrow().len(),
                before.unknown_spans,
                unknown_spans.len(),
            )
        {
            return None;
        }
        // The functions it made are its own, numbered from `start`.
        let extras = sink.extras.borrow()[start.extras as usize..].to_vec();
        if extras.iter().any(|f| !own(&f.id))
            || sink.any_dispatch.borrow()[start.any_dispatch as usize..]
                .iter()
                .any(|d| !own(&d.id))
        {
            return None;
        }
        let structs = env.defs.structs.borrow()[start.structs as usize..].to_vec();
        let enums = env.defs.enums.borrow()[start.enums as usize..].to_vec();
        // Each new type is an instance `base__args` (`mangle_inst`).
        let base = |name: &str, args: &[HirTy]| {
            let tail = mangle_inst("", args);
            name.strip_suffix(&tail).map(str::to_owned)
        };
        let bases: Vec<String> = structs
            .iter()
            .map(|s| base(&s.name, &s.generic_args))
            .chain(enums.iter().map(|e| base(&e.name, &e.generic_args)))
            .collect::<Option<_>>()?;
        let body = Body {
            func: f.clone(),
            extras,
            structs,
            enums,
            entries,
            keyed,
            unknown_spans,
            any_tags: sink.any_tags.borrow()[start.any_tags as usize..].to_vec(),
            any_dispatch: sink.any_dispatch.borrow()[start.any_dispatch as usize..].to_vec(),
            infer_vars: sink.infer_vars.borrow()[start.infer_vars as usize..].to_vec(),
            warns: rec.warns,
            diagnostics,
            name_diags,
            alias_uses: env.names.alias_uses.borrow()[before.alias_uses..].to_vec(),
        };
        // The files its spans point into outside its body.
        let span = env.decls[i].def().span;
        let mut e = Enc::new(true);
        e.home = Some((span.file, span.start, span.end));
        e.ids = Some(Vec::new());
        body.enc(&mut e);
        // Reads of what it made itself follow from its other reads.
        let made = |r: &Read| {
            let mut e = Enc::new(false);
            e.ids = Some(Vec::new());
            r.enc(&mut e);
            e.ids.into_iter().flatten().any(|(k, id)| match k {
                IdKind::Function => (start.next_id..end.next_id).contains(&id),
                IdKind::Struct => (start.structs..end.structs).contains(&id),
                IdKind::Enum => (start.enums..end.enums).contains(&id),
                IdKind::InferVar => true,
            })
        };
        let reads = rec
            .reads
            .into_iter()
            .filter(|r| !made(r))
            .map(|r| match r {
                Read::Sig(k) => Read::Sig(stable_name(&k)),
                r => r,
            })
            .map(|r| Some((self.answer(env, &r)?, r)))
            .map(|a| a.map(|(h, r)| (r, h)))
            .collect::<Option<Vec<_>>>()?;
        let mut head = Head {
            start,
            end,
            lookups: rec.lookups,
            reads,
            deps: Vec::new(),
            bases,
            named: Vec::new(),
        };
        let mut h = Enc::new(true);
        h.home = e.home;
        h.ids = Some(Vec::new());
        head.enc(&mut h);
        let written = e.ids.take().into_iter().chain(h.ids.take()).flatten();
        head.named = self.named(env, start, end, written)?;
        let mut foreign: Vec<(u32, u32)> = Vec::new();
        for (f, end) in e.foreign.into_iter().chain(h.foreign) {
            match foreign.iter_mut().find(|(g, _)| *g == f) {
                Some((_, e)) => *e = (*e).max(end),
                None => foreign.push((f, end)),
            }
        }
        foreign.sort_unstable();
        head.deps = foreign
            .iter()
            .map(|&(id, end)| {
                let (at, hash) = self.prefix(env, id, end, false)?;
                Some((env.sources.path(id)?.to_owned(), at, hash))
            })
            .collect::<Option<_>>()?;
        Some(Unit {
            head,
            body: e.buf,
            files: e.files,
        })
    }

    /// The ids of what was made before `start` that `encodings` (of a
    /// unit made from `start` to `end`) name, with their names.
    fn named(
        &self,
        env: &Env<'_>,
        start: Counts,
        end: Counts,
        written: impl Iterator<Item = (IdKind, u32)>,
    ) -> Option<Vec<(u8, u32, String)>> {
        let mut seen = HashSet::new();
        let mut names = self.names.borrow_mut();
        names.update(env);
        let mut out = Vec::new();
        for (kind, id) in written {
            let (from, to) = match kind {
                IdKind::Function => (start.next_id, end.next_id),
                IdKind::Struct => (start.structs, end.structs),
                IdKind::Enum => (start.enums, end.enums),
                IdKind::InferVar => (start.infer_vars, end.infer_vars),
            };
            let k = (kind_tag(kind), id);
            if id != NO_ID && !(from..to).contains(&id) && seen.insert(k) {
                out.push((k.0, id, names.name_of(k.0, id)?.to_owned()));
            }
        }
        Some(out)
    }

    /// Replay `u` when it holds for this build (see the module doc).
    fn replay(&self, env: &Env<'_>, decl: usize, u: &Unit) -> Option<HirFunction> {
        let sink = &env.sink;
        let head = &u.head;
        for (path, at, hash) in &head.deps {
            let id = env.sources.id_of(path)?;
            if self.prefix(env, id, *at, true) != Some((*at, *hash)) {
                return None;
            }
        }
        let ids = Ids::new(head, counts(env));
        {
            let mut names = self.names.borrow_mut();
            names.update(env);
            let mut learned = ids.learned.borrow_mut();
            for (k, old, name) in &head.named {
                if let Some(now) = names.id_of(*k, name) {
                    learned.insert((*k, *old), now);
                }
            }
        }
        if !head.lookups.iter().all(|l| answers(env, &ids, l)) {
            return None;
        }
        for (r, h) in &head.reads {
            // A read naming no id is asked as it is.
            let mut e = Enc::new(true);
            e.ids = Some(Vec::new());
            r.enc(&mut e);
            let renumbered;
            let r = if e.ids.take().is_some_and(|v| v.is_empty()) {
                r
            } else {
                renumbered = ids.read(r)?;
                e = Enc::new(true);
                renumbered.enc(&mut e);
                &renumbered
            };
            if self.answer_at(env, e.buf, r) != Some(*h) {
                return None;
            }
        }
        let map = |kind, id| ids.map(kind, id);
        let mut d = Dec::new(&u.body);
        d.files = Some(&u.files);
        d.ids = Some(&map);
        let b = Body::dec(&mut d)?;
        // A new tag must still be free (`any_tag` probes past a taken one).
        {
            let tags = sink.any_tags.borrow();
            if b.any_tags
                .iter()
                .any(|(_, t)| tags.iter().any(|(_, old)| old == t))
            {
                return None;
            }
        }
        let now = ids.now;
        // Names made of this build's ids follow them.
        let renamed = |mut f: HirFunction| {
            let old = f.id.0 - now.next_id + head.start.next_id;
            for p in ["__closure_", "__coro_", "__thunk_"] {
                if f.name == format!("{p}{old}") {
                    f.name = format!("{p}{}", f.id.0);
                }
            }
            f.name = map_type_ids(&f.name, |enum_, id| {
                let k = if enum_ { IdKind::Enum } else { IdKind::Struct };
                let tag = if enum_ { "Enum" } else { "Struct" };
                Some(format!("{tag}{}", ids.map(k, id)?))
            })?;
            Some(f)
        };
        let extras = b
            .extras
            .into_iter()
            .map(renamed)
            .collect::<Option<Vec<_>>>()?;
        sink.extras.borrow_mut().extend(extras);
        let (struct_bases, enum_bases) = head.bases.split_at(b.structs.len());
        {
            let mut table = env.defs.struct_table.borrow_mut();
            let mut structs = env.defs.structs.borrow_mut();
            for (mut s, base) in b.structs.into_iter().zip(struct_bases) {
                s.name = mangle_inst(base, &s.generic_args);
                crate::name_type(false, s.id.0, base, &s.generic_args);
                table.insert(s.name.clone(), s.id);
                structs.push(s);
            }
        }
        {
            let mut table = env.defs.enum_table.borrow_mut();
            let mut enums = env.defs.enums.borrow_mut();
            for (mut e, base) in b.enums.into_iter().zip(enum_bases) {
                e.name = mangle_inst(base, &e.generic_args);
                crate::name_type(true, e.id.0, base, &e.generic_args);
                table.insert(e.name.clone(), e.id);
                enums.push(e);
            }
        }
        for e in b.entries {
            set(&mut sink.ref_out.borrow_mut(), e.id, e.ref_out);
            set(&mut sink.mono_sigs.borrow_mut(), e.id, e.mono_sig);
            set(&mut sink.fn_origin.borrow_mut(), e.id, e.fn_origin);
            set(&mut sink.inferred_ret.borrow_mut(), e.id, e.inferred_ret);
            set(
                &mut sink.inferred_fns.borrow_mut(),
                e.id,
                e.inferred_fn.map(|(at, mut f)| {
                    if let Some(n) = decl_name(env, f.id) {
                        f.name = n;
                    }
                    (now.next_id + at, f)
                }),
            );
            if e.track_caller {
                sink.track_caller.borrow_mut().insert(e.id);
            } else {
                sink.track_caller.borrow_mut().remove(&e.id);
            }
        }
        for (k, v) in b.keyed {
            match k {
                Key::Mono(name, args) => {
                    set(
                        &mut sink.mono_cache.borrow_mut(),
                        Key::mono(&name, &args),
                        v,
                    );
                }
                Key::Thunk(kind, target, slot, callee) => set(
                    &mut sink.thunks.borrow_mut(),
                    super::throws::thunk_key(kind, target, &slot, &callee),
                    v,
                ),
                Key::Unknown(_) => {}
            }
        }
        {
            let mut unknown = env.names.unknown_spans.borrow_mut();
            for (path, at) in b.unknown_spans {
                unknown.entry(path).or_insert(at);
            }
        }
        sink.any_tags.borrow_mut().extend(b.any_tags);
        sink.any_dispatch.borrow_mut().extend(b.any_dispatch);
        sink.infer_vars.borrow_mut().extend(b.infer_vars);
        {
            let mut iw = sink.instance_warns.borrow_mut();
            for w in b.warns {
                match w {
                    WarnEvent::Instance(t, id) => iw.instance(t, id),
                    WarnEvent::Warn(t, d, id) => iw.warn(t, d, id),
                }
            }
        }
        sink.diagnostics.borrow_mut().extend(b.diagnostics);
        env.names.diags.borrow_mut().extend(b.name_diags);
        env.names.alias_uses.borrow_mut().extend(b.alias_uses);
        sink.next_id
            .set(head.end.next_id - head.start.next_id + now.next_id);
        let mut func = b.func;
        func.name = decl_name(env, FunctionId(u32::try_from(decl).ok()?))?;
        Some(func)
    }

    /// The `fn_table` key whose `stable_name` is `k` (`k` when none).
    fn raw_key<'k>(&'k self, env: &'k Env<'_>, k: &'k str) -> &'k str {
        let keys = self.raw_keys.get_or_init(|| {
            let mut m: HashMap<String, Option<String>> = HashMap::new();
            for raw in env.fn_table.keys() {
                let s = stable_name(raw);
                if s != *raw {
                    m.entry(s)
                        .and_modify(|v| *v = None)
                        .or_insert_with(|| Some(raw.clone()));
                }
            }
            m
        });
        match keys.get(k) {
            Some(Some(raw)) => raw,
            Some(None) => "",
            None => k,
        }
    }
}

/// A replay's renumbering: what the unit made moves to where this
/// build's counters stand; what it found made before is what the reads
/// answer now (`learn`); the program's own declarations and types keep
/// their ids.
struct Ids {
    start: Counts,
    end: Counts,
    now: Counts,
    learned: std::cell::RefCell<HashMap<(u8, u32), u32>>,
}

/// The name declaration `id` is lowered under: a method's key may name
/// types by id (`Struct12__len`), so a replayed one takes this build's.
fn decl_name(env: &Env<'_>, id: FunctionId) -> Option<String> {
    Some(match env.decls.get(id.0 as usize)? {
        MethodOrFn::Fn(f) => f.name.clone(),
        MethodOrFn::Method { key, .. } => key.clone(),
    })
}

/// The id of no function (a signature made up for a call).
const NO_ID: u32 = u32::MAX;

const fn kind_tag(k: IdKind) -> u8 {
    match k {
        IdKind::Function => 0,
        IdKind::Struct => 1,
        IdKind::Enum => 2,
        IdKind::InferVar => 3,
    }
}

impl Ids {
    fn new(head: &Head, now: Counts) -> Self {
        Self {
            start: head.start,
            end: head.end,
            now,
            learned: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// The id `id` of `kind` of the unit is in this build.
    fn map(&self, kind: IdKind, id: u32) -> Option<u32> {
        let (from, to, now) = match kind {
            IdKind::Function => (self.start.next_id, self.end.next_id, self.now.next_id),
            IdKind::Struct => (self.start.structs, self.end.structs, self.now.structs),
            IdKind::Enum => (self.start.enums, self.end.enums, self.now.enums),
            IdKind::InferVar => (
                self.start.infer_vars,
                self.end.infer_vars,
                self.now.infer_vars,
            ),
        };
        if id == NO_ID {
            Some(id)
        } else if (from..to).contains(&id) {
            Some(id - from + now)
        } else {
            self.learned.borrow().get(&(kind_tag(kind), id)).copied()
        }
    }

    /// Whether `old` (a read's answer then) is `new` (its answer now):
    /// an id made before the unit and not known yet is learned.
    fn same(&self, kind: IdKind, old: Option<u32>, new: Option<u32>) -> bool {
        match (old, new) {
            (None, None) => true,
            (Some(o), Some(n)) => {
                if let Some(m) = self.map(kind, o) {
                    return m == n;
                }
                self.learned.borrow_mut().insert((kind_tag(kind), o), n);
                true
            }
            _ => false,
        }
    }

    /// `v` with its ids as this build numbers them.
    fn renumber<T: Codec>(&self, v: &T) -> Option<T> {
        let mut e = Enc::new(true);
        v.enc(&mut e);
        let map = |kind, id| self.map(kind, id);
        let mut d = Dec::new(&e.buf);
        d.ids = Some(&map);
        T::dec(&mut d)
    }

    fn ty(&self, ty: &HirTy) -> Option<HirTy> {
        self.renumber(ty)
    }

    fn read(&self, r: &Read) -> Option<Read> {
        self.renumber(r)
    }

    fn tys(&self, tys: &[HirTy]) -> Option<Vec<HirTy>> {
        tys.iter().map(|t| self.ty(t)).collect()
    }
}

codec_struct!(MethodInfo {
    module,
    vis,
    trait_name,
    trait_args
});
codec_struct!(GenericMethod { def, self_ty, info });
codec_struct!(TraitInfo {
    methods,
    supers,
    needs,
    decls,
    module,
    params,
    generics,
    super_tys,
    assoc,
    consts
});

/// A fingerprint of interface values: positions left out, ids by their
/// build-independent names.
struct Fp(Enc);

impl Fp {
    fn new() -> Self {
        let mut e = Enc::new(false);
        e.ids = Some(Vec::new());
        e.no_ids = true;
        Self(e)
    }

    fn add<T: Codec>(&mut self, v: &T) -> &mut Self {
        v.enc(&mut self.0);
        self
    }

    fn opt<T>(&mut self, v: Option<T>, f: impl FnOnce(&mut Self, T)) -> &mut Self {
        match v {
            Some(v) => {
                self.0.u(1);
                f(self, v);
            }
            None => self.0.u(0),
        }
        self
    }

    fn debug(&mut self, v: &impl std::fmt::Debug) -> &mut Self {
        self.0.bytes(format!("{v:?}").as_bytes());
        self
    }

    fn finish(mut self, names: &Names) -> Option<u64> {
        for (k, id) in self.0.ids.take().unwrap_or_default() {
            match k {
                _ if id == NO_ID => self.0.bytes(b"!"),
                IdKind::InferVar => self.0.u(u64::from(id)),
                k => self.0.bytes(names.name_of(kind_tag(k), id)?.as_bytes()),
            }
        }
        Some(rokc_core::hash::fast(&self.0.buf))
    }
}

/// A declaration as lowering reads it.
fn fp_decl(f: &mut Fp, d: &MethodOrFn<'_>) {
    match d {
        MethodOrFn::Fn(def) => {
            f.add(&0u8).add(*def);
        }
        MethodOrFn::Method {
            fn_def,
            self_ty,
            module,
            key,
            subst,
        } => {
            f.add(&1u8)
                .add(&**fn_def)
                .add(self_ty)
                .add(module)
                .add(&stable_name(key))
                .add(subst);
        }
    }
}

fn fp_impl(f: &mut Fp, i: &super::Impl) {
    f.add(&i.trait_key)
        .add(&i.trait_args)
        .add(&i.assoc)
        .add(&i.generics)
        .add(&i.where_clause)
        .add(&i.self_ty)
        .add(&i.module)
        .add(&i.methods)
        .add(&i.consts)
        .add(&i.concrete);
}

/// Keys by last segment, the ones of `leaf` in order.
fn leaf_keys(l: &super::Leaves, leaf: &str) -> Vec<String> {
    let mut v = l.get(leaf).cloned().unwrap_or_default();
    v.sort();
    v
}

impl Incr<'_> {
    /// The fingerprint of impl `i`, taken once per build.
    fn impl_fp(&self, env: &Env<'_>, i: usize) -> Option<u64> {
        if let Some(h) = self.impl_fps.borrow().get(&i) {
            return *h;
        }
        let mut f = Fp::new();
        fp_impl(&mut f, &env.impls.all[i]);
        let mut names = self.names.borrow_mut();
        names.update(env);
        let h = f.finish(&names);
        drop(names);
        self.impl_fps.borrow_mut().insert(i, h);
        h
    }

    /// The fingerprints of impls `at`, in source order.
    fn impls_fp(&self, env: &Env<'_>, f: &mut Fp, at: &[usize]) -> Option<()> {
        for &i in at {
            f.add(&self.impl_fp(env, i)?);
        }
        Some(())
    }

    /// The impls by method (generic ones) or by associated const.
    fn impls_by(&self, env: &Env<'_>, consts: bool, name: &str) -> &[usize] {
        let by = self.impls_by.get_or_init(|| {
            let mut by: [HashMap<String, Vec<usize>>; 2] = Default::default();
            for (i, im) in env.impls.all.iter().enumerate() {
                if im.is_generic() {
                    for m in im.methods.keys() {
                        by[0].entry(m.clone()).or_default().push(i);
                    }
                }
                for (c, ..) in &im.consts {
                    let at = by[1].entry(c.clone()).or_default();
                    if at.last() != Some(&i) {
                        at.push(i);
                    }
                }
            }
            by
        });
        by[usize::from(consts)].get(name).map_or(&[], Vec::as_slice)
    }

    /// The fingerprint of what `r` reads now (`None`: not readable).
    fn answer(&self, env: &Env<'_>, r: &Read) -> Option<u64> {
        let mut e = Enc::new(true);
        r.enc(&mut e);
        self.answer_at(env, e.buf, r)
    }

    /// `answer` of `r`, encoded as `key`.
    fn answer_at(&self, env: &Env<'_>, key: Vec<u8>, r: &Read) -> Option<u64> {
        if let Some(a) = self.answers.borrow().get(&key) {
            return *a;
        }
        let a = self.answer_now(env, r);
        self.answers.borrow_mut().insert(key, a);
        a
    }

    #[allow(clippy::too_many_lines)] // one arm per table
    fn answer_now(&self, env: &Env<'_>, r: &Read) -> Option<u64> {
        let mut f = Fp::new();
        let names = &env.names;
        match r {
            Read::Sig(k) => {
                f.add(&env.fn_table.get(self.raw_key(env, k)).cloned());
            }
            Read::Generic(k) => {
                f.opt(env.generic_fns.get(k), |f, d| {
                    f.add(*d);
                });
            }
            Read::FnLeaf(k) => {
                let l = env.fn_leaves.get_or_init(|| {
                    super::leaves(env.fn_table.keys().chain(env.generic_fns.keys()))
                });
                f.add(&leaf_keys(l, k));
            }
            Read::ExternFn(k) => {
                f.add(&env.extern_fns.get(k).cloned());
            }
            Read::Methods(ty, m) => {
                let ids = super::method_target_for(ty)
                    .and_then(|t| env.method_table.get(&(t, m.clone())));
                f.add(&ids.cloned());
            }
            Read::AllMethods => {
                // Order-free: the sum of the entries' fingerprints.
                let mut sum = 0u64;
                let mut names_now = self.names.borrow_mut();
                names_now.update(env);
                for ((t, m), ids) in &env.method_table {
                    let mut g = Fp::new();
                    match t {
                        super::MethodTarget::Struct(id) => g.add(&0u8).add(id),
                        super::MethodTarget::Enum(id) => g.add(&1u8).add(id),
                        super::MethodTarget::Mangled(s) => g.add(&2u8).add(&stable_name(s)),
                    };
                    g.add(m).add(ids);
                    sum = sum.wrapping_add(g.finish(&names_now)?);
                }
                f.add(&sum);
            }
            Read::AllGenericMethods => {
                f.add(&env.generic_methods);
            }
            Read::MethodInfo(id) => {
                f.opt(env.method_info.get(id), |f, i| {
                    f.add(i);
                });
            }
            Read::GenericMethods(b, m) => {
                f.opt(env.generic_methods.get(&(b.clone(), m.clone())), |f, v| {
                    f.add(&(v.len() as u64));
                    for g in v {
                        f.add(g);
                    }
                });
            }
            Read::KeyOf(id) => {
                f.add(&env.key_of.get(id).map(|k| stable_name(k)));
            }
            Read::InferRet(id) => {
                f.add(&env.infer_ret.contains(id));
            }
            Read::MayDiverge(at) => {
                f.add(&env.may_diverge.contains(at));
            }
            Read::Intrinsic(id) => {
                f.debug(&env.intrinsics.get(id));
            }
            Read::GenericIntrinsic(k) => {
                f.debug(&env.generic_intrinsics.get(k));
            }
            Read::FnIn(set, id) => {
                f.add(&super::FnSet::of(*set)?.contains(env, *id));
            }
            Read::ItemVis(k) => {
                f.add(&env.item_visibility.get(k).copied());
            }
            Read::FieldVis(k) => {
                f.add(&env.field_vis.get(k).cloned());
            }
            Read::Const(k) => {
                f.add(&env.consts.get(k).cloned());
            }
            Read::ConstLeaf(k) => {
                let mut v = env.value_leaf(false, k);
                v.sort();
                f.add(&v);
            }
            Read::Static(k) => {
                f.add(&env.statics.get(k).cloned());
            }
            Read::StaticLeaf(k) => {
                let mut v = env.value_leaf(true, k);
                v.sort();
                f.add(&v);
            }
            Read::MutStatic(k) => {
                f.add(&env.mut_statics.contains(k));
            }
            Read::ExternStatic(k) => {
                f.add(&env.extern_statics.contains(k));
            }
            Read::TraitInfo(k) => {
                f.opt(env.trait_info.get(k), |f, t| {
                    f.add(t);
                });
            }
            Read::NegAuto(b, k) => {
                f.add(&env.neg_auto.contains(&(b.clone(), k.clone())));
            }
            Read::NonExhaustive(k) => {
                f.add(&env.non_exhaustive.contains(k));
            }
            Read::FnMark(id) => {
                f.add(&env.fn_marks.get(id).cloned());
            }
            Read::Mark(k) => {
                f.add(&env.marks.get(k).cloned());
            }
            Read::BoundMethod(t, m) => {
                f.opt(env.bound_methods.get(&(t.clone(), m.clone())), |f, g| {
                    f.add(g);
                });
            }
            Read::BoundTrait(k) => {
                f.add(&env.bound_traits.get(k).cloned());
            }
            Read::Decl(id) => {
                f.opt(env.decls.get(id.0 as usize), fp_decl);
            }
            Read::CheckFailed(at) => {
                f.add(&env.sink.failed.borrow().contains(at));
            }
            Read::Uses(m) => {
                f.debug(&env.names.uses.get(m));
            }
            Read::GenericImpls => {
                for (i, _) in env.impls.iter().enumerate().filter(|(_, i)| i.is_generic()) {
                    f.add(&self.impl_fp(env, i)?);
                }
            }
            Read::GenericImplsOf(k) => {
                for &i in env.impls.generic_of.get(k).into_iter().flatten() {
                    f.add(&self.impl_fp(env, i)?);
                }
            }
            Read::GenericImplsWith(m) => {
                self.impls_fp(env, &mut f, self.impls_by(env, false, m))?;
            }
            Read::ImplConsts(n) => {
                self.impls_fp(env, &mut f, self.impls_by(env, true, n))?;
            }
            Read::ImplsOn(ty) => {
                let refs: Vec<&crate::TraitRef> = env.impls.refs_on(ty).collect();
                f.add(&(refs.len() as u64));
                for r in refs {
                    f.add(r);
                }
            }
            Read::ImplsOnOf(ty, k) => {
                let refs: Vec<&crate::TraitRef> = env.impls.refs_of(ty, k).collect();
                f.add(&(refs.len() as u64));
                for r in refs {
                    f.add(r);
                }
            }
            Read::ImplsOf(k) => {
                let tys: Vec<&HirTy> = env.impls.tys_of(k).collect();
                f.add(&(tys.len() as u64));
                for t in tys {
                    f.add(t);
                }
            }
            Read::Canon(segs, module) => {
                let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
                let errors = names.diags.borrow().len();
                let full = names.canon(&segs, module, &[], Span::DUMMY);
                let failed = names.diags.borrow().len() != errors;
                names.diags.borrow_mut().truncate(errors);
                f.add(&full).add(&failed);
            }
            Read::Type(k) => {
                f.add(&names.names.contains(k));
            }
            Read::TypeLeaf(k) => {
                let l = names.leaves.get_or_init(|| {
                    super::leaves(names.names.iter().chain(names.reexports.keys()))
                });
                f.add(&leaf_keys(l, k));
            }
            Read::TypeVis(k) => {
                f.add(&names.visibility.get(k).copied());
            }
            Read::ModVis(k) => {
                f.add(&names.mod_vis.get(k).copied());
            }
            Read::Reexport(k) => {
                f.add(&names.reexports.get(k).cloned());
            }
            Read::Trait(k) => {
                f.add(&names.traits.contains(k));
            }
            Read::Traits => {
                f.add(&names.traits);
            }
            Read::Variant(k) => {
                f.add(&names.variants.contains(k));
            }
            Read::VariantLeaf(k) => {
                let l = names
                    .variant_leaves
                    .get_or_init(|| super::leaves(&names.variants));
                f.add(&leaf_keys(l, k));
            }
            Read::NameConst(k) => {
                f.add(&names.consts.get(k).cloned());
            }
            Read::NameConstLeaf(k) => {
                let l = names
                    .const_leaves
                    .get_or_init(|| super::leaves(names.consts.keys()));
                f.add(&leaf_keys(l, k));
            }
            Read::ValueReexport(k) => {
                f.add(&names.value_reexports.get(k).cloned());
            }
            Read::GenericAlias(k) => {
                f.add(&names.generic_aliases.get(k).cloned());
            }
            Read::AnyGenericAlias => {
                f.add(&!names.generic_aliases.is_empty());
            }
            Read::DefaultedLeaf(k) => {
                f.add(&env.defs.defaulted_leaves.contains(k));
            }
            Read::PlainAlias(k) => {
                f.add(&names.plain_aliases.get(k).cloned());
            }
            Read::StructDef(id) => {
                let structs = env.defs.structs.borrow();
                f.opt(structs.get(id.0 as usize), |f, d| {
                    f.add(&crate::type_name(false, id.0))
                        .add(&d.repr)
                        .add(&d.def_id)
                        .add(&d.fields)
                        .add(&d.generic_args)
                        .add(&d.union_kind)
                        .add(&d.niche);
                });
            }
            Read::EnumDef(id) => {
                let enums = env.defs.enums.borrow();
                f.opt(enums.get(id.0 as usize), |f, d| {
                    f.add(&crate::type_name(true, id.0))
                        .add(&d.repr)
                        .add(&d.def_id)
                        .add(&d.variants)
                        .add(&d.generic_args);
                });
            }
            Read::GenericStruct(k) => {
                f.opt(env.defs.generic_structs.get(k), |f, d| {
                    f.add(*d);
                });
            }
            Read::GenericEnum(k) => {
                f.opt(env.defs.generic_enums.get(k), |f, d| {
                    f.add(*d);
                });
            }
            Read::GenericUnion(k) => {
                f.opt(env.defs.generic_unions.get(k), |f, d| {
                    f.add(*d);
                });
            }
        }
        let mut names = self.names.borrow_mut();
        names.update(env);
        f.finish(&names)
    }
}

/// What every lowering sees alike: the build's target and flags, the
/// operators, the lang slots, the literal handlers.
fn global_key(env: &Env<'_>, input: &crate::HirInput) -> u64 {
    let mut e = Enc::new(false);
    input.fingerprint.enc(&mut e);
    e.bytes(
        format!(
            "{:?} {} {:?} {} {} {} {:?}",
            (&input.packages, &input.lib, &input.prebuilt),
            input.check_library,
            input.root_main,
            input.inline_prebuilt,
            input.pointer_width,
            input.stdcall_system,
            input.no_implicit_rok,
        )
        .as_bytes(),
    );
    let mut ops: Vec<String> = env.prec.entries().map(|o| format!("{o:?}")).collect();
    ops.sort();
    let mut lang: Vec<String> = env.names.lang.entries().map(|l| format!("{l:?}")).collect();
    lang.sort();
    let shapes = env
        .literal_shapes
        .iter()
        .map(|h| format!("{:?} {} {}", h.shape, h.name, h.priority));
    for s in ops.into_iter().chain(lang).chain(shapes) {
        e.bytes(s.as_bytes());
    }
    rokc_core::hash::fast(&e.buf)
}

/// Set or remove the entry at `k`.
fn set<K: std::hash::Hash + Eq, V>(map: &mut HashMap<K, V>, k: K, v: Option<V>) {
    match v {
        Some(v) => {
            map.insert(k, v);
        }
        None => {
            map.remove(&k);
        }
    }
}

fn counts(env: &Env<'_>) -> Counts {
    let n = |x: usize| u32::try_from(x).unwrap_or(u32::MAX);
    Counts {
        next_id: env.sink.next_id.get(),
        structs: n(env.defs.structs.borrow().len()),
        enums: n(env.defs.enums.borrow().len()),
        any_tags: n(env.sink.any_tags.borrow().len()),
        any_dispatch: n(env.sink.any_dispatch.borrow().len()),
        extras: n(env.sink.extras.borrow().len()),
        infer_vars: n(env.sink.infer_vars.borrow().len()),
    }
}

/// Sizes of the shared tables before a lowering.
struct Sizes {
    diagnostics: usize,
    name_diags: usize,
    alias_uses: usize,
    unknown_spans: usize,
    uninferred: u32,
    inferred_fns: usize,
    mono_cache: usize,
    mono_sigs: usize,
    ref_out: usize,
    thunks: usize,
    fn_origin: usize,
    track_caller: usize,
    inferred_ret: usize,
}

impl Sizes {
    fn of(env: &Env<'_>) -> Self {
        let sink = &env.sink;
        Self {
            diagnostics: sink.diagnostics.borrow().len(),
            name_diags: env.names.diags.borrow().len(),
            alias_uses: env.names.alias_uses.borrow().len(),
            unknown_spans: env.names.unknown_spans.borrow().len(),
            uninferred: sink.uninferred.get(),
            inferred_fns: sink.inferred_fns.borrow().len(),
            mono_cache: sink.mono_cache.borrow().len(),
            mono_sigs: sink.mono_sigs.borrow().len(),
            ref_out: sink.ref_out.borrow().len(),
            thunks: sink.thunks.borrow().len(),
            fn_origin: sink.fn_origin.borrow().len(),
            track_caller: sink.track_caller.borrow().len(),
            inferred_ret: sink.inferred_ret.borrow().len(),
        }
    }
}

/// Whether the shared state answers `l` (read under `ids`) as it did
/// when it was logged.
fn answers(env: &Env<'_>, ids: &Ids, l: &Lookup) -> bool {
    let sink = &env.sink;
    let raw = |id: Option<FunctionId>| id.map(|i| i.0);
    let fun = |old: &Option<FunctionId>, new: Option<FunctionId>| {
        ids.same(IdKind::Function, raw(*old), raw(new))
    };
    let list = |kind, old: Vec<u32>, new: Vec<u32>| {
        old.len() == new.len()
            && old
                .into_iter()
                .zip(new)
                .all(|(o, n)| ids.same(kind, Some(o), Some(n)))
    };
    match l {
        Lookup::Mono(name, args, r) => {
            let Some(args) = ids.tys(args) else {
                return false;
            };
            let now = sink
                .mono_cache
                .borrow()
                .get(&Key::mono(name, &args))
                .copied();
            fun(r, now)
        }
        Lookup::Struct(k, r) => {
            let now = env.defs.struct_table.borrow().get(k).copied();
            ids.same(IdKind::Struct, r.map(|i| i.0), now.map(|i| i.0))
        }
        Lookup::Enum(k, r) => {
            let now = env.defs.enum_table.borrow().get(k).copied();
            ids.same(IdKind::Enum, r.map(|i| i.0), now.map(|i| i.0))
        }
        Lookup::StructInst(base, args, r) => {
            let Some(args) = ids.tys(args) else {
                return false;
            };
            let now = env
                .defs
                .struct_table
                .borrow()
                .get(&mangle_inst(base, &args))
                .copied();
            ids.same(IdKind::Struct, r.map(|i| i.0), now.map(|i| i.0))
        }
        Lookup::EnumInst(base, args, r) => {
            let Some(args) = ids.tys(args) else {
                return false;
            };
            let now = env
                .defs
                .enum_table
                .borrow()
                .get(&mangle_inst(base, &args))
                .copied();
            ids.same(IdKind::Enum, r.map(|i| i.0), now.map(|i| i.0))
        }
        Lookup::StructInsts(base, r) => {
            let now = super::LowerCtx::instantiations(&env.defs.struct_table.borrow(), base);
            list(
                IdKind::Struct,
                r.iter().map(|i| i.0).collect(),
                now.iter().map(|i| i.0).collect(),
            )
        }
        Lookup::EnumInsts(base, r) => {
            let now = super::LowerCtx::instantiations(&env.defs.enum_table.borrow(), base);
            list(
                IdKind::Enum,
                r.iter().map(|i| i.0).collect(),
                now.iter().map(|i| i.0).collect(),
            )
        }
        Lookup::Thunk(Key::Thunk(kind, target, slot, callee), r) => {
            let target = match target {
                Some(t) => match ids.map(IdKind::Function, t.0) {
                    Some(t) => Some(FunctionId(t)),
                    None => return false,
                },
                None => None,
            };
            let (Some(slot), Some(callee)) = (ids.ty(slot), ids.ty(callee)) else {
                return false;
            };
            let key = super::throws::thunk_key(kind, target, &slot, &callee);
            fun(r, sink.thunks.borrow().get(&key).copied())
        }
        Lookup::Thunk(..) => false,
        Lookup::AnyTag(ty, r) => {
            let Some(ty) = ids.ty(ty) else {
                return false;
            };
            sink.any_tags
                .borrow()
                .iter()
                .find(|(t, _)| *t == ty)
                .map(|(_, tag)| *tag)
                == *r
        }
        Lookup::AnyDispatch(recv, trait_key, method, shape, r) => {
            let Some(recv) = ids.ty(recv) else {
                return false;
            };
            let Some(shape) = shape.iter().map(|t| ids.ty(t)).collect::<Option<Vec<_>>>() else {
                return false;
            };
            let now = sink
                .any_dispatch
                .borrow()
                .iter()
                .find(|d| d.is(&recv, trait_key, method, &shape))
                .map(|d| d.id);
            fun(r, now)
        }
        Lookup::InferredRet(id, r) => {
            let Some(id) = ids.map(IdKind::Function, id.0) else {
                return false;
            };
            let now = sink.inferred_ret.borrow().get(&FunctionId(id)).cloned();
            match (r, now) {
                (None, None) | (Some(None), Some(None)) => true,
                (Some(Some(t)), Some(Some(n))) => ids.ty(t).is_some_and(|t| t == n),
                _ => false,
            }
        }
    }
}

/// Declaration `i` is plain (see the module doc).
fn is_plain(env: &Env<'_>, i: usize) -> bool {
    let (def, borrowed) = match &env.decls[i] {
        MethodOrFn::Fn(f) => (*f, true),
        MethodOrFn::Method { fn_def, subst, .. } => (
            &**fn_def,
            matches!(fn_def, std::borrow::Cow::Borrowed(_)) && subst.is_empty(),
        ),
    };
    borrowed
        && def.body.is_some()
        && def.generics.params.is_empty()
        && !def
            .attrs
            .iter()
            .any(|a| a.path == [rokc_ast::CONST_FN_ATTR])
}

/// The program's interface: everything lowering a body can see of it,
/// but the plain bodies (`bodies`) and source positions.
fn interface_key(
    env: &Env<'_>,
    bodies: &HashSet<(&str, Span)>,
    programs: &[&Program],
    raw_uses: &[(String, rokc_core::Visibility, rokc_ast::UseTree)],
    input: &crate::HirInput,
) -> u64 {
    let mut e = Enc::new(false);
    input.fingerprint.enc(&mut e);
    e.bytes(format!("{:?}", (&input.packages, &input.lib, &input.prebuilt)).as_bytes());
    e.bytes(
        format!(
            "{} {:?} {} {} {}",
            input.check_library,
            input.root_main,
            input.inline_prebuilt,
            input.pointer_width,
            input.stdcall_system
        )
        .as_bytes(),
    );
    input.item_modules.enc(&mut e);
    e.u(raw_uses.len() as u64);
    for u in raw_uses {
        u.enc(&mut e);
    }
    let mut infer: Vec<u32> = env.infer_ret.iter().map(|id| id.0).collect();
    infer.sort_unstable();
    e.u(infer.len() as u64);
    for id in infer {
        e.u(u64::from(id));
    }
    // Files by id (spans name them by id).
    for (p, _) in env.sources.files() {
        e.bytes(p.as_bytes());
    }
    let fn_def = |f: &FnDef, e: &mut Enc| {
        // A plain declaration's body and parameters and return type are
        // read through `Lookup::Sig` (its callers) and its unit's key
        // (itself).
        if bodies.contains(&(f.name.as_str(), f.span)) {
            e.buf.push(1);
            f.vis.enc(e);
            f.attrs.enc(e);
            f.name.enc(e);
            f.generics.enc(e);
            // The parameters but a method's receiver are the signature's.
            f.params
                .first()
                .filter(|p| p.name == "self")
                .map(|p| p.by_ref)
                .enc(e);
            f.ret_ty.is_some().enc(e);
            (f.is_async, f.is_generator, f.is_unsafe).enc(e);
            match &f.throws {
                rokc_ast::ThrowsKind::None => e.u(0),
                rokc_ast::ThrowsKind::Untyped => e.u(1),
                rokc_ast::ThrowsKind::Typed(t) => e.u(2 + t.len() as u64),
            }
            f.where_clause.enc(e);
        } else {
            e.buf.push(0);
            f.enc(e);
        }
    };
    // A kept library stage's items (the first program's) are those of its
    // key.
    let programs = match input.library {
        Some(k) => {
            e.u(k);
            &programs[1..]
        }
        None => programs,
    };
    let items = programs.iter().flat_map(|p| &p.items);
    e.u(items.clone().count() as u64);
    for item in items {
        match item {
            Item::Fn(f) => {
                e.buf.push(0);
                fn_def(f, &mut e);
            }
            Item::Impl(imp) => {
                e.buf.push(1);
                imp.vis.enc(&mut e);
                imp.attrs.enc(&mut e);
                imp.generics.enc(&mut e);
                imp.trait_path.enc(&mut e);
                (imp.is_unsafe, imp.negative).enc(&mut e);
                imp.self_ty.enc(&mut e);
                imp.where_clause.enc(&mut e);
                imp.module.enc(&mut e);
                e.u(imp.items.len() as u64);
                for i in &imp.items {
                    match i {
                        ImplItem::Method(f) => fn_def(f, &mut e),
                        other => other.enc(&mut e),
                    }
                }
            }
            other => {
                e.buf.push(2);
                other.enc(&mut e);
            }
        }
    }
    rokc_core::hash::fast(&e.buf)
}
