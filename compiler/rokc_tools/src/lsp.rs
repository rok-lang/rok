//! Language server over stdio (JSON-RPC 2.0 with `Content-Length`
//! framing). Full-text sync; every query runs on the last check of the
//! buffer (`analysis::check`: front end and lints, no codegen).

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use rokc_ast::{Expr, ExprKind, ImplItem, Item, TraitItem};
use rokc_diagnostic::{Code, RokDiagnostic, Span};
use rokc_hir::walk::{AstVisitor, walk_item, walk_stmt};
use rokc_hir::{HirExprKind, HirTy};
use rokc_interface::Session;

use rokc_diagnostic::Json;

use crate::analysis::{Analysis, analyze, short_name, signature, ty_name};
use crate::lint::Levels;

mod actions;
mod signature;

struct Doc {
    text: String,
    /// Needs a compile (changed since the last one).
    dirty: bool,
    /// Inputs of the last check, including one that reported errors.
    inputs: Vec<PathBuf>,
    /// The last compile that type-checked: hover / definition /
    /// completion keep working while the buffer has errors.
    good: Option<Analysis>,
    /// The last published diagnostics and what they came from, for
    /// code actions.
    diags: Vec<(Json, RokDiagnostic)>,
}

struct Server<W> {
    out: W,
    bootstrap: Vec<PathBuf>,
    externs: Vec<(String, PathBuf)>,
    levels: Levels,
    docs: HashMap<String, Doc>,
    /// `initialize` came.
    initialized: bool,
    shutdown: bool,
}

/// Serve `input` until `exit` or EOF, linting at `levels`; exit code 0
/// after `shutdown`, else 1 (LSP spec).
pub fn serve(
    input: impl BufRead + Send + 'static,
    out: impl Write,
    bootstrap: Vec<PathBuf>,
    externs: Vec<(String, PathBuf)>,
    levels: impl Into<Levels>,
) -> i32 {
    if std::io::stdin().is_terminal() {
        eprintln!(
            "rokls speaks the language server protocol on stdin; start it from an editor \
             (see `rok help lsp`)"
        );
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut input = input;
        while let Some(msg) = read_message(&mut input) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    let mut server = Server {
        out,
        bootstrap,
        externs,
        levels: levels.into(),
        docs: HashMap::new(),
        initialized: false,
        shutdown: false,
    };
    while let Ok(first) = rx.recv() {
        // Handle everything already queued before compiling: a burst
        // of edits costs one compile, not one per keystroke.
        let batch: Vec<Json> = std::iter::once(first).chain(rx.try_iter()).collect();
        for msg in batch {
            if let Some(code) = server.handle(&msg) {
                return code;
            }
        }
        let dirty: Vec<String> = server
            .docs
            .iter()
            .filter(|(_, d)| d.dirty)
            .map(|(u, _)| u.clone())
            .collect();
        for uri in dirty {
            server.refresh(&uri);
        }
    }
    if !server.initialized {
        eprintln!("rokls: input ended before the editor sent `initialize`");
        return 1;
    }
    i32::from(!server.shutdown)
}

/// One framed message; `None` at end of input. Unparsable bodies are
/// skipped.
fn read_message(input: &mut impl BufRead) -> Option<Json> {
    loop {
        let mut len = None;
        loop {
            let mut line = String::new();
            if input.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse::<usize>().ok();
                }
            }
        }
        let mut body = vec![0; len?];
        input.read_exact(&mut body).ok()?;
        if let Ok(msg) = Json::parse(&String::from_utf8_lossy(&body)) {
            return Some(msg);
        }
    }
}

impl<W: Write> Server<W> {
    fn send(&mut self, msg: &Json) {
        let body = msg.to_string();
        // A closed client: nothing left to do but stop on the next read.
        let _ = write!(self.out, "Content-Length: {}\r\n\r\n{body}", body.len());
        let _ = self.out.flush();
    }

    fn error(&mut self, id: &Json, code: i32, message: String) {
        self.send(&Json::obj([
            ("jsonrpc", Json::str("2.0")),
            ("id", id.clone()),
            (
                "error",
                Json::obj([("code", Json::num(code)), ("message", Json::str(message))]),
            ),
        ]));
    }

    fn reply(&mut self, id: &Json, result: Json) {
        self.send(&Json::obj([
            ("jsonrpc", Json::str("2.0")),
            ("id", id.clone()),
            ("result", result),
        ]));
    }

