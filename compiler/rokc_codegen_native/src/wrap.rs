//! Functions codegen makes on its own: the C ABI entries of `extern "C"`
//! definitions, the C `main`, lazy static initializers and the GC type
//! registration (the MLIR emitter's `emit_c_entry`, `emit_main_wrapper`,
//! `emit_lazy_init`, `emit_gc_support`).

use rokc_codegen_mlir::abi::{ArgAbi, FnAbi, RetAbi};
use rokc_ir_common::{FunctionId, PrimTy};
use rokc_mir::{MirFunction, MirTy};

use crate::cc;
use crate::lir::{Func, Inst, K, Loc, Mem, Part, SymId, V};
use crate::lower::{Ctx, FnLower, Sig};

/// An empty MIR function to hang a generated body on.
pub fn shell(name: &str) -> MirFunction {
    MirFunction {
        id: FunctionId(u32::MAX),
        def_id: None,
        name: name.to_owned(),
        params: Vec::new(),
        locals: Vec::new(),
        blocks: Vec::new(),
        param_tys: Vec::new(),
        ret_ty: MirTy::Unit,
        span: rokc_diagnostic::Span::default(),
        facts: rokc_ir_common::FnFacts::default(),
        strips: Vec::new(),
        pins: Vec::new(),
    }
}

impl FnLower<'_, '_> {
    /// The C ABI entry of an `extern "C" fn` definition:
    /// takes the arguments as C passes them, runs the rok body as a
    /// mutator again (`rok_native_reenter`), returns the C
    /// way.
    pub fn c_entry(mut self, sig: &Sig, abi: &FnAbi) -> Func {
        let t = self.c.t;
        let mut shapes = Vec::new();
        for (p, a) in sig.params.iter().zip(&abi.args) {
            shapes.push(match a {
                ArgAbi::Direct => cc::shape(p, t),
                ArgAbi::Coerce(c) => cc::coerced(c),
                ArgAbi::Indirect { byval: true } => {
                    let (size, align) = t.size_align(p);
                    cc::byval(size, align)
                }
                ArgAbi::Indirect { byval: false } => cc::coerced("!llvm.ptr"),
            });
        }
        let ret_shape = match &abi.ret {
            RetAbi::Direct if sig.ret != MirTy::Unit => Some(cc::shape(&sig.ret, t)),
            RetAbi::Coerce(c) => Some(cc::coerced(c)),
            _ => None,
        };
        let locs = cc::assign(
            self.c.conv,
            &shapes,
            ret_shape.as_ref(),
            abi.ret == RetAbi::Sret,
            None,
        );
        let sret = (abi.ret == RetAbi::Sret).then(|| {
            let m = self.tmp_k(K::I64);
            self.push(Inst::Param {
                part: Part {
                    loc: Loc::Sret,
                    k: K::I64,
                    signed: false,
                },
                dst: m,
            });
            m
        });
        // Every parameter stored first: what follows may use argument
        // registers as scratch.
        let mut raws = Vec::new();
        for (p, (parts, sh)) in sig.params.iter().zip(locs.params.iter().zip(&shapes)) {
            let size = t.size_align(p).0;
            let raw = self.tmp_bytes(size.next_multiple_of(8).max(16), 16);
            for (part, l) in parts.iter().zip(&sh.leaves) {
                self.push(Inst::Param {
                    part: *part,
                    dst: raw.at(l.off),
                });
            }
            raws.push(raw);
        }
        let mut vals = Vec::new();
        for ((p, a), raw) in sig.params.iter().zip(&abi.args).zip(raws) {
            let size = t.size_align(p).0;
            let v = match a {
                ArgAbi::Indirect { byval: false } => {
                    let m = self.tmp(p);
                    let Mem::Frame(s, _) = raw else {
                        unreachable!()
                    };
                    if size > 0 {
                        self.push(Inst::Copy {
                            dst: m,
                            src: Mem::Ind(s, 0),
                            size,
                        });
                    }
                    m
                }
                _ => raw,
            };
            vals.push(self.val_of(v, p));
        }
        let i32_ = MirTy::Prim(PrimTy::I32);
        let parked = self.tmp_k(K::I32);
        let reenter = self.c.sym("rok_native_reenter");
        self.call_sym(reenter, &[], &i32_, &[], Some(parked));
        let r = (sig.ret != MirTy::Unit).then(|| self.tmp(&sig.ret));
        self.call_body(sig, &vals, r);
        let resume = self.c.sym("rok_native_resume");
        let pv = self.val_of(parked, &i32_);
        self.call_sym(resume, &[i32_], &MirTy::Unit, &[pv], None);
        match (&abi.ret, r) {
            (RetAbi::Sret, Some(r)) => {
                let Some(Mem::Frame(s, _)) = sret else {
                    unreachable!()
                };
                let size = t.size_align(&sig.ret).0;
                self.push(Inst::Copy {
                    dst: Mem::Ind(s, 0),
                    src: r,
                    size,
                });
                self.push(crate::lower::sret_ret(s));
            }
            (_, Some(r)) => {
                let parts = locs.ret.unwrap_or_default();
                let sh = ret_shape.unwrap_or_else(|| cc::shape(&sig.ret, t));
                let src = if matches!(abi.ret, RetAbi::Coerce(_)) {
                    let size = t.size_align(&sig.ret).0;
                    let m = self.tmp_bytes(size.max(16), 16);
                    self.push(Inst::Zero {
                        dst: m,
                        size: size.max(16),
                    });
                    self.push(Inst::Copy {
                        dst: m,
                        src: r,
                        size,
                    });
                    m
                } else {
                    r
                };
                let vals = parts
                    .iter()
                    .zip(&sh.leaves)
                    .map(|(p, l)| (*p, V::M(src.at(l.off))))
                    .collect();
                self.push(Inst::Ret { vals });
            }
            _ => self.push(Inst::Ret { vals: Vec::new() }),
        }
        self.out
    }

