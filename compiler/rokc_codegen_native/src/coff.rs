//! COFF relocatable object writer (Windows), `x86_64` and arm64.
//!
//! A COMDAT section per function and data object so `/opt:ref` drops the
//! unused; their relocations; `.pdata` / `.xdata` unwind tables (frame pointer
//! frames; arm64 functions past 1 MiB in fragments) in COMDATs associated with
//! the code; symbols and strings; `/bigobj` past 65279 sections. No debug info:
//! Windows `-g` builds go through LLVM's `CodeView`.

use crate::obj::{FnRange, Obj, RelKind, Reloc, Sect};

const MACHINE_AMD64: u16 = 0x8664;
const MACHINE_ARM64: u16 = 0xaa64;
const SCN_CNT_CODE: u32 = 0x20;
const SCN_CNT_INITIALIZED_DATA: u32 = 0x40;
const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const SCN_MEM_READ: u32 = 0x4000_0000;
const SCN_MEM_WRITE: u32 = 0x8000_0000;
const SCN_LNK_NRELOC_OVFL: u32 = 0x0100_0000;
const SCN_LNK_COMDAT: u32 = 0x1000;
const SELECT_NODUPLICATES: u8 = 1;
const SELECT_ANY: u8 = 2;
const SELECT_ASSOCIATIVE: u8 = 5;
/// Sections a regular object holds; more need a big object.
const MAX_SECTIONS16: usize = 65279;
const BIGOBJ_CLASS_ID: [u8; 16] = [
    0xc7, 0xa1, 0xba, 0xd1, 0xee, 0xba, 0xa9, 0x4b, 0xaf, 0x20, 0xfa, 0xf6, 0x6a, 0xa4, 0xdc, 0xb8,
];
const SYM_CLASS_EXTERNAL: u8 = 2;
const SYM_CLASS_STATIC: u8 = 3;
const SYM_CLASS_WEAK_EXTERNAL: u8 = 105;
const SYM_DTYPE_FUNCTION: u16 = 0x20;
const WEAK_EXTERN_SEARCH_ALIAS: u32 = 3;

const REL_AMD64_ADDR64: u16 = 1;
const REL_AMD64_ADDR32NB: u16 = 3;
const REL_AMD64_REL32: u16 = 4;
const REL_AMD64_SECREL: u16 = 0xb;
const REL_ARM64_ADDR32NB: u16 = 2;
const REL_ARM64_BRANCH26: u16 = 3;
const REL_ARM64_PAGEBASE_REL21: u16 = 4;
const REL_ARM64_PAGEOFFSET_12A: u16 = 6;
const REL_ARM64_PAGEOFFSET_12L: u16 = 7;
const REL_ARM64_SECREL_LOW12A: u16 = 9;
const REL_ARM64_SECREL_HIGH12A: u16 = 0xa;
const REL_ARM64_ADDR64: u16 = 0xe;

/// x64 `UNWIND_INFO` shared by every function: version 1, 4-byte prolog,
/// `rbp` frame register at offset 0; codes in reverse: `mov rbp, rsp` at 4
/// (`UWOP_SET_FPREG`), `push rbp` at 1 (`UWOP_PUSH_NONVOL`).
const X64_UNWIND: [u8; 8] = [0x01, 0x04, 0x02, 0x05, 0x04, 0x03, 0x01, 0x50];

/// arm64 unwind codes shared by every prolog and epilog: `set_fp`,
/// `save_fplr_x` 16, `end`, a `nop` pad (`a64::prologue` / `epilogue`).
const A64_CODES: [u8; 4] = [0xe1, 0x81, 0xe4, 0xe3];
/// The codes of a fragment without the prolog: `end_c`, then those.
const A64_CODES_NO_PROLOG: [u8; 4] = [0xe5, 0xe1, 0x81, 0xe4];
/// Longest fragment an `.xdata` record covers: 2^18 - 1 words.
const A64_FRAGMENT: u32 = 0x3_ffff * 4;
/// The first two instructions of `a64::EPILOGUE`.
const A64_EPILOG: [u32; 2] = [crate::a64::EPILOGUE[0], crate::a64::EPILOGUE[1]];

