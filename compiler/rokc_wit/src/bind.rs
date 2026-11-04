//! rok source for WIT bindings: types as declarations, imports as rok
//! functions that lower arguments and lift results through the canonical
//! ABI, exports as `extern "C"` entries that lift, call the user's
//! function and lower the result (a post-return frees it).
//!
//! Mapping: `record` -> `struct`, `variant` / `enum` -> `enum`, `flags` ->
//! a `struct` of `bool`s, `option<T>` -> `T?`, `result<T, E>` ->
//! `Result<T, E>` (a function returning one `throws (E) -> T`),
//! `tuple` -> tuple, `list<T>` -> `List<T>`, `list<T, N>` -> `[T; N]`,
//! `string` -> `str`, `sN` / `uN` -> `iN` / `uN`, `future<T>` ->
//! `AsyncFuture<T>`, `stream<T>` -> a channel `Receiver<T>`,
//! `error-context` -> `ErrorContext`. An imported resource is a struct
//! over its handle (`Drop` drops it); an exported one is the user's
//! struct, kept in a table while the host holds a handle to it. An
//! `async func` is an `async fn` both ways (the async canonical ABI:
//! imports lowered async, exports lifted with a callback).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::abi::{self, Flat, MAX_FLAT_PARAMS, MAX_FLAT_RESULTS};
use crate::parse::Prim;
use crate::resolve::{
    FnKind, Function, IfaceId, Owner, Resolve, Type, TypeId, TypeKind, WorldEntry, WorldId,
};
use crate::{Loaded, Request};

/// The support library the generated code calls: memory, blocks,
/// handles (`W`); the async ABI, which runs on the executor (`A`).
const W: &str = "rok::bare::ffi::wit";
const A: &str = "rok::std::wit";
const LIST: &str = "rok::core::collections::list::List";
const RESULT: &str = "rok::core::error::Result";
const FUTURE: &str = "rok::core::future::AsyncFuture";
const RECEIVER: &str = "rok::core::channel::Receiver";

/// Most flat parameters of an async-lowered import.
const MAX_FLAT_ASYNC_PARAMS: usize = 4;

/// What [`bind`] makes of the `ffi::wit` items of one module.
#[derive(Clone, Debug, Default)]
pub struct Bindings {
    /// rok items to add to the module the `ffi::wit` items are in.
    pub source: String,
    /// Symbols the core module must export.
    pub exports: Vec<String>,
    /// The component world's `import` / `export` / `include` lines.
    pub world: String,
}

enum View {
    Prim(Prim),
    List(Type),
    Option(Type),
    Result(Option<Type>, Option<Type>),
    Tuple(Vec<Type>),
    FixedList(Type, u32),
    Future,
    Stream,
    ErrorContext,
    Record(TypeId, Vec<(String, Type)>),
    Variant(TypeId, Vec<(String, Option<Type>)>),
    Enum(TypeId, usize),
    Flags(TypeId, Vec<String>),
    Own(TypeId),
    Borrow(TypeId),
}

struct Gen<'a> {
    r: &'a Resolve,
    /// rok path of the module the bindings go in (`pkg`, `pkg::m`).
    base: String,
    exported: BTreeSet<IfaceId>,
    /// Generated items by module path (below `base`).
    mods: BTreeMap<Vec<String>, String>,
    /// `extern "C"` declarations by module path and core module.
    externs: Externs,
    exports: Vec<String>,
    /// The function being bound: what its futures' and streams' built-ins
    /// are named after.
    cur: Option<FnCtx>,
    /// Payload ops functions made.
    ops: BTreeSet<String>,
    /// The user's exported resource types (by rok name) that implement
    /// `Drop`.
    drops: BTreeSet<String>,
}

/// The function whose built-ins a future or stream type uses.
#[derive(Clone)]
struct FnCtx {
    /// The core module (`[export]` prefixed for an export's).
    module: String,
    /// The function's name in the canonical ABI.
    name: String,
    /// Its futures and streams, in `wit-parser`'s order.
    payloads: Vec<Type>,
}

/// rok name of a WIT identifier: `snake_case`, a keyword escaped with
/// a trailing `_`.
fn snake(s: &str) -> String {
    let s = s.to_ascii_lowercase().replace('-', "_");
    let reserved = [
        "in", "pkg", "sup", "rok", "type", "const", "static", "extern", "throws", "catch", "where",
        "union", "rethrows", "any", "Self", "str",
    ];
    if rokc_lexer::token::keyword_kind(&s).is_some() || reserved.contains(&s.as_str()) {
        s + "_"
    } else {
        s
    }
}

/// rok type name of a WIT identifier: `PascalCase`.
fn pascal(s: &str) -> String {
    let mut out = String::new();
    for w in s.split('-') {
        let mut cs = w.chars();
        if let Some(c) = cs.next() {
            out.push(c.to_ascii_uppercase());
            out.extend(cs.map(|c| c.to_ascii_lowercase()));
        }
    }
    out
}

