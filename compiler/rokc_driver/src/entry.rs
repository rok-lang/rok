//! Entry point rules: `main.rok` provides either an explicit
//! `fn main` or top-level statements (the implicit `main`), never both.
//! Every other file is definitions-only.

use std::path::Path;

use rokc_ast::{Block, FnDef, Generics, Item, Stmt, ThrowsKind, TyKind};
use rokc_diagnostic::{Code, RokDiagnostic, Span};

pub fn stmts_outside_entry(file: &Path) -> RokDiagnostic {
    RokDiagnostic::error(
        Code::ENTRY_POINT,
        Span::DUMMY,
        format!(
            "{}: top-level statements are only allowed in the entry file `main.rok`; \
             modules are definitions-only",
            file.display()
        ),
    )
}

/// Turn the entry file's top-level statements into the implicit `main`
/// and validate an explicit one. Whether `main` is the implicit one.
pub fn resolve_entry(
    stmts: Vec<Stmt>,
    items: &mut Vec<(String, Item)>,
    diags: &mut Vec<RokDiagnostic>,
) -> bool {
    let explicit = items.iter().find_map(|(m, i)| match i {
        Item::Fn(f) if m.is_empty() && f.name == "main" => Some(f),
        _ => None,
    });
    match (explicit, stmts.is_empty()) {
        (Some(f), false) => {
            diags.push(
                RokDiagnostic::error(
                    Code::ENTRY_POINT,
                    Span::DUMMY,
                    "`main.rok` has both top-level statements and `fn main`; use exactly one \
                 entry point",
                )
                .with_span(stmts[0].span, "a top-level statement")
                .with_secondary(f.span, "`fn main` here"),
            );
            false
        }
        (Some(f), true) => {
            check_main_signature(f, diags);
            false
        }
        // Items only: form 2 with no statements, an empty implicit `main`.
        (None, _) => {
            items.push((String::new(), Item::Fn(implicit_main(stmts))));
            true
        }
    }
}

fn implicit_main(stmts: Vec<Stmt>) -> FnDef {
    let span = stmts.first().map_or(Span::DUMMY, |s| s.span);
    let is_async = crate::coro::has_top_level_await(&stmts);
    FnDef {
        vis: rokc_core::Visibility::Private,
        attrs: Vec::new(),
        name: "main".to_owned(),
        generics: Generics::default(),
        params: Vec::new(),
        ret_ty: None,
        body: Some(
            Block {
                stmts,
                trailing: None,
                span,
                label: None,
            }
            .into(),
        ),
        // `await` at top level makes it `async fn main`.
        is_async,
        is_generator: false,
        is_unsafe: false,
        // The implicit `main` is `fn main() throws` (untyped).
        throws: ThrowsKind::Typed(vec![rokc_ast::untyped_throws_ty(span)]),
        where_clause: Vec::new(),
        span,
    }
}

/// No parameters, generics or `unsafe`; `()` or `i32`, or
/// `()` with `throws (E)` (an escaping error exits 1). `Result` isn't an
/// entry form.
fn check_main_signature(f: &FnDef, diags: &mut Vec<RokDiagnostic>) {
    let mut bad = |msg: &str| {
        diags.push(RokDiagnostic::error(
            Code::ENTRY_POINT,
            f.span,
            format!("`fn main`: {msg}"),
        ));
    };
    if !f.params.is_empty() {
        bad("takes no parameters; read arguments through `rok::std::env`");
    }
    if !f.generics.params.is_empty() {
        bad("cannot be generic");
    }
    if f.is_generator {
        bad("cannot be a generator");
    }
    if f.is_unsafe {
        bad("cannot be `unsafe`; put the unsafe operations in an `unsafe { }` block");
    }
    let ret_ok = match &f.ret_ty {
        None => true,
        Some(t) => match &t.kind {
            TyKind::Path(p) => p.segments.len() == 1 && p.segments[0].name == "i32",
            TyKind::Tuple(elems) => elems.is_empty(),
            _ => false,
        },
    };
    let is_result = f.ret_ty.as_ref().is_some_and(|t| {
        matches!(&t.kind, TyKind::Path(p) if p.segments.last().is_some_and(|s| s.name == "Result"))
    });
    if is_result {
        bad(
            "cannot return `Result`; declare `fn main() throws (E)` so an escaping error \
             exits 1, and call `rok::std::process::exit(code)` for any other exit code",
        );
    } else if !ret_ok {
        bad("must return `()` or `i32`");
    } else if f
        .ret_ty
        .as_ref()
        .is_some_and(|t| matches!(&t.kind, TyKind::Path(_)))
        && !matches!(&f.throws, ThrowsKind::None)
    {
        bad(
            "cannot combine `throws` with `-> i32`; drop `-> i32` and call \
             `rok::std::process::exit(code)` in a `catch` for an exit code",
        );
    }
}
