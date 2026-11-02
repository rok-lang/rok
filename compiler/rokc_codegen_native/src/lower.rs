//! MIR → LIR, one function at a time, with the MLIR emitter's semantics
//! construct by construct (layouts, GC frames, barriers, allocation, polls,
//! checks, panic messages), so programs behave the same on both backends.

use std::collections::{HashMap, HashSet};

use rokc_codegen_mlir::abi::{ArgAbi, FnAbi, RetAbi, rok_by_ref};
use rokc_codegen_mlir::emit::{
    cannot_overflow, const_bytes, const_int, gc_object_size, scan_table,
};
use rokc_ir_common::{Abi, ConstVal, FunctionId, LocalId, PrimOp, PrimTy};
use rokc_mir::gc_abi::{
    CARD_SHIFT, FLAG_CARDED, FLAG_UNLOGGED, GC_FLAGS_OFFSET, GC_WORD_OFFSET, HEADER_SIZE,
    LOS_THRESHOLD, PLAIN_TYPE_ID,
};
use rokc_mir::tls_abi;
use rokc_mir::{BasicBlock, MirFunction, MirTy, Operand, RValue, Statement, Target, Terminator};

use crate::cc::{self, Shape};
use crate::lir::{
    Bin, Callee, Cc, Conv, Func, Inst, K, Label, Loc, Mem, Part, Rmw, Slot, SymId, Un, V,
};
use crate::obj::Obj;

/// How a call reaches a module function (the MLIR emitter's `FnSig`).
pub struct Sig {
    pub sym: SymId,
    pub params: Vec<MirTy>,
    pub ret: MirTy,
    /// A C declaration: its C lowering.
    pub c: Option<FnAbi>,
    /// An `extern "C" fn` definition: its C ABI entry.
    pub c_entry: Option<SymId>,
    /// Defined in this object (else reached through the GOT).
    pub local: bool,
    /// The rok body takes the caller's `rok_tls` address last
    /// (`FnFacts::takes_tls`).
    pub tls: bool,
}

/// Module-wide state the functions share.
pub struct Ctx<'a> {
    pub t: &'a Target,
    /// The compile's source files (panic locations).
    pub sources: &'a rokc_diagnostic::SourceMap,
    pub conv: cc::Conv,
    pub obj: &'a mut Obj,
    pub fns: &'a HashMap<FunctionId, Sig>,
    pub pkgs: &'a rokc_codegen_mlir::mangle::Pkgs,
    /// `ScanTable` of each GC type (index + `gc_type_base`).
    pub gc_types: &'a mut Vec<Vec<u32>>,
    pub gc_type_base: u32,
    /// In a codegen unit, the user package's hash in its GC type id
    /// symbols (`Lower::gc_tid`).
    pub gc_pkg: Option<&'a str>,
    pub overflow_checks: bool,
    /// `-g`: describe functions and their locals (`MirModule::debug`).
    pub debug: Option<&'a rokc_mir::DebugTypes>,
    /// Statics this object defines (else through the GOT).
    pub local_statics: &'a HashSet<String>,
    /// Functions that never reach a safepoint (`quiet_functions`).
    pub quiet: &'a HashSet<FunctionId>,
    /// Functions a call cycle can enter: they poll on entry
    /// (`reentrant_functions`).
    pub reentrant: &'a HashSet<FunctionId>,
    /// Unsupported constructs, at their function (dummy outside one).
    pub errors: Vec<(rokc_diagnostic::Span, String)>,
    /// Functions whose function-value thunk (`thunk`) this object needs.
    pub thunks: std::collections::BTreeSet<FunctionId>,
}

impl Ctx<'_> {
    /// The index of `ty` objects' (`array`: buffers of them) GC type.
    fn gc_type(&mut self, ty: &MirTy, array: bool) -> usize {
        let key = scan_table(ty, array, self.t);
        self.gc_types
            .iter()
            .position(|k| *k == key)
            .unwrap_or_else(|| {
                self.gc_types.push(key);
                self.gc_types.len() - 1
            })
    }

    /// The symbol of a string literal's bytes (NUL-terminated).
    pub fn str_sym(&mut self, text: &str) -> SymId {
        self.obj.bytes(text.as_bytes())
    }

    pub fn sym(&mut self, name: &str) -> SymId {
        self.obj.sym(name)
    }
}

/// A typed operand: a scalar value, or the memory of an aggregate (`str`
/// included).
#[derive(Clone, Copy, Debug)]
pub enum Val {
    S(V, K),
    M(Mem),
    Unit,
}

/// A panic site: message, location (a literal, or the
/// `#[track_caller]` parameter).
#[derive(Clone, PartialEq, Eq, Hash)]
struct Panic {
    msg: &'static str,
    at: String,
}

pub struct FnLower<'c, 'a> {
    pub(crate) c: &'c mut Ctx<'a>,
    f: &'c MirFunction,
    pub out: Func,
    slots: Vec<Option<Slot>>,
    block_label: HashMap<u32, Label>,
    block_params: HashMap<u32, Vec<LocalId>>,
    /// This thread's `rok_tls` block address (filled in the prologue).
    tls: Option<Slot>,
    /// In `lower` (not a generated function): the prologue fills `tls`.
    lowering: bool,
    /// The shadow-stack frame: its slot and the TLS slot of the top.
    gc_frame: Option<(Slot, Mem)>,
    rooted: HashSet<u32>,
    deaths: rokc_mir::Deaths,
    back_edges: HashSet<u32>,
    panics: Vec<(Panic, Label)>,
    at: rokc_diagnostic::Span,
    /// The hidden result pointer's slot.
    sret: Option<Slot>,
    ret_parts: Option<Vec<Part>>,
    /// `rokc_mir::addressed`.
    addressed: HashSet<LocalId>,
}

const fn const_ty(c: &ConstVal) -> MirTy {
    match c {
        ConstVal::Int { ty, .. } | ConstVal::UInt { ty, .. } | ConstVal::Float { ty, .. } => {
            MirTy::Prim(*ty)
        }
        ConstVal::Bool(_) => MirTy::Prim(PrimTy::Bool),
        ConstVal::Str(_) => MirTy::Prim(PrimTy::Str),
        ConstVal::Char(_) => MirTy::Prim(PrimTy::Char),
        ConstVal::Unit | ConstVal::None | ConstVal::Bytes(_) => MirTy::Unit,
    }
}

/// The MLIR type class of a primitive: equal classes need no cast.
const fn prim_class(p: PrimTy) -> u8 {
    match p {
        PrimTy::I8 | PrimTy::U8 | PrimTy::CChar => 1,
        PrimTy::I16 | PrimTy::U16 => 2,
        PrimTy::I32 | PrimTy::U32 | PrimTy::Char => 3,
        PrimTy::I64 | PrimTy::U64 | PrimTy::Isize | PrimTy::Usize => 4,
        PrimTy::F32 => 5,
        PrimTy::F64 => 6,
        PrimTy::Bool => 7,
        PrimTy::Str => 8,
    }
}

fn struct_fields(ty: &MirTy) -> &[MirTy] {
    match ty {
        MirTy::Struct(_, fs) | MirTy::Tuple(fs) => fs,
        _ => &[],
    }
}

/// `(size, align)` of a slot of `ty`.
fn slot_layout(ty: &MirTy, t: &Target) -> (u32, u32) {
    let (s, a) = t.size_align(ty);
    (s.max(1), a.max(1))
}

impl<'c, 'a> FnLower<'c, 'a> {
    pub fn new(c: &'c mut Ctx<'a>, f: &'c MirFunction, sym: SymId) -> Self {
        let mut out = Func {
            sym,
            ..Func::default()
        };
        let slots = f
            .locals
            .iter()
            .map(|l| {
                (l.ty != MirTy::Unit).then(|| {
                    let (s, a) = slot_layout(&l.ty, c.t);
                    out.slot(s, a)
                })
            })
            .collect();
        let back_edges = rokc_codegen_mlir::emit::polled_back_edges(f, c.quiet, c.pkgs, c.t)
            .into_keys()
            .collect();
        let mut block_label = HashMap::new();
        let mut block_params = HashMap::new();
        for b in &f.blocks {
            block_label.insert(b.id.0, out.label());
            block_params.insert(b.id.0, b.params.clone());
        }
        Self {
            c,
            f,
            out,
            slots,
            block_label,
            block_params,
            tls: None,
            lowering: false,
            gc_frame: None,
            rooted: HashSet::new(),
            deaths: rokc_mir::Deaths::default(),
            back_edges,
            panics: Vec::new(),
            at: f.span,
            sret: None,
            ret_parts: None,
            addressed: rokc_mir::addressed(f),
        }
    }

    fn err(&mut self, what: String) {
        let msg = format!("in `{}`: unsupported in codegen: {what}", self.f.name);
        self.c.errors.push((self.f.span, msg));
    }

    fn pw(&self) -> u32 {
        self.c.t.pointer_width
    }

    fn word(&self) -> K {
        if self.pw() == 32 { K::I32 } else { K::I64 }
    }

    fn local_ty(&self, l: LocalId) -> MirTy {
        self.f
            .locals
            .get(l.0 as usize)
            .map_or(MirTy::Any, |d| d.ty.clone())
    }

    fn local_mem(&self, l: LocalId) -> Option<Mem> {
        self.slots
            .get(l.0 as usize)
            .copied()
            .flatten()
            .map(|s| Mem::Frame(s, 0))
    }

    pub(crate) fn tmp(&mut self, ty: &MirTy) -> Mem {
        let (s, a) = slot_layout(ty, self.c.t);
        Mem::Frame(self.out.slot(s, a), 0)
    }

    pub(crate) fn tmp_k(&mut self, k: K) -> Mem {
        Mem::Frame(self.out.slot(k.size(), k.size()), 0)
    }

    pub(crate) fn tmp_bytes(&mut self, size: u32, align: u32) -> Mem {
        Mem::Frame(self.out.slot(size, align), 0)
    }

    pub(crate) fn push(&mut self, i: Inst) {
        self.out.push(i);
    }

    pub(crate) fn label(&mut self) -> Label {
        self.out.label()
    }

    fn word_tmp(&mut self) -> Mem {
        let k = self.word();
        self.tmp_k(k)
    }

    /// The value of type `ty` in `m`.
    pub(crate) fn val_of(&self, m: Mem, ty: &MirTy) -> Val {
        match cc::scalar_k(ty, self.c.t) {
            Some(k) => Val::S(V::M(m), k),
            None if *ty == MirTy::Unit => Val::Unit,
            None => Val::M(m),
        }
    }

    pub(crate) const fn imm(v: i64, k: K) -> Val {
        Val::S(V::I(v), k)
    }

    /// A scalar `V` held in memory (`Ind` needs its pointer in a slot).
    fn in_slot(&mut self, v: V, k: K) -> Slot {
        if let V::M(Mem::Frame(s, 0)) = v {
            return s;
        }
        let m = self.tmp_k(k);
        self.push(Inst::Mov { k, dst: m, src: v });
        let Mem::Frame(s, _) = m else { unreachable!() };
        s
    }

    /// Slot `slot` of this thread's TLS block `rok_tls`:
    /// the block's address is taken once, in the prologue.
    fn tls(&mut self, slot: tls_abi::Slot) -> Mem {
        let out = &mut self.out;
        let base = *self.tls.get_or_insert_with(|| out.slot(8, 8));
        let off = slot.offset(self.pw() / 8);
        Mem::Ind(base, i32::try_from(off).expect("TLS offset"))
    }

    /// The `rok_tls` address a rok body takes last: this function's
    /// (filled in the prologue), else (a generated function) read here.
    fn tls_val(&mut self) -> Val {
        let base = if let Some(s) = self.tls {
            s
        } else {
            let s = self.out.slot(8, 8);
            self.tls = Some(s);
            if !self.lowering {
                let sym = self.c.sym("rok_tls");
                self.push(Inst::Tls {
                    dst: Mem::Frame(s, 0),
                    sym,
                });
            }
            s
        };
        Val::S(V::M(Mem::Frame(base, 0)), K::I64)
    }