/// A rok identifier for the pair `(a, b)`: readable, made unique by a
/// hash of the exact strings.
fn ident(prefix: &str, a: &str, b: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for byte in a.bytes().chain([0]).chain(b.bytes()) {
        h = (h ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    let clean: String = format!("{a}_{b}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{prefix}_{clean}_{h:08x}")
}

const fn zero(f: Flat) -> &'static str {
    match f {
        Flat::I32 => "0i32",
        Flat::I64 => "0i64",
        Flat::F32 => "0.0f32",
        Flat::F64 => "0.0f64",
    }
}

/// `v` (of flat type `from`) in a variant slot of type `to`.
fn coerce(from: Flat, to: Flat, v: &str) -> String {
    match (from, to) {
        (a, b) if a == b => v.to_owned(),
        (Flat::F32, Flat::I32) => format!("{W}::f32_bits({v})"),
        (Flat::F32, Flat::I64) => format!("({W}::f32_bits({v}) as u32) as i64"),
        (Flat::I32, Flat::I64) => format!("({v} as u32) as i64"),
        _ => format!("{W}::f64_bits({v})"),
    }
}

/// A slot `v` of type `from` back as the case's flat type `to`.
fn uncoerce(from: Flat, to: Flat, v: &str) -> String {
    match (from, to) {
        (a, b) if a == b => v.to_owned(),
        (Flat::I32, Flat::F32) => format!("{W}::f32_from({v})"),
        (Flat::I64, Flat::I32) => format!("{v} as i32"),
        (Flat::I64, Flat::F32) => format!("{W}::f32_from({v} as i32)"),
        _ => format!("{W}::f64_from({v})"),
    }
}

impl Gen<'_> {
    fn view(&self, t: &Type) -> View {
        match t {
            Type::Prim(p) => View::Prim(*p),
            Type::List(e) => View::List((**e).clone()),
            Type::Option(e) => View::Option((**e).clone()),
            Type::Result(a, b) => View::Result(a.as_deref().cloned(), b.as_deref().cloned()),
            Type::Tuple(ts) => View::Tuple(ts.clone()),
            Type::FixedList(e, n) => View::FixedList((**e).clone(), *n),
            Type::Future(_) => View::Future,
            Type::Stream(_) => View::Stream,
            Type::ErrorContext => View::ErrorContext,
            Type::Own(id) => View::Own(self.r.dealias(*id)),
            Type::Borrow(id) => View::Borrow(self.r.dealias(*id)),
            Type::Id(id) => {
                let d = self.r.dealias(*id);
                match &self.r.types[d].kind {
                    TypeKind::Alias(t) => self.view(t),
                    TypeKind::Record(fs) => View::Record(d, fs.clone()),
                    TypeKind::Variant(cs) => View::Variant(d, cs.clone()),
                    TypeKind::Enum(cs) => View::Enum(d, cs.len()),
                    TypeKind::Flags(fs) => View::Flags(d, fs.clone()),
                    TypeKind::Resource | TypeKind::Unresolved => View::Own(d),
                }
            }
        }
    }

    /// Module path (below `base`) of what `owner` defines.
    fn owner_mod(&self, owner: Owner) -> Vec<String> {
        let (pkg, mut tail) = match owner {
            Owner::Iface(i) => {
                let f = &self.r.interfaces[i];
                let mut tail = Vec::new();
                if let Some(w) = f.inline {
                    tail.push(snake(&self.r.worlds[w].name));
                }
                tail.push(snake(&f.name));
                (f.pkg, tail)
            }
            Owner::World(w) => (self.r.worlds[w].pkg, vec![snake(&self.r.worlds[w].name)]),
        };
        let name = &self.r.packages[pkg].name;
        let mut path: Vec<String> = name.ns.split(':').map(snake).collect();
        path.push(snake(&name.name));
        path.append(&mut tail);
        path
    }

    fn abs(&self, m: &[String]) -> String {
        let mut s = self.base.clone();
        for p in m {
            s.push_str("::");
            s.push_str(p);
        }
        s
    }

    fn type_path(&self, id: TypeId) -> String {
        let t = &self.r.types[id];
        format!(
            "{}::{}",
            self.abs(&self.owner_mod(t.owner)),
            pascal(&t.name)
        )
    }

    fn ours(&self, res: TypeId) -> bool {
        matches!(self.r.types[res].owner, Owner::Iface(i) if self.exported.contains(&i))
    }

    fn rty(&self, t: &Type) -> Result<String, String> {
        Ok(match t {
            Type::Prim(p) => match p {
                Prim::Bool => "bool",
                Prim::U8 => "u8",
                Prim::U16 => "u16",
                Prim::U32 => "u32",
                Prim::U64 => "u64",
                Prim::S8 => "i8",
                Prim::S16 => "i16",
                Prim::S32 => "i32",
                Prim::S64 => "i64",
                Prim::F32 => "f32",
                Prim::F64 => "f64",
                Prim::Char => "char",
                Prim::String => "str",
            }
            .to_owned(),
            Type::List(e) => format!("{LIST}<{}>", self.rty(e)?),
            // `T?` keeps a layer for a `T` that is optional itself.
            Type::Option(e) => format!("{}?", self.rty(e)?),
            Type::FixedList(e, n) => format!("[{}; {n}]", self.rty(e)?),
            Type::Future(e) => format!("{FUTURE}<{}>", self.payload_ty(e.as_deref())?),
            Type::Stream(e) => format!("{RECEIVER}<{}>", self.payload_ty(e.as_deref())?),
            Type::ErrorContext => format!("{W}::ErrorContext"),
            Type::Result(a, b) => format!(
                "{RESULT}<{}, {}>",
                a.as_deref()
                    .map_or_else(|| Ok("()".into()), |t| self.rty(t))?,
                b.as_deref()
                    .map_or_else(|| Ok("()".into()), |t| self.rty(t))?
            ),
            Type::Tuple(ts) => {
                let parts = ts
                    .iter()
                    .map(|t| self.rty(t))
                    .collect::<Result<Vec<_>, _>>()?;
                match parts.len() {
                    0 => "()".into(),
                    1 => format!("({},)", parts[0]),
                    _ => format!("({})", parts.join(", ")),
                }
            }
            Type::Own(id) | Type::Borrow(id) | Type::Id(id) => self.type_path(*id),
        })
    }

    /// A future's or stream's value type (`()` without one).
    fn payload_ty(&self, t: Option<&Type>) -> Result<String, String> {
        t.map_or_else(|| Ok("()".into()), |t| self.rty(t))
    }

    /// `t` holds a resource (no derives then: the user's types need not
    /// have them).
    fn has_resource(&self, t: &Type, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match self.view(t) {
            // No derives either for what holds a future or stream.
            View::Own(_) | View::Borrow(_) | View::Future | View::Stream | View::ErrorContext => {
                true
            }
            View::Prim(_) | View::Enum(..) | View::Flags(..) => false,
            View::List(e) | View::Option(e) | View::FixedList(e, _) => {
                self.has_resource(&e, depth + 1)
            }
            View::Result(a, b) => [a, b]
                .iter()
                .flatten()
                .any(|t| self.has_resource(t, depth + 1)),
            View::Tuple(ts) => ts.iter().any(|t| self.has_resource(t, depth + 1)),
            View::Record(_, fs) => fs.iter().any(|(_, t)| self.has_resource(t, depth + 1)),
            View::Variant(_, cs) => cs
                .iter()
                .filter_map(|(_, t)| t.as_ref())
                .any(|t| self.has_resource(t, depth + 1)),
        }
    }

    /// `t` lowers into memory blocks (strings, lists) that a
    /// post-return function frees.
    fn has_heap(&self, t: &Type) -> bool {
        match self.view(t) {
            View::Prim(Prim::String) | View::List(_) => true,
            View::Prim(_)
            | View::Enum(..)
            | View::Flags(..)
            | View::Own(_)
            | View::Borrow(_)
            | View::Future
            | View::Stream
            | View::ErrorContext => false,
            View::Option(e) | View::FixedList(e, _) => self.has_heap(&e),
            View::Result(a, b) => [a, b].iter().flatten().any(|t| self.has_heap(t)),
            View::Tuple(ts) => ts.iter().any(|t| self.has_heap(t)),
            View::Record(_, fs) => fs.iter().any(|(_, t)| self.has_heap(t)),
            View::Variant(_, cs) => cs
                .iter()
                .filter_map(|(_, t)| t.as_ref())
                .any(|t| self.has_heap(t)),
        }
    }

    fn push(&mut self, m: &[String], text: &str) {
        self.mods.entry(m.to_vec()).or_default().push_str(text);
    }

    /// The `extern "C"` function for core import `name` of `module`.
    fn extern_fn(
        &mut self,
        m: &[String],
        module: &str,
        name: &str,
        params: &[Flat],
        ret: Option<Flat>,
    ) -> String {
        let id = ident("wit", module, name);
        let ps: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(i, f)| format!("a{i}: {}", f.rok()))
            .collect();
        let ret = ret.map(|f| format!(" -> {}", f.rok())).unwrap_or_default();
        self.externs
            .entry(m.to_vec())
            .or_default()
            .entry(module.to_owned())
            .or_default()
            .insert(format!(
                "    #[link_name = \"{name}\"]\n    fn {id}({}){ret};\n",
                ps.join(", ")
            ));
        id
    }

    /// The `[resource-new]` / `[resource-rep]` / `[resource-drop]`
    /// imports of resource `res`, and its table (exported ones).
    fn resource_fns(&mut self, res: TypeId) -> (String, String, String, String) {
        let Owner::Iface(i) = self.r.types[res].owner else {
            unreachable!("resources live in interfaces")
        };
        let m = self.owner_mod(Owner::Iface(i));
        let name = &self.r.types[res].name;
        let key = self.r.iface_key(i);
        if self.ours(res) {
            let module = format!("[export]{key}");
            let new = self.extern_fn(
                &m,
                &module,
                &format!("[resource-new]{name}"),
                &[Flat::I32],
                Some(Flat::I32),
            );
            let rep = self.extern_fn(
                &m,
                &module,
                &format!("[resource-rep]{name}"),
                &[Flat::I32],
                Some(Flat::I32),
            );
            let drop = self.extern_fn(
                &m,
                &module,
                &format!("[resource-drop]{name}"),
                &[Flat::I32],
                None,
            );
            let table = format!(
                "{}::WIT_TABLE_{}",
                self.abs(&m),
                snake(name).to_ascii_uppercase()
            );
            (new, rep, drop, table)
        } else {
            let drop = self.extern_fn(
                &m,
                &key,
                &format!("[resource-drop]{name}"),
                &[Flat::I32],
                None,
            );
            (String::new(), String::new(), drop, String::new())
        }
    }

    /// The rok declaration of type `id`.
    fn type_decl(&mut self, id: TypeId) -> Result<(), String> {
        let t = &self.r.types[id];
        let m = self.owner_mod(t.owner);
        let name = pascal(&t.name);
        let me = Type::Id(id);
        let derive = if self.has_resource(&me, 0) {
            ""
        } else {
            "#[derive(Debug, Clone, PartialEq)]\n"
        };
        let text = match t.kind.clone() {
            TypeKind::Alias(a) => format!("pub type {name} = {};\n", self.rty(&a)?),
            TypeKind::Record(fs) => {
                let mut s = format!("{derive}pub struct {name} {{\n");
                for (f, ft) in &fs {
                    let _ = writeln!(s, "    pub {}: {};", snake(f), self.rty(ft)?);
                }
                s + "}\n"
            }
            TypeKind::Variant(cs) => {
                let mut s = format!("{derive}pub enum {name} {{\n");
                for (c, ct) in &cs {
                    match ct {
                        Some(ct) => {
                            let _ = writeln!(s, "    {}({}),", pascal(c), self.rty(ct)?);
                        }
                        None => {
                            let _ = writeln!(s, "    {},", pascal(c));
                        }
                    }
                }
                s + "}\n"
            }
            TypeKind::Enum(cs) => {
                let mut s = format!("{derive}pub enum {name} {{\n");
                for c in &cs {
                    let _ = writeln!(s, "    {},", pascal(c));
                }
                s + "}\n"
            }
            TypeKind::Flags(fs) => {
                let mut s =
                    format!("#[derive(Debug, Clone, PartialEq, Default)]\npub struct {name} {{\n");
                for f in &fs {
                    let _ = writeln!(s, "    pub {}: bool;", snake(f));
                }
                s + "}\n"
            }
            TypeKind::Resource if self.ours(id) => {
                let (.., table) = self.resource_fns(id);
                let user = format!("{}::{name}", self.base);
                let table = table.rsplit("::").next().unwrap_or_default().to_owned();
                format!(
                    "pub type {name} = {user};\npub(pkg) static {table}: {W}::Table<{user}> = unsafe {{ {W}::Table::new() }};\n"
                )
            }
            TypeKind::Resource => {
                let (_, _, drop, _) = self.resource_fns(id);
                format!(
                    "pub struct {name} {{\n    pub(pkg) handle: {W}::Handle;\n}}\n\
                     impl rok::core::marker::Drop for {name} {{\n    fn drop(ref self) {{\n        \
                     match self.handle.release() {{\n            h: i32 => {{ unsafe {{ {drop}(h); }} }}\n            \
                     none => {{}}\n        }}\n    }}\n}}\n"
                )
            }
            TypeKind::Unresolved => return Err(format!("type `{}` is unresolved", t.name)),
        };
        self.push(&m, &text);
        Ok(())
    }
}

/// One function body being generated.
struct Fx<'g, 'a> {
    g: &'g mut Gen<'a>,
    out: String,
    n: usize,
}

impl Fx<'_, '_> {
    fn tmp(&mut self) -> String {
        self.n += 1;
        format!("w_{}", self.n)
    }

    fn line(&mut self, s: &str) {
        self.out.push_str("        ");
        self.out.push_str(s);
        self.out.push('\n');
    }

    /// `let w_N: ty = expr;`, its name.
    fn let_(&mut self, ty: &str, expr: &str) -> String {
        let v = self.tmp();
        self.line(&format!("let {v}: {ty} = {expr};"));
        v
    }

