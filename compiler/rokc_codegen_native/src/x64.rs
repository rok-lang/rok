//! LIR -> `x86_64` machine code (System V; Windows x64 in COFF objects).
//!
//! Frame: `rbp` pushed, `rbp` = it, then the slots, outgoing arguments at
//! `rsp`. Values: `rax rcx rdx r8 r9`, `xmm0`..`xmm3`; pointer bases `r10
//! r11`. Only registers caller-saved in both conventions, so nothing is
//! saved and unwinding sees a plain `rbp` frame. `Held` / `Pend` as on
//! arm64. Fixed-register instructions (division, conversions, atomics)
//! load operands first, then `scratch`. Baseline `x86_64` only (no
//! `popcnt` / `lzcnt`).

use crate::lir::{
    Bin, Callee, Cc, Conv, Func, Inst, K, Label, Loc, Mem, Rmw, Un, V, Waits, scalar, waitable,
};
use crate::obj::{FnRange, Obj, RelKind, Reloc, Sect};

const RAX: u32 = 0;
const RCX: u32 = 1;
const RDX: u32 = 2;
const RSP: u32 = 4;
const RBP: u32 = 5;
const RSI: u32 = 6;
const RDI: u32 = 7;
const R10: u32 = 10;
const R11: u32 = 11;
const R8: u32 = 8;
const R9: u32 = 9;
const XMM0: u32 = 0;
const XMM1: u32 = 1;
/// Value registers, picked by `pick`.
const POOL: [u32; 5] = [RAX, RCX, RDX, R8, R9];

/// Every prologue (before `sub rsp`, undone via `rbp`): `push rbp; mov rbp,
/// rsp`, as the unwind encodings say.
pub const PROLOGUE: [u8; 4] = [0x55, 0x48, 0x89, 0xe5];
const FP_POOL: [u32; 4] = [XMM0, XMM1, 2, 3];
/// Integer argument registers.
const ARGS: [u32; 6] = [RDI, RSI, RDX, RCX, 8, 9];
/// Windows x64: by register position.
const WIN_ARGS: [u32; 4] = [RCX, RDX, 8, 9];
/// Integer result registers.
const RETS: [u32; 3] = [RAX, RDX, RCX];
/// Stack probe interval.
const PAGE: u32 = 4096;

// Condition codes.
const CC_O: u8 = 0;
const CC_B: u8 = 2;
const CC_AE: u8 = 3;
const CC_E: u8 = 4;
const CC_NE: u8 = 5;
const CC_BE: u8 = 6;
const CC_A: u8 = 7;
const CC_S: u8 = 8;
const CC_P: u8 = 0xa;
const CC_NP: u8 = 0xb;
const CC_L: u8 = 0xc;
const CC_GE: u8 = 0xd;
const CC_LE: u8 = 0xe;
const CC_G: u8 = 0xf;

/// `i` as a `k` immediate extended by `signed` (as `Em::load` does).
const fn ext(i: i64, k: K, signed: bool) -> i64 {
    let bits = k.bits();
    if bits == 64 {
        i
    } else if signed {
        (i << (64 - bits)) >> (64 - bits)
    } else {
        i & ((1i64 << bits) - 1)
    }
}

/// A register-or-memory operand.
#[derive(Clone, Copy)]
enum Rm {
    R(u32),
    /// `[base + disp]`.
    M(u32, i32),
}

/// A register (`fp`: `xmm`) holding `size` bytes at `off` of `slot`;
/// `ext`: upper bits are those bytes extended (`Some(signed)`) or unknown.
#[derive(Clone, Copy)]
struct Held {
    reg: u32,
    fp: bool,
    slot: u32,
    off: u32,
    size: u32,
    ext: Option<bool>,
}

/// A store not yet emitted: `reg` (`k`) into byte `off` of `slot`, at
/// `rsp + disp`.
#[derive(Clone, Copy)]
struct Pend {
    reg: u32,
    fp: bool,
    k: K,
    slot: u32,
    off: u32,
    disp: i32,
}

struct Em<'a> {
    obj: &'a mut Obj,
    start: u32,
    code: Vec<u8>,
    relocs: Vec<(u32, RelKind, u32)>,
    /// `(position of a rel32, label)`.
    fixes: Vec<(usize, Label)>,
    labels: Vec<Option<usize>>,
    slot_off: Vec<u32>,
    frame: u32,
    lines: Vec<(u32, u32, u32, u32)>,
    held: Vec<Held>,
    /// Slots whose address is taken: a store through a pointer may write them.
    escaped: Vec<bool>,
    pending: Vec<Pend>,
    waits: Waits,
    /// A comparison left its result in the flags for the next `Br`.
    flags: Option<u8>,
}