    fn call_rt(&mut self, name: &str, args: &[(V, K)], ret: Option<(Mem, K)>) {
        let sym = self.c.sym(name);
        let shapes: Vec<Shape> = args
            .iter()
            .map(|(_, k)| Shape {
                leaves: vec![cc::Leaf {
                    off: 0,
                    k: *k,
                    signed: false,
                }],
                array: false,
                align: k.size(),
                byval: false,
                aggregate: false,
            })
            .collect();
        let rs = ret.map(|(_, k)| Shape {
            leaves: vec![cc::Leaf {
                off: 0,
                k,
                signed: false,
            }],
            array: false,
            align: k.size(),
            byval: false,
            aggregate: false,
        });
        let locs = cc::assign(self.c.conv, &shapes, rs.as_ref(), false, None);
        let args = locs
            .params
            .iter()
            .zip(args)
            .map(|(p, (v, _))| (p[0], *v))
            .collect();
        let rets = match (ret, locs.ret) {
            (Some((m, _)), Some(p)) => vec![(p[0], m)],
            _ => Vec::new(),
        };
        self.push(Inst::Call {
            callee: Callee::Sym(sym),
            args,
            rets,
            sret: None,
            variadic: false,
        });
    }

    /// Safepoint poll: `rok_runtime_yield` when this
    /// thread's yield flag is set.
    fn poll(&mut self) {
        let flag = self.tls(tls_abi::YIELD_FLAG);
        let v = self.tmp_k(K::I8);
        self.push(Inst::Mov {
            k: K::I8,
            dst: v,
            src: V::M(flag),
        });
        let (y, run) = (self.label(), self.label());
        self.push(Inst::Br {
            c: V::M(v),
            t: y,
            f: run,
        });
        self.push(Inst::Label(y));
        self.call_rt("rok_runtime_yield", &[], None);
        self.push(Inst::Jmp(run));
        self.push(Inst::Label(run));
    }

    /// Into (`enter`) or out of a C call.
    fn native_edge(&mut self, enter: bool) {
        let (state, slow) = if enter {
            (2, "rok_enter_native")
        } else {
            (0, "rok_leave_native")
        };
        let st = self.tls(tls_abi::MUTATOR_STATE);
        let fl = self.tls(tls_abi::YIELD_FLAG);
        self.push(Inst::AtomicStore {
            k: K::I8,
            ptr: st,
            src: V::I(state),
        });
        let v = self.tmp_k(K::I8);
        self.push(Inst::AtomicLoad {
            k: K::I8,
            dst: v,
            ptr: fl,
        });
        let (s, fast) = (self.label(), self.label());
        self.push(Inst::Br {
            c: V::M(v),
            t: s,
            f: fast,
        });
        self.push(Inst::Label(s));
        self.call_rt(slow, &[], None);
        self.push(Inst::Jmp(fast));
        self.push(Inst::Label(fast));
    }

    fn panic_label(&mut self, msg: &'static str) -> Label {
        let at = if self.f.facts.track_caller {
            "#caller".to_owned()
        } else {
            self.c.sources.location(self.at)
        };
        let p = Panic { msg, at };
        if let Some((_, l)) = self.panics.iter().find(|(q, _)| *q == p) {
            return *l;
        }
        let l = self.label();
        self.panics.push((p, l));
        l
    }

    /// Panic with `msg` when the `I8` `fail` is set.
    fn check(&mut self, fail: V, msg: &'static str) {
        let p = self.panic_label(msg);
        let ok = self.label();
        self.push(Inst::Br {
            c: fail,
            t: p,
            f: ok,
        });
        self.push(Inst::Label(ok));
    }

    fn cmp(&mut self, cc: Cc, k: K, signed: bool, a: V, b: V) -> V {
        let dst = self.tmp_k(K::I8);
        self.push(Inst::Cmp {
            cc,
            k,
            signed,
            dst,
            a,
            b,
        });
        V::M(dst)
    }

    fn str_lit(&mut self, text: &str, dst: Mem) {
        let sym = self.c.str_sym(text);
        let w = self.word();
        self.push(Inst::Sym {
            dst,
            sym,
            got: false,
        });
        self.push(Inst::Mov {
            k: w,
            dst: dst.at(self.pw() / 8),
            src: V::I(text.len() as i64),
        });
    }

    fn emit_panics(&mut self) {
        let str_ty = MirTy::Prim(PrimTy::Str);
        for (p, l) in std::mem::take(&mut self.panics) {
            self.push(Inst::Label(l));
            let msg = self.tmp(&str_ty);
            self.str_lit(p.msg, msg);
            let at = if p.at == "#caller" {
                let last = *self.f.params.last().expect("track_caller location");
                self.local_mem(last).expect("location slot")
            } else {
                let m = self.tmp(&str_ty);
                self.str_lit(&p.at, m);
                m
            };
            let sym = self.c.sym("rok_panic_at");
            self.call_runtime(
                sym,
                &[str_ty.clone(), str_ty.clone()],
                &MirTy::Unit,
                &[Val::M(msg), Val::M(at)],
                None,
            );
            self.push(Inst::Trap);
        }
    }

    /// `op` read at type `ty` (the emitter's `materialize_operand`).
    fn val(&mut self, op: &Operand, ty: &MirTy) -> Val {
        match op {
            Operand::Copy(id) => {
                let from = self.local_ty(*id);
                let Some(m) = self.local_mem(*id) else {
                    return if *ty == MirTy::Unit {
                        Val::Unit
                    } else {
                        self.zero_val(ty)
                    };
                };
                if let (MirTy::Prim(a), MirTy::Prim(b)) = (&from, ty) {
                    if prim_class(*a) != prim_class(*b) {
                        return self.cast(Val::S(V::M(m), cc::prim_k(*a, self.pw())), &from, ty);
                    }
                }
                match cc::scalar_k(&from, self.c.t) {
                    Some(k) => Val::S(V::M(m), k),
                    None => Val::M(m),
                }
            }
            Operand::Const(c) => self.const_val(c, ty),
        }
    }

    fn zero_val(&mut self, ty: &MirTy) -> Val {
        if *ty == MirTy::Unit {
            return Val::Unit;
        }
        if let Some(k) = cc::scalar_k(ty, self.c.t) {
            return Val::S(V::I(0), k);
        }
        let m = self.tmp(ty);
        let size = self.c.t.size_align(ty).0;
        self.push(Inst::Zero { dst: m, size });
        Val::M(m)
    }

    /// A constant at type `ty` (the emitter's `emit_const_assign`).
    fn const_val(&mut self, c: &ConstVal, ty: &MirTy) -> Val {
        let pw = self.pw();
        match c {
            ConstVal::Int { ty: p, .. } | ConstVal::UInt { ty: p, .. } => {
                let (bits, as_f64) = match c {
                    ConstVal::Int { value, .. } => (*value as i64, *value as f64),
                    ConstVal::UInt { value, .. } => (*value as i64, *value as f64),
                    _ => unreachable!(),
                };
                match ty {
                    MirTy::Prim(q) if q.is_float() => {
                        let b = if *q == PrimTy::F32 {
                            i64::from((as_f64 as f32).to_bits())
                        } else {
                            as_f64.to_bits() as i64
                        };
                        Val::S(V::I(b), cc::prim_k(*q, pw))
                    }
                    MirTy::Prim(q) if q.is_int() => Val::S(V::I(bits), cc::prim_k(*q, pw)),
                    MirTy::Any | MirTy::GcRef => Val::S(V::I(bits), self.word()),
                    _ => Val::S(V::I(bits), cc::prim_k(*p, pw)),
                }
            }
            ConstVal::Bool(b) => Val::S(V::I(i64::from(*b)), K::I8),
            ConstVal::Char(ch) => {
                let k = cc::scalar_k(ty, self.c.t).unwrap_or(K::I32);
                Val::S(V::I(i64::from(*ch)), k)
            }
            ConstVal::Float { bits, ty: p } => {
                let (q, b) = match ty {
                    MirTy::Prim(q) if q.is_float() && q != p => {
                        let v = if *p == PrimTy::F32 {
                            f64::from(f32::from_bits(*bits as u32))
                        } else {
                            f64::from_bits(*bits)
                        };
                        let b = if *q == PrimTy::F32 {
                            u64::from((v as f32).to_bits())
                        } else {
                            v.to_bits()
                        };
                        (*q, b)
                    }
                    _ => (*p, *bits),
                };
                Val::S(V::I(b as i64), cc::prim_k(q, pw))
            }
            ConstVal::Str(s) => {
                let m = self.tmp(&MirTy::Prim(PrimTy::Str));
                self.str_lit(s, m);
                Val::M(m)
            }
            ConstVal::Unit | ConstVal::None => self.zero_val(ty),
            ConstVal::Bytes(_) => unreachable!("MIR lowering expands byte arrays"),
        }
    }

    /// Write `v` (of type `ty`) to `dst`.
    fn store(&mut self, v: Val, ty: &MirTy, dst: Mem) {
        match v {
            Val::S(src, k) => self.push(Inst::Mov { k, dst, src }),
            Val::M(src) => {
                if src != dst {
                    let size = self.c.t.size_align(ty).0;
                    if size > 0 {
                        self.push(Inst::Copy { dst, src, size });
                    }
                }
            }
            Val::Unit => {}
        }
    }

    /// `v` in memory.
    fn mem_of(&mut self, v: Val, ty: &MirTy) -> Mem {
        match v {
            Val::M(m) | Val::S(V::M(m), _) => m,
            Val::S(src, k) => {
                let m = self.tmp_k(k);
                self.push(Inst::Mov { k, dst: m, src });
                m
            }
            Val::Unit => self.tmp(ty),
        }
    }

    fn scalar(&mut self, op: &Operand, ty: &MirTy) -> (V, K) {
        match self.val(op, ty) {
            Val::S(v, k) => (v, k),
            Val::M(m) => (V::M(m), cc::scalar_k(ty, self.c.t).unwrap_or(K::I64)),
            Val::Unit => (V::I(0), K::I8),
        }
    }

    fn op_ty(&self, op: &Operand) -> MirTy {
        match op {
            Operand::Copy(id) => self.local_ty(*id),
            Operand::Const(c) => const_ty(c),
        }
    }

    /// A value conversion (the emitter's `cast_value`).
    fn cast(&mut self, v: Val, from: &MirTy, to: &MirTy) -> Val {
        let pw = self.pw();
        if let (MirTy::Fn(..) | MirTy::GcRef, MirTy::Prim(_)) = (from, to) {
            return self.cast(v, &MirTy::Prim(PrimTy::Usize), to);
        }
        let (MirTy::Prim(fp), MirTy::Prim(tp)) = (from, to) else {
            if cc::scalar_k(from, self.c.t) == cc::scalar_k(to, self.c.t) {
                return v;
            }
            self.err(format!("cast from `{from:?}` to `{to:?}`"));
            return self.zero_val(to);
        };
        if prim_class(*fp) == prim_class(*tp) {
            return v;
        }
        let Val::S(a, fk) = v else {
            self.err(format!("cast from `{fp:?}` to `{tp:?}`"));
            return self.zero_val(to);
        };
        let tk = cc::prim_k(*tp, pw);
        let int_like = |p: &PrimTy| p.is_int() || *p == PrimTy::Bool;
        let conv = match (int_like(fp), int_like(tp), fp.is_float(), tp.is_float()) {
            (true, true, _, _) => {
                let w = |p: &PrimTy| {
                    if *p == PrimTy::Bool {
                        1
                    } else {
                        p.bit_width(pw)
                    }
                };
                if *tp == PrimTy::Bool && w(fp) > 1 {
                    Conv::ToBool
                } else {
                    Conv::Int {
                        signed: *fp != PrimTy::Bool && fp.is_signed_int(),
                    }
                }
            }
            (false, true, true, false) => Conv::FToI {
                signed: tp.is_signed_int(),
                bits: if *tp == PrimTy::Bool {
                    1
                } else {
                    tp.bit_width(pw)
                },
            },
            (true, false, false, true) => Conv::IToF {
                signed: fp.is_signed_int(),
            },
            (false, false, true, true) => Conv::FToF,
            _ => {
                self.err(format!("cast from `{fp:?}` to `{tp:?}`"));
                return self.zero_val(to);
            }
        };
        let dst = self.tmp_k(tk);
        self.push(Inst::Conv {
            conv,
            from: fk,
            to: tk,
            dst,
            a,
        });
        Val::S(V::M(dst), tk)
    }

