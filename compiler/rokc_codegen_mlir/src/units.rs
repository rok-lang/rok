//! Codegen units (spec 9.2.16): a package cut into parts compiled to
//! separate objects; an incremental build recompiles only units whose key
//! changed.
//!
//! A function's unit is its home module (a library's generic instance also
//! keys on its type arguments). The *glue* unit holds what belongs to the
//! whole package: statics, the C `main`, `rok.register_types`.
//!
//! Units of one plan agree on what none sees alone:
//!
//! - GC type ids, in clean-build order (`Plan::gc_tables`); code reads an
//!   id from a glue-defined symbol (`gc_symbol`), so other units' types
//!   don't change it;
//! - linkage: a function another unit uses is external; a private copy of
//!   another package's code (a prebuilt generic instance; in a library,
//!   any) is `<symbol>.copy_Y<hash>_`, since that package may define it too
//!   (`Pkgs::unit_copies`);
//! - the package hash in symbols, a placeholder ([`Plan::placeholder`]) the
//!   driver patches, so a source edit doesn't touch every unit;
//! - whole-program analyses (`quiet_functions`, `reentrant_functions`).
//!
//! A unit's key hashes its functions' MIR (ids as symbols and type names),
//! the signatures and facts of its callees in other units, the statics and
//! GC types it uses, and the build options (`salt`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

use rokc_core::hash::{fnv, hex, sha256};
use rokc_ir_common::{FnRole, FunctionId, Linkage};
use rokc_mir::{
    BasicBlock, DebugTy, Link, MirFunction, MirModule, MirTy, Operand, RValue, Statement,
    Terminator,
};

use crate::emit::{entered_functions, quiet_functions, reentrant_functions, scan_table};
use crate::mangle::{self, Pkgs};

/// Library generic instances are spread over this many units per module.
const INSTANCE_BUCKETS: u64 = 8;

/// One unit.
#[derive(Clone, Debug)]
pub struct Unit {
    /// Its home module (`#k` for an instance bucket; `<root>` the
    /// package root), or `<glue>`.
    pub name: String,
    /// The functions it defines.
    pub fns: Vec<FunctionId>,
    pub glue: bool,
    /// Hex hash of everything its object depends on.
    pub key: String,
}

/// A program's units and what they share.
pub struct Plan {
    pub units: Vec<Unit>,
    /// Every GC `ScanTable` of the program, sorted: type id
    /// `USER_TYPE_BASE + type_offset + i`.
    pub gc_tables: Vec<Vec<u32>>,
    pub core: Arc<Core>,
}

/// What a unit's emitter takes from its plan.
pub struct Core {
    pub placeholder: String,
    pub unit_of: HashMap<FunctionId, usize>,
    /// Defined in one unit, used by another (or by the glue).
    pub shared: HashSet<FunctionId>,
    /// Shared private copies: `<symbol>.copy_Y<hash>_`.
    pub renamed: HashSet<FunctionId>,
    pub quiet: Arc<HashSet<FunctionId>>,
    pub reentrant: Arc<HashSet<FunctionId>>,
    pub entered: Arc<HashSet<FunctionId>>,
    /// Lazy statics: name → symbol of the initializer.
    pub lazy_statics: HashMap<String, String>,
    /// Per unit: the functions it refers to but does not define
    /// (through prebuilt bodies kept for inlining too).
    pub refs: Vec<Vec<FunctionId>>,
    /// Per unit: the statics it refers to.
    pub statics: Vec<BTreeSet<String>>,
}

/// How to plan: `salt` names the build (compiler, target, options).
pub struct PlanOptions<'a> {
    pub salt: &'a str,
    pub debug: bool,
    /// The compile's sources (spans name them).
    pub sources: &'a rokc_diagnostic::SourceMap,
}

impl Plan {
    /// The user package's hash in this plan's symbols.
    #[must_use]
    pub fn placeholder(&self) -> &str {
        &self.core.placeholder
    }
}

impl Core {
    /// Unit `u` defines `f`.
    pub fn defines(&self, u: usize, f: FunctionId) -> bool {
        self.unit_of.get(&f) == Some(&u)
    }

