//! `-g` (spec 9.9.6): DWARF 4 matching the MLIR emitter's debug info
//! (`rokc_codegen_mlir::debug`): a subprogram per positioned function
//! (and generated ones) named by rok path, parameters and named locals in
//! `fp`-relative slots within lexical blocks, their types (as the LLVM
//! path describes them), and a line table.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rokc_ir_common::PrimTy;
use rokc_mir::gc_abi::HEADER_SIZE;
use rokc_mir::{DebugTy, MirModule, MirTy, Target};

use crate::obj::{FnRange, Obj, RelKind, Reloc, Section};

const DW_TAG_ARRAY_TYPE: u8 = 0x01;
const DW_TAG_FORMAL_PARAMETER: u8 = 0x05;
const DW_TAG_LEXICAL_BLOCK: u8 = 0x0b;
const DW_TAG_MEMBER: u8 = 0x0d;
const DW_TAG_POINTER_TYPE: u8 = 0x0f;
const DW_TAG_COMPILE_UNIT: u8 = 0x11;
const DW_TAG_STRUCTURE_TYPE: u8 = 0x13;
const DW_TAG_SUBROUTINE_TYPE: u8 = 0x15;
const DW_TAG_VARIANT: u8 = 0x19;
const DW_TAG_SUBRANGE_TYPE: u8 = 0x21;
const DW_TAG_VARIANT_PART: u8 = 0x33;
const DW_TAG_BASE_TYPE: u8 = 0x24;
const DW_TAG_SUBPROGRAM: u8 = 0x2e;
const DW_TAG_VARIABLE: u8 = 0x34;

const DW_AT_NAME: u8 = 0x03;
const DW_AT_BYTE_SIZE: u8 = 0x0b;
const DW_AT_STMT_LIST: u8 = 0x10;
const DW_AT_LOW_PC: u8 = 0x11;
const DW_AT_HIGH_PC: u8 = 0x12;
const DW_AT_LANGUAGE: u8 = 0x13;
const DW_AT_COMP_DIR: u8 = 0x1b;
const DW_AT_PRODUCER: u8 = 0x25;
const DW_AT_DISCR: u8 = 0x15;
const DW_AT_DISCR_VALUE: u8 = 0x16;
const DW_AT_COUNT: u8 = 0x37;
const DW_AT_DATA_MEMBER_LOCATION: u8 = 0x38;
const DW_AT_DECL_FILE: u8 = 0x3a;
const DW_AT_DECLARATION: u8 = 0x3c;
const DW_AT_ARTIFICIAL: u8 = 0x34;
const DW_AT_DECL_LINE: u8 = 0x3b;
const DW_AT_ENCODING: u8 = 0x3e;
const DW_AT_FRAME_BASE: u8 = 0x40;
const DW_AT_LOCATION: u8 = 0x02;
const DW_AT_TYPE: u8 = 0x49;
const DW_AT_RANGES: u8 = 0x55;

const DW_FORM_ADDR: u8 = 0x01;
const DW_FORM_DATA1: u8 = 0x0b;
const DW_FORM_DATA2: u8 = 0x05;
const DW_FORM_DATA4: u8 = 0x06;
const DW_FORM_STRING: u8 = 0x08;
const DW_FORM_SDATA: u8 = 0x0d;
const DW_FORM_UDATA: u8 = 0x0f;
const DW_FORM_REF4: u8 = 0x13;
const DW_FORM_SEC_OFFSET: u8 = 0x17;
const DW_FORM_EXPRLOC: u8 = 0x18;
const DW_FORM_FLAG_PRESENT: u8 = 0x19;

const DW_OP_FBREG: u8 = 0x91;
const DW_OP_DEREF: u8 = 0x06;
const DW_OP_PLUS_UCONST: u8 = 0x23;
/// `DW_OP_reg29` (arm64 `x29`) / `DW_OP_reg6` (`x86_64` `rbp`).
const DW_OP_REG29: u8 = 0x6d;
const DW_OP_REG6: u8 = 0x56;
/// Rust: the language debuggers read variant parts in (lldb only
/// there); rok's types are C-like otherwise.
const DW_LANG_RUST: u16 = 0x1c;

const DW_ATE_BOOLEAN: u8 = 0x02;
const DW_ATE_FLOAT: u8 = 0x04;
const DW_ATE_SIGNED: u8 = 0x05;
const DW_ATE_SIGNED_CHAR: u8 = 0x06;
const DW_ATE_UNSIGNED: u8 = 0x07;
const DW_ATE_UTF: u8 = 0x10;