    /// Returns the exit code on `exit`.
    fn handle(&mut self, msg: &Json) -> Option<i32> {
        let method = msg.get("method").as_str().unwrap_or("");
        let id = msg.get("id");
        let params = msg.get("params");
        let uri = params
            .get("textDocument")
            .get("uri")
            .as_str()
            .unwrap_or("")
            .to_owned();
        self.initialized |= method == "initialize";
        let result = match method {
            "initialize" => Json::obj([
                (
                    "capabilities",
                    Json::obj([
                        ("textDocumentSync", Json::num(1)),
                        ("hoverProvider", Json::Bool(true)),
                        ("definitionProvider", Json::Bool(true)),
                        ("referencesProvider", Json::Bool(true)),
                        (
                            "renameProvider",
                            Json::obj([("prepareProvider", Json::Bool(true))]),
                        ),
                        (
                            "codeActionProvider",
                            Json::obj([(
                                "codeActionKinds",
                                Json::Arr(vec![Json::str("quickfix")]),
                            )]),
                        ),
                        (
                            "signatureHelpProvider",
                            Json::obj([(
                                "triggerCharacters",
                                Json::Arr(vec![Json::str("("), Json::str(",")]),
                            )]),
                        ),
                        (
                            "executeCommandProvider",
                            Json::obj([("commands", Json::Arr(vec![Json::str(actions::ROK_ADD)]))]),
                        ),
                        ("documentSymbolProvider", Json::Bool(true)),
                        ("documentFormattingProvider", Json::Bool(true)),
                        (
                            "completionProvider",
                            Json::obj([(
                                "triggerCharacters",
                                Json::Arr(vec![Json::str("."), Json::str(":")]),
                            )]),
                        ),
                        // Highlight what the static grammar cannot know:
                        // macro calls and user-defined literal suffixes.
                        (
                            "semanticTokensProvider",
                            Json::obj([
                                (
                                    "legend",
                                    Json::obj([
                                        (
                                            "tokenTypes",
                                            Json::Arr(vec![
                                                Json::str("macro"),
                                                Json::str("decorator"),
                                                Json::str("keyword"),
                                                Json::str("operator"),
                                            ]),
                                        ),
                                        ("tokenModifiers", Json::Arr(Vec::new())),
                                    ]),
                                ),
                                ("full", Json::Bool(true)),
                            ]),
                        ),
                    ]),
                ),
                (
                    "serverInfo",
                    Json::obj([
                        ("name", Json::str("rokls")),
                        ("version", Json::str(env!("CARGO_PKG_VERSION"))),
                    ]),
                ),
            ]),
            "shutdown" => {
                self.shutdown = true;
                Json::Null
            }
            "exit" => return Some(i32::from(!self.shutdown)),
            "textDocument/didOpen" | "textDocument/didChange" => {
                let text = if method == "textDocument/didOpen" {
                    params.get("textDocument").get("text")
                } else {
                    match params.get("contentChanges") {
                        Json::Arr(cs) => cs.last().map_or(&Json::Null, |c| c.get("text")),
                        _ => &Json::Null,
                    }
                };
                if let Some(text) = text.as_str() {
                    let path = uri_path(&uri);
                    let doc = self.docs.entry(uri).or_insert(Doc {
                        text: String::new(),
                        dirty: true,
                        inputs: vec![path.clone()],
                        good: None,
                        diags: Vec::new(),
                    });
                    text.clone_into(&mut doc.text);
                    doc.dirty = true;
                    self.invalidate(&path);
                }
                return None;
            }
            "textDocument/didClose" => {
                self.docs.remove(&uri);
                self.invalidate(&uri_path(&uri));
                self.publish(&uri, Vec::new());
                return None;
            }
            "textDocument/semanticTokens/full" => {
                let data = self
                    .docs
                    .get(&uri)
                    .map(|d| semantic_tokens(&d.text))
                    .unwrap_or_default();
                Json::obj([("data", Json::Arr(data.into_iter().map(Json::num).collect()))])
            }
            "workspace/executeCommand" => {
                let args: Vec<String> = match params.get("arguments") {
                    Json::Arr(a) => a
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect(),
                    _ => Vec::new(),
                };
                if params.get("command").as_str() != Some(actions::ROK_ADD) || args.is_empty() {
                    self.error(id, -32602, "unknown command".to_owned());
                    return None;
                }
                if let Err(why) = actions::rok_add(&args[0], &args[1..]) {
                    self.error(id, -32603, why);
                    return None;
                }
                // The manifest changed: every check may.
                for doc in self.docs.values_mut() {
                    doc.dirty = true;
                }
                Json::Null
            }
            "textDocument/codeAction" => {
                if self.docs.get(&uri).is_some_and(|d| d.dirty) {
                    self.refresh(&uri);
                }
                let found = self.docs.get(&uri).map(|d| {
                    let (ctx, at) = (params.get("context"), params.get("range"));
                    actions::code_actions(&uri, &d.text, &d.diags, ctx, at)
                });
                Json::Arr(found.unwrap_or_default())
            }
            "textDocument/hover"
            | "textDocument/definition"
            | "textDocument/documentSymbol"
            | "textDocument/completion"
            | "textDocument/signatureHelp"
            | "textDocument/references"
            | "textDocument/rename"
            | "textDocument/prepareRename"
            | "textDocument/formatting" => {
                if self.docs.get(&uri).is_some_and(|d| d.dirty) {
                    self.refresh(&uri);
                }
                let pos = params.get("position");
                let line = pos.get("line").as_u32().unwrap_or(0);
                let col = pos.get("character").as_u32().unwrap_or(0);
                let new_name = params.get("newName").as_str();
                let renamed = match method {
                    "textDocument/rename" => {
                        Some(self.rename(&uri, line, col, new_name.unwrap_or("")))
                    }
                    "textDocument/prepareRename" => Some(self.prepare_rename(&uri, line, col)),
                    _ => None,
                };
                if let Some(r) = renamed {
                    match r {
                        Ok(v) => v,
                        Err(why) => {
                            self.error(id, -32602, why);
                            return None;
                        }
                    }
                } else {
                    self.query(method, &uri, line, col).unwrap_or(Json::Null)
                }
            }
            _ if matches!(id, Json::Null) => return None,
            _ => {
                self.error(id, -32601, format!("unsupported method `{method}`"));
                return None;
            }
        };
        self.reply(id, result);
        None
    }

    /// An edited or closed buffer changes every check that read it.
    fn invalidate(&mut self, path: &Path) {
        for doc in self.docs.values_mut() {
            if doc.inputs.iter().any(|p| p == path) {
                doc.dirty = true;
            }
        }
    }

    fn session(&self, anonymous: bool) -> Session {
        let mut s = Session::new();
        for b in &self.bootstrap {
            s.add_bootstrap_file(b.clone());
        }
        for (n, p) in &self.externs {
            s.add_extern(n.clone(), p.clone());
        }
        s.set_anonymous(anonymous);
        // Submodule files as edited.
        for (uri, d) in &self.docs {
            s.add_buffer(uri_path(uri), d.text.clone());
        }
        s
    }

    /// Check `text` as buffer `path`: the package root (902.2) with this
    /// buffer as root or the root as open / on disk; files outside any
    /// package are anonymous. Open buffers stand for their files. With both
    /// root files (0.3.1), a file the library reaches is the library's, any
    /// other the executable's, which mounts the library (as edited) as
    /// dependency `<name>`.
    fn compile(&self, path: &Path, text: &str) -> Analysis {
        let Some(pkg) = rokc_interface::discover(path).ok().filter(|p| !p.anonymous) else {
            return analyze(self.session(true), text, path, &self.levels);
        };
        let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        if let Some(lib) = pkg.library_target().filter(|_| abs(&pkg.root) != path) {
            let a = self.compile_root(&abs(&lib.root), path, text, None);
            let key = path.display().to_string();
            if a.source_map.files().iter().any(|(p, _)| *p == key) {
                return a;
            }
        }
        let library = pkg.library.as_deref().map(|l| (pkg.name.as_str(), abs(l)));
        self.compile_root(&abs(&pkg.root), path, text, library)
    }

    /// Check root `root` for buffer `path`, with `library` (name and
    /// `src/lib.rok`) as a dependency.
    fn compile_root(
        &self,
        root: &Path,
        path: &Path,
        text: &str,
        library: Option<(&str, PathBuf)>,
    ) -> Analysis {
        let mut session = self.session(false);
        if let Some((name, lib)) = library {
            let name = rokc_interface::code_name(name);
            session.add_extern(name.clone(), lib);
            session.set_own_library(Some(name));
        }
        if root == path {
            return analyze(session, text, path, &self.levels);
        }
        // By path: the editor's URI spelling may differ from ours.
        let open = self
            .docs
            .iter()
            .find(|(uri, _)| uri_path(uri) == root)
            .map(|(_, d)| d.text.clone());
        let root_text = open
            .or_else(|| std::fs::read_to_string(root).ok())
            .unwrap_or_default();
        analyze(session, &root_text, root, &self.levels)
    }

    /// Recompile a buffer and publish its diagnostics.
    fn refresh(&mut self, uri: &str) {
        let Some(doc) = self.docs.get(uri) else {
            return;
        };
        let path = uri_path(uri);
        // A `.rokon` data file (`rok.rokon`, the package manifest) is one
        // ROKON value, not a module: only its syntax is checked here.
        if path.extension().is_some_and(|x| x == "rokon") {
            let diags = match rokc_rokon::parse(&doc.text) {
                Ok(_) => Vec::new(),
                Err(e) => {
                    let d = RokDiagnostic::error(Code::SYNTAX, e.span, &e.message);
                    vec![(diag_json(&d, uri, &doc.text), d)]
                }
            };
            let doc = self.docs.get_mut(uri).expect("checked above");
            doc.dirty = false;
            let json = diags.iter().map(|(j, _)| j.clone()).collect();
            doc.diags = diags;
            self.publish(uri, json);
            return;
        }
        let a = self.compile(&path, &doc.text);
        let key = path.display().to_string();
        let diags = a
            .diags
            .iter()
            .filter(|d| a.diag_path(d).is_none_or(|p| p == key))
            .map(|d| {
                let span = d.primary.as_ref().map(|l| l.span).unwrap_or_default();
                let j = diag_json(d, uri, a.file(span).map_or("", |(_, t)| t.as_str()));
                (j, d.clone())
            })
            .collect::<Vec<_>>();
        let doc = self.docs.get_mut(uri).expect("checked above");
        doc.dirty = false;
        let json = diags.iter().map(|(j, _)| j.clone()).collect();
        doc.diags = diags;
        if !a.source_map.files().is_empty() {
            doc.inputs = a
                .source_map
                .files()
                .iter()
                .map(|(p, _)| PathBuf::from(p))
                .collect();
        }
        if a.hir.is_some() {
            doc.good = Some(a);
        }
        self.publish(uri, json);
    }

