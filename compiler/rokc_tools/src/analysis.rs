//! The check pipeline (`rokc check`, `rokc lint`, the language server)
//! and the queries the language server and the REPL run on its result.

use std::collections::HashSet;
use std::path::Path;

use rokc_ast::{ImplItem, Item, TraitItem, doc_hidden};
use rokc_diagnostic::{RokDiagnostic, Span};
pub use rokc_hir::each_block;
use rokc_hir::{HirExpr, HirFunction, HirModule, HirTy};
use rokc_interface::{BuildOptions, CompileResult, Session, SourceFile};

use crate::lint::{self, Level};

/// Front end, lints at `levels`, then with `codegen` MLIR codegen.
///
/// Codegen rejects what the backend can't lower. `source`: the root file's
/// text (`None`: read `path`). Diagnostics in report order; the parsed
/// files are kept.
pub fn check(
    session: &mut Session,
    source: Option<&str>,
    path: &Path,
    levels: &[(Level, String)],
    codegen: Option<&BuildOptions>,
) -> (CompileResult, Vec<RokDiagnostic>) {
    session.set_keep_ast(true);
    session.set_library_items(true);
    let result = match source {
        Some(s) => session.compile_source(s, path, rokc_interface::SourceMap::new()),
        None => session.compile_file(path),
    };
    if result.has_errors() {
        let diags = result
            .warnings
            .iter()
            .chain(&result.errors)
            .cloned()
            .collect();
        return (result, diags);
    }
    let mut diags = lint::run(&result, levels);
    if let Some(opts) = codegen {
        diags.extend(result.check_codegen(opts).err());
    }
    (result, diags)
}

/// Run compile `f` on the compile thread; a panic there or on a thread it
/// spawns is `Err(message)` and the caller goes on.
///
/// Compiles run one at a time on one long-lived thread, so per-thread
/// state carries over (a REPL cell's front end, then its codegen). rokc
/// aborts on panic, so a panicking thread is parked forever instead (the
/// next compile gets a new thread); what it holds leaks, and a late panic
/// of an abandoned compile may be blamed on the current one. A panic on
/// the thread that called `isolate` is fatal as before.
pub fn isolate<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    use std::sync::Mutex;
    use std::sync::mpsc::{Sender, channel};
    type Job = Box<dyn FnOnce() + Send>;
    type Fail = Box<dyn FnOnce(String) + Send>;
    static WORKER: Mutex<Option<Sender<Job>>> = Mutex::new(None);
    /// Fails the running compile.
    static FAIL: Mutex<Option<Fail>> = Mutex::new(None);
    static HOOK: std::sync::Once = std::sync::Once::new();
    thread_local! {
        static CALLER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    fn lock<V>(m: &Mutex<V>) -> std::sync::MutexGuard<'_, V> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    HOOK.call_once(|| {
        let fatal = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // A closed stdout / stderr ends rokc, not just the compile.
            if CALLER.with(std::cell::Cell::get) || rokc_diagnostic::output_closed(info) {
                return fatal(info);
            }
            let fail = lock(&FAIL).take();
            match fail {
                Some(fail) => fail(info.to_string()),
                None => eprintln!("rokc: internal compiler error: {info}"),
            }
            loop {
                std::thread::park();
            }
        }));
    });
    CALLER.with(|c| c.set(true));
    let (tx, rx) = channel::<Result<T, String>>();
    let failed = tx.clone();
    *lock(&FAIL) = Some(Box::new(move |e| drop(failed.send(Err(e)))));
    let job: Job = Box::new(move || drop(tx.send(Ok(f()))));
    let mut worker = lock(&WORKER);
    let send = worker.get_or_insert_with(|| {
        let (jobs, next) = channel::<Job>();
        // The stack of rokc's main thread (address space until used).
        std::thread::Builder::new()
            .name("compile".into())
            .stack_size(1 << 30)
            .spawn(move || next.into_iter().for_each(|job| job()))
            .expect("spawn the compile thread");
        jobs
    });
    send.send(job).expect("the compile thread waits for jobs");
    let r = rx.recv().expect("a compile replies or fails");
    lock(&FAIL).take();
    if r.is_err() {
        // Parked, or waiting on a parked thread.
        *worker = None;
    }
    r
}