    /// Bit-preserving move (the emitter's `bitcast_value`) into `dst`.
    fn bitcast_into(&mut self, v: Val, from: &MirTy, to: &MirTy, dst: Mem) {
        let t = self.c.t;
        if *from == MirTy::Unit || *to == MirTy::Unit {
            let size = t.size_align(to).0;
            if size > 0 {
                self.push(Inst::Zero { dst, size });
            }
            return;
        }
        if let (
            MirTy::Prim(
                fp @ (PrimTy::I8
                | PrimTy::I16
                | PrimTy::I32
                | PrimTy::I64
                | PrimTy::Isize
                | PrimTy::U8
                | PrimTy::U16
                | PrimTy::U32
                | PrimTy::U64
                | PrimTy::Usize
                | PrimTy::F32
                | PrimTy::F64
                | PrimTy::Bool
                | PrimTy::CChar
                | PrimTy::Char),
            ),
            MirTy::Prim(
                tp @ (PrimTy::I8
                | PrimTy::I16
                | PrimTy::I32
                | PrimTy::I64
                | PrimTy::Isize
                | PrimTy::U8
                | PrimTy::U16
                | PrimTy::U32
                | PrimTy::U64
                | PrimTy::Usize
                | PrimTy::F32
                | PrimTy::F64
                | PrimTy::Bool
                | PrimTy::CChar
                | PrimTy::Char),
            ),
            Val::S(a, fk),
        ) = (from, to, v)
        {
            let _ = fp;
            let pw = self.pw();
            let tk = cc::prim_k(*tp, pw);
            let ik = |k: K| match k {
                K::F32 => K::I32,
                K::F64 => K::I64,
                k => k,
            };
            let mut cur = if fk.is_float() {
                let m = self.tmp_k(ik(fk));
                self.push(Inst::Conv {
                    conv: Conv::Bits,
                    from: fk,
                    to: ik(fk),
                    dst: m,
                    a,
                });
                (V::M(m), ik(fk))
            } else {
                (a, fk)
            };
            if *tp == PrimTy::Bool {
                // `trunci` to `i1`: bit 0.
                if cur.1 == K::I8 {
                    self.push(Inst::Bin {
                        op: Bin::And,
                        k: K::I8,
                        signed: false,
                        dst,
                        a: cur.0,
                        b: V::I(1),
                    });
                } else {
                    let m = self.tmp_k(cur.1);
                    self.push(Inst::Bin {
                        op: Bin::And,
                        k: cur.1,
                        signed: false,
                        dst: m,
                        a: cur.0,
                        b: V::I(1),
                    });
                    self.push(Inst::Conv {
                        conv: Conv::Int { signed: false },
                        from: cur.1,
                        to: K::I8,
                        dst,
                        a: V::M(m),
                    });
                }
                return;
            }
            if ik(tk) != cur.1 {
                let m = self.tmp_k(ik(tk));
                self.push(Inst::Conv {
                    conv: Conv::Int { signed: false },
                    from: cur.1,
                    to: ik(tk),
                    dst: m,
                    a: cur.0,
                });
                cur = (V::M(m), ik(tk));
            }
            if tk.is_float() {
                self.push(Inst::Conv {
                    conv: Conv::Bits,
                    from: cur.1,
                    to: tk,
                    dst,
                    a: cur.0,
                });
            } else {
                self.push(Inst::Mov {
                    k: tk,
                    dst,
                    src: cur.0,
                });
            }
            return;
        }
        // Through memory: the common bytes, the rest zeroed.
        let src = self.mem_of(v, from);
        let (fs, ts) = (t.size_align(from).0, t.size_align(to).0);
        let n = fs.min(ts);
        if n > 0 && src != dst {
            self.push(Inst::Copy { dst, src, size: n });
        }
        if ts > n {
            self.push(Inst::Zero {
                dst: dst.at(n),
                size: ts - n,
            });
        }
    }

    /// A rok-ABI call (`callee` takes `params`, returns `ret`) of `args`;
    /// the result into `dst`.
    pub(crate) fn call_sym(
        &mut self,
        sym: SymId,
        params: &[MirTy],
        ret: &MirTy,
        args: &[Val],
        dst: Option<Mem>,
    ) {
        self.call_rok(Callee::Sym(sym), params, ret, args, dst, false);
    }

    /// A call of the rok body `sig` (`call_sym`, `rok_tls` passed if it
    /// takes it).
    pub(crate) fn call_body(&mut self, sig: &Sig, args: &[Val], dst: Option<Mem>) {
        self.call_rok(
            Callee::Sym(sig.sym),
            &sig.params,
            &sig.ret,
            args,
            dst,
            sig.tls,
        );
    }

    /// A call of a rok body `sym` without arguments that takes `rok_tls`
    /// (the user's `main`, a static's initializer).
    pub(crate) fn call_tls(&mut self, sym: SymId, ret: &MirTy, dst: Option<Mem>) {
        self.call_rok(Callee::Sym(sym), &[], ret, &[], dst, true);
    }

    /// Call a runtime function codegen uses itself: `call_sym`, with `str`
    /// passed as a pointer to a copy on Windows x64
    /// (`abi::classify_rok`).
    fn call_runtime(
        &mut self,
        sym: SymId,
        params: &[MirTy],
        ret: &MirTy,
        args: &[Val],
        dst: Option<Mem>,
    ) {
        if self.c.t.c_abi != rokc_mir::CAbi::X86_64Win {
            return self.call_sym(sym, params, ret, args, dst);
        }
        let str_ty = MirTy::Prim(PrimTy::Str);
        let word = MirTy::Prim(PrimTy::Usize);
        let mut ps = Vec::with_capacity(params.len());
        let mut vs = Vec::with_capacity(args.len());
        for (ty, v) in params.iter().zip(args) {
            if *ty == str_ty {
                let (size, align) = self.c.t.size_align(ty);
                let m = self.tmp_bytes(size, align);
                self.store(*v, ty, m);
                let p = self.word_tmp();
                self.push(Inst::Addr { dst: p, src: m });
                ps.push(word.clone());
                vs.push(Val::S(V::M(p), self.word()));
            } else {
                ps.push(ty.clone());
                vs.push(*v);
            }
        }
        self.call_sym(sym, &ps, ret, &vs, dst);
    }

    fn arg_parts(&self, v: Val, ty: &MirTy, parts: &[Part], out: &mut Vec<(Part, V)>) {
        let sh = cc::shape(ty, self.c.t);
        match v {
            Val::S(src, _) => {
                if let Some(p) = parts.first() {
                    out.push((*p, src));
                }
            }
            Val::M(m) => {
                for (p, l) in parts.iter().zip(&sh.leaves) {
                    out.push((*p, V::M(m.at(l.off))));
                }
            }
            Val::Unit => {}
        }
    }

    /// Shapes of a rok signature: by-address arguments are pointer
    /// words; a by-address result comes through a leading pointer (`true`).
    fn rok_shapes(
        &self,
        params: &[MirTy],
        ret: &MirTy,
        tls: bool,
    ) -> (Vec<Shape>, Option<Shape>, bool) {
        let t = self.c.t;
        let lead = rok_by_ref(ret, t);
        let mut shapes = Vec::new();
        if lead {
            shapes.push(cc::coerced("!llvm.ptr"));
        }
        shapes.extend(params.iter().map(|p| {
            if rok_by_ref(p, t) {
                cc::coerced("!llvm.ptr")
            } else {
                cc::shape(p, t)
            }
        }));
        if tls {
            shapes.push(cc::coerced("!llvm.ptr"));
        }
        let rs = (*ret != MirTy::Unit && !lead).then(|| cc::shape(ret, t));
        (shapes, rs, lead)
    }

    /// The address of `v` (a `ty` value) as a pointer argument.
    fn addr_arg(&mut self, v: Val, ty: &MirTy) -> Val {
        let m = match v {
            Val::M(m) => m,
            v => {
                let m = self.tmp(ty);
                self.store(v, ty, m);
                m
            }
        };
        let p = self.word_tmp();
        self.push(Inst::Addr { dst: p, src: m });
        Val::S(V::M(p), K::I64)
    }

    fn call_rok(
        &mut self,
        callee: Callee,
        params: &[MirTy],
        ret: &MirTy,
        args: &[Val],
        dst: Option<Mem>,
        tls: bool,
    ) {
        let t = self.c.t;
        let (shapes, rs, lead) = self.rok_shapes(params, ret, tls);
        let locs = cc::assign(self.c.conv, &shapes, rs.as_ref(), false, None);
        let dst = match (dst, ret) {
            (_, MirTy::Unit) => None,
            (Some(d), _) => Some(d),
            (None, r) => Some(self.tmp(r)),
        };
        let word = MirTy::Prim(PrimTy::Usize);
        let mut vals: Vec<(Val, &MirTy)> = Vec::new();
        if let (true, Some(d)) = (lead, dst) {
            let p = self.word_tmp();
            self.push(Inst::Addr { dst: p, src: d });
            vals.push((Val::S(V::M(p), K::I64), &word));
        }
        for (v, ty) in args.iter().zip(params) {
            if rok_by_ref(ty, t) {
                let p = self.addr_arg(*v, ty);
                vals.push((p, &word));
            } else {
                vals.push((*v, ty));
            }
        }
        if tls {
            let v = self.tls_val();
            vals.push((v, &word));
        }
        let mut a = Vec::new();
        for ((v, ty), parts) in vals.into_iter().zip(&locs.params) {
            self.arg_parts(v, ty, parts, &mut a);
        }
        let dst = if lead { None } else { dst };
        let (rets, sret) = match (&locs.ret, dst, &rs) {
            (Some(parts), Some(d), Some(sh)) => (
                parts
                    .iter()
                    .zip(&sh.leaves)
                    .map(|(p, l)| (*p, d.at(l.off)))
                    .collect(),
                None,
            ),
            (None, Some(d), _) => (Vec::new(), Some(d)),
            _ => (Vec::new(), None),
        };
        self.push(Inst::Call {
            callee,
            args: a,
            rets,
            sret,
            variadic: false,
        });
        if let (MirTy::Prim(PrimTy::Bool), Some(d)) = (ret, dst) {
            self.normalize_bool(d);
        }
    }

    /// Keep bit 0 of a `bool` another compiler handed over (only that bit
    /// is defined).
    fn normalize_bool(&mut self, m: Mem) {
        self.push(Inst::Bin {
            op: Bin::And,
            k: K::I8,
            signed: false,
            dst: m,
            a: V::M(m),
            b: V::I(1),
        });
    }

