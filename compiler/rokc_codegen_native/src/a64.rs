//! LIR -> arm64 machine code.
//!
//! Frame: `fp` / `lr` pushed, `fp` = that pair, then the slots, outgoing
//! arguments at `sp`. Only caller-saved registers are used (`x9`..`x17`,
//! `v16`..`v18`), so nothing is saved and compact unwind sees a plain
//! frame. Every LIR value lives in its slot; in straight-line code the
//! emitter tracks which register still holds a slot (`Held`) and delays
//! the store of a slot written and read once (`Pend`). Branches, labels
//! and calls see every slot in memory.

use crate::lir::{
    Bin, Callee, Cc, Conv, Func, Inst, K, Label, Loc, Mem, Rmw, Un, V, Waits, scalar, waitable,
};
use crate::obj::{FnRange, Obj, RelKind, Reloc, Sect};

/// Stack probe interval.
const PAGE: u32 = 4096;
const SP: u32 = 31;
const ZR: u32 = 31;
const FP: u32 = 29;
/// Data scratch.
const X9: u32 = 9;
const X10: u32 = 10;
const X11: u32 = 11;
const X12: u32 = 12;
const X13: u32 = 13;
/// Pointer bases of indirect source / destination memory.
const X14: u32 = 14;
const X15: u32 = 15;
/// Call target; address legalization.
const X16: u32 = 16;
const X17: u32 = 17;
const V16: u32 = 16;
const V17: u32 = 17;
const V18: u32 = 18;

/// Load / store opcodes (unsigned-offset form; the unscaled form clears
/// bit 24), with log2 of the access size.
#[derive(Clone, Copy)]
struct Ls(u32, u32);

const STRB: Ls = Ls(0x3900_0000, 0);
const LDRB: Ls = Ls(0x3940_0000, 0);
const LDRSB: Ls = Ls(0x3980_0000, 0);
const STRH: Ls = Ls(0x7900_0000, 1);
const LDRH: Ls = Ls(0x7940_0000, 1);
const LDRSH: Ls = Ls(0x7980_0000, 1);
const STRW: Ls = Ls(0xb900_0000, 2);
const LDRW: Ls = Ls(0xb940_0000, 2);
const LDRSW: Ls = Ls(0xb980_0000, 2);
const STRX: Ls = Ls(0xf900_0000, 3);
const LDRX: Ls = Ls(0xf940_0000, 3);
const STRS: Ls = Ls(0xbd00_0000, 2);
const LDRS: Ls = Ls(0xbd40_0000, 2);
const STRD: Ls = Ls(0xfd00_0000, 3);
const LDRD: Ls = Ls(0xfd40_0000, 3);

/// Condition codes.
const EQ: u32 = 0;
const NE: u32 = 1;
const HS: u32 = 2;
const LO: u32 = 3;
const MI: u32 = 4;
const VS: u32 = 6;
const HI: u32 = 8;
const LS_: u32 = 9;
const GE: u32 = 10;
const LT: u32 = 11;
const GT: u32 = 12;
const LE: u32 = 13;

enum Fix {
    /// `b` (26-bit).
    B(Label),
}

/// A register that holds the `size` bytes at `off` of frame slot `slot`. `ext`:
/// its upper bits are those bytes extended (`Some(signed)`), or unknown.
#[derive(Clone, Copy)]
struct Held {
    reg: u32,
    fp: bool,
    slot: u32,
    off: u32,
    size: u32,
    ext: Option<bool>,
}

/// A store not yet emitted: `reg` into byte `off` of `slot`, at `sp + at`.
#[derive(Clone, Copy)]
struct Pend {
    reg: u32,
    op: Ls,
    slot: u32,
    off: u32,
    at: i64,
}

/// Value registers, picked by `pick`: those no instruction uses for a
/// fixed purpose across its operand loads.
const POOL: [u32; 5] = [X9, X10, X11, X12, X13];
const FP_POOL: [u32; 3] = [V16, V17, V18];

/// Every prologue (before the frame's `sub sp`, undone via `fp`): `stp
/// x29, x30, [sp, #-16]!; mov x29, sp`, as the unwind encodings
/// (`coff::A64_CODES`, Mach-O frame mode) say.
pub const PROLOGUE: [u32; 2] = [0xa9bf_7bfd, 0x9100_03fd];
/// Every epilogue: `mov sp, x29; ldp x29, x30, [sp], #16; ret`.
pub const EPILOGUE: [u32; 3] = [0x9100_03bf, 0xa8c1_7bfd, 0xd65f_03c0];

struct Em<'a> {
    obj: &'a mut Obj,
    start: u32,
    code: Vec<u32>,
    relocs: Vec<(u32, RelKind, u32)>,
    fixes: Vec<(usize, Fix)>,
    labels: Vec<Option<usize>>,
    slot_off: Vec<u32>,
    frame: u32,
    lines: Vec<(u32, u32, u32, u32)>,
    held: Vec<Held>,
    /// Slots whose address is taken: a store through a pointer may write them.
    escaped: Vec<bool>,
    pending: Vec<Pend>,
    /// `waitable`: read at most once, the store may wait; never read, it is
    /// dead.
    waits: Waits,
    /// The relocation of the next instruction.
    next_rel: Option<(RelKind, u32)>,
    /// A comparison left its result in the flags for the next `Br`.
    flags: Option<u32>,
}

const fn sf(k: K) -> u32 {
    if matches!(k, K::I64 | K::F64) {
        1 << 31
    } else {
        0
    }
}

/// Emit `f` into `obj`'s text.
pub fn emit(obj: &mut Obj, f: &Func) {
    // The outgoing argument area: the most any call puts on the stack.
    let mut out = 0u32;
    for i in &f.insts {
        if let Inst::Call { args, .. } = i {
            for (p, _) in args {
                if let Loc::Stack(o) = p.loc {
                    out = out.max(o + p.k.size().max(8));
                }
            }
        }
    }
    let mut at = out.next_multiple_of(16);
    let mut slot_off = Vec::with_capacity(f.slots.len());
    for &(size, align) in &f.slots {
        at = at.next_multiple_of(align.min(16));
        slot_off.push(at);
        at += size;
    }
    let frame = at.next_multiple_of(16);
    let (waits, escaped) = waitable(f);
    let start = obj.sect(Sect::Text).align_to(4);
    let mut e = Em {
        obj,
        start,
        code: Vec::new(),
        relocs: Vec::new(),
        fixes: Vec::new(),
        labels: vec![None; f.labels as usize],
        slot_off,
        frame,
        lines: Vec::new(),
        held: Vec::new(),
        escaped,
        pending: Vec::new(),
        waits,
        next_rel: None,
        flags: None,
    };
    e.prologue();
    for (n, i) in f.insts.iter().enumerate() {
        e.inst(i, f.insts.get(n + 1));
    }
    let debug = f.debug.as_ref().map(|d| {
        let offs = d
            .vars
            .iter()
            .map(|v| e.slot_off[v.slot as usize] as i32 - frame as i32)
            .collect();
        (d.clone(), offs)
    });
    e.finish(f.sym, debug);
}

