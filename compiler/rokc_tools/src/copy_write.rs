//! `write_to_copy`: a field write on a copy nothing reads
//! afterwards. Structs are values, so by-value parameters,
//! `for` bindings and `let x = xs[i]` hold copies.
//!
//! Reported when no read of the variable can follow: none later, and none
//! in a loop around the write that the variable outlives. A read feeding
//! only a write to the same variable (`p.x = p.x + 1`) counts only if that
//! write is read. Variables a closure or coroutine mentions are skipped.

use std::collections::{HashMap, HashSet};

use rokc_ast::{Expr, ExprKind, FnDef, PatternKind, Stmt, StmtKind};
use rokc_diagnostic::Span;
use rokc_hir::walk::{AstVisitor, walk_fn, walk_stmt};

/// A lost write: where, the headline, the fix.
pub struct Lost {
    pub span: Span,
    pub msg: String,
    pub help: String,
}

enum Kind {
    /// `self`, the parameter of that index.
    Receiver(usize),
    /// The parameter of that index.
    Param(usize),
    /// A `for` binding over `iter` (as written).
    Loop(String),
    /// A `let` of an element read (`xs[i]`, as written).
    Element(String),
}

struct Write {
    /// Where it takes effect: the end of its statement.
    at: u32,
    span: Span,
    /// The place and the statement, as written.
    target: String,
    text: String,
    /// The right side's extent, when it only computes the stored value.
    pure_rhs: Option<(u32, u32)>,
}

struct Var {
    name: String,
    kind: Option<Kind>,
    decl: u32,
    depth: u32,
    reads: Vec<u32>,
    writes: Vec<Write>,
    escaped: bool,
}

#[derive(Default)]
struct V {
    /// The function walked: its name, its signature, and its by-value
    /// parameters by span.
    fn_name: String,
    sig: Option<FnDef>,
    params: HashMap<(u32, u32), Kind>,
    loop_binds: HashMap<(u32, u32), String>,
    element_lets: HashMap<(u32, u32), String>,
    /// Roots of write targets: not reads.
    skip: HashSet<(u32, u32)>,
    vars: Vec<Var>,
    scopes: Vec<Vec<usize>>,
    depth: u32,
    loops: Vec<(u32, u32)>,
    out: Vec<Lost>,
}

const fn key(s: Span) -> (u32, u32) {
    (s.file, s.start)
}

/// The lost writes of `f`'s body.
pub fn check_fn(f: &FnDef) -> Vec<Lost> {
    if f.body.is_none() {
        return Vec::new();
    }
    let mut v = V {
        fn_name: rokc_core::local::leaf(&f.name).into_owned(),
        sig: Some(f.signature()),
        ..V::default()
    };
    for (i, p) in f
        .params
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.by_ref && !p.variadic)
    {
        let kind = if p.name == "self" {
            Kind::Receiver(i)
        } else {
            Kind::Param(i)
        };
        v.params.insert(key(p.span), kind);
    }
    walk_fn(&mut v, f);
    v.out
}

impl V {
    /// The signature taking parameter `i` by `ref`.
    fn by_ref(&self, i: usize) -> String {
        let Some(mut sig) = self.sig.clone() else {
            return String::new();
        };
        sig.params[i].by_ref = true;
        sig.name.clone_from(&self.fn_name);
        rokc_ast::print::print_fn_sig(&sig)
    }
}

/// The lost writes of top-level statements.
pub fn check_stmts(stmts: &[Stmt]) -> Vec<Lost> {
    let mut v = V::default();
    v.push_scope();
    for s in stmts {
        walk_stmt(&mut v, s);
    }
    v.pop_scope();
    v.out
}

/// The variable `p.f.g` writes, and the root's span.
fn target_root(e: &Expr) -> Option<(&str, Span)> {
    let (ExprKind::Field(base, _) | ExprKind::TupleField(base, _)) = &e.kind else {
        return None;
    };
    let mut b = base.as_ref();
    loop {
        match &b.kind {
            ExprKind::Field(x, _) | ExprKind::TupleField(x, _) => b = x,
            ExprKind::Path(p) if p.segments.len() == 1 => {
                return Some((&p.segments[0].name, b.span));
            }
            ExprKind::SelfLower => return Some(("self", b.span)),
            _ => return None,
        }
    }
}

/// `e` computes a value from paths, fields, literals and operators only.
fn pure(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::SelfLower => true,
        ExprKind::Field(x, _) | ExprKind::TupleField(x, _) | ExprKind::Cast(x, _) => pure(x),
        ExprKind::OpPrefix { operand, .. } => pure(operand),
        ExprKind::OpInfix { lhs, rhs, .. } => pure(lhs) && pure(rhs),
        _ => false,
    }
}

/// `text` with the variable `name` (followed by `.`) replaced by `with`.
fn rename(text: &str, name: &str, with: &str) -> String {
    let pat = format!("{name}.");
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find(&pat) {
        let starts_word = rest[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '.'));
        out.push_str(&rest[..i]);
        out.push_str(if starts_word { with } else { name });
        out.push('.');
        rest = &rest[i + pat.len()..];
    }
    out.push_str(rest);
    out
}

