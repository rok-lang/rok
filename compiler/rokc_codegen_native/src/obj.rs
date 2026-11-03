//! A relocatable object in the making: sections, symbols, relocations.
//! The container format (`macho`, ..) only serializes it.

use rokc_core::HashMap;

use crate::lir::SymId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Sect {
    Text,
    /// Read-only data without relocations.
    Const,
    /// Read-only data with relocations (pointers).
    DataConst,
    Data,
}

pub const SECTS: [Sect; 4] = [Sect::Text, Sect::Const, Sect::DataConst, Sect::Data];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelKind {
    /// `bl` / `b` to a symbol.
    Branch26,
    /// `adrp` of a symbol's page / the low 12 bits (`add` or a scaled
    /// load / store offset).
    Page21,
    PageOff12,
    /// `adrp` / `ldr` of the symbol's GOT slot.
    GotPage21,
    GotPageOff12,
    /// `adrp` / `ldr` of a thread-local's descriptor.
    TlvPage21,
    TlvPageOff12,
    /// A 64-bit absolute address (data).
    Abs64,
    /// `x86_64` `call rel32` to a function.
    X86Call,
    /// `x86_64` `[rip + rel32]` of a symbol.
    X86Pc32,
    /// `x86_64` `mov reg, [rip + rel32]` of the symbol's GOT slot.
    X86GotLoad,
    /// `x86_64` `mov reg, [rip + rel32]` of a thread-local: its TLV
    /// descriptor (Mach-O), its thread-pointer offset (ELF initial exec).
    X86Tls,
    /// COFF: a thread-local's 32-bit offset in the TLS section (`x86_64`
    /// displacement).
    X86SecRel,
    /// COFF arm64: `add` of bits 12..24 / 0..12 of a thread-local's TLS
    /// section offset.
    SecRelHi12,
    SecRelLo12,
    /// DWARF `sec_offset` into debug section `sym` (an `Obj::extra` index):
    /// relocated on ELF (sections join at link), kept as is on Mach-O.
    SecOff32,
}

#[derive(Clone, Copy, Debug)]
pub struct Reloc {
    pub off: u32,
    pub kind: RelKind,
    pub sym: SymId,
}

#[derive(Clone, Debug, Default)]
pub struct Section {
    pub data: Vec<u8>,
    pub relocs: Vec<Reloc>,
    /// log2 of the alignment.
    pub align: u32,
}

impl Section {
    /// Align the end to `align` bytes; the offset there.
    pub fn align_to(&mut self, align: u32) -> u32 {
        let a = align.max(1);
        while self.data.len() % a as usize != 0 {
            self.data.push(0);
        }
        self.align = self.align.max(a.trailing_zeros());
        self.data.len() as u32
    }
}

#[derive(Clone, Debug)]
pub struct Sym {
    pub name: String,
    /// `(section, offset, size)` when defined here.
    pub def: Option<(Sect, u32, u32)>,
    pub global: bool,
    pub weak: bool,
    /// Global within the link only (Mach-O private extern, ELF hidden;
    /// a COFF image exports nothing unasked).
    pub hidden: bool,
}

/// A code range of a function (unwind info, debug info).
#[derive(Clone, Debug)]
pub struct FnRange {
    pub sym: SymId,
    pub start: u32,
    pub len: u32,
    /// `(code offset, line, column, scope)` rows.
    pub lines: Vec<(u32, u32, u32, u32)>,
    /// `-g`: the function's facts, and each variable's offset from the
    /// frame pointer.
    pub debug: Option<(crate::lir::FnDebug, Vec<i32>)>,
}

/// A section's bytes from one definition to the next: its own section in
/// ELF and COFF, so the linker drops unused ones.
pub struct Piece {
    pub sect: Sect,
    pub start: u32,
    pub end: u32,
    /// log2 of the alignment.
    pub align: u32,
    /// The symbols defined at `start`, the first one names the piece.
    pub syms: Vec<SymId>,
    /// Relocations, offsets from `start`.
    pub relocs: Vec<Reloc>,
}

#[derive(Default)]
pub struct Obj {
    pub syms: Vec<Sym>,
    by_name: HashMap<String, SymId>,
    pub sects: HashMap<Sect, Section>,
    strings: HashMap<Vec<u8>, SymId>,
    pub fns: Vec<FnRange>,
    /// More sections: `(segment, section, flags, contents)` (debug info).
    pub extra: Vec<(&'static str, &'static str, u32, Section)>,
    /// `x86_64` code (else arm64).
    pub x86: bool,
    /// An ELF object (else Mach-O, or COFF).
    pub elf: bool,
    /// A COFF object (Windows: no GOT, TLS through `_tls_index`).
    pub coff: bool,
}

impl Obj {
    #[must_use]
    pub fn new(x86: bool, elf: bool, coff: bool) -> Self {
        let mut o = Self {
            x86,
            elf,
            coff,
            ..Self::default()
        };
        for s in SECTS {
            o.sects.insert(s, Section::default());
        }
        o
    }