/// One check of a buffer.
pub struct Analysis {
    pub diags: Vec<RokDiagnostic>,
    /// The source files of this compile.
    pub source_map: rokc_interface::SourceMap,
    /// Present when the program type-checked.
    pub hir: Option<HirModule>,
    pub sources: Vec<SourceFile>,
    /// The items of the library files `sources` leaves out.
    pub library: Option<&'static rokc_ast::Program>,
}

/// Check `source` as the root file `path` (no codegen), `isolate`d: an
/// internal compiler error is the analysis's one diagnostic.
pub fn analyze(
    mut session: Session,
    source: &str,
    path: &Path,
    levels: &[(Level, String)],
) -> Analysis {
    let (source, path, levels) = (source.to_owned(), path.to_owned(), levels.to_vec());
    isolate(move || {
        let (r, diags) = check(&mut session, Some(&source), &path, &levels, None);
        Analysis {
            diags,
            source_map: r.source_map,
            hir: r.modules.into_iter().next().map(|m| m.hir),
            sources: r.sources,
            library: r.library,
        }
    })
    .unwrap_or_else(|e| Analysis {
        diags: vec![ice(&e)],
        source_map: rokc_interface::SourceMap::new(),
        hir: None,
        sources: Vec::new(),
        library: None,
    })
}

/// The diagnostic of an `isolate`d compile's panic.
pub fn ice(message: &str) -> RokDiagnostic {
    RokDiagnostic::error(
        rokc_diagnostic::ErrorKind::Internal,
        Span::default(),
        format!("internal compiler error: {message}"),
    )
}

impl Analysis {
    /// Path and text of a span's file.
    pub fn file(&self, span: Span) -> Option<&(String, String)> {
        self.source_map.file(span.file)
    }

    /// Item lists that may hold an item of file `file`: its parse, and
    /// the library's items.
    fn items_of(&self, file: u32) -> impl Iterator<Item = &[Item]> {
        self.sources
            .iter()
            .filter(move |f| f.file_id == file)
            .map(|f| f.items.as_slice())
            .chain(self.library.map(|p| p.items.as_slice()))
    }

    pub(crate) fn in_file(&self, span: Span, path: &str) -> bool {
        self.file(span).is_some_and(|(p, _)| p == path)
    }

    /// Spans of the functions and types marked `#[doc(hidden)]`, or
    /// inside a module so marked: compiler hooks completion leaves out.
    pub fn hidden_spans(&self) -> HashSet<Span> {
        fn walk(items: &[Item], all: bool, out: &mut HashSet<Span>) {
            for item in items {
                let mut mark = |attrs: &[rokc_ast::Attribute], span: Span| {
                    if all || doc_hidden(attrs) {
                        out.insert(span);
                    }
                };
                match item {
                    Item::Fn(f) => mark(&f.attrs, f.span),
                    Item::Struct(x) => mark(&x.attrs, x.span),
                    Item::Enum(x) => mark(&x.attrs, x.span),
                    Item::Impl(i) => {
                        for ii in &i.items {
                            if let ImplItem::Method(m) = ii {
                                mark(&m.attrs, m.span);
                            }
                        }
                    }
                    Item::Trait(t) => {
                        for ti in &t.items {
                            if let TraitItem::Method(m) = ti {
                                mark(&m.attrs, m.span);
                            }
                        }
                    }
                    Item::Mod(m) => walk(
                        m.items.as_deref().unwrap_or_default(),
                        all || doc_hidden(&m.attrs),
                        out,
                    ),
                    _ => {}
                }
            }
        }
        let mut out = HashSet::new();
        for f in &self.sources {
            walk(&f.items, false, &mut out);
        }
        if let Some(p) = self.library {
            walk(&p.items, false, &mut out);
        }
        out
    }