    /// The functions unit `u` sees: its own, and what they refer to.
    pub fn view<'m>(&self, module: &'m MirModule, u: usize) -> Vec<&'m MirFunction> {
        let refs: HashSet<FunctionId> = self.refs[u].iter().copied().collect();
        module
            .functions
            .iter()
            .filter(|f| self.defines(u, f.id) || refs.contains(&f.id))
            .collect()
    }

    /// `pkgs` as a unit names symbols: the user package's hash the
    /// placeholder, copies renamed (`Pkgs::unit_copies`).
    pub fn unit_pkgs(&self, pkgs: &mut Pkgs) {
        let own = own_package(&pkgs.link).to_owned();
        for (p, h) in &mut pkgs.link.packages {
            if *p == own {
                h.clone_from(&self.placeholder);
            }
        }
        pkgs.unit_copies = true;
    }
}

/// The `Link::packages` prefix of the package a build compiles (the user
/// package): a library's name, else `""`.
#[must_use]
pub fn own_package(link: &Link) -> &str {
    link.lib.as_deref().unwrap_or("")
}

/// The symbol of `f` in a unit (`mangle::symbol_for`), a copy's when
/// `renamed` (`Core::renamed`).
#[must_use]
pub fn symbol(f: &MirFunction, pkgs: &Pkgs, renamed: bool) -> String {
    copy(mangle::symbol_for(f, pkgs), renamed, pkgs)
}

/// The body symbol of a C-ABI definition in a unit (`mangle::mangled`).
#[must_use]
pub fn body_symbol(f: &MirFunction, pkgs: &Pkgs, renamed: bool) -> String {
    copy(mangle::mangled(f, pkgs), renamed, pkgs)
}

/// `s`, or (`renamed`) the user package's copy of it.
fn copy(s: String, renamed: bool, pkgs: &Pkgs) -> String {
    if renamed {
        let own = own_package(&pkgs.link);
        let pkg = pkgs.link.packages.iter().find(|(p, _)| p == own);
        format!("{s}.copy_Y{}_", pkg.map_or("", |(_, h)| h.as_str()))
    } else {
        s
    }
}

/// A unit-build symbol only the package's units share: a copy's
/// (`Core::renamed`) or a GC type id's (`gc_symbol`).
#[must_use]
pub fn package_internal(symbol: &str) -> bool {
    symbol.contains(".copy_Y") || symbol.contains("_gctype.")
}

/// The symbol of the GC type id of `table` in a unit build of the
/// package with hash `pkg`: an `i32` the glue defines.
#[must_use]
pub fn gc_symbol(pkg: &str, table: &[u32]) -> String {
    format!("_Y{pkg}_gctype.{:016x}", table_hash(table))
}

/// The symbol of `ScanTable` `table`, named by content: a hidden definition
/// each registering package has, kept once by the link.
#[must_use]
pub fn scan_symbol(table: &[u32]) -> String {
    format!("rok.scan.{:016x}", table_hash(table))
}

fn table_hash(table: &[u32]) -> u64 {
    let bytes: Vec<u8> = table.iter().flat_map(|w| w.to_le_bytes()).collect();
    fnv(&bytes)
}

