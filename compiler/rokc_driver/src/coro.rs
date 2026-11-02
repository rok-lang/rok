//! `yield fn` / `async fn` / `async yield fn` as plain
//! functions returning the coroutine wrapper, the body a coroutine block:
//!
//! ```text
//! async fn f(x: i32) throws (E) -> T { body }
//!   => fn f(x: i32) -> rok::core::future::TryAsyncFuture<T, E> {
//!          async '#coro_fn: { body }
//!      }
//! ```
//!
//! The error leaves through the coroutine (`await` / `next`),
//! so the rewritten function doesn't throw. `async fn main` runs
//! its body on the executor:
//! `fn main() -> T { rok::core::task::block_on(async { body }) }`.

use rokc_ast::{
    Block, Expr, ExprKind, FnDef, ImplItem, Item, Path, PathSegment, Stmt, StmtKind, ThrowsKind,
    TryKind, Ty, TyKind,
};
use rokc_diagnostic::{Code, RokDiagnostic, Span};

/// The flattened items; a root `main` is the entry.
pub fn rewrite_items(items: &mut [(String, Item)], diags: &mut Vec<RokDiagnostic>) {
    for (module, item) in items {
        match item {
            Item::Fn(f) => rewrite_fn(f, module.is_empty(), diags),
            Item::Impl(i) => {
                for it in &mut i.items {
                    if let ImplItem::Method(f) = it {
                        rewrite_fn(f, false, diags);
                    }
                }
            }
            Item::Trait(t) => {
                for it in &mut t.items {
                    if let rokc_ast::TraitItem::Method(f) = it {
                        rewrite_fn(f, false, diags);
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn path(segs: &[&str], generics: Vec<Ty>, span: Span) -> Path {
    let n = segs.len();
    Path {
        segments: segs
            .iter()
            .enumerate()
            .map(|(i, s)| PathSegment {
                name: (*s).to_owned(),
                generics: if i + 1 == n {
                    generics.clone()
                } else {
                    Vec::new()
                },
            })
            .collect(),
        span,
        origin: None,
    }
}

/// `root`: a free function of the root module (`main` is the entry).
fn rewrite_fn(f: &mut FnDef, root: bool, diags: &mut Vec<RokDiagnostic>) {
    if !(f.is_async || f.is_generator)
        || f.attrs
            .iter()
            .any(|a| a.path.first().map(String::as_str) == Some(rokc_ast::EXTERN_FN_ATTR))
    {
        return;
    }
    // A trait method without a default body only changes its signature.
    let body = f.body.take();
    let span = f.span;
    // An `async` method's `ref self` is written back when its future
    // completes (HIR `ref_unpack`); nothing else can wait for that.
    let deferred = |p: &rokc_ast::Param| p.name == "self" && f.is_async && !f.is_generator;
    if f.params.iter().any(|p| p.by_ref && !deferred(p)) {
        let mut d = RokDiagnostic::error(
            Code::INVALID_SIGNATURE,
            span,
            format!(
                "`{}`: a coroutine (`async` / `yield`) fn cannot take `ref` parameters - \
                 its body runs after the call returns",
                f.name
            ),
        );
        d = d.with_help(if f.is_async && !f.is_generator {
            "only an `async` method's `ref self` is written back (when its future \
             completes); return the new value instead"
        } else {
            "return the new value instead, or take the value and yield what changed"
        });
        diags.push(d);
    }
    let unit = Ty {
        kind: TyKind::Tuple(Vec::new()),
        span,
    };
    // An `async fn` without `-> T` takes its result type
    // from the body (HIR infers the fn's return type from the block).
    let infer = f.ret_ty.is_none()
        && f.is_async
        && !f.is_generator
        && body.is_some()
        && matches!(f.throws, ThrowsKind::None)
        && !(root && f.name == "main");
    let val = match f.ret_ty.take() {
        Some(t) => t,
        None if f.is_generator => {
            diags.push(RokDiagnostic::error(
                Code::INVALID_SIGNATURE,
                f.span,
                format!(
                    "`yield fn {}` needs its item type: `yield fn {}(..) -> T`",
                    f.name, f.name
                ),
            ));
            unit
        }
        None => unit,
    };
    let err = match std::mem::take(&mut f.throws) {
        ThrowsKind::Typed(errs) => match errs.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            many => Some(Ty {
                kind: TyKind::Sum(many.to_vec()),
                span,
            }),
        },
        ThrowsKind::None | ThrowsKind::Untyped => None,
    };
    let (fa, fg) = (f.is_async, f.is_generator);
    let coro = body.map(|body| {
        let block = Block {
            label: Some(rokc_hir::FN_BLOCK_LABEL.to_owned()),
            ..body.into_inner()
        };
        Expr {
            kind: match (fa, fg) {
                (true, true) => ExprKind::AsyncYieldBlock(block),
                (true, false) => ExprKind::AsyncBlock(block),
                _ => ExprKind::YieldBlock(block),
            },
            span,
        }
    });
    let is_main = root && f.name == "main" && f.is_async && !f.is_generator;
    let (is_async, is_gen) = (f.is_async, f.is_generator);
    f.is_async = false;
    f.is_generator = false;
    if is_async && !is_gen {
        f.attrs.push(rokc_ast::Attribute {
            path: vec![rokc_ast::CORO_FN_ATTR.to_owned()],
            body: rokc_ast::AttrBody::Empty,
            inner: false,
            span,
        });
    }
    if is_main {
        // The body is the executor's top-level task. The future's
        // type comes from the signature, so a non-throwing body still fits
        // `try_block_on::<T, E>`.
        let (runner, generics, throws) = match err {
            Some(e) => (
                "try_block_on",
                vec![val.clone(), e.clone()],
                ThrowsKind::Typed(vec![e]),
            ),
            None => ("block_on", Vec::new(), ThrowsKind::None),
        };
        let call = Expr {
            kind: ExprKind::Call {
                callee: Box::new(Expr {
                    kind: ExprKind::Path(path(&["rok", "core", "task", runner], generics, span)),
                    span,
                }),
                args: coro
                    .into_iter()
                    .map(rokc_ast::CallArg::Positional)
                    .collect(),
                trailing_closure: None,
            },
            span,
        };
        let call = if matches!(throws, ThrowsKind::None) {
            call
        } else {
            Expr {
                kind: ExprKind::Try {
                    kind: TryKind::Plain,
                    operand: Box::new(call),
                },
                span,
            }
        };
        f.throws = throws;
        f.ret_ty = Some(val);
        f.body = Some(trailing(call, span).into());
        return;
    }
    let module = if is_gen && !is_async {
        "iter"
    } else if is_async && !is_gen {
        "future"
    } else {
        "stream"
    };
    let name = match (module, err.is_some()) {
        ("iter", false) => "GenIter",
        ("iter", true) => "TryGenIter",
        ("future", false) => "AsyncFuture",
        ("future", true) => "TryAsyncFuture",
        (_, false) => "AsyncStream",
        (_, true) => "TryAsyncStream",
    };
    let mut args = vec![val];
    args.extend(err);
    f.ret_ty = (!infer).then(|| Ty {
        kind: TyKind::Path(path(&["rok", "core", module, name], args, span)),
        span,
    });
    f.body = coro.map(|c| trailing(c, span).into());
}

fn trailing(e: Expr, span: Span) -> Block {
    Block {
        stmts: Vec::<Stmt>::new(),
        trailing: Some(Box::new(e)),
        span,
        label: None,
    }
}

/// Top-level statements with an `await` (or `for await`)
/// outside any `async` block / closure make the implicit `main` async.
pub fn has_top_level_await(stmts: &[Stmt]) -> bool {
    struct V {
        depth: u32,
        found: bool,
    }
    impl rokc_hir::walk::AstVisitor for V {
        fn expr(&mut self, e: &Expr) {
            if self.depth == 0
                && matches!(
                    e.kind,
                    ExprKind::Await(_) | ExprKind::For { is_await: true, .. }
                )
            {
                self.found = true;
            }
        }
        fn enter_body(&mut self) {
            self.depth += 1;
        }
        fn leave_body(&mut self) {
            self.depth -= 1;
        }
    }
    let mut v = V {
        depth: 0,
        found: false,
    };
    for s in stmts {
        // Items (`fn` inside the top level) are their own bodies.
        if matches!(s.kind, StmtKind::Item(_)) {
            continue;
        }
        rokc_hir::walk::walk_stmt(&mut v, s);
    }
    v.found
}