/// Emit `f` into `obj`'s text.
pub fn emit(obj: &mut Obj, f: &Func) {
    let mut out = 0u32;
    for i in &f.insts {
        if let Inst::Call { args, .. } = i {
            // Windows: the callee's 32-byte home area.
            if obj.coff {
                out = out.max(32);
            }
            for (p, _) in args {
                if let Loc::Stack(o) = p.loc {
                    out = out.max(o + 8);
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
    let start = obj.sect(Sect::Text).align_to(16);
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
    fn b(&mut self, x: u8) {
        self.code.push(x);
    }

    fn bytes(&mut self, x: &[u8]) {
        self.code.extend_from_slice(x);
    }

    fn d32(&mut self, x: i32) {
        self.code.extend_from_slice(&x.to_le_bytes());
    }

    fn finish(mut self, sym: u32, debug: Option<(crate::lir::FnDebug, Vec<i32>)>) {
        for (at, l) in std::mem::take(&mut self.fixes) {
            let target = self.labels[l as usize].expect("bound label");
            let d = target as i64 - (at as i64 + 4);
            self.code[at..at + 4].copy_from_slice(&(d as i32).to_le_bytes());
        }
        let len = self.code.len() as u32;
        let start = self.start;
        let text = self.obj.sect(Sect::Text);
        text.data.extend_from_slice(&self.code);
        for (off, kind, s) in self.relocs {
            text.relocs.push(Reloc { off, kind, sym: s });
        }
        let lines = std::mem::take(&mut self.lines);
        self.obj.syms[sym as usize].def = Some((Sect::Text, start, len));
        self.obj.fns.push(FnRange {
            sym,
            start,
            len,
            lines,
            debug,
        });
    }

    /// A `ModRM` instruction: optional prefix, REX (`w` 64-bit; `byte` for
    /// `spl`..`dil`), opcode, `reg`, `rm`.
    fn op(&mut self, pre: Option<u8>, w: bool, byte: bool, opc: &[u8], reg: u32, rm: Rm) {
        if let Some(p) = pre {
            self.b(p);
        }
        let (rmreg, index) = match rm {
            Rm::R(r) => (r, 0),
            Rm::M(base, _) => (base, 0),
        };
        let mut rex = 0x40u8;
        if w {
            rex |= 8;
        }
        if reg >= 8 {
            rex |= 4;
        }
        if index >= 8 {
            rex |= 2;
        }
        if rmreg >= 8 {
            rex |= 1;
        }
        let force =
            byte && ((4..8).contains(&reg) || matches!(rm, Rm::R(r) if (4..8).contains(&r)));
        if rex != 0x40 || force {
            self.b(rex);
        }
        self.bytes(opc);
        let r = ((reg & 7) << 3) as u8;
        match rm {
            Rm::R(x) => self.b(0xc0 | r | (x & 7) as u8),
            Rm::M(base, disp) => {
                let lo = (base & 7) as u8;
                let (md, small) = if disp == 0 && lo != 5 {
                    (0u8, None)
                } else if (-128..128).contains(&disp) {
                    (0x40, Some(true))
                } else {
                    (0x80, Some(false))
                };
                self.b(md | r | lo);
                if lo == 4 {
                    self.b(0x24); // SIB: base only
                }
                match small {
                    Some(true) => self.b(disp as u8),
                    Some(false) => self.d32(disp),
                    None => {}
                }
            }
        }
    }

    /// The integer argument register at position `n`.
    fn arg_reg(&self, n: u8) -> u32 {
        if self.obj.coff {
            WIN_ARGS[n as usize]
        } else {
            ARGS[n as usize]
        }
    }

    /// The hidden result pointer's register.
    fn sret_reg(&self) -> u32 {
        if self.obj.coff { RCX } else { RDI }
    }

    /// `[rip + sym]` with relocation `kind` (the 32-bit field ends the
    /// instruction).
    fn op_rip(&mut self, pre: Option<u8>, w: bool, opc: &[u8], reg: u32, kind: RelKind, sym: u32) {
        if let Some(p) = pre {
            self.b(p);
        }
        let mut rex = 0x40u8;
        if w {
            rex |= 8;
        }
        if reg >= 8 {
            rex |= 4;
        }
        if rex != 0x40 {
            self.b(rex);
        }
        self.bytes(opc);
        self.b((((reg & 7) << 3) | 5) as u8);
        let off = self.start + self.code.len() as u32;
        self.relocs.push((off, kind, sym));
        self.d32(0);
    }

    /// Register `r` (`fp`: an `xmm`) is about to be written: the stores
    /// waiting on it go out first, and what it held is gone.
    fn w(&mut self, r: u32, fp: bool) {
        while let Some(i) = self.pending.iter().position(|p| p.reg == r && p.fp == fp) {
            let p = self.pending.remove(i);
            self.emit_pend(p);
        }
        self.held.retain(|h| h.reg != r || h.fp != fp);
    }

    /// Before code that writes scratch registers on its own: every store
    /// out, no register known.
    fn scratch(&mut self) {
        self.flush(None);
        self.held.clear();
    }

    fn emit_pend(&mut self, p: Pend) {
        let rm = Rm::M(RSP, p.disp);
        if p.fp {
            self.op(Some(Self::sse(p.k)), false, false, &[0x0f, 0x11], p.reg, rm);
        } else {
            self.store_raw(p.reg, rm, p.k);
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

    /// `size` bytes at `m` are read from memory: what they hold must be there.
    fn rd(&mut self, m: Mem, size: u32) {
        if let Mem::Frame(s, o) = m {
            while let Some(i) = self
                .pending
                .iter()
                .position(|p| p.slot == s && p.off < o + size && o < p.off + p.k.size())
            {
                let p = self.pending.remove(i);
                self.emit_pend(p);
            }
        }
    }

    /// Slot `m`'s one read takes the register: its store is dead.
    fn consume(&mut self, m: Mem) {
        if let Mem::Frame(s, o) = m {
            self.pending.retain(|p| p.slot != s || p.off != o);
        }
    }

    /// The first register of `pool` not in `avoid`, preferring one that
    /// no store waits on and holds nothing.
    fn pick(&self, pool: &[u32], fp: bool, avoid: &[u32]) -> u32 {
        let ok = |r: &&u32| !avoid.contains(r);
        let free = |r: &&u32| ok(r) && !self.pending.iter().any(|p| p.reg == **r && p.fp == fp);
        pool.iter()
            .filter(free)
            .find(|r| !self.held.iter().any(|h| h.reg == **r && h.fp == fp))
            .or_else(|| pool.iter().find(free))
            .or_else(|| pool.iter().find(ok))
            .copied()
            .unwrap_or(pool[0])
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

    /// `[base + disp]` of `m`; an indirect pointer goes into `tmp`
    /// (unless a register holds it).
    fn mem(&mut self, m: Mem, tmp: u32) -> Rm {
        match m {
            Mem::Frame(s, o) => Rm::M(RSP, (self.slot_off[s as usize] + o) as i32),
            Mem::Ind(s, o) => {
                let p = Mem::Frame(s, 0);
                if let Some(h) = self.find(p, 8, false) {
                    self.consume(p);
                    return Rm::M(h.reg, o);
                }
                self.rd(p, 8);
                self.w(tmp, false);
                let so = self.slot_off[s as usize] as i32;
                self.op(None, true, false, &[0x8b], tmp, Rm::M(RSP, so));
                self.hold(tmp, false, p, 8, None);
                Rm::M(tmp, o)
            }
        }
    }

    fn mov_imm(&mut self, r: u32, v: u64) {
        self.w(r, false);
        if u32::try_from(v).is_ok() {
            // mov r32, imm32 (zero-extends)
            if r >= 8 {
                self.b(0x41);
            }
            self.b(0xb8 + (r & 7) as u8);
            self.d32(v as u32 as i32);
        } else if (v as i64) >= i64::from(i32::MIN) && (v as i64) < 0 {
            // mov r64, simm32
            self.op(None, true, false, &[0xc7], 0, Rm::R(r));
            self.d32(v as i32);
        } else {
            self.b(0x48 | u8::from(r >= 8));
            self.b(0xb8 + (r & 7) as u8);
            self.bytes(&v.to_le_bytes());
        }
    }

    /// Integer register `r` = `v` as `k`, extended by `signed` to 64 bits.
    fn load(&mut self, r: u32, v: V, k: K, signed: bool) {
        // A float's bits load unextended.
        let signed = signed && !k.is_float();
        if let V::M(m) = v {
            if let Some(h) = self.find(m, k.size(), false) {
                self.consume(m);
                if k.size() == 8 || h.ext == Some(signed) {
                    if h.reg != r {
                        self.w(r, false);
                        self.op(None, true, false, &[0x8b], r, Rm::R(h.reg));
                        self.hold(r, false, m, k.size(), h.ext);
                    }
                    return;
                }
                self.w(r, false);
                self.extend(r, Rm::R(h.reg), k, signed);
                self.hold(r, false, m, k.size(), Some(signed));
                return;
            }
            if let Some(h) = self
                .find(m, k.size(), true)
                .filter(|_| k.size() == 8 || !signed)
            {
                self.consume(m);
                self.w(r, false);
                // movq / movd r, xmm (zero-extends)
                self.op(
                    Some(0x66),
                    k.size() == 8,
                    false,
                    &[0x0f, 0x7e],
                    h.reg,
                    Rm::R(r),
                );
                self.hold(r, false, m, k.size(), Some(false));
                return;
            }
        }
        match v {
            V::I(i) => self.mov_imm(r, ext(i, k, signed) as u64),
            V::M(m) => {
                let rm = self.mem(m, R10);
                self.rd(m, k.size());
                self.w(r, false);
                self.extend(r, rm, k, signed);
                self.hold(r, false, m, k.size(), Some(signed));
            }
        }
    }

    /// `r` = the `k` at `rm`, extended by `signed` to 64 bits.
    fn extend(&mut self, r: u32, rm: Rm, k: K, signed: bool) {
        match (k, signed) {
            (K::I8, true) => self.op(None, true, true, &[0x0f, 0xbe], r, rm),
            (K::I8, false) => self.op(None, false, true, &[0x0f, 0xb6], r, rm),
            (K::I16, true) => self.op(None, true, false, &[0x0f, 0xbf], r, rm),
            (K::I16, false) => self.op(None, false, false, &[0x0f, 0xb7], r, rm),
            (K::I32, true) => self.op(None, true, false, &[0x63], r, rm),
            (K::I32 | K::F32, _) => self.op(None, false, false, &[0x8b], r, rm),
            (K::I64 | K::F64, _) => self.op(None, true, false, &[0x8b], r, rm),
        }
    }

    /// A register holding `v` as `k` extended by `signed`, not in `avoid`: one
    /// already holding it, else a loaded value register; never `r10` / `r11`.
    fn reg(&mut self, v: V, k: K, signed: bool, avoid: &[u32]) -> u32 {
        let signed = signed && !k.is_float();
        if let V::M(m) = v {
            if let Some(h) = self.find(m, k.size(), false) {
                if !matches!(h.reg, R10 | R11)
                    && !avoid.contains(&h.reg)
                    && (k.size() == 8 || h.ext == Some(signed))
                {
                    self.consume(m);
                    return h.reg;
                }
            }
        }
        let r = self.pick(&POOL, false, avoid);
        self.load(r, v, k, signed);
        r
    }

    /// `reg` for a float: an `xmm`.
    fn freg(&mut self, v: V, k: K, avoid: &[u32]) -> u32 {
        if let V::M(m) = v {
            if let Some(h) = self.find(m, k.size(), true) {
                if !avoid.contains(&h.reg) {
                    self.consume(m);
                    return h.reg;
                }
            }
        }
        let x = self.pick(&FP_POOL, true, avoid);
        self.load_fp(x, v, k);
        x
    }

    /// `rd` = `ra` (all 64 bits), about to be written.
    fn mov_rr(&mut self, rd: u32, ra: u32) {
        self.w(rd, false);
        if rd != ra {
            self.op(None, true, false, &[0x8b], rd, Rm::R(ra));
        }
    }

    fn store(&mut self, r: u32, m: Mem, k: K) {
        self.store_ext(r, m, k, None);
    }

    /// Store `r`, whose upper bits are known (`ext`) or not.
    fn store_ext(&mut self, r: u32, m: Mem, k: K, ext: Option<bool>) {
        self.put(r, false, m, k);
        self.hold(r, false, m, k.size(), ext);
    }

    /// Store `r` (`fp`: an `xmm`) at `m`: later, for a slot written and
    /// read once; never, for one never read.
    fn put(&mut self, r: u32, fp: bool, m: Mem, k: K) {
        let rm = self.mem(m, R11);
        self.wrote(m, k.size());
        if let (Mem::Frame(slot, off), Rm::M(_, disp)) = (m, rm) {
            match self.waits.get(&(slot, off)) {
                Some(&(z, 0)) if z == k.size() => return,
                Some(&(z, 1)) if z == k.size() => {
                    self.pending.push(Pend {
                        reg: r,
                        fp,
                        k,
                        slot,
                        off,
                        disp,
                    });
                    return;
                }
                _ => {}
            }
        }
        if fp {
            self.op(Some(Self::sse(k)), false, false, &[0x0f, 0x11], r, rm);
        } else {
            self.store_raw(r, rm, k);
        }
    }

    fn store_raw(&mut self, r: u32, rm: Rm, k: K) {
        match k {
            K::I8 => self.op(None, false, true, &[0x88], r, rm),
            K::I16 => self.op(Some(0x66), false, false, &[0x89], r, rm),
            K::I32 | K::F32 => self.op(None, false, false, &[0x89], r, rm),
            K::I64 | K::F64 => self.op(None, true, false, &[0x89], r, rm),
        }
    }

    const fn sse(k: K) -> u8 {
        if matches!(k, K::F32) { 0xf3 } else { 0xf2 }
    }

    fn load_fp(&mut self, x: u32, v: V, k: K) {
        if let V::M(m) = v {
            if let Some(h) = self.find(m, k.size(), true) {
                self.consume(m);
                if h.reg != x {
                    self.w(x, true);
                    self.op(None, false, false, &[0x0f, 0x28], x, Rm::R(h.reg)); // movaps
                    self.hold(x, true, m, k.size(), None);
                }
                return;
            }
            if let Some(h) = self.find(m, k.size(), false) {
                self.consume(m);
                self.w(x, true);
                // movq / movd xmm, r
                self.op(
                    Some(0x66),
                    k == K::F64,
                    false,
                    &[0x0f, 0x6e],
                    x,
                    Rm::R(h.reg),
                );
                self.hold(x, true, m, k.size(), None);
                return;
            }
        }
        match v {
            V::I(bits) => {
                self.mov_imm(RAX, bits as u64);
                self.w(x, true);
                // movq / movd xmm, rax
                self.op(Some(0x66), k == K::F64, false, &[0x0f, 0x6e], x, Rm::R(RAX));
            }
            V::M(m) => {
                let rm = self.mem(m, R10);
                self.rd(m, k.size());
                self.w(x, true);
                self.op(Some(Self::sse(k)), false, false, &[0x0f, 0x10], x, rm);
                self.hold(x, true, m, k.size(), None);
            }
        }
    }

    fn store_fp(&mut self, x: u32, m: Mem, k: K) {
        self.put(x, true, m, k);
        self.hold(x, true, m, k.size(), None);
    }

    fn lea(&mut self, r: u32, m: Mem) {
        if let Mem::Frame(s, _) = m {
            // Reached through the address.
            self.flush(Some(s));
        }
        let rm = self.mem(m, r);
        self.w(r, false);
        self.op(None, true, false, &[0x8d], r, rm);
    }

    fn jmp(&mut self, l: Label) {
        self.flush(None);
        self.b(0xe9);
        self.fixes.push((self.code.len(), l));
        self.d32(0);
    }

    fn jcc(&mut self, cc: u8, l: Label) {
        self.flush(None);
        self.bytes(&[0x0f, 0x80 + cc]);
        self.fixes.push((self.code.len(), l));
        self.d32(0);
    }

    /// A forward `jcc` to a not yet known spot: its rel32's position.
    fn jcc_fwd(&mut self, cc: u8) -> usize {
        self.bytes(&[0x0f, 0x80 + cc]);
        self.d32(0);
        self.code.len() - 4
    }

    fn jmp_fwd(&mut self) -> usize {
        self.b(0xe9);
        self.d32(0);
        self.code.len() - 4
    }

    /// Point the rel32 at `at` here.
    fn here(&mut self, at: usize) {
        let d = (self.code.len() - (at + 4)) as i32;
        self.code[at..at + 4].copy_from_slice(&d.to_le_bytes());
    }

    /// `setcc al; movzx eax, al`.
    fn setcc(&mut self, cc: u8) {
        self.bytes(&[0x0f, 0x90 + cc, 0xc0, 0x0f, 0xb6, 0xc0]);
    }

    fn alu(&mut self, opc: u8, w: bool, dst: u32, src: u32) {
        self.op(None, w, false, &[opc], src, Rm::R(dst));
    }

    fn prologue(&mut self) {
        self.bytes(&PROLOGUE);
        let n = self.frame;
        let pages = n / PAGE;
        if pages > 8 {
            self.mov_imm(R11, u64::from(pages));
            let top = self.code.len();
            self.bytes(&[0x48, 0x81, 0xec]); // sub rsp, imm32
            self.d32(PAGE as i32);
            self.bytes(&[0x48, 0x83, 0x0c, 0x24, 0x00]); // or qword [rsp], 0
            self.bytes(&[0x49, 0xff, 0xcb]); // dec r11
            self.bytes(&[0x0f, 0x85]); // jnz top
            let d = top as i32 - (self.code.len() as i32 + 4);
            self.d32(d);
        } else {
            for _ in 0..pages {
                self.bytes(&[0x48, 0x81, 0xec]);
                self.d32(PAGE as i32);
                self.bytes(&[0x48, 0x83, 0x0c, 0x24, 0x00]);
            }
        }
        let rest = n % PAGE;
        if rest != 0 {
            self.bytes(&[0x48, 0x81, 0xec]);
            self.d32(rest as i32);
        }
    }

    fn epilogue(&mut self) {
        self.bytes(&[0x48, 0x89, 0xec, 0x5d, 0xc3]); // mov rsp, rbp; pop rbp; ret
    }

    fn copy(&mut self, dst: Mem, src: Mem, size: u32, zero: bool) {
        if size > 128 {
            // A qword loop, not `rep movsb` / `rep stosb`: Rosetta runs
            // those a byte at a time.
            self.scratch();
            if !zero {
                self.lea(R10, src);
            }
            self.lea(R11, dst);
            self.mov_imm(RCX, u64::from(size / 8));
            if zero {
                self.bytes(&[0x31, 0xc0]); // xor eax, eax
            }
            let top = self.code.len();
            if !zero {
                self.op(None, true, false, &[0x8b], RAX, Rm::M(R10, 0));
                self.bytes(&[0x49, 0x83, 0xc2, 0x08]); // add r10, 8
            }
            self.op(None, true, false, &[0x89], RAX, Rm::M(R11, 0));
            self.bytes(&[0x49, 0x83, 0xc3, 0x08]); // add r11, 8
            self.bytes(&[0xff, 0xc9, 0x0f, 0x85]); // dec ecx; jnz top
            let d = top as i32 - (self.code.len() as i32 + 4);
            self.d32(d);
            self.copy_words(size % 8, zero);
            return;
        }
        if !zero {
            self.lea(R10, src);
        }
        self.lea(R11, dst);
        self.w(RAX, false);
        if zero {
            self.bytes(&[0x31, 0xc0]);
        }
        self.copy_words(size, zero);
        self.wrote(dst, size);
    }

    /// `[r11 + i] = [r10 + i]` (or zero) for `size` bytes.
    fn copy_words(&mut self, size: u32, zero: bool) {
        let mut off = 0i32;
        let mut rest = size;
        for (n, k) in [(8u32, K::I64), (4, K::I32), (2, K::I16), (1, K::I8)] {
            while rest >= n {
                if !zero {
                    let rm = Rm::M(R10, off);
                    match k {
                        K::I64 => self.op(None, true, false, &[0x8b], RAX, rm),
                        K::I32 => self.op(None, false, false, &[0x8b], RAX, rm),
                        K::I16 => self.op(None, false, false, &[0x0f, 0xb7], RAX, rm),
                        _ => self.op(None, false, false, &[0x0f, 0xb6], RAX, rm),
                    }
                }
                let rm = Rm::M(R11, off);
                match k {
                    K::I64 => self.op(None, true, false, &[0x89], RAX, rm),
                    K::I32 => self.op(None, false, false, &[0x89], RAX, rm),
                    K::I16 => self.op(Some(0x66), false, false, &[0x89], RAX, rm),
                    _ => self.op(None, false, true, &[0x88], RAX, rm),
                }
                rest -= n;
                off += n as i32;
            }
        }
    }

    /// `rd = ra op imm` (`ra` is `rd` or `rd` is free) when the immediate
    /// fits 32 bits (any for 32-bit ops); not for division.
    #[allow(clippy::too_many_arguments)]
    fn bin_imm(&mut self, op: Bin, k: K, signed: bool, rd: u32, ra: u32, imm: i64) -> bool {
        let w = k == K::I64;
        let Some(i) = (if w {
            i32::try_from(imm).ok()
        } else {
            Some(imm as i32)
        }) else {
            return false;
        };
        let bits = if w { 63 } else { 31 };
        match op {
            Bin::Add | Bin::Or | Bin::And | Bin::Sub | Bin::Xor => {
                let n = match op {
                    Bin::Add => 0,
                    Bin::Or => 1,
                    Bin::And => 4,
                    Bin::Sub => 5,
                    _ => 6,
                };
                self.mov_rr(rd, ra);
                self.op(None, w, false, &[0x81], n, Rm::R(rd));
                self.d32(i);
            }
            // imul rd, ra, imm32
            Bin::Mul => {
                self.w(rd, false);
                self.op(None, w, false, &[0x69], rd, Rm::R(ra));
                self.d32(i);
            }
            Bin::Shl | Bin::Shr => {
                let n = match (op, signed) {
                    (Bin::Shl, _) => 4,
                    (_, true) => 7,
                    _ => 5,
                };
                self.mov_rr(rd, ra);
                self.op(None, w, false, &[0xc1], n, Rm::R(rd));
                self.b((i & bits) as u8);
            }
            Bin::Div | Bin::Rem => return false,
        }
        true
    }

    /// `rd op= rb` for all but division (variable shifts: `rb` is `rcx`).
    fn bin_rr(&mut self, op: Bin, k: K, signed: bool, rd: u32, rb: u32) {
        let w = k == K::I64;
        match op {
            Bin::Add => self.alu(0x01, w, rd, rb),
            Bin::Sub => self.alu(0x29, w, rd, rb),
            Bin::And => self.alu(0x21, w, rd, rb),
            Bin::Or => self.alu(0x09, w, rd, rb),
            Bin::Xor => self.alu(0x31, w, rd, rb),
            Bin::Mul => self.op(None, w, false, &[0x0f, 0xaf], rd, Rm::R(rb)),
            Bin::Shl => self.op(None, w, false, &[0xd3], 4, Rm::R(rd)),
            Bin::Shr => self.op(
                None,
                w,
                false,
                &[0xd3],
                if signed { 7 } else { 5 },
                Rm::R(rd),
            ),
            Bin::Div | Bin::Rem => unreachable!("division goes through rax"),
        }
    }

    fn bin_int(&mut self, op: Bin, k: K, signed: bool) {
        // rax = rax op rcx
        let w = k == K::I64;
        match op {
            Bin::Add => self.alu(0x01, w, RAX, RCX),
            Bin::Sub => self.alu(0x29, w, RAX, RCX),
            Bin::And => self.alu(0x21, w, RAX, RCX),
            Bin::Or => self.alu(0x09, w, RAX, RCX),
            Bin::Xor => self.alu(0x31, w, RAX, RCX),
            Bin::Mul => self.op(None, w, false, &[0x0f, 0xaf], RAX, Rm::R(RCX)),
            Bin::Div | Bin::Rem => {
                if signed {
                    // cqo / cdq; idiv rcx
                    if w {
                        self.bytes(&[0x48, 0x99]);
                    } else {
                        self.b(0x99);
                    }
                    self.op(None, w, false, &[0xf7], 7, Rm::R(RCX));
                } else {
                    self.bytes(&[0x31, 0xd2]); // xor edx, edx
                    self.op(None, w, false, &[0xf7], 6, Rm::R(RCX));
                }
                if op == Bin::Rem {
                    self.op(None, true, false, &[0x89], RDX, Rm::R(RAX));
                }
            }
            Bin::Shl => self.op(None, w, false, &[0xd3], 4, Rm::R(RAX)),
            Bin::Shr => self.op(
                None,
                w,
                false,
                &[0xd3],
                if signed { 7 } else { 5 },
                Rm::R(RAX),
            ),
        }
    }

    fn inst(&mut self, i: &Inst, next: Option<&Inst>) {
        match i {
            Inst::Label(l) => {
                self.flush(None);
                self.labels[*l as usize] = Some(self.code.len());
                self.held.clear();
            }
            Inst::Jmp(l) => {
                if !matches!(next, Some(Inst::Label(n)) if n == l) {
                    self.jmp(*l);
                }
            }
            Inst::Br { c, t, f } => {
                if let V::I(v) = c {
                    self.jmp(if *v & 0xff != 0 { *t } else { *f });
                    return;
                }
                if let Some(cc) = self.flags.take() {
                    // The comparison just before: its flags.
                    self.jcc(cc, *t);
                } else {
                    let r = self.reg(*c, K::I8, false, &[]);
                    self.op(None, false, false, &[0x85], r, Rm::R(r)); // test r32, r32
                    self.jcc(CC_NE, *t);
                }
                if !matches!(next, Some(Inst::Label(n)) if n == f) {
                    self.jmp(*f);
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
                let r = self.pick(&POOL, false, &[]);
                self.lea(r, *src);
                self.store(r, *dst, K::I64);
            }
            Inst::Sym { dst, sym, got } => {
                // COFF has no GOT: everything is in the image (a DLL's
                // functions through import thunks).
                let r = self.pick(&POOL, false, &[]);
                self.w(r, false);
                if *got && !self.obj.coff {
                    self.op_rip(None, true, &[0x8b], r, RelKind::X86GotLoad, *sym);
                } else {
                    self.op_rip(None, true, &[0x8d], r, RelKind::X86Pc32, *sym);
                }
                self.store(r, *dst, K::I64);
            }
            Inst::Tls { dst, sym } => {
                self.scratch();
                if self.obj.coff {
                    // The TEB's TLS array at `_tls_index`, plus the
                    // offset in the TLS section.
                    let index = self.obj.sym("_tls_index");
                    self.op_rip(None, false, &[0x8b], RAX, RelKind::X86Pc32, index);
                    // mov rcx, gs:[0x58]; mov rcx, [rcx + rax * 8]
                    self.bytes(&[0x65, 0x48, 0x8b, 0x0c, 0x25, 0x58, 0, 0, 0]);
                    self.bytes(&[0x48, 0x8b, 0x0c, 0xc1]);
                    self.bytes(&[0x48, 0x8d, 0x81]); // lea rax, [rcx + disp32]
                    let off = self.start + self.code.len() as u32;
                    self.relocs.push((off, RelKind::X86SecRel, *sym));
                    self.d32(0);
                } else if self.obj.elf {
                    // Initial exec: the offset from the thread pointer.
                    self.op_rip(None, true, &[0x8b], RAX, RelKind::X86Tls, *sym);
                    // mov rcx, fs:[0]; add rax, rcx
                    self.bytes(&[0x64, 0x48, 0x8b, 0x0c, 0x25, 0, 0, 0, 0]);
                    self.alu(0x01, true, RAX, RCX);
                } else {
                    // Mach-O: the descriptor's getter, address in rax.
                    self.op_rip(None, true, &[0x8b], RDI, RelKind::X86Tls, *sym);
                    self.bytes(&[0xff, 0x17]); // call [rdi]
                }
                self.store(RAX, *dst, K::I64);
            }
            Inst::Bin {
                op,
                k,
                signed,
                dst,
                a,
                b,
            } => self.bin(*op, *k, *signed, *dst, *a, *b),
            Inst::Ovf {
                op,
                k,
                signed,
                dst,
                a,
                b,
                fail,
            } => {
                let imm = match (k, op, b) {
                    (K::I64, Bin::Add | Bin::Sub, V::I(i)) => i32::try_from(*i).ok(),
                    (K::I32, Bin::Add | Bin::Sub, V::I(i)) => Some(*i as i32),
                    _ => None,
                };
                if imm.is_none() {
                    self.load(RCX, *b, *k, *signed);
                }
                self.load(RAX, *a, *k, *signed);
                self.scratch();
                match k {
                    K::I64 | K::I32 => {
                        let w = *k == K::I64;
                        match op {
                            // add / sub rax, imm32
                            _ if imm.is_some() => {
                                let n = if *op == Bin::Add { 0 } else { 5 };
                                self.op(None, w, false, &[0x81], n, Rm::R(RAX));
                                self.d32(imm.unwrap_or(0));
                            }
                            Bin::Add => self.alu(0x01, w, RAX, RCX),
                            Bin::Sub => self.alu(0x29, w, RAX, RCX),
                            _ if *signed => self.op(None, w, false, &[0x0f, 0xaf], RAX, Rm::R(RCX)),
                            // mul rcx: rdx:rax; OF / CF when the high half is used.
                            _ => self.op(None, w, false, &[0xf7], 4, Rm::R(RCX)),
                        }
                        let cc = match (op, signed) {
                            (_, true) | (Bin::Mul, false) => CC_O,
                            _ => CC_B,
                        };
                        self.jcc(cc, *fail);
                    }
                    _ => {
                        // 8 / 16 bits in 32: the result must equal its
                        // own extension.
                        self.bin_int(*op, K::I32, *signed);
                        let opc = match (k, signed) {
                            (K::I8, true) => [0x0f, 0xbe],
                            (K::I8, false) => [0x0f, 0xb6],
                            (_, true) => [0x0f, 0xbf],
                            _ => [0x0f, 0xb7],
                        };
                        self.op(None, false, false, &opc, RCX, Rm::R(RAX));
                        self.alu(0x39, false, RAX, RCX);
                        self.jcc(CC_NE, *fail);
                    }
                }
                self.store(RAX, *dst, *k);
            }
            Inst::Un { op, k, dst, a } => self.un(*op, *k, *dst, *a),
            Inst::Cmp {
                cc,
                k,
                signed,
                dst,
                a,
                b,
            } => {
                if k.is_float() {
                    self.load_fp(XMM0, *a, *k);
                    self.load_fp(XMM1, *b, *k);
                    self.scratch();
                    let pre = if *k == K::F64 { Some(0x66) } else { None };
                    let ucomi = |s: &mut Self, x: u32, y: u32| {
                        s.op(pre, false, false, &[0x0f, 0x2e], x, Rm::R(y));
                    };
                    match cc {
                        Cc::Eq | Cc::Ne => {
                            ucomi(self, XMM0, XMM1);
                            let (c1, c2, comb) = if *cc == Cc::Eq {
                                (CC_E, CC_NP, 0x21)
                            } else {
                                (CC_NE, CC_P, 0x09)
                            };
                            self.bytes(&[0x0f, 0x90 + c1, 0xc0]); // setcc al
                            self.bytes(&[0x0f, 0x90 + c2, 0xc1]); // setcc cl
                            self.bytes(&[comb - 1, 0xc8]); // and / or al, cl
                            self.bytes(&[0x0f, 0xb6, 0xc0]);
                        }
                        Cc::Lt | Cc::Le | Cc::Gt | Cc::Ge => {
                            if matches!(cc, Cc::Lt | Cc::Le) {
                                ucomi(self, XMM1, XMM0);
                            } else {
                                ucomi(self, XMM0, XMM1);
                            }
                            let c = if matches!(cc, Cc::Lt | Cc::Gt) {
                                CC_A
                            } else {
                                CC_AE
                            };
                            self.setcc(c);
                            if matches!(next, Some(Inst::Br { c: V::M(m), .. }) if m == dst) {
                                self.flags = Some(c);
                            }
                        }
                    }
                    self.store_ext(RAX, *dst, K::I8, Some(false));
                    return;
                }
                let ra = self.reg(*a, *k, *signed, &[]);
                match b {
                    // cmp ra, simm32 (the immediate extended as `load` would)
                    V::I(i) if i32::try_from(ext(*i, *k, *signed)).is_ok() => {
                        self.op(None, true, false, &[0x81], 7, Rm::R(ra));
                        self.d32(ext(*i, *k, *signed) as i32);
                    }
                    _ => {
                        let rb = if b == a {
                            ra
                        } else {
                            self.reg(*b, *k, *signed, &[ra])
                        };
                        self.alu(0x39, true, ra, rb);
                    }
                }
                let c = match (cc, signed) {
                    (Cc::Eq, _) => CC_E,
                    (Cc::Ne, _) => CC_NE,
                    (Cc::Lt, true) => CC_L,
                    (Cc::Lt, false) => CC_B,
                    (Cc::Le, true) => CC_LE,
                    (Cc::Le, false) => CC_BE,
                    (Cc::Gt, true) => CC_G,
                    (Cc::Gt, false) => CC_A,
                    (Cc::Ge, true) => CC_GE,
                    (Cc::Ge, false) => CC_AE,
                };
                // A branch right after on it takes the flags; when it is
                // the only reader, nothing else is left to do.
                if matches!(next, Some(Inst::Br { c: V::M(m), .. }) if m == dst) {
                    self.flags = Some(c);
                    if let Mem::Frame(s, o) = dst {
                        if self.waits.get(&(*s, *o)) == Some(&(1, 1)) {
                            return;
                        }
                    }
                }
                // Stores going out leave the flags alone.
                let rd = self.pick(&POOL, false, &[]);
                self.w(rd, false);
                self.op(None, false, true, &[0x0f, 0x90 + c], 0, Rm::R(rd)); // setcc
                self.op(None, false, true, &[0x0f, 0xb6], rd, Rm::R(rd)); // movzx
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
                self.lea(R10, *ptr);
                self.scratch();
                self.sized_load(*k, RAX, Rm::M(R10, 0));
                self.store(RAX, *dst, *k);
            }
            Inst::AtomicStore { k, ptr, src } => {
                self.load(RAX, *src, *k, false);
                self.lea(R10, *ptr);
                self.scratch();
                self.xchg(*k);
            }
            Inst::AtomicRmw {
                op,
                k,
                dst,
                ptr,
                val,
            } => {
                self.load(RCX, *val, *k, false);
                self.lea(R10, *ptr);
                self.scratch();
                match op {
                    Rmw::Xchg => {
                        self.op(None, true, false, &[0x89], RCX, Rm::R(RAX));
                        self.xchg(*k);
                    }
                    Rmw::Add | Rmw::Sub => {
                        if *op == Rmw::Sub {
                            self.op(None, true, false, &[0xf7], 3, Rm::R(RCX)); // neg rcx
                        }
                        self.b(0xf0); // lock xadd [r10], rcx
                        self.sized(*k, &[0x0f, 0xc1], &[0x0f, 0xc0], RCX, Rm::M(R10, 0));
                        self.op(None, true, false, &[0x89], RCX, Rm::R(RAX));
                    }
                    Rmw::And | Rmw::Or | Rmw::Xor => {
                        // A compare-and-swap loop: rax = old, rdx = new.
                        self.sized_load(*k, RAX, Rm::M(R10, 0));
                        let top = self.code.len();
                        self.op(None, true, false, &[0x89], RAX, Rm::R(RDX));
                        let opc = match op {
                            Rmw::And => 0x21,
                            Rmw::Or => 0x09,
                            _ => 0x31,
                        };
                        self.alu(opc, true, RDX, RCX);
                        self.b(0xf0);
                        self.sized(*k, &[0x0f, 0xb1], &[0x0f, 0xb0], RDX, Rm::M(R10, 0));
                        self.bytes(&[0x0f, 0x85]);
                        let d = top as i32 - (self.code.len() as i32 + 4);
                        self.d32(d);
                    }
                }
                self.store(RAX, *dst, *k);
            }
            Inst::Cas {
                k,
                dst,
                ptr,
                expected,
                new,
            } => {
                self.load(RAX, *expected, *k, false);
                self.load(RCX, *new, *k, false);
                self.lea(R10, *ptr);
                self.scratch();
                self.b(0xf0);
                self.sized(*k, &[0x0f, 0xb1], &[0x0f, 0xb0], RCX, Rm::M(R10, 0));
                self.store(RAX, *dst, *k);
            }
            Inst::Call {
                callee,
                args,
                rets,
                sret,
                variadic,
            } => self.call(callee, args, rets, *sret, *variadic),
            Inst::Param { part, dst } => match part.loc {
                Loc::Int(r) => self.store(self.arg_reg(r), *dst, part.k),
                Loc::Fp(r) => self.store_fp(u32::from(r), *dst, part.k),
                Loc::Stack(o) => {
                    // rax: no parameter comes in it.
                    self.w(RAX, false);
                    let rm = Rm::M(RBP, 16 + o as i32);
                    self.op(None, true, false, &[0x8b], RAX, rm);
                    self.store(RAX, *dst, part.k);
                }
                Loc::Sret => self.store(self.sret_reg(), *dst, K::I64),
            },
            Inst::Ret { vals } => {
                // x87 results: the last pushed first, so `st0` is the first.
                let x87 = [3, 2]
                    .into_iter()
                    .filter_map(|n| vals.iter().find(|(p, _)| p.loc == Loc::Fp(n)));
                for (p, v) in x87 {
                    let rm = match v {
                        V::M(m) => {
                            // From memory: what a register holds goes there.
                            self.flush(None);
                            self.mem(*m, R10)
                        }
                        V::I(bits) => {
                            self.mov_imm(RAX, *bits as u64);
                            self.op(None, true, false, &[0x89], RAX, Rm::M(RSP, -8));
                            Rm::M(RSP, -8)
                        }
                    };
                    // fld dword / qword
                    let opc = if p.k == K::F32 { 0xd9 } else { 0xdd };
                    self.op(None, false, false, &[opc], 0, rm);
                }
                // Floats first: a float constant passes through rax.
                for (p, v) in vals {
                    if let Loc::Fp(r @ 0..2) = p.loc {
                        self.load_fp(u32::from(r), *v, p.k);
                    }
                }
                for (p, v) in vals {
                    if let Loc::Int(r) = p.loc {
                        self.load(RETS[r as usize], *v, p.k, p.signed);
                    }
                }
                // The frame dies.
                self.pending.clear();
                self.epilogue();
                self.held.clear();
            }
            Inst::Trap => {
                self.pending.clear();
                self.bytes(&[0x0f, 0x0b]); // ud2
            }
            Inst::Line(line, col, scope) => {
                let at = self.code.len() as u32;
                self.lines.push((at, *line, *col, *scope));
            }
        }
    }

    fn bin(&mut self, op: Bin, k: K, signed: bool, dst: Mem, a: V, b: V) {
        if k.is_float() {
            let va = self.freg(a, k, &[]);
            let vb = if b == a { va } else { self.freg(b, k, &[va]) };
            let vd = self.pick(&FP_POOL, true, &[vb]);
            self.w(vd, true);
            if vd != va {
                self.op(None, false, false, &[0x0f, 0x28], vd, Rm::R(va)); // movaps
            }
            let opc = match op {
                Bin::Add => 0x58,
                Bin::Sub => 0x5c,
                Bin::Mul => 0x59,
                _ => 0x5e,
            };
            self.op(
                Some(Self::sse(k)),
                false,
                false,
                &[0x0f, opc],
                vd,
                Rm::R(vb),
            );
            self.store_fp(vd, dst, k);
            return;
        }
        if matches!(op, Bin::Div | Bin::Rem) {
            self.load(RCX, b, k, signed);
            self.load(RAX, a, k, signed);
            self.scratch();
            self.bin_int(op, k, signed);
            self.store(RAX, dst, k);
            return;
        }
        if let V::I(imm) = b {
            let ra = self.reg(a, k, signed, &[]);
            let rd = self.pick(&POOL, false, &[]);
            if self.bin_imm(op, k, signed, rd, ra, imm) {
                self.store(rd, dst, k);
                return;
            }
        }
        let shift = matches!(op, Bin::Shl | Bin::Shr);
        let (ra, rb) = if shift {
            // The amount in cl.
            self.load(RCX, b, k, signed);
            (self.reg(a, k, signed, &[RCX]), RCX)
        } else {
            let ra = self.reg(a, k, signed, &[]);
            let rb = if b == a {
                ra
            } else {
                self.reg(b, k, signed, &[ra])
            };
            (ra, rb)
        };
        let rd = self.pick(&POOL, false, &[rb]);
        self.mov_rr(rd, ra);
        self.bin_rr(op, k, signed, rd, rb);
        self.store(rd, dst, k);
    }

    /// `opc` (16 / 32 / 64-bit) or `byte_opc` by `k`, `reg`, `rm`.
    fn sized(&mut self, k: K, opc: &[u8], byte_opc: &[u8], reg: u32, rm: Rm) {
        match k {
            K::I8 => self.op(None, false, true, byte_opc, reg, rm),
            K::I16 => self.op(Some(0x66), false, false, opc, reg, rm),
            K::I32 | K::F32 => self.op(None, false, false, opc, reg, rm),
            K::I64 | K::F64 => self.op(None, true, false, opc, reg, rm),
        }
    }

    fn sized_load(&mut self, k: K, r: u32, rm: Rm) {
        match k {
            K::I8 => self.op(None, false, false, &[0x0f, 0xb6], r, rm),
            K::I16 => self.op(None, false, false, &[0x0f, 0xb7], r, rm),
            K::I32 | K::F32 => self.op(None, false, false, &[0x8b], r, rm),
            K::I64 | K::F64 => self.op(None, true, false, &[0x8b], r, rm),
        }
    }

    /// `xchg [r10], rax` (sequentially consistent store); the old value in rax.
    fn xchg(&mut self, k: K) {
        self.sized(k, &[0x87], &[0x86], RAX, Rm::M(R10, 0));
    }

    fn call(
        &mut self,
        callee: &Callee,
        args: &[(crate::lir::Part, V)],
        rets: &[(crate::lir::Part, Mem)],
        sret: Option<Mem>,
        variadic: bool,
    ) {
        // The stack arguments first: register ones are loaded last.
        for (p, v) in args {
            if let Loc::Stack(o) = p.loc {
                let r = self.reg(*v, p.k, p.signed, &[]);
                self.op(None, true, false, &[0x89], r, Rm::M(RSP, o as i32));
            }
        }
        let mut fps = 0;
        for (n, (p, v)) in args.iter().enumerate() {
            match p.loc {
                Loc::Int(r) => {
                    let r = self.arg_reg(r);
                    self.keep(r, &args[n..]);
                    self.load(r, *v, p.k, p.signed);
                }
                Loc::Fp(r) => {
                    self.load_fp(u32::from(r), *v, p.k);
                    fps = fps.max(r + 1);
                    // Windows: a variadic callee reads a float from the
                    // integer register of its position.
                    if variadic && self.obj.coff {
                        let w = p.k == K::F64;
                        let g = self.arg_reg(r);
                        self.w(g, false);
                        self.op(Some(0x66), w, false, &[0x0f, 0x7e], u32::from(r), Rm::R(g));
                    }
                }
                _ => {}
            }
        }
        if let Some(m) = sret {
            self.lea(self.sret_reg(), m);
        }
        if variadic && !self.obj.coff {
            self.mov_imm(RAX, u64::from(fps));
        }
        if let Callee::Ptr(m) = callee {
            self.load(R11, V::M(*m), K::I64, false);
        }
        self.flush(None);
        match callee {
            Callee::Sym(s) => {
                self.b(0xe8);
                let off = self.start + self.code.len() as u32;
                self.relocs.push((off, RelKind::X86Call, *s));
                self.d32(0);
            }
            Callee::Ptr(_) => self.op(None, false, false, &[0xff], 2, Rm::R(R11)), // call r11
        }
        self.held.clear();
        // x87 results (`st0`, then `st1`), popped in order.
        let x87: Vec<_> = [2, 3]
            .into_iter()
            .filter_map(|n| rets.iter().find(|(p, _)| p.loc == Loc::Fp(n)))
            .collect();
        for (p, m) in rets {
            match p.loc {
                Loc::Int(r) => self.store(RETS[r as usize], *m, p.k),
                Loc::Fp(r) if r < 2 => self.store_fp(u32::from(r), *m, p.k),
                _ => {}
            }
        }
        for (p, m) in x87 {
            let rm = self.mem(*m, R11);
            self.wrote(*m, p.k.size());
            // fstp dword / qword
            let opc = if p.k == K::F32 { 0xd9 } else { 0xdd };
            self.op(None, false, false, &[opc], 3, rm);
        }
    }

    /// Before loading `args[0]` into `r`, move what `r` holds for a later
    /// argument to a spare (`rax`, `r11`) instead of memory.
    fn keep(&mut self, r: u32, args: &[(crate::lir::Part, V)]) {
        let of = |v: &V, h: &Held| {
            !h.fp
                && h.reg == r
                && matches!(v, V::M(Mem::Frame(s, o)) if h.slot == *s && h.off == *o)
        };
        let needed = |v: &V| self.held.iter().any(|h| of(v, h));
        // Already in place, or nothing later reads it.
        if needed(&args[0].1) || !args[1..].iter().any(|(_, v)| needed(v)) {
            return;
        }
        let to = self.pick(&[RAX, R11], false, &[r]);
        self.w(to, false);
        self.op(None, true, false, &[0x8b], to, Rm::R(r));
        for h in self.held.iter_mut().filter(|h| h.reg == r && !h.fp) {
            h.reg = to;
        }
        for p in self.pending.iter_mut().filter(|p| p.reg == r && !p.fp) {
            p.reg = to;
        }
    }

    fn un(&mut self, op: Un, k: K, dst: Mem, a: V) {
        let w = k == K::I64;
        let bits = k.bits();
        if op == Un::Sqrt {
            self.load_fp(XMM0, a, k);
            self.scratch();
            self.op(
                Some(Self::sse(k)),
                false,
                false,
                &[0x0f, 0x51],
                XMM0,
                Rm::R(XMM0),
            );
            self.store_fp(XMM0, dst, k);
            return;
        }
        self.load(RAX, a, k, op == Un::Neg && !k.is_float());
        self.scratch();
        match op {
            Un::Sqrt => unreachable!("sqrt is a float operation"),
            Un::Neg if k.is_float() => {
                // Flip the sign bit.
                if k == K::F64 {
                    self.mov_imm(RCX, 1 << 63);
                    self.alu(0x31, true, RAX, RCX);
                } else {
                    self.b(0x35); // xor eax, imm32
                    self.d32(i32::MIN);
                }
            }
            Un::Neg => {
                self.op(None, w, false, &[0xf7], 3, Rm::R(RAX));
            }
            Un::Not => {
                self.op(None, w, false, &[0xf7], 2, Rm::R(RAX));
            }
            Un::Popcnt => {
                // The classic bit-slicing count, 64-bit.
                let step = |s: &mut Self, sh: u8, mask: u64| {
                    s.op(None, true, false, &[0x89], RAX, Rm::R(RCX)); // rcx = x
                    s.op(None, true, false, &[0xc1], 5, Rm::R(RCX)); // shr rcx, sh
                    s.b(sh);
                    s.mov_imm(RDX, mask);
                    s.alu(0x21, true, RCX, RDX);
                    s.alu(0x21, true, RAX, RDX);
                };
                // x -= (x >> 1) & 0x55..
                self.op(None, true, false, &[0x89], RAX, Rm::R(RCX));
                self.op(None, true, false, &[0xd1], 5, Rm::R(RCX)); // shr rcx, 1
                self.mov_imm(RDX, 0x5555_5555_5555_5555);
                self.alu(0x21, true, RCX, RDX);
                self.alu(0x29, true, RAX, RCX);
                // x = (x & 0x33..) + ((x >> 2) & 0x33..)
                step(self, 2, 0x3333_3333_3333_3333);
                self.alu(0x01, true, RAX, RCX);
                // x = (x + (x >> 4)) & 0x0f..
                self.op(None, true, false, &[0x89], RAX, Rm::R(RCX));
                self.op(None, true, false, &[0xc1], 5, Rm::R(RCX));
                self.b(4);
                self.alu(0x01, true, RAX, RCX);
                self.mov_imm(RDX, 0x0f0f_0f0f_0f0f_0f0f);
                self.alu(0x21, true, RAX, RDX);
                // x = (x * 0x0101..) >> 56
                self.mov_imm(RDX, 0x0101_0101_0101_0101);
                self.op(None, true, false, &[0x0f, 0xaf], RAX, Rm::R(RDX));
                self.op(None, true, false, &[0xc1], 5, Rm::R(RAX));
                self.b(56);
            }
            Un::Clz | Un::Ctz => {
                self.alu(0x85, true, RAX, RAX); // test rax, rax
                let zero = self.jcc_fwd(CC_E);
                if op == Un::Clz {
                    self.op(None, true, false, &[0x0f, 0xbd], RAX, Rm::R(RAX)); // bsr
                    self.mov_imm(RCX, u64::from(bits - 1));
                    self.alu(0x29, true, RCX, RAX);
                    self.op(None, true, false, &[0x89], RCX, Rm::R(RAX));
                } else {
                    self.op(None, true, false, &[0x0f, 0xbc], RAX, Rm::R(RAX)); // bsf
                }
                let done = self.jmp_fwd();
                self.here(zero);
                self.mov_imm(RAX, u64::from(bits));
                self.here(done);
            }
            Un::Bswap => {
                match k {
                    K::I16 => self.bytes(&[0x66, 0xc1, 0xc0, 0x08]), // rol ax, 8
                    K::I32 => self.bytes(&[0x0f, 0xc8]),
                    K::I64 => self.bytes(&[0x48, 0x0f, 0xc8]),
                    _ => {}
                }
            }
        }
        self.store(RAX, dst, k);
    }

    fn conv(&mut self, conv: Conv, from: K, to: K, dst: Mem, a: V) {
        match conv {
            Conv::Int { signed } => {
                let r = self.reg(a, from, signed, &[]);
                // Widened: extended as the destination's own bits.
                let ext = (to.size() > from.size()).then_some(signed);
                self.store_ext(r, dst, to, ext);
            }
            Conv::ToBool => {
                self.load(RAX, a, from, false);
                self.scratch();
                self.alu(0x85, true, RAX, RAX);
                self.setcc(CC_NE);
                self.store(RAX, dst, K::I8);
            }
            Conv::IToF { signed } => {
                self.load(RAX, a, from, signed);
                self.scratch();
                let cvt = |s: &mut Self, r: u32| {
                    s.op(
                        Some(Self::sse(to)),
                        true,
                        false,
                        &[0x0f, 0x2a],
                        XMM0,
                        Rm::R(r),
                    );
                };
                if !signed && from == K::I64 {
                    // u64 >= 2^63: halve (keeping the low bit), convert,
                    // double.
                    self.alu(0x85, true, RAX, RAX);
                    let big = self.jcc_fwd(CC_S);
                    cvt(self, RAX);
                    let done = self.jmp_fwd();
                    self.here(big);
                    self.op(None, true, false, &[0x89], RAX, Rm::R(RCX));
                    self.op(None, true, false, &[0xd1], 5, Rm::R(RCX)); // shr rcx, 1
                    self.bytes(&[0x83, 0xe0, 0x01]); // and eax, 1
                    self.alu(0x09, true, RCX, RAX);
                    cvt(self, RCX);
                    self.op(
                        Some(Self::sse(to)),
                        false,
                        false,
                        &[0x0f, 0x58],
                        XMM0,
                        Rm::R(XMM0),
                    );
                    self.here(done);
                } else {
                    cvt(self, RAX);
                }
                self.store_fp(XMM0, dst, to);
            }
            Conv::FToI { signed, bits } => self.f_to_i(from, to, dst, a, signed, bits),
            Conv::FToF => {
                self.load_fp(XMM0, a, from);
                self.scratch();
                self.op(
                    Some(Self::sse(from)),
                    false,
                    false,
                    &[0x0f, 0x5a],
                    XMM0,
                    Rm::R(XMM0),
                );
                self.store_fp(XMM0, dst, to);
            }
            Conv::Bits => {
                let r = self.reg(a, from, false, &[]);
                self.store(r, dst, to);
            }
        }
    }

    /// Float -> integer, saturating, NaN to 0 (`bits` of the destination are
    /// the integer's).
    fn f_to_i(&mut self, from: K, to: K, dst: Mem, a: V, signed: bool, bits: u32) {
        let pre = Self::sse(from);
        let ucomi_pre = if from == K::F64 { Some(0x66) } else { None };
        // xmm1 = 2^63 (as `from`).
        let two63 = if from == K::F64 {
            (63f64.exp2()).to_bits()
        } else {
            u64::from(63f32.exp2().to_bits())
        };
        self.load_fp(XMM0, a, from);
        self.scratch();
        self.mov_imm(RAX, two63);
        self.op(
            Some(0x66),
            from == K::F64,
            false,
            &[0x0f, 0x6e],
            XMM1,
            Rm::R(RAX),
        );
        let cvtt = |s: &mut Self| s.op(Some(pre), true, false, &[0x0f, 0x2c], RAX, Rm::R(XMM0));
        let ucomi = |s: &mut Self, x: u32, y: u32| {
            s.op(ucomi_pre, false, false, &[0x0f, 0x2e], x, Rm::R(y));
        };
        let mut ends = Vec::new();
        // NaN (and, unsigned, negative): 0.
        ucomi(self, XMM0, XMM0);
        let mut zeros = vec![self.jcc_fwd(CC_P)];
        if signed {
            ucomi(self, XMM0, XMM1);
            let big = self.jcc_fwd(CC_AE);
            cvtt(self); // too negative: i64::MIN
            ends.push(self.jmp_fwd());
            self.here(big);
            self.mov_imm(RAX, i64::MAX as u64);
        } else {
            // >= 2^64: max; >= 2^63: through the sign bit.
            self.bytes(&[0x0f, 0x57, 0xd2]); // xorps xmm2, xmm2
            ucomi(self, XMM0, 2);
            zeros.push(self.jcc_fwd(CC_B));
            ucomi(self, XMM0, XMM1);
            let high = self.jcc_fwd(CC_AE);
            cvtt(self);
            ends.push(self.jmp_fwd());
            self.here(high);
            self.op(Some(pre), false, false, &[0x0f, 0x5c], XMM0, Rm::R(XMM1)); // sub 2^63
            ucomi(self, XMM0, XMM1);
            let huge = self.jcc_fwd(CC_AE);
            cvtt(self);
            self.mov_imm(RCX, 1 << 63);
            self.alu(0x31, true, RAX, RCX);
            ends.push(self.jmp_fwd());
            self.here(huge);
            self.mov_imm(RAX, u64::MAX);
        }
        ends.push(self.jmp_fwd());
        for z in zeros {
            self.here(z);
        }
        self.bytes(&[0x31, 0xc0]); // xor eax, eax
        for e in ends {
            self.here(e);
        }
        if bits < 64 {
            let (max, min) = if signed {
                ((1i64 << (bits - 1)) - 1, -(1i64 << (bits - 1)))
            } else {
                ((1i64 << bits) - 1, 0)
            };
            self.mov_imm(RCX, max as u64);
            self.alu(0x39, true, RAX, RCX);
            // cmovg / cmova rax, rcx
            let cc = if signed { CC_G } else { CC_A };
            self.op(None, true, false, &[0x0f, 0x40 + cc], RAX, Rm::R(RCX));
            if signed {
                self.mov_imm(RCX, min as u64);
                self.alu(0x39, true, RAX, RCX);
                self.op(None, true, false, &[0x0f, 0x40 + CC_L], RAX, Rm::R(RCX));
            }
        }
        self.store(RAX, dst, to);
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
            flags: None,
        }
    }

    /// The unwind encodings describe `PROLOGUE` only: no register
    /// callee-saved in either convention (`rbx`, `rsi` / `rdi` on
    /// Windows, `r12`..`r15`, `xmm6`..) may hold a value.
    #[test]
    fn values_stay_in_caller_saved_registers() {
        for r in POOL.iter().chain(&[R10, R11]) {
            assert!(![3, RSI, RDI, 12, 13, 14, 15].contains(r), "r{r}");
        }
        assert!(FP_POOL.iter().all(|x| *x < 6));
        let code = bytes(|e| {
            e.frame = 3 * PAGE + 32;
            e.prologue();
        });
        assert_eq!(code[..4], PROLOGUE);
    }

    /// The bytes `f` writes, checked against the reference assembler's
    /// encodings (REX, `ModRM`, SIB, displacement and immediate forms).
    fn bytes(f: impl FnOnce(&mut Em<'_>)) -> Vec<u8> {
        let mut obj = Obj::new(true, true, false);
        let mut e = em(&mut obj);
        f(&mut e);
        e.code
    }

    #[test]
    fn modrm_forms() {
        // mov rax, rcx / mov r9, r10
        assert_eq!(bytes(|e| e.mov_rr(RAX, RCX)), [0x48, 0x8b, 0xc1]);
        assert_eq!(bytes(|e| e.mov_rr(R9, R10)), [0x4d, 0x8b, 0xca]);
        assert_eq!(bytes(|e| e.mov_rr(RDX, RDX)), []);
        // lea rax, [rsp + 8]: a SIB byte for an rsp base.
        let lea = |e: &mut Em<'_>| e.op(None, true, false, &[0x8d], RAX, Rm::M(RSP, 8));
        assert_eq!(bytes(lea), [0x48, 0x8d, 0x44, 0x24, 0x08]);
        // mov rax, [rbp]: rbp takes a zero disp8.
        let rbp = |e: &mut Em<'_>| e.op(None, true, false, &[0x8b], RAX, Rm::M(RBP, 0));
        assert_eq!(bytes(rbp), [0x48, 0x8b, 0x45, 0x00]);
        // mov rdx, [r13 - 0x200]: disp32.
        let r13 = |e: &mut Em<'_>| e.op(None, true, false, &[0x8b], RDX, Rm::M(13, -0x200));
        assert_eq!(bytes(r13), [0x49, 0x8b, 0x95, 0x00, 0xfe, 0xff, 0xff]);
        // mov r8, [r12 + 0x100]: SIB for r12, REX.R and REX.B.
        let r12 = |e: &mut Em<'_>| e.op(None, true, false, &[0x8b], R8, Rm::M(12, 0x100));
        assert_eq!(bytes(r12), [0x4d, 0x8b, 0x84, 0x24, 0x00, 0x01, 0x00, 0x00]);
        // mov rcx, [rax]: no displacement.
        let rax = |e: &mut Em<'_>| e.op(None, true, false, &[0x8b], RCX, Rm::M(RAX, 0));
        assert_eq!(bytes(rax), [0x48, 0x8b, 0x08]);
        // mov sil, [rax] needs an empty REX; mov al, [rcx] none.
        let sil = |e: &mut Em<'_>| e.op(None, false, true, &[0x8a], RSI, Rm::M(RAX, 0));
        assert_eq!(bytes(sil), [0x40, 0x8a, 0x30]);
        let al = |e: &mut Em<'_>| e.op(None, false, true, &[0x8a], RAX, Rm::M(RCX, 0));
        assert_eq!(bytes(al), [0x8a, 0x01]);
        // movsd xmm1, [rsp + 0x80]: prefix before REX, disp32.
        let sd = |e: &mut Em<'_>| {
            e.op(
                Some(0xf2),
                false,
                false,
                &[0x0f, 0x10],
                XMM1,
                Rm::M(RSP, 0x80),
            );
        };
        assert_eq!(
            bytes(sd),
            [0xf2, 0x0f, 0x10, 0x8c, 0x24, 0x80, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn mov_imm_picks_the_shortest_form() {
        // mov r32, imm32 (zero-extends)
        assert_eq!(bytes(|e| e.mov_imm(RAX, 1)), [0xb8, 1, 0, 0, 0]);
        assert_eq!(
            bytes(|e| e.mov_imm(R10, 0xffff_ffff)),
            [0x41, 0xba, 0xff, 0xff, 0xff, 0xff]
        );
        // mov r64, simm32
        assert_eq!(
            bytes(|e| e.mov_imm(RCX, u64::MAX)),
            [0x48, 0xc7, 0xc1, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            bytes(|e| e.mov_imm(R9, i64::from(i32::MIN) as u64)),
            [0x49, 0xc7, 0xc1, 0x00, 0x00, 0x00, 0x80]
        );
        // movabs
        assert_eq!(
            bytes(|e| e.mov_imm(RDX, 1 << 32)),
            [0x48, 0xba, 0, 0, 0, 0, 1, 0, 0, 0]
        );
        assert_eq!(
            bytes(|e| e.mov_imm(R11, 1 << 63)),
            [0x49, 0xbb, 0, 0, 0, 0, 0, 0, 0, 0x80]
        );
        // Just below `i32::MIN`: not a simm32.
        assert_eq!(
            bytes(|e| e.mov_imm(RAX, (i64::from(i32::MIN) - 1) as u64)),
            [0x48, 0xb8, 0xff, 0xff, 0xff, 0x7f, 0xff, 0xff, 0xff, 0xff]
        );
    }

    #[test]
    fn forward_jumps_patch_their_rel32() {
        let code = bytes(|e| {
            let at = e.jcc_fwd(CC_NE);
            e.mov_imm(RAX, 1);
            e.here(at);
        });
        assert_eq!(code, [0x0f, 0x85, 5, 0, 0, 0, 0xb8, 1, 0, 0, 0]);
        let code = bytes(|e| {
            let at = e.jmp_fwd();
            e.here(at);
        });
        assert_eq!(code, [0xe9, 0, 0, 0, 0]);
    }
}