/// The units of `module`, a program or library (not a REPL cell).
#[must_use]
pub fn plan(module: &MirModule, opts: &PlanOptions<'_>) -> Plan {
    let link = &module.link;
    let mut placeholder = 0u32;
    while link
        .packages
        .iter()
        .any(|(p, h)| p != own_package(link) && *h == format!("{placeholder:08x}"))
    {
        placeholder += 1;
    }
    let placeholder = format!("{placeholder:08x}");
    let index: HashMap<FunctionId, usize> = module
        .functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id, i))
        .collect();

    // Units: by home, sorted by name; the glue last.
    let mut homes: BTreeMap<String, Vec<FunctionId>> = BTreeMap::new();
    for f in &module.functions {
        if f.facts.linkage == Linkage::Defined {
            homes.entry(home(f, link)).or_default().push(f.id);
        }
    }
    let mut units: Vec<Unit> = homes
        .into_iter()
        .map(|(name, fns)| Unit {
            name,
            fns,
            glue: false,
            key: String::new(),
        })
        .collect();
    units.push(Unit {
        name: "<glue>".to_owned(),
        fns: Vec::new(),
        glue: true,
        key: String::new(),
    });
    let glue = units.len() - 1;
    let unit_of: HashMap<FunctionId, usize> = units
        .iter()
        .enumerate()
        .flat_map(|(u, unit)| unit.fns.iter().map(move |f| (*f, u)))
        .collect();

    // Each unit's references: calls and function addresses, through the
    // bodies of prebuilt functions kept for inlining. The glue calls `main`
    // and lazy static initializers.
    let mut refs: Vec<BTreeSet<FunctionId>> = vec![BTreeSet::new(); units.len()];
    let mut statics: Vec<BTreeSet<String>> = vec![BTreeSet::new(); units.len()];
    let mut glue_calls: Vec<FunctionId> = module
        .functions
        .iter()
        .filter(|f| f.facts.role == FnRole::Main && f.facts.linkage == Linkage::Defined)
        .map(|f| f.id)
        .collect();
    // A prebuilt package's statics are its `.a`'s.
    for s in module.statics.iter().filter(|s| !link.in_prebuilt(&s.name)) {
        glue_calls.extend(s.init_fn.filter(|i| index.contains_key(i)));
        statics[glue].insert(s.name.clone());
    }
    for (u, unit) in units.iter().enumerate() {
        let mut work: Vec<FunctionId> = unit.fns.clone();
        let mut seen: HashSet<FunctionId> = work.iter().copied().collect();
        let mut visit = |g: FunctionId, work: &mut Vec<FunctionId>| {
            if unit_of.get(&g) != Some(&u) {
                refs[u].insert(g);
            }
            let inline = index
                .get(&g)
                .is_some_and(|&j| kept_body(&module.functions[j]));
            if inline && seen.insert(g) {
                work.push(g);
            }
        };
        if u == glue {
            for &g in &glue_calls {
                visit(g, &mut work);
            }
        }
        while let Some(id) = work.pop() {
            let f = &module.functions[index[&id]];
            each_ref(f, &mut |r| match r {
                Ref::Fn(g) => visit(g, &mut work),
                Ref::Static(s) => {
                    statics[u].insert(s.to_owned());
                }
            });
        }
    }
    let shared: HashSet<FunctionId> = refs
        .iter()
        .flatten()
        .filter(|g| unit_of.contains_key(g))
        .copied()
        .collect();
    // Copies: a prebuilt package's code and a library's of other packages;
    // not `any` dispatchers, whose program copy overrides the weak one.
    let renamed: HashSet<FunctionId> = shared
        .iter()
        .filter(|g| {
            let f = &module.functions[index[g]];
            let path = f.path();
            f.facts.role != FnRole::AnyDispatcher
                && (link.in_prebuilt(path)
                    || link.lib.as_deref().is_some_and(|l| !Link::owns(l, path)))
        })
        .copied()
        .collect();

    let mut pkgs = Pkgs {
        link: link.clone(),
        unit_copies: true,
        ..Pkgs::default()
    };
    let own = own_package(link);
    for (p, h) in &mut pkgs.link.packages {
        if p == own {
            h.clone_from(&placeholder);
        }
    }
    pkgs.set_statics(&module.statics, module.target.os == "windows");
    let symbols: HashMap<FunctionId, String> = module
        .functions
        .iter()
        .map(|f| (f.id, symbol(f, &pkgs, renamed.contains(&f.id))))
        .collect();
    pkgs.lazy_statics = module
        .statics
        .iter()
        .filter_map(|s| {
            let init = s.init_fn.and_then(|i| symbols.get(&i)).cloned();
            let init = init.or_else(|| link.in_prebuilt(&s.name).then(String::new));
            Some((s.name.clone(), init?))
        })
        .collect();
    let quiet = quiet_functions(module, &pkgs, &module.target);
    let reentrant = reentrant_functions(module);
    let entered = entered_functions(module);

    // GC types: every table the code allocates with, sorted.
    let fn_tables: HashMap<FunctionId, Vec<Vec<u32>>> = module
        .functions
        .iter()
        .filter(|f| !f.blocks.is_empty())
        .map(|f| (f.id, tables(f, &module.target)))
        .collect();
    let gc_tables: Vec<Vec<u32>> = fn_tables
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let core = Core {
        placeholder,
        unit_of,
        shared,
        renamed,
        quiet: Arc::new(quiet),
        reentrant: Arc::new(reentrant),
        entered: Arc::new(entered),
        lazy_statics: pkgs.lazy_statics.clone(),
        refs: refs.into_iter().map(|r| r.into_iter().collect()).collect(),
        statics,
    };
    let ids: HashMap<&Vec<u32>, usize> =
        gc_tables.iter().enumerate().map(|(i, t)| (t, i)).collect();
    let fn_tables: HashMap<FunctionId, Vec<usize>> = fn_tables
        .iter()
        .map(|(f, ts)| (*f, ts.iter().map(|t| ids[t]).collect()))
        .collect();
    let shared = Shared {
        m: module,
        index: &index,
        symbols: &symbols,
        pkgs: &pkgs,
        core: &core,
        tables: &fn_tables,
        init_fns: module.statics.iter().filter_map(|s| s.init_fn).collect(),
        files: opts
            .sources
            .files()
            .iter()
            .map(|(p, _)| p.clone())
            .collect(),
        gc_tables: &gc_tables,
        salt: opts.salt,
        debug: opts.debug,
    };
    // Each unit's key, on a thread per core.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let keys: Vec<std::sync::Mutex<String>> =
        units.iter().map(|_| std::sync::Mutex::default()).collect();
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    std::thread::scope(|s| {
        for _ in 0..cores.min(units.len()) {
            s.spawn(|| {
                loop {
                    let u = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(unit) = units.get(u) else { break };
                    let mut k = Key {
                        b: String::new(),
                        s: &shared,
                        structs: BTreeSet::new(),
                    };
                    k.unit(u, unit);
                    let key = hex(&sha256(k.b.as_bytes()));
                    *keys[u]
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = key;
                }
            });
        }
    });
    for (unit, key) in units.iter_mut().zip(keys) {
        unit.key = key
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    Plan {
        units,
        gc_tables,
        core: Arc::new(core),
    }
}