    /// A call under the C ABI (the emitter's `emit_c_call`).
    fn call_c(
        &mut self,
        callee: Callee,
        params: &[MirTy],
        ret: &MirTy,
        abi: &FnAbi,
        args: &[Operand],
        dst: Option<Mem>,
    ) {
        let t = self.c.t;
        let str_ty = MirTy::Prim(PrimTy::Str);
        let mut shapes: Vec<Shape> = Vec::new();
        let mut srcs: Vec<Vec<V>> = Vec::new();
        let mut c_strs: Vec<(Mem, Mem)> = Vec::new();
        for (i, op) in args.iter().enumerate() {
            let ty = params.get(i).cloned().unwrap_or_else(|| self.op_ty(op));
            let mut v = self.val(op, &ty);
            let mut ty = ty;
            if ty == str_ty && !abi.rok {
                let s = self.mem_of(v, &str_ty);
                let p = self.word_tmp();
                let sym = self.c.sym("rok_str_cstr");
                self.call_runtime(
                    sym,
                    std::slice::from_ref(&str_ty),
                    &MirTy::Prim(PrimTy::Usize),
                    &[Val::M(s)],
                    Some(p),
                );
                c_strs.push((s, p));
                v = Val::S(V::M(p), self.word());
                ty = MirTy::Prim(PrimTy::Usize);
            }
            let variadic = abi.variadic && i >= params.len();
            match abi.args.get(i).unwrap_or(&ArgAbi::Direct) {
                ArgAbi::Direct => {
                    let mut sh = cc::shape(&ty, t);
                    if variadic {
                        // C default promotions: narrow integers to a word,
                        // `f32` to `f64`.
                        if let (Val::S(src, k), Some(l)) = (v, sh.leaves.first_mut()) {
                            let (nk, conv) = if k.is_float() {
                                (K::F64, Conv::FToF)
                            } else {
                                (K::I64, Conv::Int { signed: l.signed })
                            };
                            if nk != k {
                                let m = self.tmp_k(nk);
                                self.push(Inst::Conv {
                                    conv,
                                    from: k,
                                    to: nk,
                                    dst: m,
                                    a: src,
                                });
                                v = Val::S(V::M(m), nk);
                            }
                            l.k = nk;
                        }
                    }
                    let vs = match v {
                        Val::S(src, _) => vec![src],
                        Val::M(m) => sh.leaves.iter().map(|l| V::M(m.at(l.off))).collect(),
                        Val::Unit => Vec::new(),
                    };
                    shapes.push(sh);
                    srcs.push(vs);
                }
                ArgAbi::Coerce(c) => {
                    let sh = cc::coerced(c);
                    let size = t.size_align(&ty).0.max(16);
                    let m = self.tmp_bytes(size, 16);
                    self.push(Inst::Zero { dst: m, size });
                    self.store(v, &ty, m);
                    srcs.push(sh.leaves.iter().map(|l| V::M(m.at(l.off))).collect());
                    shapes.push(sh);
                }
                ArgAbi::Indirect { byval: true } => {
                    // A copy in the argument area.
                    let (size, align) = slot_layout(&ty, t);
                    let sh = cc::byval(size, align);
                    let m = self.tmp_bytes(size.next_multiple_of(8), align.max(8));
                    self.store(v, &ty, m);
                    srcs.push(sh.leaves.iter().map(|l| V::M(m.at(l.off))).collect());
                    shapes.push(sh);
                }
                ArgAbi::Indirect { byval: false } => {
                    let (size, align) = slot_layout(&ty, t);
                    let m = self.tmp_bytes(size, align.max(8));
                    self.store(v, &ty, m);
                    let p = self.word_tmp();
                    self.push(Inst::Addr { dst: p, src: m });
                    shapes.push(cc::coerced("!llvm.ptr"));
                    srcs.push(vec![V::M(p)]);
                }
            }
        }
        let ret_shape = match &abi.ret {
            RetAbi::Direct if *ret == str_ty => Some(cc::coerced("!llvm.ptr")),
            RetAbi::Direct if *ret != MirTy::Unit => Some(cc::shape(ret, t)),
            RetAbi::Coerce(c) => Some(cc::coerced(c)),
            _ => None,
        };
        let variadic_from = abi.variadic.then_some(params.len());
        let locs = cc::assign(
            self.c.conv,
            &shapes,
            ret_shape.as_ref(),
            abi.ret == RetAbi::Sret,
            variadic_from,
        );
        let mut a = Vec::new();
        for (parts, vs) in locs.params.iter().zip(&srcs) {
            for (p, v) in parts.iter().zip(vs) {
                a.push((*p, *v));
            }
        }
        let out = match (dst, ret) {
            (_, MirTy::Unit) => None,
            (Some(d), _) => Some(d),
            (None, r) => Some(self.tmp(r)),
        };
        let mut rets = Vec::new();
        let mut sret = None;
        let mut coerce_back: Option<(Mem, Mem)> = None;
        let mut cstr_ret: Option<Mem> = None;
        match &abi.ret {
            RetAbi::Sret => {
                let m = match out {
                    Some(m) => m,
                    None => self.tmp(ret),
                };
                sret = Some(m);
            }
            RetAbi::Coerce(_) => {
                let size = t.size_align(ret).0.max(16);
                let m = self.tmp_bytes(size, 16);
                if let (Some(parts), Some(sh)) = (&locs.ret, &ret_shape) {
                    rets = parts
                        .iter()
                        .zip(&sh.leaves)
                        .map(|(p, l)| (*p, m.at(l.off)))
                        .collect();
                }
                if let Some(o) = out {
                    coerce_back = Some((m, o));
                }
            }
            RetAbi::Direct if *ret == str_ty => {
                let p = self.word_tmp();
                if let Some(parts) = &locs.ret {
                    rets = parts.iter().map(|q| (*q, p)).collect();
                }
                cstr_ret = Some(p);
            }
            RetAbi::Direct => {
                if let (Some(parts), Some(sh), Some(o)) = (&locs.ret, &ret_shape, out) {
                    rets = parts
                        .iter()
                        .zip(&sh.leaves)
                        .map(|(p, l)| (*p, o.at(l.off)))
                        .collect();
                }
            }
        }
        // Not for a `rok_*` runtime symbol declared `extern "C"` (as emit.rs).
        let parked = !abi.rok
            && !matches!(callee, Callee::Sym(s) if self.c.obj.syms[s as usize].name.starts_with("rok_"));
        if parked {
            self.native_edge(true);
        }
        self.push(Inst::Call {
            callee,
            args: a,
            rets,
            sret,
            variadic: abi.variadic,
        });
        if parked {
            self.native_edge(false);
        }
        for (s, p) in c_strs {
            let sym = self.c.sym("rok_str_cstr_done");
            let w = MirTy::Prim(PrimTy::Usize);
            self.call_runtime(
                sym,
                &[str_ty.clone(), w],
                &MirTy::Unit,
                &[Val::M(s), Val::S(V::M(p), self.word())],
                None,
            );
        }
        if let Some((m, o)) = coerce_back {
            let size = t.size_align(ret).0;
            self.push(Inst::Copy {
                dst: o,
                src: m,
                size,
            });
        }
        if let (Some(p), Some(o)) = (cstr_ret, out) {
            // A `char*` result becomes `(ptr, strlen)`; null reads as "".
            let w = self.word();
            self.push(Inst::Mov {
                k: w,
                dst: o,
                src: V::M(p),
            });
            let sym = self.c.sym("rok_cstr_len");
            self.call_rok(
                Callee::Sym(sym),
                &[MirTy::Prim(PrimTy::Usize)],
                &MirTy::Prim(PrimTy::Usize),
                &[Val::S(V::M(p), w)],
                Some(o.at(self.pw() / 8)),
                false,
            );
        }
        if let (MirTy::Prim(PrimTy::Bool), Some(o), RetAbi::Direct) = (ret, out, &abi.ret) {
            self.normalize_bool(o);
        }
    }

    /// A zeroed GC object with a `ty` payload; its address in a word temp
    /// (the emitter's `emit_gc_object`).
    fn gc_object(&mut self, ty: &MirTy) -> Mem {
        let w = self.word();
        let tid = self.gc_tid(ty, false);
        let (size, align) = gc_object_size(ty, self.c.t);
        let obj = self.word_tmp();
        let args = [
            (V::I(i64::from(size)), w),
            (V::I(i64::from(align)), w),
            (tid, K::I32),
        ];
        if size >= LOS_THRESHOLD || align > 8 {
            self.call_rt("rok_gc_alloc_slow", &args, Some((obj, w)));
            return obj;
        }
        let cp = self.tls(tls_abi::TLAB_CURSOR);
        let lp = self.tls(tls_abi::TLAB_LIMIT);
        let [cur, lim, room] = [(); 3].map(|()| self.word_tmp());
        self.push(Inst::Mov {
            k: w,
            dst: cur,
            src: V::M(cp),
        });
        self.push(Inst::Mov {
            k: w,
            dst: lim,
            src: V::M(lp),
        });
        self.push(Inst::Bin {
            op: Bin::Sub,
            k: w,
            signed: false,
            dst: room,
            a: V::M(lim),
            b: V::M(cur),
        });
        let fits = self.cmp(Cc::Le, w, false, V::I(i64::from(size)), V::M(room));
        let (fast, slow, done) = (self.label(), self.label(), self.label());
        self.push(Inst::Br {
            c: fits,
            t: fast,
            f: slow,
        });
        self.push(Inst::Label(fast));
        self.push(Inst::Bin {
            op: Bin::Add,
            k: w,
            signed: false,
            dst: cp,
            a: V::M(cur),
            b: V::I(i64::from(size)),
        });
        let Mem::Frame(cs, _) = cur else {
            unreachable!()
        };
        self.push(Inst::Mov {
            k: K::I32,
            dst: Mem::Ind(cs, 0),
            src: tid,
        });
        self.push(Inst::Mov {
            k: w,
            dst: Mem::Ind(cs, GC_WORD_OFFSET as i32),
            src: V::I(i64::from(size)),
        });
        self.push(Inst::Mov {
            k: w,
            dst: obj,
            src: V::M(cur),
        });
        self.push(Inst::Jmp(done));
        self.push(Inst::Label(slow));
        self.call_rt("rok_gc_alloc_slow", &args, Some((obj, w)));
        self.push(Inst::Jmp(done));
        self.push(Inst::Label(done));
        obj
    }

    /// Write barrier before storing a reference into `obj` at
    /// address `slot` (both word slots), `len` bytes.
    fn write_barrier(&mut self, obj: Slot, slot: Slot, len: u32) {
        let w = self.word();
        let flags = self.tmp_k(K::I32);
        self.push(Inst::Mov {
            k: K::I32,
            dst: flags,
            src: V::M(Mem::Ind(obj, GC_FLAGS_OFFSET as i32)),
        });
        let masked = self.tmp_k(K::I32);
        let bit = |s: &mut Self, mask: u32| {
            s.push(Inst::Bin {
                op: Bin::And,
                k: K::I32,
                signed: false,
                dst: masked,
                a: V::M(flags),
                b: V::I(i64::from(mask)),
            });
            s.cmp(Cc::Ne, K::I32, false, V::M(masked), V::I(0))
        };
        let set = bit(self, FLAG_UNLOGGED | FLAG_CARDED);
        let [wb, slow, cq, card, ok] = [(); 5].map(|()| self.label());
        self.push(Inst::Br {
            c: set,
            t: wb,
            f: ok,
        });
        self.push(Inst::Label(wb));
        let unlogged = bit(self, FLAG_UNLOGGED);
        self.push(Inst::Br {
            c: unlogged,
            t: slow,
            f: cq,
        });
        self.push(Inst::Label(slow));
        self.call_rt("rok_gc_wb_slow", &[(V::M(Mem::Frame(obj, 0)), w)], None);
        self.push(Inst::Jmp(cq));
        self.push(Inst::Label(cq));
        let carded = bit(self, FLAG_CARDED);
        self.push(Inst::Br {
            c: carded,
            t: card,
            f: ok,
        });
        self.push(Inst::Label(card));
        let wb_ = (self.pw() / 8) as i32;
        let [ct, off, i0, e1, i1, c0, c1] = [(); 7].map(|()| self.word_tmp());
        self.push(Inst::Mov {
            k: w,
            dst: ct,
            src: V::M(Mem::Ind(obj, -wb_)),
        });
        let bin = |s: &mut Self, op, dst, a, b| {
            s.push(Inst::Bin {
                op,
                k: w,
                signed: false,
                dst,
                a,
                b,
            });
        };
        bin(
            self,
            Bin::Sub,
            off,
            V::M(Mem::Frame(slot, 0)),
            V::M(Mem::Frame(obj, 0)),
        );
        bin(self, Bin::Shr, i0, V::M(off), V::I(i64::from(CARD_SHIFT)));
        bin(
            self,
            Bin::Add,
            e1,
            V::M(off),
            V::I(i64::from(len.max(1) - 1)),
        );
        bin(self, Bin::Shr, i1, V::M(e1), V::I(i64::from(CARD_SHIFT)));
        bin(self, Bin::Add, c0, V::M(ct), V::M(i0));
        bin(self, Bin::Add, c1, V::M(ct), V::M(i1));
        for c in [c0, c1] {
            let Mem::Frame(s, _) = c else { unreachable!() };
            self.push(Inst::Mov {
                k: K::I8,
                dst: Mem::Ind(s, 0),
                src: V::I(1),
            });
        }
        self.push(Inst::Jmp(ok));
        self.push(Inst::Label(ok));
    }