    /// The C `main`: runtime up, types registered, the user
    /// `main`, runtime down; its result as the exit code.
    pub fn main_wrapper(mut self, user: SymId, ret: &MirTy, register: SymId) -> Func {
        let init = self.c.sym("rok_runtime_init");
        self.call_sym(init, &[], &MirTy::Unit, &[], None);
        self.call_sym(register, &[], &MirTy::Unit, &[], None);
        let r = (*ret != MirTy::Unit).then(|| self.tmp(ret));
        self.call_tls(user, ret, r);
        let down = self.c.sym("rok_runtime_shutdown");
        self.call_sym(down, &[], &MirTy::Unit, &[], None);
        let part = Part {
            loc: Loc::Int(0),
            k: K::I32,
            signed: true,
        };
        let code = match (ret, r) {
            (MirTy::Prim(p), Some(m)) if p.is_int() || *p == PrimTy::Bool => {
                let pw = self.c.t.pointer_width;
                let k = cc::prim_k(*p, pw);
                let signed = p.is_signed_int();
                let out = self.tmp_k(K::I32);
                self.push(Inst::Conv {
                    conv: crate::lir::Conv::Int { signed },
                    from: k,
                    to: K::I32,
                    dst: out,
                    a: V::M(m),
                });
                V::M(out)
            }
            (MirTy::Unit, _) => V::I(0),
            _ => {
                self.c.errors.push((
                    rokc_diagnostic::Span::DUMMY,
                    format!("unsupported in codegen: `main` returning `{ret:?}`"),
                ));
                V::I(0)
            }
        };
        self.push(Inst::Ret {
            vals: vec![(part, code)],
        });
        self.out
    }

    /// A REPL cell's entry: register its types and roots, run its `main`.
    pub fn repl_cell(mut self, user: SymId, ret: &MirTy, register: SymId) -> Func {
        self.call_sym(register, &[], &MirTy::Unit, &[], None);
        let r = (*ret != MirTy::Unit).then(|| self.tmp(ret));
        self.call_tls(user, ret, r);
        self.push(Inst::Ret { vals: Vec::new() });
        self.out
    }