impl V {
    fn resolve(&self, name: &str) -> Option<usize> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|s| s.iter().rev())
            .copied()
            .find(|&i| self.vars[i].name == name)
    }

    fn read(&mut self, name: &str, at: Span) {
        if self.skip.contains(&key(at)) {
            return;
        }
        if let Some(i) = self.resolve(name) {
            let depth = self.depth;
            let v = &mut self.vars[i];
            v.reads.push(at.start);
            v.escaped |= depth > v.depth;
        }
    }

    /// The first write of `v` no read can follow.
    fn lost(&self, v: &Var) -> Option<usize> {
        // A read feeding write `w` (inside its pure right side).
        let feeds = |r: u32| {
            v.writes
                .iter()
                .position(|w| w.pure_rhs.is_some_and(|(s, e)| s <= r && r < e))
        };
        let follows = |r: u32, w: &Write| {
            r > w.at
                || self
                    .loops
                    .iter()
                    .any(|&(s, e)| s > v.decl && s <= w.at && w.at <= e && s <= r && r <= e)
        };
        let mut seen = vec![false; v.writes.len()];
        loop {
            let mut changed = false;
            for (i, w) in v.writes.iter().enumerate() {
                if !seen[i]
                    && v.reads
                        .iter()
                        .any(|&r| follows(r, w) && feeds(r).is_none_or(|j| seen[j]))
                {
                    seen[i] = true;
                    changed = true;
                }
            }
            if !changed {
                return seen.iter().position(|s| !s);
            }
        }
    }

    fn report(&mut self, i: usize) {
        let v = &self.vars[i];
        let Some(kind) = v.kind.as_ref().filter(|_| !v.escaped) else {
            return;
        };
        let Some(w) = self.lost(v).map(|w| &v.writes[w]) else {
            return;
        };
        let (name, f, target) = (&v.name, &self.fn_name, &w.target);
        let (msg, help) = match kind {
            Kind::Receiver(param) => (
                format!(
                    "`{target}` is set on a copy of the receiver, so the change is lost when \
                     `{f}` returns"
                ),
                format!(
                    "take `ref self` to change the caller's value: `{}`",
                    self.by_ref(*param)
                ),
            ),
            Kind::Param(param) => (
                format!(
                    "`{target}` is set on a copy of the argument, so the change is lost when \
                     `{f}` returns"
                ),
                format!(
                    "take `ref {name}` to change the caller's value: `{}`, and call it \
                     with `ref` before the argument; or return `{name}`",
                    self.by_ref(*param)
                ),
            ),
            Kind::Loop(iter) => {
                let simple = !iter.contains(['(', '.', '[', ' ']);
                let (list, each) = if simple {
                    (iter.as_str(), rename(&w.text, name, &format!("{iter}[i]")))
                } else {
                    ("xs", rename(&w.text, name, "xs[i]"))
                };
                (
                    format!(
                        "`{target}` is set on a copy of the element, so `{iter}` does not change"
                    ),
                    format!(
                        "loop over the indexes to change the elements: \
                         `for i in 0..{list}.len() {{ {each}; }}`"
                    ),
                )
            }
            Kind::Element(from) => (
                format!("`{target}` is set on a copy of `{from}`, so `{from}` does not change"),
                format!("write it back afterwards: `{from} = {name};`"),
            ),
        };
        self.out.push(Lost {
            span: w.span,
            msg,
            help,
        });
    }
}

impl AstVisitor for V {
    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Path(p) if p.segments.len() == 1 => {
                let n = p.segments[0].name.clone();
                self.read(&n, e.span);
            }
            ExprKind::SelfLower => self.read("self", e.span),
            ExprKind::For { pat, iter, .. } => {
                self.loops.push((e.span.start, e.span.end));
                if let PatternKind::Binding { sub: None, .. } = &pat.kind {
                    self.loop_binds
                        .insert(key(pat.span), rokc_ast::print::print_expr(iter));
                }
            }
            ExprKind::While { .. } | ExprKind::Loop { .. } => {
                self.loops.push((e.span.start, e.span.end));
            }
            _ => {}
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        let (target, value) = match &s.kind {
            StmtKind::Let {
                init:
                    init @ Expr {
                        kind: ExprKind::Index(..),
                        ..
                    },
                ..
            } => {
                self.element_lets
                    .insert(key(s.span), rokc_ast::print::print_expr(init));
                return;
            }
            StmtKind::Assign(t, v)
            | StmtKind::CompoundAssign {
                target: t,
                value: v,
                ..
            } => (t, v),
            _ => return,
        };
        let Some((name, root)) = target_root(target) else {
            return;
        };
        self.skip.insert(key(root));
        let Some(i) = self.resolve(name) else {
            return;
        };
        let text = rokc_ast::print::print_stmt(s);
        self.vars[i].writes.push(Write {
            at: s.span.end.max(value.span.end),
            span: target.span,
            target: rokc_ast::print::print_expr(target),
            text: text.trim_end_matches(';').to_owned(),
            pure_rhs: pure(value).then_some((value.span.start, value.span.end)),
        });
    }

    fn bind_at(&mut self, name: &str, at: Span) {
        let k = key(at);
        let kind = self
            .params
            .remove(&k)
            .or_else(|| self.loop_binds.remove(&k).map(Kind::Loop))
            .or_else(|| self.element_lets.remove(&k).map(Kind::Element));
        self.vars.push(Var {
            name: name.to_owned(),
            kind,
            decl: at.start,
            depth: self.depth,
            reads: Vec::new(),
            writes: Vec::new(),
            escaped: false,
        });
        let i = self.vars.len() - 1;
        if let Some(s) = self.scopes.last_mut() {
            s.push(i);
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        for i in self.scopes.pop().unwrap_or_default() {
            self.report(i);
        }
    }

    fn enter_body(&mut self) {
        self.depth += 1;
    }

    fn leave_body(&mut self) {
        self.depth -= 1;
    }
}