    fn publish(&mut self, uri: &str, diags: Vec<Json>) {
        self.send(&Json::obj([
            ("jsonrpc", Json::str("2.0")),
            ("method", Json::str("textDocument/publishDiagnostics")),
            (
                "params",
                Json::obj([("uri", Json::str(uri)), ("diagnostics", Json::Arr(diags))]),
            ),
        ]));
    }

    /// `textDocument/rename`: every reference in the buffer (see
    /// `references`), when the name is declared in it.
    fn rename(&self, uri: &str, line: u32, col: u32, new_name: &str) -> Result<Json, String> {
        let ident = new_name.starts_with(|c: char| c == '_' || c.is_alphabetic())
            && new_name.chars().all(is_ident_char)
            && rokc_lexer::token::keyword_kind(new_name).is_none();
        if !ident {
            return Err(format!("`{new_name}` is not a valid name"));
        }
        let doc = self.docs.get(uri).ok_or("unknown document")?;
        let text = doc.text.as_str();
        let key = uri_path(uri).display().to_string();
        let off = offset(text, line, col);
        let refs = renamable(doc, &key, off)?.ok_or("nothing to rename here")?;
        let edits = refs
            .into_iter()
            .map(|(s, e)| {
                Json::obj([
                    ("range", range(text, s, e)),
                    ("newText", Json::str(new_name)),
                ])
            })
            .collect();
        Ok(Json::obj([(
            "changes",
            Json::obj([(uri, Json::Arr(edits))]),
        )]))
    }

    /// `textDocument/prepareRename`: the name under the cursor when
    /// `rename` would take it, else why not.
    fn prepare_rename(&self, uri: &str, line: u32, col: u32) -> Result<Json, String> {
        let doc = self.docs.get(uri).ok_or("unknown document")?;
        let text = doc.text.as_str();
        let key = uri_path(uri).display().to_string();
        let off = offset(text, line, col);
        if renamable(doc, &key, off)?.is_none() {
            return Ok(Json::Null);
        }
        let (name, start) = ident_at(text, off).expect("renamable found it");
        Ok(Json::obj([
            ("range", range(text, start, start + name.len())),
            ("placeholder", Json::str(name)),
        ]))
    }

    fn query(&self, method: &str, uri: &str, line: u32, col: u32) -> Option<Json> {
        let doc = self.docs.get(uri)?;
        let text = doc.text.as_str();
        let path = uri_path(uri);
        let key = path.display().to_string();
        let off = offset(text, line, col);
        match method {
            "textDocument/formatting" => {
                let new = rokc_fmt::format_file(&path, text).ok()?;
                Some(Json::Arr(if new == text {
                    Vec::new()
                } else {
                    vec![Json::obj([
                        ("range", range(text, 0, text.len())),
                        ("newText", Json::Str(new)),
                    ])]
                }))
            }
            "textDocument/documentSymbol" => {
                let program = parse_doc(text, &key);
                let (toks, _) = rokc_lexer::Lexer::new(text, key).lex_all();
                let shape = Shape { text, toks };
                Some(Json::Arr(
                    program
                        .items
                        .iter()
                        .filter_map(|i| shape.symbol(i))
                        .collect(),
                ))
            }
            "textDocument/hover" => {
                // On a `name!(..)` call, show its expansion in place.
                let value = macro_hover(text, off).or_else(|| {
                    let a = doc.good.as_ref()?;
                    hover(a, &key, text, off)
                })?;
                Some(Json::obj([(
                    "contents",
                    Json::obj([("kind", Json::str("markdown")), ("value", Json::str(value))]),
                )]))
            }
            "textDocument/definition" => {
                let (ident, start) = ident_at(text, off)?;
                let good = doc.good.as_ref();
                if let Some(m) = good.and_then(|a| member_at(a, &key, text, ident, start)) {
                    let (file, def_text) = good?.file(m.def_span())?;
                    let (from, to) = m.name_range(def_text)?;
                    return Some(location_at(&path_uri(Path::new(file)), def_text, from, to));
                }
                if let Some(m) = good.and_then(|a| indexed_member(a, &key, text, ident, start)) {
                    let (file, def_text) = good?.file(m.span)?;
                    let (from, to) = name_range(def_text, m.span.start as usize, ident)
                        .unwrap_or((m.span.start as usize, m.span.end as usize));
                    return Some(location_at(&path_uri(Path::new(file)), def_text, from, to));
                }
                // A local: resolve through the scopes of the buffer's AST.
                let program = parse_doc(text, &key);
                if let Some(span) = resolve_local(&program, off) {
                    let (from, to) = name_range(text, span.start as usize, ident)
                        .unwrap_or((span.start as usize, span.end as usize));
                    return Some(location_at(uri, text, from, to));
                }
                let good = good?;
                let span = definition(good, &key, ident, off)?;
                let (file, def_text) = good.file(span)?;
                let (from, to) = name_range(def_text, span.start as usize, ident)
                    .unwrap_or((span.start as usize, span.end as usize));
                Some(location_at(&path_uri(Path::new(file)), def_text, from, to))
            }
            "textDocument/references" => {
                let refs = references(doc, &key, off)?;
                Some(Json::Arr(
                    refs.into_iter()
                        .map(|(s, e)| location_at(uri, text, s, e))
                        .collect(),
                ))
            }
            "textDocument/signatureHelp" => {
                let compile = |t: &str| self.compile(&path, t);
                signature::signature_help(text, &key, off, compile, doc.good.as_ref())
            }
            "textDocument/completion" => {
                let compile = |t: &str| self.compile(&path, t);
                let good = doc.good.as_ref();
                let (_, found) = crate::complete::complete(text, &key, off, compile, good, &[]);
                Some(Json::Arr(
                    found
                        .into_iter()
                        .map(|(label, kind, detail)| {
                            Json::obj([
                                ("label", Json::Str(label)),
                                ("kind", Json::num(kind)),
                                ("detail", Json::Str(detail)),
                            ])
                        })
                        .collect(),
                ))
            }
            _ => None,
        }
    }
}

