//! In a coroutine a pointer to a local is valid only
//! until the next suspend, since each resume reloads the frame into new
//! slots. A `rok::bare::ptr` local built from such an address (or another
//! such local) used after a suspend it was live at is an error. Read off
//! the HIR in order (branches merge, loops walked twice); pointers in
//! temporaries or fields aren't seen.

use std::collections::HashSet;

use rokc_diagnostic::{Code, Span};
use rokc_ir_common::LocalId;

use super::send_check::{Child, each_child, root};
use super::{Env, LowerCtx};
use crate::{HirBlock, HirExpr, HirExprKind, HirFunction, HirStmt, HirTy};

pub(super) fn check(env: &Env<'_>, functions: &[HirFunction]) {
    let extras = env.sink.extras.borrow();
    let no_subst = std::collections::HashMap::new();
    let ctx = LowerCtx::new(env, &no_subst, None, Span::DUMMY);
    let mut seen = HashSet::new();
    for f in functions.iter().chain(extras.iter()) {
        let Some(body) = f.body.as_ref().filter(|_| f.is_async || f.is_generator) else {
            continue;
        };
        let is_ptr = |t: &HirTy| ptr_ty(env, t);
        let ptr_local: HashSet<LocalId> = f
            .locals
            .iter()
            .filter(|l| is_ptr(&l.ty))
            .map(|l| l.id)
            .collect();
        let mut w = Walk {
            ptr_local: &ptr_local,
            frame: HashSet::new(),
            crossed: HashSet::new(),
            uses: Vec::new(),
        };
        w.block(body);
        for (l, at) in w.uses {
            if !seen.insert((at.file, at.start)) {
                continue;
            }
            let name = f
                .locals
                .iter()
                .find(|x| x.id == l)
                .and_then(|x| x.name.clone())
                .unwrap_or_else(|| "a pointer".to_owned());
            ctx.emit_error_at(
                Code::COROUTINE,
                at,
                format!(
                    "`{name}` points to a local of this coroutine and is used after a \
                     suspension point: such a pointer is valid only until the next `await` / \
                     `yield`"
                ),
            );
        }
    }
}

/// A `rok::bare::ptr` pointer type, or a sum with one as a member.
fn ptr_ty(env: &Env<'_>, t: &HirTy) -> bool {
    match t {
        HirTy::Struct(id) => env.defs.structs.borrow()[id.0 as usize]
            .name
            .starts_with("rok::bare::ptr::"),
        HirTy::Sum(ms) => ms.iter().any(|m| ptr_ty(env, m)),
        _ => false,
    }
}

struct Walk<'a> {
    /// Locals of a pointer type.
    ptr_local: &'a HashSet<LocalId>,
    /// Those that may point into the frame now.
    frame: HashSet<LocalId>,
    /// Those that were in `frame` at a suspend point walked since.
    crossed: HashSet<LocalId>,
    /// Uses of a crossed local.
    uses: Vec<(LocalId, Span)>,
}

impl Walk<'_> {
    /// `e` may yield an address into the frame.
    fn carries(&self, e: &HirExpr) -> bool {
        let mut found = false;
        crate::each_expr(e, &mut |x| match &x.kind {
            HirExprKind::AddrOf { place } => found |= root(place).is_some(),
            HirExprKind::Local(l) => found |= self.frame.contains(l),
            _ => {}
        });
        found
    }

    fn set(&mut self, l: LocalId, value: &HirExpr) {
        self.crossed.remove(&l);
        if self.ptr_local.contains(&l) && self.carries(value) {
            self.frame.insert(l);
        } else {
            self.frame.remove(&l);
        }
    }

    fn block(&mut self, b: &HirBlock) {
        for s in &b.stmts {
            match s {
                HirStmt::Let { local, init } => {
                    self.expr(init);
                    self.set(*local, init);
                }
                HirStmt::Assign { local, value } => {
                    self.expr(value);
                    self.set(*local, value);
                }
                HirStmt::ArrayAssign {
                    array_local,
                    index,
                    value,
                    ..
                } => {
                    self.expr(index);
                    self.expr(value);
                    self.use_local(*array_local, value.span);
                }
                HirStmt::StaticAssign { value: e, .. }
                | HirStmt::Expr(e)
                | HirStmt::Panic(e)
                | HirStmt::Return(Some(e))
                | HirStmt::Break { value: Some(e), .. } => self.expr(e),
                _ => {}
            }
        }
        if let Some(t) = &b.trailing {
            self.expr(t);
        }
    }

    fn use_local(&mut self, l: LocalId, at: Span) {
        if self.crossed.remove(&l) {
            self.uses.push((l, at));
        }
    }

    /// Walk each branch from the current state; afterwards any of them
    /// may have run.
    fn branches<'e>(&mut self, arms: impl Iterator<Item = Child<'e>>) {
        let (frame0, crossed0) = (self.frame.clone(), self.crossed.clone());
        let (mut frame, mut crossed) = (frame0.clone(), crossed0.clone());
        for c in arms {
            self.frame.clone_from(&frame0);
            self.crossed.clone_from(&crossed0);
            self.child(c);
            frame.extend(self.frame.drain());
            crossed.extend(self.crossed.drain());
        }
        // A use already reported in one branch stays reported.
        crossed.retain(|l| !self.uses.iter().any(|(u, _)| u == l));
        self.frame = frame;
        self.crossed = crossed;
    }

    fn child(&mut self, c: Child<'_>) {
        match c {
            Child::E(x) => self.expr(x),
            Child::B(b) => self.block(b),
        }
    }

    fn expr(&mut self, e: &HirExpr) {
        match &e.kind {
            HirExprKind::Local(l) => self.use_local(*l, e.span),
            HirExprKind::Suspend { value, is_final } => {
                self.expr(value);
                if !is_final {
                    self.crossed.extend(self.frame.iter().copied());
                }
            }
            HirExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                self.expr(cond);
                let else_arm = else_branch.as_deref().map(Child::E);
                self.branches(std::iter::once(Child::B(then_block)).chain(else_arm));
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms {
                    a.pat.each_binding(&mut |l| {
                        self.frame.remove(&l);
                    });
                    if let Some(g) = &a.guard {
                        self.expr(g);
                    }
                }
                self.branches(arms.iter().map(|a| Child::E(&a.body)));
            }
            HirExprKind::While { cond, body, .. } => {
                for _ in 0..2 {
                    self.expr(cond);
                    self.branches(std::iter::once(Child::B(body)));
                }
            }
            HirExprKind::Loop { body, .. } => {
                self.block(body);
                self.block(body);
            }
            _ => each_child(e, &mut |c| self.child(c)),
        }
    }
}