/// A prebuilt function whose body is here for the optimizer to inline.
fn kept_body(f: &MirFunction) -> bool {
    f.facts.linkage == Linkage::Prebuilt && !f.blocks.is_empty()
}

/// `f`'s unit: its item path's module (an instance's generic's, a
/// closure's creator's); a library instance also buckets by type arguments.
fn home(f: &MirFunction, link: &Link) -> String {
    let path = f.path();
    // A type's arguments (`Pair<..>__m`) or a closure (`f::{closure#0}`)
    // are not modules.
    let cut = path.find(['<', '{']).unwrap_or(path.len());
    let head = path[..cut].trim_end_matches(':');
    let module = head.rsplit_once("::").map_or("", |(m, _)| m);
    let own = own_package(link);
    let library = link
        .packages
        .iter()
        .find(|(p, _)| p.is_empty() || Link::owns(p, path))
        .is_some_and(|(p, _)| !p.is_empty() && p != own);
    match &f.facts.mono_args {
        Some(args) if library => {
            format!("{module}#{}", fnv(args.as_bytes()) % INSTANCE_BUCKETS)
        }
        _ if module.is_empty() => "<root>".to_owned(),
        _ => module.to_owned(),
    }
}

enum Ref<'a> {
    Fn(FunctionId),
    Static(&'a str),
}

/// Every function and static `f`'s body refers to.
fn each_ref<'a>(f: &'a MirFunction, out: &mut dyn FnMut(Ref<'a>)) {
    for s in f.blocks.iter().flat_map(|b| &b.stmts) {
        match s {
            Statement::Assign { rvalue, .. } => match rvalue {
                RValue::Call { callee, .. } | RValue::FnAddr { callee, .. } => {
                    out(Ref::Fn(*callee));
                }
                RValue::StaticLoad { name, .. } | RValue::StaticAddr(name) => {
                    out(Ref::Static(name));
                }
                _ => {}
            },
            Statement::StaticStore { name, .. } => out(Ref::Static(name)),
            _ => {}
        }
    }
}

/// The `ScanTable`s `f`'s allocations register (`gc_type_id`'s uses).
pub fn tables(f: &MirFunction, t: &rokc_mir::Target) -> Vec<Vec<u32>> {
    let mut out = Vec::new();
    for s in f.blocks.iter().flat_map(|b| &b.stmts) {
        let Statement::Assign { rvalue, .. } = s else {
            continue;
        };
        match rvalue {
            RValue::HeapBox { ty, .. } if !t.any_inline(ty) => out.push(scan_table(ty, false, t)),
            RValue::HeapAlloc { ty } => out.push(scan_table(ty, false, t)),
            RValue::BufAlloc { elem_ty, .. } if t.has_gc_ref(elem_ty) => {
                out.push(scan_table(elem_ty, true, t));
            }
            _ => {}
        }
    }
    out
}

