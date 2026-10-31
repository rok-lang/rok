//! Exit-path checks over HIR bodies: a `Drop` resource or a channel
//! `Sender` left open on some path (`unclosed_resource`,
//! `unclosed_sender`), a `Drop` value released twice (`using_twice`), and
//! a thread-local value held across an `await`
//! (`thread_local_across_await`).
//!
//! A body is walked in evaluation order with, per value, what may hold on
//! each path to that point: open, closed, handed on, released. `using`
//! lowers to `drop` calls on each of its exits and `throw` to a return of
//! the error, so both are plain paths here. A panic ends a path.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use rokc_diagnostic::Span;
use rokc_hir::{HirBlock, HirExpr, HirExprKind, HirFunction, HirModule, HirStmt, HirTy, StructId};
use rokc_ir_common::{FunctionId, LocalId};

use crate::analysis::{each_block, short_name};

/// A finding of this pass.
pub(super) struct Hit {
    pub lint: &'static str,
    pub span: Span,
    pub msg: String,
    pub notes: Vec<String>,
    pub secondary: Option<(Span, String)>,
}

/// Methods that close what they are called on (`into_*` too). `next`,
/// `poll_next`: a `ReadDir` (`ReadDirStream`) closes itself once drained,
/// as a `for` (`for await`) does.
const CLOSERS: &[&str] = &[
    "close",
    "try_close",
    "drop",
    "wait",
    "wait_with_output",
    "unmap",
    "wipe",
    "next",
    "poll_next",
];

/// What the checks need of the whole module, made once.
pub(super) struct Resources<'h> {
    fns: HashMap<FunctionId, &'h HirFunction>,
    /// `Drop` types: name and whether the lint asks to close them (not
    /// the secrets the collection wipes anyway).
    types: HashMap<StructId, (&'h str, bool)>,
    senders: HashSet<StructId>,
    tls: HashSet<StructId>,
    /// `BufWriter`, `LineWriter`.
    flushed: HashSet<StructId>,
    structs: HashMap<StructId, &'h rokc_hir::HirStructDef>,
    /// `(callee, parameter)` → the callee only calls the value's own
    /// non-closing methods.
    reads: RefCell<HashMap<(FunctionId, usize), bool>>,
}

impl<'h> Resources<'h> {
    pub(super) fn new(hir: &'h HirModule) -> Self {
        // The std descriptor holders; any other type whose `drop` the
        // program reaches counts too.
        const STD: &[&str] = &[
            "rok::std::fs::File",
            "rok::std::fs::Dir",
            "rok::std::fs::ReadDir",
            "rok::std::net::TcpStream",
            "rok::std::net::TcpListener",
            "rok::std::net::UdpSocket",
            "rok::std::process::Child",
        ];
        // Methods are named `Struct<id>__<method>`, a prebuilt library's
        // `<type>.<method>`; a trait impl's add `__by_<Trait>`.
        let drops: HashSet<&str> = hir
            .functions
            .iter()
            .filter_map(|g| {
                let n = g.name.as_str();
                let n = n.split("__by_").next().unwrap_or(n);
                n.strip_suffix(".drop").or_else(|| {
                    n.strip_suffix("__drop")
                        .map(|_| n.split("__").next().unwrap_or(n))
                })
            })
            .collect();
        // A generic type's instances: `drop` taking one.
        let dropped: HashSet<StructId> = hir
            .functions
            .iter()
            .filter(|g| leaf(&g.name) == "drop")
            .filter_map(|g| match g.params.first().map(|p| &p.ty) {
                Some(HirTy::Struct(id)) => Some(*id),
                _ => None,
            })
            .collect();
        let base = |n: &'h str| n.split("__").next().unwrap_or(n);
        let mut types = HashMap::new();
        let (mut senders, mut tls, mut flushed) = (HashSet::new(), HashSet::new(), HashSet::new());
        for s in &hir.structs {
            let own = format!("Struct{}", s.id.0);
            let n = s.name.as_str();
            if STD.contains(&n)
                || drops.contains(own.as_str())
                || drops.contains(n)
                || dropped.contains(&s.id)
            {
                // A secret's bytes are zeroed by the collection that
                // frees it: `wipe` is early, not required.
                let wiped = n.starts_with("rok::core::crypto::")
                    && ["Secret", "SecretWords", "Aead"]
                        .iter()
                        .any(|t| n.ends_with(&format!("::{t}")));
                types.insert(s.id, (n, !wiped));
            }
            match base(n) {
                "rok::core::channel::Sender" => {
                    senders.insert(s.id);
                }
                "rok::std::thread::ThreadLocal" => {
                    tls.insert(s.id);
                }
                "rok::std::io::BufWriter" | "rok::std::io::LineWriter" => {
                    flushed.insert(s.id);
                }
                _ => {}
            }
        }
        Self {
            fns: hir.functions.iter().map(|g| (g.id, g)).collect(),
            types,
            senders,
            tls,
            flushed,
            structs: hir.structs.iter().map(|s| (s.id, s)).collect(),
            reads: RefCell::default(),
        }
    }

    /// Method `m` of a `sid` closes it. A buffered writer's `drop` only
    /// flushes it: `flush` does the same.
    fn closes(&self, m: &str, sid: StructId) -> bool {
        CLOSERS.contains(&m)
            || m.starts_with("into_")
            || (m == "flush" && self.flushed.contains(&sid))
    }