/// `(offset, type, symbol table index)`.
type Rel = (u32, u16, u32);

struct Out {
    name: &'static str,
    flags: u32,
    data: Vec<u8>,
    /// log2 of the alignment.
    align: u32,
    relocs: Vec<Rel>,
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

/// Symbol table records and the string table of the longer names; a
/// big object's records are 20 bytes (a 32-bit section number).
#[derive(Default)]
struct SymTab {
    recs: Vec<u8>,
    strs: Vec<u8>,
    big: bool,
}

impl SymTab {
    const fn size(&self) -> usize {
        if self.big { 20 } else { 18 }
    }

    /// A record, its name inline or in the string table; `aux`
    /// auxiliary records follow.
    fn add(&mut self, name: &str, value: u32, sect: u32, ty: u16, class: u8, aux: u8) {
        let b = &mut self.recs;
        if name.len() <= 8 {
            let mut n = [0u8; 8];
            n[..name.len()].copy_from_slice(name.as_bytes());
            b.extend_from_slice(&n);
        } else {
            put32(b, 0);
            put32(b, self.strs.len() as u32 + 4);
            self.strs.extend_from_slice(name.as_bytes());
            self.strs.push(0);
        }
        put32(b, value);
        if self.big {
            put32(b, sect);
        } else {
            put16(b, sect as u16);
        }
        put16(b, ty);
        b.push(class);
        b.push(aux);
    }

    /// An auxiliary record (18 bytes, padded in a big object).
    fn aux(&mut self, rec: [u8; 18]) {
        self.recs.extend_from_slice(&rec);
        if self.big {
            self.recs.extend_from_slice(&[0, 0]);
        }
    }

    /// A section's symbol and its definition record: `relocs`, the
    /// COMDAT selection and the associated section (`IMAGE_COMDAT_*`).
    fn section(&mut self, name: &str, sect: u32, o: &Out, selection: u8, assoc: u32) {
        self.add(name, 0, sect, 0, SYM_CLASS_STATIC, 1);
        let mut rec = [0u8; 18];
        rec[..4].copy_from_slice(&(o.data.len() as u32).to_le_bytes());
        let nrel = o.relocs.len().min(0xffff) as u16;
        rec[4..6].copy_from_slice(&nrel.to_le_bytes());
        rec[12..14].copy_from_slice(&(assoc as u16).to_le_bytes());
        rec[14] = selection;
        rec[16..18].copy_from_slice(&((assoc >> 16) as u16).to_le_bytes());
        self.aux(rec);
    }