// Abbreviation codes.
const A_CU: u8 = 1;
const A_SUB: u8 = 2;
const A_PARAM: u8 = 3;
const A_VAR: u8 = 4;
const A_BLOCK: u8 = 5;
const A_BASE: u8 = 6;
const A_PTR_NAMED: u8 = 7;
const A_PTR_TO: u8 = 8;
const A_STRUCT: u8 = 9;
const A_MEMBER1: u8 = 10;
const A_ARRAY: u8 = 11;
const A_SUBRANGE: u8 = 12;
const A_STRUCT_EMPTY: u8 = 13;
const A_MEMBER4: u8 = 14;
const A_STRUCT_DECL: u8 = 15;
const A_ARRAY_OPEN: u8 = 16;
const A_VARIANT_PART: u8 = 17;
const A_VARIANT: u8 = 18;
const A_DISCR: u8 = 19;
const A_SUB_RET: u8 = 20;
// Subprograms without parameters or locals: no child list (as LLVM).
const A_SUB_LEAF: u8 = 21;
const A_SUB_RET_LEAF: u8 = 22;
// Subroutine types: with a return type or not, with parameters or not.
const A_FN_RET: u8 = 23;
const A_FN: u8 = 24;
const A_FN_RET_LEAF: u8 = 25;
const A_FN_LEAF: u8 = 26;
const A_FN_PARAM: u8 = 27;

fn uleb(b: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            b.push(byte);
            return;
        }
        b.push(byte | 0x80);
    }
}

fn sleb(b: &mut Vec<u8>, mut v: i64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0);
        b.push(if done { byte } else { byte | 0x80 });
        if done {
            return;
        }
    }
}

fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

/// `split`: a code section per function (ELF), so ranges are per function
/// and a dropped one's is empty (both ends 0).
fn abbrevs(split: bool) -> Vec<u8> {
    let cu_end = if split {
        (DW_AT_RANGES, DW_FORM_SEC_OFFSET)
    } else {
        (DW_AT_HIGH_PC, DW_FORM_DATA4)
    };
    let mut b = Vec::new();
    let mut add = |code: u8, tag: u8, children: bool, attrs: &[(u8, u8)]| {
        b.push(code);
        b.push(tag);
        b.push(u8::from(children));
        for (a, f) in attrs {
            b.push(*a);
            b.push(*f);
        }
        b.extend_from_slice(&[0, 0]);
    };
    add(
        A_CU,
        DW_TAG_COMPILE_UNIT,
        true,
        &[
            (DW_AT_PRODUCER, DW_FORM_STRING),
            (DW_AT_LANGUAGE, DW_FORM_DATA2),
            (DW_AT_NAME, DW_FORM_STRING),
            (DW_AT_COMP_DIR, DW_FORM_STRING),
            (DW_AT_STMT_LIST, DW_FORM_SEC_OFFSET),
            (DW_AT_LOW_PC, DW_FORM_ADDR),
            cu_end,
        ],
    );
    let sub = [
        (DW_AT_NAME, DW_FORM_STRING),
        (DW_AT_DECL_FILE, DW_FORM_UDATA),
        (DW_AT_DECL_LINE, DW_FORM_UDATA),
        (DW_AT_LOW_PC, DW_FORM_ADDR),
        (
            DW_AT_HIGH_PC,
            if split { DW_FORM_ADDR } else { DW_FORM_DATA4 },
        ),
        (DW_AT_FRAME_BASE, DW_FORM_EXPRLOC),
        (DW_AT_TYPE, DW_FORM_REF4),
    ];
    add(A_SUB, DW_TAG_SUBPROGRAM, true, &sub[..6]);
    add(A_SUB_RET, DW_TAG_SUBPROGRAM, true, &sub);
    add(A_SUB_LEAF, DW_TAG_SUBPROGRAM, false, &sub[..6]);
    add(A_SUB_RET_LEAF, DW_TAG_SUBPROGRAM, false, &sub);
    let var = [
        (DW_AT_NAME, DW_FORM_STRING),
        (DW_AT_DECL_FILE, DW_FORM_UDATA),
        (DW_AT_DECL_LINE, DW_FORM_UDATA),
        (DW_AT_TYPE, DW_FORM_REF4),
        (DW_AT_LOCATION, DW_FORM_EXPRLOC),
    ];
    add(A_PARAM, DW_TAG_FORMAL_PARAMETER, false, &var);
    add(A_VAR, DW_TAG_VARIABLE, false, &var);
    add(
        A_BLOCK,
        DW_TAG_LEXICAL_BLOCK,
        true,
        &[(DW_AT_RANGES, DW_FORM_SEC_OFFSET)],
    );
    add(
        A_BASE,
        DW_TAG_BASE_TYPE,
        false,
        &[
            (DW_AT_NAME, DW_FORM_STRING),
            (DW_AT_ENCODING, DW_FORM_DATA1),
            (DW_AT_BYTE_SIZE, DW_FORM_UDATA),
        ],
    );
    add(
        A_PTR_NAMED,
        DW_TAG_POINTER_TYPE,
        false,
        &[
            (DW_AT_NAME, DW_FORM_STRING),
            (DW_AT_BYTE_SIZE, DW_FORM_UDATA),
        ],
    );
    add(
        A_PTR_TO,
        DW_TAG_POINTER_TYPE,
        false,
        &[(DW_AT_TYPE, DW_FORM_REF4), (DW_AT_BYTE_SIZE, DW_FORM_UDATA)],
    );
    add(
        A_STRUCT,
        DW_TAG_STRUCTURE_TYPE,
        true,
        &[
            (DW_AT_NAME, DW_FORM_STRING),
            (DW_AT_BYTE_SIZE, DW_FORM_UDATA),
        ],
    );
    add(
        A_MEMBER1,
        DW_TAG_MEMBER,
        false,
        &[
            (DW_AT_NAME, DW_FORM_STRING),
            (DW_AT_TYPE, DW_FORM_REF4),
            (DW_AT_DATA_MEMBER_LOCATION, DW_FORM_DATA1),
        ],
    );
    add(
        A_ARRAY,
        DW_TAG_ARRAY_TYPE,
        true,
        &[(DW_AT_TYPE, DW_FORM_REF4)],
    );
    add(
        A_SUBRANGE,
        DW_TAG_SUBRANGE_TYPE,
        false,
        &[(DW_AT_COUNT, DW_FORM_UDATA)],
    );
    add(
        A_STRUCT_EMPTY,
        DW_TAG_STRUCTURE_TYPE,
        false,
        &[
            (DW_AT_NAME, DW_FORM_STRING),
            (DW_AT_BYTE_SIZE, DW_FORM_UDATA),
        ],
    );
    add(
        A_MEMBER4,
        DW_TAG_MEMBER,
        false,
        &[
            (DW_AT_NAME, DW_FORM_STRING),
            (DW_AT_TYPE, DW_FORM_REF4),
            (DW_AT_DATA_MEMBER_LOCATION, DW_FORM_DATA4),
        ],
    );
    add(
        A_STRUCT_DECL,
        DW_TAG_STRUCTURE_TYPE,
        false,
        &[
            (DW_AT_NAME, DW_FORM_STRING),
            (DW_AT_DECLARATION, DW_FORM_FLAG_PRESENT),
        ],
    );
    add(
        A_ARRAY_OPEN,
        DW_TAG_ARRAY_TYPE,
        false,
        &[(DW_AT_TYPE, DW_FORM_REF4)],
    );
    add(
        A_VARIANT_PART,
        DW_TAG_VARIANT_PART,
        true,
        &[(DW_AT_DISCR, DW_FORM_REF4)],
    );
    add(
        A_VARIANT,
        DW_TAG_VARIANT,
        true,
        &[(DW_AT_DISCR_VALUE, DW_FORM_SDATA)],
    );
    add(
        A_DISCR,
        DW_TAG_MEMBER,
        false,
        &[
            (DW_AT_TYPE, DW_FORM_REF4),
            (DW_AT_DATA_MEMBER_LOCATION, DW_FORM_DATA1),
            (DW_AT_ARTIFICIAL, DW_FORM_FLAG_PRESENT),
        ],
    );
    let ret = [(DW_AT_TYPE, DW_FORM_REF4)];
    add(A_FN_RET, DW_TAG_SUBROUTINE_TYPE, true, &ret);
    add(A_FN, DW_TAG_SUBROUTINE_TYPE, true, &[]);
    add(A_FN_RET_LEAF, DW_TAG_SUBROUTINE_TYPE, false, &ret);
    add(A_FN_LEAF, DW_TAG_SUBROUTINE_TYPE, false, &[]);
    add(A_FN_PARAM, DW_TAG_FORMAL_PARAMETER, false, &ret);
    b.push(0);
    b
}

