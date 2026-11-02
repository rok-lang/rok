//! GC constants the compiler and the runtime must agree on.
//! Plain constants only: `rokc_mir` includes this file by path.

/// Object header size; the payload starts here.
pub const HEADER_SIZE: u32 = 16;
/// Smallest object, header included: every object has a payload word,
/// so compiled code may read the first word of any buffer it holds a
/// reference to before knowing the buffer's length.
pub const MIN_OBJECT_SIZE: u32 = 24;
/// Header offsets of `gc_flags` (u32) and `gc_word` (u64); `type_id` is
/// at 0.
pub const GC_FLAGS_OFFSET: u32 = 4;
pub const GC_WORD_OFFSET: u32 = 8;
/// `gc_flags`: an old object not yet in the remembered set.
pub const FLAG_UNLOGGED: u32 = 1 << 6;
/// `gc_flags`: an old large object whose reference stores mark cards.
/// Its card table pointer is the word before the header.
pub const FLAG_CARDED: u32 = 1 << 7;
/// log2 of the card size of a `FLAG_CARDED` object.
pub const CARD_SHIFT: u32 = 9;
/// Objects this large (header included) go to the large-object space,
/// never to a TLAB.
pub const LOS_THRESHOLD: u32 = 8 << 10;
/// First type id of the compiler-assigned range.
pub const USER_TYPE_BASE: u32 = 0x10000;
/// Type id of a buffer with no references (runtime strings, byte buffers).
pub const PLAIN_TYPE_ID: u32 = 1;
/// Type id of a buffer whose every pointer-aligned word is an ambiguous
/// reference (runtime word buffers).
pub const SCAN_WORDS_TYPE_ID: u32 = 2;
/// Type id of a weak reference cell (`rok_gc_weak_new`): one
/// `WEAK_REF` word after the header (runtime range).
pub const WEAK_TYPE_ID: u32 = 3;

/// `ScanTable` field kinds, the low byte of `field_kind`.
///
/// A reference to an object header, a pointer into an object (`str`), an
/// `any` (`u32` tag, then the payload word), a sum (below), and a word
/// that holds a reference or other bits (its referent is kept alive and
/// never moved). A `WEAK_REF` word is a reference the mark does not
/// follow; the collection that frees its referent clears it.
pub const STRONG_REF: u32 = 0;
pub const WEAK_REF: u32 = 1;
pub const INTERIOR_REF: u32 = 2;
pub const ANY_SLOT: u32 = 3;
pub const SUM_PAYLOAD: u32 = 4;
pub const AMBIGUOUS: u32 = 5;
/// Mask of the kind in `field_kind`.
pub const KIND_MASK: u32 = 0xff;
/// Where a `SUM_PAYLOAD` entry's descriptor is.
///
/// The entry's `field_offset` is its `u32` tag;
/// `field_kind >> SUM_DESC_SHIFT` is the `u32` index (from the table
/// start) of its descriptor: `num_variants`, then `{tag, num_entries,
/// entries}` per variant that holds references, where `entries` is the
/// `u32` index of that many `{offset from the tag, field_kind}` pairs.
/// A tag no variant lists has no references.
pub const SUM_DESC_SHIFT: u32 = 8;
/// `ScanTable` version bit: the table is one element of `total_size`
/// bytes repeated over the payload (a buffer).
pub const TABLE_ARRAY: u32 = 0x8000;
/// `any` tag bit: the payload word is a box reference, not the value.
pub const ANY_BOXED: u32 = 1 << 31;