    /// The method name of `callee` when its receiver is a `sid`.
    fn method(&self, callee: FunctionId, sid: StructId) -> Option<&'h str> {
        let f = self.fns.get(&callee)?;
        if f.params.first()?.ty != HirTy::Struct(sid) {
            return None;
        }
        // Its own type's (`File.read`, `Struct7__read`, `Sender__send`) or
        // a trait impl's (`__impl1a2b_drop`), not another type's
        // constructor taking one (`Client__connect(tcp, ..)`).
        let n = f.name.split("__mono__").next().unwrap_or(&f.name);
        let n = n.split("__by_").next().unwrap_or(n);
        let n = n.rsplit("::").next().unwrap_or(n);
        let owner = match n.rsplit_once('.') {
            Some((t, _)) => t,
            None if n.starts_with("__impl") => return Some(leaf(&f.name)),
            None => n.rsplit_once("__").map_or("", |(t, _)| t),
        };
        let ty = self.structs.get(&sid).map_or("", |s| s.name.as_str());
        let short = short_name(ty.split("__").next().unwrap_or(ty));
        (owner == format!("Struct{}", sid.0) || leaf_type(owner) == short).then(|| leaf(&f.name))
    }

    /// `callee` is `Dir::root()`, whose `Dir` holds no descriptor.
    fn no_fd(&self, callee: FunctionId) -> bool {
        self.fns.get(&callee).is_some_and(|f| {
            f.params.is_empty()
                && leaf(&f.name) == "root"
                && matches!(&f.ret_ty, HirTy::Struct(id) if self
                    .structs
                    .get(id)
                    .is_some_and(|d| d.name == "rok::std::fs::Dir"))
        })
    }

    /// `ty`, when it is a `Drop` resource or a `Sender`, or a sum that
    /// may hold one (`File | i32`).
    fn tracked(&self, ty: &HirTy) -> Option<StructId> {
        match ty {
            HirTy::Struct(id) if self.types.contains_key(id) || self.senders.contains(id) => {
                Some(*id)
            }
            HirTy::Sum(ms) => ms.iter().find_map(|m| self.tracked(m)),
            _ => None,
        }
    }

    fn name(&self, sid: StructId) -> &'h str {
        self.types.get(&sid).map_or("Sender", |(n, _)| {
            short_name(n.split("__").next().unwrap_or(n))
        })
    }

    /// The closing method `sid` has, for the help.
    fn closer(&self, sid: StructId) -> Option<&'h str> {
        let has: HashSet<&str> = self
            .fns
            .values()
            .filter(|f| f.params.first().is_some_and(|p| p.ty == HirTy::Struct(sid)))
            .map(|f| leaf(&f.name))
            .collect();
        ["close", "wait", "unmap", "wipe", "flush"]
            .into_iter()
            .find(|m| has.contains(m))
    }

    /// Whether `callee` only reads its parameter `i` (calls its own
    /// non-closing methods): passing a value there does not hand it on.
    fn reads_only(&self, callee: FunctionId, i: usize) -> bool {
        if let Some(&r) = self.reads.borrow().get(&(callee, i)) {
            return r;
        }
        let r = self.fns.get(&callee).is_some_and(|f| {
            let Some(p) = f.params.get(i) else {
                return false;
            };
            let Some(sid) = self.tracked(&p.ty) else {
                return false;
            };
            !p.by_ref && self.only_reads(f, &HashSet::from([p.local]), sid)
        });
        self.reads.borrow_mut().insert((callee, i), r);
        r
    }

    /// Whether closure `fn_id` only reads its capture `k`.
    fn env_reads_only(&self, fn_id: FunctionId, k: u32, sid: StructId) -> bool {
        let Some(f) = self.fns.get(&fn_id) else {
            return false;
        };
        let Some(env) = f.params.first().filter(|p| p.name == "__env") else {
            return false;
        };
        let Some(body) = &f.body else { return false };
        let mut start = HashSet::new();
        let mut lets = |b: &HirBlock| {
            for s in &b.stmts {
                if let HirStmt::Let { local, init } = s {
                    if let HirExprKind::TupleField { target, index, .. } = &init.kind {
                        if *index == k
                            && matches!(target.kind, HirExprKind::Local(l) if l == env.local)
                        {
                            start.insert(*local);
                        }
                    }
                }
            }
        };
        lets(body);
        !start.is_empty() && self.only_reads(f, &start, sid)
    }

    /// Every use of `start` (and the locals bound to it) in `f` is a call
    /// of its own method that does not close it.
    fn only_reads(&self, f: &HirFunction, start: &HashSet<LocalId>, sid: StructId) -> bool {
        let Some(body) = &f.body else { return false };
        let mut group = start.clone();
        // The holders: `let y = x` of a member.
        let mut forms: HashSet<*const HirExpr> = HashSet::new();
        loop {
            let n = group.len();
            let mut add = |b: &HirBlock, group: &mut HashSet<LocalId>| {
                for s in &b.stmts {
                    if let HirStmt::Let { local, init } = s {
                        if let HirExprKind::Local(x) = init.kind {
                            if group.contains(&x) {
                                group.insert(*local);
                                forms.insert(std::ptr::from_ref(init));
                            }
                        }
                    }
                }
            };
            add(body, &mut group);
            let mut blocks = Vec::new();
            each_block(body, &mut |e| {
                if let HirExprKind::Block(b)
                | HirExprKind::Loop { body: b, .. }
                | HirExprKind::While { body: b, .. }
                | HirExprKind::If { then_block: b, .. } = &e.kind
                {
                    blocks.push(&**b);
                }
            });
            for b in blocks {
                add(b, &mut group);
            }
            if group.len() == n {
                break;
            }
        }
        let (mut uses, mut kept) = (0, 0);
        each_block(body, &mut |e| {
            if matches!(e.kind, HirExprKind::Local(x) if group.contains(&x))
                && !forms.contains(&std::ptr::from_ref(e))
            {
                uses += 1;
            }
            let HirExprKind::Call { callee, args } = &e.kind else {
                return;
            };
            if matches!(args.first().map(|a| &a.kind), Some(HirExprKind::Local(x)) if group.contains(x))
                && self
                    .method(*callee, sid)
                    .is_some_and(|m| !self.closes(m, sid) && m != "clone" && m != "try_clone")
            {
                kept += 1;
            }
        });
        uses == kept
    }
}

/// A function's method or own name: `File.read`, `Struct7__read__by_Read`,
/// `m::Sender__send__mono__..`, `m::__impl1a2b_clone__mono__..`.
/// `File` of `rok::std::fs::File`, `Sender` of `Sender`.
fn leaf_type(t: &str) -> &str {
    t.rsplit("::").next().unwrap_or(t)
}

fn leaf(name: &str) -> &str {
    let n = name.split("__mono__").next().unwrap_or(name);
    let n = n.split("__by_").next().unwrap_or(n);
    let n = n.rsplit("::").next().unwrap_or(n);
    if let Some(rest) = n.strip_prefix("__impl") {
        return rest.split_once('_').map_or(rest, |(_, m)| m);
    }
    let n = n.rsplit('.').next().unwrap_or(n);
    n.rsplit("__").next().unwrap_or(n)
}

const OPEN: u8 = 1;
const CLOSED: u8 = 2;
const GONE: u8 = 4;
const RELEASED: u8 = 8;

/// What may hold of each value on the paths to a point (`None`: no path
/// reaches it).
type State = Option<Vec<u8>>;

fn join(a: State, b: State) -> State {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(mut a), Some(b)) => {
            if a.len() < b.len() {
                a.resize(b.len(), 0);
            }
            for (x, y) in a.iter_mut().zip(b) {
                *x |= y;
            }
            Some(a)
        }
    }
}

fn get(st: &State, g: usize) -> u8 {
    st.as_ref().and_then(|v| v.get(g).copied()).unwrap_or(0)
}

fn set(st: &mut State, g: usize, f: impl FnOnce(u8) -> u8) {
    if let Some(v) = st {
        if v.len() <= g {
            v.resize(g + 1, 0);
        }
        v[g] = f(v[g]);
    }
}