    fn lower(&mut self, t: &Type, v: &str) -> Result<Vec<String>, String> {
        Ok(match self.g.view(t) {
            View::Prim(p) => match p {
                Prim::Bool => vec![self.let_("i32", &format!("if {v} {{ 1i32 }} else {{ 0i32 }}"))],
                Prim::U8 | Prim::U16 | Prim::U32 | Prim::S8 | Prim::S16 => {
                    vec![self.let_("i32", &format!("{v} as i32"))]
                }
                Prim::S32 => vec![self.let_("i32", v)],
                Prim::U64 => vec![self.let_("i64", &format!("{v} as i64"))],
                Prim::S64 => vec![self.let_("i64", v)],
                Prim::F32 => vec![self.let_("f32", v)],
                Prim::F64 => vec![self.let_("f64", v)],
                Prim::Char => vec![self.let_("i32", &format!("({v} as u32) as i32"))],
                Prim::String => {
                    let (p, n) = (self.tmp(), self.tmp());
                    self.line(&format!("let ({p}, {n}) = w_b.str({v});"));
                    vec![p, n]
                }
            },
            View::List(e) => {
                let (p, n) = self.lower_list(&e, v)?;
                vec![p, n]
            }
            View::Tuple(ts) => {
                let mut out = Vec::new();
                for (i, ft) in ts.iter().enumerate() {
                    let f = self.let_(&self.g.rty(ft)?, &format!("{v}.{i}"));
                    out.extend(self.lower(ft, &f)?);
                }
                out
            }
            View::Record(_, fs) => {
                let mut out = Vec::new();
                for (name, ft) in &fs {
                    let f = self.let_(&self.g.rty(ft)?, &format!("{v}.{}", snake(name)));
                    out.extend(self.lower(ft, &f)?);
                }
                out
            }
            View::FixedList(e, n) => {
                let mut out = Vec::new();
                for i in 0..n {
                    let x = self.let_(&self.g.rty(&e)?, &format!("{v}[{i}]"));
                    out.extend(self.lower(&e, &x)?);
                }
                out
            }
            View::Future | View::Stream => {
                let ops = self.g.ops_fn(t)?;
                let f = if matches!(t, Type::Future(_)) {
                    "future_out"
                } else {
                    "stream_out"
                };
                vec![self.let_("i32", &format!("{A}::{f}({ops}(), {v})"))]
            }
            View::ErrorContext => vec![self.let_("i32", &format!("{v}.raw()"))],
            View::Flags(_, names) => vec![self.flag_bits(&names, v)],
            View::Enum(..) => vec![self.let_("i32", &format!("{v} as i32"))],
            View::Option(_) | View::Result(..) | View::Variant(..) => self.lower_variant(t, v)?,
            View::Own(res) => {
                if self.g.ours(res) {
                    let (new, _, _, table) = self.g.resource_fns(res);
                    vec![self.let_("i32", &format!("{new}({table}.insert({v}))"))]
                } else {
                    vec![self.let_("i32", &format!("{v}.handle.take()"))]
                }
            }
            View::Borrow(res) => {
                // An import's types are imports: its `borrow<r>` is never
                // the component's own exported `r`.
                if self.g.ours(res) {
                    return Err(format!(
                        "a `borrow<{}>` of a resource this component exports cannot be passed to an import",
                        self.g.r.types[res].name
                    ));
                }
                vec![self.let_("i32", &format!("{v}.handle.get()"))]
            }
        })
    }

    /// The values of `f`'s parameters an export receives: core
    /// parameters `names`, or a block at `a0` (freed).
    fn lift_params(
        &mut self,
        f: &Function,
        pmem: bool,
        names: Vec<String>,
    ) -> Result<Vec<String>, String> {
        let r = self.g.r;
        let mut vals = Vec::new();
        if pmem {
            let types: Vec<Type> = f.params.iter().map(|(_, t)| t.clone()).collect();
            for (t, off) in types.iter().zip(abi::field_offsets(r, &types)) {
                vals.push(self.load(t, &format!("a0 + {off}i32"))?);
            }
            self.line(&format!("{W}::free(a0);"));
        } else {
            let mut it = names.into_iter();
            for (_, t) in &f.params {
                vals.push(self.lift(t, &mut it)?);
            }
        }
        Ok(vals)
    }

    fn flag_bits(&mut self, names: &[String], v: &str) -> String {
        let x = self.let_("i32", "0i32");
        for (i, n) in names.iter().enumerate() {
            self.line(&format!(
                "if {v}.{} {{ {x} = {x} | (1u32 << {i}u32) as i32; }}",
                snake(n)
            ));
        }
        x
    }

    /// The case patterns of a variant-like `t`: `(pattern, binding)`.
    fn patterns(&mut self, t: &Type) -> Result<Vec<(String, Option<String>)>, String> {
        Ok(match self.g.view(t) {
            View::Variant(id, cs) => {
                let path = self.g.type_path(id);
                cs.iter()
                    .map(|(c, ct)| match ct {
                        Some(_) => {
                            let b = self.tmp();
                            (format!("{path}::{}({b})", pascal(c)), Some(b))
                        }
                        None => (format!("{path}::{}", pascal(c)), None),
                    })
                    .collect()
            }
            View::Option(e) => {
                let b = self.tmp();
                vec![
                    ("none".into(), None),
                    (format!("{b}: {}", self.g.rty(&e)?), Some(b)),
                ]
            }
            View::Result(a, b) => {
                let mut out = Vec::new();
                for (ctor, t) in [("Ok", a), ("Err", b)] {
                    if t.is_some() {
                        let v = self.tmp();
                        out.push((format!("{RESULT}::{ctor}({v})"), Some(v)));
                    } else {
                        out.push((format!("{RESULT}::{ctor}(_)"), None));
                    }
                }
                out
            }
            _ => unreachable!("variant-like"),
        })
    }

    /// The value of case `i` of variant-like `t` holding `payload`.
    fn construct(&self, t: &Type, i: usize, payload: Option<&str>) -> String {
        match self.g.view(t) {
            View::Variant(id, cs) => {
                let c = format!("{}::{}", self.g.type_path(id), pascal(&cs[i].0));
                payload.map_or(c.clone(), |p| format!("{c}({p})"))
            }
            View::Option(_) => payload.map_or("none".into(), str::to_owned),
            View::Result(..) => format!(
                "{RESULT}::{}({})",
                if i == 0 { "Ok" } else { "Err" },
                payload.unwrap_or("()")
            ),
            _ => unreachable!("variant-like"),
        }
    }

    fn lower_variant(&mut self, t: &Type, v: &str) -> Result<Vec<String>, String> {
        let cs = abi::cases(self.g.r, t).unwrap_or_default();
        let flat = abi::flatten_cases(self.g.r, &cs);
        let d = self.let_("i32", "0i32");
        let slots: Vec<String> = flat[1..]
            .iter()
            .map(|f| self.let_(f.rok(), zero(*f)))
            .collect();
        let pats = self.patterns(t)?;
        self.line(&format!("match {v} {{"));
        for (i, ((pat, bind), ct)) in pats.into_iter().zip(&cs).enumerate() {
            self.line(&format!("{pat} => {{"));
            self.line(&format!("{d} = {i}i32;"));
            if let (Some(b), Some(ct)) = (bind, ct) {
                let vals = self.lower(ct, &b)?;
                let pf = abi::flatten(self.g.r, ct);
                for (j, val) in vals.iter().enumerate() {
                    let c = coerce(pf[j], flat[j + 1], val);
                    self.line(&format!("{} = {c};", slots[j]));
                }
            }
            self.line("}");
        }
        self.line("}");
        let mut out = vec![d];
        out.extend(slots);
        Ok(out)
    }

    fn lift(&mut self, t: &Type, vals: &mut std::vec::IntoIter<String>) -> Result<String, String> {
        let mut next = || vals.next().unwrap_or_else(|| "0i32".into());
        Ok(match self.g.view(t) {
            View::Prim(p) => {
                let x = next();
                match p {
                    Prim::Bool => self.let_("bool", &format!("{x} != 0i32")),
                    Prim::U8 => self.let_("u8", &format!("{x} as u8")),
                    Prim::U16 => self.let_("u16", &format!("{x} as u16")),
                    Prim::U32 => self.let_("u32", &format!("{x} as u32")),
                    Prim::S8 => self.let_("i8", &format!("{x} as i8")),
                    Prim::S16 => self.let_("i16", &format!("{x} as i16")),
                    Prim::U64 => self.let_("u64", &format!("{x} as u64")),
                    Prim::S32 | Prim::S64 | Prim::F32 | Prim::F64 => x,
                    Prim::Char => self.let_("char", &format!("{W}::char_from({x})")),
                    Prim::String => {
                        let n = next();
                        self.let_("str", &format!("{W}::str_take({x}, {n})"))
                    }
                }
            }
            View::List(e) => {
                let (p, n) = (next(), next());
                self.lift_list(&e, &p, &n)?
            }
            View::Tuple(ts) => {
                let mut parts = Vec::new();
                for ft in &ts {
                    parts.push(self.lift(ft, vals)?);
                }
                self.let_(&self.g.rty(t)?, &format!("({},)", parts.join(", ")))
            }
            View::Record(id, fs) => {
                let mut parts = Vec::new();
                for (name, ft) in &fs {
                    let v = self.lift(ft, vals)?;
                    parts.push(format!("{}: {v}", snake(name)));
                }
                self.let_(
                    &self.g.rty(t)?,
                    &format!("{} {{ {} }}", self.g.type_path(id), parts.join(", ")),
                )
            }
            View::FixedList(e, n) => {
                let mut parts = Vec::new();
                for _ in 0..n {
                    parts.push(self.lift(&e, vals)?);
                }
                self.let_(&self.g.rty(t)?, &format!("[{}]", parts.join(", ")))
            }
            View::Future | View::Stream => {
                let h = next();
                let ops = self.g.ops_fn(t)?;
                let f = if matches!(t, Type::Future(_)) {
                    "future_in"
                } else {
                    "stream_in"
                };
                self.let_(&self.g.rty(t)?, &format!("{A}::{f}({ops}(), {h})"))
            }
            View::ErrorContext => {
                let h = next();
                self.let_(
                    &self.g.rty(t)?,
                    &format!("{W}::ErrorContext::from_raw({h})"),
                )
            }
            View::Flags(id, names) => {
                let x = next();
                self.flags_value(id, &names, &x)
            }
            View::Enum(id, n) => {
                let x = next();
                self.enum_value(id, n, &x)
            }
            View::Option(_) | View::Result(..) | View::Variant(..) => {
                let cs = abi::cases(self.g.r, t).unwrap_or_default();
                let flat = abi::flatten_cases(self.g.r, &cs);
                let d = next();
                let slots: Vec<String> = flat[1..]
                    .iter()
                    .map(|_| vals.next().unwrap_or_default())
                    .collect();
                let res = self.tmp();
                self.line(&format!("let {res}: {} = match {d} {{", self.g.rty(t)?));
                for (i, ct) in cs.iter().enumerate() {
                    self.line(&format!("{i}i32 => {{"));
                    let payload = match ct {
                        Some(ct) => {
                            let pf = abi::flatten(self.g.r, ct);
                            let mut cvals = Vec::new();
                            for (j, f) in pf.iter().enumerate() {
                                cvals.push(
                                    self.let_(f.rok(), &uncoerce(flat[j + 1], *f, &slots[j])),
                                );
                            }
                            Some(self.lift(ct, &mut cvals.into_iter())?)
                        }
                        None => None,
                    };
                    let c = self.construct(t, i, payload.as_deref());
                    self.line(&c);
                    self.line("}");
                }
                self.line(&format!("_ => {W}::bad_case(),"));
                self.line("};");
                res
            }
            View::Own(res) => {
                let h = next();
                self.lift_handle(res, &h, true)
            }
            View::Borrow(res) => {
                let h = next();
                self.lift_handle(res, &h, false)
            }
        })
    }