    /// Drop undefined symbols no relocation uses (a REPL cell declares
    /// every earlier function).
    pub fn drop_unreferenced(&mut self) {
        let refs = |r: &Reloc| r.kind != RelKind::SecOff32;
        let mut keep: Vec<bool> = self.syms.iter().map(|s| s.def.is_some()).collect();
        for s in self.sects.values().chain(self.extra.iter().map(|e| &e.3)) {
            for r in s.relocs.iter().filter(|r| refs(r)) {
                keep[r.sym as usize] = true;
            }
        }
        let mut map = vec![SymId::MAX; self.syms.len()];
        let mut n = 0;
        for (i, k) in keep.iter().enumerate() {
            if *k {
                map[i] = n;
                n += 1;
            }
        }
        if n as usize == self.syms.len() {
            return;
        }
        let mut i = 0;
        self.syms.retain(|_| {
            i += 1;
            keep[i - 1]
        });
        let sects = self.sects.values_mut();
        for s in sects.chain(self.extra.iter_mut().map(|e| &mut e.3)) {
            for r in s.relocs.iter_mut().filter(|r| refs(r)) {
                r.sym = map[r.sym as usize];
            }
        }
        for f in &mut self.fns {
            f.sym = map[f.sym as usize];
        }
        self.by_name.retain(|_, s| keep[*s as usize]);
        for s in self.by_name.values_mut().chain(self.strings.values_mut()) {
            *s = map[*s as usize];
        }
    }

    /// Some code so far refers to `name` (or defines it).
    #[must_use]
    pub fn named(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    pub fn sym(&mut self, name: &str) -> SymId {
        if let Some(s) = self.by_name.get(name) {
            return *s;
        }
        self.syms.push(Sym {
            name: name.to_owned(),
            def: None,
            global: true,
            weak: false,
            hidden: false,
        });
        let id = self.syms.len() as SymId - 1;
        self.by_name.insert(name.to_owned(), id);
        id
    }

    /// Every non-empty section split at its symbol definitions, and the
    /// piece of each defined symbol.
    #[must_use]
    pub fn pieces(&self) -> (Vec<Piece>, Vec<Option<usize>>) {
        let mut pieces: Vec<Piece> = Vec::new();
        let mut of = vec![None; self.syms.len()];
        for s in SECTS {
            let sec = &self.sects[&s];
            if sec.data.is_empty() {
                continue;
            }
            let mut defs: Vec<(u32, SymId)> = self
                .syms
                .iter()
                .enumerate()
                .filter_map(|(i, y)| y.def.filter(|d| d.0 == s).map(|d| (d.1, i as SymId)))
                .collect();
            defs.sort_unstable();
            let first = pieces.len();
            let mut prev = None;
            for (off, sym) in defs {
                if prev == Some(off) {
                    pieces.last_mut().expect("a piece").syms.push(sym);
                } else {
                    pieces.push(Piece {
                        sect: s,
                        // Bytes before the first definition go with it.
                        start: if prev.is_none() { 0 } else { off },
                        end: 0,
                        align: 0,
                        syms: vec![sym],
                        relocs: Vec::new(),
                    });
                }
                prev = Some(off);
                of[sym as usize] = Some(pieces.len() - 1);
            }
            let ps = &mut pieces[first..];
            for k in 0..ps.len() {
                ps[k].end = ps.get(k + 1).map_or(sec.data.len() as u32, |n| n.start);
                ps[k].align = sec.align.min(ps[k].start.trailing_zeros());
            }
            for r in &sec.relocs {
                let k = ps.partition_point(|p| p.start <= r.off) - 1;
                ps[k].relocs.push(Reloc {
                    off: r.off - ps[k].start,
                    ..*r
                });
            }
        }
        (pieces, of)
    }

    pub fn sect(&mut self, s: Sect) -> &mut Section {
        self.sects.get_mut(&s).expect("section")
    }

    /// Define `sym` at the end of `sect` with `bytes` (aligned to
    /// `align`); returns its offset.
    pub fn define_data(
        &mut self,
        sym: SymId,
        sect: Sect,
        bytes: &[u8],
        align: u32,
        global: bool,
    ) -> u32 {
        let s = self.sect(sect);
        let off = s.align_to(align);
        s.data.extend_from_slice(bytes);
        let sy = &mut self.syms[sym as usize];
        sy.def = Some((sect, off, bytes.len() as u32));
        sy.global = global;
        off
    }

    /// A pointer-sized relocated word at `off` of `sect`.
    pub fn abs64(&mut self, sect: Sect, off: u32, target: SymId) {
        self.sect(sect).relocs.push(Reloc {
            off,
            kind: RelKind::Abs64,
            sym: target,
        });
    }

    /// Constant bytes and a NUL (a string literal, a constant array),
    /// one per content.
    pub fn bytes(&mut self, content: &[u8]) -> SymId {
        if let Some(s) = self.strings.get(content) {
            return *s;
        }
        let name = rokc_codegen_mlir::mangle::str_global(content);
        let sym = self.sym(&name);
        let mut bytes = content.to_vec();
        bytes.push(0);
        self.define_data(sym, Sect::Const, &bytes, 1, false);
        self.strings.insert(content.to_vec(), sym);
        sym
    }
}
