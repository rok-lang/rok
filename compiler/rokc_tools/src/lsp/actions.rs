//! `textDocument/codeAction`: quick fixes from a diagnostic's fixes, else
//! from its help lines (add `;`, add `use a::b;`, `rok add x`).
//! `workspace/executeCommand` runs the `rok.add` they name.

use std::path::Path;
use std::process::Command;

use rokc_diagnostic::{Json, RokDiagnostic};

use super::{range, uri_path};

/// The command a `rok add` quick fix runs.
pub(super) const ROK_ADD: &str = "rok.add";

/// Quick fixes for the diagnostics of `context` (the published ones in
/// `diags`, matched by range and first line), or with none given, for
/// those overlapping `at`.
pub(super) fn code_actions(
    uri: &str,
    text: &str,
    diags: &[(Json, RokDiagnostic)],
    context: &Json,
    at: &Json,
) -> Vec<Json> {
    let first = |j: &Json| {
        j.get("message")
            .as_str()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .to_owned()
    };
    let asked: Vec<&Json> = match context.get("diagnostics") {
        Json::Arr(v) if !v.is_empty() => v.iter().collect(),
        _ => Vec::new(),
    };
    let chosen = diags.iter().filter(|(j, _)| {
        if asked.is_empty() {
            overlaps(j.get("range"), at)
        } else {
            asked
                .iter()
                .any(|c| c.get("range") == j.get("range") && first(c) == first(j))
        }
    });
    let mut out = Vec::new();
    for (j, d) in chosen {
        out.extend(fixes(uri, text, j, d));
    }
    out
}

/// LSP position as `(line, character)`.
fn pos(p: &Json) -> (u32, u32) {
    (
        p.get("line").as_u32().unwrap_or(0),
        p.get("character").as_u32().unwrap_or(0),
    )
}

fn overlaps(a: &Json, b: &Json) -> bool {
    pos(a.get("start")) <= pos(b.get("end")) && pos(b.get("start")) <= pos(a.get("end"))
}

fn edit_action(uri: &str, title: &str, diag: &Json, edits: Vec<Json>) -> Json {
    Json::obj([
        ("title", Json::str(title)),
        ("kind", Json::str("quickfix")),
        ("diagnostics", Json::Arr(vec![diag.clone()])),
        (
            "edit",
            Json::obj([("changes", Json::obj([(uri, Json::Arr(edits))]))]),
        ),
    ])
}

fn text_edit(text: &str, start: usize, end: usize, new: &str) -> Json {
    Json::obj([
        ("range", range(text, start, end)),
        ("newText", Json::str(new)),
    ])
}