/// The type reference to the `any` being described, patched once it is
/// written: an `any` inside a type stored in one is that variant part.
const ANY_SELF: u32 = u32::MAX;

/// While an `any` is described: where references to it were written and
/// the types since that hold one, dropped from the cache with it.
#[derive(Default)]
struct AnyOpen {
    fixups: Vec<usize>,
    held: Vec<String>,
    held_offs: std::collections::HashSet<u32>,
    /// The last reference to it or to a held type.
    last: Option<usize>,
}

/// The `.debug_info` unit being written: type DIEs by key.
struct Info<'a> {
    b: Vec<u8>,
    relocs: Vec<Reloc>,
    types: HashMap<String, u32>,
    t: &'a Target,
    m: &'a MirModule,
    /// Structs being described, by key: a reference back to one (a
    /// `GcBuf` field) is a declaration, completed by name.
    building: HashMap<String, String>,
    /// The `any` being described ([`ANY_SELF`] refers to it).
    any_open: Option<AnyOpen>,
    /// `.debug_ranges`, entries relative to the unit's `low_pc`.
    ranges: Vec<u8>,
    ranges_relocs: Vec<Reloc>,
    cu_low: u32,
    /// The functions' code is in sections of their own (ELF): ranges
    /// are relocated addresses, the unit's `low_pc` 0.
    split: bool,
}