    fn lift_handle(&mut self, res: TypeId, h: &str, own: bool) -> String {
        let ty = self.g.type_path(res);
        if self.g.ours(res) {
            let (_, rep, drop, table) = self.g.resource_fns(res);
            if own {
                let r = self.let_("i32", &format!("{rep}({h})"));
                // Out of the table first: the handle's drop runs the
                // `[dtor]`, which then finds no value to drop.
                let v = self.let_(&ty, &format!("{table}.get({r})"));
                self.line(&format!("{table}.take({r});"));
                self.line(&format!("{drop}({h});"));
                v
            } else {
                // A borrow of one's own resource arrives as its rep.
                self.let_(&ty, &format!("{table}.get({h})"))
            }
        } else {
            self.g.resource_fns(res);
            let how = if own { "own" } else { "borrowed" };
            self.let_(&ty, &format!("{ty} {{ handle: {W}::Handle::{how}({h}) }}"))
        }
    }

    fn flags_value(&mut self, id: TypeId, names: &[String], x: &str) -> String {
        let fields: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, n)| format!("{}: ({x} & (1u32 << {i}u32) as i32) != 0i32", snake(n)))
            .collect();
        let path = self.g.type_path(id);
        self.let_(&path, &format!("{path} {{ {} }}", fields.join(", ")))
    }

    fn enum_value(&mut self, id: TypeId, n: usize, x: &str) -> String {
        let path = self.g.type_path(id);
        let TypeKind::Enum(cs) = &self.g.r.types[id].kind else {
            unreachable!("enum")
        };
        let mut arms: Vec<String> = cs
            .iter()
            .take(n)
            .enumerate()
            .map(|(i, c)| format!("{i}i32 => {path}::{}", pascal(c)))
            .collect();
        arms.push(format!("_ => {W}::bad_case()"));
        self.let_(&path, &format!("match {x} {{ {} }}", arms.join(", ")))
    }

    fn lower_list(&mut self, e: &Type, v: &str) -> Result<(String, String), String> {
        if matches!(self.g.view(e), View::Prim(Prim::U8)) {
            let (p, n) = (self.tmp(), self.tmp());
            self.line(&format!("let ({p}, {n}) = w_b.bytes({v});"));
            return Ok((p, n));
        }
        let size = abi::size(self.g.r, e);
        let align = abi::align(self.g.r, e);
        let n = self.let_("i32", &format!("{v}.len() as i32"));
        let p = self.let_("i32", &format!("w_b.alloc({n} * {size}i32, {align}i32)"));
        let i = self.tmp();
        self.line(&format!("for {i} in 0..{v}.len() {{"));
        let x = self.let_(&self.g.rty(e)?, &format!("{v}[{i}]"));
        self.store(e, &x, &format!("{p} + ({i} as i32) * {size}i32"))?;
        self.line("}");
        Ok((p, n))
    }

    fn lift_list(&mut self, e: &Type, p: &str, n: &str) -> Result<String, String> {
        let ty = format!("{LIST}<{}>", self.g.rty(e)?);
        if matches!(self.g.view(e), View::Prim(Prim::U8)) {
            return Ok(self.let_(&ty, &format!("{W}::bytes_take({p}, {n})")));
        }
        let size = abi::size(self.g.r, e);
        let out = self.let_(&ty, &format!("{LIST}::new()"));
        let i = self.tmp();
        self.line(&format!("for {i} in 0..({n} as u32 as usize) {{"));
        let x = self.load(e, &format!("{p} + ({i} as i32) * {size}i32"))?;
        self.line(&format!("{out}.push({x});"));
        self.line("}");
        self.line(&format!("if {n} != 0i32 {{ {W}::free({p}); }}"));
        Ok(out)
    }

    fn store_disc(&mut self, a: &str, d: &str, cases: usize) {
        match abi::disc_size(cases) {
            1 => self.line(&format!("{W}::store_u8({a}, ({d}) as u8);")),
            2 => self.line(&format!("{W}::store_u16({a}, ({d}) as u16);")),
            _ => self.line(&format!("{W}::store_i32({a}, {d});")),
        }
    }

    fn load_disc(&mut self, a: &str, cases: usize) -> String {
        match abi::disc_size(cases) {
            1 => self.let_("i32", &format!("{W}::load_u8({a}) as i32")),
            2 => self.let_("i32", &format!("{W}::load_u16({a}) as i32")),
            _ => self.let_("i32", &format!("{W}::load_i32({a})")),
        }
    }

    fn store(&mut self, t: &Type, v: &str, a: &str) -> Result<(), String> {
        match self.g.view(t) {
            View::Prim(p) => {
                let s = match p {
                    Prim::Bool => format!("store_u8({a}, if {v} {{ 1u8 }} else {{ 0u8 }})"),
                    Prim::U8 => format!("store_u8({a}, {v})"),
                    Prim::S8 => format!("store_u8({a}, {v} as u8)"),
                    Prim::U16 => format!("store_u16({a}, {v})"),
                    Prim::S16 => format!("store_u16({a}, {v} as u16)"),
                    Prim::U32 => format!("store_i32({a}, {v} as i32)"),
                    Prim::S32 => format!("store_i32({a}, {v})"),
                    Prim::U64 => format!("store_i64({a}, {v} as i64)"),
                    Prim::S64 => format!("store_i64({a}, {v})"),
                    Prim::F32 => format!("store_f32({a}, {v})"),
                    Prim::F64 => format!("store_f64({a}, {v})"),
                    Prim::Char => format!("store_i32({a}, ({v} as u32) as i32)"),
                    Prim::String => {
                        let (p, n) = (self.tmp(), self.tmp());
                        self.line(&format!("let ({p}, {n}) = w_b.str({v});"));
                        format!("store_i32({a}, {p}); {W}::store_i32({a} + 4i32, {n})")
                    }
                };
                self.line(&format!("{W}::{s};"));
            }
            View::List(e) => {
                let (p, n) = self.lower_list(&e, v)?;
                self.line(&format!(
                    "{W}::store_i32({a}, {p}); {W}::store_i32({a} + 4i32, {n});"
                ));
            }
            View::Tuple(ts) => {
                let offs = abi::field_offsets(self.g.r, &ts);
                for (i, (ft, off)) in ts.iter().zip(offs).enumerate() {
                    let f = self.let_(&self.g.rty(ft)?, &format!("{v}.{i}"));
                    self.store(ft, &f, &format!("{a} + {off}i32"))?;
                }
            }
            View::Record(_, fs) => {
                let types: Vec<Type> = fs.iter().map(|(_, t)| t.clone()).collect();
                let offs = abi::field_offsets(self.g.r, &types);
                for ((name, ft), off) in fs.iter().zip(offs) {
                    let f = self.let_(&self.g.rty(ft)?, &format!("{v}.{}", snake(name)));
                    self.store(ft, &f, &format!("{a} + {off}i32"))?;
                }
            }
            View::FixedList(e, n) => {
                let size = abi::size(self.g.r, &e);
                let i = self.tmp();
                self.line(&format!("for {i} in 0..{n}usize {{"));
                let x = self.let_(&self.g.rty(&e)?, &format!("{v}[{i}]"));
                self.store(&e, &x, &format!("{a} + ({i} as i32) * {size}i32"))?;
                self.line("}");
            }
            View::Future | View::Stream | View::ErrorContext => {
                let h = self.lower(t, v)?.remove(0);
                self.line(&format!("{W}::store_i32({a}, {h});"));
            }
            View::Flags(_, names) => {
                let x = self.flag_bits(&names, v);
                match abi::flags_size(names.len()) {
                    0 => {}
                    1 => self.line(&format!("{W}::store_u8({a}, {x} as u8);")),
                    2 => self.line(&format!("{W}::store_u16({a}, {x} as u16);")),
                    _ => self.line(&format!("{W}::store_i32({a}, {x});")),
                }
            }
            View::Enum(_, n) => self.store_disc(a, &format!("{v} as i32"), n),
            View::Option(_) | View::Result(..) | View::Variant(..) => {
                let cs = abi::cases(self.g.r, t).unwrap_or_default();
                let off = abi::payload_offset(self.g.r, &cs);
                let pats = self.patterns(t)?;
                self.line(&format!("match {v} {{"));
                for (i, ((pat, bind), ct)) in pats.into_iter().zip(&cs).enumerate() {
                    self.line(&format!("{pat} => {{"));
                    self.store_disc(a, &format!("{i}i32"), cs.len());
                    if let (Some(b), Some(ct)) = (bind, ct) {
                        self.store(ct, &b, &format!("{a} + {off}i32"))?;
                    }
                    self.line("}");
                }
                self.line("}");
            }
            View::Own(_) | View::Borrow(_) => {
                let h = self.lower(t, v)?.remove(0);
                self.line(&format!("{W}::store_i32({a}, {h});"));
            }
        }
        Ok(())
    }

    fn load(&mut self, t: &Type, a: &str) -> Result<String, String> {
        Ok(match self.g.view(t) {
            View::Prim(p) => {
                let (ty, e) = match p {
                    Prim::Bool => ("bool", format!("{W}::load_u8({a}) != 0u8")),
                    Prim::U8 => ("u8", format!("{W}::load_u8({a})")),
                    Prim::S8 => ("i8", format!("{W}::load_u8({a}) as i8")),
                    Prim::U16 => ("u16", format!("{W}::load_u16({a})")),
                    Prim::S16 => ("i16", format!("{W}::load_u16({a}) as i16")),
                    Prim::U32 => ("u32", format!("{W}::load_i32({a}) as u32")),
                    Prim::S32 => ("i32", format!("{W}::load_i32({a})")),
                    Prim::U64 => ("u64", format!("{W}::load_i64({a}) as u64")),
                    Prim::S64 => ("i64", format!("{W}::load_i64({a})")),
                    Prim::F32 => ("f32", format!("{W}::load_f32({a})")),
                    Prim::F64 => ("f64", format!("{W}::load_f64({a})")),
                    Prim::Char => ("char", format!("{W}::char_from({W}::load_i32({a}))")),
                    Prim::String => {
                        let p = self.let_("i32", &format!("{W}::load_i32({a})"));
                        let n = self.let_("i32", &format!("{W}::load_i32({a} + 4i32)"));
                        ("str", format!("{W}::str_take({p}, {n})"))
                    }
                };
                self.let_(ty, &e)
            }
            View::List(e) => {
                let p = self.let_("i32", &format!("{W}::load_i32({a})"));
                let n = self.let_("i32", &format!("{W}::load_i32({a} + 4i32)"));
                self.lift_list(&e, &p, &n)?
            }
            View::Tuple(ts) => {
                let offs = abi::field_offsets(self.g.r, &ts);
                let mut parts = Vec::new();
                for (ft, off) in ts.iter().zip(offs) {
                    parts.push(self.load(ft, &format!("{a} + {off}i32"))?);
                }
                self.let_(&self.g.rty(t)?, &format!("({},)", parts.join(", ")))
            }
            View::Record(id, fs) => {
                let types: Vec<Type> = fs.iter().map(|(_, t)| t.clone()).collect();
                let offs = abi::field_offsets(self.g.r, &types);
                let mut parts = Vec::new();
                for ((name, ft), off) in fs.iter().zip(offs) {
                    let v = self.load(ft, &format!("{a} + {off}i32"))?;
                    parts.push(format!("{}: {v}", snake(name)));
                }
                self.let_(
                    &self.g.rty(t)?,
                    &format!("{} {{ {} }}", self.g.type_path(id), parts.join(", ")),
                )
            }
            View::FixedList(e, n) => {
                let size = abi::size(self.g.r, &e);
                let first = self.load(&e, a)?;
                let arr = self.let_(&self.g.rty(t)?, &format!("[{first}; {n}]"));
                let i = self.tmp();
                self.line(&format!("for {i} in 1..{n}usize {{"));
                let x = self.load(&e, &format!("{a} + ({i} as i32) * {size}i32"))?;
                self.line(&format!("{arr}[{i}] = {x};"));
                self.line("}");
                arr
            }
            View::Future | View::Stream | View::ErrorContext => {
                let h = self.let_("i32", &format!("{W}::load_i32({a})"));
                self.lift(t, &mut vec![h].into_iter())?
            }
            View::Flags(id, names) => {
                let x = match abi::flags_size(names.len()) {
                    0 => "0i32".to_owned(),
                    1 => format!("{W}::load_u8({a}) as i32"),
                    2 => format!("{W}::load_u16({a}) as i32"),
                    _ => format!("{W}::load_i32({a})"),
                };
                let x = self.let_("i32", &x);
                self.flags_value(id, &names, &x)
            }
            View::Enum(id, n) => {
                let d = self.load_disc(a, n);
                self.enum_value(id, n, &d)
            }
            View::Option(_) | View::Result(..) | View::Variant(..) => {
                let cs = abi::cases(self.g.r, t).unwrap_or_default();
                let off = abi::payload_offset(self.g.r, &cs);
                let d = self.load_disc(a, cs.len());
                let res = self.tmp();
                self.line(&format!("let {res}: {} = match {d} {{", self.g.rty(t)?));
                for (i, ct) in cs.iter().enumerate() {
                    self.line(&format!("{i}i32 => {{"));
                    let payload = match ct {
                        Some(ct) => Some(self.load(ct, &format!("{a} + {off}i32"))?),
                        None => None,
                    };
                    let c = self.construct(t, i, payload.as_deref());
                    self.line(&c);
                    self.line("}");
                }
                self.line(&format!("_ => {W}::bad_case(),"));
                self.line("};");
                res
            }
            View::Own(_) | View::Borrow(_) => {
                let h = self.let_("i32", &format!("{W}::load_i32({a})"));
                self.lift(t, &mut vec![h].into_iter())?
            }
        })
    }

    /// Free what lowering `t` at `a` allocated (a post-return function).
    fn free_mem(&mut self, t: &Type, a: &str) {
        if !self.g.has_heap(t) {
            return;
        }
        match self.g.view(t) {
            View::Prim(_) => {
                self.line(&format!(
                    "if {W}::load_i32({a} + 4i32) != 0i32 {{ {W}::free({W}::load_i32({a})); }}"
                ));
            }
            View::List(e) => {
                let p = self.let_("i32", &format!("{W}::load_i32({a})"));
                let n = self.let_("i32", &format!("{W}::load_i32({a} + 4i32)"));
                if self.g.has_heap(&e) {
                    let size = abi::size(self.g.r, &e);
                    let i = self.tmp();
                    self.line(&format!("for {i} in 0..{n} {{"));
                    self.free_mem(&e, &format!("{p} + {i} * {size}i32"));
                    self.line("}");
                }
                self.line(&format!("if {n} != 0i32 {{ {W}::free({p}); }}"));
            }
            View::Tuple(ts) => {
                for (ft, off) in ts.iter().zip(abi::field_offsets(self.g.r, &ts)) {
                    self.free_mem(ft, &format!("{a} + {off}i32"));
                }
            }
            View::Record(_, fs) => {
                let types: Vec<Type> = fs.into_iter().map(|(_, t)| t).collect();
                for (ft, off) in types.iter().zip(abi::field_offsets(self.g.r, &types)) {
                    self.free_mem(ft, &format!("{a} + {off}i32"));
                }
            }
            View::Option(_) | View::Result(..) | View::Variant(..) => {
                let cs = abi::cases(self.g.r, t).unwrap_or_default();
                let off = abi::payload_offset(self.g.r, &cs);
                let d = self.load_disc(a, cs.len());
                self.line(&format!("match {d} {{"));
                for (i, ct) in cs.iter().enumerate() {
                    if let Some(ct) = ct.as_ref().filter(|c| self.g.has_heap(c)) {
                        self.line(&format!("{i}i32 => {{"));
                        self.free_mem(ct, &format!("{a} + {off}i32"));
                        self.line("}");
                    }
                }
                self.line("_ => {}");
                self.line("}");
            }
            View::FixedList(e, n) => {
                let size = abi::size(self.g.r, &e);
                let i = self.tmp();
                self.line(&format!("for {i} in 0..{n}i32 {{"));
                self.free_mem(&e, &format!("{a} + {i} * {size}i32"));
                self.line("}");
            }
            View::Enum(..)
            | View::Flags(..)
            | View::Own(_)
            | View::Borrow(_)
            | View::Future
            | View::Stream
            | View::ErrorContext => {}
        }
    }
}