/// What every unit's key reads.
struct Shared<'a> {
    m: &'a MirModule,
    index: &'a HashMap<FunctionId, usize>,
    symbols: &'a HashMap<FunctionId, String>,
    /// Symbol naming as the units do (`Core::unit_pkgs`).
    pkgs: &'a Pkgs,
    core: &'a Core,
    /// The GC type ids of each function's allocations.
    tables: &'a HashMap<FunctionId, Vec<usize>>,
    init_fns: HashSet<FunctionId>,
    /// Source paths by `Span::file` - 1.
    files: Vec<String>,
    gc_tables: &'a [Vec<u32>],
    salt: &'a str,
    debug: bool,
}

/// A unit's key material: text, ids replaced by build-independent names.
struct Key<'a> {
    b: String,
    s: &'a Shared<'a>,
    /// Structs the text names (their layouts follow at the end).
    structs: BTreeSet<u32>,
}

impl Key<'_> {
    fn unit(&mut self, u: usize, unit: &Unit) {
        let s = self.s;
        let (m, core) = (s.m, s.core);
        let _ = writeln!(self.b, "{}\n{}\n{}", s.salt, unit.name, unit.glue);
        // A package's hash enters only through symbols the unit names, so an
        // edit recompiles just the units referring to it.
        let mut link = m.link.clone();
        for (_, h) in &mut link.packages {
            h.clear();
        }
        let _ = writeln!(self.b, "{link:?}");
        let by_symbol = |ids: &mut Vec<&MirFunction>| {
            ids.sort_by(|a, b| s.symbols[&a.id].cmp(&s.symbols[&b.id]));
        };
        let mut fns: Vec<&MirFunction> = unit
            .fns
            .iter()
            .map(|id| &m.functions[s.index[id]])
            .collect();
        by_symbol(&mut fns);
        let mut used: BTreeSet<usize> = BTreeSet::new();
        for f in fns {
            self.b.push_str("def ");
            self.decl(f);
            self.body(f);
            used.extend(s.tables.get(&f.id).into_iter().flatten());
        }
        let mut refs: Vec<&MirFunction> = core.refs[u]
            .iter()
            .filter_map(|id| s.index.get(id).map(|&i| &m.functions[i]))
            .collect();
        by_symbol(&mut refs);
        for f in refs {
            self.b.push_str("ref ");
            self.decl(f);
            if kept_body(f) {
                self.body(f);
                used.extend(s.tables.get(&f.id).into_iter().flatten());
            }
        }
        for st in m
            .statics
            .iter()
            .filter(|st| core.statics[u].contains(&st.name))
        {
            let _ = write!(
                self.b,
                "static {} {} {:?} {} {} {} {:?} ",
                st.name,
                s.pkgs.static_sym(&st.name),
                core.lazy_statics.get(&st.name),
                st.is_mut,
                st.is_extern,
                st.dll_import,
                unit.glue.then_some(&st.init)
            );
            self.ty(&st.ty);
            self.b.push('\n');
        }
        if unit.glue {
            // The registration of every type, and `main`.
            let _ = writeln!(self.b, "tables {:?}", s.gc_tables);
            for f in m.functions.iter().filter(|f| f.facts.role == FnRole::Main) {
                let _ = write!(self.b, "main {} ", s.symbols[&f.id]);
                // The C `main` is described at the user `main`.
                self.span(f.span);
                self.ty(&f.ret_ty);
            }
        } else {
            // By content: a unit reads ids from symbols.
            for i in used {
                let _ = writeln!(self.b, "table {:?}", s.gc_tables[i]);
            }
        }
        self.layouts();
    }

    /// What a call sees of `f`: its symbol, signature, facts and the
    /// plan's analyses.
    fn decl(&mut self, f: &MirFunction) {
        let (s, c) = (self.s, self.s.core);
        let _ = write!(
            self.b,
            "{} {:?} {} {} {} {} {} {} (",
            s.symbols[&f.id],
            f.facts,
            c.shared.contains(&f.id),
            c.renamed.contains(&f.id),
            c.quiet.contains(&f.id),
            c.reentrant.contains(&f.id),
            c.entered.contains(&f.id),
            s.init_fns.contains(&f.id),
        );
        for t in &f.param_tys {
            self.ty(t);
            self.b.push(',');
        }
        self.b.push_str(") ");
        self.ty(&f.ret_ty);
        self.b.push('\n');
    }

    fn body(&mut self, f: &MirFunction) {
        self.span(f.span);
        let _ = writeln!(self.b, "{:?}", f.params);
        for l in &f.locals {
            // Only `-g` shows names, and not the compiler's (`#..`).
            let name = l
                .name
                .as_deref()
                .filter(|n| self.s.debug && rokc_mir::debug_visible(n));
            let _ = write!(self.b, "{} {name:?} ", l.id.0);
            self.ty(&l.ty);
            if self.s.debug {
                if let Some(d) = self.s.m.debug.locals.get(&(f.id, l.id)) {
                    self.debug_ty(d);
                }
            }
            self.b.push('\n');
        }
        for b in &f.blocks {
            self.block(b);
        }
    }

    fn block(&mut self, b: &BasicBlock) {
        let _ = writeln!(self.b, "bb{} {:?}", b.id.0, b.params);
        for s in &b.stmts {
            self.stmt(s);
            self.b.push('\n');
        }
        match &b.terminator {
            Terminator::Goto { target, args } => {
                let _ = write!(self.b, "goto {}", target.0);
                self.operands(args);
            }
            Terminator::If {
                cond,
                then_bb,
                then_args,
                else_bb,
                else_args,
            } => {
                self.b.push_str("if ");
                self.operand(cond);
                let _ = write!(self.b, " {}", then_bb.0);
                self.operands(then_args);
                let _ = write!(self.b, " {}", else_bb.0);
                self.operands(else_args);
            }
            Terminator::Return(v) => {
                self.b.push_str("ret ");
                if let Some(v) = v {
                    self.operand(v);
                }
            }
            Terminator::Unreachable => self.b.push_str("unreachable"),
            Terminator::Suspend {
                value,
                resume,
                is_final,
            } => {
                self.b.push_str("suspend ");
                if let Some(v) = value {
                    self.operand(v);
                }
                let _ = write!(self.b, " {} {is_final}", resume.0);
            }
        }
        self.b.push('\n');
    }

    fn stmt(&mut self, s: &Statement) {
        match s {
            Statement::Assign { target, rvalue } => {
                let _ = write!(self.b, "{} = ", target.0);
                self.rvalue(rvalue);
            }
            Statement::ArrayWrite {
                array,
                offset,
                index,
                value,
                elem_ty,
                len,
            } => {
                let _ = write!(self.b, "awrite {} {offset} {len} ", array.0);
                self.operand(index);
                self.operand(value);
                self.ty(elem_ty);
            }
            Statement::StorageDead(l) => {
                let _ = write!(self.b, "dead {}", l.0);
            }
            Statement::BufStore {
                buf,
                index,
                value,
                elem_ty,
            } => {
                self.b.push_str("bstore ");
                self.operand(buf);
                self.operand(index);
                self.operand(value);
                self.ty(elem_ty);
            }
            Statement::Loc(span, scope) => {
                self.b.push_str("loc ");
                self.span(*span);
                let _ = write!(self.b, "{scope:?}");
            }
            Statement::RawStore {
                addr,
                value,
                ty,
                volatile,
            } => {
                let _ = write!(self.b, "rstore {volatile} ");
                self.operand(addr);
                self.operand(value);
                self.ty(ty);
            }
            Statement::HeapStore {
                ptr,
                ty,
                index,
                value,
            } => {
                let _ = write!(self.b, "hstore {index} ");
                self.operand(ptr);
                self.operand(value);
                self.ty(ty);
            }
            Statement::StaticStore { name, value, ty } => {
                let _ = write!(self.b, "sstore {name} ");
                self.operand(value);
                self.ty(ty);
            }
        }
    }

    fn rvalue(&mut self, r: &RValue) {
        match r {
            RValue::Use(o) => {
                self.b.push_str("use ");
                self.operand(o);
            }
            RValue::BinOp {
                op,
                op_ty,
                lhs,
                rhs,
            } => {
                let _ = write!(self.b, "bin {op:?} ");
                self.ty(op_ty);
                self.operand(lhs);
                self.operand(rhs);
            }
            RValue::UnaryOp { op, op_ty, operand } => {
                let _ = write!(self.b, "un {op:?} ");
                self.ty(op_ty);
                self.operand(operand);
            }
            RValue::Call {
                callee,
                args,
                ret_ty,
            } => {
                self.b.push_str("call ");
                self.callee(*callee);
                self.operands(args);
                self.ty(ret_ty);
            }
            RValue::StructInit { ty, fields } => {
                self.b.push_str("struct ");
                self.ty(ty);
                self.operands(fields);
            }
            RValue::FieldRead {
                target,
                index,
                field_ty,
            } => {
                let _ = write!(self.b, "field {index} ");
                self.operand(target);
                self.ty(field_ty);
            }
            RValue::Cast {
                source,
                from_ty,
                to_ty,
            } => {
                self.b.push_str("cast ");
                self.operand(source);
                self.ty(from_ty);
                self.ty(to_ty);
            }
            RValue::Bitcast {
                source,
                from_ty,
                to_ty,
            } => {
                self.b.push_str("bitcast ");
                self.operand(source);
                self.ty(from_ty);
                self.ty(to_ty);
            }
            RValue::WithField {
                source,
                struct_ty,
                index,
                value,
            } => {
                let _ = write!(self.b, "with {index} ");
                self.operand(source);
                self.ty(struct_ty);
                self.operand(value);
            }
            RValue::ArrayInit { elem_ty, elements } => {
                self.b.push_str("array ");
                self.ty(elem_ty);
                self.operands(elements);
            }
            RValue::ArrayIndex {
                array,
                offset,
                index,
                elem_ty,
                len,
            } => {
                let _ = write!(self.b, "aindex {} {offset} {len} ", array.0);
                self.operand(index);
                self.ty(elem_ty);
            }
            RValue::FnAddr { callee, fn_ty } => {
                self.b.push_str("fnaddr ");
                self.callee(*callee);
                self.ty(fn_ty);
            }
            RValue::StaticLoad { name, ty } => {
                let _ = write!(self.b, "sload {name} ");
                self.ty(ty);
            }
            RValue::HeapBox { value, ty } => {
                self.b.push_str("box ");
                self.operand(value);
                self.ty(ty);
            }
            RValue::BufAlloc { elem_ty, len } => {
                self.b.push_str("balloc ");
                self.ty(elem_ty);
                self.operand(len);
            }
            RValue::RawLoad { addr, ty, volatile } => {
                let _ = write!(self.b, "rload {volatile} ");
                self.operand(addr);
                self.ty(ty);
            }
            RValue::AddrOf(l) => {
                let _ = write!(self.b, "addr {}", l.0);
            }
            RValue::StaticAddr(name) => {
                let _ = write!(self.b, "saddr {name}");
            }
            RValue::SizeOf { ty, align } => {
                let _ = write!(self.b, "size {align} ");
                self.ty(ty);
            }
            RValue::BufLoad {
                buf,
                index,
                elem_ty,
            } => {
                self.b.push_str("bload ");
                self.operand(buf);
                self.operand(index);
                self.ty(elem_ty);
            }
            RValue::BufAtomic {
                op,
                buf,
                index,
                args,
                elem_ty,
                order,
            } => {
                let _ = write!(self.b, "batomic {op} {order:?} ");
                self.operand(buf);
                self.operand(index);
                self.operands(args);
                self.ty(elem_ty);
            }
            RValue::HeapLoad { ptr, ty } => {
                self.b.push_str("hload ");
                self.operand(ptr);
                self.ty(ty);
            }
            RValue::HeapAlloc { ty } => {
                self.b.push_str("halloc ");
                self.ty(ty);
            }
            RValue::HeapField { ptr, ty, index } => {
                let _ = write!(self.b, "hfield {index} ");
                self.operand(ptr);
                self.ty(ty);
            }
            RValue::CallIndirect {
                callee,
                args,
                arg_tys,
                ret_ty,
                abi,
            } => {
                let _ = write!(self.b, "icall {abi:?} ");
                self.operand(callee);
                self.operands(args);
                for t in arg_tys {
                    self.ty(t);
                }
                self.ty(ret_ty);
            }
        }
    }

    fn callee(&mut self, id: FunctionId) {
        match self.s.symbols.get(&id) {
            Some(s) => {
                let _ = write!(self.b, "@{s} ");
            }
            None => {
                let _ = write!(self.b, "@?{} ", id.0);
            }
        }
    }

    fn operands(&mut self, ops: &[Operand]) {
        self.b.push('(');
        for o in ops {
            self.operand(o);
        }
        self.b.push(')');
    }

    fn operand(&mut self, o: &Operand) {
        match o {
            Operand::Const(c) => {
                let _ = write!(self.b, "{c:?} ");
            }
            Operand::Copy(l) => {
                let _ = write!(self.b, "%{} ", l.0);
            }
        }
    }

    fn span(&mut self, at: rokc_diagnostic::Span) {
        let s = self.s;
        let file = (at.file as usize)
            .checked_sub(1)
            .and_then(|i| s.files.get(i))
            .map_or("", String::as_str);
        let _ = write!(
            self.b,
            "[{file}:{}:{}:{}:{}] ",
            at.line, at.col, at.start, at.end
        );
    }

    fn ty(&mut self, t: &MirTy) {
        match t {
            MirTy::Unit => self.b.push_str("()"),
            MirTy::Prim(p) => {
                let _ = write!(self.b, "{p:?}");
            }
            MirTy::Any => self.b.push_str("any"),
            MirTy::Tuple(ts) => {
                self.b.push('(');
                for t in ts {
                    self.ty(t);
                    self.b.push(',');
                }
                self.b.push(')');
            }
            MirTy::Struct(id, fields) => {
                self.structs.insert(id.0);
                let _ = write!(self.b, "S{{{}}}{{", self.struct_name(id.0));
                for t in fields {
                    self.ty(t);
                    self.b.push(',');
                }
                self.b.push('}');
            }
            MirTy::Array(t, n) => {
                self.b.push('[');
                self.ty(t);
                let _ = write!(self.b, ";{n}]");
            }
            MirTy::Fn(ps, r) => {
                self.b.push_str("fn(");
                for t in ps {
                    self.ty(t);
                    self.b.push(',');
                }
                self.b.push_str(")->");
                self.ty(r);
            }
            MirTy::GcRef => self.b.push_str("gcref"),
            MirTy::Union(vs, words, kind) => {
                let _ = write!(self.b, "union{words}{kind:?}<");
                for (tag, t) in vs {
                    let _ = write!(self.b, "{tag}:");
                    self.ty(t);
                    self.b.push(',');
                }
                self.b.push('>');
            }
        }
        self.b.push(' ');
    }

    fn struct_name(&self, id: u32) -> String {
        self.s
            .m
            .struct_keys
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("#{id}"))
    }

    fn debug_ty(&mut self, d: &DebugTy) {
        match d {
            DebugTy::Mir(t) => self.ty(t),
            DebugTy::Sum(n, t, vs) => {
                let _ = write!(self.b, "sum {n} ");
                self.ty(t);
                for (tag, name, v) in vs {
                    let _ = write!(self.b, "{tag} {name} ");
                    self.debug_ty(v);
                }
            }
            DebugTy::Buf(n, e) => {
                let _ = write!(self.b, "buf {n} ");
                self.debug_ty(e);
            }
            DebugTy::Any => self.b.push_str("dany "),
            DebugTy::Boxed(n, e) => {
                let _ = write!(self.b, "boxed {n} ");
                self.debug_ty(e);
            }
            DebugTy::Cell(e) => {
                self.b.push_str("cell ");
                self.debug_ty(e);
            }
            DebugTy::Closure(n, t, e) => {
                let _ = write!(self.b, "closure {n} ");
                self.ty(t);
                self.debug_ty(e);
            }
        }
    }

    /// Layout facts of the structs named so far (`#[repr]`; under `-g` also
    /// names, field types and `any` types).
    fn layouts(&mut self) {
        let mut done: BTreeSet<u32> = BTreeSet::new();
        loop {
            let todo: Vec<u32> = self.structs.difference(&done).copied().collect();
            if todo.is_empty() {
                break;
            }
            for id in todo {
                done.insert(id);
                let _ = write!(
                    self.b,
                    "struct {} {:?}",
                    self.struct_name(id),
                    self.s.m.target.reprs.get(&id)
                );
                if self.s.debug {
                    let _ = write!(self.b, " {:?}", self.s.m.struct_names.get(&id));
                    for d in self.s.m.debug.fields.get(&id).into_iter().flatten() {
                        self.debug_ty(d);
                    }
                }
                self.b.push('\n');
            }
        }
        if self.s.debug {
            let any = self.s.m.debug.any.clone();
            for (tag, name, d) in &any {
                let _ = write!(self.b, "any {tag} {name} ");
                self.debug_ty(d);
            }
        }
    }
}