impl Info<'_> {
    fn u32(&mut self, v: u32) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }

    /// A reference to type DIE `ty`.
    fn type_ref(&mut self, ty: u32) {
        let at = self.b.len();
        if let Some(open) = &mut self.any_open {
            if ty == ANY_SELF {
                open.fixups.push(at);
            }
            if ty == ANY_SELF || open.held_offs.contains(&ty) {
                open.last = Some(at);
            }
        }
        self.u32(ty);
    }

    /// Cache type DIE `off` (written from `start` on) under `key`.
    fn cache(&mut self, key: String, start: usize, off: u32) {
        if let Some(open) = &mut self.any_open {
            if open.last.is_some_and(|p| p >= start) {
                open.held.push(key.clone());
                open.held_offs.insert(off);
            }
        }
        self.types.insert(key, off);
    }

    /// Offset `v` into debug section `sect` (an index of `Obj::extra`).
    fn sec_off(&mut self, sect: u32, v: u32) {
        self.relocs.push(Reloc {
            off: self.b.len() as u32,
            kind: RelKind::SecOff32,
            sym: sect,
        });
        self.u32(v);
    }

    /// The address `at` of function `sym`: its object address (a
    /// section-relative relocation, as LLVM writes them for dsymutil).
    fn addr(&mut self, sym: u32, at: u32) {
        self.relocs.push(Reloc {
            off: self.b.len() as u32,
            kind: RelKind::Abs64,
            sym,
        });
        self.b.extend_from_slice(&u64::from(at).to_le_bytes());
    }

    /// A `.debug_ranges` entry: `[lo, hi)` of function `f`'s code.
    fn range(&mut self, f: &FnRange, lo: u32, hi: u32) {
        for at in [lo, hi] {
            if self.split {
                self.ranges_relocs.push(Reloc {
                    off: self.ranges.len() as u32,
                    kind: RelKind::Abs64,
                    sym: f.sym,
                });
            }
            let v = f.start + at - self.cu_low;
            self.ranges.extend_from_slice(&u64::from(v).to_le_bytes());
        }
    }

    fn base(&mut self, p: PrimTy) -> Option<u32> {
        let w = self.t.pointer_width / 8;
        let (name, size, enc) = match p {
            PrimTy::I8 => ("i8", 1, DW_ATE_SIGNED),
            PrimTy::I16 => ("i16", 2, DW_ATE_SIGNED),
            PrimTy::I32 => ("i32", 4, DW_ATE_SIGNED),
            PrimTy::I64 => ("i64", 8, DW_ATE_SIGNED),
            PrimTy::Isize => ("isize", w, DW_ATE_SIGNED),
            PrimTy::U8 => ("u8", 1, DW_ATE_UNSIGNED),
            PrimTy::U16 => ("u16", 2, DW_ATE_UNSIGNED),
            PrimTy::U32 => ("u32", 4, DW_ATE_UNSIGNED),
            PrimTy::U64 => ("u64", 8, DW_ATE_UNSIGNED),
            PrimTy::Usize => ("usize", w, DW_ATE_UNSIGNED),
            PrimTy::F32 => ("f32", 4, DW_ATE_FLOAT),
            PrimTy::F64 => ("f64", 8, DW_ATE_FLOAT),
            PrimTy::Bool => ("bool", 1, DW_ATE_BOOLEAN),
            PrimTy::CChar => ("c_char", 1, DW_ATE_SIGNED_CHAR),
            PrimTy::Char => ("char", 4, DW_ATE_UTF),
            PrimTy::Str => return None,
        };
        if let Some(o) = self.types.get(name) {
            return Some(*o);
        }
        let off = self.b.len() as u32;
        self.b.push(A_BASE);
        cstr(&mut self.b, name);
        self.b.push(enc);
        uleb(&mut self.b, u64::from(size));
        self.types.insert(name.to_owned(), off);
        Some(off)
    }

    fn structure(&mut self, name: &str, size: u32, members: &[(String, u32, u32)]) -> u32 {
        let off = self.b.len() as u32;
        if members.is_empty() {
            self.b.push(A_STRUCT_EMPTY);
            cstr(&mut self.b, name);
            uleb(&mut self.b, u64::from(size));
            return off;
        }
        self.b.push(A_STRUCT);
        cstr(&mut self.b, name);
        uleb(&mut self.b, u64::from(size));
        for (n, ty, at) in members {
            self.member(n, *ty, *at);
        }
        self.b.push(0);
        off
    }

    fn member(&mut self, name: &str, ty: u32, at: u32) {
        if at < 256 {
            self.b.push(A_MEMBER1);
            cstr(&mut self.b, name);
            self.type_ref(ty);
            self.b.push(at as u8);
        } else {
            self.b.push(A_MEMBER4);
            cstr(&mut self.b, name);
            self.type_ref(ty);
            self.u32(at);
        }
    }

    fn pointer_to(&mut self, ty: u32) -> u32 {
        let o = self.b.len() as u32;
        self.b.push(A_PTR_TO);
        self.type_ref(ty);
        uleb(&mut self.b, u64::from(self.t.pointer_width / 8));
        o
    }

    /// The type DIE of `d`, written on first use; `None` for `()`.
    fn debug_ty(&mut self, d: &DebugTy) -> Option<u32> {
        let key = format!("{d:?}");
        if let Some(o) = self.types.get(&key) {
            return Some(*o);
        }
        let start = self.b.len();
        let off = match d {
            DebugTy::Mir(t) => return self.ty(t),
            // Shown as the value; the variable's location goes through
            // the reference.
            DebugTy::Cell(elem) => return self.debug_ty(elem),
            // A pointer to the object: its header, then the elements (as
            // many as its size holds).
            DebugTy::Buf(name, elem) => {
                let mut m = self.header()?;
                if let Some(e) = self.debug_ty(elem) {
                    let items = self.b.len() as u32;
                    self.b.push(A_ARRAY_OPEN);
                    self.type_ref(e);
                    m.push(("items".to_owned(), items, HEADER_SIZE));
                }
                let obj = self.structure(name, HEADER_SIZE, &m);
                self.pointer_to(obj)
            }
            // A pointer to the object: its header, then the value.
            DebugTy::Boxed(name, value) => {
                let mut m = self.header()?;
                let mut size = HEADER_SIZE;
                if let Some(v) = self.debug_ty(value) {
                    m.push(("value".to_owned(), v, HEADER_SIZE));
                    size += self.t.size_align(&value.mir()).0;
                }
                let obj = self.structure(name, size, &m);
                self.pointer_to(obj)
            }
            DebugTy::Any => {
                if self.any_open.is_some() {
                    return Some(ANY_SELF);
                }
                self.any_open = Some(AnyOpen::default());
                let m = self.m;
                let off = self.variant_part("any", &rokc_mir::any_ty(), &m.debug.any);
                let open = self.any_open.take().unwrap_or_default();
                let off = off?;
                for at in open.fixups {
                    self.b[at..at + 4].copy_from_slice(&off.to_le_bytes());
                }
                for key in open.held {
                    self.types.remove(&key);
                }
                off
            }
            DebugTy::Sum(name, mir, variants) => self.variant_part(name, mir, variants)?,
            // `{code, env}`.
            DebugTy::Closure(name, mir, env) => {
                let MirTy::Tuple(ps) = mir else {
                    return self.ty(mir);
                };
                let code = self.ty(&ps[0])?;
                let env = self.debug_ty(env)?;
                let at = self.t.field_offsets(mir);
                let size = self.t.size_align(mir).0;
                self.structure(
                    name,
                    size,
                    &[("code".into(), code, at[0]), ("env".into(), env, at[1])],
                )
            }
        };
        self.cache(key, start, off);
        Some(off)
    }

    /// A GC object header's members.
    fn header(&mut self) -> Option<Vec<(String, u32, u32)>> {
        let u32_ = self.base(PrimTy::U32)?;
        let u64_ = self.base(PrimTy::U64)?;
        Some(vec![
            ("type_id".to_owned(), u32_, 0),
            ("gc_flags".to_owned(), u32_, 4),
            ("gc_word".to_owned(), u64_, 8),
        ])
    }

    /// A `(i32 tag, payload)` value as a variant part.
    fn variant_part(
        &mut self,
        name: &str,
        mir: &MirTy,
        variants: &[(u32, String, DebugTy)],
    ) -> Option<u32> {
        let size = self.t.size_align(mir).0;
        let at = self.t.field_offsets(mir)[1];
        // Each variant a struct over the whole sum (as Rust
        // describes enums), its payload's parts at their offsets.
        let mut arms = Vec::new();
        for (tag, v, p) in variants {
            let fields: Vec<_> = match p {
                DebugTy::Mir(t @ MirTy::Tuple(ps)) => self
                    .members(t, ps, &[], &[])
                    .into_iter()
                    .map(|(n, ty, o)| (n, ty, at + o))
                    .collect(),
                p => self
                    .debug_ty(p)
                    .map(|ty| ("0".to_owned(), ty, at))
                    .into_iter()
                    .collect(),
            };
            arms.push((
                *tag,
                v,
                self.structure(&format!("{name}::{v}"), size, &fields),
            ));
        }
        let tag = self.base(mir.debug_tag())?;
        let off = self.b.len() as u32;
        self.b.push(A_STRUCT);
        cstr(&mut self.b, name);
        uleb(&mut self.b, u64::from(size));
        // The discriminant member follows the variant part's
        // abbreviation code and reference.
        let discr = self.b.len() as u32 + 5;
        self.b.push(A_VARIANT_PART);
        self.u32(discr);
        self.b.push(A_DISCR);
        self.u32(tag);
        self.b.push(0);
        for (t, v, ty) in arms {
            // As the discriminant reads it: an `i32` (a `u8` tag is 0 or 1).
            self.b.push(A_VARIANT);
            sleb(&mut self.b, i64::from(t as i32));
            self.member(v, ty, 0);
            self.b.push(0);
        }
        self.b.extend_from_slice(&[0, 0]);
        Some(off)
    }

    /// The type DIE of `ty`, written on first use; `None` for `()`.
    fn ty(&mut self, ty: &MirTy) -> Option<u32> {
        let key = format!("{ty:?}");
        if let Some(o) = self.types.get(&key) {
            return Some(*o);
        }
        if let Some(name) = self.building.get(&key).cloned() {
            let decl = format!("decl {key}");
            if let Some(o) = self.types.get(&decl) {
                return Some(*o);
            }
            let o = self.b.len() as u32;
            self.b.push(A_STRUCT_DECL);
            cstr(&mut self.b, &name);
            self.types.insert(decl, o);
            return Some(o);
        }
        let start = self.b.len();
        let t = self.t;
        let w = t.pointer_width / 8;
        let size = t.size_align(ty).0;
        let off = match ty {
            MirTy::Unit => return None,
            MirTy::Prim(PrimTy::Str) => {
                let u8_ = self.base(PrimTy::U8)?;
                let ptr = self.pointer_to(u8_);
                let len = self.base(PrimTy::Usize)?;
                self.structure(
                    "str",
                    size,
                    &[("ptr".into(), ptr, 0), ("len".into(), len, w)],
                )
            }
            MirTy::Prim(p) => return self.base(*p),
            // A pointer to its subroutine type (`()` parameters left out).
            MirTy::Fn(args, ret) => {
                let ret = self.ty(ret);
                let args: Vec<u32> = args.iter().filter_map(|a| self.ty(a)).collect();
                let sub = self.b.len() as u32;
                self.b.push(match (ret.is_some(), args.is_empty()) {
                    (true, false) => A_FN_RET,
                    (false, false) => A_FN,
                    (true, true) => A_FN_RET_LEAF,
                    (false, true) => A_FN_LEAF,
                });
                if let Some(r) = ret {
                    self.type_ref(r);
                }
                for a in &args {
                    self.b.push(A_FN_PARAM);
                    self.type_ref(*a);
                }
                if !args.is_empty() {
                    self.b.push(0);
                }
                self.pointer_to(sub)
            }
            MirTy::GcRef => {
                let o = self.b.len() as u32;
                self.b.push(A_PTR_NAMED);
                cstr(&mut self.b, "gcref");
                uleb(&mut self.b, u64::from(w));
                o
            }
            MirTy::Any => {
                let tag = self.base(PrimTy::I32)?;
                let payload = self.base(PrimTy::Usize)?;
                self.structure(
                    "any",
                    size,
                    &[("tag".into(), tag, 0), ("payload".into(), payload, w)],
                )
            }
            MirTy::Tuple(ps) => {
                let m = self.members(ty, ps, &[], &[]);
                self.structure("tuple", size, &m)
            }
            MirTy::Struct(id, ps) => {
                let m = self.m;
                let (name, names) = m
                    .struct_names
                    .get(&id.0)
                    .cloned()
                    .unwrap_or_else(|| (format!("Struct#{}", id.0), Vec::new()));
                let names = if names.len() == ps.len() {
                    names
                } else {
                    Vec::new()
                };
                let dbg = m.debug.fields.get(&id.0).map_or(&[][..], Vec::as_slice);
                self.building.insert(key.clone(), name.clone());
                let fields = self.members(ty, ps, &names, dbg);
                self.building.remove(&key);
                self.structure(&name, size, &fields)
            }
            MirTy::Array(..) | MirTy::Union(..) => {
                let (elem, n) = match ty {
                    MirTy::Array(e, n) => ((**e).clone(), *n),
                    MirTy::Union(_, n, _) => (MirTy::Prim(PrimTy::Usize), *n),
                    _ => unreachable!(),
                };
                let base = self.ty(&elem)?;
                let o = self.b.len() as u32;
                self.b.push(A_ARRAY);
                self.type_ref(base);
                self.b.push(A_SUBRANGE);
                uleb(&mut self.b, u64::from(n));
                self.b.push(0);
                o
            }
        };
        self.cache(key, start, off);
        Some(off)
    }

    /// `(name, type, offset)` of each part of `agg` (`names[i]`, else
    /// `i`; typed `debug[i]` when given); `()` parts are left out.
    fn members(
        &mut self,
        agg: &MirTy,
        parts: &[MirTy],
        names: &[String],
        debug: &[DebugTy],
    ) -> Vec<(String, u32, u32)> {
        let offs = self.t.field_offsets(agg);
        let mut out = Vec::new();
        for (i, (p, off)) in parts.iter().zip(offs).enumerate() {
            let ty = match debug.get(i) {
                Some(d) if d.fits(p) => self.debug_ty(d),
                _ => self.ty(p),
            };
            if let Some(ty) = ty {
                let n = names.get(i).cloned().unwrap_or_else(|| i.to_string());
                out.push((n, ty, off));
            }
        }
        out
    }
}