/// The call of the user's function `call` with `vals` (a method's
/// first value its receiver).
fn user_call(f: &Function, call: &str, vals: &[String]) -> String {
    match f.kind {
        FnKind::Method(_) => format!("{}.{call}({})", vals[0], vals[1..].join(", ")),
        _ => format!("{call}({})", vals.join(", ")),
    }
}

/// The core signature of `f`: flat params (one pointer when there are
/// too many) and whether results go through memory.
fn core_sig(r: &Resolve, f: &Function) -> (Vec<Flat>, Vec<Flat>, bool, bool) {
    let pflat: Vec<Flat> = f
        .params
        .iter()
        .flat_map(|(_, t)| abi::flatten(r, t))
        .collect();
    let rflat = f
        .result
        .as_ref()
        .map(|t| abi::flatten(r, t))
        .unwrap_or_default();
    let pmem = pflat.len() > MAX_FLAT_PARAMS;
    let rmem = rflat.len() > MAX_FLAT_RESULTS;
    (pflat, rflat, pmem, rmem)
}

/// `result<T, E>` with an error type: `(T, E)`.
fn throwing(t: Option<&Type>) -> Option<(Option<Type>, Type)> {
    match t {
        Some(Type::Result(ok, Some(err))) => Some((ok.as_deref().cloned(), (**err).clone())),
        _ => None,
    }
}