/// One value and the locals holding it.
struct Group {
    sid: StructId,
    /// Made here: this body must close it.
    owned: bool,
    name: Option<String>,
    at: Span,
    /// A closer runs on some path.
    closed: bool,
    /// Where it is still open as a path leaves it, and why.
    leak: Option<(Span, &'static str)>,
    /// Where it is used after a release (`true`: released again).
    twice: Option<(Span, bool)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Origin {
    /// A new value (a call's result).
    Fresh,
    /// The value of group `.0`.
    Alias(usize),
    Other,
}

struct Target {
    label: Option<String>,
    is_loop: bool,
    /// The block's value goes on to a holder (see `Walk::expr`).
    keep: bool,
    breaks: State,
    conts: State,
}

struct Walk<'a, 'h> {
    res: &'a Resources<'h>,
    f: &'h HirFunction,
    files: &'a HashSet<u32>,
    groups: Vec<Group>,
    member: HashMap<LocalId, usize>,
    /// A struct local holding values followed here: field, group.
    wrappers: HashMap<LocalId, Vec<(u32, usize)>>,
    /// Struct literals walked for the holder the statement binds.
    pending: Vec<&'h HirExpr>,
    sites: HashMap<(LocalId, u32, u32), usize>,
    inits: HashMap<LocalId, &'h HirExpr>,
    /// Temporaries a pattern binds: the scrutinee.
    binds: HashMap<LocalId, &'h HirExpr>,
    targets: Vec<Target>,
    /// The last span walked: where a bare `return` is.
    last: Span,
    temps: HashSet<(u32, u32)>,
    hits: &'a mut Vec<Hit>,
}

/// The exit-path checks of `f`.
pub(super) fn check<'h>(
    res: &Resources<'h>,
    f: &'h HirFunction,
    files: &HashSet<u32>,
    hits: &mut Vec<Hit>,
) {
    thread_local_across_await(res, f, files, hits);
    let Some(body) = &f.body else { return };
    let mut any = f.locals.iter().any(|l| res.tracked(&l.ty).is_some());
    if !any {
        each_block(body, &mut |e| any |= res.tracked(&e.ty).is_some());
    }
    if !any {
        return;
    }
    let mut inits = HashMap::new();
    let mut lets = |b: &'h HirBlock| {
        for s in &b.stmts {
            if let HirStmt::Let { local, init } = s {
                inits.insert(*local, init);
            }
        }
    };
    lets(body);
    each_block(body, &mut |e| {
        if let HirExprKind::Block(b)
        | HirExprKind::Loop { body: b, .. }
        | HirExprKind::While { body: b, .. }
        | HirExprKind::If { then_block: b, .. } = &e.kind
        {
            lets(b);
        }
    });
    let mut w = Walk {
        res,
        f,
        files,
        groups: Vec::new(),
        member: HashMap::new(),
        wrappers: HashMap::new(),
        pending: Vec::new(),
        sites: HashMap::new(),
        inits,
        binds: HashMap::new(),
        targets: Vec::new(),
        last: f.span,
        temps: HashSet::new(),
        hits,
    };
    let mut st: State = Some(Vec::new());
    // What a parameter holds is the caller's to close; a second release
    // of it is still this body's.
    for p in &f.params {
        if let Some(sid) = res.tracked(&p.ty).filter(|s| res.types.contains_key(s)) {
            let g = w.group(p.local, sid, false, f.span);
            set(&mut st, g, |_| 0);
        }
    }
    let st = w.block(body, st, false);
    if let Some(t) = &body.trailing {
        w.last = t.span;
    }
    w.exit(&st, None);
    w.report();
    w.report_twice();
}

