//! Mach-O 64 relocatable object (`MH_OBJECT`) writer, arm64 and `x86_64`:
//! one unnamed segment, symbols local / external / undefined
//! (`LC_DYSYMTAB`), and a compact unwind entry per function (plain frame
//! pointer frames).

use crate::obj::{Obj, RelKind, SECTS, Sect};

const MH_MAGIC_64: u32 = 0xfeed_facf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_SUBTYPE_X86_64_ALL: u32 = 3;
const MH_OBJECT: u32 = 1;
const MH_SUBSECTIONS_VIA_SYMBOLS: u32 = 0x2000;
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_BUILD_VERSION: u32 = 0x32;
const N_SECT: u8 = 0xe;
const N_EXT: u8 = 0x1;
const N_PEXT: u8 = 0x10;
const N_WEAK_DEF: u16 = 0x80;
const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x400;
const S_ATTR_DEBUG: u32 = 0x0200_0000;
/// `UNWIND_ARM64_MODE_FRAME`: `fp` / `lr` pushed, `fp` the frame.
const UNWIND_ARM64_MODE_FRAME: u32 = 0x0400_0000;
/// `UNWIND_X86_64_MODE_RBP_FRAME`, no saved registers.
const UNWIND_X86_64_MODE_RBP_FRAME: u32 = 0x0100_0000;

/// `*_RELOC_UNSIGNED`, arm64 and `x86_64` alike.
const RELOC_UNSIGNED: u32 = 0;
const ARM64_RELOC_BRANCH26: u32 = 2;
const ARM64_RELOC_PAGE21: u32 = 3;
const ARM64_RELOC_PAGEOFF12: u32 = 4;
const ARM64_RELOC_GOT_LOAD_PAGE21: u32 = 5;
const ARM64_RELOC_GOT_LOAD_PAGEOFF12: u32 = 6;
const ARM64_RELOC_TLVP_LOAD_PAGE21: u32 = 8;
const ARM64_RELOC_TLVP_LOAD_PAGEOFF12: u32 = 9;
const X86_64_RELOC_SIGNED: u32 = 1;
const X86_64_RELOC_BRANCH: u32 = 2;
const X86_64_RELOC_GOT_LOAD: u32 = 3;
const X86_64_RELOC_TLV: u32 = 9;

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn name16(b: &mut Vec<u8>, s: &str) {
    let mut n = [0u8; 16];
    n[..s.len()].copy_from_slice(s.as_bytes());
    b.extend_from_slice(&n);
}

struct Out {
    seg: &'static str,
    name: &'static str,
    flags: u32,
    data: Vec<u8>,
    align: u32,
    /// `(offset, type, pcrel, length, extern, symbolnum)`.
    relocs: Vec<(u32, u32, bool, u32, bool, u32)>,
    addr: u64,
}