/// Code of scope `id` and its nested scopes as merged `[start, end)`
/// ranges (interleaved with other scopes' code).
fn scope_code(id: u32, d: &crate::lir::FnDebug, f: &FnRange) -> Vec<(u32, u32)> {
    let mut ids = vec![id];
    let mut n = 0;
    while n < ids.len() {
        let p = ids[n];
        ids.extend(d.scopes.iter().filter(|s| s.1 == p).map(|s| s.0));
        n += 1;
    }
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (i, &(at, _, _, sc)) in f.lines.iter().enumerate() {
        let end = f.lines.get(i + 1).map_or(f.len, |n| n.0);
        if end <= at || !ids.contains(&sc) {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 == at => last.1 = end,
            _ => out.push((at, end)),
        }
    }
    out
}

/// Add the debug sections of `obj`'s functions.
pub fn write(obj: &mut Obj, m: &MirModule, sources: &rokc_diagnostic::SourceMap) {
    let t = &m.target;
    let fns: Vec<&FnRange> = obj.fns.iter().filter(|f| f.debug.is_some()).collect();
    // The unit is named by its first function with a position; generated
    // functions without one are at line 0 of its file.
    let Some(first_file) = fns
        .iter()
        .filter_map(|f| f.debug.as_ref().map(|d| d.0.file))
        .find(|&f| f != 0)
    else {
        return;
    };
    // Files numbered by first use: an object names only its own, so a unit's
    // doesn't change when another file appears.
    let mut used: Vec<u32> = vec![first_file];
    for f in &fns {
        let id = f.debug.as_ref().map_or(0, |d| d.0.file);
        if id != 0 && !used.contains(&id) {
            used.push(id);
        }
    }
    let fid = |id: u32| {
        let id = if id == 0 { first_file } else { id };
        used.iter()
            .position(|&u| u == id)
            .map_or(0, |i| i as u32 + 1)
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let all = sources.files();
    let files: Vec<PathBuf> = used
        .iter()
        .map(|&id| {
            all.get(id as usize - 1)
                .map_or_else(PathBuf::new, |(p, _)| cwd.join(p))
        })
        .collect();
    let cu_path = files[0].clone();
    let dir_of = |p: &Path| {
        p.parent()
            .map_or_else(String::new, |d| d.to_string_lossy().into_owned())
    };

    // The sections this adds, by `Obj::extra` index.
    let base = obj.extra.len() as u32;
    let (abbrev_sect, line_sect, ranges_sect) = (base, base + 2, base + 3);
    let first = obj.fns.iter().min_by_key(|f| f.start);
    let mut info = Info {
        b: Vec::new(),
        relocs: Vec::new(),
        types: HashMap::new(),
        t,
        m,
        building: HashMap::new(),
        any_open: None,
        ranges: Vec::new(),
        ranges_relocs: Vec::new(),
        cu_low: if obj.elf {
            0
        } else {
            first.map_or(0, |f| f.start)
        },
        split: obj.elf,
    };
    // Header (length patched below), then the unit.
    info.u32(0);
    info.b.extend_from_slice(&4u16.to_le_bytes());
    info.sec_off(abbrev_sect, 0);
    info.b.push(8);
    info.b.push(A_CU);
    cstr(&mut info.b, "rokc");
    info.b.extend_from_slice(&DW_LANG_RUST.to_le_bytes());
    cstr(
        &mut info.b,
        &cu_path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
    );
    cstr(&mut info.b, &dir_of(&cu_path));
    info.sec_off(line_sect, 0);
    if info.split {
        info.b.extend_from_slice(&[0; 8]);
        info.sec_off(ranges_sect, 0);
        for f in &obj.fns {
            info.range(f, 0, f.len);
        }
        info.ranges.extend_from_slice(&[0; 16]);
    } else {
        // The text section is one range from its first function.
        let text_start = obj.fns.first().map_or(0, |f| f.start);
        let text_end = obj.fns.last().map_or(0, |f| f.start + f.len);
        info.addr(first.map_or(0, |f| f.sym), first.map_or(0, |f| f.start));
        info.u32(text_end - text_start);
    }
    // Types first, so variables refer back to them.
    for f in &fns {
        let d = &f.debug.as_ref().expect("debug").0;
        for v in &d.vars {
            info.debug_ty(&v.ty);
        }
        if let Some(p) = d.ret {
            info.base(p);
        }
    }
    for f in &fns {
        let (d, offs) = f.debug.as_ref().expect("debug");
        let ret = d.ret.and_then(|p| info.base(p));
        let kids = d.vars.iter().any(|v| info.debug_ty(&v.ty).is_some());
        info.b.push(match (ret.is_some(), kids) {
            (true, true) => A_SUB_RET,
            (false, true) => A_SUB,
            (true, false) => A_SUB_RET_LEAF,
            (false, false) => A_SUB_LEAF,
        });
        // An item in a block by its own name (spec 3.9.1).
        cstr(&mut info.b, &rokc_core::local::unmangle(&d.name));
        uleb(&mut info.b, u64::from(fid(d.file)));
        uleb(&mut info.b, u64::from(d.line));
        info.addr(f.sym, f.start);
        if info.split {
            info.addr(f.sym, f.start + f.len);
        } else {
            info.u32(f.len);
        }
        // The frame pointer: `x29` / `rbp`.
        info.b
            .extend_from_slice(&[1, if obj.x86 { DW_OP_REG6 } else { DW_OP_REG29 }]);
        if let Some(r) = ret {
            info.type_ref(r);
        }
        // Scopes that hold a variable (or one that does).
        let mut live: Vec<u32> = Vec::new();
        for v in &d.vars {
            let mut s = v.scope;
            while s != 0 && !live.contains(&s) {
                live.push(s);
                s = d.scopes.iter().find(|(i, _)| *i == s).map_or(0, |x| x.1);
            }
        }
        let var_die = |info: &mut Info<'_>, i: usize| {
            let v = &d.vars[i];
            let Some(ty) = info.debug_ty(&v.ty) else {
                return;
            };
            info.b.push(if v.arg.is_some() { A_PARAM } else { A_VAR });
            cstr(&mut info.b, &v.name);
            uleb(&mut info.b, u64::from(fid(d.file)));
            uleb(&mut info.b, u64::from(v.line));
            info.u32(ty);
            let mut loc = vec![DW_OP_FBREG];
            sleb(&mut loc, i64::from(offs[i]));
            if matches!(v.ty, DebugTy::Cell(_)) {
                loc.extend([DW_OP_DEREF, DW_OP_PLUS_UCONST]);
                uleb(&mut loc, u64::from(HEADER_SIZE));
            }
            uleb(&mut info.b, loc.len() as u64);
            info.b.extend_from_slice(&loc);
        };
        if kids {
            scope(&mut info, 0, d, &live, f, ranges_sect, &var_die);
            info.b.push(0);
        }
    }
    info.b.push(0);
    let len = info.b.len() as u32 - 4;
    info.b[..4].copy_from_slice(&len.to_le_bytes());

    let line = line_program(&fns, &files, &dir_of, &fid, if obj.x86 { 1 } else { 4 });
    let mut sect = |name: &'static str, data: Vec<u8>, relocs: Vec<Reloc>| {
        obj.extra.push((
            "__DWARF",
            name,
            0x0200_0000,
            Section {
                data,
                relocs,
                align: 0,
            },
        ));
    };
    sect("__debug_abbrev", abbrevs(obj.elf), Vec::new());
    sect("__debug_info", info.b, info.relocs);
    sect("__debug_line", line.0, line.1);
    sect("__debug_ranges", info.ranges, info.ranges_relocs);
}