impl<'h> Walk<'_, 'h> {
    fn local_name(&self, l: LocalId) -> Option<&'h str> {
        self.f
            .locals
            .iter()
            .find(|x| x.id == l)
            .and_then(|x| x.name.as_deref())
            .filter(|n| !n.starts_with('#'))
    }

    /// Local `l` holds the value of group `g` from here on.
    fn hold(&mut self, l: LocalId, g: usize) {
        self.member.insert(l, g);
    }

    /// A local the lowering made (`_1`, `#e15`).
    fn hidden(&self, l: LocalId) -> bool {
        self.local_name(l).is_none()
    }

    fn local_ty(&self, l: LocalId) -> Option<&'h HirTy> {
        self.f.locals.iter().find(|x| x.id == l).map(|x| &x.ty)
    }

    /// The group of local `l` (a new one if it has none).
    fn group(&mut self, l: LocalId, sid: StructId, owned: bool, at: Span) -> usize {
        // One value per place it is made: a loop's next pass reuses it.
        let key = (l, at.file, at.start);
        let g = if let Some(&g) = self.sites.get(&key) {
            g
        } else {
            self.groups.push(Group {
                sid,
                owned,
                name: self.local_name(l).map(str::to_owned),
                at,
                closed: false,
                leak: None,
                twice: None,
            });
            self.sites.insert(key, self.groups.len() - 1);
            self.groups.len() - 1
        };
        self.hold(l, g);
        g
    }

    /// `e` asks whether the task was cancelled (an `await`'s lowering).
    fn cancel_check(&self, e: &HirExpr) -> bool {
        let HirExprKind::Call { callee, .. } = &e.kind else {
            return false;
        };
        self.res.fns.get(callee).is_some_and(|f| {
            matches!(
                f.name.as_str(),
                "rok::core::task::cancel_point" | "rok::core::task::cancel_left"
            )
        })
    }

    /// The call whose result `e` is, directly or through a temporary.
    fn call_of(&self, e: &'h HirExpr) -> Option<(FunctionId, &'h [HirExpr])> {
        match &e.kind {
            HirExprKind::Call { callee, args } => Some((*callee, args)),
            HirExprKind::Local(x) if !self.member.contains_key(x) => {
                self.call_of(self.inits.get(x)?)
            }
            _ => None,
        }
    }

    /// Where the value of `e` comes from.
    fn origin(&self, e: &HirExpr, depth: u32) -> Origin {
        if depth > 16 {
            return Origin::Other;
        }
        let sid = self.res.tracked(&e.ty);
        let same = |g: usize| sid.is_none_or(|s| self.groups[g].sid == s);
        match &e.kind {
            HirExprKind::Local(x) => {
                if let Some(&g) = self.member.get(x) {
                    return if same(g) {
                        Origin::Alias(g)
                    } else {
                        Origin::Other
                    };
                }
                // A tuple or carrier the lowering made, or a value
                // narrowed out of one.
                let lowered = self
                    .local_ty(*x)
                    .and_then(|t| self.res.tracked(t))
                    .is_none()
                    || self.hidden(*x);
                match self.inits.get(x).or_else(|| self.binds.get(x)) {
                    Some(i) if lowered => self.origin(i, depth + 1),
                    _ => Origin::Other,
                }
            }
            HirExprKind::Block(b) => {
                let t = b
                    .trailing
                    .as_ref()
                    .filter(|t| t.ty != HirTy::Never)
                    .map(|t| self.origin(t, depth + 1));
                let breaks = b
                    .label
                    .as_deref()
                    .map(|l| breaks_to(b, l))
                    .unwrap_or_default();
                combine(
                    t.into_iter()
                        .chain(breaks.into_iter().map(|v| self.origin(v, depth + 1))),
                )
            }
            // A field set on it (drop glue's write-back) keeps it.
            HirExprKind::SumWrap { value: Some(v), .. }
            | HirExprKind::WithField { target: v, .. } => self.origin(v, depth + 1),
            // A part of a call's result: `(stream, addr)`, a `ref self`
            // method's `(value, self)`, a carrier's value.
            HirExprKind::TupleField { target, index, .. } => {
                // A call with `ref` parameters returns `(value, ref args..)`:
                // part `k` is what went in as the `k`th `ref` argument.
                if let Some((callee, args)) = self.call_of(target) {
                    let refs: Vec<usize> = self
                        .res
                        .fns
                        .get(&callee)
                        .map(|f| {
                            f.params
                                .iter()
                                .enumerate()
                                .filter(|(_, p)| p.by_ref)
                                .map(|(i, _)| i)
                                .collect()
                        })
                        .unwrap_or_default();
                    if !refs.is_empty() {
                        return match (*index as usize).checked_sub(1) {
                            None => Origin::Fresh,
                            Some(k) => refs
                                .get(k)
                                .and_then(|&i| args.get(i))
                                .map_or(Origin::Other, |a| self.origin(a, depth + 1)),
                        };
                    }
                }
                match self.origin(target, depth + 1) {
                    Origin::Alias(g) if same(g) => Origin::Alias(g),
                    Origin::Alias(_) if sid.is_some() => Origin::Fresh,
                    Origin::Fresh => Origin::Fresh,
                    _ => Origin::Other,
                }
            }
            HirExprKind::Call { callee, args } => {
                // An element of a collection is no new value.
                let element = args.first().is_some_and(|a| match &a.ty {
                    HirTy::Struct(id) => self
                        .res
                        .structs
                        .get(id)
                        .is_some_and(|d| d.name.starts_with("rok::core::collections::")),
                    _ => false,
                });
                if element || self.res.no_fd(*callee) {
                    return Origin::Other;
                }
                if let Some(Origin::Alias(g)) = args.first().map(|a| self.origin(a, depth + 1)) {
                    let m = self.res.method(*callee, self.groups[g].sid);
                    if m.is_some_and(|m| m != "clone" && m != "try_clone") && same(g) {
                        return Origin::Alias(g);
                    }
                }
                Origin::Fresh
            }
            HirExprKind::CallIndirect { .. } | HirExprKind::CallClosure { .. } => Origin::Fresh,
            // A field of a struct local holding a followed value.
            HirExprKind::FieldRead {
                target,
                field_index,
                ..
            } => match &target.kind {
                HirExprKind::Local(h) => self
                    .wrappers
                    .get(h)
                    .and_then(|fs| fs.iter().find(|(i, _)| i == field_index))
                    .map_or(Origin::Other, |&(_, g)| {
                        if same(g) {
                            Origin::Alias(g)
                        } else {
                            Origin::Other
                        }
                    }),
                _ => Origin::Other,
            },
            HirExprKind::If {
                then_block,
                else_branch,
                ..
            } => {
                let a = then_block
                    .trailing
                    .as_ref()
                    .filter(|t| t.ty != HirTy::Never)
                    .map(|t| self.origin(t, depth + 1));
                let b = else_branch
                    .as_deref()
                    .filter(|t| t.ty != HirTy::Never)
                    .map(|t| self.origin(t, depth + 1));
                combine([a, b].into_iter().flatten())
            }
            HirExprKind::Match { arms, .. } => combine(
                arms.iter()
                    .filter(|a| a.body.ty != HirTy::Never)
                    .map(|a| self.origin(&a.body, depth + 1)),
            ),
            // `await`'s poll loop: the value its `break` carries.
            HirExprKind::Loop { label, body, .. } => {
                let mut out = Vec::new();
                loop_breaks(body, label.as_deref(), true, &mut out);
                combine(out.into_iter().map(|v| self.origin(v, depth + 1)))
            }
            _ => Origin::Other,
        }
    }

    fn block(&mut self, b: &'h HirBlock, mut st: State, keep: bool) -> State {
        if b.label.is_some() {
            self.targets.push(Target {
                label: b.label.clone(),
                is_loop: false,
                keep,
                breaks: None,
                conts: None,
            });
        }
        for s in &b.stmts {
            st = self.stmt(s, st);
        }
        if let Some(t) = &b.trailing {
            st = self.expr(t, st, keep);
        }
        if b.label.is_some() {
            let t = self.targets.pop().expect("the block's target");
            st = join(st, t.breaks);
        }
        st
    }

    fn stmt(&mut self, s: &'h HirStmt, st: State) -> State {
        st.as_ref()?;
        match s {
            HirStmt::Let { local, init } => {
                let mut st = self.expr(init, st, true);
                self.bind(*local, init, &mut st);
                self.wrap(*local, init, &mut st);
                st
            }
            HirStmt::Assign { local, value } => {
                let mut st = self.expr(value, st, true);
                self.assign(*local, value, &mut st);
                st
            }
            HirStmt::ArrayAssign { index, value, .. } => {
                let st = self.expr(index, st, false);
                self.expr(value, st, false)
            }
            HirStmt::StaticAssign { value, .. } => self.expr(value, st, false),
            HirStmt::Expr(e) => {
                let mut st = self.expr(e, st, true);
                for lit in std::mem::take(&mut self.pending) {
                    for fe in children(lit) {
                        if let Origin::Alias(g) = self.origin(fe, 0) {
                            set(&mut st, g, |v| (v & !OPEN) | GONE);
                        }
                    }
                }
                if e.ty != HirTy::Never && self.origin(e, 0) == Origin::Fresh {
                    if let Some(sid) = self.res.tracked(&e.ty) {
                        self.temp(e.span, sid, "its value is dropped here");
                    }
                }
                st
            }
            HirStmt::Return(v) => {
                let st = match v {
                    Some(v) => self.expr(v, st, false),
                    None => st,
                };
                let thrown = v.as_ref().is_some_and(is_throw);
                let at = v.as_ref().map_or(self.last, |v| v.span);
                self.exit(&st, Some((at, thrown)));
                None
            }
            HirStmt::Break { label, value } => {
                let i = self.target(label.as_deref(), false);
                let keep = i.and_then(|i| self.targets.get(i)).is_some_and(|t| t.keep);
                let st = match value {
                    Some(v) => self.expr(v, st, keep),
                    None => st,
                };
                if let Some(t) = i.and_then(|i| self.targets.get_mut(i)) {
                    t.breaks = join(t.breaks.take(), st);
                }
                None
            }
            HirStmt::Continue { label } => {
                let i = self.target(label.as_deref(), true);
                if let Some(t) = i.and_then(|i| self.targets.get_mut(i)) {
                    t.conts = join(t.conts.take(), st);
                }
                None
            }
            HirStmt::Panic(e) => {
                self.expr(e, st, false);
                None
            }
        }
    }

    fn target(&self, label: Option<&str>, needs_loop: bool) -> Option<usize> {
        self.targets.iter().rposition(|t| match label {
            None => t.is_loop,
            Some(l) => t.label.as_deref() == Some(l) && (t.is_loop || !needs_loop),
        })
    }

    /// A `let` of `local` to `init` (walked already).
    fn bind(&mut self, local: LocalId, init: &'h HirExpr, st: &mut State) {
        let Some(sid) = self.local_ty(local).and_then(|t| self.res.tracked(t)) else {
            return;
        };
        match self.origin(init, 0) {
            Origin::Alias(g) if self.groups[g].sid == sid => {
                self.hold(local, g);
            }
            Origin::Fresh => {
                // A `let` run again (a loop) while its last value is open.
                if let Some(&g) = self.member.get(&local) {
                    if get(st, g) & OPEN != 0 {
                        self.leak(
                            g,
                            init.span,
                            "it is still open when the loop comes back here and opens another",
                        );
                    }
                }
                let g = self.group(local, sid, true, init.span);
                set(st, g, |_| OPEN);
            }
            _ => {
                if self.res.types.contains_key(&sid) {
                    let g = self.group(local, sid, false, init.span);
                    set(st, g, |_| 0);
                } else {
                    self.member.remove(&local);
                }
            }
        }
    }

    /// `let h = S { f: x, .. }`: `h.f` is followed as `x`. A struct
    /// literal kept for a holder that is not this one hands its values on.
    fn wrap(&mut self, local: LocalId, init: &'h HirExpr, st: &mut State) {
        let pending = std::mem::take(&mut self.pending);
        let mut x = init;
        while let HirExprKind::Block(b) = &x.kind {
            match &b.trailing {
                Some(t) if b.label.is_none() => x = t,
                _ => break,
            }
        }
        let named = self.local_name(local);
        for lit in pending {
            let HirExprKind::StructInit { struct_id, fields } = &lit.kind else {
                continue;
            };
            let here = std::ptr::eq(lit, x) && named.is_some();
            for (i, fe) in fields.iter().enumerate() {
                let Some(sid) = self.res.tracked(&fe.ty) else {
                    continue;
                };
                let i = u32::try_from(i).unwrap_or(u32::MAX);
                match (self.origin(fe, 0), here) {
                    (Origin::Alias(g), true) => {
                        self.wrappers.entry(local).or_default().push((i, g));
                    }
                    (Origin::Fresh, true) => {
                        let field = self
                            .res
                            .structs
                            .get(struct_id)
                            .and_then(|d| d.fields.get(i as usize))
                            .map_or("?", |f| f.name.as_str());
                        let name = format!("{}.{field}", named.unwrap_or("?"));
                        self.groups.push(Group {
                            sid,
                            owned: true,
                            name: Some(name),
                            at: fe.span,
                            closed: false,
                            leak: None,
                            twice: None,
                        });
                        let g = self.groups.len() - 1;
                        set(st, g, |_| OPEN);
                        self.wrappers.entry(local).or_default().push((i, g));
                    }
                    (Origin::Alias(g), false) => set(st, g, |v| (v & !OPEN) | GONE),
                    _ => {}
                }
            }
        }
    }

    /// `local = value` (walked already).
    fn assign(&mut self, local: LocalId, value: &'h HirExpr, st: &mut State) {
        for lit in std::mem::take(&mut self.pending) {
            for fe in children(lit) {
                if let Origin::Alias(g) = self.origin(fe, 0) {
                    set(st, g, |v| (v & !OPEN) | GONE);
                }
            }
        }
        if let Some(fs) = self.wrappers.remove(&local) {
            for (_, g) in fs {
                set(st, g, |v| (v & !OPEN) | GONE);
            }
        }
        let o = self.origin(value, 0);
        if let Some(&g) = self.member.get(&local) {
            if o == Origin::Alias(g) {
                return;
            }
            if get(st, g) & OPEN != 0 {
                self.leak(g, value.span, "it is still open when this replaces it");
            }
        }
        let Some(sid) = self.local_ty(local).and_then(|t| self.res.tracked(t)) else {
            return;
        };
        match o {
            Origin::Alias(g) if self.groups[g].sid == sid => {
                self.hold(local, g);
            }
            Origin::Fresh => {
                let g = self.group(local, sid, true, value.span);
                set(st, g, |_| OPEN);
            }
            _ => {
                self.member.remove(&local);
            }
        }
    }

    /// Walk `e`. `keep`: its value goes on to a holder this walk follows
    /// (a `let`, a receiver), so a member read there is not handed on.
    fn expr(&mut self, e: &'h HirExpr, st: State, keep: bool) -> State {
        st.as_ref()?;
        if !e.span.is_dummy() {
            self.last = e.span;
        }
        let st = match &e.kind {
            HirExprKind::Local(x) => {
                let mut st = st;
                if !keep {
                    if let Some(&g) = self.member.get(x) {
                        set(&mut st, g, |v| (v & !OPEN) | GONE);
                    }
                    for &(_, g) in self.wrappers.get(x).into_iter().flatten() {
                        set(&mut st, g, |v| (v & !OPEN) | GONE);
                    }
                }
                st
            }
            HirExprKind::Block(b) => self.block(b, st, keep),
            HirExprKind::StructInit { fields, .. } if keep => {
                let mut st = st;
                for x in fields {
                    let k = self.res.tracked(&x.ty).is_some();
                    st = self.expr(x, st, k);
                }
                self.pending.push(e);
                st
            }
            HirExprKind::SumWrap { value, .. } => match value {
                Some(v) => self.expr(v, st, keep),
                None => st,
            },
            HirExprKind::TupleField { target, .. } | HirExprKind::FieldRead { target, .. } => {
                self.expr(target, st, true)
            }
            HirExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                let st = self.expr(cond, st, false);
                // A cancelled task leaves through its cleanups: `using`
                // covers that path, a plain `close()` cannot.
                let a = if self.cancel_check(cond) {
                    None
                } else {
                    self.block(then_block, st.clone(), keep)
                };
                let b = match else_branch {
                    Some(x) => self.expr(x, st, keep),
                    None => st,
                };
                join(a, b)
            }
            HirExprKind::Match { scrutinee, arms } => {
                let o = self.origin(scrutinee, 0);
                let narrows = o != Origin::Other;
                let st = self.expr(scrutinee, st, narrows);
                let mut out = None;
                for a in arms {
                    let mut s = st.clone();
                    if narrows {
                        let mut bound = Vec::new();
                        a.pat.each_binding(&mut |l| bound.push(l));
                        for l in bound {
                            let Some(sid) = self.local_ty(l).and_then(|t| self.res.tracked(t))
                            else {
                                continue;
                            };
                            match o {
                                Origin::Alias(g) if self.groups[g].sid == sid => {
                                    self.hold(l, g);
                                }
                                // A temporary of the lowering (`await`'s
                                // ready value): what it flows into owns it.
                                Origin::Fresh if self.hidden(l) => {
                                    self.binds.insert(l, scrutinee);
                                }
                                Origin::Fresh => {
                                    let g = self.group(l, sid, true, scrutinee.span);
                                    set(&mut s, g, |_| OPEN);
                                }
                                _ => {}
                            }
                        }
                    }
                    if let Some(gd) = &a.guard {
                        s = self.expr(gd, s, false);
                    }
                    s = self.expr(&a.body, s, keep);
                    out = join(out, s);
                }
                out
            }
            HirExprKind::Loop { label, body, .. } => self.looped(label.as_deref(), None, body, st),
            HirExprKind::While { label, cond, body } => {
                self.looped(label.as_deref(), Some(cond), body, st)
            }
            HirExprKind::Call { callee, args } => self.call(*callee, args, st),
            HirExprKind::ClosureValue { fn_id, env, .. } => {
                let mut st = st;
                let fields: Vec<&'h HirExpr> = match &env.kind {
                    HirExprKind::Tuple { fields } => fields.iter().collect(),
                    _ => vec![&**env],
                };
                for (k, x) in fields.into_iter().enumerate() {
                    let Origin::Alias(g) = self.origin(x, 0) else {
                        st = self.expr(x, st, false);
                        continue;
                    };
                    st = self.expr(x, st, true);
                    // A closure that only reads a captured `Sender` does
                    // not close it for this body.
                    let sid = self.groups[g].sid;
                    let k = u32::try_from(k).unwrap_or(u32::MAX);
                    if !(self.res.senders.contains(&sid) && self.res.env_reads_only(*fn_id, k, sid))
                    {
                        set(&mut st, g, |v| (v & !OPEN) | GONE);
                    }
                }
                st
            }
            _ => {
                let mut st = st;
                for c in children(e) {
                    st = self.expr(c, st, false);
                }
                st
            }
        };
        if e.ty == HirTy::Never { None } else { st }
    }

    fn call(&mut self, callee: FunctionId, args: &'h [HirExpr], mut st: State) -> State {
        for (i, a) in args.iter().enumerate() {
            let o = self.origin(a, 0);
            let sid = self.res.tracked(&a.ty);
            match (o, sid) {
                (Origin::Alias(g), Some(sid)) if self.groups[g].sid == sid => {
                    st = self.expr(a, st, true);
                    let m = (i == 0).then(|| self.res.method(callee, sid)).flatten();
                    let flags = get(&st, g);
                    match m {
                        Some(m) => {
                            if flags & RELEASED != 0 && st.is_some() {
                                self.twice(g, a.span, m);
                            }
                            if m == "drop" {
                                set(&mut st, g, |v| (v & !OPEN) | CLOSED | RELEASED);
                                self.groups[g].closed = true;
                            } else if self.res.closes(m, sid) {
                                set(&mut st, g, |v| (v & !OPEN) | CLOSED);
                                self.groups[g].closed = true;
                            }
                        }
                        None => {
                            if !self.res.reads_only(callee, i) {
                                set(&mut st, g, |v| (v & !OPEN) | GONE);
                            }
                        }
                    }
                }
                (Origin::Fresh, Some(sid)) => {
                    st = self.expr(a, st, true);
                    let m = (i == 0).then(|| self.res.method(callee, sid)).flatten();
                    let dropped = match m {
                        Some(m) => !self.res.closes(m, sid),
                        None => self.res.reads_only(callee, i),
                    };
                    if dropped && st.is_some() {
                        self.temp(a.span, sid, "nothing closes it after this call");
                    }
                }
                _ => st = self.expr(a, st, false),
            }
        }
        st
    }

    fn looped(
        &mut self,
        label: Option<&str>,
        cond: Option<&'h HirExpr>,
        body: &'h HirBlock,
        st: State,
    ) -> State {
        self.targets.push(Target {
            label: label.map(str::to_owned),
            is_loop: true,
            keep: false,
            breaks: None,
            conts: None,
        });
        let mut head = st;
        let mut out = None;
        for _ in 0..8 {
            let at = match cond {
                Some(c) => self.expr(c, head.clone(), false),
                None => head.clone(),
            };
            if cond.is_some() {
                out = join(out, at.clone());
            }
            let end = self.block(body, at, false);
            let conts = self.targets.last_mut().and_then(|t| t.conts.take());
            let next = join(join(head.clone(), end), conts);
            if next == head {
                break;
            }
            head = next;
        }
        let t = self.targets.pop().expect("the loop's target");
        join(out, t.breaks)
    }

    /// A path leaves the body: `at` the `return` (`true`: a `throw`), or
    /// the end.
    fn exit(&mut self, st: &State, at: Option<(Span, bool)>) {
        let Some(v) = st else { return };
        for g in 0..self.groups.len() {
            if v.get(g).copied().unwrap_or(0) & OPEN == 0 {
                continue;
            }
            match at {
                Some((at, true)) => self.leak(g, at, "it is still open if this throws"),
                Some((at, false)) => self.leak(g, at, "it is still open when this returns"),
                None => {
                    let end = self.last;
                    self.leak(g, end, "it is still open where the function ends");
                }
            }
        }
    }

    fn leak(&mut self, g: usize, at: Span, why: &'static str) {
        let gr = &mut self.groups[g];
        if gr.owned && gr.leak.is_none() {
            gr.leak = Some((at, why));
        }
    }

    /// The leaks found, once the whole body is walked.
    fn report(&mut self) {
        for gr in &self.groups {
            let Some((at, why)) = gr.leak else { continue };
            if !self.files.contains(&gr.at.file) {
                continue;
            }
            let sender = self.res.senders.contains(&gr.sid);
            if !sender && !self.res.types.get(&gr.sid).is_some_and(|t| t.1) {
                continue;
            }
            let ty = self.res.name(gr.sid);
            let name = gr.name.as_deref();
            let (lint, msg, help) = if sender {
                let who = name.map_or_else(|| "this `Sender`".to_owned(), |n| format!("`{n}`"));
                (
                    "unclosed_sender",
                    format!(
                        "{who} is {} closed: its receivers wait for more values forever",
                        if gr.closed { "not always" } else { "never" }
                    ),
                    format!(
                        "help: call `{}.close()` once it has sent its last value, on every \
                         path (each clone too; stdlib 2.17)",
                        name.unwrap_or("tx")
                    ),
                )
            } else {
                let fix = match (self.res.closer(gr.sid), name) {
                    (Some(m), Some(n)) => {
                        format!("bind it with `using`, or call `{n}.{m}()` on every path")
                    }
                    (Some(m), None) => {
                        format!("bind it with `using`, or call `{m}()` on it on every path")
                    }
                    (None, _) => "bind it with `using`".to_owned(),
                };
                (
                    "unclosed_resource",
                    match name {
                        Some(n) if gr.closed => {
                            format!("`{n}` holds a `{ty}` that is not closed on every path")
                        }
                        Some(n) => format!("`{n}` holds a `{ty}` that is never closed"),
                        None => format!("the `{ty}` made here is never closed"),
                    },
                    format!("help: {fix}; the GC does not close it (stdlib 2.13)"),
                )
            };
            let open = gr.at;
            let shown = at.start > open.start && at.file == open.file && !at.is_dummy();
            let mut notes = vec![help];
            if !shown {
                notes.insert(0, why.to_owned());
            }
            self.hits.push(Hit {
                lint,
                span: open,
                msg,
                notes,
                secondary: shown.then(|| (at, why.to_owned())),
            });
        }
    }

    /// A value never bound: `(try! File::open(p)).read_to_str()`.
    fn temp(&mut self, at: Span, sid: StructId, why: &str) {
        if !self.files.contains(&at.file)
            || !self.temps.insert((at.file, at.start))
            || !(self.res.senders.contains(&sid) || self.res.types.get(&sid).is_some_and(|t| t.1))
        {
            return;
        }
        let ty = self.res.name(sid);
        let sender = self.res.senders.contains(&sid);
        self.hits.push(Hit {
            lint: if sender {
                "unclosed_sender"
            } else {
                "unclosed_resource"
            },
            span: at,
            msg: format!("the `{ty}` made here is never closed: {why}"),
            notes: vec![format!(
                "help: bind it first: `using x = .. {{ .. }}`, or `let x = ..;` and call `x.{}()` \
                 when done; the GC does not close it (stdlib 2.13)",
                self.res.closer(sid).unwrap_or("close")
            )],
            secondary: None,
        });
    }

    /// A use of group `g` after a `using` (or `drop`) released it.
    fn twice(&mut self, g: usize, at: Span, m: &str) {
        // One finding per value: a second release over a use after one.
        let drop = m == "drop";
        let gr = &mut self.groups[g];
        if self.files.contains(&at.file) && gr.twice.is_none_or(|(_, d)| drop && !d) {
            gr.twice = Some((at, drop));
        }
    }

    fn report_twice(&mut self) {
        for gr in &self.groups {
            let Some((at, drop)) = gr.twice else { continue };
            let ty = self.res.name(gr.sid);
            let who = gr
                .name
                .as_deref()
                .map_or_else(|| format!("this `{ty}`"), |n| format!("`{n}`"));
            let msg = if drop {
                format!("{who} is released twice: an earlier `using` already released it")
            } else {
                format!("{who} is used after a `using` released it")
            };
            self.hits.push(Hit {
                lint: "using_twice",
                span: at,
                msg,
                notes: vec![
                    "help: do all the work inside one `using` block, or give each `using` a \
                     value of its own"
                        .to_owned(),
                ],
                secondary: None,
            });
        }
    }
}