/// Serialize `obj`.
#[must_use]
pub fn write(obj: &Obj) -> Vec<u8> {
    // Symbol order: locals, external defined, undefined.
    let mut order: Vec<usize> = (0..obj.syms.len()).collect();
    let class = |i: usize| {
        let s = &obj.syms[i];
        match (s.def.is_some(), s.global) {
            (true, false) => 0,
            (true, true) => 1,
            (false, _) => 2,
        }
    };
    order.sort_by(|&a, &b| {
        class(a).cmp(&class(b)).then_with(|| match class(a) {
            0 => a.cmp(&b),
            _ => obj.syms[a].name.cmp(&obj.syms[b].name),
        })
    });
    let mut index = vec![0u32; obj.syms.len()];
    for (k, &i) in order.iter().enumerate() {
        index[i] = k as u32;
    }

    let sect_of = |s: Sect| -> (&'static str, &'static str, u32) {
        match s {
            Sect::Text => (
                "__TEXT",
                "__text",
                S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS,
            ),
            Sect::Const => ("__TEXT", "__const", 0),
            Sect::DataConst => ("__DATA", "__const", 0),
            Sect::Data => ("__DATA", "__data", 0),
        }
    };
    let mut outs: Vec<Out> = Vec::new();
    let mut sect_index = rokc_core::HashMap::default();
    for s in SECTS {
        let sec = &obj.sects[&s];
        if sec.data.is_empty() {
            continue;
        }
        let (seg, name, flags) = sect_of(s);
        let relocs = sec
            .relocs
            .iter()
            .map(|r| {
                let (ty, pcrel, len) = match r.kind {
                    RelKind::Branch26 => (ARM64_RELOC_BRANCH26, true, 2),
                    RelKind::Page21 => (ARM64_RELOC_PAGE21, true, 2),
                    RelKind::PageOff12 => (ARM64_RELOC_PAGEOFF12, false, 2),
                    RelKind::GotPage21 => (ARM64_RELOC_GOT_LOAD_PAGE21, true, 2),
                    RelKind::GotPageOff12 => (ARM64_RELOC_GOT_LOAD_PAGEOFF12, false, 2),
                    RelKind::TlvPage21 => (ARM64_RELOC_TLVP_LOAD_PAGE21, true, 2),
                    RelKind::TlvPageOff12 => (ARM64_RELOC_TLVP_LOAD_PAGEOFF12, false, 2),
                    RelKind::Abs64 => (RELOC_UNSIGNED, false, 3),
                    RelKind::X86Call => (X86_64_RELOC_BRANCH, true, 2),
                    RelKind::X86Pc32 => (X86_64_RELOC_SIGNED, true, 2),
                    RelKind::X86GotLoad => (X86_64_RELOC_GOT_LOAD, true, 2),
                    RelKind::X86Tls => (X86_64_RELOC_TLV, true, 2),
                    RelKind::X86SecRel
                    | RelKind::SecRelHi12
                    | RelKind::SecRelLo12
                    | RelKind::SecOff32 => {
                        unreachable!("{:?} in Mach-O code or data", r.kind)
                    }
                };
                (r.off, ty, pcrel, len, true, index[r.sym as usize])
            })
            .collect();
        sect_index.insert(s, outs.len() as u8 + 1);
        outs.push(Out {
            seg,
            name,
            flags,
            data: sec.data.clone(),
            align: sec.align.max(if s == Sect::Text { 2 } else { 0 }),
            relocs,
            addr: 0,
        });
    }
    // Compact unwind: one 32-byte entry per function.
    if !obj.fns.is_empty() {
        let mut data = Vec::new();
        let mut relocs = Vec::new();
        for f in &obj.fns {
            relocs.push((
                data.len() as u32,
                RELOC_UNSIGNED,
                false,
                3,
                true,
                index[f.sym as usize],
            ));
            put64(&mut data, 0);
            put32(&mut data, f.len);
            put32(
                &mut data,
                if obj.x86 {
                    UNWIND_X86_64_MODE_RBP_FRAME
                } else {
                    UNWIND_ARM64_MODE_FRAME
                },
            );
            put64(&mut data, 0);
            put64(&mut data, 0);
        }
        outs.push(Out {
            seg: "__LD",
            name: "__compact_unwind",
            flags: S_ATTR_DEBUG,
            data,
            align: 3,
            relocs,
            addr: 0,
        });
    }
    for (seg, name, flags, sec) in &obj.extra {
        outs.push(Out {
            seg,
            name,
            flags: *flags,
            data: sec.data.clone(),
            align: sec.align,
            // Section-relative (the bytes hold the object address), as
            // dsymutil reads debug info; offsets into debug sections stay.
            relocs: sec
                .relocs
                .iter()
                .filter(|r| r.kind != RelKind::SecOff32)
                .map(|r| {
                    let sect = obj.syms[r.sym as usize]
                        .def
                        .and_then(|d| sect_index.get(&d.0))
                        .copied()
                        .unwrap_or(1);
                    (r.off, RELOC_UNSIGNED, false, 3, false, u32::from(sect))
                })
                .collect(),
            addr: 0,
        });
    }
    // Addresses within the one segment.
    let mut addr = 0u64;
    for o in &mut outs {
        let a = 1u64 << o.align;
        addr = addr.next_multiple_of(a);
        o.addr = addr;
        addr += o.data.len() as u64;
    }
    let vmsize = addr;

    // Symbol and string tables.
    let mut strtab = vec![b' ', 0];
    let mut nlist = Vec::new();
    let (mut nlocal, mut nextdef, mut nundef) = (0u32, 0u32, 0u32);
    for &i in &order {
        let s = &obj.syms[i];
        let strx = strtab.len() as u32;
        strtab.extend_from_slice(b"_");
        strtab.extend_from_slice(s.name.as_bytes());
        strtab.push(0);
        let (ty, sect, desc, value) = match s.def {
            Some((sec, off, _)) => {
                let k = sect_index.get(&sec).copied().unwrap_or(0);
                let base = outs.get(k as usize - 1).map_or(0, |o| o.addr);
                let ext = match (s.global, s.hidden) {
                    (true, true) => N_EXT | N_PEXT,
                    (true, false) => N_EXT,
                    (false, _) => 0,
                };
                let desc = if s.weak { N_WEAK_DEF } else { 0 };
                (N_SECT | ext, k, desc, base + u64::from(off))
            }
            None => (N_EXT, 0, 0, 0),
        };
        match class(i) {
            0 => nlocal += 1,
            1 => nextdef += 1,
            _ => nundef += 1,
        }
        put32(&mut nlist, strx);
        nlist.push(ty);
        nlist.push(sect);
        nlist.extend_from_slice(&desc.to_le_bytes());
        put64(&mut nlist, value);
    }
    while strtab.len() % 8 != 0 {
        strtab.push(0);
    }

    // Load commands.
    let nsects = outs.len() as u32;
    let seg_size = 72 + 80 * nsects;
    let build_size = 24;
    let symtab_size = 24;
    let dysym_size = 80;
    let sizeofcmds = seg_size + build_size + symtab_size + dysym_size;
    let header = 32;
    let mut file_off = u64::from(header + sizeofcmds);
    let mut sect_offs = Vec::new();
    for o in &outs {
        let a = 1u64 << o.align;
        file_off = file_off.next_multiple_of(a);
        sect_offs.push(file_off);
        file_off += o.data.len() as u64;
    }
    let seg_fileoff = sect_offs.first().copied().unwrap_or(file_off);
    let seg_filesize = file_off - seg_fileoff;
    file_off = file_off.next_multiple_of(8);
    let mut reloc_offs = Vec::new();
    for o in &outs {
        reloc_offs.push(file_off);
        file_off += 8 * o.relocs.len() as u64;
    }
    let symoff = file_off;
    file_off += nlist.len() as u64;
    let stroff = file_off;

    let mut b = Vec::new();
    put32(&mut b, MH_MAGIC_64);
    if obj.x86 {
        put32(&mut b, CPU_TYPE_X86_64);
        put32(&mut b, CPU_SUBTYPE_X86_64_ALL);
    } else {
        put32(&mut b, CPU_TYPE_ARM64);
        put32(&mut b, 0);
    }
    put32(&mut b, MH_OBJECT);
    put32(&mut b, 4);
    put32(&mut b, sizeofcmds);
    put32(&mut b, MH_SUBSECTIONS_VIA_SYMBOLS);
    put32(&mut b, 0);
    // LC_SEGMENT_64
    put32(&mut b, LC_SEGMENT_64);
    put32(&mut b, seg_size);
    name16(&mut b, "");
    put64(&mut b, 0);
    put64(&mut b, vmsize);
    put64(&mut b, seg_fileoff);
    put64(&mut b, seg_filesize);
    put32(&mut b, 7);
    put32(&mut b, 7);
    put32(&mut b, nsects);
    put32(&mut b, 0);
    for (k, o) in outs.iter().enumerate() {
        name16(&mut b, o.name);
        name16(&mut b, o.seg);
        put64(&mut b, o.addr);
        put64(&mut b, o.data.len() as u64);
        put32(&mut b, sect_offs[k] as u32);
        put32(&mut b, o.align);
        put32(
            &mut b,
            if o.relocs.is_empty() {
                0
            } else {
                reloc_offs[k] as u32
            },
        );
        put32(&mut b, o.relocs.len() as u32);
        put32(&mut b, o.flags);
        put32(&mut b, 0);
        put32(&mut b, 0);
        put32(&mut b, 0);
    }
    // LC_BUILD_VERSION: macOS 11.0, no tools.
    put32(&mut b, LC_BUILD_VERSION);
    put32(&mut b, build_size);
    put32(&mut b, 1);
    put32(&mut b, 0x000b_0000);
    put32(&mut b, 0);
    put32(&mut b, 0);
    // LC_SYMTAB
    put32(&mut b, LC_SYMTAB);
    put32(&mut b, symtab_size);
    put32(&mut b, symoff as u32);
    put32(&mut b, order.len() as u32);
    put32(&mut b, stroff as u32);
    put32(&mut b, strtab.len() as u32);
    // LC_DYSYMTAB
    put32(&mut b, LC_DYSYMTAB);
    put32(&mut b, dysym_size);
    for v in [0, nlocal, nlocal, nextdef, nlocal + nextdef, nundef] {
        put32(&mut b, v);
    }
    for _ in 0..12 {
        put32(&mut b, 0);
    }
    // Section contents.
    for (k, o) in outs.iter().enumerate() {
        b.resize(sect_offs[k] as usize, 0);
        b.extend_from_slice(&o.data);
    }
    b.resize(b.len().next_multiple_of(8), 0);
    for o in &outs {
        for &(off, ty, pcrel, len, ext, sym) in &o.relocs {
            put32(&mut b, off);
            put32(
                &mut b,
                sym | (u32::from(pcrel) << 24) | (len << 25) | (u32::from(ext) << 27) | (ty << 28),
            );
        }
    }
    b.extend_from_slice(&nlist);
    b.extend_from_slice(&strtab);
    b
}