/// Hover markdown: the code in a `rok` fence, then a function's doc
/// text from its `#[doc]` attributes (spec 1.2).
pub(crate) fn hover(a: &Analysis, key: &str, text: &str, off: usize) -> Option<String> {
    let hir = a.hir.as_ref()?;
    let fenced = |code: String| format!("```rok\n{code}\n```");
    let with_doc = |f: &rokc_hir::HirFunction| {
        let doc = a.fn_doc(f.span);
        let code = fenced(signature(hir, f));
        if doc.is_empty() {
            code
        } else {
            format!("{code}\n\n{doc}")
        }
    };
    let ident = ident_at(text, off).map(|(s, _)| s);
    // `.field` / `.method`: the member, with its doc comment.
    if let Some((name, start)) = ident_at(text, off) {
        match member_at(a, key, text, name, start) {
            Some(Member::Method(f)) => return Some(with_doc(f)),
            Some(Member::Field(s, name)) => {
                let ty = s.fields.iter().find(|f| f.name == name)?;
                let owner = ty_name(hir, &HirTy::Struct(s.id));
                let code = fenced(format!("{owner}.{name}: {}", ty_name(hir, &ty.ty)));
                let doc = a.field_doc(s.span, name);
                return Some(if doc.is_empty() {
                    code
                } else {
                    format!("{code}\n\n{doc}")
                });
            }
            None => {
                if let Some(m) = indexed_member(a, key, text, name, start) {
                    let code = fenced(m.sig);
                    return Some(if m.doc.is_empty() {
                        code
                    } else {
                        format!("{code}\n\n{}", m.doc)
                    });
                }
            }
        }
    }
    // Offsets into an older compile of an edited buffer point elsewhere.
    let fresh = a
        .source_map
        .files()
        .iter()
        .any(|(p, t)| p == key && t == text);
    let at_off = |s: Span| (s.start as usize..s.end as usize).contains(&off);
    if let Some(e) = fresh.then(|| a.find_expr(key, at_off, false)).flatten() {
        if let HirExprKind::Call { callee, .. } | HirExprKind::FnRef(callee) = &e.kind {
            let f = hir.functions.iter().find(|f| f.id == *callee)?;
            if ident == Some(short_name(&f.name)) {
                return Some(with_doc(f));
            }
        }
        let ty = ty_name(hir, &e.ty);
        let src = text.get(e.span.start as usize..e.span.end as usize);
        return Some(fenced(match ident {
            Some(name) if src == Some(name) => format!("{name}: {ty}"),
            _ => ty,
        }));
    }
    // A binding site (`let x`, a parameter) has no expression: the
    // buffer's locals and items of that name.
    let name = ident?;
    let doc_fns = hir
        .functions
        .iter()
        .filter(|f| a.file(f.span).is_some_and(|(p, _)| p == key));
    for f in doc_fns {
        if short_name(&f.name) == name {
            return Some(with_doc(f));
        }
        if let Some(l) = f.locals.iter().find(|l| l.name.as_deref() == Some(name)) {
            return Some(fenced(format!("{name}: {}", ty_name(hir, &l.ty))));
        }
    }
    None
}

/// Where the item under the cursor is defined (functions, types).
fn definition(a: &Analysis, key: &str, ident: &str, off: usize) -> Option<Span> {
    let hir = a.hir.as_ref()?;
    let fn_span = |id| hir.functions.iter().find(|f| f.id == id).map(|f| f.span);
    let struct_span = |id| hir.structs.iter().find(|s| s.id == id).map(|s| s.span);
    let at = a.find_expr(
        key,
        |s| (s.start as usize..s.end as usize).contains(&off),
        false,
    );
    let hit =
        at.and_then(|e| match &e.kind {
            HirExprKind::Call { callee, .. } | HirExprKind::FnRef(callee) => fn_span(*callee)
                .filter(|_| {
                    hir.functions
                        .iter()
                        .any(|f| f.id == *callee && short_name(&f.name) == ident)
                }),
            HirExprKind::StructInit { struct_id, .. }
            | HirExprKind::FieldRead { struct_id, .. } => struct_span(*struct_id),
            HirExprKind::EnumVariantInit { enum_id, .. } => {
                hir.enums.iter().find(|e| e.id == *enum_id).map(|e| e.span)
            }
            _ => None,
        });
    let real = |s: &Span| !s.is_dummy() && a.file(*s).is_some();
    hit.filter(real).or_else(|| {
        // By name: functions, then types.
        let fns = hir
            .functions
            .iter()
            .filter(|f| short_name(&f.name) == ident)
            .map(|f| f.span);
        let types = hir
            .structs
            .iter()
            .filter(|s| short_name(&s.name) == ident)
            .map(|s| s.span)
            .chain(
                hir.enums
                    .iter()
                    .filter(|e| short_name(&e.name) == ident)
                    .map(|e| e.span),
            );
        fns.chain(types).filter(real).min_by_key(|s| {
            // Prefer the buffer itself.
            a.file(*s).is_none_or(|(p, _)| p != key)
        })
    })
}

/// Scope walk of the buffer's AST: the binding a one-segment path at
/// `off` refers to.
pub(crate) struct Locals {
    off: usize,
    scopes: Vec<Vec<(String, Span)>>,
    found: Option<Span>,
    /// Every binding: name and declaring span.
    binds: Vec<(String, Span)>,
    /// The names in scope at the innermost expression around `off`.
    around: Vec<String>,
    /// The names in scope at the first construct after `off`.
    after: Option<Vec<String>>,
}