/// A returned carrier `(true, .., err)`: the lowering of a `throw`.
fn is_throw(v: &HirExpr) -> bool {
    let HirExprKind::Tuple { fields } = &v.kind else {
        return false;
    };
    matches!(v.ty, HirTy::Throws { .. })
        && matches!(
            fields.first().map(|f| &f.kind),
            Some(HirExprKind::Const(rokc_ir_common::ConstVal::Bool(true)))
        )
}

/// The values of the `break v` (`own`: unlabelled too) and `break
/// 'label v` in loop body `b` that leave the loop.
fn loop_breaks<'h>(b: &'h HirBlock, label: Option<&str>, own: bool, out: &mut Vec<&'h HirExpr>) {
    for s in &b.stmts {
        match s {
            HirStmt::Break {
                label: l,
                value: Some(v),
            } if (l.is_none() && own) || (l.is_some() && l.as_deref() == label) => out.push(v),
            HirStmt::Let { init: e, .. }
            | HirStmt::Assign { value: e, .. }
            | HirStmt::Expr(e)
            | HirStmt::Return(Some(e)) => loop_breaks_in(e, label, own, out),
            _ => {}
        }
    }
    if let Some(t) = &b.trailing {
        loop_breaks_in(t, label, own, out);
    }
}

fn loop_breaks_in<'h>(e: &'h HirExpr, label: Option<&str>, own: bool, out: &mut Vec<&'h HirExpr>) {
    match &e.kind {
        HirExprKind::Block(b) => loop_breaks(b, label, own, out),
        HirExprKind::If {
            then_block,
            else_branch,
            ..
        } => {
            loop_breaks(then_block, label, own, out);
            if let Some(x) = else_branch {
                loop_breaks_in(x, label, own, out);
            }
        }
        HirExprKind::Match { arms, .. } => {
            for a in arms {
                loop_breaks_in(&a.body, label, own, out);
            }
        }
        // An inner loop takes the unlabelled ones.
        HirExprKind::Loop { body, .. } | HirExprKind::While { body, .. } if label.is_some() => {
            loop_breaks(body, label, false, out);
        }
        _ => {}
    }
}