    /// Before touching a lazily initialized static: `@sym.init()` unless
    /// its state byte reads 2 (ready).
    fn ensure_static_init(&mut self, name: &str) {
        if !self.c.pkgs.lazy_statics.contains_key(name) {
            return;
        }
        let sym = self.c.pkgs.static_sym(name);
        let state = self.c.sym(&format!("{sym}.state"));
        let local = self.c.local_statics.contains(name);
        let p = self.word_tmp();
        self.push(Inst::Sym {
            dst: p,
            sym: state,
            got: !local,
        });
        let Mem::Frame(ps, _) = p else { unreachable!() };
        let s = self.tmp_k(K::I8);
        self.push(Inst::AtomicLoad {
            k: K::I8,
            dst: s,
            ptr: Mem::Ind(ps, 0),
        });
        let ready = self.cmp(Cc::Eq, K::I8, false, V::M(s), V::I(2));
        let (r, init) = (self.label(), self.label());
        self.push(Inst::Br {
            c: ready,
            t: r,
            f: init,
        });
        self.push(Inst::Label(init));
        // The location of this read: a recursive initialization's panic.
        let str_ty = MirTy::Prim(PrimTy::Str);
        let at = self.tmp(&str_ty);
        let loc = self.c.sources.location(self.at);
        self.str_lit(&loc, at);
        let f = self.c.sym(&format!("{sym}.init"));
        let at = self.val_of(at, &str_ty);
        self.call_sym(f, &[str_ty], &MirTy::Unit, &[at], None);
        self.push(Inst::Jmp(r));
        self.push(Inst::Label(r));
    }

    /// GC type id of `ty` objects (`array`: buffers): a constant, or in a
    /// unit read from the glue's symbol (`units::gc_symbol`).
    fn gc_tid(&mut self, ty: &MirTy, array: bool) -> V {
        let i = self.c.gc_type(ty, array);
        let Some(pkg) = self.c.gc_pkg else {
            return V::I(i64::from(self.c.gc_type_base + i as u32));
        };
        let name = rokc_codegen_mlir::units::gc_symbol(pkg, &self.c.gc_types[i]);
        let s = self.c.sym(&name);
        // The glue is in the same executable, as a unit's statics are.
        let p = self.word_tmp();
        self.push(Inst::Sym {
            dst: p,
            sym: s,
            got: false,
        });
        let Mem::Frame(ps, _) = p else { unreachable!() };
        let tid = self.tmp_k(K::I32);
        self.push(Inst::Mov {
            k: K::I32,
            dst: tid,
            src: V::M(Mem::Ind(ps, 0)),
        });
        V::M(tid)
    }

    /// The address of static `name` in a word temp.
    fn static_addr(&mut self, name: &str) -> Slot {
        let sym = self.c.pkgs.static_sym(name);
        // A DLL's (Windows): the address at its import address.
        let dll = self.c.pkgs.dll_statics.contains(name);
        let s = self.c.sym(&if dll { format!("__imp_{sym}") } else { sym });
        let local = self.c.local_statics.contains(name);
        let p = self.word_tmp();
        self.push(Inst::Sym {
            dst: p,
            sym: s,
            got: !local,
        });
        let Mem::Frame(ps, _) = p else { unreachable!() };
        if !dll {
            return ps;
        }
        let q = self.word_tmp();
        self.push(Inst::Mov {
            k: K::I64,
            dst: q,
            src: V::M(Mem::Ind(ps, 0)),
        });
        let Mem::Frame(qs, _) = q else { unreachable!() };
        qs
    }

    /// Whether a collection can run while this function is active.
    fn reaches_safepoint(&self) -> bool {
        rokc_codegen_mlir::emit::body_safepoints(self.f, self.c.quiet, self.c.pkgs, self.c.t)
    }

    /// The shadow-stack frame, as `emit_gc_frame` decides it.
    fn gc_frame(&mut self) {
        if self.back_edges.is_empty() && !self.reaches_safepoint() {
            return;
        }
        let (t, quiet, pkgs) = (self.c.t, self.c.quiet, self.c.pkgs);
        let live = rokc_mir::live_across(
            self.f,
            |l| !t.root_offsets(&self.f.locals[l.0 as usize].ty).is_empty(),
            |s| rokc_codegen_mlir::emit::stmt_safepoint(s, quiet, pkgs, t).then_some(true),
            |b| self.back_edges.contains(&b.0),
            true,
        );
        let caller = self.f.params.last().filter(|_| self.f.facts.track_caller);
        let mut roots: Vec<Mem> = Vec::new();
        for local in &self.f.locals {
            let offsets = self.c.t.root_offsets(&local.ty);
            if offsets.is_empty() || caller == Some(&local.id) || !live.contains(&local.id) {
                continue;
            }
            let Some(m) = self.local_mem(local.id) else {
                continue;
            };
            self.rooted.insert(local.id.0);
            if !self.f.params.contains(&local.id) {
                let size = self.c.t.size_align(&local.ty).0;
                self.push(Inst::Zero { dst: m, size });
            }
            roots.extend(offsets.into_iter().map(|o| m.at(o)));
        }
        if roots.is_empty() {
            return;
        }
        let wb = self.pw() / 8;
        let w = self.word();
        let n = roots.len() as u32;
        let frame = self.out.slot(wb * (2 + n), wb);
        let top = self.tls(tls_abi::SHADOW_STACK_TOP);
        self.push(Inst::Mov {
            k: w,
            dst: Mem::Frame(frame, 0),
            src: V::M(top),
        });
        self.push(Inst::Mov {
            k: w,
            dst: Mem::Frame(frame, wb),
            src: V::I(i64::from(n)),
        });
        for (i, r) in roots.into_iter().enumerate() {
            self.push(Inst::Addr {
                dst: Mem::Frame(frame, wb * (2 + i as u32)),
                src: r,
            });
        }
        self.push(Inst::Addr {
            dst: top,
            src: Mem::Frame(frame, 0),
        });
        self.gc_frame = Some((frame, top));
        let rooted = self.rooted.clone();
        self.deaths = rokc_mir::deaths(self.f, |l| rooted.contains(&l.0));
    }

    fn gc_frame_pop(&mut self) {
        if let Some((frame, top)) = self.gc_frame {
            let w = self.word();
            self.push(Inst::Mov {
                k: w,
                dst: top,
                src: V::M(Mem::Frame(frame, 0)),
            });
        }
    }

    fn clear(&mut self, l: LocalId) {
        if let Some(m) = self.local_mem(l) {
            let size = self.c.t.size_align(&self.local_ty(l)).0;
            if size > 0 {
                self.push(Inst::Zero { dst: m, size });
            }
        }
    }

    /// Lower the whole function.
    pub fn lower(mut self) -> Func {
        let t = self.c.t;
        let f = self.f;
        self.lowering = true;
        // Incoming parameters, the caller's `rok_tls` address last.
        let tls = self.c.fns.get(&f.id).is_some_and(|s| s.tls);
        let (shapes, rs, lead) = self.rok_shapes(&f.param_tys, &f.ret_ty, tls);
        let locs = cc::assign(self.c.conv, &shapes, rs.as_ref(), false, None);
        if tls {
            let s = self.out.slot(8, 8);
            self.tls = Some(s);
            let part = locs.params.last().expect("the rok_tls address")[0];
            self.push(Inst::Param {
                part,
                dst: Mem::Frame(s, 0),
            });
        }
        let (mut param_locs, mut param_shapes) = (locs.params.iter(), shapes.iter());
        if lead {
            // The result's address: returned through like a hidden one.
            let s = self.out.slot(8, 8);
            let (parts, _) = (param_locs.next(), param_shapes.next());
            self.push(Inst::Param {
                part: parts.expect("result pointer")[0],
                dst: Mem::Frame(s, 0),
            });
            self.sret = Some(s);
        } else if locs.ret.is_none() {
            let s = self.out.slot(8, 8);
            self.push(Inst::Param {
                part: Part {
                    loc: Loc::Sret,
                    k: K::I64,
                    signed: false,
                },
                dst: Mem::Frame(s, 0),
            });
            self.sret = Some(s);
        }
        self.ret_parts.clone_from(&locs.ret);
        // A by-address argument: its pointer first, the copy once every
        // parameter is stored.
        let mut copies = Vec::new();
        for (((p, ty), parts), sh) in f
            .params
            .iter()
            .zip(&f.param_tys)
            .zip(param_locs)
            .zip(param_shapes)
        {
            let Some(m) = self.local_mem(*p) else {
                continue;
            };
            if rok_by_ref(ty, t) {
                let s = self.out.slot(8, 8);
                self.push(Inst::Param {
                    part: parts[0],
                    dst: Mem::Frame(s, 0),
                });
                copies.push((m, s, t.size_align(ty).0));
                continue;
            }
            for (part, l) in parts.iter().zip(&sh.leaves) {
                self.push(Inst::Param {
                    part: *part,
                    dst: m.at(l.off),
                });
            }
        }
        for (dst, s, size) in copies {
            self.push(Inst::Copy {
                dst,
                src: Mem::Ind(s, 0),
                size,
            });
        }
        // After every parameter is stored: an instruction's scratch
        // registers may be argument registers.
        for p in &f.params {
            if let (MirTy::Prim(PrimTy::Bool), Some(m)) = (self.local_ty(*p), self.local_mem(*p)) {
                self.normalize_bool(m);
            }
        }
        let head = self.out.insts.len();
        self.gc_frame();
        // Entry poll: only where a call cycle can come back in without
        // a loop's poll, and a collection can run at all.
        if self.c.reentrant.contains(&f.id) && self.reaches_safepoint() {
            self.poll();
        }
        for b in &f.blocks {
            self.block(b);
        }
        self.emit_panics();
        // The TLS block's address, right after the parameters.
        if let Some(s) = self.tls.filter(|_| !tls) {
            let sym = self.c.sym("rok_tls");
            self.out.insts.insert(
                head,
                Inst::Tls {
                    dst: Mem::Frame(s, 0),
                    sym,
                },
            );
        }
        if self.c.debug.is_some() && f.span.line != 0 {
            self.out.debug = Some(self.debug_facts());
        }
        self.out
    }

    /// The `-g` description of this function: its scopes (by first
    /// position) and named locals (the emitter's `subprogram` / `local`).
    fn debug_facts(&self) -> crate::lir::FnDebug {
        let f = self.f;
        let mut scopes: Vec<(u32, u32)> = Vec::new();
        let mut decl: HashMap<u32, (u32, u32)> = HashMap::new();
        let (mut line, mut scope) = (f.span.line, 0);
        for s in f.blocks.iter().flat_map(|b| &b.stmts) {
            match s {
                Statement::Loc(sp, sc) => {
                    (line, scope) = (sp.line, sc.id);
                    if sc.id != 0 && !scopes.iter().any(|(i, _)| *i == sc.id) {
                        scopes.push((sc.id, sc.parent));
                    }
                }
                Statement::Assign { target, .. } => {
                    decl.entry(target.0).or_insert((line, scope));
                }
                _ => {}
            }
        }
        scopes.sort_unstable();
        let params: HashSet<u32> = f
            .blocks
            .iter()
            .flat_map(|b| b.params.iter().map(|p| p.0))
            .collect();
        let vars = f
            .locals
            .iter()
            .filter_map(|l| {
                let name = l.name.as_deref().filter(|n| rokc_mir::debug_visible(n))?;
                if params.contains(&l.id.0) {
                    return None;
                }
                let Some(Mem::Frame(slot, _)) = self.local_mem(l.id) else {
                    return None;
                };
                let arg = f.params.iter().position(|p| *p == l.id).map(|i| i + 1);
                let (line, scope) = decl.get(&l.id.0).copied().unwrap_or((f.span.line, 0));
                let ty = self
                    .c
                    .debug
                    .and_then(|d| d.locals.get(&(f.id, l.id)))
                    .filter(|t| t.fits(&l.ty))
                    .cloned()
                    .unwrap_or_else(|| rokc_mir::DebugTy::Mir(l.ty.clone()));
                Some(crate::lir::DebugVar {
                    name: name.to_owned(),
                    slot,
                    ty,
                    line,
                    scope,
                    arg,
                })
            })
            .collect();
        crate::lir::FnDebug {
            name: f.path().to_owned(),
            file: f.span.file,
            line: f.span.line,
            scopes,
            vars,
            ret: f.debug_ret(),
        }
    }