impl Locals {
    fn in_scope(&self) -> Vec<String> {
        self.scopes
            .iter()
            .flatten()
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// A construct at `span`: snapshot the scope the first time one
    /// starts past `off`.
    fn reach(&mut self, span: Span) {
        if self.after.is_none() && span.start as usize >= self.off {
            self.after = Some(self.in_scope());
        }
    }

    /// The locals in scope at `off` (a statement the parser dropped
    /// still sees what precedes it).
    pub(crate) fn visible(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .around
            .iter()
            .chain(self.after.iter().flatten())
            .cloned()
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

impl AstVisitor for Locals {
    fn stmt(&mut self, s: &rokc_ast::Stmt) {
        self.reach(s.span);
    }
    fn expr(&mut self, e: &Expr) {
        self.reach(e.span);
        if (e.span.start as usize..=e.span.end as usize).contains(&self.off) {
            self.around = self.in_scope();
        }
        if let ExprKind::Path(p) = &e.kind {
            if let [seg] = p.segments.as_slice() {
                if (e.span.start as usize..=e.span.end as usize).contains(&self.off) {
                    let hit = self
                        .scopes
                        .iter()
                        .rev()
                        .flat_map(|s| s.iter().rev())
                        .find(|(n, _)| *n == seg.name);
                    if let Some((_, span)) = hit {
                        self.found = Some(*span);
                    }
                }
            }
        }
    }
    fn bind_at(&mut self, name: &str, at: Span) {
        self.reach(at);
        if let Some(s) = self.scopes.last_mut() {
            s.push((name.to_owned(), at));
        }
        self.binds.push((name.to_owned(), at));
    }
    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }
}

fn walk_items(v: &mut Locals, items: &[Item]) {
    for item in items {
        match item {
            Item::Mod(m) => walk_items(v, m.items.as_deref().unwrap_or(&[])),
            item => walk_item(v, item),
        }
    }
}

/// The scope walk of `program` for a path at `off`.
pub(crate) fn locals(program: &rokc_ast::Program, off: usize) -> Locals {
    let mut v = Locals {
        off,
        scopes: vec![Vec::new()],
        found: None,
        binds: Vec::new(),
        around: Vec::new(),
        after: None,
    };
    walk_items(&mut v, &program.items);
    program.stmts.iter().for_each(|s| walk_stmt(&mut v, s));
    // Nothing past `off`: the top-level scope at the end.
    if v.after.is_none() {
        v.after = Some(v.in_scope());
    }
    v
}

/// The declaring span of the local a one-segment path at `off` names.
fn resolve_local(program: &rokc_ast::Program, off: usize) -> Option<Span> {
    locals(program, off).found
}

/// The first whole-word `name` at or after byte `from` of `text`.
fn name_range(text: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let mut at = from;
    while let Some(i) = text.get(at..)?.find(name) {
        let (s, e) = (at + i, at + i + name.len());
        let before = text[..s].chars().next_back().is_some_and(is_ident_char);
        let after = text[e..].chars().next().is_some_and(is_ident_char);
        if !before && !after {
            return Some((s, e));
        }
        at = e;
    }
    None
}

/// `.name` (at byte `start` of `text`) by the receiver's type: its
/// member in the index, called or not.
fn indexed_member(
    a: &Analysis,
    key: &str,
    text: &str,
    name: &str,
    start: usize,
) -> Option<crate::members::Member> {
    let dot = start.checked_sub(1)?;
    if text.as_bytes().get(dot) != Some(&b'.') {
        return None;
    }
    let recv = receiver_start(text, dot);
    let e = a.find_expr(
        key,
        |s| s.start as usize == recv && s.end as usize <= dot,
        true,
    )?;
    let ty = ty_name(a.hir.as_ref()?, &e.ty);
    let found = crate::members::Members::of(a)
        .get(&ty)
        .find(|m| m.name == name)
        .cloned();
    found
}

/// What `.name` names: a struct field or a method.
enum Member<'a> {
    Field(&'a rokc_hir::HirStructDef, &'a str),
    Method(&'a rokc_hir::HirFunction),
}

impl Member<'_> {
    const fn def_span(&self) -> Span {
        match self {
            Self::Field(s, _) => s.span,
            Self::Method(f) => f.span,
        }
    }

    /// The name at the definition in its file's `text`.
    fn name_range(&self, text: &str) -> Option<(usize, usize)> {
        match self {
            // The field's `name:` inside the struct's braces (not a
            // mention in a doc comment).
            Self::Field(s, name) => {
                let start = s.span.start as usize;
                let mut at = start + text.get(start..)?.find('{')?;
                loop {
                    let (s, e) = name_range(text, at, name)?;
                    if text[e..].trim_start().starts_with(':') {
                        return Some((s, e));
                    }
                    at = e;
                }
            }
            Self::Method(f) => name_range(text, f.span.start as usize, short_name(&f.name)),
        }
    }
}

/// The member `name` (at byte `start` of `text`, after a `.`) of the
/// receiver before it, from the typed expressions of `a`.
fn member_at<'a>(
    a: &'a Analysis,
    key: &str,
    text: &str,
    name: &str,
    start: usize,
) -> Option<Member<'a>> {
    let dot = start.checked_sub(1)?;
    if text.as_bytes().get(dot) != Some(&b'.') {
        return None;
    }
    let receiver = receiver_start(text, dot);
    let hir = a.hir.as_ref()?;
    let mut found = None;
    for f in &hir.functions {
        let Some(body) = &f.body else { continue };
        crate::analysis::each_block(body, &mut |e| {
            if found.is_some() || e.span.start as usize != receiver || !a.in_file(e.span, key) {
                return;
            }
            match &e.kind {
                HirExprKind::FieldRead {
                    struct_id,
                    field_index,
                    ..
                } => {
                    let s = hir.structs.iter().find(|s| s.id == *struct_id);
                    let field = s.and_then(|s| s.fields.get(*field_index as usize));
                    if let (Some(s), Some(fd)) = (s, field) {
                        if fd.name == name {
                            found = Some(Member::Field(s, fd.name.as_str()));
                        }
                    }
                }
                HirExprKind::Call { callee, .. } => {
                    found = hir
                        .functions
                        .iter()
                        .find(|g| {
                            g.id == *callee
                                && short_name(&g.name) == name
                                && g.params.first().is_some_and(|p| p.name == "self")
                        })
                        .map(Member::Method);
                }
                _ => {}
            }
        });
        if found.is_some() {
            break;
        }
    }
    found
}

/// The `name:` of every struct literal field `name` in the buffer.
struct FieldInits<'t> {
    /// The struct's name as written in a literal.
    owner: &'t str,
    name: &'t str,
    text: &'t str,
    out: Vec<(usize, usize)>,
}

impl AstVisitor for FieldInits<'_> {
    fn expr(&mut self, e: &Expr) {
        let ExprKind::StructInit { path, fields, .. } = &e.kind else {
            return;
        };
        if path.segments.last().is_none_or(|s| s.name != self.owner) {
            return;
        }
        for (n, value) in fields.iter().filter(|(n, _)| n == self.name) {
            // The name is the last one before the value (`x: v`), or the
            // value itself (`P { x }`).
            let end = (value.span.end as usize).min(self.text.len());
            let head = &self.text[..end];
            if let Some(s) = head.rfind(n.as_str()) {
                self.out.push((s, s + n.len()));
            }
        }
    }
}

/// Every reference to what the identifier at `off` names, its declaration
/// included, as byte ranges: a local through the buffer's scopes, a
/// `.member` every same-named member, anything else every same-named
/// non-local identifier.
fn references(doc: &Doc, key: &str, off: usize) -> Option<Vec<(usize, usize)>> {
    let text = doc.text.as_str();
    let (name, start) = ident_at(text, off)?;
    let (tokens, _) = rokc_lexer::Lexer::new(text, key.to_owned()).lex_all();
    let words: Vec<(usize, usize)> = tokens
        .iter()
        .filter(|t| matches!(&t.kind, rokc_lexer::TokenKind::Ident(n) if n == name))
        .map(|t| (t.span.start as usize, t.span.end as usize))
        .collect();
    let is_member = |s: usize| s > 0 && text.as_bytes()[s - 1] == b'.';
    let program = parse_doc(text, key);
    let walk = locals(&program, usize::MAX);
    // A binding's name, mapped to its declaring span.
    let binding = |s: usize| {
        walk.binds.iter().find_map(|(n, at)| {
            (n == name && name_range(text, at.start as usize, name).is_some_and(|r| r.0 == s))
                .then_some(*at)
        })
    };
    let resolve = |s: usize| binding(s).or_else(|| resolve_local(&program, s));
    let mut out: Vec<(usize, usize)> = if is_member(start) {
        let a = doc.good.as_ref()?;
        let m = member_at(a, key, text, name, start)?;
        // `.name` of the same field / method.
        let same = |w: &(usize, usize)| {
            is_member(w.0)
                && member_at(a, key, text, name, w.0).is_some_and(|o| o.def_span() == m.def_span())
        };
        let mut v: Vec<_> = words.iter().copied().filter(same).collect();
        let here = a.file(m.def_span()).is_some_and(|(p, _)| p == key);
        if let Some(r) = m.name_range(text).filter(|_| here) {
            v.push(r);
        }
        if let Member::Field(s, _) = m {
            let mut inits = FieldInits {
                owner: short_name(s.name.split("__").next().unwrap_or(&s.name)),
                name,
                text,
                out: Vec::new(),
            };
            for item in &program.items {
                walk_item(&mut inits, item);
            }
            program.stmts.iter().for_each(|s| walk_stmt(&mut inits, s));
            v.extend(inits.out);
        }
        v
    } else if let Some(target) = resolve(start) {
        words
            .iter()
            .copied()
            .filter(|w| resolve(w.0) == Some(target))
            .collect()
    } else {
        words
            .iter()
            .copied()
            .filter(|w| !is_member(w.0) && resolve(w.0).is_none())
            .collect()
    };
    out.sort_unstable();
    out.dedup();
    (!out.is_empty()).then_some(out)
}