/// The values of the `break 'label v` in `b` that leave `b`.
fn breaks_to<'h>(b: &'h HirBlock, label: &str) -> Vec<&'h HirExpr> {
    fn stmts<'h>(b: &'h HirBlock, label: &str, out: &mut Vec<&'h HirExpr>) {
        for s in &b.stmts {
            if let HirStmt::Break {
                label: Some(l),
                value: Some(v),
            } = s
            {
                if l == label {
                    out.push(v);
                }
            }
        }
    }
    let mut out = Vec::new();
    stmts(b, label, &mut out);
    each_block(b, &mut |e| {
        if let HirExprKind::Block(x)
        | HirExprKind::Loop { body: x, .. }
        | HirExprKind::While { body: x, .. }
        | HirExprKind::If { then_block: x, .. } = &e.kind
        {
            stmts(x, label, &mut out);
        }
    });
    out
}

fn combine(os: impl IntoIterator<Item = Origin>) -> Origin {
    let mut out = None;
    for o in os {
        match (o, out) {
            (Origin::Alias(g), _) => return Origin::Alias(g),
            (Origin::Other, _) => out = Some(Origin::Other),
            (Origin::Fresh, None) => out = Some(Origin::Fresh),
            (Origin::Fresh, Some(_)) => {}
        }
    }
    out.unwrap_or(Origin::Other)
}