    fn block(&mut self, b: &BasicBlock) {
        let l = self.block_label[&b.id.0];
        self.push(Inst::Label(l));
        let pos = self.f.blocks.iter().position(|x| x.id == b.id).unwrap_or(0);
        if let Some(d) = self.deaths.on_entry.get(pos).cloned() {
            for l in d {
                self.clear(l);
            }
        }
        for (i, s) in b.stmts.iter().enumerate() {
            self.stmt(s);
            let dead = self
                .deaths
                .after_stmt
                .get(pos)
                .and_then(|d| d.get(i))
                .cloned()
                .unwrap_or_default();
            for l in dead {
                self.clear(l);
            }
        }
        if self.back_edges.contains(&b.id.0) {
            self.poll();
        }
        self.terminator(&b.terminator);
    }

    fn stmt(&mut self, s: &Statement) {
        let t = self.c.t;
        match s {
            Statement::Assign { target, rvalue } => {
                let ty = self.local_ty(*target);
                if ty == MirTy::Unit && matches!(rvalue, RValue::Use(_)) {
                    return;
                }
                let dst = self.local_mem(*target);
                // A callee may build a by-address result as it
                // goes: a local a pointer may read gets it once done.
                if let Some(d) = dst.filter(|_| {
                    self.addressed.contains(target)
                        && rok_by_ref(&ty, t)
                        && matches!(rvalue, RValue::Call { .. } | RValue::CallIndirect { .. })
                }) {
                    let tmp = self.tmp(&ty);
                    self.rvalue(&ty, rvalue, Some(tmp));
                    self.store(Val::M(tmp), &ty, d);
                    return;
                }
                self.rvalue(&ty, rvalue, dst);
            }
            Statement::ArrayWrite {
                array,
                offset,
                index,
                value,
                elem_ty,
                len,
            } => {
                let Some(base) = self.local_mem(*array) else {
                    return;
                };
                let base = base.at(*offset);
                let p = self.elem_addr(base, index, elem_ty, *len);
                let v = self.val(value, elem_ty);
                self.store(v, elem_ty, Mem::Ind(p, 0));
            }
            Statement::StorageDead(l) => {
                if self.rooted.contains(&l.0) {
                    self.clear(*l);
                }
            }
            Statement::BufStore { elem_ty, .. } if *elem_ty == MirTy::Unit => {}
            Statement::BufStore {
                buf,
                index,
                value,
                elem_ty,
            } => {
                let (b, p) = self.buf_elem(buf, index, elem_ty);
                if t.has_gc_ref(elem_ty) {
                    let len = t.size_align(elem_ty).0;
                    let a = self.word_tmp();
                    self.push(Inst::Addr {
                        dst: a,
                        src: Mem::Ind(p, HEADER_SIZE as i32),
                    });
                    let Mem::Frame(asl, _) = a else {
                        unreachable!()
                    };
                    self.write_barrier(b, asl, len);
                }
                let v = self.val(value, elem_ty);
                self.store(v, elem_ty, Mem::Ind(p, HEADER_SIZE as i32));
            }
            Statement::Loc(sp, scope) => {
                self.at = *sp;
                self.push(Inst::Line(sp.line, sp.col.max(1), scope.id));
            }
            Statement::RawStore {
                addr, value, ty, ..
            } => {
                let w = MirTy::Prim(PrimTy::Usize);
                let (a, k) = self.scalar(addr, &w);
                let ps = self.in_slot(a, k);
                let v = self.val(value, ty);
                self.store(v, ty, Mem::Ind(ps, 0));
            }
            Statement::HeapStore {
                ptr,
                ty,
                index,
                value,
            } => {
                let field = struct_fields(ty)[*index as usize].clone();
                let off = HEADER_SIZE + t.field_offsets(ty)[*index as usize];
                let (pv, k) = self.scalar(ptr, &MirTy::GcRef);
                let ps = self.in_slot(pv, k);
                if t.has_gc_ref(&field) {
                    let len = t.size_align(&field).0;
                    let a = self.word_tmp();
                    self.push(Inst::Addr {
                        dst: a,
                        src: Mem::Ind(ps, off as i32),
                    });
                    let Mem::Frame(asl, _) = a else {
                        unreachable!()
                    };
                    self.write_barrier(ps, asl, len);
                }
                let v = self.val(value, &field);
                self.store(v, &field, Mem::Ind(ps, off as i32));
            }
            Statement::StaticStore { name, value, ty } => {
                self.ensure_static_init(name);
                let v = self.val(value, ty);
                let p = self.static_addr(name);
                self.store(v, ty, Mem::Ind(p, 0));
            }
        }
    }

    /// Address of element `index` of the array at `base` (bounds checked).
    fn elem_addr(&mut self, base: Mem, index: &Operand, elem_ty: &MirTy, len: u32) -> Slot {
        let ity = self.op_ty(index);
        let (iv, ik) = self.scalar(index, &ity);
        let in_range = const_int(index).is_some_and(|c| (0..i128::from(len)).contains(&c));
        if !in_range {
            let fail = self.cmp(Cc::Ge, ik, false, iv, V::I(i64::from(len)));
            self.check(fail, "index out of bounds");
        }
        let signed = matches!(ity, MirTy::Prim(p) if p.is_signed_int());
        let w = self.word();
        let i64_ = self.word_tmp();
        self.push(Inst::Conv {
            conv: Conv::Int { signed },
            from: ik,
            to: w,
            dst: i64_,
            a: iv,
        });
        let size = self.c.t.size_align(elem_ty).0;
        let off = self.word_tmp();
        self.push(Inst::Bin {
            op: Bin::Mul,
            k: w,
            signed: false,
            dst: off,
            a: V::M(i64_),
            b: V::I(i64::from(size)),
        });
        let a = self.word_tmp();
        self.push(Inst::Addr { dst: a, src: base });
        let p = self.word_tmp();
        self.push(Inst::Bin {
            op: Bin::Add,
            k: w,
            signed: false,
            dst: p,
            a: V::M(a),
            b: V::M(off),
        });
        let Mem::Frame(ps, _) = p else { unreachable!() };
        ps
    }

    /// `(buffer slot, element address minus the header)` of a `BufAlloc`
    /// element.
    fn buf_elem(&mut self, buf: &Operand, index: &Operand, elem_ty: &MirTy) -> (Slot, Slot) {
        let w = self.word();
        let (bv, bk) = self.scalar(buf, &MirTy::GcRef);
        let bs = self.in_slot(bv, bk);
        let (iv, _) = self.scalar(index, &MirTy::Prim(PrimTy::Usize));
        let size = i64::from(self.c.t.size_align(elem_ty).0);
        let off = if let V::I(i) = iv {
            V::I(i.wrapping_mul(size))
        } else {
            let o = self.word_tmp();
            self.push(Inst::Bin {
                op: Bin::Mul,
                k: w,
                signed: false,
                dst: o,
                a: iv,
                b: V::I(size),
            });
            V::M(o)
        };
        let p = self.word_tmp();
        self.push(Inst::Bin {
            op: Bin::Add,
            k: w,
            signed: false,
            dst: p,
            a: off,
            b: V::M(Mem::Frame(bs, 0)),
        });
        let Mem::Frame(ps, _) = p else { unreachable!() };
        (bs, ps)
    }