/// Is what the identifier at `off` names declared in this buffer?
fn defined_here(doc: &Doc, key: &str, off: usize) -> bool {
    let text = doc.text.as_str();
    let Some((name, start)) = ident_at(text, off) else {
        return false;
    };
    let a = doc.good.as_ref();
    if let Some(m) = a.and_then(|a| member_at(a, key, text, name, start)) {
        return a
            .and_then(|a| a.file(m.def_span()))
            .is_some_and(|(p, _)| p == key);
    }
    let program = parse_doc(text, key);
    let walk = locals(&program, off);
    if walk.found.is_some()
        || walk.binds.iter().any(|(n, at)| {
            n == name && name_range(text, at.start as usize, name).is_some_and(|r| r.0 == start)
        })
    {
        return true;
    }
    a.and_then(|a| {
        let span = definition(a, key, name, off)?;
        Some(a.file(span)?.0 == key)
    })
    .unwrap_or(false)
}

/// The references `rename` edits for the identifier at `off`: `None`
/// with nothing there, an error naming where a name declared outside
/// the buffer comes from.
fn renamable(doc: &Doc, key: &str, off: usize) -> Result<Option<Vec<(usize, usize)>>, String> {
    let text = doc.text.as_str();
    let Some((name, start)) = ident_at(text, off) else {
        return Ok(None);
    };
    if rokc_lexer::token::keyword_kind(name).is_some() {
        return Err(format!("`{name}` is a keyword"));
    }
    let Some(refs) = references(doc, key, off) else {
        return Ok(None);
    };
    if defined_here(doc, key, off) {
        return Ok(Some(refs));
    }
    let a = doc.good.as_ref();
    let span = a.and_then(|a| {
        member_at(a, key, text, name, start)
            .map(|m| m.def_span())
            .or_else(|| indexed_member(a, key, text, name, start).map(|m| m.span))
            .or_else(|| definition(a, key, name, off))
    });
    let source = span.and_then(|s| a?.sources.iter().find(|f| f.file_id == s.file));
    let why = match (span, source) {
        (None, _) => "nothing in this file declares it".to_owned(),
        (Some(_), Some(f)) if f.package.is_empty() => {
            "it is declared in another file; rename it there".to_owned()
        }
        (Some(_), Some(f)) if f.package != "rok" && !f.package.starts_with("rok::") => {
            format!("it is defined in dependency `{}`", f.package)
        }
        _ => "it is defined in the standard library".to_owned(),
    };
    Err(format!("cannot rename `{name}`: {why}"))
}

/// LSP `Diagnostic` of `d`, whose span is in `text`.
fn diag_json(d: &RokDiagnostic, uri: &str, text: &str) -> Json {
    let span = d.primary.as_ref().map(|l| l.span).unwrap_or_default();
    let mut message = d.message.clone();
    for n in &d.notes {
        message.push('\n');
        if !n.starts_with("help: ") {
            message.push_str("note: ");
        }
        message.push_str(n);
    }
    // Labels in the same file (`first defined here`, an unclosed
    // delimiter) as related locations.
    let related: Vec<Json> = d
        .secondary
        .iter()
        .filter(|l| l.span.file == span.file && !l.span.is_dummy())
        .map(|l| {
            Json::obj([
                (
                    "location",
                    location_at(uri, text, l.span.start as usize, l.span.end as usize),
                ),
                ("message", Json::str(&l.message)),
            ])
        })
        .collect();
    let (start, end) = (span.start as usize, span.end as usize);
    // Some editors hide an empty range.
    let (start, end) = if start == end && !span.is_dummy() {
        widen(text, start)
    } else {
        (start, end)
    };
    let mut d_json = Json::obj([
        ("range", range(text, start, end)),
        ("severity", Json::num(if d.is_error() { 1 } else { 2 })),
        ("code", Json::str(d.code.to_string())),
        ("source", Json::str("rokc")),
        ("message", Json::str(message)),
        ("relatedInformation", Json::Arr(related)),
    ]);
    // `DiagnosticTag.Deprecated`: editors strike the use through.
    if let (Json::Obj(fields), rokc_diagnostic::Tag::Deprecated) = (&mut d_json, d.tag) {
        fields.push(("tags".to_owned(), Json::Arr(vec![Json::num(2)])));
    }
    d_json
}

/// The token a zero-width range at `at` points at: the next one on its
/// line, else the one before it on the line, else one character.
fn widen(text: &str, at: usize) -> (usize, usize) {
    let at = at.min(text.len());
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    let (toks, _) = rokc_lexer::Lexer::new(text, String::new()).lex_all();
    let spans = toks
        .iter()
        .map(|t| (t.span.start as usize, t.span.end as usize))
        .filter(|&(s, e)| s < e && e <= text.len() && !text[s..e].trim().is_empty());
    let mut before = None;
    for (s, e) in spans {
        if s >= at && s < line_end {
            return (s, e);
        }
        if e <= at && s >= line_start {
            before = Some((s, e));
        }
    }
    before.unwrap_or_else(|| match text[at..].chars().next() {
        Some(c) if c != '\n' => (at, at + c.len_utf8()),
        _ => (at, at),
    })
}

/// The buffer alone, parsed for its shape (no library operators).
pub(crate) fn parse_doc(text: &str, file: &str) -> rokc_ast::Program {
    let (tokens, _) = rokc_lexer::Lexer::new(text, file.to_owned()).lex_all();
    rokc_parser::parse_program(&tokens, file).0
}

/// The buffer's tokens, for item extents.
struct Shape<'t> {
    text: &'t str,
    toks: Vec<rokc_lexer::Token>,
}

impl Shape<'_> {
    /// The item whose keyword is at `start`: from its modifiers to its
    /// closing `}` or `;`.
    fn extent(&self, start: usize) -> (usize, usize) {
        use rokc_lexer::TokenKind as K;
        let mut from = start;
        loop {
            let head = self.text[..from].trim_end();
            let Some(m) = ["pub(pkg)", "pub", "const", "unsafe", "async", "extern"]
                .into_iter()
                .find(|m| {
                    head.ends_with(m)
                        && !head[..head.len() - m.len()]
                            .chars()
                            .next_back()
                            .is_some_and(is_ident_char)
                })
            else {
                break;
            };
            from = head.len() - m.len();
        }
        let first = self
            .toks
            .partition_point(|t| (t.span.start as usize) < start);
        let mut depth = 0u32;
        for (i, t) in self.toks.iter().enumerate().skip(first) {
            match &t.kind {
                K::LParen | K::LBracket => depth += 1,
                K::RParen | K::RBracket => depth = depth.saturating_sub(1),
                K::LBrace if depth == 0 => {
                    let end =
                        balanced_close(&self.toks, i).map_or(t.span.end, |c| self.toks[c].span.end);
                    return (from, end as usize);
                }
                K::Semi if depth == 0 => return (from, t.span.end as usize),
                _ => {}
            }
        }
        (from, self.text.len())
    }

    /// LSP `DocumentSymbol` of an item.
    fn symbol(&self, item: &Item) -> Option<Json> {
        let fn_sym = |f: &rokc_ast::FnDef, kind: u32| self.sym(&f.name, kind, f.span, Vec::new());
        let (name, kind, children): (String, u32, Vec<Json>) = match item {
            Item::Fn(f) => return Some(fn_sym(f, 12)),
            Item::Struct(s) => (s.name.clone(), 23, Vec::new()),
            Item::Union(u) => (u.name.clone(), 23, Vec::new()),
            Item::Enum(e) => (e.name.clone(), 10, Vec::new()),
            Item::Trait(t) => (
                t.name.clone(),
                11,
                t.items
                    .iter()
                    .filter_map(|i| match i {
                        TraitItem::Method(f) => Some(fn_sym(f, 6)),
                        _ => None,
                    })
                    .collect(),
            ),
            Item::Impl(i) => {
                // `impl Tr for T` as written, up to the body. A derive's
                // generated impl (its span is the attribute's) is no symbol.
                let head = self.text.get(i.span.start as usize..)?;
                if !(head.starts_with("impl") || head.starts_with("unsafe")) {
                    return None;
                }
                let head = head.split('{').next().unwrap_or(head);
                (
                    head.split_whitespace().collect::<Vec<_>>().join(" "),
                    5,
                    i.items
                        .iter()
                        .filter_map(|i| match i {
                            ImplItem::Method(f) => Some(fn_sym(f, 6)),
                            _ => None,
                        })
                        .collect(),
                )
            }
            Item::Const(c) => (c.name.clone(), 14, Vec::new()),
            Item::Static(s) => (s.name.clone(), 13, Vec::new()),
            Item::TypeAlias(t) => (t.name.clone(), 26, Vec::new()),
            Item::Mod(m) => (
                m.name.clone(),
                2,
                m.items
                    .iter()
                    .flatten()
                    .filter_map(|i| self.symbol(i))
                    .collect(),
            ),
            Item::Use(_) | Item::Extern(_) => return None,
        };
        Some(self.sym(&name, kind, item.span(), children))
    }

    /// A symbol whose keyword is at `span`: `range` covers the item,
    /// `selectionRange` its name (an impl's head).
    fn sym(&self, name: &str, kind: u32, span: Span, children: Vec<Json>) -> Json {
        let start = span.start as usize;
        let (from, to) = self.extent(start);
        let (s, e) = if kind == 5 {
            let head = self.text[start..to].split('{').next().unwrap_or_default();
            (start, start + head.trim_end().len())
        } else {
            name_range(self.text, start, name)
                .filter(|r| r.1 <= to)
                .unwrap_or((start, span.end as usize))
        };
        Json::obj([
            ("name", Json::str(name)),
            ("kind", Json::num(kind)),
            ("range", range(self.text, from, to.max(e))),
            ("selectionRange", range(self.text, s, e)),
            ("children", Json::Arr(children)),
        ])
    }
}