    /// `@sym.init(at)`: runs the initializer once across threads (0 new,
    /// 1 running, 2 ready; `rok_static_init_begin` / `_end`); `at` is the
    /// reading code's location, for the panic of a recursive one.
    pub fn lazy_init(
        mut self,
        state: SymId,
        storage: SymId,
        got: bool,
        init: SymId,
        ty: &MirTy,
    ) -> Func {
        let w = MirTy::Prim(PrimTy::Usize);
        let str_ty = MirTy::Prim(PrimTy::Str);
        let shapes = [cc::shape(&str_ty, self.c.t)];
        let locs = cc::assign(self.c.conv, &shapes, None, false, None);
        let at = self.tmp_bytes(16, 16);
        for (part, l) in locs.params[0].iter().zip(&shapes[0].leaves) {
            self.push(Inst::Param {
                part: *part,
                dst: at.at(l.off),
            });
        }
        let p = self.tmp_k(K::I64);
        self.push(Inst::Sym {
            dst: p,
            sym: state,
            got,
        });
        let pv = self.val_of(p, &w);
        let ap = self.val_of(at, &w);
        let al = self.val_of(at.at(8), &w);
        let r = self.tmp_k(K::I32);
        let begin = self.c.sym("rok_static_init_begin");
        self.call_sym(
            begin,
            &[w.clone(), w.clone(), w.clone()],
            &MirTy::Prim(PrimTy::I32),
            &[pv, ap, al],
            Some(r),
        );
        let (run, done) = (self.label(), self.label());
        let ne = self.tmp_k(K::I8);
        self.push(Inst::Cmp {
            cc: crate::lir::Cc::Ne,
            k: K::I32,
            signed: false,
            dst: ne,
            a: V::M(r),
            b: V::I(0),
        });
        self.push(Inst::Br {
            c: V::M(ne),
            t: run,
            f: done,
        });
        self.push(Inst::Label(run));
        let v = (*ty != MirTy::Unit).then(|| self.tmp(ty));
        self.call_tls(init, ty, v);
        if let Some(v) = v {
            let a = self.tmp_k(K::I64);
            self.push(Inst::Sym {
                dst: a,
                sym: storage,
                got,
            });
            let Mem::Frame(s, _) = a else { unreachable!() };
            let size = self.c.t.size_align(ty).0;
            if size > 0 {
                self.push(Inst::Copy {
                    dst: Mem::Ind(s, 0),
                    src: v,
                    size,
                });
            }
        }
        let end = self.c.sym("rok_static_init_end");
        self.call_sym(end, &[w], &MirTy::Unit, &[pv], None);
        self.push(Inst::Jmp(done));
        self.push(Inst::Label(done));
        self.push(Inst::Ret { vals: Vec::new() });
        self.out
    }

    /// `rok.register_types`: the `ScanTable`s (by symbol and type id),
    /// the static roots (`(symbol, local, offset)`), and the prebuilt
    /// dependencies' own registrations.
    pub fn register_types(
        mut self,
        tables: &[(u32, SymId)],
        roots: &[(SymId, bool, u32)],
        deps: &[SymId],
    ) -> Func {
        let w = MirTy::Prim(PrimTy::Usize);
        let i32_ = MirTy::Prim(PrimTy::I32);
        let reg = self.c.sym("rok_gc_register_type");
        for &(id, sym) in tables {
            let p = self.tmp_k(K::I64);
            self.push(Inst::Sym {
                dst: p,
                sym,
                got: false,
            });
            let pv = self.val_of(p, &w);
            let idv = Self::imm(i64::from(id), K::I32);
            self.call_sym(
                reg,
                &[i32_.clone(), w.clone()],
                &MirTy::Unit,
                &[idv, pv],
                None,
            );
        }
        let add = self.c.sym("rok_gc_add_root");
        for &(sym, local, off) in roots {
            let p = self.tmp_k(K::I64);
            self.push(Inst::Sym {
                dst: p,
                sym,
                got: !local,
            });
            if off != 0 {
                self.push(Inst::Bin {
                    op: crate::lir::Bin::Add,
                    k: K::I64,
                    signed: false,
                    dst: p,
                    a: V::M(p),
                    b: V::I(i64::from(off)),
                });
            }
            let pv = self.val_of(p, &w);
            self.call_sym(add, std::slice::from_ref(&w), &MirTy::Unit, &[pv], None);
        }
        for &d in deps {
            self.call_sym(d, &[], &MirTy::Unit, &[], None);
        }
        self.push(Inst::Ret { vals: Vec::new() });
        self.out
    }
}

/// Lower generated function `name` with `build`; under `-g` a
/// subprogram at `span` (a dummy one: line 0 of the unit's file).
pub fn generated(
    c: &mut Ctx<'_>,
    name: &str,
    sym: SymId,
    span: rokc_diagnostic::Span,
    build: impl FnOnce(FnLower<'_, '_>) -> Func,
) -> Func {
    let shell = shell(name);
    let mut f = build(FnLower::new(c, &shell, sym));
    if c.debug.is_some() {
        f.debug = Some(crate::lir::FnDebug {
            name: name.to_owned(),
            file: if span.line == 0 { 0 } else { span.file },
            line: span.line,
            scopes: Vec::new(),
            vars: Vec::new(),
            ret: None,
        });
    }
    f
}