impl Em<'_> {
    /// Append `w`. Every instruction written names its destination in bits
    /// 0..5 (write-back bases and `cas`'s `rs` aside), so a store pending on
    /// that register goes out first and its `Held` is dropped.
    fn ins(&mut self, w: u32) {
        self.flush_reg(w & 31);
        if let Some((kind, sym)) = self.next_rel.take() {
            let off = self.start + 4 * self.code.len() as u32;
            self.relocs.push((off, kind, sym));
        }
        self.code.push(w);
        self.clobber(w & 31);
    }

    fn emit_pend(&mut self, p: Pend) {
        let Ls(enc, sz) = p.op;
        self.code
            .push(enc | (((p.at >> sz) as u32) << 10) | (SP << 5) | p.reg);
    }

    /// Emit the stores waiting on register `r`.
    fn flush_reg(&mut self, r: u32) {
        while let Some(i) = self.pending.iter().position(|p| p.reg == r) {
            let p = self.pending.remove(i);
            self.emit_pend(p);
        }
    }

    /// Emit the stores waiting on slot `slot` (`None`: all).
    fn flush(&mut self, slot: Option<u32>) {
        while let Some(i) = self
            .pending
            .iter()
            .position(|p| slot.is_none_or(|s| p.slot == s))
        {
            let p = self.pending.remove(i);
            self.emit_pend(p);
        }
    }

    /// Slot `m`'s one read takes the register: its store is dead.
    fn consume(&mut self, m: Mem) {
        if let Mem::Frame(s, o) = m {
            self.pending.retain(|p| p.slot != s || p.off != o);
        }
    }

    /// The first value register not in `avoid`, preferring one that no
    /// store waits on and holds nothing.
    fn pick(&self, pool: &[u32], avoid: &[u32]) -> u32 {
        let ok = |r: &&u32| !avoid.contains(r);
        let free = |r: &&u32| ok(r) && !self.pending.iter().any(|p| p.reg == **r);
        pool.iter()
            .filter(free)
            .find(|r| !self.held.iter().any(|h| h.reg == **r))
            .or_else(|| pool.iter().find(free))
            .or_else(|| pool.iter().find(ok))
            .copied()
            .unwrap_or(pool[0])
    }

    fn clobber(&mut self, r: u32) {
        self.held.retain(|h| h.reg != r);
    }

    /// A call or a join: no register is known.
    fn forget(&mut self) {
        self.held.clear();
    }

    /// `size` bytes at `m` were written.
    fn wrote(&mut self, m: Mem, size: u32) {
        match m {
            Mem::Frame(s, o) => self
                .held
                .retain(|h| h.slot != s || h.off >= o + size || o >= h.off + h.size),
            Mem::Ind(..) => {
                let esc = &self.escaped;
                self.held.retain(|h| !esc[h.slot as usize]);
            }
        }
    }

    fn find(&self, m: Mem, size: u32, fp: bool) -> Option<Held> {
        let Mem::Frame(s, o) = m else { return None };
        self.held
            .iter()
            .find(|h| h.slot == s && h.off == o && h.size == size && h.fp == fp)
            .copied()
    }

    fn hold(&mut self, reg: u32, fp: bool, m: Mem, size: u32, ext: Option<bool>) {
        if let Mem::Frame(slot, off) = m {
            self.held.push(Held {
                reg,
                fp,
                slot,
                off,
                size,
                ext,
            });
        }
    }

    fn pos(&self) -> usize {
        self.code.len()
    }

    /// Relocate the next instruction.
    const fn reloc(&mut self, kind: RelKind, sym: u32) {
        self.next_rel = Some((kind, sym));
    }

    fn finish(mut self, sym: u32, debug: Option<(crate::lir::FnDebug, Vec<i32>)>) {
        for (at, fix) in std::mem::take(&mut self.fixes) {
            let Fix::B(l) = fix;
            let target = self.labels[l as usize].expect("bound label");
            let d = target as i64 - at as i64;
            self.code[at] |= (d as u32) & 0x03ff_ffff;
        }
        let len = 4 * self.code.len() as u32;
        let start = self.start;
        let text = self.obj.sect(Sect::Text);
        for w in &self.code {
            text.data.extend_from_slice(&w.to_le_bytes());
        }
        for (off, kind, s) in self.relocs {
            text.relocs.push(Reloc { off, kind, sym: s });
        }
        let lines = std::mem::take(&mut self.lines);
        let s = &mut self.obj.syms[sym as usize];
        s.def = Some((Sect::Text, start, len));
        self.obj.fns.push(FnRange {
            sym,
            start,
            len,
            lines,
            debug,
        });
    }

    fn movz(&mut self, rd: u32, imm: u32, hw: u32) {
        self.ins(0xd280_0000 | (hw << 21) | (imm << 5) | rd);
    }

    fn movk(&mut self, rd: u32, imm: u32, hw: u32) {
        self.ins(0xf280_0000 | (hw << 21) | (imm << 5) | rd);
    }

    fn movn(&mut self, rd: u32, imm: u32, hw: u32) {
        self.ins(0x9280_0000 | (hw << 21) | (imm << 5) | rd);
    }

    fn mov_imm(&mut self, rd: u32, v: u64) {
        let halves = |x: u64| (0..4).filter(|h| (x >> (16 * h)) & 0xffff != 0).count();
        if halves(!v) < halves(v) {
            // `movn` then `movk` the halfwords that are not all ones.
            let mut first = true;
            for h in 0..4u32 {
                let part = ((v >> (16 * h)) & 0xffff) as u32;
                if first {
                    if part != 0xffff || h == 3 {
                        self.movn(rd, !part & 0xffff, h);
                        first = false;
                    }
                } else if part != 0xffff {
                    self.movk(rd, part, h);
                }
            }
            if first {
                self.movn(rd, 0, 0);
            }
            return;
        }
        let mut first = true;
        for h in 0..4u32 {
            let part = ((v >> (16 * h)) & 0xffff) as u32;
            if part != 0 {
                if first {
                    self.movz(rd, part, h);
                    first = false;
                } else {
                    self.movk(rd, part, h);
                }
            }
        }
        if first {
            self.movz(rd, 0, 0);
        }
    }

    /// `rd = rn + imm` (64-bit; `sp` allowed for both).
    fn add_imm(&mut self, rd: u32, rn: u32, imm: i64) {
        let (op, a) = if imm < 0 {
            (0xd100_0000u32, imm.unsigned_abs())
        } else {
            (0x9100_0000u32, imm as u64)
        };
        if a == 0 {
            if rd != rn {
                self.ins(0x9100_0000 | (rn << 5) | rd);
            }
            return;
        }
        if a < 1 << 24 {
            let (hi, lo) = ((a >> 12) as u32, (a & 0xfff) as u32);
            let mut src = rn;
            if hi != 0 {
                self.ins(op | (1 << 22) | (hi << 10) | (src << 5) | rd);
                src = rd;
            }
            if lo != 0 {
                self.ins(op | (lo << 10) | (src << 5) | rd);
            }
            return;
        }
        // Far: through x17, extended-register form (takes `sp`).
        self.mov_imm(X17, imm as u64);
        self.ins(0x8b20_6000 | (X17 << 16) | (rn << 5) | rd);
    }

    /// Load / store `rt` at `base + off`.
    fn ldst(&mut self, op: Ls, rt: u32, base: u32, off: i64) {
        let Ls(enc, sz) = op;
        let store = enc & (3 << 22) == 0;
        let scale = 1i64 << sz;
        if base == SP && !store {
            // A frame read: what it reads must be there.
            let hit: Vec<u32> = self
                .pending
                .iter()
                .filter(|p| p.at < off + scale && off < p.at + (1 << p.op.1))
                .map(|p| p.slot)
                .collect();
            for s in hit {
                self.flush(Some(s));
            }
        }
        let w = if off >= 0 && off % scale == 0 && off / scale < 4096 {
            enc | (((off / scale) as u32) << 10) | (base << 5) | rt
        } else if (-256..256).contains(&off) {
            (enc & !0x0100_0000) | (((off as u32) & 0x1ff) << 12) | (base << 5) | rt
        } else {
            self.add_imm(X17, base, off);
            enc | (X17 << 5) | rt
        };
        if store {
            // Writes no register.
            self.code.push(w);
        } else {
            self.ins(w);
        }
    }

    /// `(base register, offset)` of `m`; an indirect pointer goes into `tmp`.
    fn base(&mut self, m: Mem, tmp: u32) -> (u32, i64) {
        match m {
            Mem::Frame(s, o) => (SP, i64::from(self.slot_off[s as usize] + o)),
            Mem::Ind(s, o) => {
                let p = Mem::Frame(s, 0);
                if let Some(h) = self.find(p, 8, false) {
                    self.consume(p);
                    return (h.reg, i64::from(o));
                }
                let so = i64::from(self.slot_off[s as usize]);
                self.ldst(LDRX, tmp, SP, so);
                self.hold(tmp, false, p, 8, None);
                (tmp, i64::from(o))
            }
        }
    }

    fn addr(&mut self, rd: u32, m: Mem) {
        if let Mem::Frame(s, _) = m {
            // Reached through the address.
            self.flush(Some(s));
        }
        let (b, o) = self.base(m, rd);
        self.add_imm(rd, b, o);
    }

    const fn load_op(k: K, signed: bool) -> Ls {
        match (k, signed) {
            (K::I8, true) => LDRSB,
            (K::I8, false) => LDRB,
            (K::I16, true) => LDRSH,
            (K::I16, false) => LDRH,
            (K::I32, true) => LDRSW,
            (K::I32 | K::F32, _) => LDRW,
            (K::I64 | K::F64, _) => LDRX,
        }
    }

    const fn store_op(k: K) -> Ls {
        match k {
            K::I8 => STRB,
            K::I16 => STRH,
            K::I32 | K::F32 => STRW,
            K::I64 | K::F64 => STRX,
        }
    }

    /// Integer register `rd` = `v` as `k`, extended by `signed`.
    fn load(&mut self, rd: u32, v: V, k: K, signed: bool) {
        // A float's bits load unextended.
        let signed = signed && !k.is_float();
        if let V::M(m) = v {
            if let Some(h) = self.find(m, k.size(), false) {
                self.consume(m);
                let r = h.reg;
                if k.size() == 8 || h.ext == Some(signed) {
                    if r != rd {
                        self.ins(0xaa00_03e0 | (r << 16) | rd); // mov
                        self.hold(rd, false, m, k.size(), h.ext);
                    }
                    return;
                }
                let w = match (k.size(), signed) {
                    (1, true) => 0x9340_1c00 | (r << 5),  // sxtb x
                    (2, true) => 0x9340_3c00 | (r << 5),  // sxth x
                    (4, true) => 0x9340_7c00 | (r << 5),  // sxtw
                    (1, false) => 0x5300_1c00 | (r << 5), // uxtb
                    (2, false) => 0x5300_3c00 | (r << 5), // uxth
                    _ => 0x2a00_03e0 | (r << 16),         // mov w
                };
                self.ins(w | rd);
                self.hold(rd, false, m, k.size(), Some(signed));
                return;
            }
        }
        match v {
            V::I(i) => {
                let bits = k.bits();
                let x = if bits == 64 {
                    i as u64
                } else if signed {
                    ((i << (64 - bits)) >> (64 - bits)) as u64
                } else {
                    (i as u64) & ((1u64 << bits) - 1)
                };
                self.mov_imm(rd, x);
            }
            V::M(m) => {
                let (b, o) = self.base(m, X14);
                self.ldst(Self::load_op(k, signed), rd, b, o);
                self.hold(rd, false, m, k.size(), Some(signed));
            }
        }
    }

    /// A register holding `v` as `k` extended by `signed`, not in `avoid`
    /// (registers written before this is read): one already holding it,
    /// else a loaded value register; never `x14`, `x15`, `x17`.
    fn reg(&mut self, v: V, k: K, signed: bool, avoid: &[u32]) -> u32 {
        let signed = signed && !k.is_float();
        if let V::M(m) = v {
            if let Some(h) = self.find(m, k.size(), false) {
                if !matches!(h.reg, X14 | X15 | X17)
                    && !avoid.contains(&h.reg)
                    && (k.size() == 8 || h.ext == Some(signed))
                {
                    self.consume(m);
                    return h.reg;
                }
            }
        }
        let r = self.pick(&POOL, avoid);
        self.load(r, v, k, signed);
        r
    }

    /// `reg` for a float: a vector register.
    fn freg(&mut self, v: V, k: K, avoid: &[u32]) -> u32 {
        if let V::M(m) = v {
            if let Some(h) = self.find(m, k.size(), true) {
                if !avoid.contains(&h.reg) {
                    self.consume(m);
                    return h.reg;
                }
            }
        }
        let r = self.pick(&FP_POOL, avoid);
        self.load_fp(r, v, k);
        r
    }

    fn store(&mut self, rs: u32, m: Mem, k: K) {
        self.store_ext(rs, m, k, None);
    }

    /// Store `rs`, whose upper bits are known (`ext`) or not.
    fn store_ext(&mut self, rs: u32, m: Mem, k: K, ext: Option<bool>) {
        self.put(Self::store_op(k), rs, m, k.size());
        self.hold(rs, false, m, k.size(), ext);
    }

    /// Store `rs` (`op`, `size` bytes) at `m`: later, for a `once` slot.
    fn put(&mut self, op: Ls, rs: u32, m: Mem, size: u32) {
        let (b, o) = self.base(m, X15);
        self.wrote(m, size);
        if let Mem::Frame(slot, off) = m {
            let fits = o % i64::from(size) == 0 && o / i64::from(size) < 4096;
            match self.waits.get(&(slot, off)) {
                Some(&(z, 0)) if z == size => return,
                Some(&(z, 1)) if z == size && fits => {
                    self.pending.push(Pend {
                        reg: rs,
                        op,
                        slot,
                        off,
                        at: o,
                    });
                    return;
                }
                _ => {}
            }
        }
        self.ldst(op, rs, b, o);
    }

    fn load_fp(&mut self, vd: u32, v: V, k: K) {
        if let V::M(m) = v {
            if let Some(h) = self.find(m, k.size(), false) {
                self.consume(m);
                // fmov s, w / d, x
                let w = if k == K::F64 {
                    0x9e67_0000
                } else {
                    0x1e27_0000
                };
                self.ins(w | (h.reg << 5) | vd);
                self.hold(vd, true, m, k.size(), None);
                return;
            }
            if let Some(h) = self.find(m, k.size(), true) {
                self.consume(m);
                if h.reg != vd {
                    // fmov s / d
                    let ty = if k == K::F64 { 1 << 22 } else { 0 };
                    self.ins(0x1e20_4000 | ty | (h.reg << 5) | vd);
                    self.hold(vd, true, m, k.size(), None);
                }
                return;
            }
        }
        match v {
            V::I(bits) => {
                self.mov_imm(X13, bits as u64);
                // fmov s/d, w/x
                self.ins(
                    if k == K::F32 {
                        0x1e27_0000
                    } else {
                        0x9e67_0000
                    } | (X13 << 5)
                        | vd,
                );
            }
            V::M(m) => {
                let (b, o) = self.base(m, X14);
                self.ldst(if k == K::F32 { LDRS } else { LDRD }, vd, b, o);
                self.hold(vd, true, m, k.size(), None);
            }
        }
    }

    fn store_fp(&mut self, vs: u32, m: Mem, k: K) {
        self.put(if k == K::F32 { STRS } else { STRD }, vs, m, k.size());
        self.hold(vs, true, m, k.size(), None);
    }

    fn b(&mut self, l: Label) {
        self.flush(None);
        let at = self.pos();
        self.ins(0x1400_0000);
        self.fixes.push((at, Fix::B(l)));
    }

    fn bcond(&mut self, cond: u32, l: Label) {
        self.flush(None);
        // Inverted around an unconditional branch: any distance.
        self.ins(0x5400_0000 | (2 << 5) | (cond ^ 1));
        self.b(l);
    }

    fn cset(&mut self, rd: u32, cond: u32) {
        self.ins(0x1a9f_07e0 | ((cond ^ 1) << 12) | rd);
    }

    /// `cmp xn, xm` (`sf` from `k`).
    fn cmp_rr(&mut self, k: K, rn: u32, rm: u32) {
        self.ins(0x6b00_001f | sf(k) | (rm << 16) | (rn << 5));
    }

    fn prologue(&mut self) {
        for w in PROLOGUE {
            self.ins(w);
        }
        let n = self.frame;
        if n == 0 {
            return;
        }
        // Touch every page on the way down (a guard page cannot be skipped).
        let pages = n / PAGE;
        if pages > 0 {
            if pages > 8 {
                self.mov_imm(X17, u64::from(pages));
                let top = self.pos();
                self.ins(0xd140_07ff); // sub sp, sp, #1, lsl #12
                self.ins(0xf900_03ff); // str xzr, [sp]
                self.ins(0xf100_0631); // subs x17, x17, #1
                let d = top as i64 - self.pos() as i64;
                self.ins(0x5400_0000 | (((d as u32) & 0x7ffff) << 5) | NE);
            } else {
                for _ in 0..pages {
                    self.ins(0xd140_07ff);
                    self.ins(0xf900_03ff);
                }
            }
        }
        let rest = n % PAGE;
        if rest != 0 {
            self.add_imm(SP, SP, -i64::from(rest));
        }
    }

    fn epilogue(&mut self) {
        for w in EPILOGUE {
            self.ins(w);
        }
    }

    fn copy(&mut self, dst: Mem, src: Mem, size: u32, zero: bool) {
        let words = size / 8;
        if words <= 16 {
            // Straight through the bases; x16 carries the data (never
            // `Held` here, so neither base is it).
            self.clobber(X16);
            let (sb, soff) = if zero { (ZR, 0) } else { self.base(src, X14) };
            // The destination's pointer must not land on the source's.
            let (db, doff) = self.base(dst, if sb == X15 { X14 } else { X15 });
            let mut off = 0i64;
            for (n, ld, st) in [
                (8, LDRX, STRX),
                (4, LDRW, STRW),
                (2, LDRH, STRH),
                (1, LDRB, STRB),
            ] {
                while (size as i64 - off) >= n {
                    if zero {
                        self.ldst(st, ZR, db, doff + off);
                    } else {
                        self.ldst(ld, X16, sb, soff + off);
                        self.ldst(st, X16, db, doff + off);
                    }
                    off += n;
                }
            }
            self.wrote(dst, size);
            return;
        }
        // A loop: nothing may be emitted into it.
        self.flush(None);
        // Addresses into x14 (source) and x15 (destination).
        if !zero {
            self.addr(X14, src);
        }
        self.addr(X15, dst);
        let mut off = 0i64;
        {
            self.mov_imm(X13, u64::from(words));
            let top = self.pos();
            if zero {
                self.ins(0xf800_85ff); // str xzr, [x15], #8
            } else {
                self.ins(0xf840_85c9); // ldr x9, [x14], #8
                self.ins(0xf800_85e9); // str x9, [x15], #8
            }
            self.ins(0xf100_05ad); // subs x13, x13, #1
            let d = top as i64 - self.pos() as i64;
            self.ins(0x5400_0000 | (((d as u32) & 0x7ffff) << 5) | NE);
        }
        // Post-indexed: the bases moved.
        self.clobber(X14);
        self.clobber(X15);
        self.wrote(dst, size);
        let mut rest = size % 8;
        for (n, ld, st) in [(4, LDRW, STRW), (2, LDRH, STRH), (1, LDRB, STRB)] {
            while rest >= n {
                if zero {
                    self.ldst(st, ZR, X15, off);
                } else {
                    self.ldst(ld, X9, X14, off);
                    self.ldst(st, X9, X15, off);
                }
                off += i64::from(n);
                rest -= n;
            }
        }
    }

    fn bin_int(&mut self, op: Bin, k: K, signed: bool, rd: u32, a: u32, b: u32) {
        let s = sf(k);
        let w = match op {
            Bin::Add => 0x0b00_0000 | s | (b << 16) | (a << 5) | rd,
            Bin::Sub => 0x4b00_0000 | s | (b << 16) | (a << 5) | rd,
            Bin::Mul => 0x1b00_7c00 | s | (b << 16) | (a << 5) | rd,
            Bin::Div => {
                (if signed { 0x1ac0_0c00 } else { 0x1ac0_0800 }) | s | (b << 16) | (a << 5) | rd
            }
            Bin::Rem => {
                let q = X12;
                self.ins(
                    (if signed { 0x1ac0_0c00 } else { 0x1ac0_0800 }) | s | (b << 16) | (a << 5) | q,
                );
                // msub rd, q, b, a
                0x1b00_8000 | s | (b << 16) | (a << 10) | (q << 5) | rd
            }
            Bin::And => 0x0a00_0000 | s | (b << 16) | (a << 5) | rd,
            Bin::Or => 0x2a00_0000 | s | (b << 16) | (a << 5) | rd,
            Bin::Xor => 0x4a00_0000 | s | (b << 16) | (a << 5) | rd,
            Bin::Shl => 0x1ac0_2000 | s | (b << 16) | (a << 5) | rd,
            Bin::Shr => {
                (if signed { 0x1ac0_2800 } else { 0x1ac0_2400 }) | s | (b << 16) | (a << 5) | rd
            }
        };
        self.ins(w);
    }

    /// `rd = ra <op> imm` in one instruction, when `imm` encodes: add /
    /// sub of 12 bits, shifts, a multiply by a power of two.
    #[allow(clippy::too_many_arguments)]
    fn bin_imm(&mut self, op: Bin, k: K, signed: bool, rd: u32, ra: u32, imm: i64) -> bool {
        let s = sf(k);
        let bits: i64 = if s == 0 { 32 } else { 64 };
        let (ubfm, sbfm) = if s == 0 {
            (0x5300_0000, 0x1300_0000)
        } else {
            (0xd340_0000, 0x9340_0000)
        };
        let lsl =
            |n: i64| ubfm | ((((bits - n) % bits) as u32) << 16) | (((bits - 1 - n) as u32) << 10);
        let w = match op {
            Bin::Add | Bin::Sub if (-4095..4096).contains(&imm) => {
                let sub = (op == Bin::Sub) != (imm < 0);
                (if sub { 0x5100_0000 } else { 0x1100_0000 })
                    | s
                    | ((imm.unsigned_abs() as u32) << 10)
            }
            Bin::Mul
                if imm > 0
                    && (imm as u64).is_power_of_two()
                    && i64::from(imm.trailing_zeros()) < bits =>
            {
                lsl(i64::from(imm.trailing_zeros()))
            }
            Bin::Shl => lsl(imm & (bits - 1)),
            Bin::Shr => {
                let n = (imm & (bits - 1)) as u32;
                (if signed { sbfm } else { ubfm }) | (n << 16) | (((bits - 1) as u32) << 10)
            }
            _ => return false,
        };
        self.ins(w | (ra << 5) | rd);
        true
    }

    /// `cmp rn, #imm` (or `cmn`) when the immediate `imm` of `k`
    /// (extended by `signed`, as `load` would) encodes.
    fn cmp_imm(&mut self, k: K, signed: bool, rn: u32, imm: i64) -> bool {
        let bits = k.bits();
        let x = if bits == 64 {
            imm
        } else if signed {
            (imm << (64 - bits)) >> (64 - bits)
        } else {
            imm & ((1i64 << bits) - 1)
        };
        // 8 / 16 / 32 bits compare as 32.
        let v = if sf(k) == 0 { i64::from(x as i32) } else { x };
        if !(-4095..4096).contains(&v) {
            return false;
        }
        let op = if v < 0 { 0x3100_001f } else { 0x7100_001f };
        self.ins(op | sf(k) | ((v.unsigned_abs() as u32) << 10) | (rn << 5));
        true
    }

    fn inst(&mut self, i: &Inst, next: Option<&Inst>) {
        match i {
            Inst::Label(l) => {
                self.flush(None);
                self.labels[*l as usize] = Some(self.pos());
                self.forget();
            }
            Inst::Jmp(l) => {
                if !matches!(next, Some(Inst::Label(n)) if n == l) {
                    self.b(*l);
                }
            }
            Inst::Br { c, t, f } => {
                if let V::I(v) = c {
                    self.b(if *v & 0xff != 0 { *t } else { *f });
                    return;
                }
                if let Some(cond) = self.flags.take() {
                    // The comparison just before: its flags.
                    self.flush(None);
                    self.ins(0x5400_0000 | (2 << 5) | (cond ^ 1)); // b.!cond +8
                } else {
                    let r = self.reg(*c, K::I8, false, &[]);
                    self.flush(None);
                    self.ins(0x3400_0000 | (2 << 5) | r); // cbz w, +8
                }
                self.b(*t);
                if !matches!(next, Some(Inst::Label(n)) if n == f) {
                    self.b(*f);
                }
            }
            Inst::Mov { k, dst, src } => {
                let r = self.reg(*src, *k, false, &[]);
                self.store(r, *dst, *k);
            }
            Inst::Copy { dst, src, size } => {
                if let Some(k) = scalar(*size) {
                    // One scalar's bytes: a move.
                    let r = self.reg(V::M(*src), k, false, &[]);
                    self.store(r, *dst, k);
                } else {
                    self.copy(*dst, *src, *size, false);
                }
            }
            Inst::Zero { dst, size } => self.copy(*dst, *dst, *size, true),
            Inst::Addr { dst, src } => {
                let r = self.pick(&POOL, &[]);
                self.addr(r, *src);
                self.store(r, *dst, K::I64);
            }
            Inst::Sym { dst, sym, got } => {
                // COFF has no GOT: everything is in the image (a DLL's
                // functions through import thunks).
                let got = *got && !self.obj.coff;
                let (p, o) = if got {
                    (RelKind::GotPage21, RelKind::GotPageOff12)
                } else {
                    (RelKind::Page21, RelKind::PageOff12)
                };
                let r = self.pick(&POOL, &[]);
                self.reloc(p, *sym);
                self.ins(0x9000_0000 | r); // adrp
                self.reloc(o, *sym);
                if got {
                    self.ins(0xf940_0000 | (r << 5) | r); // ldr r, [r]
                } else {
                    self.ins(0x9100_0000 | (r << 5) | r); // add r, r, #0
                }
                self.store(r, *dst, K::I64);
            }
            Inst::Tls { dst, sym } if self.obj.coff => {
                // The TEB's (x18) TLS array at `_tls_index`, plus the
                // offset in the TLS section.
                let index = self.obj.sym("_tls_index");
                self.ins(0xf940_2e50); // ldr x16, [x18, #0x58]
                self.reloc(RelKind::Page21, index);
                self.ins(0x9000_0000 | X17); // adrp x17
                self.reloc(RelKind::PageOff12, index);
                self.ins(0xb940_0000 | (X17 << 5) | X17); // ldr w17, [x17]
                self.ins(0xf871_7a10); // ldr x16, [x16, x17, lsl #3]
                self.reloc(RelKind::SecRelHi12, *sym);
                self.ins(0x9140_0200); // add x0, x16, #0, lsl #12
                self.reloc(RelKind::SecRelLo12, *sym);
                self.ins(0x9100_0000); // add x0, x0, #0
                self.store(0, *dst, K::I64);
            }
            Inst::Tls { dst, sym } => {
                self.reloc(RelKind::TlvPage21, *sym);
                self.ins(0x9000_0000); // adrp x0
                self.reloc(RelKind::TlvPageOff12, *sym);
                self.ins(0xf940_0000); // ldr x0, [x0]
                if self.obj.elf {
                    // Initial exec: the thread pointer plus the offset.
                    self.ins(0xd53b_d050); // mrs x16, tpidr_el0
                    self.ins(0x8b00_0200); // add x0, x16, x0
                } else {
                    // Mach-O: the descriptor's getter.
                    self.ins(0xf940_0010); // ldr x16, [x0]
                    self.flush(None);
                    self.ins(0xd63f_0200); // blr x16
                    self.forget();
                }
                self.store(0, *dst, K::I64);
            }
            Inst::Bin {
                op,
                k,
                signed,
                dst,
                a,
                b,
            } => {
                if k.is_float() {
                    let va = self.freg(*a, *k, &[]);
                    let vb = if b == a { va } else { self.freg(*b, *k, &[va]) };
                    let vd = self.pick(&FP_POOL, &[]);
                    let ty = if *k == K::F64 { 1 << 22 } else { 0 };
                    let opc = match op {
                        Bin::Add => 0x1e20_2800,
                        Bin::Sub => 0x1e20_3800,
                        Bin::Mul => 0x1e20_0800,
                        _ => 0x1e20_1800,
                    };
                    self.ins(opc | ty | (vb << 16) | (va << 5) | vd);
                    self.store_fp(vd, *dst, *k);
                    return;
                }
                // `rem` computes the quotient into x12 first.
                let fixed: &[u32] = if *op == Bin::Rem { &[X12] } else { &[] };
                let ra = self.reg(*a, *k, *signed, fixed);
                if let V::I(imm) = b {
                    let rd = self.pick(&POOL, &[]);
                    if self.bin_imm(*op, *k, *signed, rd, ra, *imm) {
                        self.store(rd, *dst, *k);
                        return;
                    }
                }
                let rb = if b == a {
                    ra
                } else {
                    self.reg(*b, *k, *signed, &[&[ra], fixed].concat())
                };
                let rd = self.pick(&POOL, fixed);
                self.bin_int(*op, *k, *signed, rd, ra, rb);
                self.store(rd, *dst, *k);
            }
            Inst::Ovf {
                op,
                k,
                signed,
                dst,
                a,
                b,
                fail,
            } => self.ovf(*op, *k, *signed, *dst, *a, *b, *fail),
            Inst::Un { op, k, dst, a } => self.un(*op, *k, *dst, *a),
            Inst::Cmp {
                cc,
                k,
                signed,
                dst,
                a,
                b,
            } => {
                let cond = if k.is_float() {
                    let va = self.freg(*a, *k, &[]);
                    let vb = if b == a { va } else { self.freg(*b, *k, &[va]) };
                    let ty = if *k == K::F64 { 1 << 22 } else { 0 };
                    self.ins(0x1e20_2000 | ty | (vb << 16) | (va << 5));
                    match cc {
                        Cc::Eq => EQ,
                        Cc::Ne => NE,
                        Cc::Lt => MI,
                        Cc::Le => LS_,
                        Cc::Gt => GT,
                        Cc::Ge => GE,
                    }
                } else {
                    let ra = self.reg(*a, *k, *signed, &[]);
                    let imm = matches!(b, V::I(v) if self.cmp_imm(*k, *signed, ra, *v));
                    if !imm {
                        let rb = if b == a {
                            ra
                        } else {
                            self.reg(*b, *k, *signed, &[ra])
                        };
                        self.cmp_rr(*k, ra, rb);
                    }
                    match (cc, signed) {
                        (Cc::Eq, _) => EQ,
                        (Cc::Ne, _) => NE,
                        (Cc::Lt, true) => LT,
                        (Cc::Lt, false) => LO,
                        (Cc::Le, true) => LE,
                        (Cc::Le, false) => LS_,
                        (Cc::Gt, true) => GT,
                        (Cc::Gt, false) => HI,
                        (Cc::Ge, true) => GE,
                        (Cc::Ge, false) => HS,
                    }
                };
                // A branch right after on it takes the flags; when it is
                // the only reader, nothing else is left to do.
                if matches!(next, Some(Inst::Br { c: V::M(m), .. }) if m == dst) {
                    self.flags = Some(cond);
                    if let Mem::Frame(s, o) = dst {
                        if self.waits.get(&(*s, *o)) == Some(&(1, 1)) {
                            return;
                        }
                    }
                }
                let rd = self.pick(&POOL, &[]);
                self.cset(rd, cond);
                self.store_ext(rd, *dst, K::I8, Some(false));
            }
            Inst::Conv {
                conv,
                from,
                to,
                dst,
                a,
            } => self.conv(*conv, *from, *to, *dst, *a),
            Inst::AtomicLoad { k, dst, ptr } => {
                self.addr_of_ptr(X14, *ptr);
                let sz = k.size().trailing_zeros();
                self.ins((sz << 30) | 0x08df_fc00 | (X14 << 5) | X9);
                self.store(X9, *dst, *k);
            }
            Inst::AtomicStore { k, ptr, src } => {
                self.load(X9, *src, *k, false);
                self.addr_of_ptr(X14, *ptr);
                let sz = k.size().trailing_zeros();
                self.ins((sz << 30) | 0x089f_fc00 | (X14 << 5) | X9);
                self.wrote(*ptr, k.size());
            }
            Inst::AtomicRmw {
                op,
                k,
                dst,
                ptr,
                val,
            } => {
                self.load(X10, *val, *k, false);
                self.addr_of_ptr(X14, *ptr);
                let sz = k.size().trailing_zeros();
                let opc = match op {
                    Rmw::Add => 0,
                    Rmw::Sub => {
                        self.ins(0xcb0a_03ea); // neg x10, x10
                        0
                    }
                    Rmw::And => {
                        self.ins(0xaa2a_03ea); // mvn x10, x10
                        0x1000
                    }
                    Rmw::Xor => 0x2000,
                    Rmw::Or => 0x3000,
                    Rmw::Xchg => 0x8000,
                };
                self.ins((sz << 30) | 0x38e0_0000 | opc | (X10 << 16) | (X14 << 5) | X9);
                self.wrote(*ptr, k.size());
                self.store(X9, *dst, *k);
            }
            Inst::Cas {
                k,
                dst,
                ptr,
                expected,
                new,
            } => {
                self.load(X9, *expected, *k, false);
                self.load(X10, *new, *k, false);
                self.addr_of_ptr(X14, *ptr);
                let sz = k.size().trailing_zeros();
                // `cas` writes `rs`.
                self.flush_reg(X9);
                self.ins((sz << 30) | 0x08e0_fc00 | (X9 << 16) | (X14 << 5) | X10);
                self.wrote(*ptr, k.size());
                // `cas` writes `rs`.
                self.clobber(X9);
                self.store(X9, *dst, *k);
            }
            Inst::Call {
                callee,
                args,
                rets,
                sret,
                ..
            } => {
                for (p, v) in args {
                    match p.loc {
                        Loc::Stack(o) => {
                            if p.k.is_float() {
                                self.load(X9, *v, p.k, false);
                            } else {
                                self.load(X9, *v, p.k, p.signed);
                            }
                            self.ldst(Self::store_op(p.k), X9, SP, i64::from(o));
                        }
                        Loc::Int(r) => self.load(u32::from(r), *v, p.k, p.signed),
                        Loc::Fp(r) => self.load_fp(u32::from(r), *v, p.k),
                        Loc::Sret => {}
                    }
                }
                if let Some(m) = sret {
                    self.addr(8, *m);
                }
                if let Callee::Ptr(m) = callee {
                    self.load(X16, V::M(*m), K::I64, false);
                }
                self.flush(None);
                match callee {
                    Callee::Sym(s) => {
                        self.reloc(RelKind::Branch26, *s);
                        self.ins(0x9400_0000);
                    }
                    Callee::Ptr(_) => self.ins(0xd63f_0000 | (X16 << 5)),
                }
                self.forget();
                for (p, m) in rets {
                    match p.loc {
                        Loc::Int(r) => self.store(u32::from(r), *m, p.k),
                        Loc::Fp(r) => self.store_fp(u32::from(r), *m, p.k),
                        _ => {}
                    }
                }
            }
            Inst::Param { part, dst } => match part.loc {
                Loc::Int(r) => self.store(u32::from(r), *dst, part.k),
                Loc::Fp(r) => self.store_fp(u32::from(r), *dst, part.k),
                Loc::Stack(o) => {
                    self.ldst(Self::load_op(part.k, false), X9, FP, 16 + i64::from(o));
                    self.store(X9, *dst, part.k);
                }
                Loc::Sret => self.store(8, *dst, K::I64),
            },
            Inst::Ret { vals } => {
                for (p, v) in vals {
                    match p.loc {
                        Loc::Int(r) => self.load(u32::from(r), *v, p.k, p.signed),
                        Loc::Fp(r) => self.load_fp(u32::from(r), *v, p.k),
                        _ => {}
                    }
                }
                // The frame dies.
                self.pending.clear();
                self.epilogue();
                self.forget();
            }
            Inst::Trap => {
                self.pending.clear();
                self.ins(0xd420_0020); // brk #1
            }
            Inst::Line(line, col, scope) => {
                let at = 4 * self.code.len() as u32;
                self.lines.push((at, *line, *col, *scope));
            }
        }
    }

    /// The address a `ptr` operand of an atomic names, in `rd`.
    fn addr_of_ptr(&mut self, rd: u32, m: Mem) {
        self.addr(rd, m);
    }

    #[allow(clippy::too_many_arguments)]
    fn ovf(&mut self, op: Bin, k: K, signed: bool, dst: Mem, a: V, b: V, fail: Label) {
        self.load(X9, a, k, signed);
        if let (K::I64 | K::I32, Bin::Add | Bin::Sub, V::I(imm @ 0..4096)) = (k, op, b) {
            // adds / subs #imm
            let base = if op == Bin::Add {
                0x3100_0000
            } else {
                0x7100_0000
            };
            self.ins(base | sf(k) | ((imm as u32) << 10) | (X9 << 5) | X11);
            let cond = match (op, signed) {
                (_, true) => VS,
                (Bin::Add, false) => HS,
                _ => LO,
            };
            self.bcond(cond, fail);
            self.store(X11, dst, k);
            return;
        }
        self.load(X10, b, k, signed);
        match (k, op) {
            (K::I64, Bin::Add | Bin::Sub) => {
                let base = if op == Bin::Add {
                    0xab00_0000
                } else {
                    0xeb00_0000
                };
                self.ins(base | (X10 << 16) | (X9 << 5) | X11);
                let cond = match (op, signed) {
                    (_, true) => VS,
                    (Bin::Add, false) => HS,
                    _ => LO,
                };
                self.bcond(cond, fail);
            }
            (K::I64, _) => {
                self.ins(0x9b00_7c00 | (X10 << 16) | (X9 << 5) | X11); // mul
                if signed {
                    self.ins(0x9b40_7c00 | (X10 << 16) | (X9 << 5) | X12); // smulh
                    // cmp x12, x11, asr #63
                    self.ins(0xeb80_001f | (X11 << 16) | (63 << 10) | (X12 << 5));
                } else {
                    self.ins(0x9bc0_7c00 | (X10 << 16) | (X9 << 5) | X12); // umulh
                    self.ins(0xeb1f_001f | (X12 << 5)); // cmp x12, xzr
                }
                self.bcond(NE, fail);
            }
            (K::I32, Bin::Add | Bin::Sub) => {
                let base = if op == Bin::Add {
                    0x2b00_0000
                } else {
                    0x6b00_0000
                };
                self.ins(base | (X10 << 16) | (X9 << 5) | X11);
                let cond = match (op, signed) {
                    (_, true) => VS,
                    (Bin::Add, false) => HS,
                    _ => LO,
                };
                self.bcond(cond, fail);
            }
            (K::I32, _) => {
                // smull / umull x11, w9, w10; the product must fit 32 bits.
                let m = if signed { 0x9b20_7c00 } else { 0x9ba0_7c00 };
                self.ins(m | (X10 << 16) | (X9 << 5) | X11);
                let option = if signed { 0b110 } else { 0b010 };
                // cmp x11, w11, sxtw / uxtw
                self.ins(0xeb20_001f | (X11 << 16) | (option << 13) | (X11 << 5));
                self.bcond(NE, fail);
            }
            _ => {
                // 8 / 16 bits, computed in 32 from extended operands: the
                // result must equal its own extension.
                self.bin_int(op, K::I32, signed, X11, X9, X10);
                let option = match (k, signed) {
                    (K::I8, true) => 0b100,
                    (K::I16, true) => 0b101,
                    (K::I8, false) => 0b000,
                    _ => 0b001,
                };
                self.ins(0x6b20_001f | (X11 << 16) | (option << 13) | (X11 << 5));
                self.bcond(NE, fail);
            }
        }
        self.store(X11, dst, k);
    }

    fn un(&mut self, op: Un, k: K, dst: Mem, a: V) {
        if k.is_float() {
            let va = self.freg(a, k, &[]);
            let vd = self.pick(&FP_POOL, &[va]);
            let ty = if k == K::F64 { 1 << 22 } else { 0 };
            let opc = if op == Un::Sqrt {
                0x1e21_c000
            } else {
                0x1e21_4000
            };
            self.ins(opc | ty | (va << 5) | vd);
            self.store_fp(vd, dst, k);
            return;
        }
        let s = sf(k);
        let bits = k.bits();
        match op {
            Un::Neg => {
                self.load(X9, a, k, true);
                self.ins(0x4b00_03e0 | s | (X9 << 16) | X9);
            }
            Un::Not => {
                self.load(X9, a, k, false);
                self.ins(0x2a20_03e0 | s | (X9 << 16) | X9);
            }
            Un::Popcnt => {
                self.load(X9, a, k, false);
                self.ins(0x9e67_0000 | (X9 << 5) | V16); // fmov d16, x9
                self.ins(0x0e20_5800 | (V16 << 5) | V16); // cnt v16.8b
                self.ins(0x0e31_b800 | (V16 << 5) | V16); // addv b16, v16.8b
                self.ins(0x1e26_0000 | (V16 << 5) | X9); // fmov w9, s16
            }
            Un::Clz => {
                self.load(X9, a, k, false);
                self.ins(0x5ac0_1000 | s | (X9 << 5) | X9);
                if bits < 32 {
                    // sub w9, w9, #(32 - bits)
                    self.ins(0x5100_0000 | ((32 - bits) << 10) | (X9 << 5) | X9);
                }
            }
            Un::Ctz => {
                self.load(X9, a, k, false);
                if bits < 32 {
                    self.mov_imm(X10, 1 << bits);
                    self.ins(0x2a00_0000 | (X10 << 16) | (X9 << 5) | X9);
                }
                self.ins(0x5ac0_0000 | s | (X9 << 5) | X9); // rbit
                self.ins(0x5ac0_1000 | s | (X9 << 5) | X9); // clz
            }
            Un::Bswap => {
                self.load(X9, a, k, false);
                match k {
                    K::I16 => self.ins(0x5ac0_0400 | (X9 << 5) | X9),
                    K::I32 => self.ins(0x5ac0_0800 | (X9 << 5) | X9),
                    K::I64 => self.ins(0xdac0_0c00 | (X9 << 5) | X9),
                    _ => {}
                }
            }
            Un::Sqrt => self.load(X9, a, k, false),
        }
        self.store(X9, dst, k);
    }

    fn conv(&mut self, conv: Conv, from: K, to: K, dst: Mem, a: V) {
        match conv {
            Conv::Int { signed } => {
                self.load(X9, a, from, signed);
                self.store(X9, dst, to);
            }
            Conv::ToBool => {
                self.load(X9, a, from, false);
                self.ins(0xf100_001f | (X9 << 5)); // cmp x9, #0
                self.cset(X9, NE);
                self.store(X9, dst, K::I8);
            }
            Conv::IToF { signed } => {
                self.load(X9, a, from, signed);
                let base = match (to, signed) {
                    (K::F32, true) => 0x9e22_0000,
                    (K::F32, false) => 0x9e23_0000,
                    (_, true) => 0x9e62_0000,
                    (_, false) => 0x9e63_0000,
                };
                self.ins(base | (X9 << 5) | V16);
                self.store_fp(V16, dst, to);
            }
            Conv::FToI { signed, bits } => {
                self.load_fp(V16, a, from);
                let ty = if from == K::F64 { 1 << 22 } else { 0 };
                let op = if signed { 0x9e38_0000 } else { 0x9e39_0000 };
                self.ins(op | ty | (V16 << 5) | X9);
                if bits < 64 {
                    let (max, min) = if signed {
                        ((1i64 << (bits - 1)) - 1, -(1i64 << (bits - 1)))
                    } else {
                        ((1i64 << bits) - 1, 0)
                    };
                    self.mov_imm(X10, max as u64);
                    self.cmp_rr(K::I64, X9, X10);
                    // csel x9, x10, x9, gt / hi
                    let c = if signed { GT } else { HI };
                    self.ins(0x9a80_0000 | (X9 << 16) | (c << 12) | (X10 << 5) | X9);
                    if signed {
                        self.mov_imm(X10, min as u64);
                        self.cmp_rr(K::I64, X9, X10);
                        self.ins(0x9a80_0000 | (X9 << 16) | (LT << 12) | (X10 << 5) | X9);
                    }
                }
                self.store(X9, dst, to);
            }
            Conv::FToF => {
                self.load_fp(V16, a, from);
                if from == K::F32 {
                    self.ins(0x1e22_c000 | (V16 << 5) | V16); // fcvt d, s
                } else {
                    self.ins(0x1e62_4000 | (V16 << 5) | V16); // fcvt s, d
                }
                self.store_fp(V16, dst, to);
            }
            Conv::Bits => {
                self.load(X9, a, from, false);
                self.store(X9, dst, to);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn em(obj: &mut Obj) -> Em<'_> {
        Em {
            obj,
            start: 0,
            code: Vec::new(),
            relocs: Vec::new(),
            fixes: Vec::new(),
            labels: Vec::new(),
            slot_off: Vec::new(),
            frame: 0,
            lines: Vec::new(),
            held: Vec::new(),
            escaped: Vec::new(),
            pending: Vec::new(),
            waits: Waits::default(),
            next_rel: None,
            flags: None,
        }
    }

    /// The unwind encodings describe `PROLOGUE` only: no callee-saved
    /// register (`x19`..`x28`, `v8`..`v15`) may hold a value.
    #[test]
    fn values_stay_in_caller_saved_registers() {
        for r in POOL.iter().chain(&[X14, X15, X16, X17]) {
            assert!(!(19..=28).contains(r), "x{r}");
        }
        for v in FP_POOL {
            assert!(!(8..=15).contains(&v), "v{v}");
        }
        let mut obj = Obj::new(false, false, false);
        let mut e = em(&mut obj);
        e.frame = 3 * PAGE + 32;
        e.prologue();
        assert_eq!(e.code[..2], PROLOGUE);
    }

    /// Run the move-wide and 64-bit add / sub (immediate, extended
    /// register) words of `code` on `x` (31 is `sp`).
    fn run(code: &[u32], x: &mut [u64; 32]) {
        for &w in code {
            let (rd, rn) = ((w & 31) as usize, ((w >> 5) & 31) as usize);
            let hw = (w >> 21) & 3;
            let imm16 = u64::from((w >> 5) & 0xffff) << (16 * hw);
            match w & 0xff80_0000 {
                0xd280_0000 => x[rd] = imm16,
                0x9280_0000 => x[rd] = !imm16,
                0xf280_0000 => x[rd] = (x[rd] & !(0xffff << (16 * hw))) | imm16,
                op @ (0x9100_0000 | 0xd100_0000) => {
                    let imm = u64::from((w >> 10) & 0xfff) << (12 * ((w >> 22) & 1));
                    x[rd] = if op == 0x9100_0000 {
                        x[rn].wrapping_add(imm)
                    } else {
                        x[rn].wrapping_sub(imm)
                    };
                }
                _ if w & 0xffe0_fc00 == 0x8b20_6000 => {
                    x[rd] = x[rn].wrapping_add(x[((w >> 16) & 31) as usize]);
                }
                _ => panic!("unexpected word {w:#010x}"),
            }
        }
    }

    /// Halfword patterns: zeros, ones, and mixed values.
    fn values() -> Vec<u64> {
        let mut out = vec![0, 1, 0xffff, 0x1_0000, u64::MAX, u64::MAX - 1, 1 << 63];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..2000 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let mut v = 0u64;
            for h in 0..4 {
                let part = match (seed >> (h * 8)) & 3 {
                    0 => 0,
                    1 => 0xffff,
                    _ => (seed >> (16 + h * 11)) & 0xffff,
                };
                v |= part << (16 * h);
            }
            out.push(v);
        }
        out
    }

    #[test]
    fn mov_imm_builds_every_value_in_fewest_words() {
        let mut obj = Obj::new(false, false, false);
        for v in values() {
            let mut e = em(&mut obj);
            e.mov_imm(X9, v);
            let mut x = [0xdead_beef_u64; 32];
            run(&e.code, &mut x);
            assert_eq!(x[9], v, "{v:#x}: {:x?}", e.code);
            let count = |y: u64| (0..4).filter(|h| (y >> (16 * h)) & 0xffff != 0).count();
            let best = count(v).min(count(!v)).max(1);
            assert_eq!(e.code.len(), best, "{v:#x}: {:x?}", e.code);
        }
    }

    #[test]
    fn add_imm_reaches_far_offsets() {
        let mut obj = Obj::new(false, false, false);
        let imms = [
            0,
            1,
            -1,
            8,
            4095,
            4096,
            4097,
            -4096,
            -4097,
            0xff_ffff,
            -0xff_ffff,
            1 << 24,
            -(1 << 24) - 8,
            i64::from(i32::MAX),
            -(1 << 40),
        ];
        for imm in imms {
            for (rd, rn) in [(X9, X9), (X9, SP), (SP, SP), (X10, X9)] {
                let mut e = em(&mut obj);
                e.add_imm(rd, rn, imm);
                let mut x = [0u64; 32];
                x[rn as usize] = 0x1000_0000_0000;
                run(&e.code, &mut x);
                let want = 0x1000_0000_0000u64.wrapping_add(imm as u64);
                assert_eq!(x[rd as usize], want, "{imm} x{rd} x{rn}: {:x?}", e.code);
                assert!(e.code.len() <= 2 || imm.unsigned_abs() >= 1 << 24, "{imm}");
            }
        }
    }

    /// A load's address: the scaled 12-bit form, the unscaled 9-bit form
    /// or `x17` from `add_imm`.
    #[test]
    fn ldst_addresses_every_offset() {
        let mut obj = Obj::new(false, false, false);
        for op in [LDRX, LDRW, LDRH, LDRB, LDRD] {
            let size = 1i64 << op.1;
            for off in [
                0,
                size,
                -size,
                255,
                -256,
                -257,
                4095 * size,
                4096 * size,
                12,
                301,
                1 << 20,
            ] {
                let mut e = em(&mut obj);
                e.ldst(op, X9, X10, off);
                let (last, setup) = e.code.split_last().unwrap();
                let mut x = [0u64; 32];
                x[10] = 0x10_0000;
                run(setup, &mut x);
                let base = x[((last >> 5) & 31) as usize];
                let addr = if last & (1 << 24) != 0 {
                    base + (u64::from((last >> 10) & 0xfff) << op.1)
                } else {
                    assert_eq!(last & ((1 << 21) | (3 << 10)), 0, "{last:#x}");
                    let imm9 = i64::from(((last >> 12) & 0x1ff) as i32) << 55 >> 55;
                    base.wrapping_add(imm9 as u64)
                };
                assert_eq!(
                    addr,
                    0x10_0000u64.wrapping_add(off as u64),
                    "{size} {off}: {:x?}",
                    e.code
                );
                assert_eq!(last & 31, X9, "{last:#x}");
                let opcode = |w: u32| w & 0xffc0_0000 & !(1 << 24);
                assert_eq!(opcode(*last), opcode(op.0), "{last:#x}");
            }
        }
    }
}