fn location_at(uri: &str, text: &str, start: usize, end: usize) -> Json {
    Json::obj([("uri", Json::str(uri)), ("range", range(text, start, end))])
}

pub(crate) fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Where the receiver of the `.` at `dot` starts: back over a chain of
/// identifiers, `.`, `::` and bracketed groups (`a.b(x)[0].`).
pub(crate) fn receiver_start(text: &str, dot: usize) -> usize {
    let b = text.as_bytes();
    let (mut i, mut depth) = (dot, 0u32);
    while i > 0 {
        let c = b[i - 1];
        match c {
            b')' | b']' => depth += 1,
            b'(' | b'[' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            _ if c.is_ascii_alphanumeric() || c >= 0x80 || b"_.:".contains(&c) => {}
            _ => break,
        }
        i -= 1;
    }
    i
}

/// The identifier around byte `off` and its start.
fn ident_at(text: &str, off: usize) -> Option<(&str, usize)> {
    let off = off.min(text.len());
    let start = text[..off]
        .rfind(|c: char| !is_ident_char(c))
        .map_or(0, |i| {
            i + text[i..].chars().next().map_or(1, char::len_utf8)
        });
    let end = text[off..]
        .find(|c: char| !is_ident_char(c))
        .map_or(text.len(), |i| off + i);
    let s = &text[start..end];
    (!s.is_empty() && !s.starts_with(|c: char| c.is_ascii_digit())).then_some((s, start))
}