/// The direct subexpressions of `e` that are not blocks of statements
/// (`Block`, loops, `If`, `Match` are walked on their own).
fn children(e: &HirExpr) -> Vec<&HirExpr> {
    use HirExprKind as K;
    match &e.kind {
        K::PrimBinOp { lhs, rhs, .. } => vec![lhs, rhs],
        K::PrimUnaryOp { operand: x, .. }
        | K::Cast { operand: x, .. }
        | K::ClosureValue { env: x, .. }
        | K::FieldRead { target: x, .. }
        | K::TupleField { target: x, .. }
        | K::ArrayRepeat { value: x, .. }
        | K::BufNew { len: x, .. }
        | K::RawRead { addr: x, .. }
        | K::AddrOf { place: x }
        | K::UnionWrite { value: x, .. }
        | K::UnionRead { target: x, .. }
        | K::Suspend { value: x, .. }
        | K::SumWrap { value: Some(x), .. } => vec![x],
        K::Call { args: xs, .. }
        | K::StructInit { fields: xs, .. }
        | K::EnumVariantInit { args: xs, .. }
        | K::Tuple { fields: xs }
        | K::ArrayInit { elements: xs, .. }
        | K::CoroNew { args: xs, .. }
        | K::MlirOp { args: xs, .. } => xs.iter().collect(),
        K::CallIndirect {
            callee: c, args, ..
        }
        | K::CallClosure {
            closure: c, args, ..
        } => std::iter::once(&**c).chain(args).collect(),
        K::WithField { target, value, .. } => vec![target, value],
        K::RawWrite { addr, value, .. } => vec![addr, value],
        K::BufGet { buf, index, .. }
        | K::ArrayIndex {
            array: buf, index, ..
        } => {
            vec![buf, index]
        }
        K::BufSet {
            buf, index, value, ..
        } => vec![buf, index, value],
        K::BufAtomic {
            buf, index, args, ..
        } => [&**buf, &**index].into_iter().chain(args).collect(),
        _ => Vec::new(),
    }
}