impl Gen<'_> {
    /// The futures and streams in `t`, in `wit-parser`'s order (each one
    /// after those of its value type), through aliases.
    fn payloads(&self, t: &Type, out: &mut Vec<Type>, depth: usize) {
        if depth > 64 {
            return;
        }
        match t {
            Type::Prim(_) | Type::Own(_) | Type::Borrow(_) | Type::ErrorContext => {}
            Type::List(e) | Type::Option(e) | Type::FixedList(e, _) => {
                self.payloads(e, out, depth + 1);
            }
            Type::Result(a, b) => {
                for x in [a, b].into_iter().flatten() {
                    self.payloads(x, out, depth + 1);
                }
            }
            Type::Tuple(ts) => {
                for x in ts {
                    self.payloads(x, out, depth + 1);
                }
            }
            Type::Future(e) | Type::Stream(e) => {
                if let Some(e) = e {
                    self.payloads(e, out, depth + 1);
                }
                out.push(t.clone());
            }
            Type::Id(id) => match &self.r.types[*id].kind {
                TypeKind::Alias(a) => self.payloads(a, out, depth + 1),
                TypeKind::Record(fs) => {
                    for (_, x) in fs {
                        self.payloads(x, out, depth + 1);
                    }
                }
                TypeKind::Variant(cs) => {
                    for x in cs.iter().filter_map(|(_, t)| t.as_ref()) {
                        self.payloads(x, out, depth + 1);
                    }
                }
                _ => {}
            },
        }
    }

    /// `t` through its aliases.
    fn unalias(&self, t: &Type) -> Type {
        let mut t = t.clone();
        loop {
            let Type::Id(id) = &t else { return t };
            match &self.r.types[*id].kind {
                TypeKind::Alias(a) => t = a.clone(),
                _ => return t,
            }
        }
    }

    /// Bind `f`, whose built-ins live in core module `module`, next.
    fn set_ctx(&mut self, module: &str, f: &Function) {
        let mut payloads = Vec::new();
        for (_, t) in &f.params {
            self.payloads(t, &mut payloads, 0);
        }
        if let Some(t) = &f.result {
            self.payloads(t, &mut payloads, 0);
        }
        let payloads = payloads.iter().map(|t| self.unalias(t)).collect();
        self.cur = Some(FnCtx {
            module: module.to_owned(),
            name: f.wit_name(self.r),
            payloads,
        });
    }

    /// Path of the function giving the ops (`FutureOps` / `StreamOps`) of
    /// future or stream type `t`, named after the bound function and `t`'s
    /// place in it.
    fn ops_fn(&mut self, t: &Type) -> Result<String, String> {
        let t = self.unalias(t);
        let ctx = self
            .cur
            .clone()
            .ok_or("a future or stream outside a function")?;
        let idx = ctx
            .payloads
            .iter()
            .position(|p| *p == t)
            .ok_or("a future or stream not in its function's type")?;
        let (kind, elem) = match &t {
            Type::Future(e) => ("future", e.as_deref().cloned()),
            Type::Stream(e) => ("stream", e.as_deref().cloned()),
            _ => unreachable!("future or stream"),
        };
        let fname = &ctx.name;
        let id = ident("wit_ops", &ctx.module, &format!("{kind}{idx}{fname}"));
        let path = format!("{}::{id}", self.base);
        if !self.ops.insert(id.clone()) {
            return Ok(path);
        }
        let module = &ctx.module;
        let new = self.extern_fn(
            &[],
            module,
            &format!("[{kind}-new-{idx}]{fname}"),
            &[],
            Some(Flat::I64),
        );
        let future = kind == "future";
        let rw: &[Flat] = if future {
            &[Flat::I32; 2]
        } else {
            &[Flat::I32; 3]
        };
        let [read, write] = ["read", "write"].map(|op| {
            self.extern_fn(
                &[],
                module,
                &format!("[async-lower][{kind}-{op}-{idx}]{fname}"),
                rw,
                Some(Flat::I32),
            )
        });
        let [dr, dw] = ["readable", "writable"].map(|end| {
            self.extern_fn(
                &[],
                module,
                &format!("[{kind}-drop-{end}-{idx}]{fname}"),
                &[Flat::I32],
                None,
            )
        });
        let (ty, size, align, lift, lower) = match &elem {
            None => (
                "()".to_owned(),
                0,
                1,
                "        ()\n".to_owned(),
                String::new(),
            ),
            Some(e) => {
                let ty = self.rty(e)?;
                let mut fx = Fx {
                    g: self,
                    out: String::new(),
                    n: 0,
                };
                let v = fx.load(e, "a")?;
                let lift = format!("{}        {v}\n", fx.out);
                let mut fx = Fx {
                    g: self,
                    out: String::new(),
                    n: 0,
                };
                fx.store(e, "v", "a")?;
                let lower = fx.out;
                (ty, abi::size(self.r, e), abi::align(self.r, e), lift, lower)
            }
        };
        let ops = if future { "FutureOps" } else { "StreamOps" };
        let (rp, ra) = if future {
            ("h: i32, a: i32", "h, a")
        } else {
            ("h: i32, a: i32, n: i32", "h, a, n")
        };
        let text = format!(
            "pub(pkg) fn {id}() -> {A}::{ops}<{ty}> {{
    {A}::{ops} {{
        new: || -> i64 {{ unsafe {{ {new}() }} }},
        read: |{rp}| -> i32 {{ unsafe {{ {read}({ra}) }} }},
        write: |{rp}| -> i32 {{ unsafe {{ {write}({ra}) }} }},
        drop_readable: |h: i32| {{ unsafe {{ {dr}(h); }} }},
        drop_writable: |h: i32| {{ unsafe {{ {dw}(h); }} }},
        size: {size}i32,
        align: {align}i32,
        lift: |a: i32| -> {ty} {{
    unsafe {{
{lift}    }}
        }},
        lower: |v: {ty}, a: i32, w_b: {W}::Blocks| {{
    unsafe {{
{lower}    }}
        }},
    }}
}}
"
        );
        self.push(&[], &text);
        Ok(path)
    }

    /// The rok function calling import `f`. An `async func` starts a subtask
    /// its future awaits; arguments and result stay in memory until it
    /// returns.
    fn import_fn(&mut self, m: &[String], module: &str, f: &Function) -> Result<String, String> {
        let r = self.r;
        self.set_ctx(module, f);
        let (pflat, rflat, ..) = core_sig(r, f);
        let max = if f.is_async {
            MAX_FLAT_ASYNC_PARAMS
        } else {
            MAX_FLAT_PARAMS
        };
        let pmem = pflat.len() > max;
        let rmem = (f.is_async && f.result.is_some()) || rflat.len() > MAX_FLAT_RESULTS;
        let mut cparams = if pmem { vec![Flat::I32] } else { pflat };
        if rmem {
            cparams.push(Flat::I32);
        }
        let cret = if f.is_async {
            Some(Flat::I32)
        } else if rmem {
            None
        } else {
            rflat.first().copied()
        };
        let core = if f.is_async {
            format!("[async-lower]{}", f.wit_name(r))
        } else {
            f.wit_name(r)
        };
        let ext = self.extern_fn(m, module, &core, &cparams, cret);
        let throws = throwing(f.result.as_ref());
        let ret = match (&throws, &f.result) {
            (Some((ok, err)), _) => {
                let ok = ok.as_ref().map(|t| self.rty(t)).transpose()?;
                format!(
                    " throws ({}){}",
                    self.rty(err)?,
                    ok.map(|t| format!(" -> {t}")).unwrap_or_default()
                )
            }
            (None, Some(t)) => format!(" -> {}", self.rty(t)?),
            (None, None) => String::new(),
        };
        let mut params = Vec::new();
        let mut vars = Vec::new();
        for (i, (n, t)) in f.params.iter().enumerate() {
            if i == 0 && matches!(f.kind, FnKind::Method(_)) {
                params.push("self".to_owned());
                vars.push("self".to_owned());
            } else {
                params.push(format!("{}: {}", snake(n), self.rty(t)?));
                vars.push(snake(n));
            }
        }
        let name = match f.kind {
            FnKind::Constructor(_) => "new".to_owned(),
            _ => snake(&f.name),
        };
        let rt = f.result.clone().unwrap_or(Type::Tuple(Vec::new()));
        let mut pre = format!("        let w_b = {W}::Blocks::new();\n");
        if rmem {
            let _ = writeln!(
                pre,
                "        let w_ra: i32 = w_b.alloc({}i32, {}i32);",
                abi::size(r, &rt),
                abi::align(r, &rt)
            );
        }
        let mut fx = Fx {
            g: self,
            out: String::new(),
            n: 0,
        };
        let mut args = Vec::new();
        if pmem {
            let types: Vec<Type> = f.params.iter().map(|(_, t)| t.clone()).collect();
            let rec = Type::Tuple(types.clone());
            let area = fx.let_(
                "i32",
                &format!(
                    "w_b.alloc({}i32, {}i32)",
                    abi::size(r, &rec),
                    abi::align(r, &rec)
                ),
            );
            for ((t, v), off) in types.iter().zip(&vars).zip(abi::field_offsets(r, &types)) {
                fx.store(t, v, &format!("{area} + {off}i32"))?;
            }
            args.push(area);
        } else {
            for ((_, t), v) in f.params.iter().zip(&vars) {
                args.extend(fx.lower(t, v)?);
            }
        }
        if rmem {
            args.push("w_ra".to_owned());
        }
        let call = format!("{ext}({})", args.join(", "));
        // The call's lines; then the result's.
        let mut value = None;
        let lowering = if f.is_async {
            fx.line(&call);
            std::mem::take(&mut fx.out)
        } else {
            if rmem {
                fx.line(&format!("{call};"));
            } else if let (Some(rt), Some(cf)) = (&f.result, cret) {
                let x = fx.let_(cf.rok(), &call);
                value = Some(fx.lift(rt, &mut vec![x].into_iter())?);
            } else {
                fx.line(&format!("{call};"));
            }
            String::new()
        };
        if rmem && f.result.is_some() {
            value = Some(fx.load(&rt, "w_ra")?);
        }
        fx.line("w_b.free();");
        match (&throws, value) {
            (Some((ok, _)), Some(v)) => {
                let okv = if ok.is_some() { "w_ok" } else { "_" };
                fx.line(&format!(
                    "match {v} {{ {RESULT}::Ok({okv}) => {}, {RESULT}::Err(w_err) => throw w_err, }}",
                    if ok.is_some() { "w_ok" } else { "()" }
                ));
            }
            (_, Some(v)) => fx.line(&v),
            _ => {}
        }
        let lifting = fx.out;
        let ps = params.join(", ");
        Ok(if f.is_async {
            format!(
                "    pub async fn {name}({ps}){ret} {{\n{pre}        let w_st: i32 = unsafe {{\n{lowering}        }};\n        await {A}::subtask(w_st);\n        unsafe {{\n{lifting}        }}\n    }}\n"
            )
        } else {
            format!(
                "    pub fn {name}({ps}){ret} {{\n    unsafe {{\n{pre}{lifting}    }}\n    }}\n"
            )
        })
    }

    /// Core export `export` running `f` via `call` (a method: its name);
    /// built-ins in core module `module`.
    fn export_fn(
        &mut self,
        export: &str,
        module: &str,
        f: &Function,
        call: &str,
    ) -> Result<(), String> {
        self.set_ctx(module, f);
        if f.is_async {
            return self.export_async(export, module, f, call);
        }
        let r = self.r;
        let (pflat, rflat, pmem, rmem) = core_sig(r, f);
        let cparams = if pmem { vec![Flat::I32] } else { pflat };
        let cret = if rmem {
            Some(Flat::I32)
        } else {
            rflat.first().copied()
        };
        let id = ident("export", export, "");
        let mut fx = Fx {
            g: self,
            out: String::new(),
            n: 0,
        };
        fx.line(&format!("let w_b = {W}::Blocks::new();"));
        let names: Vec<String> = (0..cparams.len()).map(|i| format!("a{i}")).collect();
        let vals = fx.lift_params(f, pmem, names.clone())?;
        let call = user_call(f, call, &vals);
        let result = f.result.clone();
        let v = match (&result, throwing(result.as_ref())) {
            (Some(rt), Some(_)) => Some(fx.let_(
                &fx.g.rty(rt)?,
                &format!("{RESULT}::catching(|| try {call})"),
            )),
            (Some(rt), None) => Some(fx.let_(&fx.g.rty(rt)?, &call)),
            (None, _) => {
                fx.line(&format!("{call};"));
                None
            }
        };
        let tail = match (&result, v) {
            (Some(rt), Some(v)) if rmem => {
                let ra = fx.let_(
                    "i32",
                    &format!(
                        "w_b.alloc({}i32, {}i32)",
                        abi::size(r, rt),
                        abi::align(r, rt)
                    ),
                );
                fx.store(rt, &v, &ra)?;
                ra
            }
            (Some(rt), Some(v)) => fx.lower(rt, &v)?.remove(0),
            _ => String::new(),
        };
        let body = fx.out;
        let ps: Vec<String> = names
            .iter()
            .zip(&cparams)
            .map(|(n, f)| format!("{n}: {}", f.rok()))
            .collect();
        let ret = cret.map(|f| format!(" -> {}", f.rok())).unwrap_or_default();
        let text = format!(
            "#[export_name(\"{export}\")]\nextern \"C\" fn {id}({}){ret} {{\n    unsafe {{\n{body}        {tail}\n    }}\n}}\n",
            ps.join(", ")
        );
        self.push(&[], &text);
        self.exports.push(export.to_owned());
        if rmem {
            let rt = result.unwrap_or(Type::Tuple(Vec::new()));
            let mut fx = Fx {
                g: self,
                out: String::new(),
                n: 0,
            };
            fx.free_mem(&rt, "a0");
            fx.line(&format!("{W}::free(a0);"));
            let body = fx.out;
            let post = format!("cabi_post_{export}");
            let pid = ident("post", export, "");
            self.push(
                &[],
                &format!("#[export_name(\"{post}\")]\nextern \"C\" fn {pid}(a0: i32) {{\n    unsafe {{\n{body}    }}\n}}\n"),
            );
            self.exports.push(post);
        }
        Ok(())
    }

    /// An `async func` export lifted with a callback: starts the user's
    /// future on the host's event loop, result through `task.return`.
    fn export_async(
        &mut self,
        export: &str,
        module: &str,
        f: &Function,
        call: &str,
    ) -> Result<(), String> {
        let r = self.r;
        let (pflat, rflat, pmem, _) = core_sig(r, f);
        let cparams = if pmem { vec![Flat::I32] } else { pflat };
        let rmem = rflat.len() > MAX_FLAT_PARAMS;
        let task_return = self.extern_fn(
            &[],
            module,
            &format!("[task-return]{}", f.wit_name(r)),
            &if rmem { vec![Flat::I32] } else { rflat },
            None,
        );
        let id = ident("export", export, "");
        let mut fx = Fx {
            g: self,
            out: String::new(),
            n: 0,
        };
        fx.line(&format!("{A}::async_enter();"));
        let names: Vec<String> = (0..cparams.len()).map(|i| format!("a{i}")).collect();
        let vals = fx.lift_params(f, pmem, names.clone())?;
        let mut fut = user_call(f, call, &vals);
        if throwing(f.result.as_ref()).is_some() {
            fut = format!("{A}::catching_async({fut})");
        }
        let body = fx.out;
        let mut gx = Fx {
            g: self,
            out: String::new(),
            n: 0,
        };
        gx.line(&format!("let w_b = {W}::Blocks::new();"));
        let mut args = Vec::new();
        let rty = match &f.result {
            Some(rt) if rmem => {
                let ra = gx.let_(
                    "i32",
                    &format!(
                        "w_b.alloc({}i32, {}i32)",
                        abi::size(r, rt),
                        abi::align(r, rt)
                    ),
                );
                gx.store(rt, "w_r", &ra)?;
                args.push(ra);
                gx.g.rty(rt)?
            }
            Some(rt) => {
                args = gx.lower(rt, "w_r")?;
                gx.g.rty(rt)?
            }
            None => "()".to_owned(),
        };
        gx.line(&format!("{task_return}({});", args.join(", ")));
        gx.line("w_b.free();");
        let ret_body = gx.out;
        let ps: Vec<String> = names
            .iter()
            .zip(&cparams)
            .map(|(n, f)| format!("{n}: {}", f.rok()))
            .collect();
        let lift = format!("[async-lift]{export}");
        let callback = format!("[callback][async-lift]{export}");
        let cid = ident("callback", export, "");
        let text = format!(
            "#[export_name(\"{lift}\")]
extern \"C\" fn {id}({}) -> i32 {{
    unsafe {{
{body}        {A}::async_start({fut}, |w_r: {rty}| {{
    unsafe {{
{ret_body}    }}
        }})
    }}
}}
#[export_name(\"{callback}\")]
extern \"C\" fn {cid}(a0: i32, a1: i32, a2: i32) -> i32 {{
    {A}::async_event(a0, a1, a2)
}}
",
            ps.join(", ")
        );
        self.push(&[], &text);
        self.exports.push(lift);
        self.exports.push(callback);
        Ok(())
    }

    /// Generated code calling the user's exported function `f`: its
    /// path (or method name).
    fn user_fn(&self, f: &Function) -> String {
        match f.kind {
            FnKind::Free => format!("{}::{}", self.base, snake(&f.name)),
            FnKind::Method(_) => snake(&f.name),
            FnKind::Static(t) => format!(
                "{}::{}::{}",
                self.base,
                pascal(&self.r.types[t].name),
                snake(&f.name)
            ),
            FnKind::Constructor(t) => {
                format!("{}::{}::new", self.base, pascal(&self.r.types[t].name))
            }
        }
    }

    fn import_iface(&mut self, i: IfaceId) -> Result<(), String> {
        let m = self.owner_mod(Owner::Iface(i));
        let key = self.r.iface_key(i);
        let mut free = String::new();
        let mut methods: BTreeMap<TypeId, String> = BTreeMap::new();
        for f in self.r.interfaces[i].funcs.clone() {
            let text = self.import_fn(&m, &key, &f)?;
            match f.kind {
                FnKind::Free => free.push_str(&text),
                FnKind::Method(t) | FnKind::Static(t) | FnKind::Constructor(t) => {
                    methods.entry(t).or_default().push_str(&text);
                }
            }
        }
        let mut text = free;
        for (t, body) in methods {
            let _ = write!(
                text,
                "impl {} {{\n{body}}}\n",
                pascal(&self.r.types[t].name)
            );
        }
        self.push(&m, &text);
        Ok(())
    }

    fn export_iface(&mut self, i: IfaceId) -> Result<(), String> {
        let m = self.owner_mod(Owner::Iface(i));
        let key = self.r.iface_key(i);
        let module = format!("[export]{key}");
        for f in self.r.interfaces[i].funcs.clone() {
            let call = self.user_fn(&f);
            self.export_fn(&format!("{key}#{}", f.wit_name(self.r)), &module, &f, &call)?;
        }
        for (_, t) in self.r.interfaces[i].types.clone() {
            if matches!(self.r.types[t].kind, TypeKind::Resource) {
                let (.., table) = self.resource_fns(t);
                let name = &self.r.types[t].name;
                let export = format!("{key}#[dtor]{name}");
                let id = ident("dtor", &export, "");
                // The value leaves the table; the user's `Drop` runs.
                let body = if self.drops.contains(&pascal(name)) {
                    format!(
                        "    if let w_v: {}::{} = {table}.take(a0) {{\n        using w_u = w_v {{}}\n    }}\n",
                        self.base,
                        pascal(name)
                    )
                } else {
                    format!("    {table}.take(a0);\n")
                };
                self.push(
                    &m,
                    &format!(
                        "#[export_name(\"{export}\")]\nextern \"C\" fn {id}(a0: i32) {{\n{body}}}\n"
                    ),
                );
                self.exports.push(export);
            }
        }
        Ok(())
    }

    /// Every type `t` refers to, added to `out`.
    fn reach(&self, t: &Type, out: &mut BTreeSet<TypeId>) {
        match t {
            Type::Prim(_) | Type::ErrorContext => {}
            Type::List(e) | Type::Option(e) | Type::FixedList(e, _) => self.reach(e, out),
            Type::Future(e) | Type::Stream(e) => {
                if let Some(e) = e {
                    self.reach(e, out);
                }
            }
            Type::Result(a, b) => {
                for x in [a, b].into_iter().flatten() {
                    self.reach(x, out);
                }
            }
            Type::Tuple(ts) => ts.iter().for_each(|t| self.reach(t, out)),
            Type::Own(id) | Type::Borrow(id) | Type::Id(id) => {
                if !out.insert(*id) {
                    return;
                }
                match &self.r.types[*id].kind {
                    TypeKind::Alias(a) => self.reach(a, out),
                    TypeKind::Record(fs) => fs.iter().for_each(|(_, t)| self.reach(t, out)),
                    TypeKind::Variant(cs) => cs
                        .iter()
                        .filter_map(|(_, t)| t.as_ref())
                        .for_each(|t| self.reach(t, out)),
                    _ => {}
                }
            }
        }
    }
}