    fn rvalue(&mut self, dst_ty: &MirTy, r: &RValue, dst: Option<Mem>) {
        let t = self.c.t;
        let pw = self.pw();
        // A destination for a value whose target slot is gone (`()`).
        let out = |s: &mut Self, ty: &MirTy| dst.unwrap_or_else(|| s.tmp(ty));
        match r {
            RValue::Use(op) => {
                let v = self.val(op, dst_ty);
                if let Some(d) = dst {
                    self.store(v, dst_ty, d);
                }
            }
            RValue::BinOp {
                op,
                op_ty,
                lhs,
                rhs,
            } => {
                let d = out(self, dst_ty);
                self.bin_op(*op, op_ty, lhs, rhs, d);
            }
            RValue::UnaryOp { op, op_ty, operand } => {
                let d = out(self, dst_ty);
                self.un_op(*op, op_ty, operand, d);
            }
            RValue::Call { callee, args, .. } => {
                let fns = self.c.fns;
                let sig = &fns[callee];
                if let Some(abi) = &sig.c {
                    self.call_c(Callee::Sym(sig.sym), &sig.params, &sig.ret, abi, args, dst);
                    return;
                }
                let vals: Vec<Val> = args
                    .iter()
                    .zip(&sig.params)
                    .map(|(a, ty)| self.val(a, ty))
                    .collect();
                self.call_body(sig, &vals, dst);
            }
            RValue::StructInit { ty, fields } => {
                let d = out(self, ty);
                let size = t.size_align(ty).0;
                if size > 0 {
                    self.push(Inst::Zero { dst: d, size });
                }
                let ftys = struct_fields(ty).to_vec();
                let offs = t.field_offsets(ty);
                for (i, f) in fields.iter().enumerate() {
                    let fty = ftys.get(i).cloned().unwrap_or_else(|| self.op_ty(f));
                    let v = self.val(f, &fty);
                    let off = offs.get(i).copied().unwrap_or(0);
                    self.store(v, &fty, d.at(off));
                }
            }
            RValue::FieldRead { target, index, .. } => {
                let tty = self.op_ty(target);
                let v = self.val(target, &tty);
                let src = self.mem_of(v, &tty);
                let fty = struct_fields(&tty)
                    .get(*index as usize)
                    .cloned()
                    .unwrap_or(MirTy::Unit);
                let off = t
                    .field_offsets(&tty)
                    .get(*index as usize)
                    .copied()
                    .unwrap_or(0);
                if let Some(d) = dst {
                    let size = t.size_align(&fty).0;
                    if size > 0 {
                        self.push(Inst::Copy {
                            dst: d,
                            src: src.at(off),
                            size,
                        });
                    }
                }
            }
            RValue::Cast {
                source,
                from_ty,
                to_ty,
            } => {
                let v = self.val(source, from_ty);
                let c = self.cast(v, from_ty, to_ty);
                if let Some(d) = dst {
                    self.store(c, to_ty, d);
                }
            }
            RValue::Bitcast {
                source,
                from_ty,
                to_ty,
            } => {
                let v = self.val(source, from_ty);
                if let Some(d) = dst {
                    self.bitcast_into(v, from_ty, to_ty, d);
                }
            }
            RValue::WithField {
                source,
                struct_ty,
                index,
                value,
            } => {
                let d = out(self, struct_ty);
                let v = self.val(source, struct_ty);
                self.store(v, struct_ty, d);
                let fty = struct_fields(struct_ty)
                    .get(*index as usize)
                    .cloned()
                    .unwrap_or(MirTy::Any);
                let fv = self.val(value, &fty);
                let off = t.field_offsets(struct_ty)[*index as usize];
                self.store(fv, &fty, d.at(off));
            }
            RValue::ArrayInit { elem_ty, elements } => {
                let Some(d) = dst else { return };
                let size = t.size_align(elem_ty).0;
                // A big one of constants (`b"..."`): one copy from data.
                let all = size * elements.len() as u32;
                let ty = MirTy::Array(Box::new(elem_ty.clone()), elements.len() as u32);
                if rok_by_ref(&ty, t) {
                    if let Some(bytes) = const_bytes(elements, elem_ty, t) {
                        let sym = self.c.obj.bytes(&bytes);
                        let w = self.word();
                        let p = self.tmp_k(w);
                        self.push(Inst::Sym {
                            dst: p,
                            sym,
                            got: false,
                        });
                        let Mem::Frame(s, _) = p else { unreachable!() };
                        self.push(Inst::Copy {
                            dst: d,
                            src: Mem::Ind(s, 0),
                            size: all,
                        });
                        return;
                    }
                }
                for (i, e) in elements.iter().enumerate() {
                    let v = self.val(e, elem_ty);
                    self.store(v, elem_ty, d.at(size * i as u32));
                }
            }
            RValue::ArrayIndex {
                array,
                offset,
                index,
                elem_ty,
                len,
            } => {
                let Some(base) = self.local_mem(*array) else {
                    return;
                };
                let base = base.at(*offset);
                let p = self.elem_addr(base, index, elem_ty, *len);
                if let Some(d) = dst {
                    let size = t.size_align(elem_ty).0;
                    if size > 0 {
                        self.push(Inst::Copy {
                            dst: d,
                            src: Mem::Ind(p, 0),
                            size,
                        });
                    }
                }
            }
            RValue::FnAddr { callee, .. } => {
                let sig = &self.c.fns[callee];
                let (sym, local) = match sig.c_entry {
                    Some(e) => (e, true),
                    // A function value's callers pass no `rok_tls`: its
                    // thunk (`module::thunk`).
                    None if sig.tls => {
                        let name = format!("{}.v", self.c.obj.syms[sig.sym as usize].name);
                        self.c.thunks.insert(*callee);
                        (self.c.sym(&name), true)
                    }
                    None => (sig.sym, sig.local),
                };
                if let Some(d) = dst {
                    self.push(Inst::Sym {
                        dst: d,
                        sym,
                        got: !local,
                    });
                }
            }
            RValue::CallIndirect {
                callee,
                args,
                arg_tys,
                ret_ty,
                abi,
            } => {
                let fn_ty = MirTy::Fn(arg_tys.clone(), Box::new(ret_ty.clone()));
                let cv = self.val(callee, &fn_ty);
                let cm = self.mem_of(cv, &fn_ty);
                match abi {
                    Abi::Rok => {
                        let vals: Vec<Val> = args
                            .iter()
                            .zip(arg_tys)
                            .map(|(a, ty)| self.val(a, ty))
                            .collect();
                        self.call_rok(Callee::Ptr(cm), arg_tys, ret_ty, &vals, dst, false);
                    }
                    Abi::C { variadic, .. } => {
                        let Some(fa) = classify(
                            &mut self.c.errors,
                            self.f.span,
                            t,
                            arg_tys,
                            ret_ty,
                            *variadic,
                        ) else {
                            return;
                        };
                        self.call_c(Callee::Ptr(cm), arg_tys, ret_ty, &fa, args, dst);
                    }
                }
            }
            RValue::StaticLoad { name, ty } => {
                self.ensure_static_init(name);
                let p = self.static_addr(name);
                if let Some(d) = dst {
                    self.store(Val::M(Mem::Ind(p, 0)), ty, d);
                }
            }
            RValue::HeapBox { value, ty } => {
                let w = self.word();
                if t.any_inline(ty) {
                    let v = self.val(value, ty);
                    let d = out(self, &MirTy::GcRef);
                    self.bitcast_into(v, ty, &MirTy::Prim(PrimTy::Usize), d);
                    return;
                }
                let obj = self.gc_object(ty);
                let Mem::Frame(os, _) = obj else {
                    unreachable!()
                };
                let v = self.val(value, ty);
                self.store(v, ty, Mem::Ind(os, HEADER_SIZE as i32));
                if let Some(d) = dst {
                    self.push(Inst::Mov {
                        k: w,
                        dst: d,
                        src: V::M(obj),
                    });
                }
            }
            RValue::HeapAlloc { ty } => {
                let w = self.word();
                let obj = self.gc_object(ty);
                if let Some(d) = dst {
                    self.push(Inst::Mov {
                        k: w,
                        dst: d,
                        src: V::M(obj),
                    });
                }
            }
            RValue::HeapField { ptr, ty, index } => {
                let (pv, k) = self.scalar(ptr, &MirTy::GcRef);
                let ps = self.in_slot(pv, k);
                let field = struct_fields(ty)[*index as usize].clone();
                let off = HEADER_SIZE + t.field_offsets(ty)[*index as usize];
                if let Some(d) = dst {
                    self.store(Val::M(Mem::Ind(ps, off as i32)), &field, d);
                }
            }
            RValue::HeapLoad { ptr, ty } => {
                let (pv, k) = self.scalar(ptr, &MirTy::GcRef);
                if t.any_inline(ty) {
                    if let Some(d) = dst {
                        let usize_ = MirTy::Prim(PrimTy::Usize);
                        self.bitcast_into(Val::S(pv, k), &usize_, ty, d);
                    }
                    return;
                }
                let ps = self.in_slot(pv, k);
                if let Some(d) = dst {
                    self.store(Val::M(Mem::Ind(ps, HEADER_SIZE as i32)), ty, d);
                }
            }
            RValue::BufAlloc { elem_ty, len } => {
                let w = self.word();
                let (n, nk) = self.scalar(len, &MirTy::Prim(PrimTy::Usize));
                let tid = if t.has_gc_ref(elem_ty) {
                    self.gc_tid(elem_ty, true)
                } else {
                    V::I(i64::from(PLAIN_TYPE_ID))
                };
                let (size, align) = t.size_align(elem_ty);
                let align = align.max(8);
                let umax = u64::MAX >> (64 - pw);
                let limit = (umax - u64::from(HEADER_SIZE)) / u64::from(size.max(1));
                let total = self.word_tmp();
                self.push(Inst::Bin {
                    op: Bin::Mul,
                    k: nk,
                    signed: false,
                    dst: total,
                    a: n,
                    b: V::I(i64::from(size)),
                });
                self.push(Inst::Bin {
                    op: Bin::Add,
                    k: w,
                    signed: false,
                    dst: total,
                    a: V::M(total),
                    b: V::I(i64::from(HEADER_SIZE)),
                });
                let ovf = self.cmp(Cc::Gt, w, false, n, V::I(limit as i64));
                let (big, go) = (self.label(), self.label());
                self.push(Inst::Br {
                    c: ovf,
                    t: big,
                    f: go,
                });
                self.push(Inst::Label(big));
                self.push(Inst::Mov {
                    k: w,
                    dst: total,
                    src: V::I(umax as i64),
                });
                self.push(Inst::Jmp(go));
                self.push(Inst::Label(go));
                let d = out(self, &MirTy::GcRef);
                self.call_rt(
                    "rok_gc_alloc_slow",
                    &[(V::M(total), w), (V::I(i64::from(align)), w), (tid, K::I32)],
                    Some((d, w)),
                );
            }
            RValue::RawLoad { addr, ty, .. } => {
                let (a, k) = self.scalar(addr, &MirTy::Prim(PrimTy::Usize));
                let ps = self.in_slot(a, k);
                if let Some(d) = dst {
                    self.store(Val::M(Mem::Ind(ps, 0)), ty, d);
                }
            }
            RValue::StaticAddr(name) => {
                self.ensure_static_init(name);
                let p = self.static_addr(name);
                if let Some(d) = dst {
                    let w = self.word();
                    self.push(Inst::Mov {
                        k: w,
                        dst: d,
                        src: V::M(Mem::Frame(p, 0)),
                    });
                }
            }
            RValue::AddrOf(l) => {
                let Some(d) = dst else { return };
                match self.local_mem(*l) {
                    Some(m) => self.push(Inst::Addr { dst: d, src: m }),
                    // A `()` local has no storage: any aligned address does.
                    None => {
                        let w = self.word();
                        self.push(Inst::Mov {
                            k: w,
                            dst: d,
                            src: V::I(1),
                        });
                    }
                }
            }
            RValue::SizeOf { ty, align } => {
                let (s, a) = t.size_align(ty);
                let w = self.word();
                if let Some(d) = dst {
                    self.push(Inst::Mov {
                        k: w,
                        dst: d,
                        src: V::I(i64::from(if *align { a } else { s })),
                    });
                }
            }
            RValue::BufLoad { elem_ty, .. } if *elem_ty == MirTy::Unit => {}
            RValue::BufLoad {
                buf,
                index,
                elem_ty,
            } => {
                let (_, p) = self.buf_elem(buf, index, elem_ty);
                if let Some(d) = dst {
                    self.store(Val::M(Mem::Ind(p, HEADER_SIZE as i32)), elem_ty, d);
                }
            }
            RValue::BufAtomic {
                op,
                buf,
                index,
                args,
                elem_ty,
                ..
            } => {
                let (_, p) = self.buf_elem(buf, index, elem_ty);
                let k = cc::scalar_k(elem_ty, t).unwrap_or(K::I64);
                let a = self.word_tmp();
                self.push(Inst::Addr {
                    dst: a,
                    src: Mem::Ind(p, HEADER_SIZE as i32),
                });
                let Mem::Frame(asl, _) = a else {
                    unreachable!()
                };
                let ptr = Mem::Ind(asl, 0);
                let ops: Vec<V> = args.iter().map(|x| self.scalar(x, elem_ty).0).collect();
                let d = out(self, elem_ty);
                match *op {
                    "load" => self.push(Inst::AtomicLoad { k, dst: d, ptr }),
                    // An exchange whose old value is dropped; it yields the
                    // value stored.
                    "store" => {
                        let old = self.tmp_k(k);
                        self.push(Inst::AtomicRmw {
                            op: Rmw::Xchg,
                            k,
                            dst: old,
                            ptr,
                            val: ops[0],
                        });
                        self.push(Inst::Mov {
                            k,
                            dst: d,
                            src: ops[0],
                        });
                    }
                    "cas" => self.push(Inst::Cas {
                        k,
                        dst: d,
                        ptr,
                        expected: ops[0],
                        new: ops[1],
                    }),
                    o => {
                        let op = match o {
                            "xchg" => Rmw::Xchg,
                            "add" => Rmw::Add,
                            "sub" => Rmw::Sub,
                            "and" => Rmw::And,
                            "or" => Rmw::Or,
                            _ => Rmw::Xor,
                        };
                        self.push(Inst::AtomicRmw {
                            op,
                            k,
                            dst: d,
                            ptr,
                            val: ops[0],
                        });
                    }
                }
            }
        }
    }