/// An `async` body in evaluation order (`thread_local_across_await`).
#[derive(Default)]
struct Seq {
    n: u32,
    defs: Vec<(LocalId, u32, Span)>,
    uses: Vec<(LocalId, u32, Span)>,
    awaits: Vec<(u32, Span)>,
    loops: Vec<(u32, u32)>,
}
fn seq_block(res: &Resources<'_>, b: &HirBlock, s: &mut Seq) {
    for st in &b.stmts {
        match st {
            HirStmt::Let { local, init } => {
                seq_expr(res, init, s);
                let mut x = init;
                while let HirExprKind::Block(b) = &x.kind {
                    match &b.trailing {
                        Some(t) if b.stmts.is_empty() => x = t,
                        _ => break,
                    }
                }
                let got = match &x.kind {
                    HirExprKind::Call { callee, args } => args.first().is_some_and(|a| {
                        matches!(&a.ty, HirTy::Struct(id) if res.tls.contains(id))
                            && res.fns.get(callee).is_some_and(|f| leaf(&f.name) == "get")
                    }),
                    // `let m = l` of one.
                    HirExprKind::Local(y) => s.defs.iter().any(|d| d.0 == *y),
                    _ => false,
                };
                s.n += 1;
                if got {
                    s.defs.push((*local, s.n, init.span));
                }
            }
            HirStmt::Assign { value: e, .. }
            | HirStmt::StaticAssign { value: e, .. }
            | HirStmt::Expr(e)
            | HirStmt::Panic(e)
            | HirStmt::Return(Some(e))
            | HirStmt::Break { value: Some(e), .. } => seq_expr(res, e, s),
            HirStmt::ArrayAssign {
                array_local,
                index,
                value,
                ..
            } => {
                seq_expr(res, index, s);
                seq_expr(res, value, s);
                s.n += 1;
                s.uses.push((*array_local, s.n, value.span));
            }
            HirStmt::Return(None) | HirStmt::Break { .. } | HirStmt::Continue { .. } => {}
        }
    }
    if let Some(t) = &b.trailing {
        seq_expr(res, t, s);
    }
}
fn seq_expr(res: &Resources<'_>, e: &HirExpr, s: &mut Seq) {
    match &e.kind {
        HirExprKind::Local(x) => {
            s.n += 1;
            s.uses.push((*x, s.n, e.span));
        }
        HirExprKind::Suspend { value, is_final } => {
            seq_expr(res, value, s);
            s.n += 1;
            if !is_final {
                s.awaits.push((s.n, e.span));
            }
        }
        HirExprKind::Block(b) => seq_block(res, b, s),
        HirExprKind::Loop { body, .. } => {
            let start = s.n;
            seq_block(res, body, s);
            s.loops.push((start, s.n));
        }
        HirExprKind::While { cond, body, .. } => {
            let start = s.n;
            seq_expr(res, cond, s);
            seq_block(res, body, s);
            s.loops.push((start, s.n));
        }
        HirExprKind::If {
            cond,
            then_block,
            else_branch,
        } => {
            seq_expr(res, cond, s);
            seq_block(res, then_block, s);
            if let Some(x) = else_branch {
                seq_expr(res, x, s);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            seq_expr(res, scrutinee, s);
            for a in arms {
                if let Some(g) = &a.guard {
                    seq_expr(res, g, s);
                }
                seq_expr(res, &a.body, s);
            }
        }
        _ => {
            for c in children(e) {
                seq_expr(res, c, s);
            }
        }
    }
}
/// `thread_local_across_await`: in an `async` body, a local bound to a
/// `ThreadLocal::get()` result and used after a suspension point.
fn thread_local_across_await(
    res: &Resources<'_>,
    f: &HirFunction,
    files: &HashSet<u32>,
    hits: &mut Vec<Hit>,
) {
    if !f.is_async || res.tls.is_empty() {
        return;
    }
    let Some(body) = &f.body else { return };
    let mut s = Seq::default();
    seq_block(res, body, &mut s);
    let mut seen = HashSet::new();
    for &(l, def, at) in &s.defs {
        if !files.contains(&at.file) {
            continue;
        }
        let after = s.uses.iter().filter(|u| u.0 == l && u.1 > def);
        let hit = after.clone().find_map(|u| {
            let direct = s.awaits.iter().find(|a| def < a.0 && a.0 < u.1);
            // An `await` in a loop the local is used in, made before it.
            let looped = s.loops.iter().find_map(|&(ls, le)| {
                (def <= ls && ls < u.1 && u.1 <= le)
                    .then(|| s.awaits.iter().find(|a| ls < a.0 && a.0 <= le))
                    .flatten()
            });
            direct.or(looped).map(|a| (u.2, a.1))
        });
        let Some((used, waited)) = hit else { continue };
        if !seen.insert((at.file, at.start)) {
            continue;
        }
        let name = f
            .locals
            .iter()
            .find(|x| x.id == l)
            .and_then(|x| x.name.as_deref())
            .filter(|n| !n.starts_with('#'))
            .unwrap_or("it");
        hits.push(Hit {
            lint: "thread_local_across_await",
            span: used,
            msg: format!(
                "`{name}` is a `ThreadLocal::get()` value used after an `await`: the task may \
                 resume on another thread, and this one's value is then shared"
            ),
            notes: vec![format!(
                "help: call `.get()` again after the `await` instead of keeping `{name}` across \
                 it (stdlib 3.4)"
            )],
            secondary: Some((
                waited,
                "the task may move to another thread here".to_owned(),
            )),
        });
    }
}