/// Bindings of `requests` for the module at rok path `base`, in `load`
/// order (so `loaded.roots` lines up); `drops`: exported resources that
/// implement `Drop`.
pub fn bind(
    loaded: &Loaded,
    requests: &[Request],
    base: &str,
    drops: &BTreeSet<String>,
) -> Result<Bindings, String> {
    let r = &loaded.resolve;
    let (imports, exports, funcs, includes) = targets(loaded, requests)?;
    let mut g = Gen {
        r,
        base: base.to_owned(),
        exported: exports.clone(),
        mods: BTreeMap::new(),
        externs: BTreeMap::new(),
        exports: Vec::new(),
        cur: None,
        ops: BTreeSet::new(),
        drops: drops.clone(),
    };
    generate(&mut g, &imports, &exports, &funcs)?;
    Ok(finish(g, &imports, &exports, includes))
}

type Targets = (
    BTreeSet<IfaceId>,
    BTreeSet<IfaceId>,
    Vec<(bool, WorldId, Function)>,
    Vec<WorldId>,
);

/// What `requests` bind: interfaces imported, exported, world functions
/// (`true`: imported) and the worlds the component includes.
fn targets(loaded: &Loaded, requests: &[Request]) -> Result<Targets, String> {
    let r = &loaded.resolve;
    let mut imports: BTreeSet<IfaceId> = BTreeSet::new();
    let mut exports: BTreeSet<IfaceId> = BTreeSet::new();
    let mut funcs: Vec<(bool, WorldId, Function)> = Vec::new();
    let mut includes: Vec<WorldId> = Vec::new();
    let mut root = loaded.roots.iter();
    for req in requests {
        for _ in &req.paths {
            let files = root.next().cloned().unwrap_or_default();
            let ifaces: Vec<IfaceId> = (0..r.interfaces.len())
                .filter(|i| {
                    r.interfaces[*i].inline.is_none() && files.contains(&r.interfaces[*i].file)
                })
                .collect();
            let worlds: Vec<WorldId> = (0..r.worlds.len())
                .filter(|w| files.contains(&r.worlds[*w].file))
                .collect();
            // A world is the component; without one, the interfaces are
            // imported or exported themselves.
            if worlds.is_empty() && req.export {
                exports.extend(ifaces);
            } else if worlds.is_empty() {
                imports.extend(ifaces);
            } else {
                for w in worlds {
                    includes.push(w);
                    for (import, list) in
                        [(true, &r.worlds[w].imports), (false, &r.worlds[w].exports)]
                    {
                        for e in list {
                            match e {
                                WorldEntry::Iface(i) if import => {
                                    imports.insert(*i);
                                }
                                WorldEntry::Iface(i) => {
                                    exports.insert(*i);
                                }
                                WorldEntry::Func(f) => funcs.push((import, w, f.clone())),
                            }
                        }
                    }
                }
            }
        }
    }
    // An interface whose types a bound one `use`s is imported too
    // (unless exported), as `wit-parser` elaborates a world.
    let mut todo: Vec<IfaceId> = imports.iter().chain(&exports).copied().collect();
    while let Some(i) = todo.pop() {
        for (_, t) in &r.interfaces[i].types {
            if let TypeKind::Alias(Type::Id(target)) = r.types[*t].kind {
                if let Owner::Iface(dep) = r.types[target].owner {
                    if dep != i && exports.contains(&dep) && imports.contains(&i) {
                        return Err(format!(
                            "imported interface `{}` uses types of `{}`, which this component exports: an import cannot name the component's own exported types (component model)",
                            r.iface_key(i),
                            r.iface_key(dep)
                        ));
                    }
                    if dep != i && !exports.contains(&dep) && imports.insert(dep) {
                        todo.push(dep);
                    }
                }
            }
        }
    }
    if let Some(i) = imports.intersection(&exports).next() {
        return Err(format!(
            "interface `{}` is both imported and exported",
            r.iface_key(*i)
        ));
    }
    Ok((imports, exports, funcs, includes))
}

