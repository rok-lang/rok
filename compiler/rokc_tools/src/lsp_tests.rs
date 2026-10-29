//! The language server driven by a scripted JSON-RPC session (front end
//! only: no LLVM toolchain needed).

use std::io::Cursor;
use std::path::Path;

use crate::lsp::{path_to_uri, serve};
use rokc_diagnostic::Json;

fn frame(msg: &Json) -> String {
    let body = msg.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

fn request(id: u32, method: &str, params: Json) -> Json {
    Json::obj([
        ("jsonrpc", Json::str("2.0")),
        ("id", Json::num(id)),
        ("method", Json::str(method)),
        ("params", params),
    ])
}

fn notify(method: &str, params: Json) -> Json {
    Json::obj([
        ("jsonrpc", Json::str("2.0")),
        ("method", Json::str(method)),
        ("params", params),
    ])
}

fn doc(uri: &str) -> Json {
    Json::obj([("uri", Json::str(uri))])
}

fn at(uri: &str, line: u32, character: u32) -> Json {
    Json::obj([
        ("textDocument", doc(uri)),
        (
            "position",
            Json::obj([
                ("line", Json::num(line)),
                ("character", Json::num(character)),
            ]),
        ),
    ])
}

fn change(uri: &str, text: &str) -> Json {
    notify(
        "textDocument/didChange",
        Json::obj([
            ("textDocument", doc(uri)),
            (
                "contentChanges",
                Json::Arr(vec![Json::obj([("text", Json::str(text))])]),
            ),
        ]),
    )
}

/// Every framed message of the server's output.
fn messages(out: &[u8]) -> Vec<Json> {
    let mut rest = std::str::from_utf8(out).unwrap();
    let mut msgs = Vec::new();
    while let Some(i) = rest.find("\r\n\r\n") {
        let len: usize = rest[..i]
            .trim_start_matches("Content-Length: ")
            .parse()
            .unwrap();
        msgs.push(Json::parse(&rest[i + 4..i + 4 + len]).unwrap());
        rest = &rest[i + 4 + len..];
    }
    msgs
}

const SRC: &str = "\
#[derive(Debug)]
struct P { x: i32; y: i32; }
impl P {
    fn sum(self) -> i32 { self.x + self.y } }
/// Doubles `n`.
fn twice(n: i32) -> i32 { n * 2 }
let p = P { x: 1, y: 2 };
let s = p.sum();
println(f\"{twice(s)}\");
";

#[test]
fn scripted_session() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bootstrap = workspace.join("library/core/src/lib.rok");
    let dir = rokc_core::tmp::test_dir("lsp");
    std::fs::create_dir_all(&dir).unwrap();
    // A lone file: the anonymous package.
    let file = dir.join("demo.rok");
    let uri = path_to_uri(&file.display().to_string(), cfg!(windows));
    let uri = uri.as_str();

    let completing = format!("{SRC}let t = p.\n");
    let paths = format!("{SRC}rok::core::\n");
    let broken = SRC.replace("n * 2", "n * true");
    let unformatted = "fn f() {\nlet a = 1;\n}\n";
    let script = [
        request(
            1,
            "initialize",
            Json::obj([("capabilities", Json::obj([]))]),
        ),
        notify("initialized", Json::obj([])),
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([
                    ("uri", Json::str(uri)),
                    ("languageId", Json::str("rok")),
                    ("version", Json::num(1)),
                    ("text", Json::str(SRC)),
                ]),
            )]),
        ),
        // `p` in `p.sum()`, `twice` in the call, `s` in `twice(s)`.
        request(2, "textDocument/hover", at(uri, 7, 8)),
        request(3, "textDocument/hover", at(uri, 8, 12)),
        request(4, "textDocument/definition", at(uri, 8, 12)),
        request(5, "textDocument/definition", at(uri, 8, 18)),
        request(
            6,
            "textDocument/documentSymbol",
            Json::obj([("textDocument", doc(uri))]),
        ),
        change(uri, &broken),
        request(10, "textDocument/hover", at(uri, 7, 8)),
        change(uri, &completing),
        request(7, "textDocument/completion", at(uri, 9, 10)),
        change(uri, &paths),
        request(8, "textDocument/completion", at(uri, 9, 11)),
        change(uri, unformatted),
        request(
            9,
            "textDocument/formatting",
            Json::obj([("textDocument", doc(uri))]),
        ),
        request(11, "workspace/bogus", Json::obj([])),
        request(12, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        vec![bootstrap],
        Vec::new(),
        vec![(crate::lint::Level::Allow, "dead_code".to_owned())],
    );
    assert_eq!(code, 0);
    let msgs = messages(&out);
    let result = |id: u32| {
        msgs.iter()
            .find(|m| m.get("id") == &Json::num(id))
            .unwrap_or_else(|| panic!("no reply {id}"))
            .get("result")
            .clone()
    };
    let hover = |id| {
        result(id)
            .get("contents")
            .get("value")
            .as_str()
            .unwrap_or("")
            .to_owned()
    };

    let caps = result(1);
    assert_eq!(
        caps.get("capabilities").get("hoverProvider"),
        &Json::Bool(true)
    );
    assert!(hover(2).contains("p: P"), "{}", hover(2));
    assert!(hover(3).contains("fn twice(n: i32) -> i32"), "{}", hover(3));
    assert!(hover(3).ends_with("```\n\nDoubles `n`."), "{}", hover(3));
    let line = |r: &Json| r.get("range").get("start").get("line").clone();
    assert_eq!(line(&result(4)), Json::num(5));
    assert_eq!(result(4).get("uri").as_str(), Some(uri));
    assert_eq!(line(&result(5)), Json::num(7));

    let Json::Arr(symbols) = result(6) else {
        panic!("symbols: {}", result(6))
    };
    let names: Vec<&str> = symbols
        .iter()
        .filter_map(|s| s.get("name").as_str())
        .collect();
    assert_eq!(names, ["P", "impl P", "twice"]);
    let Json::Arr(methods) = symbols[1].get("children").clone() else {
        panic!()
    };
    assert_eq!(methods[0].get("name").as_str(), Some("sum"));

    let labels = |id| match result(id) {
        Json::Arr(items) => items
            .iter()
            .filter_map(|i| i.get("label").as_str().map(str::to_owned))
            .collect::<Vec<_>>(),
        r => panic!("completion {id}: {r}"),
    };
    let members = labels(7);
    for m in ["x", "y", "sum"] {
        assert!(members.iter().any(|l| l == m), "{members:?}");
    }
    assert!(labels(8).iter().any(|l| l == "fmt"), "{:?}", labels(8));

    let Json::Arr(edits) = result(9) else {
        panic!("formatting: {}", result(9))
    };
    assert_eq!(
        edits[0].get("newText").as_str(),
        Some("fn f() {\n    let a = 1;\n}\n")
    );

    // Hover still answers from the last good compile of a broken buffer.
    assert!(hover(10).contains("p: P"), "{}", hover(10));
    let bogus = msgs.iter().find(|m| m.get("id") == &Json::num(11)).unwrap();
    assert_eq!(bogus.get("error").get("code"), &Json::num(-32601));

    // Diagnostics: clean at first, then the type error of `broken`.
    let published: Vec<&Json> = msgs
        .iter()
        .filter(|m| m.get("method").as_str() == Some("textDocument/publishDiagnostics"))
        .map(|m| m.get("params"))
        .collect();
    assert_eq!(published[0].get("diagnostics"), &Json::Arr(Vec::new()));
    let Json::Arr(second) = published[1].get("diagnostics").clone() else {
        panic!()
    };
    let err = second
        .iter()
        .find(|d| d.get("severity") == &Json::num(1))
        .unwrap_or_else(|| panic!("no error in {second:?}"));
    assert_eq!(line(err), Json::num(5));
    // The last buffer type-checks; its lints follow the given levels.
    let Json::Arr(last) = published.last().unwrap().get("diagnostics").clone() else {
        panic!()
    };
    assert_eq!(last.len(), 1, "{last:?}");
    let msg = last[0].get("message").as_str().unwrap_or("");
    assert!(msg.starts_with("unused variable `a`"), "{msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

const MEMBERS: &str = "\
struct P {
    /// The x coordinate.
    x: i32;
    y: i32;
}
impl P {
    /// Sum of the coordinates.
    fn sum(self) -> i32 { self.x + self.y }
}
let p = P { x: 1, y: 2 };
let total = p.sum() + p.x;
let u = total * 2;
println(f\"{u}\");
";

/// Hover and definition of fields and methods, name ranges,
/// references and rename.
#[test]
fn members_references_rename() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bootstrap = workspace.join("library/core/src/lib.rok");
    let dir = rokc_core::tmp::test_dir("lsp-members");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("members.rok");
    let uri = path_to_uri(&file.display().to_string(), cfg!(windows));
    let uri = uri.as_str();
    let rename = |id, line, col, name: &str| {
        let mut p = at(uri, line, col);
        if let Json::Obj(fields) = &mut p {
            fields.push(("newName".to_owned(), Json::str(name)));
        }
        request(id, "textDocument/rename", p)
    };
    let script = [
        request(
            1,
            "initialize",
            Json::obj([("capabilities", Json::obj([]))]),
        ),
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([
                    ("uri", Json::str(uri)),
                    ("languageId", Json::str("rok")),
                    ("version", Json::num(1)),
                    ("text", Json::str(MEMBERS)),
                ]),
            )]),
        ),
        // `sum` and `x` in `p.sum() + p.x`, `total` in `total * 2`.
        request(2, "textDocument/hover", at(uri, 10, 15)),
        request(3, "textDocument/hover", at(uri, 10, 24)),
        request(4, "textDocument/definition", at(uri, 10, 15)),
        request(5, "textDocument/definition", at(uri, 10, 24)),
        request(6, "textDocument/definition", at(uri, 11, 9)),
        request(7, "textDocument/references", at(uri, 11, 9)),
        request(8, "textDocument/references", at(uri, 10, 24)),
        rename(9, 11, 9, "sum2"),
        rename(10, 11, 9, "fn"),
        rename(11, 12, 1, "say"),
        request(12, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        vec![bootstrap],
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(code, 0);
    let msgs = messages(&out);
    let reply = |id: u32| {
        msgs.iter()
            .find(|m| m.get("id") == &Json::num(id))
            .unwrap_or_else(|| panic!("no reply {id}"))
            .clone()
    };
    let result = |id: u32| reply(id).get("result").clone();
    let hover = |id| {
        result(id)
            .get("contents")
            .get("value")
            .as_str()
            .unwrap_or("")
            .to_owned()
    };
    let start = |r: &Json| {
        let s = r.get("range").get("start");
        (s.get("line").clone(), s.get("character").clone())
    };
    let pos = |l: u32, c: u32| (Json::num(l), Json::num(c));

    assert!(hover(2).contains("fn sum(self: P) -> i32"), "{}", hover(2));
    assert!(
        hover(2).ends_with("Sum of the coordinates."),
        "{}",
        hover(2)
    );
    assert!(hover(3).contains("P.x: i32"), "{}", hover(3));
    assert!(hover(3).ends_with("The x coordinate."), "{}", hover(3));
    // Name ranges, not whole items.
    assert_eq!(start(&result(4)), pos(7, 7));
    assert_eq!(start(&result(5)), pos(2, 4));
    assert_eq!(start(&result(6)), pos(10, 4));

    let starts = |id| match result(id) {
        Json::Arr(locs) => locs.iter().map(start).collect::<Vec<_>>(),
        r => panic!("references {id}: {r}"),
    };
    assert_eq!(starts(7), [pos(10, 4), pos(11, 8)]);
    // `.x` reads, the declaration and the literal's `x:`.
    assert_eq!(starts(8), [pos(2, 4), pos(7, 31), pos(9, 12), pos(10, 24)]);

    let Json::Arr(edits) = result(9).get("changes").get(uri).clone() else {
        panic!("rename: {}", reply(9))
    };
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0].get("newText").as_str(), Some("sum2"));
    assert!(
        matches!(reply(10).get("error"), Json::Obj(_)),
        "{}",
        reply(10)
    );
    // `println` is declared in the library.
    assert!(
        matches!(reply(11).get("error"), Json::Obj(_)),
        "{}",
        reply(11)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `.rokon` buffer (`rok.rokon`, the package manifest) is one ROKON
/// value: only its syntax is checked, not compiled as a module.
#[test]
fn manifest_buffer() {
    let uri = "file:///nowhere/rok.rokon";
    let open = notify(
        "textDocument/didOpen",
        Json::obj([(
            "textDocument",
            Json::obj([
                ("uri", Json::str(uri)),
                ("languageId", Json::str("rok")),
                ("version", Json::num(1)),
                (
                    "text",
                    Json::str("Package {\n    deps: { \"a\": Dep::path(\"x\") },\n}\n"),
                ),
            ]),
        )]),
    );
    let script = [
        open,
        request(1, "textDocument/hover", at(uri, 0, 1)),
        change(uri, "Package {\n    name: \"a\"\n    version: \"1\",\n}\n"),
        request(2, "textDocument/hover", at(uri, 0, 1)),
        // Valid rok, but not ROKON data.
        change(uri, "Package {\n    name: bare,\n}\n"),
        request(4, "textDocument/hover", at(uri, 0, 1)),
        request(3, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(code, 0);
    let published: Vec<Json> = messages(&out)
        .into_iter()
        .filter(|m| m.get("method").as_str() == Some("textDocument/publishDiagnostics"))
        .map(|m| m.get("params").get("diagnostics").clone())
        .collect();
    assert_eq!(published[0], Json::Arr(Vec::new()));
    let Json::Arr(errs) = &published[1] else {
        panic!("{published:?}")
    };
    assert!(!errs.is_empty(), "{published:?}");
    assert_eq!(errs[0].get("range").get("start").get("line"), &Json::num(2));
    let Json::Arr(errs) = &published[2] else {
        panic!("{published:?}")
    };
    assert_eq!(errs[0].get("range").get("start").get("line"), &Json::num(1));
    assert!(
        errs[0]
            .get("message")
            .as_str()
            .is_some_and(|m| m.starts_with("bare identifier `bare`")),
        "{published:?}"
    );
}

/// An open submodule buffer stands for its file: its unsaved text is
/// what the package compiles with.
#[test]
fn unsaved_submodule_buffer() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bootstrap = workspace.join("library/core/src/lib.rok");
    let dir = rokc_core::tmp::test_dir("lsp-sub");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rok"), "mod m;\n\nlet x: i32 = m::v();\n").unwrap();
    std::fs::write(dir.join("src/m.rok"), "pub fn v() -> i32 {\n    1\n}\n").unwrap();
    let uri = path_to_uri(&dir.join("src/m.rok").display().to_string(), cfg!(windows));
    let uri = uri.as_str();
    let open = notify(
        "textDocument/didOpen",
        Json::obj([(
            "textDocument",
            Json::obj([
                ("uri", Json::str(uri)),
                ("languageId", Json::str("rok")),
                ("version", Json::num(1)),
                ("text", Json::str("pub fn v() -> i32 {\n    true\n}\n")),
            ]),
        )]),
    );
    let script = [
        open,
        request(1, "textDocument/hover", at(uri, 0, 8)),
        change(uri, "pub fn v() -> i32 {\n    2\n}\n"),
        request(2, "textDocument/hover", at(uri, 0, 8)),
        request(3, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        vec![bootstrap],
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(code, 0);
    let published: Vec<Json> = messages(&out)
        .into_iter()
        .filter(|m| m.get("method").as_str() == Some("textDocument/publishDiagnostics"))
        .map(|m| m.get("params").get("diagnostics").clone())
        .collect();
    let Json::Arr(errs) = &published[0] else {
        panic!("{published:?}")
    };
    assert_eq!(errs.len(), 1, "{published:?}");
    assert!(
        errs[0]
            .get("message")
            .as_str()
            .is_some_and(|m| m.contains("bool")),
        "{published:?}"
    );
    assert_eq!(published[1], Json::Arr(Vec::new()), "{published:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn submodule_changes_and_closes_refresh_dependents() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bootstrap = workspace.join("library/core/src/lib.rok");
    let dir = rokc_core::tmp::test_dir("lsp-dependent-buffers");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let main = "mod m;\n\nlet x = m::v() + 1;\n";
    let disk = "pub fn v() -> i32 {\n    1\n}\n";
    let edited = "pub fn v() -> i64 {\n    2\n}\n";
    std::fs::write(dir.join("src/main.rok"), main).unwrap();
    std::fs::write(dir.join("src/m.rok"), disk).unwrap();
    let uri = path_to_uri(
        &dir.join("src/main.rok").display().to_string(),
        cfg!(windows),
    );
    let sub = path_to_uri(&dir.join("src/m.rok").display().to_string(), cfg!(windows));
    let open = |uri: &str, text: &str| {
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([("uri", Json::str(uri)), ("text", Json::str(text))]),
            )]),
        )
    };
    let script = [
        open(&sub, "pub fn v() -> bool {\n    true\n}\n"),
        open(&uri, main),
        request(1, "textDocument/hover", at(&uri, 2, 4)),
        change(&sub, edited),
        request(2, "textDocument/hover", at(&uri, 2, 4)),
        notify(
            "textDocument/didClose",
            Json::obj([("textDocument", doc(&sub))]),
        ),
        request(3, "textDocument/hover", at(&uri, 2, 4)),
        open(&sub, edited),
        request(4, "textDocument/hover", at(&uri, 2, 4)),
        request(5, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    assert_eq!(
        serve(
            Cursor::new(input.into_bytes()),
            &mut out,
            vec![bootstrap],
            Vec::new(),
            Vec::new()
        ),
        0
    );
    let msgs = messages(&out);
    let result = |id| {
        msgs.iter()
            .find(|m| m.get("id") == &Json::num(id))
            .unwrap()
            .get("result")
    };
    assert_eq!(result(1), &Json::Null);
    for (id, ty) in [(2, "i64"), (3, "i32"), (4, "i64")] {
        let value = result(id).get("contents").get("value").as_str().unwrap();
        assert!(value.contains(ty), "{id}: {value}");
    }
    let diagnostics: Vec<_> = msgs
        .iter()
        .filter(|m| {
            m.get("method").as_str() == Some("textDocument/publishDiagnostics")
                && m.get("params").get("uri").as_str() == Some(uri.as_str())
        })
        .map(|m| m.get("params").get("diagnostics"))
        .collect();
    let has_error = |diags: &Json| matches!(diags, Json::Arr(ds) if ds.iter().any(|d| d.get("severity") == &Json::num(1)));
    assert!(has_error(diagnostics[0]));
    assert!(!has_error(diagnostics.last().unwrap()));
}

/// A package with both root files (spec 0.3.1): a file the library
/// reaches is checked as the library's (its unsaved text included); the
/// executable's own files see the library as the package's name, its
/// `pub` items only.
#[test]
fn library_and_executable_package() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bootstrap = workspace.join("library/core/src/lib.rok");
    let dir = rokc_core::tmp::test_dir("lsp-both").join("app");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let lib = "pub fn v() -> i32 {\n    1\n}\n\nfn hidden() -> i32 {\n    2\n}\n";
    std::fs::write(dir.join("src/lib.rok"), lib).unwrap();
    let main = "mod cli;\n\nuse app::v;\n\nlet x: i32 = v() + app::hidden() + cli::w();\n";
    std::fs::write(dir.join("src/main.rok"), main).unwrap();
    std::fs::write(dir.join("src/cli.rok"), "pub fn w() -> i32 {\n    v()\n}\n").unwrap();
    let uri = |f: &str| path_to_uri(&dir.join(f).display().to_string(), cfg!(windows));
    let open = |f: &str, text: &str| {
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([
                    ("uri", Json::str(uri(f))),
                    ("languageId", Json::str("rok")),
                    ("version", Json::num(1)),
                    ("text", Json::str(text)),
                ]),
            )]),
        )
    };
    let script = [
        open("src/main.rok", main),
        request(1, "textDocument/hover", at(&uri("src/main.rok"), 2, 9)),
        open("src/cli.rok", "pub fn w() -> i32 {\n    v()\n}\n"),
        request(2, "textDocument/hover", at(&uri("src/cli.rok"), 0, 7)),
        open("src/lib.rok", &lib.replace("    1", "    true")),
        request(3, "textDocument/hover", at(&uri("src/lib.rok"), 0, 7)),
        request(4, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        vec![bootstrap],
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(code, 0);
    let published: Vec<(String, Vec<String>)> = messages(&out)
        .into_iter()
        .filter(|m| m.get("method").as_str() == Some("textDocument/publishDiagnostics"))
        .map(|m| {
            let p = m.get("params");
            let Json::Arr(d) = p.get("diagnostics") else {
                panic!("{p:?}")
            };
            let msgs = d
                .iter()
                .map(|d| d.get("message").as_str().unwrap_or_default().to_owned())
                .collect();
            (p.get("uri").as_str().unwrap_or_default().to_owned(), msgs)
        })
        .collect();
    let of = |f: &str| {
        published
            .iter()
            .find(|(u, _)| *u == uri(f))
            .unwrap_or_else(|| panic!("no diagnostics for {f}: {published:?}"))
            .1
            .clone()
    };
    assert_eq!(
        of("src/main.rok"),
        [
            "function `app::hidden` is private to module `app` and cannot be used from `<root>` \
             (spec 5.6 - add `pub` to expose it)\nhelp: `src/main.rok` uses its package's \
             library `src/lib.rok` as the package `app`, like a dependency: it sees only the \
             library's `pub` items"
        ]
    );
    let lib_diags = of("src/lib.rok");
    assert!(
        lib_diags.len() == 1 && lib_diags[0].contains("bool"),
        "{lib_diags:?}"
    );
    // `cli` is the executable's: `v` is not in its scope.
    let cli = of("src/cli.rok");
    assert!(
        cli.len() == 1 && cli[0].contains("cannot find function `v`"),
        "{cli:?}"
    );
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}

const LOCAL: &str = "\
fn f() -> i32 {
    struct L { v: i32; }
    impl L {
        fn get(self) -> i32 { self.v }
    }
    let l = L { v: 1 };
    l.get()
}
println(f\"{f()}\");
";

/// Hover and definition of an item declared in a function body
/// (spec 3.9.1) go by the name as written.
#[test]
fn block_local_items() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bootstrap = workspace.join("library/core/src/lib.rok");
    let dir = rokc_core::tmp::test_dir("lsp-local");
    std::fs::create_dir_all(&dir).unwrap();
    let uri = path_to_uri(&dir.join("local.rok").display().to_string(), cfg!(windows));
    let uri = uri.as_str();
    let script = [
        request(
            1,
            "initialize",
            Json::obj([("capabilities", Json::obj([]))]),
        ),
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([
                    ("uri", Json::str(uri)),
                    ("languageId", Json::str("rok")),
                    ("version", Json::num(1)),
                    ("text", Json::str(LOCAL)),
                ]),
            )]),
        ),
        // `l` and `get` in `l.get()`, `L` in `L { v: 1 }`.
        request(2, "textDocument/hover", at(uri, 6, 4)),
        request(3, "textDocument/hover", at(uri, 6, 7)),
        request(4, "textDocument/definition", at(uri, 6, 7)),
        request(5, "textDocument/definition", at(uri, 5, 12)),
        request(6, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        vec![bootstrap],
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(code, 0);
    let msgs = messages(&out);
    let result = |id: u32| {
        msgs.iter()
            .find(|m| m.get("id") == &Json::num(id))
            .unwrap_or_else(|| panic!("no reply {id}"))
            .get("result")
            .clone()
    };
    let hover = |id| {
        result(id)
            .get("contents")
            .get("value")
            .as_str()
            .unwrap_or("")
            .to_owned()
    };
    let line = |id| result(id).get("range").get("start").get("line").clone();
    assert!(hover(2).contains("l: L"), "{}", hover(2));
    assert!(hover(3).contains("fn get(self: L) -> i32"), "{}", hover(3));
    assert_eq!(line(4), Json::num(3), "{}", result(4));
    assert_eq!(line(5), Json::num(1), "{}", result(5));
    let _ = std::fs::remove_dir_all(&dir);
}