    fn len(&self) -> u32 {
        (self.recs.len() / self.size()) as u32
    }
}

/// The relocation type of `r` in a section with contents `data`.
fn rel_type(r: &Reloc, data: &[u8], x86: bool) -> u16 {
    if x86 {
        return match r.kind {
            RelKind::X86Call | RelKind::X86Pc32 => REL_AMD64_REL32,
            RelKind::X86SecRel => REL_AMD64_SECREL,
            RelKind::Abs64 => REL_AMD64_ADDR64,
            k => unreachable!("{k:?} in a COFF object"),
        };
    }
    match r.kind {
        RelKind::Branch26 => REL_ARM64_BRANCH26,
        RelKind::Page21 => REL_ARM64_PAGEBASE_REL21,
        // A load / store scales the offset; an `add` not.
        RelKind::PageOff12 => {
            let at = r.off as usize;
            let ins = u32::from_le_bytes(data[at..at + 4].try_into().unwrap_or([0; 4]));
            if ins & 0x3b00_0000 == 0x3900_0000 {
                REL_ARM64_PAGEOFFSET_12L
            } else {
                REL_ARM64_PAGEOFFSET_12A
            }
        }
        RelKind::SecRelHi12 => REL_ARM64_SECREL_HIGH12A,
        RelKind::SecRelLo12 => REL_ARM64_SECREL_LOW12A,
        RelKind::Abs64 => REL_ARM64_ADDR64,
        k => unreachable!("{k:?} in a COFF object"),
    }
}

/// Serialize `obj`.
#[must_use]
pub fn write(obj: &Obj) -> Vec<u8> {
    let x86 = obj.x86;
    let (pieces, piece_of) = obj.pieces();
    let data_flags = SCN_CNT_INITIALIZED_DATA | SCN_MEM_READ;
    // Sections: the pieces, x64's shared `UNWIND_INFO`, then each
    // function's unwind info associated with its code so they drop together.
    let mut outs: Vec<Out> = pieces
        .iter()
        .map(|p| {
            let sec = &obj.sects[&p.sect];
            let (name, flags) = match p.sect {
                Sect::Text => (".text", SCN_CNT_CODE | SCN_MEM_EXECUTE | SCN_MEM_READ),
                Sect::Const | Sect::DataConst => (".rdata", data_flags),
                Sect::Data => (".data", data_flags | SCN_MEM_WRITE),
            };
            Out {
                name,
                flags: flags | SCN_LNK_COMDAT,
                data: sec.data[p.start as usize..p.end as usize].to_vec(),
                align: p.align,
                relocs: Vec::new(),
            }
        })
        .collect();
    if x86 && !obj.fns.is_empty() {
        outs.push(Out {
            name: ".xdata",
            flags: data_flags,
            data: X64_UNWIND.to_vec(),
            align: 2,
            relocs: Vec::new(),
        });
    }
    // Per function: its piece, its `.xdata` (arm64: a record per
    // fragment), its `.pdata` (arm64: an entry per fragment).
    let mut unwind = Vec::new();
    for f in &obj.fns {
        let k = piece_of[f.sym as usize].expect("a defined function");
        let at = f.start - pieces[k].start;
        let mut pdata = Vec::new();
        let xdata = if x86 {
            put32(&mut pdata, at);
            put32(&mut pdata, at + f.len);
            put32(&mut pdata, 0);
            None
        } else {
            let mut data = Vec::new();
            for (start, rec) in a64_xdata(obj, f) {
                put32(&mut pdata, at + start);
                put32(&mut pdata, data.len() as u32);
                data.extend_from_slice(&rec);
            }
            outs.push(Out {
                name: ".xdata",
                flags: data_flags | SCN_LNK_COMDAT,
                data,
                align: 2,
                relocs: Vec::new(),
            });
            Some(outs.len() - 1)
        };
        outs.push(Out {
            name: ".pdata",
            flags: data_flags | SCN_LNK_COMDAT,
            data: pdata,
            align: 2,
            relocs: Vec::new(),
        });
        unwind.push((k, xdata, outs.len() - 1));
    }

    // Symbols: each section's (a COMDAT's then its leader), then the rest.
    // A weak definition is a weak external aliasing `.weak.<name>.default`
    // as LLVM writes it, that default leading an any-copy COMDAT.
    let mut syms = SymTab {
        big: outs.len() > MAX_SECTIONS16,
        ..SymTab::default()
    };
    let assoc: Vec<Option<usize>> = {
        let mut a = vec![None; outs.len()];
        for &(k, x, p) in &unwind {
            a[p] = Some(k);
            if let Some(x) = x {
                a[x] = Some(k);
            }
        }
        a
    };
    let def = |i: usize| {
        let s = &obj.syms[i];
        let k = piece_of[i]?;
        let (sec, off, _) = s.def?;
        let ty = if sec == Sect::Text {
            SYM_DTYPE_FUNCTION
        } else {
            0
        };
        Some((k as u32 + 1, off - pieces[k].start, ty))
    };
    let mut sect_sym = vec![0u32; outs.len()];
    let mut index = vec![u32::MAX; obj.syms.len()];
    // The record of each weak definition's default.
    let mut defaults = vec![u32::MAX; obj.syms.len()];
    for (k, o) in outs.iter().enumerate() {
        sect_sym[k] = syms.len();
        let n = k as u32 + 1;
        if let Some(p) = assoc[k] {
            syms.section(o.name, n, o, SELECT_ASSOCIATIVE, p as u32 + 1);
            continue;
        }
        let Some(p) = pieces.get(k) else {
            syms.section(o.name, n, o, 0, 0);
            continue;
        };
        let lead = p.syms[0] as usize;
        let s = &obj.syms[lead];
        let (sect, value, ty) = def(lead).expect("a defined symbol");
        if s.weak {
            syms.section(o.name, n, o, SELECT_ANY, 0);
            defaults[lead] = syms.len();
            let name = format!(".weak.{}.default", s.name);
            syms.add(&name, value, sect, ty, SYM_CLASS_EXTERNAL, 0);
        } else {
            syms.section(o.name, n, o, SELECT_NODUPLICATES, 0);
            index[lead] = syms.len();
            let class = if s.global {
                SYM_CLASS_EXTERNAL
            } else {
                SYM_CLASS_STATIC
            };
            syms.add(&s.name, value, sect, ty, class, 0);
        }
    }
    for (i, s) in obj.syms.iter().enumerate() {
        if index[i] != u32::MAX {
            continue;
        }
        let (sect, value, ty) = def(i).unwrap_or((0, 0, 0));
        if s.weak && s.def.is_some() {
            if defaults[i] == u32::MAX {
                defaults[i] = syms.len();
                let name = format!(".weak.{}.default", s.name);
                syms.add(&name, value, sect, ty, SYM_CLASS_EXTERNAL, 0);
            }
            index[i] = syms.len();
            syms.add(&s.name, 0, 0, ty, SYM_CLASS_WEAK_EXTERNAL, 1);
            let mut rec = [0u8; 18];
            rec[..4].copy_from_slice(&defaults[i].to_le_bytes());
            rec[4..8].copy_from_slice(&WEAK_EXTERN_SEARCH_ALIAS.to_le_bytes());
            syms.aux(rec);
            continue;
        }
        index[i] = syms.len();
        let class = if s.def.is_some() && !s.global {
            SYM_CLASS_STATIC
        } else {
            SYM_CLASS_EXTERNAL
        };
        syms.add(&s.name, value, sect, ty, class, 0);
    }

    // Relocations: the pieces' against symbols; unwind info's against
    // section symbols, the offset in the place.
    for (k, p) in pieces.iter().enumerate() {
        let o = &mut outs[k];
        o.relocs = p
            .relocs
            .iter()
            .map(|r| (r.off, rel_type(r, &o.data, x86), index[r.sym as usize]))
            .collect();
    }
    let rva = if x86 {
        REL_AMD64_ADDR32NB
    } else {
        REL_ARM64_ADDR32NB
    };
    for &(k, xdata, pdata) in &unwind {
        let code = sect_sym[k];
        outs[pdata].relocs = if x86 {
            let shared = sect_sym[pieces.len()];
            vec![(0, rva, code), (4, rva, code), (8, rva, shared)]
        } else {
            let x = sect_sym[xdata.unwrap_or(0)];
            (0..outs[pdata].data.len() as u32)
                .step_by(8)
                .flat_map(|e| [(e, rva, code), (e + 4, rva, x)])
                .collect()
        };
    }

    // Layout: header, section headers, each section's data and
    // relocations, symbols, strings.
    let machine = if x86 { MACHINE_AMD64 } else { MACHINE_ARM64 };
    let mut b = Vec::new();
    let symoff_at;
    if syms.big {
        put16(&mut b, 0);
        put16(&mut b, 0xffff);
        put16(&mut b, 2);
        put16(&mut b, machine);
        put32(&mut b, 0);
        b.extend_from_slice(&BIGOBJ_CLASS_ID);
        b.extend_from_slice(&[0; 16]);
        put32(&mut b, outs.len() as u32);
        symoff_at = b.len();
        put32(&mut b, 0);
        put32(&mut b, syms.len());
    } else {
        put16(&mut b, machine);
        put16(&mut b, outs.len() as u16);
        put32(&mut b, 0);
        symoff_at = b.len();
        put32(&mut b, 0);
        put32(&mut b, syms.len());
        put16(&mut b, 0);
        put16(&mut b, 0);
    }
    let headers = b.len();
    b.resize(headers + 40 * outs.len(), 0);
    for (k, o) in outs.iter().enumerate() {
        b.resize(b.len().next_multiple_of(1 << o.align.min(4)), 0);
        let data = b.len() as u32;
        b.extend_from_slice(&o.data);
        let rel = b.len() as u32;
        // More than 0xfffe: the count is in a first, extra record.
        let ovfl = o.relocs.len() > 0xfffe;
        if ovfl {
            put32(&mut b, o.relocs.len() as u32 + 1);
            put32(&mut b, 0);
            put16(&mut b, 0);
        }
        for &(off, ty, sym) in &o.relocs {
            put32(&mut b, off);
            put32(&mut b, sym);
            put16(&mut b, ty);
        }
        let mut h = Vec::with_capacity(40);
        let mut name = [0u8; 8];
        name[..o.name.len()].copy_from_slice(o.name.as_bytes());
        h.extend_from_slice(&name);
        put32(&mut h, 0);
        put32(&mut h, 0);
        put32(&mut h, o.data.len() as u32);
        put32(&mut h, data);
        put32(&mut h, if o.relocs.is_empty() { 0 } else { rel });
        put32(&mut h, 0);
        put16(&mut h, if ovfl { 0xffff } else { o.relocs.len() as u16 });
        put16(&mut h, 0);
        // `IMAGE_SCN_ALIGN_<n>BYTES`: log2 + 1 in bits 20..24.
        let ovfl_flag = if ovfl { SCN_LNK_NRELOC_OVFL } else { 0 };
        put32(&mut h, o.flags | ovfl_flag | ((o.align.min(13) + 1) << 20));
        b[headers + 40 * k..headers + 40 * (k + 1)].copy_from_slice(&h);
    }
    let symoff = b.len() as u32;
    b[symoff_at..symoff_at + 4].copy_from_slice(&symoff.to_le_bytes());
    b.extend_from_slice(&syms.recs);
    put32(&mut b, syms.strs.len() as u32 + 4);
    b.extend_from_slice(&syms.strs);
    b
}

/// arm64 `.xdata` per fragment: the length field holds 2^18 - 1 words, so
/// longer functions split (never inside an epilog), as LLVM does.
fn a64_xdata(obj: &Obj, f: &FnRange) -> Vec<(u32, Vec<u8>)> {
    let text = &obj.sects[&Sect::Text].data;
    let word = |at: usize| u32::from_le_bytes(text[at..at + 4].try_into().unwrap_or([0; 4]));
    let epilogs: Vec<u32> = (0..f.len.saturating_sub(4))
        .step_by(4)
        .filter(|&o| {
            let at = (f.start + o) as usize;
            [word(at), word(at + 4)] == A64_EPILOG
        })
        .collect();
    let mut out = Vec::new();
    let mut start = 0;
    while start < f.len {
        let mut end = (start + A64_FRAGMENT).min(f.len);
        // An epilog is `mov sp, x29; ldp; ret`.
        if let Some(&e) = epilogs.iter().find(|&&e| e < end && end < e + 12) {
            end = e;
        }
        let here: Vec<u32> = epilogs
            .iter()
            .filter(|&&e| (start..end).contains(&e))
            .map(|e| e - start)
            .collect();
        out.push((start, a64_fragment(end - start, &here, start == 0)));
        start = end;
    }
    out
}

/// `.xdata` of a `len`-byte fragment: an epilog scope per epilog, the shared
/// codes. Later fragments have no prolog (codes start with `end_c`); without an
/// epilog, the `E` form's epilog starts at that `end_c`.
fn a64_fragment(len: u32, epilogs: &[u32], prolog: bool) -> Vec<u8> {
    let words = len / 4;
    let n = epilogs.len() as u32;
    let mut xdata = Vec::new();
    // One code word; more than 31 epilogs need the extended header.
    if !prolog && n == 0 {
        put32(&mut xdata, words | (1 << 21) | (1 << 27));
    } else if n <= 31 {
        put32(&mut xdata, words | (n << 22) | (1 << 27));
    } else {
        put32(&mut xdata, words);
        put32(&mut xdata, n | (1 << 16));
    }
    // The epilog's codes follow `end_c`, if any.
    let index = u32::from(!prolog) << 22;
    for e in epilogs {
        put32(&mut xdata, (e / 4) | index);
    }
    if prolog {
        xdata.extend_from_slice(&A64_CODES);
    } else {
        xdata.extend_from_slice(&A64_CODES_NO_PROLOG);
    }
    xdata
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A function calling an undefined one, a string constant, a weak
    /// definition: section, symbol and relocation records as
    /// `llvm-readobj` reads them.
    #[test]
    fn x64_object_layout() {
        let mut o = Obj::new(true, false, true);
        let f = o.sym("f");
        let g = o.sym("g");
        let w = o.sym("w");
        let code = [0x55, 0x48, 0x89, 0xe5, 0xe8, 0, 0, 0, 0, 0x5d, 0xc3];
        o.sect(Sect::Text).data.extend_from_slice(&code);
        o.sect(Sect::Text).data.resize(16, 0);
        o.sect(Sect::Text).data.extend_from_slice(&code);
        o.sect(Sect::Text).relocs.push(Reloc {
            off: 5,
            kind: RelKind::X86Call,
            sym: g,
        });
        o.syms[f as usize].def = Some((Sect::Text, 0, 11));
        o.syms[w as usize].def = Some((Sect::Text, 16, 11));
        o.syms[w as usize].weak = true;
        for (sym, start) in [(f, 0), (w, 16)] {
            o.fns.push(FnRange {
                sym,
                start,
                len: 11,
                lines: Vec::new(),
                debug: None,
            });
        }
        let b = write(&o);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), MACHINE_AMD64);
        // f's and w's code, the shared .xdata, a .pdata each.
        assert_eq!(u16::from_le_bytes([b[2], b[3]]), 5);
        let symoff = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        // Five section symbols with their definitions; f, w's default,
        // g, the weak external w and its aux.
        assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 15);
        let sym = |i: usize| &b[symoff + 18 * i..symoff + 18 * (i + 1)];
        let sect = |i: usize| i16::from_le_bytes([sym(i)[12], sym(i)[13]]);
        // .text #1 (a COMDAT of no duplicates) and its leader f.
        assert_eq!(&sym(0)[..8], b".text\0\0\0");
        assert_eq!(sym(1)[14], SELECT_NODUPLICATES);
        assert_eq!(&sym(2)[..8], b"f\0\0\0\0\0\0\0");
        assert_eq!((sym(2)[16], sect(2)), (SYM_CLASS_EXTERNAL, 1));
        // .text #2 (any copy) led by w's external default.
        assert_eq!(sym(4)[14], SELECT_ANY);
        assert_eq!((sym(5)[16], sect(5)), (SYM_CLASS_EXTERNAL, 2));
        // f's .pdata (#4) goes with section 1.
        assert_eq!(&sym(8)[..8], b".pdata\0\0");
        assert_eq!((sym(9)[12], sym(9)[14]), (1, SELECT_ASSOCIATIVE));
        // g undefined, then w: a weak external aliasing its default.
        assert_eq!((&sym(12)[..8], sect(12)), (&b"g\0\0\0\0\0\0\0"[..], 0));
        assert_eq!(sym(13)[16], SYM_CLASS_WEAK_EXTERNAL);
        assert_eq!(u32::from_le_bytes(sym(14)[..4].try_into().unwrap()), 5);
        // The call's REL32 against g (record 12), in f's section.
        let text = 20;
        let rel = u32::from_le_bytes(b[text + 24..text + 28].try_into().unwrap()) as usize;
        assert_eq!(&b[rel..rel + 10], &[5, 0, 0, 0, 12, 0, 0, 0, 4, 0]);
    }

    /// An arm64 function past 1 MiB: two fragments, the first ending
    /// before an epilog that would straddle the limit; the second has no
    /// prolog (`end_c` first) and its epilogs' codes after it.
    #[test]
    fn a64_long_function_fragments() {
        const NOP: u32 = 0xd503_201f;
        const RET: u32 = 0xd65f_03c0;
        let len = 0x18_0000u32;
        let mut words = vec![NOP; len as usize / 4];
        words[0] = crate::a64::PROLOGUE[0];
        words[1] = crate::a64::PROLOGUE[1];
        let mut epilog = |at: u32| {
            let i = at as usize / 4;
            words[i..i + 3].copy_from_slice(&[A64_EPILOG[0], A64_EPILOG[1], RET]);
        };
        let straddling = A64_FRAGMENT - 4;
        epilog(straddling);
        epilog(len - 12);
        let mut o = Obj::new(false, false, true);
        let f = o.sym("f");
        for w in words {
            o.sect(Sect::Text).data.extend_from_slice(&w.to_le_bytes());
        }
        o.syms[f as usize].def = Some((Sect::Text, 0, len));
        o.fns.push(FnRange {
            sym: f,
            start: 0,
            len,
            lines: Vec::new(),
            debug: None,
        });
        let frags = a64_xdata(&o, &o.fns[0]);
        let word = |b: &[u8], i: usize| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap());
        assert_eq!(frags.len(), 2);
        let (at, first) = &frags[0];
        assert_eq!(*at, 0);
        assert_eq!(word(first, 0), (straddling / 4) | (1 << 27));
        assert_eq!(&first[4..], &A64_CODES);
        let (at, second) = &frags[1];
        assert_eq!(*at, straddling);
        assert_eq!(
            word(second, 0),
            ((len - straddling) / 4) | (2 << 22) | (1 << 27)
        );
        assert_eq!(word(second, 1), 1 << 22);
        assert_eq!(word(second, 2), ((len - 12 - straddling) / 4) | (1 << 22));
        assert_eq!(&second[12..], &A64_CODES_NO_PROLOG);
        // A fragment with neither prolog nor epilog: the `E` form.
        let x = a64_fragment(16, &[], false);
        assert_eq!(word(&x, 0), 4 | (1 << 21) | (1 << 27));
        if let Some(p) = std::env::var_os("ROK_DUMP_A64_LONG") {
            std::fs::write(p, write(&o)).unwrap();
        }
    }

    /// Past 65279 sections: a big object, its symbols 20 bytes.
    #[test]
    fn many_sections_make_a_big_object() {
        let mut o = Obj::new(true, false, true);
        for i in 0..65_280u32 {
            let s = o.sym(&format!("d{i}"));
            o.define_data(s, Sect::Const, &[1], 1, true);
        }
        let b = write(&o);
        if let Some(p) = std::env::var_os("ROK_DUMP_BIGOBJ") {
            std::fs::write(p, &b).unwrap();
        }
        assert_eq!(&b[..6], &[0, 0, 0xff, 0xff, 2, 0]);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), MACHINE_AMD64);
        assert_eq!(u32::from_le_bytes(b[44..48].try_into().unwrap()), 65_280);
        let symoff = u32::from_le_bytes(b[48..52].try_into().unwrap()) as usize;
        // A section symbol, its definition, the leader: the last one's
        // section number is 65280.
        assert_eq!(
            u32::from_le_bytes(b[52..56].try_into().unwrap()),
            3 * 65_280
        );
        let last = symoff + 20 * (3 * 65_280 - 1);
        assert_eq!(
            u32::from_le_bytes(b[last + 12..last + 16].try_into().unwrap()),
            65_280
        );
    }
}