/// The quick fixes of one diagnostic.
fn fixes(uri: &str, text: &str, diag: &Json, d: &RokDiagnostic) -> Vec<Json> {
    let Some(primary) = d.primary.as_ref().map(|l| l.span) else {
        return Vec::new();
    };
    let own = d
        .fixes
        .iter()
        .filter(|f| !f.span.is_dummy() && f.span.file == primary.file)
        .filter(|f| f.span.end as usize <= text.len())
        .map(|f| {
            let edit = text_edit(text, f.span.start as usize, f.span.end as usize, &f.text);
            edit_action(uri, &f.title, diag, vec![edit])
        })
        .collect::<Vec<_>>();
    if !own.is_empty() {
        return own;
    }
    let mut out = Vec::new();
    for help in d.notes.iter().filter_map(|n| n.strip_prefix("help: ")) {
        // Text in backticks: the odd pieces.
        let quoted: Vec<&str> = help.split('`').skip(1).step_by(2).collect();
        if help.contains("add `;`") {
            let end = (primary.end as usize).min(text.len());
            let edit = text_edit(text, end, end, ";");
            out.push(edit_action(uri, "add `;`", diag, vec![edit]));
        }
        // `add `use a::b;`` / `add one of ..`; not into a nested module.
        if help.contains("add ") && !help.contains("inside `mod") {
            for u in quoted
                .iter()
                .filter(|q| q.starts_with("use ") && q.ends_with(';'))
            {
                let at = use_line(text);
                let lead = if at == text.len() && !text.is_empty() && !text.ends_with('\n') {
                    "\n"
                } else {
                    ""
                };
                let edit = text_edit(text, at, at, &format!("{lead}{u}\n"));
                out.push(edit_action(uri, &format!("add `{u}`"), diag, vec![edit]));
            }
        }
        for q in &quoted {
            let Some(args) = q.strip_prefix("rok add ") else {
                continue;
            };
            // `rok add <name> [<file>]`: the file may hold spaces; a
            // script's `<file>` is this document.
            let (name, file) = args.split_once(' ').unwrap_or((args, ""));
            let path = uri_path(uri).display().to_string();
            let file = if file == "<file>" {
                path.as_str()
            } else {
                file
            };
            if name.contains('<') || file.contains('<') {
                continue;
            }
            let mut list = vec![Json::str(uri), Json::str(name)];
            if !file.is_empty() {
                list.push(Json::str(file));
            }
            out.push(Json::obj([
                ("title", Json::str(format!("run `rok add {name}`"))),
                ("kind", Json::str("quickfix")),
                ("diagnostics", Json::Arr(vec![diag.clone()])),
                (
                    "command",
                    Json::obj([
                        ("title", Json::str(format!("rok add {name}"))),
                        ("command", Json::str(ROK_ADD)),
                        ("arguments", Json::Arr(list)),
                    ]),
                ),
            ]));
        }
    }
    out
}

/// Where a new `use` line goes: after the last top-level `use`, else
/// after the file's leading `//!` lines and inner attributes.
fn use_line(text: &str) -> usize {
    let mut after = None;
    let mut head = Some(0);
    let mut at = 0;
    let mut in_use = false;
    for line in text.split_inclusive('\n') {
        let next = at + line.len();
        let t = line.trim_end();
        if t.starts_with("use ") || t.starts_with("pub use ") {
            in_use = true;
        }
        if in_use {
            if t.ends_with(';') {
                in_use = false;
                after = Some(next);
            }
        } else if head == Some(at) && (t.starts_with("//!") || t.starts_with("#!")) {
            head = Some(next);
        }
        at = next;
    }
    after.or(head).unwrap_or(0)
}

/// `rok add <args>` for the document `uri`, in its package's directory
/// (a script's own).
pub(super) fn rok_add(uri: &str, args: &[String]) -> Result<(), String> {
    if args.is_empty() || args.iter().any(|a| a.starts_with('-')) {
        return Err("`rok.add` takes a stone name".to_owned());
    }
    let path = uri_path(uri);
    let dir = rokc_interface::discover(&path)
        .ok()
        .filter(|p| !p.anonymous)
        .map(|p| p.dir)
        .or_else(|| path.parent().map(Path::to_path_buf))
        .ok_or("no directory for this document")?;
    let rok = std::env::var_os("ROK").map_or_else(sibling_rok, Into::into);
    let out = Command::new(&rok)
        .arg("add")
        .args(args)
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("cannot run `{}`: {e}", rok.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    Err(if err.is_empty() {
        format!("`rok add {}` failed", args.join(" "))
    } else {
        err.to_owned()
    })
}

/// `rok` beside this server, else from `PATH`.
fn sibling_rok() -> std::path::PathBuf {
    let name = format!("rok{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.join(&name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| name.into())
}

#[cfg(test)]
mod tests {
    use super::use_line;

    #[test]
    fn new_use_goes_after_the_last_use() {
        assert_eq!(use_line("let a = 1;\n"), 0);
        assert_eq!(use_line("//! A tool.\n#![x]\nfn f() {}\n"), 18);
        let src = "use a::b;\nuse c::{\n    d,\n};\nfn f() {}\n";
        assert_eq!(use_line(src), src.find("fn").unwrap());
    }
}