const HIDDEN: &str = "\
pub mod m {
    pub fn shown() -> i32 { 1 }
    #[doc(hidden)]
    pub fn hook() -> i32 { 2 }
}
struct P { x: i32; }
impl P {
    fn get(self) -> i32 { self.x }
    #[doc(hidden)]
    fn raw(self) -> i32 { self.x } }
let p = P { x: m::shown() + m::hook() };
println(f\"{p.get() + p.raw()}\");
";

/// Completion leaves out `#[doc(hidden)]` items: the buffer's own and
/// the library's compiler hooks.
#[test]
fn completion_skips_doc_hidden() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bootstrap = workspace.join("library/core/src/lib.rok");
    let dir = rokc_core::tmp::test_dir("lsp-hidden");
    std::fs::create_dir_all(&dir).unwrap();
    let uri = path_to_uri(&dir.join("hidden.rok").display().to_string(), cfg!(windows));
    let uri = uri.as_str();
    let script = [
        request(
            1,
            "initialize",
            Json::obj([("capabilities", Json::obj([]))]),
        ),
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([
                    ("uri", Json::str(uri)),
                    ("languageId", Json::str("rok")),
                    ("version", Json::num(1)),
                    ("text", Json::str(HIDDEN)),
                ]),
            )]),
        ),
        // `p` in `let p`: the good compile path completion reads.
        request(6, "textDocument/hover", at(uri, 10, 4)),
        change(uri, &format!("{HIDDEN}m::\n")),
        request(2, "textDocument/completion", at(uri, 12, 3)),
        change(uri, &format!("{HIDDEN}let t = p.\n")),
        request(3, "textDocument/completion", at(uri, 12, 10)),
        change(uri, &format!("{HIDDEN}rok::core::index::\n")),
        request(4, "textDocument/completion", at(uri, 12, 18)),
        request(5, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        vec![bootstrap],
        Vec::new(),
        vec![(crate::lint::Level::Allow, "dead_code".to_owned())],
    );
    assert_eq!(code, 0);
    let msgs = messages(&out);
    let labels = |id: u32| match msgs
        .iter()
        .find(|m| m.get("id") == &Json::num(id))
        .unwrap_or_else(|| panic!("no reply {id}"))
        .get("result")
    {
        Json::Arr(items) => items
            .iter()
            .filter_map(|i| i.get("label").as_str().map(str::to_owned))
            .collect::<Vec<_>>(),
        r => panic!("completion {id}: {r}"),
    };
    let has = |id: u32, name: &str| labels(id).iter().any(|l| l == name);
    assert!(has(2, "shown") && !has(2, "hook"), "{:?}", labels(2));
    assert!(has(3, "get") && !has(3, "raw"), "{:?}", labels(3));
    assert!(has(4, "index_oob"), "{:?}", labels(4));
    for hook in [
        "spread_misfit",
        "spread_len",
        "index_from_end",
        "slice_range",
    ] {
        assert!(!has(4, hook), "{hook} in {:?}", labels(4));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// The server highlights macro calls and user-defined literal suffixes via
// semantic tokens (spec 6.5 / 6.8.12), which the static grammar cannot.
#[test]
fn semantic_tokens_macros_and_literals() {
    let dir = rokc_core::tmp::test_dir("lsp-sem");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("s.rok");
    let uri = path_to_uri(&file.display().to_string(), cfg!(windows));
    let uri = uri.as_str();
    // `kg` is a registered user literal; `zz` is not (so it stays plain).
    let src = "#[literal(number \"kg\")] fn kg(n: Int) -> i32 { 0 }\n\
               fn f() {\n    let a = foo!(1);\n    let b = 5kg;\n    let c = 9zz;\n}\n";
    let script = [
        request(
            1,
            "initialize",
            Json::obj([("capabilities", Json::obj([]))]),
        ),
        notify("initialized", Json::obj([])),
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([
                    ("uri", Json::str(uri)),
                    ("languageId", Json::str("rok")),
                    ("version", Json::num(1)),
                    ("text", Json::str(src)),
                ]),
            )]),
        ),
        request(
            2,
            "textDocument/semanticTokens/full",
            Json::obj([("textDocument", doc(uri))]),
        ),
        request(3, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(code, 0);
    let msgs = messages(&out);
    // The legend is advertised.
    let caps = msgs
        .iter()
        .find(|m| m.get("id") == &Json::num(1))
        .unwrap()
        .get("result")
        .get("capabilities")
        .get("semanticTokensProvider")
        .get("legend")
        .get("tokenTypes")
        .to_string();
    assert!(
        caps.contains("macro") && caps.contains("decorator"),
        "{caps}"
    );
    // Two tokens: the `foo!` call (type 0) and the `px` suffix (type 1).
    let data = match msgs
        .iter()
        .find(|m| m.get("id") == &Json::num(2))
        .unwrap()
        .get("result")
        .get("data")
    {
        Json::Arr(v) => v.iter().filter_map(Json::as_u32).collect::<Vec<_>>(),
        other => panic!("not an array: {other}"),
    };
    assert_eq!(data.len(), 10, "{data:?}");
    assert_eq!(data[3], 0, "macro token type: {data:?}");
    assert_eq!(data[8], 1, "literal token type: {data:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// Hovering a `name!(..)` call shows its expansion (spec 6.8.12 tooling).
#[test]
fn hover_expands_macro_call() {
    let dir = rokc_core::tmp::test_dir("lsp-mhover");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("h.rok");
    let uri = path_to_uri(&file.display().to_string(), cfg!(windows));
    let uri = uri.as_str();
    let src = "#[impl_macro(dbl)] pub @macro { (@e: expr) => { (@e, @e) } }\n\
               fn f() {\n    let x = dbl!(1i32);\n}\n";
    let script = [
        request(
            1,
            "initialize",
            Json::obj([("capabilities", Json::obj([]))]),
        ),
        notify("initialized", Json::obj([])),
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([
                    ("uri", Json::str(uri)),
                    ("languageId", Json::str("rok")),
                    ("version", Json::num(1)),
                    ("text", Json::str(src)),
                ]),
            )]),
        ),
        // cursor on `dbl` in `dbl!(1i32)` (line 2, inside the name)
        request(2, "textDocument/hover", at(uri, 2, 13)),
        request(3, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(code, 0);
    let msgs = messages(&out);
    let value = msgs
        .iter()
        .find(|m| m.get("id") == &Json::num(2))
        .unwrap()
        .get("result")
        .get("contents")
        .get("value")
        .as_str()
        .unwrap_or("")
        .to_owned();
    assert!(
        value.contains("1i32") && value.contains("```rok"),
        "{value}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Every user-defined construct the static grammar cannot know is lit up:
// macro call, literal suffix, keyword, operator, derive, attribute.
#[test]
fn semantic_tokens_all_user_defined() {
    let dir = rokc_core::tmp::test_dir("lsp-sem2");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("s2.rok");
    let uri = path_to_uri(&file.display().to_string(), cfg!(windows));
    let uri = uri.as_str();
    let src = concat!(
        "#[literal(number \"kg\")] fn kg(n: i32) -> i32 { return 0; }\n",
        "#[keyword(\"mykw\")] fn myh() {}\n",
        "#[infix(\"\u{2295}\")] fn circ() {}\n",
        "#[impl_derive(Foo)] pub @derive { impl Foo for @Self {} }\n",
        "#[impl_attribute(bar)] pub @attribute { @item }\n",
        "#[derive(Foo)]\n#[bar]\nstruct S { x: i32; }\n",
        "fn g() { let a = mac!(1); let b = 5kg; let c = 1 \u{2295} 2; mykw; }\n",
    );
    let script = [
        request(
            1,
            "initialize",
            Json::obj([("capabilities", Json::obj([]))]),
        ),
        notify("initialized", Json::obj([])),
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([
                    ("uri", Json::str(uri)),
                    ("languageId", Json::str("rok")),
                    ("version", Json::num(1)),
                    ("text", Json::str(src)),
                ]),
            )]),
        ),
        request(
            2,
            "textDocument/semanticTokens/full",
            Json::obj([("textDocument", doc(uri))]),
        ),
        request(3, "shutdown", Json::Null),
        notify("exit", Json::Null),
    ];
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(code, 0);
    let msgs = messages(&out);
    let data = match msgs
        .iter()
        .find(|m| m.get("id") == &Json::num(2))
        .unwrap()
        .get("result")
        .get("data")
    {
        Json::Arr(v) => v.iter().filter_map(Json::as_u32).collect::<Vec<_>>(),
        other => panic!("not an array: {other}"),
    };
    // The token type is the 4th int of each 5-tuple.
    let types: std::collections::HashSet<u32> = data.chunks(5).map(|c| c[3]).collect();
    for ty in [0u32, 1, 2, 3] {
        assert!(
            types.contains(&ty),
            "missing token type {ty}: types={types:?} data={data:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Open `text` as a lone file with core and std, then send `steps(uri)`:
/// the server's messages.
fn std_session(name: &str, text: &str, steps: impl FnOnce(&str) -> Vec<Json>) -> Vec<Json> {
    let lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library");
    let bootstrap = rokc_interface::bootstrap_tiers(&[
        lib.join("core/src/lib.rok"),
        lib.join("std/src/lib.rok"),
    ]);
    let dir = rokc_core::tmp::test_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    let uri = path_to_uri(&dir.join("main.rok").display().to_string(), cfg!(windows));
    let mut script = vec![
        request(
            1000,
            "initialize",
            Json::obj([("capabilities", Json::obj([]))]),
        ),
        notify(
            "textDocument/didOpen",
            Json::obj([(
                "textDocument",
                Json::obj([
                    ("uri", Json::str(&uri)),
                    ("languageId", Json::str("rok")),
                    ("version", Json::num(1)),
                    ("text", Json::str(text)),
                ]),
            )]),
        ),
    ];
    script.extend(steps(&uri));
    script.push(request(1001, "shutdown", Json::Null));
    script.push(notify("exit", Json::Null));
    let input: String = script.iter().map(frame).collect();
    let mut out = Vec::new();
    let code = serve(
        Cursor::new(input.into_bytes()),
        &mut out,
        bootstrap,
        Vec::new(),
        vec![(crate::lint::Level::Allow, "dead_code".to_owned())],
    );
    assert_eq!(code, 0);
    let _ = std::fs::remove_dir_all(&dir);
    messages(&out)
}

fn reply(msgs: &[Json], id: u32) -> Json {
    msgs.iter()
        .find(|m| m.get("id") == &Json::num(id))
        .unwrap_or_else(|| panic!("no reply {id}"))
        .get("result")
        .clone()
}

fn labels(msgs: &[Json], id: u32) -> Vec<String> {
    match reply(msgs, id) {
        Json::Arr(items) => items
            .iter()
            .filter_map(|i| i.get("label").as_str().map(str::to_owned))
            .collect(),
        r => panic!("completion {id}: {r}"),
    }
}

const LIST: &str = "\
let xs: List<i32> = [1, 2];
xs.push(3);
println(f\"{xs.len()}\");
";

/// `xs.` offers the public methods of `List`, not its private field,
/// helpers, operator hooks or a generic instance's name.
#[test]
fn completion_list_methods_not_internals() {
    let msgs = std_session("lsp-list-members", LIST, |uri| {
        vec![
            request(1, "textDocument/hover", at(uri, 0, 4)),
            change(uri, &format!("{LIST}let n = xs.\n")),
            request(2, "textDocument/completion", at(uri, 3, 11)),
        ]
    });
    let got = labels(&msgs, 2);
    for m in ["push", "len", "pop", "iter", "clone"] {
        assert!(got.iter().any(|l| l == m), "{m} not in {got:?}");
    }
    for m in [
        "h",
        "Prim(I32)",
        "push_grown",
        "dot_add",
        "index",
        "transfer_copy",
    ] {
        assert!(!got.iter().any(|l| l == m), "{m} in {got:?}");
    }
}

#[test]
fn completion_partial_method() {
    let msgs = std_session("lsp-partial-member", LIST, |uri| {
        vec![
            request(1, "textDocument/hover", at(uri, 0, 4)),
            change(uri, &format!("{LIST}xs.pu\n")),
            request(2, "textDocument/completion", at(uri, 3, 5)),
        ]
    });
    assert_eq!(labels(&msgs, 2), ["push"]);
}

#[test]
fn completion_bare_prelude() {
    let msgs = std_session("lsp-bare", "pri\n", |uri| {
        vec![request(1, "textDocument/completion", at(uri, 0, 3))]
    });
    let got = labels(&msgs, 1);
    assert!(got.iter().any(|l| l == "print"), "{got:?}");
    assert!(got.iter().any(|l| l == "println"), "{got:?}");
    assert!(got.iter().all(|l| l.starts_with("pri")), "{got:?}");
}

/// A bare word: the locals in scope at the cursor, the buffer's items
/// and keywords; not another function's locals.
#[test]
fn completion_local() {
    let src = "\
fn helper() -> i32 {
    let inner = 1;
    inner
}
let count = helper();
let total = co
";
    let msgs = std_session("lsp-local", src, |uri| {
        vec![
            request(1, "textDocument/completion", at(uri, 5, 14)),
            request(2, "textDocument/completion", at(uri, 5, 12)),
        ]
    });
    let got = labels(&msgs, 1);
    assert!(got.iter().any(|l| l == "count"), "{got:?}");
    assert!(got.iter().any(|l| l == "const"), "{got:?}");
    let all = labels(&msgs, 2);
    assert!(all.iter().any(|l| l == "helper"), "{all:?}");
    assert!(!all.iter().any(|l| l == "inner"), "{all:?}");
}

/// `use a::b::` lists the modules below, even before the buffer has
/// ever checked.
#[test]
fn completion_use_path() {
    let msgs = std_session("lsp-use-path", "use rok::std::\n", |uri| {
        vec![
            request(1, "textDocument/completion", at(uri, 0, 14)),
            change(uri, "use rok::std::fs::\n"),
            request(2, "textDocument/completion", at(uri, 0, 18)),
        ]
    });
    let got = labels(&msgs, 1);
    for m in ["fs", "env", "process"] {
        assert!(got.iter().any(|l| l == m), "{m} not in {got:?}");
    }
    let fs = labels(&msgs, 2);
    assert!(fs.iter().any(|l| l == "read_to_str"), "{fs:?}");
    assert!(!fs.iter().any(|l| l == "stat_at"), "{fs:?}");
}

/// Definition of a method goes by the receiver's type, and of a library
/// function to its source.
#[test]
fn definition_by_receiver_type() {
    let msgs = std_session("lsp-def-recv", LIST, |uri| {
        vec![
            request(1, "textDocument/definition", at(uri, 1, 4)),
            request(2, "textDocument/definition", at(uri, 2, 14)),
        ]
    });
    let uri = |id| {
        reply(&msgs, id)
            .get("uri")
            .as_str()
            .unwrap_or("")
            .to_owned()
    };
    assert!(
        uri(1).ends_with("collections/list.rok"),
        "{}",
        reply(&msgs, 1)
    );
    assert!(
        uri(2).ends_with("collections/list.rok"),
        "{}",
        reply(&msgs, 2)
    );
}

#[test]
fn definition_println_not_null() {
    let msgs = std_session("lsp-def-println", "println(\"hi\");\n", |uri| {
        vec![request(1, "textDocument/definition", at(uri, 0, 2))]
    });
    let got = reply(&msgs, 1);
    assert!(
        got.get("uri")
            .as_str()
            .is_some_and(|u| u.ends_with("print.rok")),
        "{got}"
    );
}

/// Hover on a library method and function: its signature, then its doc.
#[test]
fn hover_library_signature_and_doc() {
    let src = "let xs: List<i32> = [1];\nxs.extend([2, 3]);\nprintln(\"a\");\n";
    let msgs = std_session("lsp-hover-lib", src, |uri| {
        vec![
            request(1, "textDocument/hover", at(uri, 1, 5)),
            request(2, "textDocument/hover", at(uri, 2, 3)),
        ]
    });
    let value = |id| {
        reply(&msgs, id)
            .get("contents")
            .get("value")
            .as_str()
            .unwrap_or("")
            .to_owned()
    };
    assert!(value(1).contains("fn extend("), "{}", value(1));
    assert!(value(1).contains("Push every item"), "{}", value(1));
    assert!(value(2).contains("fn println("), "{}", value(2));
}

/// A symbol's range covers the whole item; its selection range is the
/// name.
#[test]
fn document_symbol_ranges_cover_item() {
    let src = "\
pub fn dist2(a: i32, b: i32) -> i32 {
    a * a + b * b
}
struct P { x: i32; }
impl P {
    fn get(self) -> i32 { self.x }
}
";
    let msgs = std_session("lsp-symbols", src, |uri| {
        vec![request(
            1,
            "textDocument/documentSymbol",
            Json::obj([("textDocument", doc(uri))]),
        )]
    });
    let Json::Arr(syms) = reply(&msgs, 1) else {
        panic!("{}", reply(&msgs, 1))
    };
    let pos = |r: &Json, end: &str| {
        let p = r.get(end);
        (
            p.get("line").as_u32().unwrap(),
            p.get("character").as_u32().unwrap(),
        )
    };
    let f = &syms[0];
    assert_eq!(pos(f.get("range"), "start"), (0, 0));
    assert_eq!(pos(f.get("range"), "end"), (2, 1));
    assert_eq!(pos(f.get("selectionRange"), "start"), (0, 7));
    assert_eq!(pos(f.get("selectionRange"), "end"), (0, 12));
    assert_eq!(pos(syms[1].get("range"), "end"), (3, 20));
    assert_eq!(pos(syms[2].get("range"), "end"), (6, 1));
    let Json::Arr(methods) = syms[2].get("children").clone() else {
        panic!()
    };
    assert_eq!(pos(methods[0].get("range"), "start"), (5, 4));
    assert_eq!(pos(methods[0].get("range"), "end"), (5, 34));
    assert_eq!(pos(methods[0].get("selectionRange"), "start"), (5, 7));
}

/// A parse error's range is never empty: it covers a token.
#[test]
fn parse_diag_has_width() {
    let msgs = std_session("lsp-diag-width", "let a = 1\nlet b = a;\n", |uri| {
        vec![request(1, "textDocument/hover", at(uri, 0, 4))]
    });
    let diags: Vec<Json> = msgs
        .iter()
        .filter(|m| m.get("method").as_str() == Some("textDocument/publishDiagnostics"))
        .flat_map(|m| match m.get("params").get("diagnostics") {
            Json::Arr(v) => v.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert!(!diags.is_empty());
    for d in diags {
        let r = d.get("range");
        assert_ne!(r.get("start"), r.get("end"), "{d}");
    }
}