/// Byte offset of an LSP position (UTF-16 columns).
fn offset(text: &str, line: u32, col: u32) -> usize {
    let start = if line == 0 {
        0
    } else {
        text.match_indices('\n')
            .nth(line as usize - 1)
            .map_or(text.len(), |(i, _)| i + 1)
    };
    let mut units = 0;
    for (i, c) in text[start..].char_indices() {
        if units >= col as usize || c == '\n' {
            return start + i;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// LSP position of a byte offset.
/// If `off` is inside a `name!(..)` macro call whose macro is defined in
/// this file, its expansion rendered as markdown (spec 6.8.12 tooling:
/// "expand in place", surfaced through hover).
fn macro_hover(text: &str, off: usize) -> Option<String> {
    use rokc_lexer::{Lexer, TokenKind};
    use rokc_parser::MetaKind;
    let (toks, _) = Lexer::new(text, "").lex_all();
    for (i, t) in toks.iter().enumerate() {
        let TokenKind::Ident(name) = &t.kind else {
            continue;
        };
        if toks.get(i + 1).map(|x| &x.kind) != Some(&TokenKind::Bang) {
            continue;
        }
        let open = i + 2;
        if !matches!(
            toks.get(open).map(|x| &x.kind),
            Some(TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace)
        ) {
            continue;
        }
        let Some(close) = balanced_close(&toks, open) else {
            continue;
        };
        let (lo, hi) = (t.span.start as usize, toks[close].span.end as usize);
        if off < lo || off > hi {
            continue;
        }
        let def = rokc_parser::collect_meta_blocks(&toks, "")
            .into_iter()
            .find(|d| d.kind == MetaKind::Macro && &d.name == name)?;
        let call = toks[open + 1..close].to_vec();
        let expanded = rokc_parser::expand_to_text(&def, &call, t.span)?;
        return Some(format!("```rok\n{expanded}\n```"));
    }
    None
}

/// The matching close of a bracket at `open` (`#[` / `#![` count as opens).
fn balanced_close(toks: &[rokc_lexer::Token], open: usize) -> Option<usize> {
    use rokc_lexer::TokenKind as K;
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match &t.kind {
            K::LParen | K::LBracket | K::LBrace | K::HashLBracket | K::HashBangLBracket => {
                depth += 1;
            }
            K::RParen | K::RBracket | K::RBrace => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// 0-based line and UTF-16 character of a byte offset.
fn line_col(text: &str, off: usize) -> (u32, u32) {
    let off = off.min(text.len());
    let before = &text[..off];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col: u32 = before[line_start..]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum();
    (line, col)
}

/// LSP semantic tokens (delta-encoded) for what the grammar cannot know:
/// macro calls `name!(..)` (type `macro`) and user-defined literal
/// suffixes `123suffix` (type `decorator`) - spec 6.5 / 6.8.12.
fn semantic_tokens(text: &str) -> Vec<u32> {
    use rokc_lexer::{Lexer, TokenKind};
    let (raw, _) = Lexer::new(text, "").lex_all();
    // User operator symbols from `#[infix/prefix/postfix("X")]`; re-lex with
    // them so their uses tokenize as extension operators.
    let ops: Vec<String> = raw
        .windows(4)
        .filter_map(|w| {
            let (
                TokenKind::HashLBracket,
                TokenKind::Ident(a),
                TokenKind::LParen,
                TokenKind::StrLit(s),
            ) = (&w[0].kind, &w[1].kind, &w[2].kind, &w[3].kind)
            else {
                return None;
            };
            matches!(a.as_str(), "infix" | "prefix" | "postfix").then(|| s.clone())
        })
        .collect();
    let (toks, _) = Lexer::new(text, "").with_ext_ops(ops).lex_all();
    // User-defined literal suffixes registered in this file by
    // `#[literal(number "x")]` - only these are highlighted (spec 6.5), so
    // `5kg` lights up when the user defined `kg`, not for any stray suffix.
    let suffixes: std::collections::HashSet<String> = rokc_parser::collect_literals(&toks, "")
        .iter()
        .filter_map(|d| match d.tokens.as_slice() {
            [a, b] if matches!(&a.kind, TokenKind::Ident(n) if n == "number") => match &b.kind {
                TokenKind::StrLit(s) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    // User-defined keywords (#[keyword]) and derives / attributes (meta
    // blocks); the macro / operator / literal forms are recognised from the
    // token stream directly.
    let keywords: std::collections::HashSet<String> =
        rokc_parser::collect_keywords(&parse_doc(text, "").items, "")
            .into_iter()
            .map(|k| k.name)
            .collect();
    let metas = rokc_parser::collect_meta_blocks(&toks, "");
    let is_kind =
        |want: rokc_parser::MetaKind, n: &str| metas.iter().any(|d| d.kind == want && d.name == n);
    // Token type indices in the legend: macro, decorator, keyword, operator.
    let mut spans: Vec<(usize, usize, u32)> = Vec::new();
    let span =
        |t: &rokc_lexer::Token| (t.span.start as usize, (t.span.end - t.span.start) as usize);
    for (i, t) in toks.iter().enumerate() {
        match &t.kind {
            // A macro call `name!( .. )`.
            TokenKind::Ident(_)
                if toks.get(i + 1).map(|x| &x.kind) == Some(&TokenKind::Bang)
                    && matches!(
                        toks.get(i + 2).map(|x| &x.kind),
                        Some(TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace)
                    ) =>
            {
                let start = t.span.start as usize;
                let end = toks[i + 1].span.end as usize; // through the `!`
                spans.push((start, end - start, 0));
            }
            // A user-defined keyword (#[keyword(..)]).
            TokenKind::Ident(n) if keywords.contains(n) => {
                let (o, l) = span(t);
                spans.push((o, l, 2));
            }
            // A user-defined literal suffix (#[literal(number "x")]).
            TokenKind::IntLit(s) | TokenKind::FloatLit(s) => {
                if let Some(sfx) = rokc_parser::user_suffix(s) {
                    if suffixes.contains(sfx) {
                        let end = t.span.end as usize;
                        spans.push((end - sfx.len(), sfx.len(), 1));
                    }
                }
            }
            // A user-defined operator (#[infix] / #[prefix] / #[postfix]):
            // an extension operator token is user-defined by construction.
            TokenKind::ExtOp(_) => {
                let (o, l) = span(t);
                spans.push((o, l, 3));
            }
            // `#[derive(X, ..)]` / `#[attr]`: light up user-defined names.
            TokenKind::HashLBracket | TokenKind::HashBangLBracket => {
                if let Some(close) = balanced_close(&toks, i) {
                    if let Some(TokenKind::Ident(n)) = toks.get(i + 1).map(|x| &x.kind) {
                        if is_kind(rokc_parser::MetaKind::Attribute, n) {
                            let (o, l) = span(&toks[i + 1]);
                            spans.push((o, l, 1));
                        }
                    }
                    for x in &toks[i + 1..close] {
                        if let TokenKind::Ident(n) = &x.kind {
                            if is_kind(rokc_parser::MetaKind::Derive, n) {
                                let (o, l) = span(x);
                                spans.push((o, l, 1));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    spans.sort_by_key(|s| s.0);
    spans.dedup();
    let mut data = Vec::with_capacity(spans.len() * 5);
    let (mut pl, mut pc) = (0u32, 0u32);
    for (off, len, ty) in spans {
        let (l, c) = line_col(text, off);
        let (dl, dc) = (l - pl, if l == pl { c - pc } else { c });
        let len16 = text[off..off + len]
            .chars()
            .map(|c| c.len_utf16() as u32)
            .sum();
        data.extend([dl, dc, len16, ty, 0]);
        (pl, pc) = (l, c);
    }
    data
}

fn position(text: &str, off: usize) -> Json {
    let mut off = off.min(text.len());
    while !text.is_char_boundary(off) {
        off -= 1;
    }
    let before = &text[..off];
    let line = before.matches('\n').count();
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col: usize = before[line_start..].chars().map(char::len_utf16).sum();
    Json::obj([
        ("line", Json::num(line as u32)),
        ("character", Json::num(col as u32)),
    ])
}

fn range(text: &str, start: usize, end: usize) -> Json {
    Json::obj([
        ("start", position(text, start)),
        ("end", position(text, end.max(start))),
    ])
}

fn uri_path(uri: &str) -> PathBuf {
    PathBuf::from(uri_to_path(uri, cfg!(windows)))
}

fn path_uri(path: &Path) -> String {
    path_to_uri(&path.display().to_string(), cfg!(windows))
}

/// A `file://` URI's path; on Windows `file:///c%3A/a/b` is `C:\a\b` (the
/// drive as rokc spells it) and `file://host/share/x` is `\\host\share\x`.
fn uri_to_path(uri: &str, windows: bool) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_owned();
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    let path = String::from_utf8_lossy(&out).into_owned();
    if !windows {
        return path;
    }
    let path = match path.as_bytes() {
        [b'/', d, b':', ..] if d.is_ascii_alphabetic() => {
            format!("{}{}", d.to_ascii_uppercase() as char, &path[2..])
        }
        [b'/', ..] => path,
        // An authority: a UNC share.
        _ => format!("//{path}"),
    };
    path.replace('/', "\\")
}

/// The `file://` URI of a path (Windows: `C:\a b\x` is
/// `file:///C:/a%20b/x`).
pub fn path_to_uri(path: &str, windows: bool) -> String {
    let path = if windows {
        let p = path.replace('\\', "/");
        match p.strip_prefix("//") {
            Some(unc) => unc.to_owned(),
            None => format!("/{p}"),
        }
    } else {
        path.to_owned()
    };
    let mut out = String::from("file://");
    for (i, b) in path.bytes().enumerate() {
        let drive_colon = windows && i == 2 && b == b':';
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) || drive_colon {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_round_trip_utf16() {
        let text = "ab\né😀x\n";
        let x = text.find('x').unwrap();
        assert_eq!(
            position(text, x),
            Json::obj([("line", Json::num(1)), ("character", Json::num(3))])
        );
        assert_eq!(offset(text, 1, 3), x);
        assert_eq!(offset(text, 9, 0), text.len());
        assert_eq!(ident_at("let foo = 1", 5), Some(("foo", 4)));
        assert_eq!(ident_at("a + 12", 5), None);
        let s = "x = 1 + a.b(f(c))[0].";
        assert_eq!(receiver_start(s, s.len() - 1), s.find('a').unwrap());
        let p = "/tmp/a b/é.rok";
        assert_eq!(path_to_uri(p, false), "file:///tmp/a%20b/%C3%A9.rok");
        assert_eq!(uri_to_path(&path_to_uri(p, false), false), p);
    }

    #[test]
    fn windows_uris_name_drives_and_shares() {
        let p = r"C:\Users\é x\main.rok";
        assert_eq!(path_to_uri(p, true), "file:///C:/Users/%C3%A9%20x/main.rok");
        assert_eq!(uri_to_path(&path_to_uri(p, true), true), p);
        // VS Code's spelling.
        assert_eq!(
            uri_to_path("file:///c%3A/Users/x/main.rok", true),
            r"C:\Users\x\main.rok"
        );
        let unc = r"\\srv\share\a.rok";
        assert_eq!(path_to_uri(unc, true), "file://srv/share/a.rok");
        assert_eq!(uri_to_path("file://srv/share/a.rok", true), unc);
    }
}