/// The declarations and functions of the targets, into `g`.
fn generate(
    g: &mut Gen<'_>,
    imports: &BTreeSet<IfaceId>,
    exports: &BTreeSet<IfaceId>,
    funcs: &[(bool, WorldId, Function)],
) -> Result<(), String> {
    let r = g.r;
    // Types: those of the interfaces bound and what they refer to.
    let mut types = BTreeSet::new();
    for &i in imports.iter().chain(exports) {
        for (_, t) in &r.interfaces[i].types {
            g.reach(&Type::Id(*t), &mut types);
        }
        for f in &r.interfaces[i].funcs {
            for (_, t) in &f.params {
                g.reach(t, &mut types);
            }
            if let Some(t) = &f.result {
                g.reach(t, &mut types);
            }
        }
    }
    for (_, _, f) in funcs {
        for (_, t) in &f.params {
            g.reach(t, &mut types);
        }
        if let Some(t) = &f.result {
            g.reach(t, &mut types);
        }
    }
    for t in types {
        g.type_decl(t)?;
    }
    for &i in imports {
        g.import_iface(i)?;
    }
    for &i in exports {
        g.export_iface(i)?;
    }
    for (import, w, f) in funcs {
        let m = g.owner_mod(Owner::World(*w));
        if *import {
            let text = g.import_fn(&m, "$root", f)?;
            g.push(&m, &text);
        } else {
            let call = g.user_fn(f);
            g.export_fn(&f.name, "[export]$root", f, &call)?;
        }
    }
    Ok(())
}

fn finish(
    g: Gen<'_>,
    imports: &BTreeSet<IfaceId>,
    exports: &BTreeSet<IfaceId>,
    includes: Vec<WorldId>,
) -> Bindings {
    let r = g.r;
    let mut source = String::new();
    print_mods(&g.mods, &g.externs, &mut source);
    let mut world = String::new();
    for &i in imports {
        if r.interfaces[i].inline.is_none() {
            let _ = writeln!(world, "    import {};", r.iface_key(i));
        }
    }
    for &i in exports {
        if r.interfaces[i].inline.is_none() {
            let _ = writeln!(world, "    export {};", r.iface_key(i));
        }
    }
    for w in includes {
        let p = &r.packages[r.worlds[w].pkg].name;
        let v = p
            .version
            .as_ref()
            .map(|v| format!("@{v}"))
            .unwrap_or_default();
        let _ = writeln!(
            world,
            "    include {}:{}/{}{v};",
            p.ns, p.name, r.worlds[w].name
        );
    }
    let mut exports = g.exports;
    exports.push("cabi_realloc".into());
    Bindings {
        source,
        exports,
        world,
    }
}

type Externs = BTreeMap<Vec<String>, BTreeMap<String, BTreeSet<String>>>;

/// The modules as nested `pub mod` items.
fn print_mods(mods: &BTreeMap<Vec<String>, String>, externs: &Externs, out: &mut String) {
    let mut paths: BTreeSet<Vec<String>> = mods.keys().cloned().collect();
    paths.extend(externs.keys().cloned());
    // Every prefix is a module too.
    for p in paths.clone() {
        for n in 1..p.len() {
            paths.insert(p[..n].to_vec());
        }
    }
    print_mod(&[], &paths, mods, externs, out);
}

/// Module `at` and the modules below it.
fn print_mod(
    at: &[String],
    paths: &BTreeSet<Vec<String>>,
    mods: &BTreeMap<Vec<String>, String>,
    externs: &Externs,
    out: &mut String,
) {
    if let Some(name) = at.last() {
        let _ = writeln!(out, "pub mod {name} {{");
    }
    for (module, decls) in externs.get(at).into_iter().flatten() {
        let _ = writeln!(
            out,
            "#[link(wasm_import_module = \"{module}\")]\nextern \"C\" {{"
        );
        for d in decls {
            out.push_str(d);
        }
        out.push_str("}\n");
    }
    if let Some(text) = mods.get(at) {
        out.push_str(text);
    }
    for p in paths
        .iter()
        .filter(|p| p.len() == at.len() + 1 && p.starts_with(at))
    {
        print_mod(p, paths, mods, externs, out);
    }
    if !at.is_empty() {
        out.push_str("}\n");
    }
}