    fn bin_op(&mut self, op: PrimOp, op_ty: &MirTy, lhs: &Operand, rhs: &Operand, dst: Mem) {
        let pw = self.pw();
        let MirTy::Prim(prim) = op_ty else {
            self.err(format!("binary `{op:?}` on `{op_ty:?}`"));
            return;
        };
        let prim = *prim;
        if prim == PrimTy::Str && matches!(op, PrimOp::Eq | PrimOp::Ne) {
            let a = self.val(lhs, op_ty);
            let b = self.val(rhs, op_ty);
            let r = self.tmp_k(K::I32);
            let sym = self.c.sym("rok_str_cmp");
            self.call_runtime(
                sym,
                &[op_ty.clone(), op_ty.clone()],
                &MirTy::Prim(PrimTy::I32),
                &[a, b],
                Some(r),
            );
            let cc = if op == PrimOp::Eq { Cc::Eq } else { Cc::Ne };
            self.push(Inst::Cmp {
                cc,
                k: K::I32,
                signed: true,
                dst,
                a: V::M(r),
                b: V::I(0),
            });
            return;
        }
        let (a, k) = self.scalar(lhs, op_ty);
        let (b, _) = self.scalar(rhs, op_ty);
        let signed = prim.is_signed_int();
        let int = prim.is_int();
        let float = prim.is_float();
        let boolean = prim == PrimTy::Bool;
        let bits = prim.bit_width(pw);
        // Checks that must panic rather than hit undefined behaviour.
        if int {
            let rc = const_int(rhs);
            match op {
                PrimOp::Div | PrimOp::Rem => {
                    let (zero_msg, ovf_msg) = if op == PrimOp::Div {
                        (
                            "attempt to divide by zero",
                            "attempt to divide with overflow",
                        )
                    } else {
                        (
                            "attempt to calculate the remainder with a divisor of zero",
                            "attempt to calculate the remainder with overflow",
                        )
                    };
                    if rc.is_none_or(|c| c == 0) {
                        let fail = self.cmp(Cc::Eq, k, signed, b, V::I(0));
                        self.check(fail, zero_msg);
                    }
                    if signed && rc.is_none_or(|c| c == -1) {
                        let min = -(1i128 << (bits - 1));
                        let is_min = self.cmp(Cc::Eq, k, true, a, V::I(min as i64));
                        let is_neg1 = self.cmp(Cc::Eq, k, true, b, V::I(-1));
                        let both = self.tmp_k(K::I8);
                        self.push(Inst::Bin {
                            op: Bin::And,
                            k: K::I8,
                            signed: false,
                            dst: both,
                            a: is_min,
                            b: is_neg1,
                        });
                        self.check(V::M(both), ovf_msg);
                    }
                }
                PrimOp::Shl | PrimOp::Shr => {
                    if !rc.is_some_and(|c| (0..i128::from(bits)).contains(&c)) {
                        let fail = self.cmp(Cc::Ge, k, false, b, V::I(i64::from(bits)));
                        let msg = if op == PrimOp::Shl {
                            "attempt to shift left with overflow"
                        } else {
                            "attempt to shift right with overflow"
                        };
                        self.check(fail, msg);
                    }
                }
                _ => {}
            }
        }
        let bin = match op {
            PrimOp::Add | PrimOp::WrappingAdd if int || float => Some(Bin::Add),
            PrimOp::Sub | PrimOp::WrappingSub if int || float => Some(Bin::Sub),
            PrimOp::Mul | PrimOp::WrappingMul if int || float => Some(Bin::Mul),
            PrimOp::Div if int || float => Some(Bin::Div),
            PrimOp::Rem if int => Some(Bin::Rem),
            PrimOp::BitAnd | PrimOp::LogicalAnd if int || boolean => Some(Bin::And),
            PrimOp::BitOr | PrimOp::LogicalOr if int || boolean => Some(Bin::Or),
            PrimOp::BitXor if int || boolean => Some(Bin::Xor),
            PrimOp::Shl if int => Some(Bin::Shl),
            PrimOp::Shr if int => Some(Bin::Shr),
            _ => None,
        };
        if op == PrimOp::Rem && float {
            // LLVM's `frem` is C's `fmod`.
            let name = if prim == PrimTy::F32 { "fmodf" } else { "fmod" };
            let sym = self.c.sym(name);
            let shape = cc::shape(op_ty, self.c.t);
            let locs = cc::assign(
                self.c.conv,
                &[shape.clone(), shape.clone()],
                Some(&shape),
                false,
                None,
            );
            let r = locs.ret.expect("float result in a register");
            self.push(Inst::Call {
                callee: Callee::Sym(sym),
                args: vec![(locs.params[0][0], a), (locs.params[1][0], b)],
                rets: vec![(r[0], dst)],
                sret: None,
                variadic: false,
            });
            return;
        }
        if let Some(bop) = bin {
            let consts = (const_int(lhs), const_int(rhs));
            let checked = self.c.overflow_checks
                && int
                && matches!(op, PrimOp::Add | PrimOp::Sub | PrimOp::Mul)
                && (signed || prim.is_unsigned_int())
                && !cannot_overflow(op, prim, bits, consts);
            if checked {
                let msg = match op {
                    PrimOp::Add => "attempt to add with overflow",
                    PrimOp::Sub => "attempt to subtract with overflow",
                    _ => "attempt to multiply with overflow",
                };
                let fail = self.panic_label(msg);
                self.push(Inst::Ovf {
                    op: bop,
                    k,
                    signed,
                    dst,
                    a,
                    b,
                    fail,
                });
            } else {
                self.push(Inst::Bin {
                    op: bop,
                    k,
                    signed,
                    dst,
                    a,
                    b,
                });
            }
            return;
        }
        let cc = match op {
            PrimOp::Eq => Some(Cc::Eq),
            PrimOp::Ne => Some(Cc::Ne),
            PrimOp::Lt if !boolean => Some(Cc::Lt),
            PrimOp::Le if !boolean => Some(Cc::Le),
            PrimOp::Gt if !boolean => Some(Cc::Gt),
            PrimOp::Ge if !boolean => Some(Cc::Ge),
            _ => None,
        };
        match cc {
            Some(cc) if int || float || boolean => self.push(Inst::Cmp {
                cc,
                k,
                signed,
                dst,
                a,
                b,
            }),
            _ => self.err(format!("binary `{op:?}` on `{prim:?}`")),
        }
    }

    fn un_op(&mut self, op: PrimOp, op_ty: &MirTy, operand: &Operand, dst: Mem) {
        let pw = self.pw();
        let MirTy::Prim(p) = op_ty else {
            self.err(format!("unary `{op:?}` on `{op_ty:?}`"));
            return;
        };
        let p = *p;
        let (a, k) = self.scalar(operand, op_ty);
        let un = |s: &mut Self, op| s.push(Inst::Un { op, k, dst, a });
        match op {
            PrimOp::Neg if p.is_signed_int() => {
                let bits = p.bit_width(pw);
                let min = -(1i128 << (bits - 1));
                if self.c.overflow_checks && const_int(operand).is_none_or(|c| c == min) {
                    let fail = self.cmp(Cc::Eq, k, true, a, V::I(min as i64));
                    self.check(fail, "attempt to negate with overflow");
                }
                un(self, Un::Neg);
            }
            PrimOp::Neg if p.is_float() => un(self, Un::Neg),
            PrimOp::Sqrt if p.is_float() => un(self, Un::Sqrt),
            PrimOp::CountOnes if p.is_int() => un(self, Un::Popcnt),
            PrimOp::SwapBytes if p.is_int() => un(self, Un::Bswap),
            PrimOp::LeadingZeros if p.is_int() => un(self, Un::Clz),
            PrimOp::TrailingZeros if p.is_int() => un(self, Un::Ctz),
            PrimOp::Not if p.is_int() => un(self, Un::Not),
            PrimOp::LogicalNot if p == PrimTy::Bool => self.push(Inst::Bin {
                op: Bin::Xor,
                k: K::I8,
                signed: false,
                dst,
                a,
                b: V::I(1),
            }),
            _ => self.err(format!("unary `{op:?}` on `{p:?}`")),
        }
    }

    fn terminator(&mut self, term: &Terminator) {
        let f = self.f;
        match term {
            Terminator::Return(None) => {
                self.gc_frame_pop();
                self.push(Inst::Ret { vals: Vec::new() });
            }
            Terminator::Return(Some(_)) if f.ret_ty == MirTy::Unit => {
                self.gc_frame_pop();
                self.push(Inst::Ret { vals: Vec::new() });
            }
            Terminator::Return(Some(op)) => {
                let ty = f.ret_ty.clone();
                let v = self.val(op, &ty);
                self.gc_frame_pop();
                if let Some(s) = self.sret {
                    self.store(v, &ty, Mem::Ind(s, 0));
                    self.push(sret_ret(s));
                    return;
                }
                let parts = self.ret_parts.clone().unwrap_or_default();
                let mut vals = Vec::new();
                self.arg_parts(v, &ty, &parts, &mut vals);
                self.push(Inst::Ret { vals });
            }
            Terminator::Goto { target, args } => self.edge(target.0, args),
            Terminator::If {
                cond,
                then_bb,
                then_args,
                else_bb,
                else_args,
            } => {
                let (c, _) = self.scalar(cond, &MirTy::Prim(PrimTy::Bool));
                let tl = if then_args.is_empty() {
                    self.block_label[&then_bb.0]
                } else {
                    self.label()
                };
                let el = if else_args.is_empty() {
                    self.block_label[&else_bb.0]
                } else {
                    self.label()
                };
                self.push(Inst::Br { c, t: tl, f: el });
                if !then_args.is_empty() {
                    self.push(Inst::Label(tl));
                    self.edge(then_bb.0, then_args);
                }
                if !else_args.is_empty() {
                    self.push(Inst::Label(el));
                    self.edge(else_bb.0, else_args);
                }
            }
            Terminator::Unreachable => self.push(Inst::Trap),
            Terminator::Suspend { .. } => {
                let msg = "internal: a coroutine suspend point survived MIR".to_owned();
                self.c.errors.push((self.f.span, msg));
                self.push(Inst::Trap);
            }
        }
    }

    /// Pass `args` to block `target`'s parameters and jump there.
    fn edge(&mut self, target: u32, args: &[Operand]) {
        let params = self.block_params.get(&target).cloned().unwrap_or_default();
        // A parallel copy: through temporaries when an argument reads a
        // parameter of the target.
        let reads_param = args
            .iter()
            .any(|a| matches!(a, Operand::Copy(l) if params.contains(l)));
        let mut staged = Vec::new();
        for (a, p) in args.iter().zip(&params) {
            let ty = self.local_ty(*p);
            let Some(d) = self.local_mem(*p) else {
                continue;
            };
            // A constant takes the parameter's type.
            let aty = match a {
                Operand::Copy(_) => self.op_ty(a),
                Operand::Const(_) => ty.clone(),
            };
            let v = self.val(a, &aty);
            let v = match (&aty, &ty) {
                (MirTy::Prim(x), MirTy::Prim(y)) if prim_class(*x) != prim_class(*y) => {
                    self.cast(v, &aty, &ty)
                }
                _ => v,
            };
            if reads_param {
                let m = self.tmp(&ty);
                self.store(v, &ty, m);
                staged.push((Val::M(m), ty, d));
            } else {
                self.store(v, &ty, d);
            }
        }
        for (v, ty, d) in staged {
            self.store(v, &ty, d);
        }
        let l = self.block_label[&target];
        self.push(Inst::Jmp(l));
    }
}

/// `abi::classify` with the emitter's `#[repr(transparent)]` scalar
/// coercion; `None` (and an error) where the target cannot pass it.
pub fn classify(
    errors: &mut Vec<(rokc_diagnostic::Span, String)>,
    at: rokc_diagnostic::Span,
    t: &Target,
    params: &[MirTy],
    ret: &MirTy,
    variadic: bool,
) -> Option<FnAbi> {
    use rokc_codegen_mlir::abi;
    let mut fa = abi::classify(t, params, ret, variadic);
    let coerce = |ty: &MirTy| {
        let inner = abi::transparent(ty, t);
        (inner != *ty && !abi::is_aggregate(&inner) && inner != MirTy::Unit).then(|| {
            match cc::scalar_k(&inner, t) {
                Some(K::F32) => "f32".to_owned(),
                Some(K::F64) => "f64".to_owned(),
                Some(k) => format!("i{}", k.bits()),
                None => "!llvm.ptr".to_owned(),
            }
        })
    };
    if let Some(a) = fa.as_mut() {
        for (x, p) in a.args.iter_mut().zip(params) {
            if let (ArgAbi::Direct, Some(c)) = (&x, coerce(p)) {
                *x = ArgAbi::Coerce(c);
            }
        }
        if let (RetAbi::Direct, Some(c)) = (&a.ret, coerce(ret)) {
            a.ret = RetAbi::Coerce(c);
        }
    }
    if fa.is_none() {
        errors.push((
            at,
            "unsupported in codegen: a `struct` by value across the C ABI on this target"
                .to_owned(),
        ));
    }
    fa
}

/// Return through the hidden result pointer in `slot`, which goes back
/// in the first result register (`x86_64` System V; harmless elsewhere).
pub fn sret_ret(slot: Slot) -> Inst {
    Inst::Ret {
        vals: vec![(
            Part {
                loc: Loc::Int(0),
                k: K::I64,
                signed: false,
            },
            V::M(Mem::Frame(slot, 0)),
        )],
    }
}