    /// The doc comment of the function defined at `span`.
    pub fn fn_doc(&self, span: Span) -> String {
        fn find(items: &[Item], span: Span) -> Option<&[rokc_ast::Attribute]> {
            items.iter().find_map(|item| match item {
                Item::Fn(f) if f.span == span => Some(f.attrs.as_slice()),
                Item::Impl(i) => i.items.iter().find_map(|ii| match ii {
                    ImplItem::Method(m) if m.span == span => Some(m.attrs.as_slice()),
                    _ => None,
                }),
                Item::Trait(t) => t.items.iter().find_map(|ti| match ti {
                    TraitItem::Method(m) if m.span == span => Some(m.attrs.as_slice()),
                    _ => None,
                }),
                Item::Mod(m) => find(m.items.as_deref().unwrap_or_default(), span),
                _ => None,
            })
        }
        self.items_of(span.file)
            .find_map(|items| find(items, span))
            .map(crate::doc::doc_text)
            .unwrap_or_default()
    }

    /// The doc comment of field `name` of the struct defined at `span`.
    pub fn field_doc(&self, span: Span, name: &str) -> String {
        fn find<'a>(
            items: &'a [Item],
            span: Span,
            name: &str,
        ) -> Option<&'a [rokc_ast::Attribute]> {
            items.iter().find_map(|item| match item {
                Item::Struct(s) if s.span == span => match &s.kind {
                    rokc_ast::StructKind::Named(fs) => fs
                        .iter()
                        .find(|f| f.name == name)
                        .map(|f| f.attrs.as_slice()),
                    _ => None,
                },
                Item::Mod(m) => find(m.items.as_deref().unwrap_or_default(), span, name),
                _ => None,
            })
        }
        self.items_of(span.file)
            .find_map(|items| find(items, span, name))
            .map(crate::doc::doc_text)
            .unwrap_or_default()
    }

    /// Names the root file can use without a path besides its own: the
    /// core library's (implicit `use rok::core::**;`) and those its `use`
    /// items bring in. Sorted, without duplicates.
    pub fn scope_names(&self) -> Vec<String> {
        /// `(module path, name, is a module)` of each public item.
        type Names = Vec<(String, String, bool)>;
        fn shown(item: &Item) -> Option<&str> {
            use rokc_core::Visibility::Pub;
            let (vis, attrs, name) = match item {
                Item::Fn(x) => (x.vis, &x.attrs, &x.name),
                Item::Struct(x) => (x.vis, &x.attrs, &x.name),
                Item::Enum(x) => (x.vis, &x.attrs, &x.name),
                Item::Union(x) => (x.vis, &x.attrs, &x.name),
                Item::Trait(x) => (x.vis, &x.attrs, &x.name),
                Item::Const(x) => (x.vis, &x.attrs, &x.name),
                Item::Static(x) => (x.vis, &x.attrs, &x.name),
                Item::TypeAlias(x) => (x.vis, &x.attrs, &x.name),
                Item::Mod(x) => (x.vis, &x.attrs, &x.name),
                _ => return None,
            };
            (vis == Pub && !doc_hidden(attrs)).then_some(name.as_str())
        }
        fn walk(items: &[Item], path: &str, out: &mut Names) {
            for item in items {
                let Some(name) = shown(item) else { continue };
                out.push((
                    path.to_owned(),
                    name.to_owned(),
                    matches!(item, Item::Mod(_)),
                ));
                if let Item::Mod(m) = item {
                    let sub = m.items.as_deref().unwrap_or_default();
                    walk(sub, &format!("{path}::{name}"), out);
                }
            }
        }
        // `**` brings in the items of every module below, but only the
        // modules right below by name.
        fn glob(names: &Names, path: &str, deep: bool, out: &mut Vec<String>) {
            let under = format!("{path}::");
            for (m, n, is_mod) in names {
                if m == path || (deep && !is_mod && m.starts_with(&under)) {
                    out.push(n.clone());
                }
            }
        }
        fn uses(names: &Names, t: &rokc_ast::UseTree, base: &str, out: &mut Vec<String>) {
            let mut path = base.to_owned();
            for s in &t.prefix.segments {
                if !path.is_empty() {
                    path.push_str("::");
                }
                path.push_str(&s.name);
            }
            match &t.kind {
                rokc_ast::UseTreeKind::Simple(alias) => {
                    let last = t.prefix.segments.last().map(|s| s.name.clone());
                    out.extend(alias.clone().or(last));
                }
                rokc_ast::UseTreeKind::Glob => glob(names, &path, false, out),
                rokc_ast::UseTreeKind::GlobRecursive => glob(names, &path, true, out),
                rokc_ast::UseTreeKind::Group(ts) => {
                    for t in ts {
                        uses(names, t, &path, out);
                    }
                }
            }
        }
        let mut names = Names::new();
        // The library's items are flat, named by their full path; a
        // module's stub carries its visibility.
        let library = self.library.map(|p| p.items.as_slice()).unwrap_or_default();
        let hidden: HashSet<&str> = library
            .iter()
            .filter_map(|i| match i {
                Item::Mod(m) if shown(i).is_none() => Some(m.name.as_str()),
                _ => None,
            })
            .collect();
        for item in library {
            let Some((module, name)) = shown(item).and_then(|n| n.rsplit_once("::")) else {
                continue;
            };
            let mut up = Some(module);
            while let Some(m) = up {
                if hidden.contains(m) {
                    break;
                }
                up = m.rsplit_once("::").map(|(p, _)| p);
            }
            if up.is_none() {
                names.push((
                    module.to_owned(),
                    name.to_owned(),
                    matches!(item, Item::Mod(_)),
                ));
            }
        }
        for f in &self.sources {
            walk(&f.items, &f.module.join("::"), &mut names);
        }
        let mut out = Vec::new();
        glob(&names, "rok::core", true, &mut out);
        let root = self.sources.iter().filter(|f| f.package.is_empty());
        if let Some(f) = root.min_by_key(|f| f.module.len()) {
            for item in &f.items {
                if let Item::Use(u) = item {
                    uses(&names, &u.tree, "", &mut out);
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The path a diagnostic belongs to (explicit, else its primary span's).
    pub fn diag_path(&self, d: &RokDiagnostic) -> Option<String> {
        d.file
            .clone()
            .or_else(|| Some(self.file(d.primary.as_ref()?.span)?.0.clone()))
    }

    /// A typed expression of `path` at `pick`: the innermost (smallest span,
    /// deepest on ties), or with `outer` the outermost. Compound
    /// expressions span from their first token, so ties are common.
    pub fn find_expr(
        &self,
        path: &str,
        pick: impl Fn(Span) -> bool,
        outer: bool,
    ) -> Option<&HirExpr> {
        let hir = self.hir.as_ref()?;
        let mut best: Option<&HirExpr> = None;
        let size = |s: Span| s.end.saturating_sub(s.start);
        for f in &hir.functions {
            let Some(body) = &f.body else { continue };
            each_block(body, &mut |e| {
                if !e.span.is_dummy()
                    && self.in_file(e.span, path)
                    && pick(e.span)
                    && !matches!(e.ty, HirTy::Unknown)
                    && best.is_none_or(|b| {
                        if outer {
                            size(e.span) > size(b.span)
                        } else {
                            size(e.span) <= size(b.span)
                        }
                    })
                {
                    best = Some(e);
                }
            });
        }
        best
    }
}

/// User-facing type name (struct / enum by name, generics as `<..>`).
pub fn ty_name(hir: &HirModule, ty: &HirTy) -> String {
    ty_text(hir, ty, false)
}

/// `ty_name` with struct / enum by their full path (`rok::core::..`),
/// which names them from any module without a `use`.
pub fn ty_path(hir: &HirModule, ty: &HirTy) -> String {
    ty_text(hir, ty, true)
}

fn ty_text(hir: &HirModule, ty: &HirTy, full: bool) -> String {
    let rec = |t: &HirTy| ty_text(hir, t, full);
    let list = |ts: &[HirTy]| ts.iter().map(rec).collect::<Vec<_>>().join(", ");
    // Types by their own name (`List<i32>`, not the module path), as
    // the REPL shows them (907.7).
    let generic = |name: &str, args: &[HirTy]| {
        let base = name.split("__").next().unwrap_or(name);
        // An item in a block by its own name (spec 3.9.1).
        let base = if full {
            std::borrow::Cow::Borrowed(base)
        } else {
            rokc_core::local::leaf(base.rsplit("::").next().unwrap_or(base))
        };
        if args.is_empty() {
            base.into_owned()
        } else {
            format!("{base}<{}>", list(args))
        }
    };
    match ty {
        HirTy::Unknown => "{unknown}".to_owned(),
        HirTy::Unit => "()".to_owned(),
        HirTy::Never => "!".to_owned(),
        HirTy::Mlir(t) => format!("__mlir_type<{t:?}>"),
        HirTy::Prim(p) => p.name().to_owned(),
        HirTy::Any => "any".to_owned(),
        HirTy::AnyTrait(b) if b.is_empty() => "any".to_owned(),
        HirTy::AnyTrait(b) => {
            let b: Vec<String> = b.iter().map(|r| r.render(&r.path, rec)).collect();
            match b.as_slice() {
                [one] => format!("any {one}"),
                _ => format!("any ({})", b.join(" + ")),
            }
        }
        HirTy::Path(p) => p.join("::"),
        HirTy::None => "none".to_owned(),
        HirTy::Const(v, _) => v.to_string(),
        HirTy::OpenConst(n) => n.clone(),
        HirTy::NoThrow | HirTy::Var(_) | HirTy::Param(_) => "_".to_owned(),
        HirTy::Sum(_) => ty.sum_name(rec),
        HirTy::Tuple(ts) => format!("({})", list(ts)),
        HirTy::Struct(id) => hir.structs.iter().find(|s| s.id == *id).map_or_else(
            || "{struct}".to_owned(),
            |s| generic(&s.name, &s.generic_args),
        ),
        HirTy::Enum(id) => hir.enums.iter().find(|e| e.id == *id).map_or_else(
            || "{enum}".to_owned(),
            |e| generic(&e.name, &e.generic_args),
        ),
        HirTy::Array(t, n) => format!("[{}; {n}]", rec(t)),
        HirTy::GcBuf(t) => format!("GcBuf<{}>", rec(t)),
        HirTy::Fn(args, ret, _) => format!("fn({}) -> {}", list(args), rec(ret)),
        HirTy::Closure { args, ret, .. } => format!("closure({}) -> {}", list(args), rec(ret)),
        HirTy::Throws { ok, err } => format!("throws ({}) -> {}", rec(err), rec(ok)),
    }
}

/// `fn name(a: T, ..) -> R`.
pub fn signature(hir: &HirModule, f: &HirFunction) -> String {
    let params: Vec<String> = f
        .params
        .iter()
        // `#caller` (`#[track_caller]`) is hidden.
        .filter(|p| !p.name.starts_with('#'))
        .map(|p| format!("{}: {}", p.name, ty_name(hir, &p.ty)))
        .collect();
    format!(
        "fn {}({}) -> {}",
        short_name(&f.name),
        params.join(", "),
        ty_name(hir, &f.ret_ty)
    )
}

/// The name a user writes for a HIR function: the last `::` segment, the
/// method name of an `<Type>__method` (a dependency's
/// `<module>::<Type>.method[__by_Trait]`), a nested fn's own name.
pub fn short_name(name: &str) -> &str {
    let last = name.rsplit("::").next().unwrap_or(name);
    let mut segs = last.rsplit('/');
    let mut last = segs.next().unwrap_or(last);
    // `f/g/2`: a repeated name's counter.
    if last.bytes().all(|b| b.is_ascii_digit()) {
        last = segs.next().unwrap_or(last);
    }
    if let Some((_, method)) = last.split_once('.') {
        return method.split("__").next().unwrap_or(method);
    }
    last.rsplit("__").next().unwrap_or(last)
}