/// The variables of scope `id` and its lexical blocks (those holding a
/// variable of their own), nested.
fn scope(
    info: &mut Info<'_>,
    id: u32,
    d: &crate::lir::FnDebug,
    live: &[u32],
    f: &FnRange,
    ranges_sect: u32,
    var_die: &dyn Fn(&mut Info<'_>, usize),
) {
    for (i, v) in d.vars.iter().enumerate() {
        if v.scope == id || (id == 0 && !live.contains(&v.scope) && v.scope != 0) {
            var_die(info, i);
        }
    }
    for &(c, parent) in &d.scopes {
        if parent != id || !live.contains(&c) {
            continue;
        }
        // Without variables of its own, its blocks go in the parent (as
        // LLVM writes them).
        let own = d
            .vars
            .iter()
            .any(|v| v.scope == c && info.debug_ty(&v.ty).is_some());
        if !own {
            scope(info, c, d, live, f, ranges_sect, var_die);
            continue;
        }
        info.b.push(A_BLOCK);
        let at = info.ranges.len() as u32;
        info.sec_off(ranges_sect, at);
        for (lo, hi) in scope_code(c, d, f) {
            info.range(f, lo, hi);
        }
        info.ranges.extend_from_slice(&[0; 16]);
        scope(info, c, d, live, f, ranges_sect, var_die);
        info.b.push(0);
    }
}

/// `.debug_line`: one sequence per function, rows `unit`-byte aligned
/// (the instruction size, 1 on `x86_64`).
fn line_program(
    fns: &[&FnRange],
    files: &[PathBuf],
    dir_of: &dyn Fn(&Path) -> String,
    fid: &dyn Fn(u32) -> u32,
    unit: u32,
) -> (Vec<u8>, Vec<Reloc>) {
    let mut b = Vec::new();
    let mut relocs = Vec::new();
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    let hl = b.len();
    b.extend_from_slice(&0u32.to_le_bytes());
    let header_start = b.len();
    b.extend_from_slice(&[unit as u8, 1, 1]); // min inst length, max ops, default_is_stmt
    b.push((-5i8) as u8);
    b.push(14);
    b.push(13);
    b.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
    let mut dirs: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    for f in files {
        let d = dir_of(f);
        let k = dirs.iter().position(|x| *x == d).unwrap_or_else(|| {
            dirs.push(d.clone());
            dirs.len() - 1
        });
        entries.push((
            f.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            k + 1,
        ));
    }
    for d in &dirs {
        cstr(&mut b, d);
    }
    b.push(0);
    for (n, d) in &entries {
        cstr(&mut b, n);
        uleb(&mut b, *d as u64);
        b.extend_from_slice(&[0, 0]);
    }
    b.push(0);
    let header_len = (b.len() - header_start) as u32;
    b[hl..hl + 4].copy_from_slice(&header_len.to_le_bytes());
    for f in fns {
        let (d, _) = f.debug.as_ref().expect("debug");
        // DW_LNE_set_address
        b.extend_from_slice(&[0, 9, 2]);
        relocs.push(Reloc {
            off: b.len() as u32,
            kind: RelKind::Abs64,
            sym: f.sym,
        });
        b.extend_from_slice(&u64::from(f.start).to_le_bytes());
        // DW_LNS_set_file
        b.push(4);
        uleb(&mut b, u64::from(fid(d.file)));
        let (mut addr, mut line) = (0u32, 1i64);
        let mut rows = vec![(0u32, d.line, 1u32)];
        rows.extend(f.lines.iter().map(|&(a, l, c, _)| (a, l, c)));
        for (a, l, c) in rows {
            if a > addr {
                b.push(2); // advance_pc
                uleb(&mut b, u64::from((a - addr) / unit));
                addr = a;
            }
            if i64::from(l) != line {
                b.push(3); // advance_line
                sleb(&mut b, i64::from(l) - line);
                line = i64::from(l);
            }
            b.push(5); // set_column
            uleb(&mut b, u64::from(c));
            b.push(1); // copy
        }
        if f.len > addr {
            b.push(2);
            uleb(&mut b, u64::from((f.len - addr) / unit));
        }
        b.extend_from_slice(&[0, 1, 1]); // end_sequence
    }
    let total = b.len() as u32 - 4;
    b[..4].copy_from_slice(&total.to_le_bytes());
    (b, relocs)
}
